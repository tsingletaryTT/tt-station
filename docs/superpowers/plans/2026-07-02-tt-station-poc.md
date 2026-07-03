# tt-station PoC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make a QuietBox 2 on the LAN discoverable and usable from a Mac — discover, pair once, `tt run <model>`, get a live OpenAI-compatible `/v1` endpoint — with no llama.cpp.

**Architecture:** A Rust core (`libttstation` + `tt` CLI) on the Mac talks over LAN HTTP+JSON to a Rust box-side agent (`tt-station-agentd`) that advertises mDNS, does 6-digit pairing, and controls serving through a `ServingBackend` trait (Docker now, dstack later). A `mock-box` dev crate advertises the same service and fakes `/v1` + control so the whole flow is testable without hardware. A SwiftUI `MenuBarExtra` shell (macOS-only) is a thin veneer over `tt --json`.

**Tech Stack:** Rust (Cargo workspace), `mdns-sd` (discovery/advertise), `axum` + `tokio` (agent HTTP), `reqwest` (client), `serde`/`serde_json`, `clap` (CLI), `security-framework` (macOS Keychain), SwiftUI (`MenuBarExtra`).

## Global Constraints

- **No llama.cpp.** Usability is delivered only through the OpenAI-compatible `/v1` from `tt-inference-server` (vLLM).
- **Cloud-burst is out of scope.** No console.tenstorrent.com routing in this plan (follow-on spec).
- **All logic lives in Rust.** The SwiftUI shell must contain no business logic — it only shells out to `tt --json`.
- **Discovery is an interface** with three providers: mDNS (primary), Manual (always works), Tailscale MagicDNS (escape hatch). All return the same `BoxRecord`.
- **Serving is an interface** (`ServingBackend`): `DockerBackend` real now, `DstackBackend` stub now.
- **mDNS service type:** `_tenstorrent._tcp`. **TXT keys:** `name`, `apiver`, `chips`, `status`, `ctrl`.
- **TDD, DRY, YAGNI, frequent commits.** Every code task: failing test → verify fail → minimal impl → verify pass → commit.
- **Environment note:** the build/test session is Linux without macOS or the QB2. Rust tasks (0–12) build and test here against `mock-box`. Tasks 13–15 are **owner-gated** (real hardware / macOS) and are executed by the owner on their Mac + box.
- **Secrets abstraction:** `SecretStore` trait; file-backed impl compiled everywhere, `security-framework` Keychain impl gated behind `#[cfg(target_os = "macos")]`.

---

## File Structure

```
tt-station/
  Cargo.toml                          # [workspace]
  crates/
    libttstation/
      Cargo.toml
      src/lib.rs                      # re-exports
      src/model.rs                    # BoxRecord, ServingStatus, Endpoint, TXT encode/decode
      src/discovery/mod.rs            # DiscoveryProvider trait + aggregate()
      src/discovery/manual.rs         # ManualProvider
      src/discovery/mdns.rs           # MdnsProvider (mdns-sd)
      src/discovery/tailscale.rs      # TailscaleProvider (stub: parses `tailscale status --json`)
      src/secrets.rs                  # SecretStore trait; FileStore + (macOS) KeychainStore
      src/pairing.rs                  # pairing client: init + complete
      src/agent_client.rs            # AgentClient: status/run/stop/endpoint (bearer)
      src/config.rs                   # KnownBoxes config (TOML)
    tt/
      Cargo.toml
      src/main.rs                     # clap CLI over libttstation, global --json
    tt-station-agentd/
      Cargo.toml
      src/main.rs                     # axum server bootstrap + mDNS advertise
      src/pairing.rs                  # 6-digit code issue/verify + token set
      src/serving/mod.rs              # ServingBackend trait + Endpoint reuse
      src/serving/docker.rs           # DockerBackend
      src/serving/dstack.rs           # DstackBackend (stub)
      src/routes.rs                   # /status /pair/init /pair/complete /run /stop /endpoint
    mock-box/
      Cargo.toml
      src/main.rs                     # advertises _tenstorrent._tcp + fake /v1 + fake control API
  macos/
    TTStation/                        # SwiftUI MenuBarExtra (owner-built on Mac)
  docs/superpowers/
    specs/2026-07-02-tt-station-poc-design.md
    plans/2026-07-02-tt-station-poc.md
```

---

### Task 0: Workspace scaffolding

**Files:**
- Create: `Cargo.toml` (workspace), `crates/libttstation/Cargo.toml`, `crates/libttstation/src/lib.rs`, `crates/tt/Cargo.toml`, `crates/tt/src/main.rs`, `crates/tt-station-agentd/Cargo.toml`, `crates/tt-station-agentd/src/main.rs`, `crates/mock-box/Cargo.toml`, `crates/mock-box/src/main.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a compiling four-crate workspace.

- [ ] **Step 1: Write workspace `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/libttstation", "crates/tt", "crates/tt-station-agentd", "crates/mock-box"]

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
anyhow = "1"
```

- [ ] **Step 2: Create `libttstation` with a trivial passing test**

`crates/libttstation/Cargo.toml`:
```toml
[package]
name = "libttstation"
version = "0.0.1"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
```
`crates/libttstation/src/lib.rs`:
```rust
#[cfg(test)]
mod smoke { #[test] fn builds() { assert_eq!(2 + 2, 4); } }
```

- [ ] **Step 3: Create the three binary crates as minimal `fn main`**

Each binary `Cargo.toml` depends on `libttstation = { path = "../libttstation" }` and has `src/main.rs` printing its name. Keep them trivial; later tasks flesh them out.

- [ ] **Step 4: Build and test**

Run: `cargo build && cargo test`
Expected: PASS, all four crates compile.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "chore: scaffold tt-station cargo workspace"
```

---

### Task 1: Core model + TXT record encode/decode

**Files:**
- Create: `crates/libttstation/src/model.rs`
- Modify: `crates/libttstation/src/lib.rs` (add `pub mod model;`)
- Test: inline `#[cfg(test)]` in `model.rs`

**Interfaces:**
- Produces:
  - `struct BoxRecord { name: String, host: String, ctrl_port: u16, chips: String, status: ServingStatus, apiver: u8 }`
  - `enum ServingStatus { Idle, Serving(String) }` (Serving holds model id)
  - `struct Endpoint { base_url: String, model: String, requires_key: bool }`
  - `fn txt_encode(rec: &BoxRecord) -> Vec<(String,String)>`
  - `fn txt_decode(name: &str, host: &str, port: u16, txt: &HashMap<String,String>) -> anyhow::Result<BoxRecord>`
  - `ServingStatus` serializes to/from the TXT string form `idle` / `serving:<model>`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn status_roundtrips_through_txt_string() {
    assert_eq!(ServingStatus::Idle.to_txt(), "idle");
    assert_eq!(ServingStatus::Serving("llama3".into()).to_txt(), "serving:llama3");
    assert_eq!(ServingStatus::from_txt("idle").unwrap(), ServingStatus::Idle);
    assert_eq!(ServingStatus::from_txt("serving:llama3").unwrap(),
               ServingStatus::Serving("llama3".into()));
}

#[test]
fn txt_decode_builds_boxrecord() {
    let mut txt = std::collections::HashMap::new();
    txt.insert("name".into(), "qb2-lab".into());
    txt.insert("apiver".into(), "1".into());
    txt.insert("chips".into(), "4xBH".into());
    txt.insert("status".into(), "idle".into());
    txt.insert("ctrl".into(), "8765".into());
    let rec = txt_decode("qb2-lab", "qb2-lab.local", 8765, &txt).unwrap();
    assert_eq!(rec.name, "qb2-lab");
    assert_eq!(rec.chips, "4xBH");
    assert_eq!(rec.ctrl_port, 8765);
    assert_eq!(rec.status, ServingStatus::Idle);
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p libttstation`
Expected: FAIL (types/functions not found).

- [ ] **Step 3: Implement `model.rs`**

Define the structs/enums above with `#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]`. Implement `ServingStatus::to_txt`/`from_txt` (split on first `:`), `txt_encode`, and `txt_decode` (read keys, parse `ctrl` as `u16`, default `apiver` to 1). Return `anyhow::Error` on missing required keys.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p libttstation`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(core): BoxRecord model + TXT encode/decode"
```

---

### Task 2: DiscoveryProvider trait + ManualProvider

**Files:**
- Create: `crates/libttstation/src/discovery/mod.rs`, `crates/libttstation/src/discovery/manual.rs`
- Modify: `crates/libttstation/src/lib.rs`
- Test: inline in `manual.rs` and `mod.rs`

**Interfaces:**
- Consumes: `BoxRecord`, `txt_decode` (Task 1).
- Produces:
  - `trait DiscoveryProvider { fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<BoxRecord>>; }`
  - `struct ManualProvider { pub hosts: Vec<(String,u16)> }` — probes each host's `GET /status` (via a passed-in resolver fn for testability) and returns a `BoxRecord`.
  - `fn aggregate(providers: &[Box<dyn DiscoveryProvider>], timeout: Duration) -> Vec<BoxRecord>` — runs all, dedups by `name`.

- [ ] **Step 1: Write failing test for aggregate dedup**

```rust
struct Fake(Vec<BoxRecord>);
impl DiscoveryProvider for Fake {
    fn discover(&self, _t: std::time::Duration) -> anyhow::Result<Vec<BoxRecord>> { Ok(self.0.clone()) }
}
#[test]
fn aggregate_dedups_by_name() {
    let r = BoxRecord{ name:"qb2".into(), host:"qb2.local".into(), ctrl_port:8765,
        chips:"4xBH".into(), status:ServingStatus::Idle, apiver:1 };
    let providers: Vec<Box<dyn DiscoveryProvider>> =
        vec![Box::new(Fake(vec![r.clone()])), Box::new(Fake(vec![r.clone()]))];
    let out = aggregate(&providers, std::time::Duration::from_millis(10));
    assert_eq!(out.len(), 1);
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test -p libttstation` → FAIL.
- [ ] **Step 3: Implement** the trait, `aggregate` (collect, dedup by `name`, ignore per-provider errors but log via `eprintln!`), and `ManualProvider` (accept a status-fetch closure `Fn(&str,u16)->Result<BoxRecord>` so it's unit-testable without a network).
- [ ] **Step 4: Run to verify pass** — `cargo test -p libttstation` → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): DiscoveryProvider trait + aggregate + ManualProvider"`

---

### Task 3: mock-box mDNS advertiser

**Files:**
- Modify: `crates/mock-box/Cargo.toml` (add `mdns-sd`, `tokio`, `clap`), `crates/mock-box/src/main.rs`
- Test: manual + used by Task 4's integration test.

**Interfaces:**
- Produces: a runnable `mock-box advertise --name qb2-mock --ctrl-port 8765` that registers `_tenstorrent._tcp` with TXT keys from Task 1's `txt_encode`.

- [ ] **Step 1:** Add deps; write `main.rs` that builds a `BoxRecord` from CLI flags, calls `txt_encode`, and registers the service via `mdns_sd::ServiceDaemon` + `ServiceInfo`. Keep it running until Ctrl-C.
- [ ] **Step 2:** Run `cargo run -p mock-box -- advertise --name qb2-mock --ctrl-port 8765` in one terminal; verify with `avahi-browse -r _tenstorrent._tcp` (Linux) or `dns-sd -B _tenstorrent._tcp` (mac) that it appears with the TXT keys.
- [ ] **Step 3: Commit** — `git commit -am "feat(mock): mock-box advertises _tenstorrent._tcp"`

*Note: no unit test here (pure I/O); it is the fixture that makes Task 4 testable.*

---

### Task 4: mDNS DiscoveryProvider

**Files:**
- Create: `crates/libttstation/src/discovery/mdns.rs`
- Modify: `crates/libttstation/Cargo.toml` (add `mdns-sd`), `discovery/mod.rs`
- Test: `crates/libttstation/tests/mdns_integration.rs` (ignored by default; needs mock-box)

**Interfaces:**
- Consumes: `txt_decode`, `DiscoveryProvider`.
- Produces: `struct MdnsProvider;` implementing `DiscoveryProvider` by browsing `_tenstorrent._tcp` for the given timeout and decoding each resolved service into a `BoxRecord`.

- [ ] **Step 1:** Write `tests/mdns_integration.rs` marked `#[ignore]` that assumes a `mock-box` is advertising and asserts `MdnsProvider.discover(2s)` finds a box named as given by env `TT_MOCK_NAME`.
- [ ] **Step 2:** Implement `MdnsProvider` using `mdns_sd::ServiceDaemon::browse`, collecting `ServiceResolved` events until timeout, mapping each to `txt_decode`.
- [ ] **Step 3:** Run the integration test with a mock-box up:
  ```bash
  cargo run -p mock-box -- advertise --name qb2-it --ctrl-port 8765 &
  TT_MOCK_NAME=qb2-it cargo test -p libttstation --test mdns_integration -- --ignored
  kill %1
  ```
  Expected: PASS (finds `qb2-it`).
- [ ] **Step 4: Commit** — `git commit -am "feat(core): mDNS DiscoveryProvider + integration test"`

---

### Task 5: SecretStore (file + macOS Keychain)

**Files:**
- Create: `crates/libttstation/src/secrets.rs`
- Modify: `Cargo.toml` (add `security-framework` under a macOS target dep), `lib.rs`
- Test: inline (FileStore round-trip)

**Interfaces:**
- Produces:
  - `trait SecretStore { fn set(&self, box_name: &str, token: &str) -> Result<()>; fn get(&self, box_name: &str) -> Result<Option<String>>; fn delete(&self, box_name: &str) -> Result<()>; }`
  - `struct FileStore { path: PathBuf }` (JSON map, 0600 perms) — compiled everywhere.
  - `#[cfg(target_os="macos")] struct KeychainStore;` — generic-password items, service `tt-station`, account = box name.
  - `fn default_store() -> Box<dyn SecretStore>` — Keychain on macOS, FileStore elsewhere.

- [ ] **Step 1:** Failing test: `FileStore` set → get returns token; delete → get returns None. Use a temp path.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement `FileStore` (serde_json map, create parent dir, set mode 0600 on unix) and the `KeychainStore` behind `#[cfg(target_os="macos")]`. `default_store()` selects per-OS.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): SecretStore (file + macOS keychain)"`

---

### Task 6: Agent HTTP skeleton + /status + mDNS advertise

**Files:**
- Modify: `crates/tt-station-agentd/Cargo.toml` (axum, tokio, serde, mdns-sd, clap, anyhow), `src/main.rs`
- Create: `crates/tt-station-agentd/src/routes.rs`
- Test: `crates/tt-station-agentd/tests/status.rs`

**Interfaces:**
- Consumes: `BoxRecord`/`ServingStatus`/`txt_encode` from `libttstation`.
- Produces:
  - Binary `tt-station-agentd --name <n> --ctrl-port <p> [--backend docker|dstack]`.
  - `GET /status` → JSON `{ name, chips, status }` (status as `idle`/`serving:<model>`).
  - On boot: advertises `_tenstorrent._tcp` with TXT from `txt_encode` (reuse Task 3 logic; factor a shared helper if convenient).

- [ ] **Step 1:** Write `tests/status.rs` that starts the router in-process (axum `into_make_service`, bind ephemeral port) and asserts `GET /status` returns 200 with `name` matching.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement `routes.rs` with shared `AppState { name, chips, status: Arc<Mutex<ServingStatus>>, tokens, backend }` and the `/status` handler; wire `main.rs` to build state, spawn mDNS advertise, and serve.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(agent): HTTP skeleton, /status, mDNS advertise"`

---

### Task 7: Pairing — agent side (6-digit code → token)

**Files:**
- Create: `crates/tt-station-agentd/src/pairing.rs`
- Modify: `routes.rs` (add `/pair/init`, `/pair/complete`), `main.rs`
- Test: `crates/tt-station-agentd/tests/pairing.rs`

**Interfaces:**
- Produces:
  - `POST /pair/init` → `{ pair_id }`; agent generates a 6-digit code, prints it to stdout/journal, stores `(pair_id → code, expiry)` with a short TTL.
  - `POST /pair/complete { pair_id, code }` → `{ token }` on match; 401 on mismatch/expired. Token added to the agent's valid-token set.
  - `fn issue_code() -> String` (6 digits) and `fn issue_token() -> String` (URL-safe random). Codes/tokens use a seeded/OS RNG; keep generation in `pairing.rs`.

- [ ] **Step 1:** Write `tests/pairing.rs`: init returns a `pair_id`; complete with the correct code (read from the returned test hook — expose the code in test builds via the router state) returns a token; complete with a wrong code → 401. *(For testability, `AppState` exposes a `last_code(pair_id)` accessor compiled under `#[cfg(test)]` or a test-only feature.)*
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement code/token issuance, the two routes, TTL expiry (store `Instant`), and token-set insertion.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(agent): 6-digit pairing → bearer token"`

---

### Task 8: Pairing client (libttstation) + tt pair wiring later

**Files:**
- Create: `crates/libttstation/src/pairing.rs`
- Modify: `Cargo.toml` (add `reqwest` with `json`, `blocking` or async — use async + tokio), `lib.rs`
- Test: `crates/libttstation/tests/pairing_client.rs` (against a spawned agent, `#[ignore]` if needed) + a unit test with a mock HTTP server (`wiremock`).

**Interfaces:**
- Consumes: agent `/pair/init`, `/pair/complete`.
- Produces:
  - `async fn pair_init(base: &str) -> Result<String /*pair_id*/>`
  - `async fn pair_complete(base: &str, pair_id: &str, code: &str) -> Result<String /*token*/>`

- [ ] **Step 1:** Unit test with `wiremock`: stub `/pair/init` and `/pair/complete`; assert the client parses `pair_id`/`token`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement with `reqwest`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): pairing client"`

---

### Task 9: ServingBackend trait + DockerBackend + DstackBackend stub

**Files:**
- Create: `crates/tt-station-agentd/src/serving/mod.rs`, `serving/docker.rs`, `serving/dstack.rs`
- Modify: `main.rs` (select backend from `--backend`)
- Test: `crates/tt-station-agentd/tests/serving.rs`

**Interfaces:**
- Consumes: `Endpoint` (from `libttstation::model`).
- Produces:
  - `trait ServingBackend: Send + Sync { fn start(&self, model:&str) -> Result<Endpoint>; fn stop(&self, model:&str) -> Result<()>; fn status(&self) -> Result<ServingStatus>; }`
  - `struct DockerBackend { image, host_port, runner: Box<dyn CommandRunner> }` — a `CommandRunner` trait wraps process execution so tests inject a fake; real impl runs `docker run/stop` and polls `GET /v1/models` until healthy (timeout), then returns `Endpoint{ base_url: "http://<host>:<port>/v1", model, requires_key:false }`.
  - `struct DstackBackend;` — `start` returns `Err(anyhow!("dstack backend not implemented (M4)"))`; `status`→Idle. Documented stub.

- [ ] **Step 1:** Write `tests/serving.rs`: with a `FakeRunner` that records commands and reports healthy, `DockerBackend.start("llama3")` returns the expected `Endpoint` and issues a `docker run` containing the model; `stop` issues `docker stop`.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement `CommandRunner` (real = `std::process::Command`), `DockerBackend`, `DstackBackend` stub, and health polling (inject a clock/poller or make the runner report health for tests).
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(agent): ServingBackend trait + DockerBackend + dstack stub"`

---

### Task 10: Agent control routes (/run /stop /endpoint) with bearer auth

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs`, `main.rs`
- Test: `crates/tt-station-agentd/tests/control.rs`

**Interfaces:**
- Consumes: `ServingBackend`, token set (Task 7).
- Produces:
  - `POST /run { model }` (bearer) → `{ endpoint }`; sets status Serving.
  - `POST /stop` (bearer) → `{}`; sets status Idle.
  - `GET /endpoint` (bearer) → `{ base_url, model, requires_key }` or 409 if idle.
  - Missing/invalid bearer → 401 (a small extractor/middleware).

- [ ] **Step 1:** `tests/control.rs`: pair to get a token; `/run` with a `FakeRunner`-backed `DockerBackend` returns an endpoint and flips `/status` to `serving:<model>`; `/run` without bearer → 401; `/endpoint` after run returns the base_url; `/stop` → status idle.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement bearer extractor + the three routes delegating to the backend and updating shared status.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(agent): /run /stop /endpoint with bearer auth"`

---

### Task 11: AgentClient (libttstation)

**Files:**
- Create: `crates/libttstation/src/agent_client.rs`
- Modify: `lib.rs`
- Test: `crates/libttstation/tests/agent_client.rs` (wiremock)

**Interfaces:**
- Produces:
  - `struct AgentClient { base: String, token: String }`
  - `async fn status(&self) -> Result<ServingStatus>`
  - `async fn run(&self, model:&str) -> Result<Endpoint>`
  - `async fn stop(&self) -> Result<()>`
  - `async fn endpoint(&self) -> Result<Endpoint>`
  - All send `Authorization: Bearer <token>`.

- [ ] **Step 1:** wiremock unit tests asserting each method hits the right path/verb, sends the bearer header, and parses the response.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3:** Implement with `reqwest`.
- [ ] **Step 4:** Run → PASS.
- [ ] **Step 5: Commit** — `git commit -am "feat(core): AgentClient"`

---

### Task 12: `tt` CLI wiring + end-to-end against mock-box

**Files:**
- Modify: `crates/tt/Cargo.toml` (clap, tokio, libttstation, serde_json, anyhow), `crates/tt/src/main.rs`
- Modify: `crates/mock-box/src/main.rs` (add a `serve` subcommand: fake `/status`, `/pair/*`, `/run`, `/stop`, `/endpoint`, and a fake `/v1/chat/completions`)
- Test: `crates/tt/tests/e2e_mock.rs`

**Interfaces:**
- Consumes: everything in `libttstation`.
- Produces: the CLI surface from the spec, with global `--json`:
  `tt discover`, `tt pair [host]`, `tt run <model>`, `tt stop`, `tt status`, `tt endpoint`.

- [ ] **Step 1:** Extend `mock-box` with `serve --ctrl-port <p>` implementing the agent's control API + a fake `/v1/chat/completions` returning a canned completion; `/pair/complete` accepts any code and returns a fixed token; `/pair/init` prints a code.
- [ ] **Step 2:** Write `tests/e2e_mock.rs` (`#[ignore]`, needs mock-box): start `mock-box serve`, run the `tt` binary via `assert_cmd`:
  - `tt --json discover` (Manual provider pointed at the mock) lists the box;
  - `tt --json pair 127.0.0.1:<p> --code 000000` stores a token (FileStore under a temp `TT_CONFIG_DIR`);
  - `tt --json run llama3` returns an endpoint JSON;
  - `curl` the returned `base_url` `/chat/completions` → canned completion (assert via reqwest in the test).
- [ ] **Step 3:** Implement `tt` commands: build providers, run `aggregate`, drive `pairing` + `SecretStore`, `AgentClient` for run/stop/status/endpoint; `--json` prints machine JSON, otherwise human text; `tt endpoint` (no `--json`) prints `export OPENAI_BASE_URL=<base_url>`.
- [ ] **Step 4:** Run:
  ```bash
  cargo run -p mock-box -- serve --ctrl-port 8899 &
  TT_CONFIG_DIR=$(mktemp -d) cargo test -p tt --test e2e_mock -- --ignored
  kill %1
  ```
  Expected: PASS — full discover→pair→run→endpoint→completion against the mock.
- [ ] **Step 5: Commit** — `git commit -am "feat(cli): tt commands + end-to-end mock-box test"`

**This task is the CI stand-in for M2.** It proves the entire flow with no hardware.

---

### Task 13 (OWNER-GATED — real hardware, run on the QB2 + Mac): M2 on real chips

**Precondition:** QB2 reachable; `tt-inference-server` image available; Docker on the box.

- [ ] **Step 1:** Cross-build / copy `tt-station-agentd` to the QB2; run `tt-station-agentd --name qb2-lab --ctrl-port 8765 --backend docker`. Confirm it prints "advertising _tenstorrent._tcp".
- [ ] **Step 2:** On the Mac, build `tt`; run `tt discover` — confirm `qb2-lab` appears via mDNS.
- [ ] **Step 3:** `tt pair` — enter the 6-digit code the agent printed on the box; confirm token lands in Keychain (`security find-generic-password -s tt-station`).
- [ ] **Step 4:** `tt run <model>` — DockerBackend starts real `tt-inference-server`; wait for ready.
- [ ] **Step 5:** `eval "$(tt endpoint)"` then:
  ```bash
  curl "$OPENAI_BASE_URL/chat/completions" -H "Content-Type: application/json" \
    -d '{"model":"<model>","messages":[{"role":"user","content":"hi from my Mac"}]}'
  ```
  Expected: a real completion from the QB2. **This is the M2 "it works" gate.**
- [ ] **Step 6:** `tt stop`; confirm status returns to idle. Commit any fixups discovered on hardware.

---

### Task 14 (OWNER-GATED — macOS): SwiftUI MenuBarExtra veneer (M3)

**Files:** `macos/TTStation/` (Xcode project).

**Interfaces:** shells out to `tt --json`; no business logic in Swift.

- [ ] **Step 1:** Create a SwiftUI macOS app with `MenuBarExtra`. On open, run `tt --json discover`; render boxes with status dots.
- [ ] **Step 2:** Add a model text field + Run/Stop buttons calling `tt --json run <model>` / `tt --json stop`; show a spinner until endpoint is ready.
- [ ] **Step 3:** "Copy endpoint" copies `base_url` from `tt --json endpoint`; add "Open Cloud Console" → `https://console.tenstorrent.com`.
- [ ] **Step 4:** Post a `UNUserNotification` when a model reaches ready.
- [ ] **Step 5:** Manual verification against the QB2 (or `mock-box serve` on localhost); commit the Xcode project.

---

### Task 15 (DEFERRED — M4 stretch): Real DstackBackend

- [ ] Replace the `DstackBackend` stub with a real implementation that submits a dstack serving task to the box's SSH fleet, keeping the `ServingBackend` interface identical so neither `tt` nor the menu-bar changes. Separate spec/plan recommended before starting; cloud-burst also lives beyond this line.

---

## Self-Review

**Spec coverage:**
- Rust core (`libttstation` + `tt`) → Tasks 1,2,4,5,8,11,12. ✓
- Box agent (advertise, pair, control, serving) → Tasks 6,7,9,10. ✓
- Discovery interface w/ 3 providers → Task 2 (Manual + aggregate), Task 4 (mDNS). *Tailscale provider listed in file structure but is lowest priority; folded as an optional add in Task 2's file (`tailscale.rs`) — **add explicitly:*** implement `TailscaleProvider` parsing `tailscale status --json` as a small extension of Task 2 if time permits; it is not gating any milestone. (Noted so it isn't silently dropped.)
- ServingBackend seam (Docker now, dstack later) → Task 9. ✓
- 6-digit pairing → Keychain → Tasks 5,7,8,12. ✓
- `/v1` handoff / `OPENAI_BASE_URL` → Task 12 (endpoint printing), Task 13 (real). ✓
- SwiftUI veneer → Task 14. ✓
- Testing (unit, mock box, M2 integration) → mock-box Tasks 3,12; unit tests throughout; M2 gate Tasks 12 (mock) + 13 (real). ✓
- Security/risks → PoC-scoped bearer-over-HTTP is implemented as designed; documented in spec. ✓
- Out-of-scope (llama.cpp, burst, dstack-primary, TT/IP) → respected; dstack real is Task 15/deferred. ✓

**Placeholder scan:** No "TBD/TODO"; each code task has test code and a real implementation description. The one soft spot (Tailscale) is explicitly called out above as optional/non-gating rather than left implicit.

**Type consistency:** `BoxRecord`, `ServingStatus` (`to_txt`/`from_txt`), `Endpoint{base_url,model,requires_key}`, `DiscoveryProvider::discover`, `ServingBackend::{start,stop,status}`, `AgentClient::{status,run,stop,endpoint}`, `SecretStore::{set,get,delete}` are used consistently across tasks. ✓

**Environment caveat:** Tasks 0–12 are fully executable in this Linux session. Tasks 13–14 require the owner's Mac + QB2 and are gated accordingly. Task 15 is deferred.
