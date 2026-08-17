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
  a previous agentd process (see §5).

---

## 2. `tt leases` — who holds what

```
tt leases --host <host:port>            # one chip per line
tt leases --host <host:port> --json     # the whole LeaseList object
```

**Authed**, unlike `tt status`/`tt serving`/`tt models`: `GET /leases` is bearer-guarded, so the
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

## 3. `GET /status`'s `leasing` object, and `tt status`

`/status` gains a `leasing` object, and `tt status` prints it:

```
$ tt status --host qb2-lab.local:8765
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
| absent / `null` | that agent is too old to report it. `tt status` prints no leasing line at all rather than inventing "no leasing". |
| `{"available": false, ...}` (every other field `null`) | gozer is not installed on that box (or its startup probe failed). Never a guessed version or a fabricated board count. |
| `{"available": true, ...}` | version plus board/concurrency counts. |

`boards`/`max_concurrent` come from the same short-TTL `gozer status --json` parse `/leases` uses,
so asking for `/status` right after (or before) `/leases` costs no extra shell-out.

---

## 4. What changes on a leasing box

### `tt run` / `POST /run`

| | Without gozer | With gozer |
|---|---|---|
| chips used | the whole box | one board, whichever gozer grants |
| pre-serve reset | `tt-smi -r` (whole box) | `tt-smi -r <bdf>,<bdf>` — **only the leased BDFs** |
| device selector | `--device-id` from config; `--tt-device` auto-detected across the whole box | both derived from the grant; a configured value is overridden and the override is logged |
| box fully occupied | n/a | **`409 Conflict`**, naming the board and the holder |

The `409` is the important new outcome: a contended box is the box's *state* conflicting with the
request, not a failure, so it is retriable and distinguishable from a `500`. Its message names the
board and the holder — and no duration, per §2.

**Swapping models needs no explicit stop.** `tt run B` while A serves hands A's board back
(resetting it) before asking gozer for chips again, so a swap does not strand a board. The exact
sequence differs by backend:

| Backend | `start` sequence |
|---|---|
| `runpy` (default) | note the held lease → sweep whatever publishes `serving_port` → release that lease → acquire → **scoped** `tt-smi -r <bdf>,<bdf>` → launch |
| `docker` | note the held lease → stop *its recorded container* → release that lease → acquire → launch (**no** stale-container sweep and **no** board reset — this backend has never run one) |

`tt stop` first is still the clearer thing to do, and on a single-board box it is the same
sequence either way.

Only the lease this agent observed at the top of `start` is released. If a concurrent `POST /run`
records its own lease in the meantime, the swap declines to release, logs the supersession, and
leaves the newer lease alone — releasing it would reset the chips under a live container. Two
overlapping `/run`s are not otherwise serialised, so one can still leave a board stranded (wasted,
not reset); prefer one `tt run` at a time.

### `POST /reset` (`tt reset --host …`) and `power reset-chips` (`tt power reset-chips --host …`)

Both run a **whole-box** `tt-smi -r`, which on a shared box resets a neighbour mid-run. So both
now **refuse with `409`** while gozer reports a lease held by anyone other than this agent's own
session (matched on the `tt-station:<serving_port>:` `who` prefix). Full behaviour, including the
"could not be determined" refusal, is in
[`power-controls.md` §1](power-controls.md#1-agent-post-power).

Two things worth knowing at the CLI:

- **A refusal is not a failure.** `tt reset --host X` normally clears local pairing even when the
  box is unreachable. On a refusal it does **not**: the box reset nothing and deliberately kept
  your token, so clearing it would destroy the only credential that can reach the box in exchange
  for nothing. The command errors out with the box's own message instead.
- **A `STALE` or `HELD-FOREIGN` holder is already gone.** "Stop that session" doesn't apply, and
  you cannot `gozer release` it either, because `gozer status` reports no lease id. Run
  `gozer reconcile` on the box, then retry. The refusal message says so and names the state.

`BUSY-UNTRACKED` is deliberately **not** a refusal: it is not a lease, there is nobody to name, and
untracked work wedging the box is one of the main reasons to reach for a reset in the first place.

---

## 5. Restarts, and leases that outlive them

Every lease this agent takes is tied to agentd's own pid (`gozer acquire --owner-pid`), so gozer
never reaps one out from under a healthy serve.

At startup agentd does one **sweep**: for every lease whose `who` begins `tt-station:`, if nothing
is serving on the service port that `who` names, release it (which resets and returns the board).
A neighbour's lease is never touched, and a lease whose id cannot be resolved is reported in the
journal and left alone rather than guessed at.

A lease whose port **is** still serving is kept — and, if that port is this agent's own, **adopted**,
so the next `tt stop` releases it explicitly (resetting its chips) instead of leaving it to gozer's
reap, which would not. The journal says which:

```
tt-station-agentd: adopted lease 'ab12ef' (tt-station:8003:Qwen/Qwen3-32B) from a previous agentd process -- ...
tt-station-agentd: WARNING: UNOWNED LEASE 'ab12ef' (...) -- ... Clear it by hand with `gozer release ab12ef` ...
```

An adopted lease's `--owner-pid` names the dead process, so gozer may reap it first; that is
exactly why it is released explicitly at the first opportunity.

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
read". Restart agentd to re-probe and turn leasing back off. The refusal message says this too.

---

## 7. Things this version deliberately does not do

- **No queuing.** `acquire` is `--no-queue`: a contended `POST /run` reports who holds the chips
  and returns, rather than waiting in line. No ticket is left on disk to cancel.
- **No preemption or eviction.** Nothing in tt-station takes chips from a live tenant. That is why
  the reset paths refuse rather than proceed.
- **No leasing on the `dstack` backend.** The stub runs nothing and owns no command seam, so it
  could neither pin a lease to a workload nor hand one back; taking chips it cannot use would
  strand them.
- **No lease durations.** See §2.

---

## See also

- `docs/reference/power-controls.md` — the two whole-box reset paths and their `409` refusals.
- `docs/reference/agentd-config.md` — the rest of the config schema.
- `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` — design rationale, failure-mode
  table, and the open questions this integration carries.
- `crates/tt-station-agentd/src/gozer.rs` — module doc, including the known limitations recorded
  in the code a maintainer actually opens.
