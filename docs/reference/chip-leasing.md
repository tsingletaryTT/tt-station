# Chip leasing via `gozer` (reference)

*What changes on a box that has `gozer` installed, and what doesn't. The design rationale lives
in `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`; `tt-gozer` itself is the
authority on leasing semantics. This document is the operator-facing reference for the surfaces
tt-station exposes.*

`gozer` is the CLI that arbitrates which Tenstorrent chips belong to which tenant when several
agents share one box. On a two-board QuietBox that means two tenants can serve at once instead
of one implicitly owning everything.

**`gozer` is optional and absence is normal.** A box without it serves exactly as it always has:
whole-box, no leasing, not a single `gozer` subprocess spawned. Nothing below is a new
requirement — every table says what happens in both cases.

**Leasing here is ADVISORY.** Its job is communication and politeness between tenants, not
enforcement against the box's owner. Two rules follow, and everything in §4 is an application of
them:

- **A lease is the default request, not a permission slip.** `tt-station run` asks for the WHOLE box
  (`--chips all`), so on an idle machine — the overwhelmingly common case — there is no
  contention, no `409`, and nothing to negotiate. Sharing a box board-by-board is the deliberate
  case, not the default one.
- **Every refusal that protects a NEIGHBOUR is `--force`-able.** If tt-station declines because
  someone else holds chips, or because it could not read `gozer` to find out, `--force` overrides
  it and the box logs what was stepped on. The refusals that are *not* overridable are the ones
  that stop tt-station corrupting work by accident (§7) — nobody is making a decision when those
  fire, so there is nothing to overrule.

---

## 1. The one domain fact everything follows from

**Releasing a lease resets its chips.** `gozer release` runs a real `tt-smi -r` on the released
BDFs as part of handing them back, which is why:

- `POST /stop` needs no separate reset — the release does it, so a **swap** is just stop-then-start
  and the incoming model gets clean silicon.
- every path that releases stops its container **first**. Releasing while a container is still
  driving those chips lands a reset on a live workload and then advertises the chips as free, so
  the next tenant collides. `gozer` cannot catch that on its own: it proves a chip is in use by
  reading `/proc/<pid>/fd`, which is unreadable for a root-owned serving container.
- a lease that is **reaped** rather than released is **not** reset (`--fresh` is gozer's protected
  path), so agentd prefers an explicit release everywhere — including for a lease inherited from
  a previous agentd process, or one whose workload exited on its own (see §5).

---

## 2. `tt-station leases` — who holds what

```
tt-station leases --host <host:port>            # one chip per line
tt-station leases --host <host:port> --json     # the whole LeaseList object
```

**Authed**, unlike `tt-station status`/`tt-station serving`/`tt-station models`: `GET /leases` is bearer-guarded, so the
box must be paired.

Human output is one line per chip, tab-separated:

```
<chip>	<bdf>	<board>	<state>	<who>
```

`who` prints as `-` when nobody holds the chip. `state` is **gozer's own vocabulary, verbatim** —
`HELD`, `HELD-FOREIGN`, `CLAIMED`, `STALE`, `BUSY-UNTRACKED`, `FREE` — because those exact strings
appear in the `gozer-gatekeeper`/`gozer-keymaster` skills a human reads. They are never renamed,
lowercased, or restyled anywhere in tt-station.

| Situation | Output |
|---|---|
| gozer installed, chips held or free | one line per chip, as above |
| gozer installed, reports no chips | `no chips reported` |
| gozer **not** installed | `leasing unavailable on this box (gozer not installed)` |

That last row is a **200 with `available: false`**, never an error: "gozer isn't here" is a value a
client needs to be able to read, and it is deliberately distinguishable from `available: true`
with an empty list ("gozer is here and everything is free").

**`/leases` is cached** (a few seconds) and is a *status view, never an interlock*. It can lag a
lease another tenant just took or released. Nothing in tt-station decides whether to reset chips
by reading it — the two reset guards (§4) call `gozer status` fresh instead.

**No durations.** gozer's per-chip status carries `who`, `reason` and `state` but no lease-start
timestamp, so tt-station can name a holder and cannot say how long they have held the board. It
does not invent one — anywhere.

---

## 3. `GET /status`'s `leasing` object, and `tt-station status`

`/status` gains a `leasing` object, and `tt-station status` prints it:

```
$ tt-station status --host mybox.local:8765
idle
leasing:        gozer 0.1.0 (2 board(s), up to 2 concurrent session(s))
```

```json
{"status":"idle","device_mesh":"p300x2",
 "leasing":{"available":true,"version":"gozer 0.1.0","boards":2,"max_concurrent":2}}
```

`version` is `gozer --version`'s output verbatim, from a single probe at agentd startup.
`max_concurrent` is the chip count when gozer's grain is `chip`, else (today, always `board` on
this hardware) the board count.

The answer is **three-way**, and a client must treat it as such:

| `leasing` | Means |
|---|---|
| absent / `null` | that agent is too old to report it. `tt-station status` prints no leasing line at all rather than inventing "no leasing". |
| `{"available": false, ...}` (every other field `null`) | gozer is not installed on that box (or its startup probe failed). Never a guessed version or a fabricated board count. |
| `{"available": true, ...}` | version plus board/concurrency counts. |

`boards`/`max_concurrent` come from the same short-TTL `gozer status --json` parse `/leases` uses,
so asking for `/status` right after (or before) `/leases` costs no extra shell-out.

---

## 4. What changes on a leasing box

### `tt-station run` / `POST /run`

| | Without gozer | With gozer |
|---|---|---|
| chips used | the whole box | **the whole box** — `--chips all` — or whatever is left of it when a neighbour already holds a board |
| pre-serve reset | `tt-smi -r` (whole box) | `tt-smi -r <bdf>,<bdf>` — **only the leased BDFs**, even when those are every BDF on the box |
| device selector | `--device-id` from config; `--tt-device` auto-detected across the whole box | both derived from the grant; a configured value is overridden and the override is logged |
| box fully occupied | n/a | **`409 Conflict`**, naming the board and the holder — or, with `--force`, an unleased serve |

On an idle box the two columns agree on the mesh: a whole-box grant derives the same `--tt-device`
the unleased path auto-detects (pinned by
`runpy_leased_whole_box_serve_passes_the_same_tt_device_as_an_unleased_one`). The lease still
*scopes* the pre-serve reset by BDF, which is what keeps a neighbour's board out of it the moment
one appears.

The `409` is the contention outcome: a contended box is the box's *state* conflicting with the
request, not a failure, so it is retriable and distinguishable from a `500`. Its message names the
board and the holder — and no duration, per §2 — and says `--force` overrides it.

**`tt-station run --force`** serves anyway. The mechanism matters, because it is what keeps `gozer status`
honest: the box does **not** take the holder's lease away, it serves **without a lease** — the
pre-gozer path, whole-box selectors and an unscoped `tt-smi -r`. So `gozer status` afterwards shows
the holder's lease still held (true) and this serve's chips as `BUSY-UNTRACKED` (also true: a
process is holding chips with no lease). Nothing in the gate claims anything that is not the case.
The trade is stated plainly: a forced run **will** disrupt the holder, and the box logs the
override — the holder's `who` and the chips — because that journal line is the only remaining
record of it.

**Swapping models needs no explicit stop.** `tt-station run B` while A serves hands A's board back
(resetting it) before asking gozer for chips again, so a swap does not strand a board. The exact
sequence differs by backend:

| Backend | `start` sequence |
|---|---|
| `runpy` (default) | note the held lease → sweep whatever publishes `serving_port` → release that lease → acquire → **scoped** `tt-smi -r <bdf>,<bdf>` → launch |
| `docker` | note the held lease → stop *its recorded container* → release that lease → acquire → launch (**no** stale-container sweep and **no** board reset — this backend has never run one) |

`tt-station stop` first is still the clearer thing to do, and on a single-board box it is the same
sequence either way.

Only the lease this agent observed at the top of `start` is released. If a concurrent `POST /run`
records its own lease in the meantime, the swap declines to release, logs the supersession, and
leaves the newer lease alone — releasing it would reset the chips under a live container. Two
overlapping `/run`s are not otherwise serialised, so one can still leave a board stranded (wasted,
not reset); prefer one `tt-station run` at a time.

### `POST /reset` (`tt-station reset --host …`) and `power reset-chips` (`tt-station power reset-chips --host …`)

Both run a **whole-box** `tt-smi -r` -- under a short whole-box lease of their own when gozer is
installed (§5) -- which on a shared box resets a neighbour mid-run. So both
**refuse with `409` by default** while gozer reports a lease held by anyone other than this agent's
own session (matched on the `tt-station:<serving_port>:` `who` prefix), and both accept `--force`
to override that. Full behaviour, including the "could not be determined" case, is in
[`power-controls.md` §1](power-controls.md#1-agent-post-power).

```
tt-station reset --host <h> --force               # reset even though someone holds chips
tt-station power reset-chips --host <h> --force    # same, but keeps pairing
```

Four things worth knowing at the CLI:

- **A refusal is not a failure.** `tt-station reset --host X` normally clears local pairing even when the
  box is unreachable. On a refusal it does **not**: the box reset nothing and deliberately kept
  your token, so clearing it would destroy the only credential that can reach the box in exchange
  for nothing. The command errors out with the box's own message instead.
- **A `STALE` or `HELD-FOREIGN` holder is already gone.** "Stop that session" doesn't apply, and
  you cannot `gozer release` it either, because `gozer status` reports no lease id. Run
  `gozer reconcile` on the box, then retry — or just `--force`. The refusal message says so and
  names the state.
- **"Could not be determined" is `--force`-able too.** An unreadable `gozer status` refuses by
  default (silence must not read as "nobody is here"), but an operator whose gozer is wedged must
  still be able to reset their own box without an ssh session. That was the sharpest symptom of
  over-enforcement: the box was unresettable exactly when it most needed resetting.
- **A forced override is logged and audited.** The refusal that would have named the holder never
  happens, so the agent's journal line — holder `who` plus the chips — and the activity-log entry
  (`GET /audit`: result `forced`, target naming the holder) are where a neighbour can later find
  out why their model died. Prefer the polite route
  (stop the other session, or `gozer reconcile` a stale one) when it is available.

`BUSY-UNTRACKED` is deliberately **not** a refusal at all: it is not a lease, there is nobody to
name, and untracked work wedging the box is one of the main reasons to reach for a reset in the
first place.

---

## 5. Who owns a lease, restarts, and the reconciler

### Leases are owned by the workload, not by agentd

gozer judges a non-detached lease by one thing: whether its `--owner-pid` is alive
(`/proc/<pid>` exists -- true for a root-owned container process too, even though its fds are
unreadable). A lease owned by agentd's own pid therefore ended with agentd, not with the serve:
every agentd restart left it `STALE`, the next `gozer acquire` anywhere on the box reaped it
(without a reset) and could hand a live container's chips to someone else. Adoption could only
remember the id; it could not fix the owner (the `UNOWNED LEASE` journal lines).

So every lease is now owned by the process that actually uses the chips:

| Workload | How the lease is taken | Owner |
|---|---|---|
| `runpy` / `docker` serve (a container) | agentd acquires (it needs the grant to pick `--device-id`/`--device`), launches, and once the serve is healthy **hands the lease off** to the container's main pid (`docker inspect --format {{.State.Pid}}`) | the container's main process |
| native vLLM, a served bring-up (Orchard harvest), a community package (`tt-model serve`) -- the `tt-orchard-serve` holder unit | the unit starts behind a **gate** (`sh -c 'go=$(timeout 120 head -n1 FIFO) ...; exec "$@"'`); agentd acquires with `--owner-pid` = the unit's `MainPID`, then opens the gate and the server `exec`s in place (same pid) | the unit's main process |
| bring-up (`lease_run.py` → `gozer run`), Orchard's own runs | `gozer run` | `gozer run`, the workload is its child |
| `POST /reset`, `power reset-chips`, and every `run_power_command` reset (Doctor, bring-up, Orchard, native vLLM, community starts) | a short whole-box lease (`gozer acquire --chips all --no-queue --owner-pid <agentd>`, `who` `tt-station:<port>:reset-chips`) around the `tt-smi -r`, then `gozer release --no-reset`. Refused (`409`, naming the holder) when someone else holds chips, unless `--force` (§4) -- then unleased, journalled, and audited. Unleased, as before, when the only busy chips are this agent's own serve or untracked work | this agent (it runs the reset) |
| a serve's pre-launch reset (`tt-smi -r <bdf>,<bdf>`) | none of its own: it already runs under the serve's lease, scoped to the leased BDFs | (the serve's) |
| Lab (`tt-lab`) | a whole-box lease (`--chips all`, `who` `tt-station:<port>:lab`) from before its pre-run reset until tt-lab exits, then `gozer release` (with reset). Chips held by anyone else: `409` naming the holder. The agent still refuses serves and resets while it runs | this agent (tt-lab's parent) |

The reset and Lab leases are never handed to a workload: the reconciler leaves them alone while
their owner lives, releases one whose owner is dead only when nothing serves on the port (a reset
under a live, forced container would be worse than gozer's reap), and the startup sweep never
adopts one as a serve's. Without gozer, both run unleased exactly as before (`GET /lab`'s
`leasing` says so).

Holder-unit lease ids are kept in `<config>/holder-leases.json` (community ones in
`community/leases.json`) and released right **after** the unit is stopped, so the chips are reset.

**The hand-off.** gozer has no way to change a lease's owner yet, so the interim is: if
`gozer transfer` exists, use it (atomic); otherwise `gozer release <id> --no-reset --force` (no
hardware is touched; `--force` only skips gozer's own-fd check) and immediately
`gozer acquire --exact <bdf>` (one board) or `--chips all` (the whole box) with
`--owner-pid <container pid>`, checking the same chips came back. The chips are unlocked for the
length of one gozer call, so the hand-off is **not attempted while anyone is queued** (the queue
head would win that window); the lease then stays agentd's and the reconciler retries. If the
re-take fails, the journal says `WARNING: ... serving WITHOUT a lease`.

### Restarts

At startup agentd still does one **sweep**: for every lease whose `who` begins `tt-station:`, if
its owner is dead (`STALE`) and nothing is serving on the service port that `who` names, release
it. A lease whose owner is alive is a live workload's and is always kept -- including a holder
unit's, which publishes no docker port. A kept lease on this agent's own port is **adopted** (the
backend's, or left to the community/holder-unit stop that owns it), so the next `tt-station stop`
releases it explicitly.

### The reconciler

Right after startup and then every 60 s (never during a start or stop), agentd compares its own
leases (`tt-station:<own port>:`) with the container publishing its serving port:

| Lease owner | Workload on the port | Action |
|---|---|---|
| dead | alive | **re-attach**: hand the lease to the workload's pid (no reset) |
| dead | none | **release** (resets the chips -- a reap would not) |
| this agent | alive | **hand off** (finishes a deferred hand-off) |
| any other live pid | any | nothing: it is a workload's |

Nothing is done when `docker ps`/`docker inspect`/`gozer status` cannot be read. Each action is
journalled as `tt-station-agentd: lease reconcile: ...`, and the trackers (backend slot, lease
files) follow the new ids. Release on workload exit is therefore: explicit release on every stop
path; otherwise within 60 s by the reconciler (with a reset); gozer's own reap (no reset) only as
the last resort.

### gozer follow-up (proposed)

Add an atomic owner change, mirroring `renew`:

```
gozer transfer <lease> --owner-pid <pid> [--json]
```

Under the gatekeeper mutex: refuse (exit 13) if the lease no longer holds its units, refuse if
`<pid>` is not alive; set the lease's `pid`, `pgid` and `detached=false`, rewrite the lease record
and each unit's lock copy (`update_unit_lease`), and log a `transferred` history event. agentd
already calls it first and falls back only on argparse's "invalid choice" (exit 2), so no
tt-station change is needed when it lands. This follow-up still stands: the reset and Lab leases
above don't need it (their owner is the agent that runs them), but every serve's hand-off does. Exposing `lease_id` in `gozer status --json` (known
limitation 4 in `gozer.rs`) remains the other small fix.

### The runpy `--device-id` mapping (verified from source)

tt-inference-server's `run.py --device-id 2,3` does exactly one thing with the list
(`workflows/run_docker_server.py`): `docker run --device /dev/tenstorrent/2:/dev/tenstorrent/2
--device /dev/tenstorrent/3:/dev/tenstorrent/3`. gozer's `dev_indices` are the same
`/dev/tenstorrent/<n>` numbers (from sysfs `tenstorrent!N`), so a leased container gets exactly the
granted device nodes and cannot open others. run.py does **not** forward its environment into the
container, so the grant's `TT_VISIBLE_DEVICES` (which agentd passes to run.py) only affects
run.py's host-side `tt-smi -s` checks.

### Still to check on hardware (needs idle chips; do not run during someone else's lease)

1. **Container pinning.** Serve one board leased (`gozer acquire --exact <bdf>` held by a
   neighbour first, then `tt-station run`). Then, read-only:
   `docker inspect --format '{{json .HostConfig.Devices}}' <ctr>` must list only the leased
   `/dev/tenstorrent/N`, and `docker exec <ctr> ls /dev/tenstorrent` only those nodes.
2. **Host checks on a half-box lease.** In the same serve's run.py log: the system-software
   validation must pass. If `TT_VISIBLE_DEVICES=<bdfs>` makes `tt-smi -s` list only the leased
   chips, `run_system_software_validation.py` indexes that list by `/dev` index
   (`fw_bundle_versions[i] for i in device_id`) and would fail on `--device-id 2,3`; the fix is
   then to stop forwarding `TT_VISIBLE_DEVICES` to run.py.
3. **Hand-off.** After that serve is healthy: `gozer status --json` shows its chips `CLAIMED`
   with `pid` = `docker inspect --format {{.State.Pid}} <ctr>`; `systemctl --user restart
   tt-station-agentd` leaves them `CLAIMED` with the same pid and no `lease reconcile` action.
4. **Holder unit.** Start a native vLLM bundle: `systemctl --user show -p MainPID
   tt-orchard-serve` equals the lease's `pid` in `gozer status --json` both before and after the
   server is up (the gate `exec`s in place); `tt-station stop` releases it with a reset.
5. **Lab lease.** With the box idle, start a Lab run: during it `gozer status` shows every chip
   `CLAIMED`/`HELD` by `tt-station:<port>:lab` with `pid` = agentd's; after it, every chip
   `FREE` and `gozer history` shows the release with a reset. tt-lab must still start on the
   chips the lease's pre-run reset left (the "BRISC command did not finish" check).
6. **Reset lease.** `tt-station power reset-chips` on an idle box: `gozer history` shows a
   `tt-station:<port>:reset-chips` grant and a release with no reset around one `tt-smi -r`.
   Because that release is `--no-reset`, gozer does not mark the units clean, so a later
   `--fresh` acquire won't count them fresh (nothing in tt-station asks for `--fresh`).

---

## 6. Configuration: `--gozer-path` / `[global].gozer_path`

The only knob. It names the `gozer` binary agentd probes for **once**, at startup.

```toml
[global]
gozer_path = "~/.local/bin/gozer"
```

```
tt-station-agentd --gozer-path ~/.local/bin/gozer
```

| | |
|---|---|
| Type | string (path); leading `~/` expands to `$HOME/` |
| Scope | `[global]` — one gozer per box, not per serving profile |
| Precedence | `--gozer-path` → `[global].gozer_path` → unset |
| Default | **none.** Unlike `tt_smi_bin` there is no built-in default *string*: unset means "search `$PATH` for `gozer`", and if that finds nothing, leasing is simply off. |

Every outcome of the probe — no path resolves, the binary can't be spawned, a non-zero exit, empty
version output — is logged once at startup and treated as **gozer absent**, never as a startup
failure. It is bounded (~5s) so a wedged binary cannot delay the socket bind by more than that.

**The probe runs once.** If you *uninstall* gozer under a running agentd, that agent keeps trying
to use it — and the reset guards will refuse every whole-box reset with "gozer status could not be
read". Restart agentd to re-probe and turn leasing back off, or pass `--force` in the meantime. The
refusal message says both.

---

## 7. Things this version deliberately does not do

- **No queuing.** `acquire` is `--no-queue`: a contended `POST /run` reports who holds the chips
  and returns, rather than waiting in line. No ticket is left on disk to cancel.
- **No eviction.** Nothing in tt-station releases another tenant's lease or kills their process.
  `--force` gets the owner *past* a lease; it does not take one away, and it never writes to
  another tenant's state. (That is also why a forced `tt-station run` serves unleased rather than
  re-acquiring: the alternative would leave `gozer status` describing chips as ours while the
  neighbour's container still drove them.)
- **No leasing on the `dstack` backend.** The stub runs nothing and owns no command seam, so it
  could neither pin a lease to a workload nor hand one back; taking chips it cannot use would
  strand them.
- **No lease durations.** See §2.

### The rules `--force` does not reach

These exist to stop tt-station corrupting work *by accident*. Nobody is making a decision when they
fire, so there is nothing for a flag to overrule — and none of them takes one:

| Rule | What it prevents |
|---|---|
| stop the container **before** releasing its lease | `gozer release` resets the chips; releasing first lands a `tt-smi -r` on a live workload and then advertises those chips free |
| the swap releases only the lease it **observed** (compare-and-take) | a concurrent `POST /run` recording its own lease meanwhile would otherwise have that newer lease released — resetting chips under a live container |
| never reset chips under a container this agent launched and has not stopped | same hazard, on `start`'s in-flight failure exits |
| reset by **BDF only**, never a bare index, and never an empty target | `tt-smi -r <int>` means a UMD logical id (a different namespace) and `tt-smi -r` with no target is a whole-box reset — either would touch chips nobody granted |
| refuse a serve whose leased mesh cannot be derived | naming a mesh wider than the lease hands the serve hardware it was not given |

---

## See also

- `docs/reference/power-controls.md` — the two whole-box reset paths and their `409` refusals.
- `docs/reference/agentd-config.md` — the rest of the config schema.
- `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` — design rationale, failure-mode
  table, and the open questions this integration carries.
- `crates/tt-station-agentd/src/gozer.rs` — module doc, including the known limitations recorded
  in the code a maintainer actually opens.
