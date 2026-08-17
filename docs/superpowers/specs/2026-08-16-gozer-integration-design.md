# Design: tt-station × tt-gozer — leased sessions, driven from a Mac

*Written 2026-08-16, simplified the same day (see [What this deliberately omits](#what-this-deliberately-omits)).
Audience: tt-station maintainers. Companion repo: `~/code/tt-gozer`, whose design doc is the
authority on leasing semantics — this document is the authority on how tt-station uses them.*

## The problem

tt-station serves **one model across the whole box**. Several Claude Code agents work on the
same box directly, and tt-gozer now arbitrates chips between them. The two systems do not
know about each other, so a Mac user running `tt-station run` can start a container on chips a local
agent is mid-bringup on, and a local agent is told those chips are free.

The goal: from a Mac, **stop, start and swap sessions** on a remote QuietBox, with the chips
underneath treated as a real contended resource — two boards hosting two tenants instead of
one whole-box session.

## Two systems, one reality

tt-station already has a concept for "something is using the box that I did not start":
`ServingEntry.source == "external"`. gozer has `BUSY-UNTRACKED`. They mean the same thing, and
neither can see the other's half — gozer knows *who holds which chips*, tt-station knows *what
is serving on which port*. Reconciling those two views is the integration.

## fd blindness, and the primitive that fixes it

gozer's ground truth is "only an open file descriptor proves a chip is in use", read from
`/proc/<pid>/fd`. That is readable **only by the owning user**. tt-station's serving containers
run as root — no `--user` flag appears in `serving/docker.rs`'s argument construction — so
their fds are invisible to gozer running as `ttuser`. Verified on the box: `ttuser` cannot list
a root-owned process's fd directory.

This has a sharper consequence than it first appears. A lease acquired without a supervising
process is *detached*, and gozer reaps a detached lease after a 900-second grace window if no
fd is visible. Container fds are never visible. **So a naive integration's lease would be
reaped roughly fifteen minutes into a running session**, and the chips would read `FREE` while
the container kept using them.

An earlier draft of this design compensated for that with docker labels and a three-way
restart reconciliation. That was machinery standing in for a missing primitive.

### Prerequisite: `gozer acquire --owner-pid N`

**This is a change to tt-gozer, not tt-station, and must land first.** `Keymaster.acquire`
already accepts a `pid` — it is how `gozer run` marks its lease non-detached — but the CLI does
not expose it.

Exposing it lets a supervisor hold a lease on behalf of work it manages. agentd passes its own
pid; gozer then judges the lease by `pid_alive(agentd)` and keeps it `CLAIMED` for exactly as
long as agentd lives. No fd required, no grace window, no reaping surprise.

That one flag removes the grace-window hazard, the docker labels, the adopt path, and most of
the reconciliation. It needs a matching amendment to gozer's own spec.

## Coupling: optional, with degradation

gozer is **not** a hard dependency. agentd probes for it once at startup and caches the
resolved absolute path and version.

* **Present** — lease before launching, expose lease state, allow concurrent tenants.
* **Absent** — behave exactly as today: whole-box serving, no leasing, and report
  `leasing: unavailable` in `/status`.

This keeps tt-station installable on boxes without gozer and means the integration cannot
regress existing users. Both paths are tested; the fallback protects people who have not
adopted gozer and is the one most likely to rot.

**PATH note:** the agentd unit already bakes the operator's `PATH` at install time so the
service can find `tt-smi` and the venv `python3` (systemd `--user` services otherwise get a
minimal `PATH` omitting `~/.local/bin`). gozer installs to `~/.local/bin/gozer` and rides the
same mechanism. agentd resolves it once and stores the absolute path.

## Transport: shell out to the CLI

agentd invokes `gozer <verb> --json` as a subprocess. It does **not** read or write gozer's
state files from Rust.

A second implementation of that lock protocol — the `mkdir` mutex, reentrancy, six-state
reconciliation, grain derivation, atomic writes — would have to stay bug-compatible with one
that took fifty commits and two Critical concurrency bugs to stabilise. Two implementations of
a locking protocol diverge, and the failure mode is silent double-allocation.

`gozer status` no longer reaps (it was changed to `reconcile(reap=False)` precisely so that
inspecting the gate stops mutating it), so `gozer status --json` is a clean read API.

## Box side

### Which backend, and what a grant actually pins

*Revised after mapping the code; the first draft assumed the wrong backend.*

`make_backend` (`serving/mod.rs:153`) dispatches `runpy` | `docker` | `dstack`, and the
default is **`runpy`** (`config.rs:269`), not `docker`. On that default path agentd never
builds a `docker run` at all — `run.py` does, internally — so `--device` is unreachable and
the only device selector agentd controls is `--device-id` (`runpy.rs:706-709`), which takes
run.py's **logical index list** (`"0,1"`), not PCI BDFs.

So a grant is applied as follows:

| Backend | Selector | Value from the grant |
|---|---|---|
| `runpy` (default) | `--device-id` | the grant's `dev_indices`, comma-joined |
| `docker` | one `--device /dev/tenstorrent/<n>` per chip | one per `dev_index` |
| `dstack` | none — no-op, leasing skipped | — |

gozer's grant JSON already carries `dev_indices` alongside `chips` (BDFs), so no mapping
needs inventing. **`DockerConfig::device_path` is a single `String` and is emitted once
(`docker.rs:288`, `:448`); pinning N chips needs a `Vec` and N flags.** That is a required
change to the docker backend, not a configuration choice.

> **Open question, must be answered before implementation.** `tt-smi` treats *UMD logical ID*
> and `/dev/tenstorrent/<id>` as **different namespaces** (`tt_smi/device_input.py`), and
> gozer's `dev_index` is the latter. Whether `run.py`'s `--device-id` means the same thing is
> not established by reading either codebase. Getting it wrong pins the wrong chip **silently**
> — the container starts, serves, and quietly uses a neighbour's hardware. Resolve it by
> launching a single-chip serve with `--device-id` set and confirming which
> `/dev/tenstorrent/N` the container actually opens, before trusting the mapping. If the
> namespaces differ, the mapping belongs in gozer (which owns topology) and is exposed in the
> grant, not reconstructed in agentd.

### The pre-serve whole-box reset must become lease-scoped

`RunPyBackend::start` runs `tt-smi -r` with no targets — a **whole-box** reset — before every
serve (`runpy.rs:622-629`), gated by `reset_before_serve`.

With one tenant that is protective: it clears the wedged ethernet cores documented at
`runpy.rs:251-263`, validated on real hardware. With two tenants it is **actively hostile** —
starting session B resets session A's chips mid-run, which is precisely the collision this
integration exists to prevent, caused by tt-station itself.

Simply disabling it trades one failure for another: the wedged-core protection is real.

**Required change:** when a lease is held, pass the leased BDFs to the reset instead of
resetting everything — `tt-smi -r <bdf>,<bdf>`. That is per-ASIC, verified in gozer's own
`reset.py`, and it keeps the protection for your own chips while leaving a neighbour's alone.
When leasing is unavailable, behaviour is unchanged.

This is a prerequisite for concurrent tenants, not an enhancement.

### Lease before launch

Before the backend launches:

```
gozer acquire --chips all --owner-pid <agentd pid> \
              --who "tt-station:<service-port>:<model>" \
              --reason "<client> via tt-station run" --json
```

*(`--chips all` as of 2026-08-17 — see "Ownership: an advisory model". The first version asked for
`1`.)*

Two details carry weight:

* `--owner-pid` ties the lease's life to agentd's, per above.
* `--who` embeds the **service port** (`tt-station:<port>:<model>`), making that the
  lease↔session binding. No
  docker labels are needed, because gozer already stores this and `docker ps` already reports
  it. It also makes a Mac user's session legible in `gozer status` and `gozer history` *on the
  box*, so a local agent can see the chips belong to someone's remote session rather than to an
  anonymous container.

### Release on every exit, not just `stop`

`ServingBackend::stop` is the obvious release point, and it is not sufficient. A lease taken
at the top of `start` leaks on **five** paths:

| Path | Location |
|---|---|
| in-flight cancel | `runpy.rs:826-832` |
| container died during health poll | `runpy.rs:847-853` |
| health poll timed out | `runpy.rs:897-906` |
| `/stop` | `runpy.rs:924` (the only one `stop()` covers) |
| power command's best-effort stop | `routes.rs:657` |

Every one must release. Because Rust runs `Drop` on unwind and early return, the release
belongs in a **guard type** holding the lease id and released on drop, rather than five
hand-written call sites that a sixth exit path will silently escape. `stop()` and the power
path take the lease explicitly; the three in-`start` failures are exactly what a guard is for.

Note `stop_serving_containers()` (`runpy.rs:426`) looks like the natural choke point and is
not: it is also called by `start`'s pre-launch stale-container sweep and by `reset`, so
releasing there would drop a lease the caller is about to use.

### Reading gozer's exit codes

`run_and_capture` (`docker.rs:224-238`) turns any non-zero exit into a stringified-stderr
error, discarding both the exit code and stdout. gozer's contract is carried *in* those codes
— 10 queued, 12 unavailable, 14 topology unreadable, 15 release refused, 16 mutex stuck — and
its `--json` payload arrives on stdout even when the code is non-zero.

Add a `CommandRunner` method that returns code plus stdout plus stderr rather than collapsing
them. The existing `run` keeps its behaviour so nothing else changes.

`POST /stop` releases the lease, which resets exactly those chips. **Swap** therefore needs no
dedicated verb: stop, then start on the same board, and the incoming model gets clean silicon
without a separate `tt-station reset`.

### Ownership: an advisory model

*Rewritten 2026-08-17, on the branch owner's instruction, superseding the "`POST /reset` refuses
rather than resetting a neighbour" section this replaces and the preemption reasoning at the end of
this document.*

The owner's framing: *"I don't want tt-gozer to be a cop about things. Most common use cases will
just be 'give me all my TT hardware' from a Mac. The goal of per-chip reservations is mostly about
communication and politeness. Not about hard rules and prevention."*

**Leases communicate. `--force` overrides. The only hard rules left are the ones that prevent
accidental corruption.**

Three consequences, and they are the whole model:

1. **The default request is the whole box.** `DEFAULT_LEASE_CHIPS` is `"all"`. A Mac user who asks
   for nothing in particular gets their machine, exactly as they did before gozer existed, and the
   common path is therefore contention-free. Board-grain sharing is the deliberate case; a
   narrower request is one `--chips` argument away in `gozer::acquire`, and other tenants'
   board-grain leases are untouched by this.
2. **Every refusal made on somebody else's behalf is overridable.** `POST /reset`, `POST /power
   reset-chips` and `POST /run` still refuse by default when a lease this request does not own is
   held — the polite thing, and the thing that stops tt-station stomping a neighbour *silently* —
   but each takes a `force` flag (`tt-station reset|power|run --force`) that proceeds anyway. The refusal
   messages name `--force`; the forced paths log what they stepped on (the holder's `who` and the
   chips), because once the refusal is skipped that journal line is the only remaining record.
   "Could not be determined" — an unreadable `gozer status` — is overridable on the same terms:
   fail-closed stays the default because silence must not read as consent, but an operator whose
   gozer is wedged must not need an ssh session to reset their own box.
3. **`--force` gets the owner PAST a lease; it never takes one away.** A forced `POST /run` serves
   **without** a lease rather than releasing the holder's and re-acquiring. The alternative would
   make `gozer status` lie: it would report those chips as ours while the neighbour's container
   still drove them. Unleased, the gate stays true — their lease reads held, ours reads
   `BUSY-UNTRACKED` — and nothing in tt-station ever writes to another tenant's state.

#### Which rules are NOT overridable

The distinction in one line: **`--force` lets the owner step on another tenant; it never lets the
tool step on someone by accident.** These take no `force` flag and must not grow one — when they
fire, nobody has made a decision, so there is nothing to overrule:

| Rule | What it prevents |
|---|---|
| stop the container **before** releasing its lease | `gozer release` resets the released chips; releasing first lands a `tt-smi -r` on a live workload and then advertises those chips free |
| `release_observed_lease`'s compare-and-take | a concurrent `POST /run` can record its own lease mid-swap; releasing whatever is in the slot would reset chips under a live container |
| never reset chips under a container this backend launched and has not stopped | the same hazard on `start`'s in-flight failure exits |
| `Grant::reset_target` refusing a non-BDF or empty grant | `tt-smi -r <int>` is a UMD logical id (different namespace) and `tt-smi -r` with no target is a whole-box reset — either resets chips nobody granted |

#### What this reverses

Two earlier rulings on this branch are deliberately overturned:

* **"Both whole-box reset paths fail closed, with no override."** That was right about the default
  and wrong to make it absolute. With preemption cut from v1 as well, the combination produced a
  tool that would neither let the owner take the box nor let them reset it — the opposite of the
  intent. The default is unchanged; the dead end is gone.
* **"One board is the right default lease (`--chips 1`)."** That inverted the priority. It made
  the rare case (two tenants deliberately sharing) the default and the common case (an owner
  wanting their box) a negotiation, and on a two-board machine it handed a Mac user half their
  hardware while their own local agents contended for the other half.

### One startup sweep

On startup agentd lists gozer leases whose `who` begins `tt-station:`, extracts the **service
port**, and releases any lease with nothing serving on that port.

*Corrected during implementation:* the first draft matched on container name. `run.py` names the
container itself and only reveals the id after launch, so agentd cannot know it at acquire time.
`--who` is therefore `tt-station:<service_port>:<model>`, and the port is the binding. This is
the better key anyway — the port is known before launch and stable across a container restart.
`discover_serving` already parses published host ports (`parse_published_host_port`), so the
sweep reuses it.

That is the whole reconciliation — one rule, one direction. The reverse case (a container with
no lease) can only arise if agentd died between `acquire` and `docker run`, and `--owner-pid`
already covers it: agentd's death makes the lease reapable, and the orphaned container shows as
`BUSY-UNTRACKED`, which gozer refuses to allocate. Visible and safe, without an adopt path.

### When chips are unavailable

*Corrected against gozer's actual output.* `gozer acquire`'s unavailable payload is only
`{"granted": false, "queued": false}` — it names no holder. The queued payload carries
`ahead` (a list of `who` strings waiting), but still not who *holds*. And `gozer status
--json` carries `who`, `reason` and `state` per chip but **no `since`**.

So agentd names the holder by cross-referencing `gozer status --json`, and **duration is not
available** — "board ...4055 is held by claude:ttm-optimize" rather than "...since 14:02".
Adding `since` to gozer's per-chip status output is a small, worthwhile follow-up in that
repo; it is not a blocker here, and agentd must not fabricate a duration it cannot know.


`gozer acquire` exits 10 (queued) or 12 (unavailable). agentd does **not** queue; it releases
any ticket and returns a clear error naming the board and the holder's `who`. With two boards
and a small number of tenants, "board ...4055 is held by claude:ttm-optimize" is more
actionable than a queue position. No duration is reported — see the correction above; gozer
does not expose one and agentd must not invent it.

## Mac side

| Endpoint | Change |
|---|---|
| `GET /status` | gains `leasing: {available, version, boards, max_concurrent}` |
| `GET /serving` | entries gain `lease_id`, `board`, `chips`, `held_since` |
| `POST /run` | on contention, returns the holder rather than a generic failure (no duration — gozer exposes none) |
| `POST /stop` | releases the lease (which resets the chips) |
| `GET /leases` | **new, the only new endpoint** — proxies `gozer status --json` |

`tt-station` CLI:

```
tt-station leases                 # every tenant, including agents that never serve anything
tt-station run qwen3-8b           # fails clearly if no board is free, naming the holder
tt-station stop qwen3-8b          # releases + resets
```

All behind the existing pairing auth.

## Failure modes

| gozer exit | agentd behaviour |
|---|---|
| 10 queued / 12 unavailable | cancel any ticket; report holder + since-when |
| 14 topology unreadable | on the SERVING path: leasing disabled for this call, reported in `/status`. On the two whole-box reset guards: refuse (`ForeignLeases::Undetermined`) — degrading there would reset a neighbour on the strength of a state nobody could read — unless `--force` |
| 15 release refused | surfaced, **not** swallowed — the stop did not happen |
| 16 mutex stuck | box health signal, with the path to clear |

A box reboot clears `/tmp` and the containers together, so both sides return consistent.

**A known gap inherited from gozer:** a lease *reaped* rather than released never resets its
chips, so the next plain `acquire` gets un-reset silicon (`--fresh` is protected). Documented
in gozer's known-limitations; agentd should prefer explicit release.

## Testing

No hardware required.

* agentd's gozer invocation is **injectable** — tests point it at a stub binary returning canned
  JSON, the same technique gozer used with `GOZER_RESET_CMD`. agentd already has an injected
  `CommandRunner` for `docker ps`; this follows that pattern.
* The existing `mock-box` crate hosts the integration tests.
* **Both** paths are tested — leasing and the no-gozer fallback.
* A test asserts the startup sweep releases a lease whose container is gone, and leaves one
  whose container is running.

## What this deliberately omits

An earlier draft of this design included an eviction endpoint with a three-tier grace/SIGKILL
policy, a history proxy, queue surfacing, docker labels, three-way reconciliation, and board
selection on `tt-station run`. All were cut for a first version that is roughly a quarter of the work
and still delivers stop, start, swap, two concurrent sessions, and visibility from the Mac.

**Preemption is still omitted — but the gap it left has been closed differently.** The stated
principle is that someone connected via tt-station owns the hardware and is ultimately in command.
The first version did not express that at all: the owner could stop tt-station's own sessions, but
getting past a local agent's lease meant an SSH and a `gozer release`.

`--force` (see "Ownership: an advisory model") expresses the principle without preemption. It lets
the owner reset the box or serve on it regardless of who holds a lease, and it does so without
touching another tenant's state — no release on their behalf, no process killed, and no
`gozer status` that describes their chips as somebody else's. The cost is that a forced serve runs
unleased and the neighbour learns about it the hard way; the mitigation is the journal line naming
the holder and the chips.

**True preemption — release the holder's lease, stop their process, then acquire — remains future
work**, and remains the right shape for the day a second person or a long-running agent makes it
real: a flag on `tt-station run`, not a separate endpoint. Whatever form it takes, every preemption must
write an `evicted` event to gozer's history naming who ordered it and why, so an agent whose work
was killed can find out from the box without asking anyone. That history record is the thing
`--force`'s local journal line approximates and cannot replace: it lives where the *victim* can
read it. Adding `evicted` to gozer's history is the natural next step even before full preemption —
a forced tt-station action could write one today if gozer exposed the verb.

Also out of scope: multi-box scheduling; moving a running session between boards (a container is
pinned at launch); GUI work in the macOS app or GTK panel beyond what `tt-station` exposes; and any
change to gozer's leasing semantics beyond the `--owner-pid` prerequisite — if this integration
wants different behaviour from gozer, that is a change to gozer's spec, made there.
