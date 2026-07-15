# Box power & hardware controls — design

**Date:** 2026-07-15
**Status:** approved (brainstorm), pending implementation plan
**Topic:** Add tasteful power/hardware controls — reset chips, suspend, reboot,
shut down, and wake — reachable from the macOS app toolbar and the Linux box
panel, backed by new authed agent routes and a polkit rule.

---

## Problem

tt-station can discover, pair, run, and monitor a box, but there is no way to
**power-manage** it from the app. Operators want, from the toolbar:

- **Reset chips** — a routine `tt-smi -r` board reset (clears wedged mesh
  ethernet cores) that keeps the box paired.
- **Suspend / Reboot / Shut Down** — take the box down (sleep / restart / off).
- **Wake** — bring a suspended/off box back from the Mac.

These span every layer: the agent (execute on the box), the CLI (`tt`), the
macOS app (the toolbar), the Linux panel (the box's own screen), and packaging
(the privilege to power the box).

### The central complication

Powering a box **kills the very agent that received the request** and drops the
Mac's connection. So the design must: return a response *before* the box goes
down; treat the ensuing connection loss as an *expected* state, not an error;
and provide a recovery path (Wake) for boxes that are off. Wake is also
different in kind — the box's agent is off, so waking is a Wake-on-LAN magic
packet sent **from the Mac**, needing the box's MAC captured while it was still
reachable.

## Decisions (from brainstorming)

| Question | Decision |
|---|---|
| Operations | Reset chips, Suspend, Reboot, Shut Down (box-side) + Wake (WoL from Mac) |
| Privilege for reboot/shutdown/suspend | **Install a polkit rule** (via the `.deb` postinst) |
| Wake | **Include** — capture box MAC at discovery/pair, magic packet from the Mac |
| macOS placement | **Header power menu + mirrored popover submenu** |
| Confirmation | **Confirm dialog for Suspend/Reboot/Shut Down**; Reset chips + Wake fire directly |
| "Reset chips" vs existing `/reset` | **Two distinct ops** — "Reset chips" = `tt-smi -r` only, keeps pairing; existing `/reset` = "reset to fresh / unpair" stays as-is |
| Linux panel power ops | **Local `systemctl`** (permitted by the polkit rule), not routed through the agent |
| polkit rule target | The **`sudo` group** by default (overridable) |

**Key semantic split:** the existing `POST /reset` runs `tt-smi -r` **and**
unpairs (clears tokens, revokes installed SSH keys, sets idle) — it is a
*factory reset*. The new **"Reset chips"** does only the board reset and
**preserves pairing/SSH/tokens**. `/reset` is unchanged; the Linux panel's
current "Reset" button (reset-to-fresh) is unchanged.

---

## Architecture

Six components, each independently testable.

### 1. Agent — `POST /power` (authed)

New route on the same authed tier as `/reset` (BearerAuth). Request body:

```json
{ "action": "reset-chips" | "suspend" | "reboot" | "shutdown" }
```

Behavior by action:

- **`reset-chips`** — runs the board-reset command (`tt-smi -r`) via
  `spawn_blocking`. Does **not** clear tokens, revoke SSH, or set idle — pairing
  survives. Returns `200 {}` on success. (This is the routine op; the heavier
  unpairing lives only in the pre-existing `/reset`.)
- **`suspend` / `reboot` / `shutdown`** —
  1. Best-effort stop any serving container first (reuse the backend `stop`
     path so a model isn't hard-killed mid-flight; non-fatal on error).
  2. Shell the power command via `spawn_blocking`:
     `systemctl suspend` / `systemctl reboot` / `systemctl poweroff`.
     `systemctl` signals the manager asynchronously and returns quickly, so the
     handler can `await` it and still respond before teardown completes.
  3. Return **`202 Accepted`** with `{ "action": "...", "accepted": true }`.
  - Pairing/tokens/SSH are **preserved** (a reboot must come back paired —
     persist-tokens already survives a restart).
  - On a non-zero exit that looks like a permission failure, return
     **`403`** with a message pointing at the polkit rule
     (`docs/reference/power-controls.md`), distinct from a generic `500`.

**Configurable commands.** `AppState`/config gains power-command fields
(defaults: `["systemctl","suspend"]`, `["systemctl","reboot"]`,
`["systemctl","poweroff"]`, and the existing `reset_cmd` for `reset-chips`),
overridable via CLI flags — so `mock-box` and unit tests inject a fake command
(e.g. `true`/a script) and never touch real power. Mirrors the existing
`reset_cmd` pattern.

**Concurrency/idempotency.** A power op while one is in flight, or while the
backend is mid-serve, returns `409` with a clear message rather than racing.

### 2. Agent — advertise MAC for Wake-on-LAN

The agent detects its primary-interface MAC once at startup (the interface that
carries the box's LAN IP) and reports it:

- in `GET /status` as `mac` (nullable — omitted if undetectable), and
- in the mDNS `_tenstorrent._tcp` TXT record (same mechanism as `device_mesh`).

`tt --json discover`/`status` therefore carry `mac`, and the macOS app persists
it into `HostRegistry` at discovery/pair so Wake works when the box is off.
Detection is best-effort; a box whose MAC can't be read simply has Wake
disabled (documented), never a crash.

### 3. CLI (`tt`)

- **`tt power <reset-chips|suspend|reboot|shutdown>`** (authed) → `POST /power`.
  Honors global `--json` (prints the JSON response) and the standard
  host/token resolution. Human output: a one-line confirmation
  ("Rebooting tsingletaryTT-quietbox — the box will disconnect shortly.").
- **`tt wake [--mac <addr>] [--host <name>]`** — purely client-side. Resolves
  the target MAC from `--mac`, else the discovery cache / registry entry for
  `--host` (or the default box), then broadcasts a Wake-on-LAN **magic packet**
  (6× `0xFF` + 16× the MAC) as a UDP datagram to the broadcast address on port
  `9`. No box contact required. Errors clearly if no MAC is known
  ("run `tt discover` while the box is up, or pass --mac").

The magic-packet builder is a pure function (`libttstation`) unit-tested against
a known MAC → 102-byte payload; the socket send is thin glue.

### 4. macOS app — the power menu

- **`PowerMenuView`** — a SwiftUI `Menu` whose label is an understated
  `Image(systemName: "power")` (+ chevron), placed in `BoxHeaderView`. Items, in
  order: **Reset chips**, **Wake**, `Divider()`, **Suspend**, **Reboot…**,
  **Shut Down…**. The last three use `role: .destructive` and the ellipsis
  (they open a confirm). Disabled unless the box `isPaired`; **Wake** is enabled
  when the box is unreachable/off (and always shown), the others when reachable.
- **Mirror in the popover** — `MenuContentView` gains a `Power ▸` submenu with
  the same items, so power is reachable without opening the window.
- **Confirmation** — Suspend/Reboot/Shut Down present a `.confirmationDialog`
  naming the consequence; Reset chips and Wake fire immediately. Copy:
  - Reboot: "Reboot <box>? This stops the serving model and disconnects this
    Mac until the box is back."
  - Shut Down: "Shut Down <box>? This stops the serving model, disconnects this
    Mac, and powers the box off. Only Wake-on-LAN can bring it back."
  - Suspend: "Suspend <box>? This stops the serving model and sleeps the box;
    use Wake to resume."
- **Commands** — `TTClient` gains `power(action:)` (→ `tt power …`) and
  `wake(mac:)` (→ `tt wake --mac …`). Consistent with "all control through
  `tt --json`."
- **Expected-disconnect handling** — `BoxViewModel` gains a transient
  `powerState: PowerState?` (`.suspending`, `.rebooting`, `.poweredOff`,
  `.waking`), set when a power op is issued. While set:
  - the telemetry/status connection dropping is rendered as the *expected*
    state (header shows "Suspending… / Rebooting… / Powered off — Wake to bring
    it back"), **not** an error banner;
  - for reboot/suspend the app keeps the normal discovery poll running and
    clears `powerState` when the box reappears in `/status`;
  - for shutdown it stays `.poweredOff` until a Wake (or manual return).
  The state machine (inputs: issued action, subsequent reachability →
  resulting `PowerState`/cleared) is pure and unit-tested in `TTStationKit`.

### 5. Linux panel (GTK)

The panel runs **on the box**, so it adds a compact **Power** row (below the
existing Start/Stop/Restart/Reset agent-lifecycle row): **Reset chips**,
**Suspend**, **Reboot**, **Shut Down** — no Wake (meaningless locally). These
shell **local** commands (permitted by the polkit rule):

- Reset chips → `tt-smi -r`
- Suspend/Reboot/Shut Down → `systemctl suspend|reboot|poweroff`

Each destructive op (suspend/reboot/shutdown) is behind a `Gtk.MessageDialog`
confirm. The pure argv-builders (`power_command(action) -> list[str]`) live in
`box-panel/panel_launchers.py` with stdlib-unittest coverage
(`test_panel_launchers.py`); the panel is thin worker-thread glue, matching the
existing Connect-launcher pattern. Missing `systemctl`/`tt-smi` surfaces an
inline message, never a crash.

Rationale for local `systemctl` (not routing through the agent's `/power`): the
panel is already local and already shells `systemctl` for agent lifecycle; the
polkit rule grants it permission; and it avoids the panel needing a pairing
token. The agent's `/power` and the panel's local calls both rely on the same
polkit rule — one privilege source, two contexts.

### 6. Packaging — the polkit rule

A reviewable polkit rule at `deploy/tt-station-power.rules` (polkit ≥0.106 JS
rules syntax) granting these logind actions to members of a group (default
`sudo`; overridable at install):

```javascript
// Allow tt-station operators to power-manage this box without an auth prompt.
polkit.addRule(function(action, subject) {
    if (subject.isInGroup("sudo") && (
            action.id == "org.freedesktop.login1.reboot" ||
            action.id == "org.freedesktop.login1.reboot-multiple-sessions" ||
            action.id == "org.freedesktop.login1.power-off" ||
            action.id == "org.freedesktop.login1.power-off-multiple-sessions" ||
            action.id == "org.freedesktop.login1.suspend" ||
            action.id == "org.freedesktop.login1.suspend-multiple-sessions")) {
        return polkit.Result.YES;
    }
});
```

- Installed to `/etc/polkit-1/rules.d/49-tt-station-power.rules` by the
  **`tt-station` `.deb` postinst** (root). Removed on purge.
- For non-`.deb` installs, `tt console` detects the rule's absence (checks the
  path) and prints the one-line manual install; the agent's `403` on a denied
  power op also points here.
- Documented in `docs/reference/power-controls.md` (the routes, the `tt power` /
  `tt wake` CLI, the polkit rule + manual install, the `sudo`-group default and
  how to retarget it).

### Safety summary

- Every box-touching op (`/power`, `/reset`) is authed via the pairing token; no
  new unauthed surface (MAC in `/status` is not a secret and mirrors
  `device_mesh`).
- Suspend/Reboot/Shut Down confirm in both UIs.
- Power ops **preserve pairing**; only the pre-existing `/reset` unpairs.
- `tt-smi -r` / `systemctl` are shelled off the async runtime
  (`spawn_blocking`), with fake commands injected in tests/mock-box.

---

## Data flow

```
Reset chips:   app/panel → tt power reset-chips → POST /power → tt-smi -r (pairing kept) → 200
Reboot:        app → confirm → tt power reboot → POST /power → stop container → systemctl reboot → 202
                 app sets powerState=.rebooting → telemetry drop is "expected" → discovery poll
                 → box returns in /status → powerState cleared
Shut Down:     … → systemctl poweroff → 202 → powerState=.poweredOff (Wake enabled)
Wake:          app → tt wake --mac <box mac from registry> → UDP magic packet (broadcast:9)
                 → box powers on → reappears in discovery → powerState cleared
Panel (on box):Power row → confirm → local systemctl suspend|reboot|poweroff (polkit-permitted)
```

## Testing

- **Rust (agent):** `/power` handler — each action dispatches the configured
  (fake) command; `reset-chips` preserves tokens/SSH (unlike `/reset`);
  suspend/reboot/shutdown attempt container stop then run the power command and
  return 202; permission-style failure → 403; in-flight/serving → 409. Unauthed
  request → 401. MAC appears in `/status` when detectable, omitted otherwise.
  `mock-box` serves `/power` with a no-op command so CLI e2e exercises it.
- **Rust (libttstation):** magic-packet builder → exact 102-byte payload for a
  known MAC; MAC parse/validation (accepts `aa:bb:…`/`aa-bb-…`, rejects
  garbage).
- **Rust (CLI):** `tt power`/`tt wake` argument wiring and `--json` output via
  the mock-box e2e (no hardware); `tt wake` builds+sends to a loopback capture.
- **Swift (TTStationKit):** the `PowerState` transition function (issued action
  + reachability → state/cleared); `TTClient.power/wake` command construction.
  The menu views and confirm dialogs are owner-verified (SwiftUI/AppKit glue),
  not unit-tested — matching the `LaunchController` convention.
- **Python (panel):** `power_command(action)` argv-builder unit tests; the GTK
  glue is owner-verified.
- **Manual:** on the live box — reset-chips keeps pairing; reboot returns paired
  and the app clears `.rebooting`; shutdown → `.poweredOff` → `tt wake` powers
  it back; polkit rule present so no auth prompt; panel power row works locally.

## Risks / open items

- **polkit rule group.** Defaulting to `sudo` assumes the run-user is in it
  (true for `ttuser` on QB2). If not, reboot/shutdown/suspend get a silent
  PolicyKit denial → the agent's `403` and the doc cover retargeting the group.
- **Wake reliability.** WoL requires it enabled in the box's BIOS/NIC and a
  reachable broadcast domain; shutdown→wake specifically needs WoL-from-off
  support. Documented as an environment prerequisite; suspend→wake is the more
  reliable pair.
- **Suspend on TT hardware.** Blackhole/accelerator resume-from-suspend is not
  guaranteed clean; the confirm copy and docs note suspend is best-effort and
  reboot is the safer "get me a clean box" action.
- **Response-before-teardown race.** `systemctl reboot/poweroff` returns before
  the box is down, so the 202 flushes first in practice; if a teardown ever
  races the flush, the caller simply sees a dropped connection, which
  `powerState` already treats as expected — no wrong success/failure claim.
- **`.deb`-only polkit install.** Source/DMG-less Linux installs must run the
  documented manual step; `tt console` warns when the rule is missing.
