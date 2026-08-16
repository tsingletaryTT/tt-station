# Design: tt-station × tt-gozer — leased sessions, driven from a Mac

*Written 2026-08-16. Audience: tt-station maintainers. Companion repo:
`~/code/tt-gozer` (github tsingletaryTT/tt-gozer), whose design doc is the authority on
leasing semantics — this document is the authority on how tt-station uses them.*

## The problem

tt-station serves **one model across the whole box**. Meanwhile several Claude Code agents
work on the same box directly, and tt-gozer now arbitrates chips between them. The two
systems do not know about each other, so:

* A Mac user running `tt run` can start a container on chips a local agent is mid-bringup on.
* A local agent asking gozer for chips is told they are free, because a serving container's
  file descriptors are invisible to it (see [fd blindness](#fd-blindness)).
* From the Mac you cannot see who holds the box, only what is serving.
* Only one session can run at a time, though the hardware supports two.

The goal: from a Mac, **stop, start and swap sessions** on a remote QuietBox, with the
chips underneath treated as a real contended resource.

## Two systems, one reality

tt-station already has a concept for "something is using the box that I did not start":
`ServingEntry.source == "external"`. gozer has `BUSY-UNTRACKED`. They mean the same thing
and neither can see the other's half — gozer knows *who holds which chips*, tt-station knows
*what is serving on which port*. Reconciling those two views is the integration.

## Ownership model

**Someone connected via tt-station owns the hardware and is ultimately in command of it.**

This is the organising principle, and it produces a two-tier model:

| Tier | Who | What they can do |
|---|---|---|
| **Peers** | Local agents, scripts, cron — anything calling `gozer` directly | Cooperate. Take leases, queue, wait. Cannot take a chip from another peer. |
| **Owner** | A paired tt-station client | Preempt. Can evict any tenant it is able to signal, including a peer's live work. |

The qualifier is a capability limit, not a policy one: agentd can stop anything running as
its own user, which covers every peer and every container it launched. A process owned by
another user (or a root-owned container it did not start) is beyond its reach without
elevated privileges, and the design says so rather than pretending otherwise — see
[Eviction](#eviction).

gozer remains advisory *between peers* — that does not change. tt-station becomes a
**privileged client** of gozer, and its privilege is exercised through a documented, logged
path rather than by bypassing the lease system. Ownership is never exercised accidentally:
`tt run` still queues by default, and preemption requires an explicit `tt evict`.

This is why eviction can be real rather than symbolic. agentd runs as a systemd `--user`
service as the same user as local agents, so it can genuinely stop their processes. An
earlier draft of this design had eviction refuse on a foreign lease out of politeness; that
was wrong twice over — it denied the owner control of their own hardware, and it did not
even work, because releasing a lease does not stop a process. The board would simply flip to
`BUSY-UNTRACKED` and stay unallocatable.

## fd blindness

gozer's ground truth is "only an open file descriptor proves a chip is in use", read from
`/proc/<pid>/fd`. That is readable **only by the owning user**. tt-station's serving
containers run as root — no `--user` flag appears in `serving/docker.rs`'s argument
construction — so their fds are invisible to gozer running as `ttuser`. Verified on the box:
`ttuser` cannot list a root-owned process's fd directory.

Left alone, a serving container would make its chips read `FREE`, and gozer would hand them
to a local agent — the exact collision both tools exist to prevent, arriving through the
front door.

**Resolution:** agentd owns the lease lifetime for anything it launches. gozer never needs to
see inside the container, because agentd — same user, supervising the container, already
listing `docker ps` for `/serving` — is the ground truth gozer lacks, and this design makes
agentd's knowledge gozer's knowledge at well-defined sync points.

Three alternatives were considered and rejected: running containers as the invoking user
(may break the inference-server image, which likely needs root for device access and
hugepages); requiring `gozer reconcile --sudo` everywhere (puts passwordless sudo on the
critical path of the core loop, and only helps callers who remember the flag); and having
gozer poll `docker ps` itself (gozer must not grow a docker dependency — it has none today
and runs on boxes without docker).

## Coupling: optional, with degradation

gozer is **not** a hard dependency. agentd probes for it once at startup and caches the
resolved path and version.

* **Present** — lease before launching, expose lease state, allow concurrent tenants.
* **Absent** — behave exactly as today: whole-box serving, no leasing, and report
  `leasing: unavailable` in `/status` so the Mac can say so.

This keeps tt-station installable on boxes without gozer and means the integration cannot
regress existing users. Both paths are tested; the fallback is the one protecting people who
have not adopted gozer.

**PATH note:** the agentd unit already bakes the operator's `PATH` at install time so the
service can find `tt-smi` and the venv `python3` (systemd `--user` services otherwise get a
minimal `PATH` that omits `~/.local/bin`). gozer installs to `~/.local/bin/gozer` and rides
the same mechanism. agentd resolves it once and stores the absolute path rather than
depending on `PATH` at call time.

## Transport: shell out to the CLI

agentd invokes `gozer <verb> --json` as a subprocess. It does **not** read or write gozer's
state files from Rust.

A second implementation of that lock protocol — the `mkdir` mutex, reentrancy, the six-state
reconciliation, grain derivation, atomic writes — would have to stay bug-compatible with one
that took fifty commits and two Critical concurrency bugs to stabilise. Two implementations
of a locking protocol diverge; the failure mode is silent double-allocation. One
implementation, driven through the tested CLI.

Since `gozer status` no longer reaps (it was changed to `reconcile(reap=False)` precisely so
that inspecting the gate stops mutating it), `gozer status --json` is a clean read API.

## Box side: agentd as a gozer client

### Lease before launch

Before `docker run`:

```
gozer acquire --chips <N> --who "tt-station:<model>" --reason "<client> via tt run" --json
```

The `who` string is load-bearing beyond bookkeeping: it makes a Mac user's session legible
in `gozer status` and `gozer history` **on the box**, so a local agent can see the chips
belong to someone's remote session rather than to an anonymous container. Accountability
crosses the machine boundary instead of stopping at it.

The granted BDFs feed the existing `--device` / `--device-id` plumbing, replacing today's
whole-box `--tt-device`.

### The lease↔container binding is a docker label

agentd launches with `--label gozer.lease=<lease_id>`.

Holding this mapping only in agentd's memory would lose it when agentd crashes. A label
survives agentd's death, survives its restart, and is recoverable with a plain
`docker ps --filter label=gozer.lease`.

### Restart reconciliation

On startup, agentd compares `docker ps --filter label=gozer.lease` against
`gozer status --json`:

| Container | Lease | Action |
|---|---|---|
| running | missing | `gozer adopt` — agentd knows the chips from the container's `--device` args |
| absent | held by `tt-station:*` | `gozer release` |
| running | matching | leave alone |

This is the sync point that closes fd blindness: gozer cannot see inside the container, but
agentd can, and here its knowledge becomes gozer's.

### Queueing

When chips are unavailable, `gozer acquire` exits 10 with a ticket. agentd surfaces this as a
**queued serving request** rather than an error, so the Mac can wait or give up. This is the
first real caller of gozer's queue.

## Mac side

All endpoints sit behind the existing pairing auth.

| Endpoint | Change |
|---|---|
| `GET /status` | gains `leasing: {available, version, boards, max_concurrent}` |
| `GET /serving` | entries gain `lease_id`, `board`, `chips`, `held_since` |
| `POST /run` | accepts `chips` (`N` / `all` / a board serial) and `wait`; returns a lease **or** a queue ticket |
| `POST /stop` | releases the lease (which resets the chips) |
| `GET /chips` | **new** — every tenant, not only tt-station's |
| `GET /chips/history` | **new** — proxies `gozer history --json` |
| `POST /evict` | **new** — preempt a tenant (see below) |

`tt` CLI:

```
tt chips                            # every tenant, including agents that never serve
tt chips --history                  # who had the box this morning
tt run qwen3-8b --chips 2 --wait
tt stop qwen3-8b                    # releases + resets
tt evict <board> [--force] [--reason "..."]
```

**Swap** needs no dedicated verb: stop then start on the same board. Because gozer's release
resets exactly the leased chips, the incoming model gets clean silicon without a separate
`tt reset`.

## Eviction

The owner can preempt any tenant. Eviction is **graceful by default and always logged**.

| Tenant | How it is stopped |
|---|---|
| tt-station's own container | `docker stop` (respects the container's own grace), then release + reset |
| A peer's process, same user | `SIGTERM`, a grace period, then `SIGKILL` if it has not exited; then release + reset |
| A root-owned process agentd cannot signal | **Refuse and report honestly** — name the pid and command, and say that stopping it needs elevated privileges. Do not release the lease and pretend |

Rules:

* **Default grace is 30 seconds** between `SIGTERM` and `SIGKILL`, so a workload can
  checkpoint. `--force` shortens it to zero.
* **Every eviction writes a `evicted` event to gozer's history** naming who ordered it, from
  which client, why, and what was stopped. A local agent whose work was killed must be able
  to find out why, from the box, without asking anyone. This is the accountability that makes
  preemption acceptable rather than arbitrary.
* **Eviction never happens implicitly.** `tt run` queues when chips are busy. Taking work
  away from someone is always an explicit act.

The last row matters: refusing when agentd genuinely cannot stop a process is not politeness,
it is honesty. Releasing a lease whose process keeps running would produce a `BUSY-UNTRACKED`
board that nobody can allocate — worse than the state before.

## Failure modes

Every gozer exit code maps to a distinct state rather than a generic error:

| Code | agentd behaviour |
|---|---|
| 10 queued | serving request enters a queued state, ticket returned to the Mac |
| 12 unavailable | reported as such; `--no-queue` path |
| 14 topology unreadable | box health degraded; leasing disabled for this call, reported |
| 15 release refused | surfaced, **not** swallowed — the stop did not happen |
| 16 mutex stuck | box health signal the Mac can display, with the path to clear |

Crash cases resolve through label reconciliation: agentd killed mid-launch leaves a lease
with no container (released on restart); a container dying on its own is caught by the
existing supervisor (lease released); a box reboot clears `/tmp` and the containers together,
so both sides return consistent.

**A known gap inherited from gozer:** a lease reaped by gozer (rather than released) never
resets its chips, so the next plain `acquire` gets un-reset silicon. `--fresh` is protected.
This is documented in gozer's own known-limitations and is not made worse here, but agentd
should prefer explicit release over letting a lease be reaped.

## Testing

No hardware required on either side.

* agentd's gozer invocation is **injectable** — tests point it at a stub binary returning
  canned JSON, the same technique gozer used with `GOZER_RESET_CMD` to test resets without
  touching a device.
* The existing `mock-box` crate hosts the integration tests.
* **Both** paths are tested — leasing and the no-gozer fallback. The fallback protects
  existing users and is the one most likely to rot.
* Eviction tests cover all three tenant kinds, including the refuse-and-report case.
* A test asserts that `tt run` never preempts implicitly.

## Out of scope

Multi-box scheduling; moving a running session between boards (a container is pinned at
launch); replacing gozer's queue with a scheduler; GUI work in the macOS app or GTK panel
beyond what the `tt` CLI exposes; and any change to gozer's own leasing semantics — if this
integration wants different behaviour from gozer, that is a change to gozer's spec, made
there.
