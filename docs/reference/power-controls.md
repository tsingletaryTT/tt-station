# Box power controls (reset chips / suspend / reboot / shutdown / wake)

tt-station can power-manage a QuietBox from the agent's `POST /power` route, the `tt
power`/`tt wake` CLI, the macOS app's power menu, and the Linux box panel's local power row —
all backed by the same design
(`docs/superpowers/specs/2026-07-15-box-power-controls-design.md`). This doc is the reference
for operators wiring up or troubleshooting any of those surfaces, and for the polkit rule that
makes suspend/reboot/shutdown work without an interactive password prompt.

**Key semantic split, read this first:** the pre-existing `POST /reset` (design predates this
feature) runs a board reset **and unpairs** — it clears stored tokens, revokes any installed SSH
keys, and sets the box back to idle. It is a factory reset. Every action documented on this page
— including `reset-chips` — **preserves pairing**: `run_power_command`
(`crates/tt-station-agentd/src/routes.rs`) never touches tokens, SSH state, or the pairing store.
A rebooted or chip-reset box comes back still paired with every Mac/CLI that was paired before
(tokens are persisted to disk — see the project CLAUDE.md's agent section).

---

## 1. Agent: `POST /power`

**Route:** `POST /power`, bearer-guarded (same `BearerAuth` gate as `/run`/`/stop`/`/reset` — the
box must be paired first).

**Request body:**

```json
{ "action": "reset-chips" | "suspend" | "reboot" | "shutdown" }
```

`PowerAction::parse` (`crates/tt-station-agentd/src/power.rs`) is the single source of truth for
these four wire values; the CLI, the macOS `PowerAction` enum, and the panel's action strings all
match it.

**Behavior by action** (`AppState::run_power_command`):

- **`reset-chips`** — runs the configured board-reset command (`tt-smi -r` by default). Does
  **not** stop serving first (there's nothing to gracefully stop for a chip reset), does **not**
  clear tokens/SSH/pairing. Completes synchronously.
- **`suspend` / `reboot` / `shutdown`** (the "machine ops",
  `PowerAction::is_machine_op() == true` for everything but `reset-chips`) —
  1. Best-effort stop any serving container first (reuses the backend's normal `stop` path) so a
     model isn't hard-killed by the machine going down. A stop failure is logged but non-fatal —
     the box is going down regardless, so refusing the power action over a failed stop would just
     strand the operator.
  2. Shell the configured command (`["systemctl","suspend"]` / `["systemctl","reboot"]` /
     `["systemctl","poweroff"]` by default) via `spawn_blocking`.

**Status codes** (`power_success_status`):

| Result | Status | Body |
|---|---|---|
| Unknown `action` string | `400` | `{"error": "unknown power action: …"}"` — checked before any network/token/command work |
| `reset-chips` succeeds | `200` | `{}` — completes synchronously, so the caller can trust the response |
| `suspend`/`reboot`/`shutdown` succeeds | `202 Accepted` | `{"action": "...", "accepted": true}` — the command only *initiates* teardown; the box may go down before a `200` could ever be observed, so the response says "accepted," never "done" |
| Command fails with a permission/polkit-shaped error (message contains "Interactive authentication required", "Access denied", or "not authorized") | `403` | Points at this doc — see §6 below |
| Any other command failure (e.g. the binary itself is missing) | `500` | The generic `backend_error` fallback used by every other route |
| No bearer token / bad token | `401` | Standard `BearerAuth` rejection |

**Configurable commands.** All four power-command vectors are overridable via
`AppState::with_power_config` (mirroring the existing `reset_cmd` pattern) so `mock-box` and unit
tests inject a harmless stub (e.g. a script that just touches a marker file) and never touch real
power.

**Reset-chips vs. `POST /reset`, one more time:** if you want "forget this box, wipe pairing,
board reset" — that's the existing `/reset` (and `tt reset --host …`). If you want "clear a
wedged mesh without losing pairing" — that's `POST /power {"action":"reset-chips"}` (`tt power
reset-chips --host …`). They both ultimately run the same `tt-smi -r`; only the token/SSH/pairing
side effects differ.

---

## 2. `mac` in `/status` and the mDNS TXT record

The agent detects its primary LAN interface's MAC address once at startup
(`net::primary_mac`, `crates/tt-station-agentd/src/net.rs`) and reports it, best-effort:

- as `mac` (nullable) in `GET /status`'s JSON body, and
- as a `mac` key in the `_tenstorrent._tcp` mDNS TXT record (same mechanism as the existing
  `device_mesh` key).

Detection failing (interface not found, permissions, etc.) simply omits the field — `mac: null`
over HTTP, no `mac` key in the TXT record — never a startup failure. `tt --json discover` and `tt
--json status` therefore surface `mac` when known; the macOS app persists it into its box
registry at discovery/pair time so Wake still works after the box goes to sleep or powers off
(the box obviously can't answer `/status` once it's down, so the MAC must have been captured
while it was still up).

---

## 3. CLI: `tt power` / `tt wake`

### `tt power <reset-chips|suspend|reboot|shutdown> --host <host:port>`

Authed — resolves a stored bearer token for `--host` from the same token store `tt pair`/`tt
reset` use, and calls `POST /power`.

- `--host` is **required**. Unlike `tt wake`, this isn't a stopgap — the CLI has no default/
  only-paired-box resolution today (there's no persisted discovery cache to draw a default from),
  so every invocation must name the target box explicitly.
- The action string is validated against the known set client-side, before any network call —
  an unknown action fails fast with the list of valid actions rather than round-tripping to the
  agent first.
- A missing token for `--host` is a **hard error** ("no token stored for `<host>`; run `tt pair
  <host>` first") — unlike `tt reset`, there's no "clear local state anyway" fallback path for a
  box that was never paired.
- `--json` output: `{"action": "<action>", "ok": true}`. Human output: `power action '<action>'
  sent` — deliberately "sent," not "completed," since machine ops tend to drop the connection
  right after the agent accepts the request.

### `tt wake [--mac <aa:bb:cc:dd:ee:ff>] [--host <name>]`

Purely client-side — **no network call to the box at all**. Broadcasts a Wake-on-LAN magic
packet (`libttstation::wol::magic_packet`: 6× `0xFF` followed by the target MAC repeated 16
times, 102 bytes total) as a UDP datagram to `255.255.255.255:9` (port 9, the conventional WoL
target).

- `--mac` is **required** today. `--host` is accepted but currently informational only — `tt`
  doesn't persist a discovery cache between invocations, so there's no stored MAC to resolve
  `--host` against, and the box can't be asked live (it may be powered off, which is the entire
  point of Wake-on-LAN). A future discovery cache could let `--host` resolve this automatically;
  until then, get the MAC from a live `tt discover`/`tt status --json` run before the box goes to
  sleep.
- `--json` output: `{"mac": "<mac>", "sent": true}`. Human output: `Wake-on-LAN packet sent to
  <mac>` — "sent," never "woke," since a UDP broadcast has no delivery confirmation.
- Requires Wake-on-LAN enabled in the box's BIOS/NIC settings. Suspend→wake is the more reliable
  pair; shutdown→wake specifically needs WoL-from-off support in the hardware, which isn't
  universal.

---

## 4. macOS app: the power menu

`PowerMenuView` (`macos/TTStation/AppShell/Sources/PowerMenuView.swift`) is a single SwiftUI
`Menu` shared verbatim between the box header (`BoxHeaderView`) and the menu-bar popover
(`MenuContentView`), so power is reachable whether the window is open or not. Items, in order:
**Reset chips**, **Wake**, a divider, then the destructive three — **Suspend**, **Reboot…**,
**Shut Down…**.

- Reset chips and Wake fire immediately (neither is destructive to a live session: a chip reset
  keeps the agent up, and Wake has no effect on a box that's already up).
- Suspend/Reboot/Shut Down are `role: .destructive` and each opens a `.confirmationDialog` naming
  the concrete consequence before it fires (e.g. Reboot: "This stops the serving model and
  disconnects this Mac until the box is back.").
- `TTClient.power(action:)` / `.wake(mac:)` shell out to `tt --json power …` / `tt --json wake
  --mac …` — consistent with the app's "all control through `tt --json`" convention.
- **Expected-disconnect handling:** `BoxViewModel.powerState` (`PowerState`: `.suspending`,
  `.rebooting`, `.poweredOff`, `.waking`) is set the moment a power op is issued. While set, the
  telemetry/status connection dropping renders as the expected state ("Suspending…", "Rebooting…",
  "Powered off — Wake to bring it back"), not an error banner. For suspend/reboot the app keeps
  polling and clears `powerState` once the box reappears in `/status`; for shutdown it stays
  `.poweredOff` until a Wake (or the operator brings it back manually). The transition logic
  (`PowerTransition.next` / `.onReachabilityChange`, `TTStationKit/PowerControls.swift`) is pure
  and unit-tested; the menu/dialog views themselves are owner-verified SwiftUI/AppKit glue.

---

## 5. Linux box panel: the Power row

The GTK panel (`box-panel/tt-station-panel.py`) runs **on the box**, so its Power row shells
**local** commands directly rather than routing through the agent's `/power` — the panel is
already local and already shells `systemctl` for agent lifecycle, and doing the same here avoids
needing a pairing token just to press a button on the box's own screen. Both paths (the agent's
`/power` and the panel's local calls) rely on the *same* polkit rule — one privilege source, two
contexts.

Row: **Reset chips**, **Suspend**, **Reboot**, **Shut Down** — deliberately **no Wake button**
(waking the box from its own screen is meaningless; it's already awake to display the panel).

- Reset chips fires immediately, no confirmation (non-destructive to the OS session).
- Suspend/Reboot/Shut Down each show a `Gtk.MessageDialog` (question, OK/Cancel) naming the
  consequence before running.
- The pure argv-builder `power_command(action) -> list[str]`
  (`box-panel/panel_launchers.py`) maps `reset-chips → ["tt-smi","-r"]`,
  `suspend/reboot/shutdown → ["systemctl","suspend"|"reboot"|"poweroff"]`; unit-tested in
  `test_panel_launchers.py`. The panel itself is thin worker-thread glue (never runs a blocking
  `systemctl` call on the GTK main thread) — a missing `tt-smi`/`systemctl` binary surfaces via
  the panel's existing inline `_log` message, never a crash.

---

## 6. The polkit rule

`systemctl suspend|reboot|poweroff` (and the equivalent logind D-Bus calls) normally require an
interactive polkit authentication prompt for a non-root user — fine at a desktop, useless for a
headless agent or a box panel with no one there to type a password. tt-station ships a polkit
**rule** (not a full policy) that grants these specific actions unconditionally to a trusted
group.

### What it grants

`deploy/tt-station-power.rules`:

```javascript
// tt-station: let box operators power-manage this machine without an
// interactive auth prompt, so `POST /power` (agent) and the box panel's
// power row work headlessly. Grants ONLY the logind power actions, and only
// to members of the `sudo` group (the QuietBox run-user is in it). Retarget
// the group for a different setup — see docs/reference/power-controls.md.
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

Exactly six `org.freedesktop.login1` actions — reboot, reboot-multiple-sessions, power-off,
power-off-multiple-sessions, suspend, suspend-multiple-sessions — and nothing else. It does not
touch `reset-chips` (that's `tt-smi -r`, a plain process the run-user already has permission to
execute; no polkit action is involved).

### Where it installs

The `tt-station` `.deb` (not `tt-station-panel` — the rule is shipped in the base package since
both the agent and the panel depend on it) installs the file to:

```
/etc/polkit-1/rules.d/49-tt-station-power.rules
```

`debian/rules`' `override_dh_auto_install` copies `deploy/tt-station-power.rules` there
(mode 644, root-owned, same as every other file the package ships — no maintainer-script copy
step needed since dpkg lays out package-owned files directly). `debian/tt-station.postrm`
removes it explicitly on `purge` (files under `/etc/` are also auto-registered as conffiles by
debhelper's `dh_installdeb`, so dpkg itself already preserves the file across a plain `remove`
and clears it at `purge` — the explicit `rm -f` in postrm is a documented belt-and-suspenders
no-op on top of that, not the only thing standing between "purged" and "rule still present").

### The `sudo`-group default, and how to retarget it

The rule targets `subject.isInGroup("sudo")` because that's the group the QuietBox's normal
run-user (`ttuser`) is already in. If your setup uses a different group (or you want a tighter
grant, e.g. a dedicated `tt-station-power` group instead of the broad `sudo`), edit the installed
rule directly:

```bash
sudo sed -i 's/isInGroup("sudo")/isInGroup("your-group-name")/' \
    /etc/polkit-1/rules.d/49-tt-station-power.rules
sudo systemctl restart polkit   # or just wait — polkit picks up rules.d changes automatically
```

(polkit reloads `rules.d/*.rules` automatically on file changes in modern versions; restarting
the `polkit` service is the fallback if a change doesn't seem to take.) A future `.deb` upgrade
will overwrite this local edit — track any customization outside the package if it needs to
survive upgrades, or carry the retargeted rule as a local override in
`/etc/polkit-1/rules.d/` under a *different* filename with a lower sort prefix (rules load in
filename order; polkit uses the *first* matching rule that returns a non-`undefined` result).

### Manual install (non-`.deb` setups)

If you're running the agent/panel from source rather than the `.deb` (or on a distro where the
package hasn't been built), install the rule by hand:

```bash
sudo install -m 644 deploy/tt-station-power.rules \
    /etc/polkit-1/rules.d/49-tt-station-power.rules
```

No service restart is required in most setups — polkit watches `rules.d/` for changes. Remove it
the same way (`sudo rm /etc/polkit-1/rules.d/49-tt-station-power.rules`) if you ever need to.

### What happens when the rule is missing

- **The agent's `POST /power`:** a `suspend`/`reboot`/`shutdown` request fails with a permission-
  shaped error from `systemctl`, which the route maps to **`403`** with a message pointing back
  at this doc (see §1's status-code table). `reset-chips` is unaffected — it doesn't need polkit
  at all.
- **`tt console`:** the operator TUI (and its `--snapshot` JSON) checks whether
  `/etc/polkit-1/rules.d/49-tt-station-power.rules` exists on every snapshot collection
  (`console::env::collect_snapshot`, via `LifecycleEnv::polkit_power_rule_present` —
  `RealLifecycleEnv`'s default implementation is a plain `Path::exists()` check). When absent, a
  one-line, **non-fatal** advisory is added to `BoxLifecycleSnapshot.polkit_power_advisory` (JSON
  contract: `docs/reference/tt-console.md`) and shown as an extra line in the TUI's `status`
  panel:

  ```
  power controls need the polkit rule (missing: /etc/polkit-1/rules.d/49-tt-station-power.rules)
  -- see docs/reference/power-controls.md or install the tt-station .deb
  ```

  This is purely informational: it never blocks any `tt console` action, and every route/CLI
  command other than the three machine-power ops works identically whether the rule is present
  or not.
- **The Linux panel's Power row / `tt power suspend|reboot|shutdown`:** the underlying
  `systemctl` call fails the same way as above; the panel surfaces it via its normal inline
  `_log` message, `tt power` surfaces the agent's `403` (with the same doc pointer) as a CLI
  error.
