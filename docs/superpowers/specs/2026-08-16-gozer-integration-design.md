# Design: tt-station × tt-gozer — leased sessions, driven from a Mac

*Written 2026-08-16, simplified the same day (see [What this deliberately omits](#what-this-deliberately-omits)).
Audience: tt-station maintainers. Companion repo: `~/code/tt-gozer`, whose design doc is the
authority on leasing semantics — this document is the authority on how tt-station uses them.*

## The problem

tt-station serves **one model across the whole box**. Several Claude Code agents work on the
same box directly, and tt-gozer now arbitrates chips between them. The two systems do not
know about each other, so a Mac user running `tt run` can start a container on chips a local
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

### Lease before launch

Before `docker run`:

```
gozer acquire --chips <N> --owner-pid <agentd pid> \
              --who "tt-station:<container-name>" \
              --reason "<client> via tt run" --json
```

Two details carry weight:

* `--owner-pid` ties the lease's life to agentd's, per above.
* `--who` embeds the **container name**, making that string the lease↔container binding. No
  docker labels are needed, because gozer already stores this and `docker ps` already reports
  it. It also makes a Mac user's session legible in `gozer status` and `gozer history` *on the
  box*, so a local agent can see the chips belong to someone's remote session rather than to an
  anonymous container.

The granted BDFs feed the existing `--device` / `--device-id` plumbing, replacing today's
whole-box `--tt-device`.

### Release on stop

`POST /stop` releases the lease, which resets exactly those chips. **Swap** therefore needs no
dedicated verb: stop, then start on the same board, and the incoming model gets clean silicon
without a separate `tt reset`.

### One startup sweep

On startup agentd lists gozer leases whose `who` begins `tt-station:`, extracts the container
name, and releases any whose container is not in `docker ps`.

That is the whole reconciliation — one rule, one direction. The reverse case (a container with
no lease) can only arise if agentd died between `acquire` and `docker run`, and `--owner-pid`
already covers it: agentd's death makes the lease reapable, and the orphaned container shows as
`BUSY-UNTRACKED`, which gozer refuses to allocate. Visible and safe, without an adopt path.

### When chips are unavailable

`gozer acquire` exits 10 (queued) or 12 (unavailable). agentd does **not** queue; it releases
any ticket and returns a clear error naming the board, the holder's `who`, and how long they
have held it. With two boards and a small number of tenants, "board ...4055 is held by
claude:ttm-optimize since 14:02" is more actionable than a queue position.

## Mac side

| Endpoint | Change |
|---|---|
| `GET /status` | gains `leasing: {available, version, boards, max_concurrent}` |
| `GET /serving` | entries gain `lease_id`, `board`, `chips`, `held_since` |
| `POST /run` | on contention, returns the holder and since-when rather than a generic failure |
| `POST /stop` | releases the lease (which resets the chips) |
| `GET /chips` | **new, the only new endpoint** — proxies `gozer status --json` |

`tt` CLI:

```
tt chips                  # every tenant, including agents that never serve anything
tt run qwen3-8b           # fails clearly if no board is free, naming the holder
tt stop qwen3-8b          # releases + resets
```

All behind the existing pairing auth.

## Failure modes

| gozer exit | agentd behaviour |
|---|---|
| 10 queued / 12 unavailable | cancel any ticket; report holder + since-when |
| 14 topology unreadable | leasing disabled for this call, reported in `/status` |
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
selection on `tt run`. All were cut for a first version that is roughly a quarter of the work
and still delivers stop, start, swap, two concurrent sessions, and visibility from the Mac.

**The significant omission is preemption.** The stated principle is that someone connected via
tt-station owns the hardware and is ultimately in command. This version does not express that:
the owner can stop tt-station's own sessions, but taking a board from a local agent means an
SSH and a `gozer release`.

That is an accepted v1 limitation, not a rejection. It is acceptable only while the box has few
tenants and the owner has shell access. **When a second person, or a long-running agent whose
work you cannot casually interrupt, makes preemption real, the cheapest addition is a
`tt run --take` flag** — release the holder's lease, stop its process, then acquire — rather
than a separate endpoint. Whatever form it takes, every preemption must write an `evicted`
event to gozer's history naming who ordered it and why, so an agent whose work was killed can
find out why from the box without asking anyone.

Also out of scope: multi-box scheduling; moving a running session between boards (a container is
pinned at launch); GUI work in the macOS app or GTK panel beyond what `tt` exposes; and any
change to gozer's leasing semantics beyond the `--owner-pid` prerequisite — if this integration
wants different behaviour from gozer, that is a change to gozer's spec, made there.
