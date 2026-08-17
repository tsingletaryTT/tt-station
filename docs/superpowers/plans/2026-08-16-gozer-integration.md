# tt-station × tt-gozer Integration Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** agentd takes a gozer lease before serving, pins the workload to the granted chips, releases on every exit path, and exposes lease state so a Mac can stop, start and swap sessions with two boards hosting two tenants.

**Architecture:** agentd shells out to the `gozer` CLI (never reimplements its state protocol). A capability probe at startup decides between the leasing path and today's unchanged whole-box behaviour. A guard type ties lease release to Rust's drop semantics rather than to five hand-written call sites.

**Tech Stack:** Rust (axum, tokio, anyhow), the existing `CommandRunner` injection for testability, `mock-box` for e2e.

**Spec:** `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` — read it first. It records three assumptions that mapping the code invalidated, and one open question that must not be silently resolved.

## Global Constraints

Every task inherits these.

- **gozer is optional.** Absent → behave exactly as today. The fallback path is tested, not assumed. It protects users who have not adopted gozer and is the one most likely to rot.
- **Never reimplement gozer's state protocol in Rust.** No reading or writing `/tmp/tt-gozer`. Shell out to the CLI.
- **`gozer status` is safe and read-only; `acquire`, `release`, `run`, `reconcile` are not.** Tests must never invoke the real binary — inject a stub through `CommandRunner`.
- **Never run a real serve, `tt-smi`, or `install.sh` against this box during development.** It has live hardware and other agents.
- The endpoint is **`/leases`**, never `/chips` — `chips` already names the box-inventory string (`"4xBH"`) in `StatusResponse`, the mDNS TXT record, `ResolvedConfig`, and agentd's `--chips` flag.
- Existing behaviour of `run`, `stop`, `reset` and `/power` must not change when leasing is unavailable.
- `cargo build`, `cargo test` and `cargo clippy` must all pass before any commit.

## The open question (do not silently resolve)

The spec records it: `tt-smi` treats *UMD logical ID* and `/dev/tenstorrent/<id>` as **different namespaces**, and gozer's `dev_index` is the latter. Whether `run.py`'s `--device-id` means the same thing is **not established**.

This plan proceeds on the assumption that they match, because it cannot be settled without launching a real serve. Therefore:

- The assumption is written down at the point of use, in code, naming what breaks if it is wrong.
- A test pins the current mapping so a future change fails loudly instead of drifting.
- It is recorded in the repo's known-limitations, not only here.

If it is wrong, the container serves happily on **someone else's chips** and nothing detects it. Treat it as unverified until someone runs a single-chip serve and confirms which `/dev/tenstorrent/N` the container opens.

## File map

| File | Role in this change |
|---|---|
| `crates/tt-station-agentd/src/config.rs` | `gozer_path` knob — five structs plus `EMPTY_PROFILE` |
| `crates/tt-station-agentd/src/main.rs` | clap flag, `overrides` literal, capability probe (`:815` precedent), startup sweep |
| `crates/tt-station-agentd/src/gozer.rs` | **new** — the whole gozer client: probe, acquire, release, status, lease guard |
| `crates/tt-station-agentd/src/serving/docker.rs` | `CommandRunner` exit-code method; `device_path: String` → `Vec` |
| `crates/tt-station-agentd/src/serving/runpy.rs` | lease before launch, `--device-id` from grant, lease-scoped reset, release on all exits |
| `crates/tt-station-agentd/src/routes.rs` | `AppState` gozer field + `with_*` builder, `GET /leases`, `leasing` on `/status` |
| `crates/libttstation/src/model.rs` | `LeaseEntry` / `LeaseList` wire types |
| `crates/libttstation/src/agent_client.rs` | `list_leases` |
| `crates/tt/src/main.rs` | `Leases` subcommand, dispatch arm, `cmd_leases`, `print_leases` |
| `crates/mock-box/src/main.rs` | canned `/leases` so `tt leases` has e2e coverage |

---

### Task 1: Foundation — config, probe, and the gozer client module

**Files:** create `crates/tt-station-agentd/src/gozer.rs`; modify `config.rs`, `main.rs`, `serving/docker.rs`

**Produces (later tasks depend on these exact names):**
- `gozer::Capability { path: String, version: String }`
- `gozer::probe(runner: &dyn CommandRunner, path_hint: Option<&str>) -> Option<Capability>`
- `gozer::Grant { lease_id: String, chips: Vec<String>, dev_indices: Vec<u32>, units: Vec<String>, expanded: bool }`
- `gozer::Outcome::{Granted(Grant), Unavailable{holder: Option<String>, since: Option<String>}, Failed(String)}`
- `CommandRunner::run_capturing(&self, args: &[&str]) -> Result<CapturedOutput>` where `CapturedOutput { code: i32, stdout: String, stderr: String }`

- [ ] **Step 1: `run_capturing` on `CommandRunner`**

`run_and_capture` (`docker.rs:224-238`) collapses any non-zero exit into a stringified-stderr error, discarding the code and stdout. gozer's contract lives in its exit codes — 10 queued, 12 unavailable, 14 topology unreadable, 15 release refused, 16 mutex stuck — and it emits `--json` on stdout *even when the code is non-zero*.

Add `run_capturing` as a trait method with a default implementation so no existing implementor breaks. Implement it properly on `RealCommandRunner`. Leave `run` untouched — everything else depends on its current behaviour.

Extend both fakes (`tests/support/mod.rs:70` `FakeRunner`, `docker.rs:554` `RecordingFakeRunner`) so a test can set a canned exit code alongside canned stdout.

- [ ] **Step 2: Write failing tests for `probe` and `Outcome` parsing**

Against `FakeRunner`: a probe that returns a version string yields `Some(Capability)`; a probe whose command fails yields `None` (**not** an error — absence is normal); exit 0 with grant JSON parses to `Granted` with the right `dev_indices`; exit 12 parses to `Unavailable` carrying the holder; exit 10 also maps to `Unavailable` (this version does not queue); malformed JSON on exit 0 yields `Failed` rather than a panic.

Run them; confirm they fail.

- [ ] **Step 3: Implement `gozer.rs`**

`probe` runs `<path> --version` through the injected runner. Resolution order: explicit config → `gozer` on `PATH`. Store the resolved absolute path. Absence is a normal outcome, logged once at info, never an error.

Parse `--json` for grants. Map exit codes per the table above. Include `--owner-pid <std::process::id()>` on every `acquire` — this is the whole reason the flag exists, and without it the lease is reaped ~15 minutes in while the container still runs.

- [ ] **Step 4: Config knob**

`gozer_path: Option<String>` following `tt_smi_bin` exactly (`config.rs:43`, `:73`, `:107`, resolved at `:256-257`). Five places plus `EMPTY_PROFILE` (`config.rs:333`) — `deny_unknown_fields` turns a miss into a hard TOML parse failure. Route it through `expand_tilde` (`config.rs:158`) or `~/.local/bin/gozer` will not expand. Add the clap flag in `main.rs` and the `overrides` literal (`main.rs:589-617`).

- [ ] **Step 5: Wire the probe into startup**

Alongside `detect_startup_device_mesh` (`main.rs:815`) — the existing precedent for a bounded one-time probe that degrades to `None`. Same `spawn_blocking` + timeout shape. Note everything between `:815` and the bind at `:839` delays the socket.

Store on `AppState` via a new `with_gozer(...)` builder following the existing pattern (`routes.rs:359` etc). **It must be inserted into the `main.rs:726-801` chain before any clone of `AppState` exists** — `Arc::get_mut` makes these silently no-op afterwards, logging only a warning.

- [ ] **Step 6: Green, clippy, commit**

```
feat: gozer client module, capability probe, and exit-code-aware runner

run_and_capture discarded exit codes, and gozer's contract lives in them --
10 queued, 12 unavailable, 15 release refused, 16 mutex stuck -- with --json
on stdout even on a non-zero exit. run_capturing preserves all three; run is
untouched.

The probe follows detect_startup_device_mesh: bounded, degrades to None, and
absence of gozer is a normal outcome rather than an error.
```

---

### Task 2: Lease lifecycle in the runpy backend

**Files:** `crates/tt-station-agentd/src/serving/runpy.rs`, `crates/tt-station-agentd/src/gozer.rs`

**Consumes:** Task 1's `gozer::{Capability, Grant, Outcome}`.
**Produces:** `gozer::LeaseGuard` — holds `lease_id` and a runner handle; releases on `Drop`; `into_inner()` to disarm when ownership transfers.

This is the task where a mistake resets someone else's running hardware. Read the spec's two relevant sections before starting.

- [ ] **Step 1: Failing tests first**

Using `FakeRunner` and `find_runpy_cmd` (`tests/runpy.rs:47`):

1. With leasing available, `start` issues `gozer acquire` **before** the reset and before `run.py`, and passes `--owner-pid`.
2. `--device-id` carries the grant's `dev_indices`, comma-joined.
3. **The pre-serve reset targets only the leased BDFs** — `tt-smi -r <bdf>,<bdf>` — not a bare whole-box `tt-smi -r`.
4. With leasing unavailable, argv is **byte-identical to today**: whole-box reset, `--device-id` from config. Assert against the existing tests at `tests/runpy.rs:282` and `:527`.
5. When `acquire` reports unavailable, `start` fails **without** launching run.py and **without** resetting anything, and the error names the holder.
6. A lease is released when `start` fails at each of the three in-flight exits (`runpy.rs:826-832` cancel, `:847-853` container died, `:897-906` health timeout).
7. `stop` releases the lease.

- [ ] **Step 2: `LeaseGuard`**

Release-on-drop. Five paths currently stop a container and only one is `stop()`; a sixth will be added someday and will silently escape hand-written release calls. `Drop` runs on early return and unwind, which is exactly the shape of the three in-`start` failures.

Do **not** put release inside `stop_serving_containers()` (`runpy.rs:426`) — it is also called by `start`'s pre-launch stale sweep and by `reset`, so releasing there would drop a lease the caller is about to use.

- [ ] **Step 3: Acquire, pin, scope the reset**

Acquire at the top of `start`, before the stale-container sweep (`runpy.rs:608`).

`--device-id` takes the grant's `dev_indices`. **Put the namespace assumption in a comment at that line**, naming what breaks if it is wrong: the container serves on another tenant's chips and nothing detects it.

Change the reset (`runpy.rs:622-629`) to pass the leased BDFs when a lease is held. Keep `reset_before_serve` semantics. When there is no lease, the command is unchanged — the wedged-ethernet-core protection documented at `runpy.rs:251-263` was validated on hardware and must survive for single-tenant boxes.

- [ ] **Step 4: Release on stop**

`stop` (`runpy.rs:924`) and the power path (`routes.rs:657`) release explicitly. The guard covers the rest.

- [ ] **Step 5: Green, clippy, commit**

```
feat: runpy backend leases chips and scopes its pre-serve reset

start unconditionally ran a whole-box tt-smi -r before every serve. With one
tenant that clears wedged ethernet cores; with two it resets the other
tenant's chips mid-run -- tt-station causing exactly the collision this
integration exists to prevent. The reset now targets the leased BDFs, which
is per-ASIC and verified in gozer's own reset.py.

Release lives in a Drop guard because a lease taken at the top of start leaks
on three in-flight failures that stop() never sees.
```

---

### Task 3: docker and dstack backends

**Files:** `crates/tt-station-agentd/src/serving/docker.rs`, `serving/dstack.rs`

- [ ] **Step 1: Failing tests**

`DockerBackend::start` with a two-chip grant emits **two** `--device` flags, one per chip; with no leasing, argv is byte-identical to today (`tests/serving.rs:44`). `DstackBackend` is unaffected and never calls gozer.

- [ ] **Step 2: `device_path: String` → `Vec<String>`**

`DockerConfig::device_path` (`docker.rs:288`) is a single string emitted once (`:448`). Pinning N chips needs N flags. Keep the single-entry default so unleased behaviour is unchanged.

- [ ] **Step 3: Same guard, same acquire**

Reuse `LeaseGuard`. `DstackBackend` (`serving/dstack.rs`, a 42-line stub) gets an explicit no-op with a comment saying why.

- [ ] **Step 4: Green, clippy, commit**

---

### Task 4: `/leases`, the wire type, and `tt leases`

**Files:** `routes.rs`, `libttstation/src/model.rs`, `libttstation/src/agent_client.rs`, `tt/src/main.rs`, `mock-box/src/main.rs`

- [ ] **Step 1: Failing tests**

A `GET /leases` test modelled on `tests/status.rs` (the simplest route test): returns lease state when gozer is present; returns an explicit `unavailable` marker rather than an error when it is not. `/status` gains `leasing`.

- [ ] **Step 2: Wire types**

`LeaseEntry` / `LeaseList` in `libttstation/src/model.rs`, alongside `ServingEntry` (`:87`) / `ServingList` (`:110`). Carry: `chip`, `bdf`, `board`, `state`, `who`, `since`, `reason`.

- [ ] **Step 3: The handler**

Authed (`_auth: BearerAuth`, per `get_endpoint` at `routes.rs:1588`). Shells out via `spawn_blocking`, following `get_serving` (`routes.rs:1853-1871`).

**Cache it.** `Inner.tt_smi_cache` (`routes.rs:188`) exists precisely because concurrent `tt-smi` runs contend with a live workload. `gozer status` reads sysfs and `/proc` and is cheaper, but an unthrottled endpoint invites a Mac polling every second. Use a short TTL following the existing pattern.

- [ ] **Step 4: `/status` gains `leasing`**

`{available, version, boards, max_concurrent}` on `StatusResponse` (`routes.rs:~1180`).

- [ ] **Step 5: Client, CLI, mock-box**

`list_leases` in `agent_client.rs` (authed, so on `AgentClient`, per `endpoint` at `:290`). `Leases` variant + dispatch arm + `cmd_leases` + `print_leases` in `tt/src/main.rs`, modelled on `Serving` (`:243`, `:431`, `:836`, `:1457`).

`mock-box` duplicates the router by hand (`mock-box/src/main.rs:657-676`) — add a canned `/leases` or `tt leases` has no e2e coverage.

- [ ] **Step 6: Green, clippy, commit**

---

### Task 5: Startup sweep, contention errors, and the fallback suite

**Files:** `main.rs`, `routes.rs`, `serving/discovery.rs`, tests

- [ ] **Step 1: Failing tests**

The sweep releases a `tt-station:*` lease whose container is gone, and leaves one whose container is running. `POST /run` under contention returns the holder and since-when rather than a generic 500 (`backend_error` at `routes.rs:1368` is always 500; a contention response wants 409 and detail).

- [ ] **Step 2: The sweep**

After the probe. For each lease whose `who` starts `tt-station:`, extract the container name and release if it is not in `docker ps`. `discover_serving` (`serving/discovery.rs:57`) already captures `.Names` (`:33`), so reuse it rather than shelling out again.

One rule, one direction. The reverse case — a container with no lease — is covered by `--owner-pid`: agentd's death makes the lease reapable, and the orphan shows as `BUSY-UNTRACKED`, which gozer refuses to allocate.

- [ ] **Step 3: Contention errors**

Name the board, the holder's `who`, and how long they have held it.

- [ ] **Step 4: The fallback suite**

Explicitly assert that with no gozer, `run`/`stop`/`reset` produce byte-identical argv to today. This is the test that protects existing users.

- [ ] **Step 5: Known limitations**

Record in the repo (not only in the spec): the `--device-id` namespace assumption is unverified; a lease *reaped* rather than released never resets its chips, so agentd should prefer explicit release; and `/leases` is cached, so it can lag by its TTL.

- [ ] **Step 6: Full suite, clippy, commit**

---

## Self-Review

**Spec coverage:** every section of the design maps to a task — fd blindness and `--owner-pid` to Task 1, backend/grant application and the lease-scoped reset to Tasks 2–3, the Mac surface to Task 4, the sweep and failure modes to Task 5, degradation throughout.

**Naming consistency:** `gozer::{Capability, Grant, Outcome, LeaseGuard}` and `CommandRunner::run_capturing` are defined in Task 1 and consumed unchanged afterwards. `/leases` is used throughout; `/chips` appears nowhere.

**Known rough edge:** the `--device-id` namespace question is unresolved by design and cannot be settled without hardware. It is documented in code, in a test, and in known-limitations rather than being quietly assumed.
