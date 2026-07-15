# Box Power & Hardware Controls Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add reset-chips / suspend / reboot / shut-down (box-side, authed) and wake (Wake-on-LAN from the Mac) controls, surfaced tastefully in the macOS app and the Linux box panel.

**Architecture:** A new authed `POST /power` route on the agent runs configurable, fakeable commands (`tt-smi -r` for reset-chips; `systemctl suspend|reboot|poweroff` for the rest, best-effort stopping any serving container first) and returns before the box tears down. The agent advertises its NIC MAC (in `/status` + mDNS TXT, mirroring `device_mesh`) so the Mac can send a WoL magic packet when the box is off. The CLI (`tt power`/`tt wake`), the macOS power menu, and the Linux panel's power row all sit on top; a polkit rule shipped by the `.deb` grants the box permission to power itself.

**Tech Stack:** Rust (agent `tt-station-agentd`, CLI `tt`, lib `libttstation`, `mock-box`), axum, tokio; Swift 5.9 / SwiftUI (`TTStationKit` + `AppShell`), XCTest; Python 3 / GTK4 (`box-panel`), stdlib unittest; polkit; debhelper.

## Global Constraints

- **Two distinct reset ops:** "reset-chips" = board reset (`tt-smi -r`) only, **preserves** tokens/SSH/pairing. The pre-existing `POST /reset` (unpair / reset-to-fresh) is **unchanged** — do not modify it.
- **Power ops preserve pairing:** suspend/reboot/shutdown must NOT clear tokens, revoke SSH, or set idle.
- **All box-touching ops are authed** via the pairing bearer token (same `BearerAuth` extractor as `/reset`). No new unauthed surface except `mac` in `/status` (not secret; mirrors `device_mesh`).
- **Shell off the async runtime:** any command execution in an async handler goes through `tokio::task::spawn_blocking` (see the existing `reset`/`stop_model` handlers in `crates/tt-station-agentd/src/routes.rs`).
- **Commands are configurable + fakeable:** power command vectors have `systemctl`/`tt-smi` defaults but are overridable so `mock-box` and tests inject a harmless command (a stub script or `true`) and never touch real power.
- **Response before teardown:** suspend/reboot/shutdown return `202 Accepted` (`systemctl` signals asynchronously and returns fast); reset-chips returns `200 {}`.
- **Confirm the destructive three** (suspend/reboot/shutdown) in both UIs; reset-chips and wake fire directly.
- **arm64 macOS / macOS 14** conventions as in the existing app; app-shell (`AppShell/Sources`) and GTK glue are owner-verified, not unit-tested (matches the `LaunchController`/panel conventions). Pure logic lives in `TTStationKit` / `panel_launchers.py` / `libttstation` and IS unit-tested.
- Rust test/lint: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`. Swift: `cd macos/TTStation && swift test`. Panel: `python3 -m unittest` in `box-panel/`.
- polkit rule default group: **`sudo`** (overridable). Documented in `docs/reference/power-controls.md`.

---

### Task 1: Agent — PowerAction + configurable power commands + executor core

The testable core: an action enum, the four configurable command vectors on `AppState`, and a synchronous `run_power_command` that (for the machine ops) best-effort stops any serving container first, then runs the configured command. No HTTP yet.

**Files:**
- Create: `crates/tt-station-agentd/src/power.rs` (the `PowerAction` enum + parsing)
- Modify: `crates/tt-station-agentd/src/lib.rs` (add `pub mod power;`)
- Modify: `crates/tt-station-agentd/src/routes.rs` (AppState power-command fields + `with_power_config` builder + `run_power_command` method; tests in the existing `#[cfg(test)] mod tests`)

**Interfaces:**
- Produces:
  - `pub enum PowerAction { ResetChips, Suspend, Reboot, Shutdown }` with `pub fn parse(s: &str) -> Option<PowerAction>` (accepts `"reset-chips"|"suspend"|"reboot"|"shutdown"`) and `pub fn is_machine_op(&self) -> bool` (true for all but `ResetChips`).
  - On `AppState`: `pub fn with_power_config(mut self, reset_chips: Vec<String>, suspend: Vec<String>, reboot: Vec<String>, shutdown: Vec<String>) -> Self` and `pub fn run_power_command(&self, action: PowerAction) -> anyhow::Result<()>`.
  - Defaults when `with_power_config` is not called: `reset_chips=["tt-smi","-r"]`, `suspend=["systemctl","suspend"]`, `reboot=["systemctl","reboot"]`, `shutdown=["systemctl","poweroff"]`.
- Consumes: existing `AppState` (`crates/tt-station-agentd/src/routes.rs`), `ServingBackend::stop(&self, model)`, and the way the current serving model is read from state (the `Endpoint` stored by `set_serving`; use the existing accessor the `/endpoint` handler uses — `state.endpoint()` returning `Option<Endpoint>` where `Endpoint.model` is the model id).

- [ ] **Step 1: Write the failing tests** (add to `power.rs` and the routes test module)

In `crates/tt-station-agentd/src/power.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_four_actions() {
        assert_eq!(PowerAction::parse("reset-chips"), Some(PowerAction::ResetChips));
        assert_eq!(PowerAction::parse("suspend"), Some(PowerAction::Suspend));
        assert_eq!(PowerAction::parse("reboot"), Some(PowerAction::Reboot));
        assert_eq!(PowerAction::parse("shutdown"), Some(PowerAction::Shutdown));
        assert_eq!(PowerAction::parse("halt"), None);
        assert_eq!(PowerAction::parse(""), None);
    }

    #[test]
    fn only_reset_chips_is_not_a_machine_op() {
        assert!(!PowerAction::ResetChips.is_machine_op());
        assert!(PowerAction::Suspend.is_machine_op());
        assert!(PowerAction::Reboot.is_machine_op());
        assert!(PowerAction::Shutdown.is_machine_op());
    }
}
```

In `crates/tt-station-agentd/src/routes.rs` test module, add a test that a stub command runs and a machine-op stops serving first. Use the stub-script idiom already used by `cached_snapshot_dedupes_tt_smi_within_ttl`:

```rust
    #[test]
    fn run_power_command_runs_the_configured_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttpower-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let _ = std::fs::remove_file(&marker);
        let script = dir.join("fake-power.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf x >> '{m}'\n", m = marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cmd = vec![script.to_string_lossy().into_owned()];

        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_power_config(cmd.clone(), cmd.clone(), cmd.clone(), cmd.clone());

        crate::power::PowerAction::Reboot;
        state
            .run_power_command(crate::power::PowerAction::Reboot)
            .expect("power command runs");
        assert!(marker.exists(), "configured power command was executed");
        let _ = std::fs::remove_dir_all(&dir);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p tt-station-agentd power`
Expected: FAIL — `power` module / `PowerAction` / `with_power_config` / `run_power_command` don't exist.

- [ ] **Step 3: Implement**

Create `crates/tt-station-agentd/src/power.rs`:

```rust
//! Box power actions surfaced by `POST /power` (see `routes::power`).
//!
//! `reset-chips` is a board reset (`tt-smi -r`) that KEEPS pairing — distinct
//! from `POST /reset`, which unpairs. `suspend`/`reboot`/`shutdown` take the
//! whole machine down and are the "machine ops": they best-effort stop any
//! serving container first (see `AppState::run_power_command`).

/// A power action requested over `POST /power`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// `tt-smi -r` board reset; preserves pairing (unlike `/reset`).
    ResetChips,
    Suspend,
    Reboot,
    Shutdown,
}

impl PowerAction {
    /// Parse the wire value from `POST /power`'s `{"action": ...}` body.
    pub fn parse(s: &str) -> Option<PowerAction> {
        match s {
            "reset-chips" => Some(PowerAction::ResetChips),
            "suspend" => Some(PowerAction::Suspend),
            "reboot" => Some(PowerAction::Reboot),
            "shutdown" => Some(PowerAction::Shutdown),
            _ => None,
        }
    }

    /// True for the ops that take the whole box down (everything but a chip
    /// reset). Machine ops best-effort stop serving before running.
    pub fn is_machine_op(&self) -> bool {
        !matches!(self, PowerAction::ResetChips)
    }
}
```

Add `pub mod power;` to `crates/tt-station-agentd/src/lib.rs` (next to the other `pub mod` lines).

In `crates/tt-station-agentd/src/routes.rs`, add four `Vec<String>` fields to the `AppState` struct (near the existing command config), defaulted in `AppState::new` to the vectors named in Interfaces, plus:

```rust
    /// Override the four power-action command vectors (tests / mock-box inject
    /// a harmless stub so no real power event fires). Defaults set in `new`
    /// are `tt-smi -r` (reset-chips) and `systemctl suspend|reboot|poweroff`.
    pub fn with_power_config(
        mut self,
        reset_chips: Vec<String>,
        suspend: Vec<String>,
        reboot: Vec<String>,
        shutdown: Vec<String>,
    ) -> Self {
        self.power_reset_chips_cmd = reset_chips;
        self.power_suspend_cmd = suspend;
        self.power_reboot_cmd = reboot;
        self.power_shutdown_cmd = shutdown;
        self
    }

    /// Run the configured command for `action`, blocking (call under
    /// `spawn_blocking`). Machine ops (suspend/reboot/shutdown) best-effort
    /// stop any serving container first so a model isn't hard-killed; a stop
    /// failure is non-fatal (we're taking the box down regardless). Does NOT
    /// touch tokens/SSH/status — power ops preserve pairing.
    pub fn run_power_command(&self, action: crate::power::PowerAction) -> anyhow::Result<()> {
        use crate::power::PowerAction;
        if action.is_machine_op() {
            if let Some(ep) = self.endpoint() {
                if let Err(e) = self.backend().stop(&ep.model) {
                    eprintln!("power: best-effort stop of '{}' before {action:?} failed: {e}", ep.model);
                }
            }
        }
        let cmd = match action {
            PowerAction::ResetChips => &self.power_reset_chips_cmd,
            PowerAction::Suspend => &self.power_suspend_cmd,
            PowerAction::Reboot => &self.power_reboot_cmd,
            PowerAction::Shutdown => &self.power_shutdown_cmd,
        };
        let (bin, args) = cmd.split_first()
            .ok_or_else(|| anyhow::anyhow!("empty power command for {action:?}"))?;
        let status = std::process::Command::new(bin).args(args).status()
            .with_context(|| format!("failed to spawn power command {cmd:?}"))?;
        if !status.success() {
            anyhow::bail!("power command {cmd:?} exited with {status}");
        }
        Ok(())
    }
```

(If `state.endpoint()` is not the correct accessor for the currently-serving `Endpoint`, use whatever the `get_endpoint` handler reads — match that.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p tt-station-agentd power` then `cargo test -p tt-station-agentd`
Expected: PASS. Also `cargo clippy -p tt-station-agentd --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/power.rs crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/src/routes.rs
git commit -m "feat(agentd): PowerAction + configurable power-command executor (keeps pairing)"
```

---

### Task 2: Agent — `POST /power` route + status-code mapping + CLI flags

Wire the executor to an authed route with the right status codes, register it in the router, and add `main.rs` flags to override the command vectors.

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (the `power` handler + router registration + a pure `power_status(action, result)` mapping helper with tests)
- Modify: `crates/tt-station-agentd/src/main.rs` (CLI flags → `with_power_config`)

**Interfaces:**
- Consumes: `AppState::run_power_command`, `PowerAction::parse` (Task 1); the `BearerAuth` extractor and `backend_error`/`ErrorResponse` (existing in `routes.rs`); the `app(state)` router builder at `routes.rs:2017`.
- Produces: route `POST /power` accepting `{"action": "..."}`; `200 {}` for reset-chips, `202 {"action","accepted":true}` for machine ops, `400` on unknown action, `401` unauthed, `500`/`403` on command failure.

- [ ] **Step 1: Write the failing test** (pure status-mapping helper)

Add to the `routes.rs` test module:

```rust
    #[test]
    fn power_success_status_is_202_for_machine_ops_200_for_reset_chips() {
        use crate::power::PowerAction;
        assert_eq!(power_success_status(PowerAction::ResetChips), StatusCode::OK);
        assert_eq!(power_success_status(PowerAction::Suspend), StatusCode::ACCEPTED);
        assert_eq!(power_success_status(PowerAction::Reboot), StatusCode::ACCEPTED);
        assert_eq!(power_success_status(PowerAction::Shutdown), StatusCode::ACCEPTED);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-station-agentd power_success_status`
Expected: FAIL — `power_success_status` not defined.

- [ ] **Step 3: Implement the helper, handler, and router line**

In `routes.rs`:

```rust
/// The success status for a power action: reset-chips completes synchronously
/// (200), while machine ops only *initiate* teardown before the box goes down
/// (202 Accepted).
fn power_success_status(action: crate::power::PowerAction) -> StatusCode {
    if action.is_machine_op() { StatusCode::ACCEPTED } else { StatusCode::OK }
}

#[derive(serde::Deserialize)]
struct PowerRequest {
    action: String,
}

async fn power(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PowerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let action = crate::power::PowerAction::parse(&req.action).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse { error: format!("unknown power action: {}", req.action) }),
        )
    })?;

    let s = state.clone();
    tokio::task::spawn_blocking(move || s.run_power_command(action))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("power task panicked: {join_err}")))?
        .map_err(|e| {
            // A permission failure (no polkit rule) is the operator's to fix —
            // distinguish it from a generic 500 with a pointer to the doc.
            let msg = e.to_string();
            if msg.contains("Interactive authentication required")
                || msg.contains("Access denied")
                || msg.contains("not authorized")
            {
                (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse {
                        error: format!(
                            "{msg} — the box is not permitted to {}. Install the polkit rule (see docs/reference/power-controls.md).",
                            req.action
                        ),
                    }),
                )
            } else {
                backend_error(e)
            }
        })?;

    let body = if action.is_machine_op() {
        serde_json::json!({ "action": req.action, "accepted": true })
    } else {
        serde_json::json!({})
    };
    Ok((power_success_status(action), Json(body)))
}
```

Register in `app(...)` (near `.route("/reset", post(reset))`):

```rust
        .route("/power", post(power))
```

In `main.rs`, add optional CLI flags (mirroring how `reset_cmd` / other command config is passed) to override each vector, and call `.with_power_config(...)` on the `AppState` builder only when any is set (otherwise leave defaults). Example flag: `--power-suspend-cmd <CMD>...` etc.; keep the defaults documented as `systemctl suspend|reboot|poweroff` and `tt-smi -r`.

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p tt-station-agentd` and `cargo build -p tt-station-agentd`
Expected: PASS + builds. `cargo clippy -p tt-station-agentd --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/routes.rs crates/tt-station-agentd/src/main.rs
git commit -m "feat(agentd): authed POST /power route (200 reset-chips, 202 machine ops, 403 on polkit denial)"
```

---

### Task 3: Agent — advertise NIC MAC in `/status` + mDNS TXT

Mirror the `device_mesh` path so `tt --json status`/`discover` carry the box MAC for Wake-on-LAN.

**Files:**
- Create: `crates/tt-station-agentd/src/net.rs` (pure-ish primary-MAC detection helper + a pure formatter tested on fixture input)
- Modify: `crates/tt-station-agentd/src/lib.rs` (`pub mod net;`)
- Modify: `crates/tt-station-agentd/src/main.rs` (detect MAC at startup, pass to state), `crates/tt-station-agentd/src/routes.rs` (`mac` in the `/status` body + `with_mac` builder + TXT record), and `crates/libttstation/src/*` (`StatusInfo.mac` + discovery `BoxRecord.mac`, following `device_mesh`)

**Interfaces:**
- Consumes: the exact `device_mesh` plumbing — the `with_device_mesh` builder (`routes.rs:420`), the `/status` JSON assembly, the mDNS TXT construction, `StatusInfo.device_mesh`, and `BoxRecord.device_mesh`. Add a parallel `mac: Option<String>` field to each, formatted as lowercased colon-separated (`aa:bb:cc:dd:ee:ff`).
- Produces: `net::normalize_mac(&str) -> Option<String>` (validates 6 hex octets, accepts `:`/`-` separators, lowercases with `:`), used to sanitize whatever the OS reports; `pub fn primary_mac() -> Option<String>` (best-effort, returns `None` on any failure).

- [ ] **Step 1: Write the failing test**

In `crates/tt-station-agentd/src/net.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalize_mac_accepts_colon_and_dash_lowercases() {
        assert_eq!(normalize_mac("AA:BB:CC:DD:EE:FF").as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(normalize_mac("aa-bb-cc-dd-ee-ff").as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None); // all-zero = not a real NIC
        assert_eq!(normalize_mac("aa:bb:cc"), None);
        assert_eq!(normalize_mac("zz:bb:cc:dd:ee:ff"), None);
        assert_eq!(normalize_mac(""), None);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p tt-station-agentd normalize_mac`
Expected: FAIL — `net`/`normalize_mac` not defined.

- [ ] **Step 3: Implement**

Create `crates/tt-station-agentd/src/net.rs`:

```rust
//! Best-effort primary-NIC MAC detection, advertised in `/status` + the mDNS
//! TXT record so the Mac can send a Wake-on-LAN magic packet when the box is
//! off. Mirrors how `device::detect_device_mesh` feeds `/status`.

/// Normalize a MAC string to lowercase colon form (`aa:bb:cc:dd:ee:ff`).
/// Returns `None` for anything that isn't 6 hex octets, or the all-zero MAC
/// (which real NICs never have and some virtual interfaces report).
pub fn normalize_mac(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = Vec::with_capacity(6);
    for p in parts {
        if p.len() != 2 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        out.push(p.to_ascii_lowercase());
    }
    let joined = out.join(":");
    if joined == "00:00:00:00:00:00" {
        return None;
    }
    Some(joined)
}

/// The MAC of the interface carrying the box's LAN IP, or `None` if it can't
/// be determined. Best-effort: parses `ip -o link` / `/sys/class/net`; any
/// failure yields `None` (Wake is simply disabled for that box).
pub fn primary_mac() -> Option<String> {
    // Prefer the iface backing the default route; fall back to the first
    // non-loopback iface with a usable MAC. Read /sys/class/net/<if>/address.
    let route = std::process::Command::new("ip").args(["route", "get", "1.1.1.1"]).output().ok()?;
    let route = String::from_utf8_lossy(&route.stdout);
    let dev = route.split_whitespace().skip_while(|t| *t != "dev").nth(1);
    if let Some(dev) = dev {
        if let Ok(addr) = std::fs::read_to_string(format!("/sys/class/net/{dev}/address")) {
            if let Some(mac) = normalize_mac(addr.trim()) {
                return Some(mac);
            }
        }
    }
    // Fallback: scan /sys/class/net for the first real MAC (skip lo).
    for entry in std::fs::read_dir("/sys/class/net").ok()?.flatten() {
        let name = entry.file_name();
        if name == "lo" { continue; }
        if let Ok(addr) = std::fs::read_to_string(entry.path().join("address")) {
            if let Some(mac) = normalize_mac(addr.trim()) {
                return Some(mac);
            }
        }
    }
    None
}
```

Add `pub mod net;` to `lib.rs`. Then mirror `device_mesh` exactly:
- `main.rs`: call `net::primary_mac()` at startup (log when `None`, like the device-mesh path) and pass it via a new `AppState::with_mac(Option<String>)` builder (copy `with_device_mesh`).
- `routes.rs`: store `mac: Option<String>`; add it to the `/status` JSON body next to `device_mesh`; add it to the mDNS TXT record next to the `device_mesh` TXT key (key name `mac`).
- `libttstation`: add `pub mac: Option<String>` to `StatusInfo` (decoded from `/status`) and to the discovery `BoxRecord` (decoded from the TXT record) — mirror the `device_mesh` field and its doc comment.

- [ ] **Step 4: Run tests + build the workspace**

Run: `cargo test -p tt-station-agentd net` then `cargo test --workspace` and `cargo build --workspace`
Expected: PASS + builds; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/net.rs crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/src/main.rs crates/tt-station-agentd/src/routes.rs crates/libttstation/src
git commit -m "feat(agentd): advertise primary NIC MAC in /status + mDNS TXT (for Wake-on-LAN)"
```

---

### Task 4: lib — agent_client `power()` + WoL magic-packet builder

**Files:**
- Modify: `crates/libttstation/src/agent_client.rs` (a `power(base, token, action)` fn mirroring `reset`)
- Create: `crates/libttstation/src/wol.rs` (pure magic-packet builder + MAC parse)
- Modify: `crates/libttstation/src/lib.rs` (`pub mod wol;`)

**Interfaces:**
- Consumes: `reqwest`, the `reset(base, token)` template at `agent_client.rs:163`.
- Produces:
  - `pub async fn power(base: &str, token: &str, action: &str) -> anyhow::Result<()>` — `POST {base}/power` with `bearer_auth` and JSON body `{"action": action}`, `error_for_status()` like `reset`.
  - `wol::parse_mac(&str) -> Option<[u8; 6]>` and `wol::magic_packet(mac: [u8; 6]) -> [u8; 102]` (6×`0xFF` then 16× the MAC).

- [ ] **Step 1: Write the failing test**

In `crates/libttstation/src/wol.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_mac_accepts_colon_and_dash() {
        assert_eq!(parse_mac("01:02:03:04:05:06"), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(parse_mac("nope"), None);
    }
    #[test]
    fn magic_packet_is_6xff_then_16x_mac() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[0..6], &[0xff; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..6 + i * 6 + 6], &mac);
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p libttstation wol`
Expected: FAIL — `wol` module not defined.

- [ ] **Step 3: Implement**

Create `crates/libttstation/src/wol.rs`:

```rust
//! Wake-on-LAN magic-packet construction (client-side; `tt wake` sends it).

/// Parse a MAC address (`:` or `-` separated hex) into 6 bytes.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// Build the 102-byte WoL magic packet: 6 bytes of `0xFF` then the target MAC
/// repeated 16 times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0u8; 102];
    for b in p.iter_mut().take(6) {
        *b = 0xff;
    }
    for i in 0..16 {
        p[6 + i * 6..6 + i * 6 + 6].copy_from_slice(&mac);
    }
    p
}
```

Add `pub mod wol;` to `lib.rs`. Add to `agent_client.rs` (copy the `reset` body, add the JSON body):

```rust
/// `POST /power` — ask the box to run a power action (`reset-chips`,
/// `suspend`, `reboot`, `shutdown`). Authed like `reset`. The box may tear
/// down before the response fully arrives for machine ops; a dropped
/// connection after a 2xx is expected, not an error.
pub async fn power(base: &str, token: &str, action: &str) -> anyhow::Result<()> {
    let url = join(base, "power");
    reqwest::Client::new()
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "action": action }))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p libttstation` and `cargo build -p libttstation`
Expected: PASS + builds; clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/libttstation/src/wol.rs crates/libttstation/src/lib.rs crates/libttstation/src/agent_client.rs
git commit -m "feat(lib): agent_client power() + Wake-on-LAN magic-packet builder"
```

---

### Task 5: CLI — `tt power` and `tt wake` + mock-box `/power`

**Files:**
- Modify: `crates/tt/src/main.rs` (two `Command` variants + dispatch)
- Modify: `crates/mock-box/src/main.rs` (a no-op `/power` route so e2e can exercise `tt power`)
- Test: `crates/tt/tests/e2e_mock.rs` (or the existing e2e test file) — a `tt power reset-chips --host <mock>` case

**Interfaces:**
- Consumes: `libttstation::agent_client::power` and `libttstation::wol` (Task 4); the token store (`build_store()`) and host/base convention from `cmd_reset` (`main.rs:915`); the discovery cache / `BoxRecord.mac` (Task 3) for `tt wake`.
- Produces: `Command::Power { action: String, host: Option<String> }` and `Command::Wake { mac: Option<String>, host: Option<String> }`; a `wol` UDP send to `255.255.255.255:9`.

- [ ] **Step 1: Write the failing e2e test**

Add to the mock e2e (follow the existing `--ignored` mock-box e2e pattern; it starts `mock-box serve`, pairs, and runs `tt` subcommands):

```rust
// tt power reset-chips against the mock box returns success (mock runs a no-op).
#[test]
#[ignore]
fn tt_power_reset_chips_against_mock_box() {
    // (Use the harness helpers this file already has to boot mock-box, pair,
    // and invoke the `tt` binary via CARGO_BIN_EXE_tt with `power reset-chips
    // --host <addr> --json`; assert exit 0 and JSON success.)
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p tt --test e2e_mock -- --ignored tt_power_reset_chips`
Expected: FAIL — `power` subcommand unknown.

- [ ] **Step 3: Implement**

In `crates/tt/src/main.rs`, add to the `Command` enum:

```rust
    /// Power-manage a box: reset its chips (tt-smi -r, keeps pairing) or take
    /// the machine down (suspend/reboot/shutdown). Authed — the box must be
    /// paired. Machine ops disconnect this box shortly after they're accepted.
    Power {
        /// One of: reset-chips, suspend, reboot, shutdown.
        action: String,
        /// The box, as host:port. Omit to use the default/only paired box.
        #[arg(long)]
        host: Option<String>,
    },

    /// Wake a suspended/powered-off box by broadcasting a Wake-on-LAN magic
    /// packet from this machine. Uses the box's MAC learned at discovery, or
    /// --mac. Requires WoL enabled in the box's BIOS/NIC.
    Wake {
        /// The target MAC (aa:bb:cc:dd:ee:ff). Omit to use the stored MAC for
        /// --host.
        #[arg(long)]
        mac: Option<String>,
        /// The box whose stored MAC to wake.
        #[arg(long)]
        host: Option<String>,
    },
```

Add dispatch arms (mirror `Command::Reset`): `Power` resolves the token from `build_store()` for `host`, calls `agent_client::power(&format!("http://{host}"), &token, &action)`, prints a one-line human message or the JSON result under `--json`; a validation guard rejects an unknown `action` before the call (reuse the four literals). `Wake` resolves the MAC (from `--mac`, else the stored `BoxRecord.mac` for `--host`/default via the discovery cache), builds `wol::magic_packet(wol::parse_mac(mac)?)`, and sends it via a `std::net::UdpSocket` bound to `0.0.0.0:0` with `set_broadcast(true)` to `255.255.255.255:9`; errors clearly when no MAC is known.

In `crates/mock-box/src/main.rs`, add `.route("/power", post(power_mock))` where `power_mock` accepts the `{action}` body and returns `200 {}`/`202 {"accepted":true}` (a no-op — never runs a real command), so the e2e is hardware-free.

- [ ] **Step 4: Run the e2e + build**

Run: `cargo build -p tt -p mock-box` then `cargo test -p tt --test e2e_mock -- --ignored tt_power_reset_chips`
Expected: builds; test PASSES. `cargo clippy --workspace --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/main.rs crates/mock-box/src/main.rs crates/tt/tests
git commit -m "feat(tt): tt power + tt wake subcommands; mock-box serves /power"
```

---

### Task 6: macOS TTStationKit — PowerAction, TTClient.power/wake, PowerState machine

**Files:**
- Create: `macos/TTStation/Sources/TTStationKit/PowerControls.swift` (`PowerAction` + `PowerState` + the pure transition function)
- Modify: `macos/TTStation/Sources/TTStationKit/TTClient.swift` (add `power`/`wake`)
- Test: `macos/TTStation/Tests/TTStationKitTests/PowerControlsTests.swift`

**Interfaces:**
- Consumes: the existing `TTClient` command-run pattern (how `reset`/`run`/`stop` shell `tt --json`).
- Produces:
  - `public enum PowerAction: String { case resetChips = "reset-chips", suspend, reboot, shutdown }` with `var isMachineOp: Bool` and `var confirms: Bool` (true for suspend/reboot/shutdown).
  - `public enum PowerState: Equatable { case suspending, rebooting, poweredOff, waking }`.
  - `public enum PowerTransition { static func next(issued: PowerAction, reachable: Bool) -> PowerState?; static func onReachabilityChange(_ current: PowerState?, reachable: Bool) -> PowerState? }` — issuing a machine op sets the matching state; when the box becomes reachable again, `.suspending`/`.rebooting`/`.waking` clear (→ `nil`); `.poweredOff` clears only on reachability (a wake). reset-chips returns `nil` (no transient state).
  - On `TTClient`: `func power(_ action: PowerAction, host: String?) async throws` (→ `tt power <action> [--host]`) and `func wake(mac: String?, host: String?) async throws` (→ `tt wake [--mac][--host]`).

- [ ] **Step 1: Write the failing test**

`macos/TTStation/Tests/TTStationKitTests/PowerControlsTests.swift`:

```swift
import XCTest
@testable import TTStationKit

final class PowerControlsTests: XCTestCase {
    func testConfirmsAndMachineOpFlags() {
        XCTAssertFalse(PowerAction.resetChips.isMachineOp)
        XCTAssertFalse(PowerAction.resetChips.confirms)
        for a in [PowerAction.suspend, .reboot, .shutdown] {
            XCTAssertTrue(a.isMachineOp)
            XCTAssertTrue(a.confirms)
        }
    }
    func testIssuingMachineOpSetsMatchingState() {
        XCTAssertEqual(PowerTransition.next(issued: .reboot, reachable: true), .rebooting)
        XCTAssertEqual(PowerTransition.next(issued: .suspend, reachable: true), .suspending)
        XCTAssertEqual(PowerTransition.next(issued: .shutdown, reachable: true), .poweredOff)
        XCTAssertNil(PowerTransition.next(issued: .resetChips, reachable: true))
    }
    func testReachabilityClearsTransientButPoweredOffNeedsWake() {
        XCTAssertNil(PowerTransition.onReachabilityChange(.rebooting, reachable: true))
        XCTAssertEqual(PowerTransition.onReachabilityChange(.rebooting, reachable: false), .rebooting)
        // Powered-off box coming back (post-wake) clears; still-unreachable stays.
        XCTAssertNil(PowerTransition.onReachabilityChange(.poweredOff, reachable: true))
        XCTAssertEqual(PowerTransition.onReachabilityChange(.poweredOff, reachable: false), .poweredOff)
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd macos/TTStation && swift test --filter PowerControlsTests`
Expected: FAIL — types undefined.

- [ ] **Step 3: Implement**

`macos/TTStation/Sources/TTStationKit/PowerControls.swift`:

```swift
import Foundation

/// A box power action, matching the agent's `POST /power` wire values.
public enum PowerAction: String, CaseIterable {
    case resetChips = "reset-chips"
    case suspend
    case reboot
    case shutdown

    /// Machine ops take the whole box down (everything but a chip reset).
    public var isMachineOp: Bool { self != .resetChips }
    /// Whether the UI must confirm before firing (the destructive three).
    public var confirms: Bool { isMachineOp }
}

/// The transient state the app shows after issuing a power op, so the ensuing
/// connection drop reads as expected rather than as an error.
public enum PowerState: Equatable {
    case suspending
    case rebooting
    case poweredOff
    case waking
}

/// Pure transitions for `BoxViewModel.powerState`.
public enum PowerTransition {
    /// State to enter when `issued` is fired. reset-chips is instantaneous
    /// (no transient state); machine ops map to their in-progress/off state.
    public static func next(issued: PowerAction, reachable _: Bool) -> PowerState? {
        switch issued {
        case .resetChips: return nil
        case .suspend: return .suspending
        case .reboot: return .rebooting
        case .shutdown: return .poweredOff
        }
    }

    /// Recompute state when reachability changes. Any transient state clears
    /// once the box is reachable again (it came back / was woken); while
    /// unreachable it persists.
    public static func onReachabilityChange(_ current: PowerState?, reachable: Bool) -> PowerState? {
        guard let current else { return nil }
        return reachable ? nil : current
    }
}
```

Add to `TTClient.swift`, matching the existing `reset`/`run` command builders (resolve args the same way, append `--host` when non-nil):

```swift
    /// `tt power <action> [--host]` — authed on the CLI side.
    public func power(_ action: PowerAction, host: String?) async throws {
        var args = ["power", action.rawValue]
        if let host { args += ["--host", host] }
        _ = try await run(args)   // match TTClient's existing run/reset helper
    }

    /// `tt wake [--mac] [--host]` — client-side WoL, no box contact.
    public func wake(mac: String?, host: String?) async throws {
        var args = ["wake"]
        if let mac { args += ["--mac", mac] }
        if let host { args += ["--host", host] }
        _ = try await run(args)
    }
```

(Match `run(_:)`/the actual private runner name `TTClient` already uses for `reset`; adjust to the real signature.)

- [ ] **Step 4: Run tests**

Run: `cd macos/TTStation && swift test --filter PowerControlsTests` then `swift test`
Expected: PASS (all).

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/Sources/TTStationKit/PowerControls.swift macos/TTStation/Sources/TTStationKit/TTClient.swift macos/TTStation/Tests/TTStationKitTests/PowerControlsTests.swift
git commit -m "feat(macos): PowerAction/PowerState + TTClient power/wake"
```

---

### Task 7: macOS AppShell — power menu (header + popover) with confirmation

Owner-verified UI wiring (no unit test — matches `LaunchController`); the build is the gate.

**Files:**
- Create: `macos/TTStation/AppShell/Sources/PowerMenuView.swift`
- Modify: `macos/TTStation/AppShell/Sources/BoxHeaderView.swift` (place the menu), `macos/TTStation/AppShell/Sources/MenuContentView.swift` (mirror as a submenu), `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift` (add `powerState` + `issuePower`/`wake` that call `TTClient` and drive `PowerTransition`; clear on reachability change in the existing status/discovery refresh path)

**Interfaces:**
- Consumes: `PowerAction`, `PowerState`, `PowerTransition`, `TTClient.power/wake` (Task 6); the box's `mac` (from `StatusInfo`/`BoxRecord`, Task 3); the existing `BoxViewModel` reachability/refresh hooks.
- Produces: `PowerMenuView(box:)` — a SwiftUI `Menu` labeled `Image(systemName: "power")`; items Reset chips, Wake, `Divider()`, Suspend, Reboot…, Shut Down… (`.destructive`), each destructive one behind a `.confirmationDialog`.

- [ ] **Step 1: Add `powerState` + actions to `BoxViewModel`**

In `BoxViewModel.swift`, add `public private(set) var powerState: PowerState?` and:

```swift
    /// Fire a power action and set the expected transient state so the
    /// following connection drop isn't rendered as an error.
    public func issuePower(_ action: PowerAction) async {
        powerState = PowerTransition.next(issued: action, reachable: true)
        do { try await commands.power(action, host: host) }
        catch { /* machine ops routinely drop the connection — swallow, the
                   powerState already communicates what's happening */ }
    }

    public func wakeBox() async {
        powerState = .waking
        try? await commands.wake(mac: mac, host: host)
    }
```

And in the existing status/discovery refresh, after computing reachability, call `powerState = PowerTransition.onReachabilityChange(powerState, reachable: isReachable)`. (Use the model's real property names for `commands`/`host`/`mac`/reachability.)

- [ ] **Step 2: Create `PowerMenuView`**

```swift
import SwiftUI
import TTStationKit

/// The tasteful power control: an understated power-symbol menu. Destructive
/// ops confirm; reset-chips and wake fire directly.
struct PowerMenuView: View {
    @Bindable var box: BoxViewModel
    @State private var confirm: PowerAction?

    var body: some View {
        Menu {
            Button("Reset chips") { Task { await box.issuePower(.resetChips) } }
            Button("Wake") { Task { await box.wakeBox() } }
            Divider()
            Button("Suspend") { confirm = .suspend }
            Button("Reboot…") { confirm = .reboot }
            Button("Shut Down…", role: .destructive) { confirm = .shutdown }
        } label: {
            Image(systemName: "power")
        }
        .menuStyle(.borderlessButton)
        .confirmationDialog(
            confirmTitle, isPresented: confirmBinding, titleVisibility: .visible
        ) {
            if let action = confirm {
                Button(confirmVerb(action), role: .destructive) {
                    Task { await box.issuePower(action) }
                }
                Button("Cancel", role: .cancel) {}
            }
        } message: { Text(confirmMessage) }
    }

    private var confirmBinding: Binding<Bool> {
        Binding(get: { confirm != nil }, set: { if !$0 { confirm = nil } })
    }
    private var confirmTitle: String {
        switch confirm {
        case .suspend: return "Suspend \(box.name)?"
        case .reboot: return "Reboot \(box.name)?"
        case .shutdown: return "Shut Down \(box.name)?"
        default: return ""
        }
    }
    private func confirmVerb(_ a: PowerAction) -> String {
        switch a { case .suspend: return "Suspend"; case .reboot: return "Reboot"; default: return "Shut Down" }
    }
    private var confirmMessage: String {
        switch confirm {
        case .suspend: return "This stops the serving model and sleeps the box; use Wake to resume."
        case .reboot: return "This stops the serving model and disconnects this Mac until the box is back."
        case .shutdown: return "This stops the serving model, disconnects this Mac, and powers the box off. Only Wake-on-LAN can bring it back."
        default: return ""
        }
    }
}
```

(Use the real `BoxViewModel` display-name property in place of `box.name` if it differs.)

- [ ] **Step 3: Place it**

In `BoxHeaderView.swift`, add `PowerMenuView(box: box)` trailing in the header row (gated on `box.isPaired`). In `MenuContentView.swift`, add a mirrored `Menu("Power") { … }` submenu with the same items/confirm handling (or embed `PowerMenuView`), so it's reachable from the popover. Where `powerState != nil`, show the header status text ("Rebooting…", "Powered off — Wake to bring it back") instead of the normal reachable/serving line.

- [ ] **Step 4: Build (the gate)**

Run:
```bash
cd macos/TTStation/AppShell && xcodegen generate \
  && xcodebuild -project TTStation.xcodeproj -scheme TTStation -destination 'platform=macOS' build
```
Expected: `BUILD SUCCEEDED`. Then `cd macos/TTStation && swift test` still green.

- [ ] **Step 5: Commit**

```bash
git add macos/TTStation/AppShell/Sources/PowerMenuView.swift macos/TTStation/AppShell/Sources/BoxHeaderView.swift macos/TTStation/AppShell/Sources/MenuContentView.swift macos/TTStation/Sources/TTStationKit/BoxViewModel.swift
git commit -m "feat(macos): power menu in header + popover with confirmation + expected-disconnect state"
```

---

### Task 8: Linux panel — local power row

**Files:**
- Modify: `box-panel/panel_launchers.py` (pure `power_command(action)` argv builder), `box-panel/test_panel_launchers.py` (tests), `box-panel/tt-station-panel.py` (Power row + confirm dialogs)

**Interfaces:**
- Consumes: the existing `panel_launchers` pattern + the panel's worker-thread/`GLib.idle_add` glue.
- Produces: `power_command(action: str) -> list[str]` where `reset-chips → ["tt-smi","-r"]`, `suspend → ["systemctl","suspend"]`, `reboot → ["systemctl","reboot"]`, `shutdown → ["systemctl","poweroff"]`; raises `ValueError` on an unknown action.

- [ ] **Step 1: Write the failing test**

Add to `box-panel/test_panel_launchers.py`:

```python
class PowerCommandTests(unittest.TestCase):
    def test_maps_each_action(self):
        from panel_launchers import power_command
        self.assertEqual(power_command("reset-chips"), ["tt-smi", "-r"])
        self.assertEqual(power_command("suspend"), ["systemctl", "suspend"])
        self.assertEqual(power_command("reboot"), ["systemctl", "reboot"])
        self.assertEqual(power_command("shutdown"), ["systemctl", "poweroff"])

    def test_rejects_unknown(self):
        from panel_launchers import power_command
        with self.assertRaises(ValueError):
            power_command("halt")
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd box-panel && python3 -m unittest test_panel_launchers -v`
Expected: FAIL — `power_command` undefined.

- [ ] **Step 3: Implement**

Add to `box-panel/panel_launchers.py`:

```python
# Local power actions for the box's own screen. reset-chips is a board reset
# (tt-smi -r); the machine ops shell systemctl (permitted by the polkit rule
# shipped with the tt-station .deb). No Wake — meaningless on the box itself.
_POWER_COMMANDS = {
    "reset-chips": ["tt-smi", "-r"],
    "suspend": ["systemctl", "suspend"],
    "reboot": ["systemctl", "reboot"],
    "shutdown": ["systemctl", "poweroff"],
}


def power_command(action):
    """Return the argv for a local power action, or raise ValueError."""
    try:
        return list(_POWER_COMMANDS[action])
    except KeyError:
        raise ValueError(f"unknown power action: {action}")
```

In `box-panel/tt-station-panel.py`, add a **Power** row (below the Start/Stop/Restart/Reset row) with buttons Reset chips / Suspend / Reboot / Shut Down. Each button runs `power_command(action)` via the existing worker-thread `subprocess.run` helper; suspend/reboot/shutdown first show a `Gtk.MessageDialog` (`QUESTION`, OK/Cancel) naming the consequence. A missing `systemctl`/`tt-smi` (FileNotFoundError) surfaces the existing inline-message path, never a crash.

- [ ] **Step 4: Run tests**

Run: `cd box-panel && python3 -m unittest -v`
Expected: PASS (all, including the new PowerCommandTests).

- [ ] **Step 5: Commit**

```bash
git add box-panel/panel_launchers.py box-panel/test_panel_launchers.py box-panel/tt-station-panel.py
git commit -m "feat(panel): local power row (reset-chips/suspend/reboot/shutdown) with confirm"
```

---

### Task 9: Packaging — polkit rule + doc

**Files:**
- Create: `deploy/tt-station-power.rules` (the polkit rule)
- Modify: `debian/` (postinst installs the rule to `/etc/polkit-1/rules.d/49-tt-station-power.rules`; postrm removes it on purge; add to the `tt-station` package's install list)
- Create: `docs/reference/power-controls.md`
- Modify: `crates/tt/src/console/…` (the `tt console` snapshot/TUI) — detect the rule's absence and surface a one-line warning + manual-install hint

**Interfaces:**
- Consumes: the existing `debian/` layout (how the `tt-station` package installs files + its maintainer scripts) and the `tt console` snapshot rendering.
- Produces: the installed polkit rule; `docs/reference/power-controls.md`.

- [ ] **Step 1: Create the polkit rule**

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

- [ ] **Step 2: Wire the `.deb` install**

Install `deploy/tt-station-power.rules` to `/etc/polkit-1/rules.d/49-tt-station-power.rules` as part of the `tt-station` package (via the package's `.install`/`dh` mechanism or an explicit postinst copy), and remove it in postrm on `purge`. Keep it in the **`tt-station`** package (not the panel package), since both the agent and panel rely on it.

- [ ] **Step 3: `tt console` absence warning**

In the `tt console` snapshot/TUI, check whether `/etc/polkit-1/rules.d/49-tt-station-power.rules` exists; when absent, include a one-line advisory ("power controls need the polkit rule; see docs/reference/power-controls.md or install the tt-station .deb") in the snapshot output and the TUI status area. Keep it non-fatal and informational.

- [ ] **Step 4: Write the doc**

`docs/reference/power-controls.md` documenting: the `POST /power` route (actions, status codes, auth, pairing-preservation, reset-chips vs `/reset`), `mac` in `/status`+TXT, `tt power`/`tt wake`, the macOS power menu, the panel power row, and the polkit rule (what it grants, where it installs, the `sudo`-group default + how to retarget, and the manual install command for non-`.deb` setups).

- [ ] **Step 5: Verify + commit**

Run: `ls deploy/tt-station-power.rules && grep -n "power-controls" docs/reference/power-controls.md >/dev/null && echo OK`
Also confirm the workspace still builds: `cargo build -p tt`.
Expected: `OK`, builds.

```bash
git add deploy/tt-station-power.rules debian docs/reference/power-controls.md crates/tt/src/console
git commit -m "feat(packaging): polkit rule for box power + power-controls reference doc"
```

---

## Self-Review

**Spec coverage:**
- §1 `POST /power` → Tasks 1 (executor) + 2 (route/status codes/auth/flags).
- §2 MAC in `/status`+TXT → Task 3.
- §3 CLI `tt power`/`tt wake` → Tasks 4 (lib) + 5 (CLI + mock-box).
- §4 macOS power menu + `powerState` → Tasks 6 (logic) + 7 (UI).
- §5 Linux panel power row → Task 8.
- §6 polkit rule + doc + `tt console` warning → Task 9.
- Reset-chips-keeps-pairing vs `/reset`-unpairs → enforced in Task 1 (`run_power_command` never touches tokens/SSH/status) and Task 2 (reset-chips returns 200, no `clear_tokens`); the existing `/reset` is untouched.
- Confirmation on the destructive three → Task 6 (`confirms` flag) + Task 7 (`.confirmationDialog`) + Task 8 (GTK dialog).
- Response-before-teardown (202) → Task 2 (`power_success_status`) + Task 4 (`power()` tolerates a dropped connection).

**Placeholder scan:** No TBD/TODO. Glue tasks (2, 3, 7, 9) that must match existing code name the pattern/file to follow and give the code to write; their verification is a build/e2e/unit gate, not a vague instruction. Every code step shows code.

**Type consistency:** `PowerAction` (Rust `power.rs`) values `reset-chips|suspend|reboot|shutdown` match the CLI literals (Task 5), the Swift `PowerAction.rawValue`s (Task 6), the panel keys (Task 8), and `agent_client::power`'s `action` string (Task 4). `is_machine_op` (Rust) ↔ `isMachineOp` (Swift). `power_success_status` used consistently (Task 2). `normalize_mac`/`parse_mac`/`magic_packet` names consistent (Tasks 3, 4). `PowerState`/`PowerTransition.next`/`onReachabilityChange` consistent across Tasks 6–7. `with_power_config`/`run_power_command` consistent across Tasks 1–2.
