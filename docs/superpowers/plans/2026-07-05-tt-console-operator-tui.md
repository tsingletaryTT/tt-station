# tt console — Operator TUI + Shared Lifecycle State Machine — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `tt console`, a ratatui operator TUI for loading/unloading/monitoring the box agent as a `systemctl --user` service over SSH, backed by a shared agent-lifecycle state machine that the GTK panel also consumes.

**Architecture:** A lifecycle core in the `tt` crate collects a `BoxLifecycleSnapshot` (service state from `systemctl`, pairing code from the journal, status/serving/config from the agent's HTTP) behind a fakeable `LifecycleEnv` trait, and exposes typed actions (start/stop/restart/reset/pair/install). `tt console` renders that snapshot as a TUI; `tt console --snapshot` prints it as JSON for the GTK panel, which migrates from child-supervision to the same systemd model. All tool/service names come from one `ToolNames` source.

**Tech Stack:** Rust (clap, ratatui, crossterm, tokio, reqwest, serde), `libttstation`, systemd user units, Python/GTK4 (panel).

## Global Constraints

- **Configurable tool names:** no tool/binary/service name hardcoded in more than one place. `ToolNames::from_env()` is the single source — `tt_bin` (`TTS_TT_BIN`, default `tt`), `agent_bin` (`TTS_AGENT_BIN`, default `tt-station-agentd`), `service_name` (`TTS_SERVICE_NAME`, default `tt-station-agentd.service`). Every `systemctl`/`journalctl -u`/unit-template/panel reference reads from it.
- **systemd user service model:** Start/Stop/Restart = `systemctl --user start|stop|restart <service_name>`. The agent survives SSH/reboot. Monitoring works even when the service is down.
- **Single source of truth:** both the TUI and the GTK panel render the same `BoxLifecycleSnapshot`; the panel gets it via `tt console --snapshot` (JSON). Actions live once in `LifecycleActions`.
- **Auth touchpoints centralized:** reset (bearer token) and `pair_localhost` are the only auth-bearing actions — kept behind `LifecycleActions` so the forthcoming SSH-key handshake is a contained swap.
- **Graceful degradation:** agent unreachable → snapshot HTTP fields `None`, `reachable=false`, UI still renders service state. `systemctl` unavailable → `ServiceState::Unknown`, actions error with a clear message, monitoring still works.
- **Reset requires a localhost bearer token** (`/reset` is `BearerAuth`); with none, the action returns a typed error and the UI offers `pair_localhost`.
- **Terminal UI rule:** left/bottom bars only, never right-side borders. Brand teal `#4fd1c5` on dark `#070d14`.
- TDD, DRY, YAGNI, frequent commits. `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo fmt --check` must pass.

## Shared Type Definitions (authoritative — used verbatim across tasks)

In `crates/libttstation/src/model.rs` (serde, so the JSON contract is one definition):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxLifecycleSnapshot {
    pub service: ServiceState,
    pub reachable: bool,
    pub name: Option<String>,
    pub chips: Option<String>,
    pub status: Option<ServingStatus>,
    pub endpoint: Option<Endpoint>,
    pub serving: Vec<ServingEntry>,
    pub config: Option<ConfigSummary>,
    pub pairing: Option<PairingState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState { Active, Inactive, Activating, Deactivating, Failed, Unknown }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingState { pub code: String, pub expires_in_secs: u64 }
```

In `crates/tt/src/console/state.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleState { Inactive, Starting, Idle, Serving(String), Stopping, Failed }
```

In `crates/tt/src/console/names.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolNames { pub tt_bin: String, pub agent_bin: String, pub service_name: String }
```

`LifecycleEnv` trait (in `crates/tt/src/console/env.rs`):

```rust
pub trait LifecycleEnv {
    /// `systemctl --user show <unit> -p ActiveState -p SubState --value` (or full show).
    fn systemctl_show(&self, unit: &str) -> anyhow::Result<String>;
    /// `journalctl --user -u <unit> -n <lines> --no-pager -o cat`.
    fn journal_tail(&self, unit: &str, lines: u32) -> anyhow::Result<Vec<String>>;
    /// GET `http://127.0.0.1:<ctrl_port><path>` → body string; Err on any failure.
    fn http_get(&self, path: &str) -> anyhow::Result<String>;
    /// Run an arbitrary systemctl/loginctl verb, returning Ok on success.
    fn run(&self, argv: &[&str]) -> anyhow::Result<()>;
    /// Unix seconds now (for pairing TTL). Injected for testable time.
    fn now_unix(&self) -> u64;
}
```

`PAIRING_TTL_SECS`: the agent's pairing-code TTL. **Grep the agent for the real constant** (`crates/tt-station-agentd/src/routes.rs`, the pending-pair TTL — the panel computes its countdown from the same value) and reuse that exact number; define `const PAIRING_TTL_SECS: u64 = <that value>;` in `state.rs`.

---

### Task 1: `ToolNames` — configurable tool/service names

**Files:**
- Create: `crates/tt/src/console/mod.rs` (module root: `pub mod names;` for now)
- Create: `crates/tt/src/console/names.rs`
- Modify: `crates/tt/src/main.rs` (add `mod console;`)

**Interfaces:**
- Produces: `ToolNames { tt_bin, agent_bin, service_name }`; `ToolNames::from_env() -> ToolNames`.

- [ ] **Step 1: Write failing tests**

In `names.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    // NB: env is process-global; set+remove within each test, don't run in parallel-hostile ways.
    #[test]
    fn defaults_when_unset() {
        std::env::remove_var("TTS_TT_BIN");
        std::env::remove_var("TTS_AGENT_BIN");
        std::env::remove_var("TTS_SERVICE_NAME");
        let n = ToolNames::from_env();
        assert_eq!(n.tt_bin, "tt");
        assert_eq!(n.agent_bin, "tt-station-agentd");
        assert_eq!(n.service_name, "tt-station-agentd.service");
    }
    #[test]
    fn env_overrides_win() {
        std::env::set_var("TTS_TT_BIN", "tt-cli");
        std::env::set_var("TTS_SERVICE_NAME", "quietbox-agent.service");
        let n = ToolNames::from_env();
        assert_eq!(n.tt_bin, "tt-cli");
        assert_eq!(n.service_name, "quietbox-agent.service");
        std::env::remove_var("TTS_TT_BIN");
        std::env::remove_var("TTS_SERVICE_NAME");
    }
}
```

- [ ] **Step 2: Run → FAIL**

Run: `cargo test -p tt --lib console::names`
Expected: FAIL (module/type missing).

- [ ] **Step 3: Implement**

`main.rs`: add `mod console;` beside the other top-level items. `console/mod.rs`: `pub mod names;`. `names.rs`:
```rust
//! Single source of truth for the project's CLI tool + service names, so a
//! future rename (`tt` → `tt-cli`, etc.) is a one-place change. Every
//! systemctl/journalctl/unit-template reference resolves names from here.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolNames {
    pub tt_bin: String,
    pub agent_bin: String,
    pub service_name: String,
}

impl ToolNames {
    pub fn from_env() -> Self {
        fn env_or(key: &str, default: &str) -> String {
            std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
        }
        ToolNames {
            tt_bin: env_or("TTS_TT_BIN", "tt"),
            agent_bin: env_or("TTS_AGENT_BIN", "tt-station-agentd"),
            service_name: env_or("TTS_SERVICE_NAME", "tt-station-agentd.service"),
        }
    }
}
```

- [ ] **Step 4: Run → PASS**

Run: `cargo test -p tt --lib console::names`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/console crates/tt/src/main.rs
git commit -m "feat(tt): console::names ToolNames (configurable tool/service names)"
```

---

### Task 2: Lifecycle snapshot types in `libttstation`

**Files:**
- Modify: `crates/libttstation/src/model.rs`

**Interfaces:**
- Produces: `BoxLifecycleSnapshot`, `ServiceState`, `PairingState` (see Shared Types).

- [ ] **Step 1: Write failing test**

In `model.rs` tests:
```rust
#[test]
fn lifecycle_snapshot_round_trips() {
    let s = BoxLifecycleSnapshot {
        service: ServiceState::Active,
        reachable: true,
        name: Some("qb2-lab".into()),
        chips: Some("4xBH".into()),
        status: None,
        endpoint: None,
        serving: vec![],
        config: None,
        pairing: Some(PairingState { code: "042817".into(), expires_in_secs: 107 }),
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: BoxLifecycleSnapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
    // ServiceState serializes snake_case
    assert!(serde_json::to_string(&ServiceState::Inactive).unwrap().contains("inactive"));
}
```

- [ ] **Step 2: Run → FAIL**

Run: `cargo test -p libttstation lifecycle_snapshot`
Expected: FAIL (types missing).

- [ ] **Step 3: Implement**

Add the three Shared-Types structs/enums to `model.rs` (near `ConfigSummary`/`ServingEntry`; reuse the file's existing `Serialize/Deserialize` import).

- [ ] **Step 4: Run → PASS**

Run: `cargo test -p libttstation lifecycle_snapshot`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/libttstation/src/model.rs
git commit -m "feat(lib): BoxLifecycleSnapshot / ServiceState / PairingState types"
```

---

### Task 3: Pure parsers + `derive_state`

**Files:**
- Create: `crates/tt/src/console/state.rs`
- Modify: `crates/tt/src/console/mod.rs` (`pub mod state;`)

**Interfaces:**
- Consumes: `ServiceState`, `PairingState`, `BoxLifecycleSnapshot` (Task 2).
- Produces: `PAIRING_TTL_SECS`; `parse_service_state(&str) -> ServiceState`; `parse_pairing(lines: &[String], now: u64) -> Option<PairingState>`; `LifecycleState`; `derive_state(&BoxLifecycleSnapshot) -> LifecycleState`.

- [ ] **Step 1: Confirm the agent's pairing TTL**

Run: `grep -rn "TTL\|ttl\|Duration::from_secs\|expires" crates/tt-station-agentd/src/routes.rs | head`
Use the pending-pair code TTL value found (the panel's countdown uses the same). Set `PAIRING_TTL_SECS` to it.

- [ ] **Step 2: Write failing tests**

In `state.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::{BoxLifecycleSnapshot, ServiceState, ServingStatus};

    #[test]
    fn service_state_from_systemctl_show() {
        assert_eq!(parse_service_state("ActiveState=active\nSubState=running\n"), ServiceState::Active);
        assert_eq!(parse_service_state("ActiveState=inactive\nSubState=dead\n"), ServiceState::Inactive);
        assert_eq!(parse_service_state("ActiveState=activating\nSubState=start\n"), ServiceState::Activating);
        assert_eq!(parse_service_state("ActiveState=failed\nSubState=failed\n"), ServiceState::Failed);
        assert_eq!(parse_service_state("garbage"), ServiceState::Unknown);
    }

    #[test]
    fn pairing_from_journal_recent_code() {
        let lines = vec![
            "agent started".to_string(),
            "pairing code issued: 042817".to_string(), // adjust to the real log wording
        ];
        // journal has no timestamps in `-o cat`; TTL is computed from "seen now".
        let p = parse_pairing(&lines, 1_000).unwrap();
        assert_eq!(p.code, "042817");
        assert_eq!(p.expires_in_secs, PAIRING_TTL_SECS); // fresh sighting → full TTL
    }

    #[test]
    fn pairing_none_when_no_code() {
        assert!(parse_pairing(&["agent started".to_string()], 1_000).is_none());
    }

    fn snap(service: ServiceState, reachable: bool, status: Option<ServingStatus>) -> BoxLifecycleSnapshot {
        BoxLifecycleSnapshot { service, reachable, name: None, chips: None, status,
            endpoint: None, serving: vec![], config: None, pairing: None }
    }

    #[test]
    fn derive_covers_states() {
        assert_eq!(derive_state(&snap(ServiceState::Inactive, false, None)), LifecycleState::Inactive);
        assert_eq!(derive_state(&snap(ServiceState::Activating, false, None)), LifecycleState::Starting);
        assert_eq!(derive_state(&snap(ServiceState::Active, false, None)), LifecycleState::Starting); // active but not yet reachable
        assert_eq!(derive_state(&snap(ServiceState::Active, true, Some(ServingStatus::Idle))), LifecycleState::Idle);
        assert_eq!(derive_state(&snap(ServiceState::Active, true, Some(ServingStatus::Serving("m".into())))), LifecycleState::Serving("m".into()));
        assert_eq!(derive_state(&snap(ServiceState::Deactivating, false, None)), LifecycleState::Stopping);
        assert_eq!(derive_state(&snap(ServiceState::Failed, false, None)), LifecycleState::Failed);
    }
}
```
> Adjust the journal log wording and `ServingStatus` variant names to the real ones — grep `crates/tt-station-agentd/src` for the code-issued log line and `crates/libttstation/src/model.rs` for `ServingStatus`'s variants (it has custom serde; use the Rust variant names in code).

- [ ] **Step 3: Run → FAIL**

Run: `cargo test -p tt --lib console::state`
Expected: FAIL.

- [ ] **Step 4: Implement**

```rust
use libttstation::model::{BoxLifecycleSnapshot, PairingState, ServiceState, ServingStatus};

pub const PAIRING_TTL_SECS: u64 = /* value from Step 1 */;

pub fn parse_service_state(show_output: &str) -> ServiceState {
    let mut active = "";
    for line in show_output.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") { active = v.trim(); }
    }
    match active {
        "active" => ServiceState::Active,
        "inactive" => ServiceState::Inactive,
        "activating" => ServiceState::Activating,
        "deactivating" => ServiceState::Deactivating,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Unknown,
    }
}

pub fn parse_pairing(lines: &[String], _now: u64) -> Option<PairingState> {
    // Most recent 6-digit code the agent logged. `-o cat` gives no timestamp,
    // so a freshly-tailed sighting is treated as full-TTL (the panel does the
    // same — it starts the countdown when it first sees the code).
    let re_digits = |s: &str| -> Option<String> {
        // find a standalone 6-digit run
        let bytes: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() { i += 1; }
                if i - start == 6 { return Some(bytes[start..i].iter().collect()); }
            } else { i += 1; }
        }
        None
    };
    for line in lines.iter().rev() {
        if line.to_lowercase().contains("pairing") || line.to_lowercase().contains("code") {
            if let Some(code) = re_digits(line) {
                return Some(PairingState { code, expires_in_secs: PAIRING_TTL_SECS });
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleState { Inactive, Starting, Idle, Serving(String), Stopping, Failed }

pub fn derive_state(s: &BoxLifecycleSnapshot) -> LifecycleState {
    match s.service {
        ServiceState::Failed => LifecycleState::Failed,
        ServiceState::Deactivating => LifecycleState::Stopping,
        ServiceState::Activating => LifecycleState::Starting,
        ServiceState::Inactive | ServiceState::Unknown => LifecycleState::Inactive,
        ServiceState::Active => {
            if !s.reachable { return LifecycleState::Starting; }
            match &s.status {
                Some(ServingStatus::Serving(m)) => LifecycleState::Serving(m.clone()),
                _ => LifecycleState::Idle,
            }
        }
    }
}
```
> Match `ServingStatus`'s real variant names (grep first). If `Idle`/`Serving(String)` differ, adapt.

- [ ] **Step 5: Run → PASS; clippy**

Run: `cargo test -p tt --lib console::state && cargo clippy -p tt --all-targets -- -D warnings`
Expected: PASS + clean.

- [ ] **Step 6: Commit**

```bash
git add crates/tt/src/console
git commit -m "feat(tt): lifecycle parsers + derive_state (pure)"
```

---

### Task 4: `LifecycleEnv` + `collect_snapshot`

**Files:**
- Create: `crates/tt/src/console/env.rs`
- Modify: `crates/tt/src/console/mod.rs` (`pub mod env;`)

**Interfaces:**
- Consumes: `ToolNames` (T1), parsers (T3), snapshot types (T2).
- Produces: `LifecycleEnv` trait (see Shared Types); `RealLifecycleEnv { names: ToolNames, ctrl_port: u16 }`; `collect_snapshot(&dyn LifecycleEnv, &ToolNames) -> BoxLifecycleSnapshot`.

- [ ] **Step 1: Write failing tests (fake env)**

In `env.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::ServiceState;

    struct FakeEnv { show: String, journal: Vec<String>, http: std::collections::HashMap<String, anyhow::Result<String>> }
    impl LifecycleEnv for FakeEnv {
        fn systemctl_show(&self, _u: &str) -> anyhow::Result<String> { Ok(self.show.clone()) }
        fn journal_tail(&self, _u: &str, _n: u32) -> anyhow::Result<Vec<String>> { Ok(self.journal.clone()) }
        fn http_get(&self, path: &str) -> anyhow::Result<String> {
            match self.http.get(path) { Some(Ok(s)) => Ok(s.clone()), Some(Err(_)) | None => Err(anyhow::anyhow!("down")) }
        }
        fn run(&self, _a: &[&str]) -> anyhow::Result<()> { Ok(()) }
        fn now_unix(&self) -> u64 { 1000 }
    }

    #[test]
    fn agent_down_degrades_but_keeps_service_state() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http: std::collections::HashMap::new(), // all GETs error
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert_eq!(snap.service, ServiceState::Active);
        assert!(!snap.reachable);
        assert!(snap.status.is_none() && snap.config.is_none() && snap.serving.is_empty());
    }

    #[test]
    fn healthy_agent_populates_status_and_reachable() {
        let mut http = std::collections::HashMap::new();
        http.insert("/status".to_string(), Ok(r#"{"name":"qb2-lab","chips":"4xBH","status":"idle"}"#.to_string()));
        http.insert("/serving".to_string(), Ok(r#"{"serving":[]}"#.to_string()));
        let env = FakeEnv { show: "ActiveState=active\nSubState=running\n".into(), journal: vec![], http };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert!(snap.reachable);
        assert_eq!(snap.name.as_deref(), Some("qb2-lab"));
    }
}
```
> Match the real `/status` JSON shape (grep `crates/libttstation/src/model.rs` for the status response type; it may be `StatusResponse` with `ServingStatus` custom serde — deserialize into the real type, not a hand-rolled one).

- [ ] **Step 2: Run → FAIL**

Run: `cargo test -p tt --lib console::env`
Expected: FAIL.

- [ ] **Step 3: Implement**

```rust
use anyhow::Result;
use libttstation::model::{BoxLifecycleSnapshot, ServingEntry};
use crate::console::names::ToolNames;
use crate::console::state::{parse_pairing, parse_service_state};

pub trait LifecycleEnv { /* exactly the Shared-Types trait */ }

pub struct RealLifecycleEnv { pub names: ToolNames, pub ctrl_port: u16 }
impl LifecycleEnv for RealLifecycleEnv {
    fn systemctl_show(&self, unit: &str) -> Result<String> {
        let out = std::process::Command::new("systemctl")
            .args(["--user", "show", unit, "-p", "ActiveState", "-p", "SubState"]).output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
    fn journal_tail(&self, unit: &str, lines: u32) -> Result<Vec<String>> {
        let out = std::process::Command::new("journalctl")
            .args(["--user", "-u", unit, "-n", &lines.to_string(), "--no-pager", "-o", "cat"]).output()?;
        Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.to_string()).collect())
    }
    fn http_get(&self, path: &str) -> Result<String> {
        let url = format!("http://127.0.0.1:{}{}", self.ctrl_port, path);
        Ok(reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2)).build()?
            .get(url).send()?.error_for_status()?.text()?)
    }
    fn run(&self, argv: &[&str]) -> Result<()> {
        let status = std::process::Command::new(argv[0]).args(&argv[1..]).status()?;
        if status.success() { Ok(()) } else { anyhow::bail!("command failed: {argv:?}") }
    }
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
    }
}

pub fn collect_snapshot(env: &dyn LifecycleEnv, names: &ToolNames) -> BoxLifecycleSnapshot {
    let service = env.systemctl_show(&names.service_name)
        .map(|o| parse_service_state(&o)).unwrap_or(libttstation::model::ServiceState::Unknown);
    let status_raw = env.http_get("/status").ok();
    let reachable = status_raw.is_some();
    // Deserialize each into its real libttstation type; any parse error → None.
    let status_resp = status_raw.as_deref().and_then(|s| serde_json::from_str::<libttstation::model::StatusResponse>(s).ok());
    let config = env.http_get("/config").ok().and_then(|s| serde_json::from_str::<libttstation::model::ConfigSummary>(&s).ok());
    let serving: Vec<ServingEntry> = env.http_get("/serving").ok()
        .and_then(|s| serde_json::from_str::<libttstation::model::ServingList>(&s).ok())
        .map(|l| l.serving).unwrap_or_default();
    let journal = env.journal_tail(&names.service_name, 40).unwrap_or_default();
    let pairing = parse_pairing(&journal, env.now_unix());
    BoxLifecycleSnapshot {
        service, reachable,
        name: status_resp.as_ref().map(|r| r.name.clone()),
        chips: status_resp.as_ref().map(|r| r.chips.clone()),
        status: status_resp.as_ref().map(|r| r.status.clone()),
        endpoint: None, // filled from /endpoint only when serving; optional — see note
        serving, config, pairing,
    }
}
```
> Use the real field names of `StatusResponse`/`ServingList` (grep first). `endpoint` may stay `None` in v1 (it needs an authed `/endpoint`); the serving model name already comes from `status`. If `StatusResponse` exposes the endpoint unauthed, populate it.

- [ ] **Step 4: Run → PASS; clippy**

Run: `cargo test -p tt --lib console::env && cargo clippy -p tt --all-targets -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/console
git commit -m "feat(tt): LifecycleEnv + collect_snapshot (fakeable, degrades on agent-down)"
```

---

### Task 5: `LifecycleActions` + systemd unit template

**Files:**
- Create: `crates/tt/src/console/actions.rs`
- Create: `deploy/tt-station-agentd.service`
- Modify: `crates/tt/src/console/mod.rs` (`pub mod actions;`)

**Interfaces:**
- Consumes: `LifecycleEnv` (T4), `ToolNames` (T1), `BoxLifecycleSnapshot`/`PairingState` (T2), `libttstation` reset/pairing client + `SecretStore`.
- Produces: `LifecycleActions<'a>` with `start/stop/restart(&self)`, `set_profile(&self, &str)`, `install_service(&self, agent_bin_path: &str)`, and the argv/drop-in helpers below. Reset + pair_localhost may reuse existing `tt` command fns (`cmd_reset`, `cmd_pair`) — call those rather than duplicating.

- [ ] **Step 1: Write failing tests (fake env asserts argv + drop-in content)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct RecEnv { calls: RefCell<Vec<Vec<String>>> }
    impl LifecycleEnv for RecEnv {
        fn systemctl_show(&self,_:&str)->anyhow::Result<String>{Ok(String::new())}
        fn journal_tail(&self,_:&str,_:u32)->anyhow::Result<Vec<String>>{Ok(vec![])}
        fn http_get(&self,_:&str)->anyhow::Result<String>{anyhow::bail!("n/a")}
        fn run(&self, argv:&[&str])->anyhow::Result<()>{ self.calls.borrow_mut().push(argv.iter().map(|s|s.to_string()).collect()); Ok(()) }
        fn now_unix(&self)->u64{0}
    }
    fn names() -> ToolNames { ToolNames { tt_bin:"tt".into(), agent_bin:"tt-station-agentd".into(), service_name:"svc.service".into() } }

    #[test]
    fn start_uses_systemctl_user() {
        let env = RecEnv{calls:RefCell::new(vec![])};
        LifecycleActions::new(&env, &names()).start().unwrap();
        assert_eq!(env.calls.borrow()[0], vec!["systemctl","--user","start","svc.service"]);
    }

    #[test]
    fn drop_in_content_pins_profile() {
        let content = render_profile_dropin("tt-station-agentd", "bleeding");
        assert!(content.contains("[Service]"));
        assert!(content.contains("ExecStart=\n")); // reset then re-set
        assert!(content.contains("--profile bleeding"));
    }

    #[test]
    fn unit_template_fills_agent_bin() {
        let unit = render_unit("/home/x/.local/bin/tt-station-agentd");
        assert!(unit.contains("ExecStart=/home/x/.local/bin/tt-station-agentd"));
        assert!(!unit.contains("{{AGENT_BIN}}"));
    }
}
```

- [ ] **Step 2: Run → FAIL**

Run: `cargo test -p tt --lib console::actions`
Expected: FAIL.

- [ ] **Step 3: Create the unit template + implement actions**

`deploy/tt-station-agentd.service`:
```ini
[Unit]
Description=tt-station box agent (QuietBox control plane)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={{AGENT_BIN}}
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
```

`actions.rs`:
```rust
use crate::console::env::LifecycleEnv;
use crate::console::names::ToolNames;

pub struct LifecycleActions<'a> { env: &'a dyn LifecycleEnv, names: &'a ToolNames }

const UNIT_TEMPLATE: &str = include_str!("../../../../deploy/tt-station-agentd.service");

pub fn render_unit(agent_bin_path: &str) -> String {
    UNIT_TEMPLATE.replace("{{AGENT_BIN}}", agent_bin_path)
}

pub fn render_profile_dropin(agent_bin: &str, profile: &str) -> String {
    // Clearing ExecStart= then re-setting it is the systemd idiom for overriding it in a drop-in.
    format!("[Service]\nExecStart=\nExecStart={agent_bin} --profile {profile}\n")
}

impl<'a> LifecycleActions<'a> {
    pub fn new(env: &'a dyn LifecycleEnv, names: &'a ToolNames) -> Self { Self { env, names } }
    pub fn start(&self) -> anyhow::Result<()> { self.env.run(&["systemctl","--user","start",&self.names.service_name]) }
    pub fn stop(&self) -> anyhow::Result<()> { self.env.run(&["systemctl","--user","stop",&self.names.service_name]) }
    pub fn restart(&self) -> anyhow::Result<()> { self.env.run(&["systemctl","--user","restart",&self.names.service_name]) }
    pub fn set_profile(&self, profile: &str) -> anyhow::Result<()> {
        // write ~/.config/systemd/user/<unit>.d/profile.conf, daemon-reload, restart
        let dir = dirs_config_systemd_user().join(format!("{}.d", self.names.service_name));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("profile.conf"), render_profile_dropin(&self.names.agent_bin, profile))?;
        self.env.run(&["systemctl","--user","daemon-reload"])?;
        self.restart()
    }
    pub fn install_service(&self, agent_bin_path: &str) -> anyhow::Result<()> {
        let dir = dirs_config_systemd_user();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&self.names.service_name);
        let content = render_unit(agent_bin_path);
        let write = !path.exists() || std::fs::read_to_string(&path).map(|c| c != content).unwrap_or(true);
        if write { std::fs::write(&path, content)?; }
        self.env.run(&["systemctl","--user","daemon-reload"])
    }
}

fn dirs_config_systemd_user() -> std::path::PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"));
    base.join("systemd").join("user")
}
```
Reset + pair-localhost: reuse the existing `tt` async command fns. Add thin wrappers in `console/mod.rs` (Task 6) that call `cmd_reset(Some("127.0.0.1:<port>"))` and the pairing flow with the code from the snapshot, rather than duplicating HTTP.

- [ ] **Step 4: Run → PASS; clippy; fmt**

Run: `cargo test -p tt --lib console::actions && cargo clippy -p tt --all-targets -- -D warnings && cargo fmt -p tt`

- [ ] **Step 5: Commit**

```bash
git add crates/tt/src/console deploy/tt-station-agentd.service
git commit -m "feat(tt): LifecycleActions (systemctl/drop-in/install) + systemd unit template"
```

---

### Task 6: `tt console` command wiring + `--snapshot` JSON + `--install-service`

**Files:**
- Modify: `crates/tt/src/main.rs` (add `Command::Console`, dispatch)
- Modify: `crates/tt/src/console/mod.rs`

**Interfaces:**
- Consumes: everything from T1–T5.
- Produces: `Command::Console { snapshot: bool, install_service: bool }`; `console::run_console(names, ctrl_port, snapshot, install_service, json) -> Result<()>`.

- [ ] **Step 1: Add the subcommand + dispatch**

In `main.rs`'s `Command` enum:
```rust
/// Operator TUI for managing this box's agent as a systemd --user service.
/// (Run ON the box, e.g. over SSH.)
Console {
    /// Print one BoxLifecycleSnapshot as JSON and exit (for the GTK panel).
    #[arg(long)]
    snapshot: bool,
    /// Install the systemd --user unit and exit.
    #[arg(long = "install-service")]
    install_service: bool,
    /// Agent control port to talk to (defaults to 8765).
    #[arg(long = "ctrl-port", default_value_t = 8765)]
    ctrl_port: u16,
},
```
Dispatch → `console::run_console(...)`.

- [ ] **Step 2: Implement `run_console`**

In `console/mod.rs`:
```rust
pub fn run_console(ctrl_port: u16, snapshot: bool, install_service: bool, _json: bool) -> anyhow::Result<()> {
    let names = names::ToolNames::from_env();
    let env = env::RealLifecycleEnv { names: names.clone(), ctrl_port };
    if install_service {
        let agent_path = which_agent(&names.agent_bin);
        actions::LifecycleActions::new(&env, &names).install_service(&agent_path)?;
        println!("installed {} (systemctl --user)", names.service_name);
        return Ok(());
    }
    if snapshot {
        let snap = env::collect_snapshot(&env, &names);
        println!("{}", serde_json::to_string_pretty(&snap)?);
        return Ok(());
    }
    ui::run_tui(&env, &names, ctrl_port) // Task 7
}

fn which_agent(agent_bin: &str) -> String {
    // absolute path if resolvable on PATH, else the bare name (systemd needs abs;
    // fall back to $HOME/.local/bin/<agent_bin> then the name).
    if let Ok(p) = which::which(agent_bin) { return p.to_string_lossy().into_owned(); }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = format!("{home}/.local/bin/{agent_bin}");
    if std::path::Path::new(&candidate).exists() { candidate } else { agent_bin.to_string() }
}
```
Add `which = "4"` to `crates/tt/Cargo.toml` (and workspace deps) for PATH resolution, OR implement a tiny PATH scan to avoid the dep — implementer's choice; prefer no new dep if trivial.

- [ ] **Step 3: Test `--snapshot` shape**

Add an e2e-ish/unit test that `tt console --snapshot` (with the agent down) prints valid JSON deserializing to `BoxLifecycleSnapshot` with `reachable=false`. If a live agent/systemd isn't available in CI, gate with `#[ignore]` like the other e2e tests and assert via `assert_cmd`.

- [ ] **Step 4: Build + smoke**

```bash
cargo build -p tt
./target/debug/tt console --snapshot --ctrl-port 8765   # prints JSON snapshot
```
Expected: JSON with a `service` field and `reachable` bool.

- [ ] **Step 5: Commit**

```bash
git add crates/tt
git commit -m "feat(tt): tt console command — --snapshot JSON + --install-service"
```

---

### Task 7: `tt console` ratatui TUI

**Files:**
- Create: `crates/tt/src/console/ui.rs`
- Modify: `crates/tt/src/console/mod.rs` (`pub mod ui;`), `crates/tt/Cargo.toml` (add `ratatui`, `crossterm`)

**Interfaces:**
- Consumes: `collect_snapshot` (T4), `LifecycleActions` (T5), `derive_state` (T3), snapshot types (T2).
- Produces: `run_tui(&dyn LifecycleEnv, &ToolNames, ctrl_port) -> Result<()>`; pure `header_lines(&BoxLifecycleSnapshot) -> Vec<String>`, `pairing_lines(&BoxLifecycleSnapshot) -> Vec<String>`, `status_lines(&BoxLifecycleSnapshot) -> Vec<String>` builders.

- [ ] **Step 1: Add deps**

`crates/tt/Cargo.toml`: `ratatui = "0.28"`, `crossterm = "0.28"` (and to workspace `[workspace.dependencies]`; match tt-toplike's versions if pinned there — check `~/code/tt-toplike/Cargo.toml`).

- [ ] **Step 2: Write failing tests (pure builders + TestBackend render)**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn idle_snap() -> BoxLifecycleSnapshot {
        BoxLifecycleSnapshot { service: ServiceState::Active, reachable: true,
            name: Some("qb2-lab".into()), chips: Some("4xBH".into()),
            status: Some(ServingStatus::Idle), endpoint: None, serving: vec![],
            config: None, pairing: None }
    }

    #[test]
    fn header_shows_name_and_service() {
        let lines = header_lines(&idle_snap());
        assert!(lines.iter().any(|l| l.contains("qb2-lab")));
        assert!(lines.iter().any(|l| l.to_lowercase().contains("active")));
    }

    #[test]
    fn pairing_lines_show_code_when_present() {
        let mut s = idle_snap();
        s.pairing = Some(PairingState { code: "042817".into(), expires_in_secs: 100 });
        let lines = pairing_lines(&s);
        assert!(lines.iter().any(|l| l.contains("042817")));
    }

    #[test]
    fn renders_without_panicking() {
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &idle_snap())).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("qb2-lab"));
    }
}
```

- [ ] **Step 3: Implement the pure builders + `draw` + `run_tui`**

Pure builders return the text lines for each panel (from the snapshot + `derive_state`), e.g.:
```rust
pub fn header_lines(s: &BoxLifecycleSnapshot) -> Vec<String> {
    let name = s.name.clone().unwrap_or_else(|| "tt-station".into());
    let svc = match s.service { /* ServiceState → "active"/"inactive"/… */ };
    let mesh = s.status.as_ref().and_then(/* device_mesh */).unwrap_or_default();
    let profile = s.config.as_ref().and_then(|c| c.active_profile.clone()).unwrap_or_else(|| "—".into());
    vec![
        format!("tt-station · {name}          ● service: {svc}"),
        format!("{mesh} · profile: {profile}"),
    ]
}
pub fn pairing_lines(s: &BoxLifecycleSnapshot) -> Vec<String> { /* code spaced out + TTL, or "no active pairing" */ }
pub fn status_lines(s: &BoxLifecycleSnapshot) -> Vec<String> { /* serving/endpoint + /serving summary */ }
```
`draw(frame, snapshot)` lays out vertical chunks (header / pairing card / status / journal / footer keybindings) using `ratatui::layout::Layout` and `Paragraph`/`Block` widgets with `Borders::LEFT | Borders::BOTTOM` only (per the terminal-UI rule), brand teal styling.

`run_tui`: crossterm raw mode + alternate screen; loop: `collect_snapshot` on a ~1s tick (and on demand after an action), `term.draw(|f| draw(f, &snap))`, poll crossterm events with a short timeout; on key:
- `s`/`x`/`r` → `LifecycleActions::{start,stop,restart}` (then re-collect)
- `R` → confirm modal → `cmd_reset` for localhost (offer pair on token error)
- `p` → pair-localhost using `snap.pairing.code`
- `c` → cycle/select profile → `set_profile` (only if `config.available_profiles` non-empty)
- `i` → `install_service`
- `q`/Esc → restore terminal + exit
Always restore the terminal (raw mode off, leave alt screen) on exit AND on error (guard with a drop/util so a panic doesn't wedge the terminal).

- [ ] **Step 4: Run → PASS; clippy; fmt**

Run: `cargo test -p tt --lib console::ui && cargo clippy -p tt --all-targets -- -D warnings && cargo fmt -p tt`

- [ ] **Step 5: Manual smoke (owner-run over SSH later) + commit**

```bash
git add crates/tt
git commit -m "feat(tt): tt console ratatui TUI (monitor + lifecycle keybindings)"
```

---

### Task 8: GTK panel migration to the shared model

**Files:**
- Modify: `box-panel/tt-station-panel.py`, `box-panel/README.md`

**Interfaces:**
- Consumes: `tt console --snapshot` (JSON `BoxLifecycleSnapshot`, T6); `systemctl --user`; the drop-in profile switch.

- [ ] **Step 1: Replace child-supervision with systemctl**

Replace `start_agent`/`stop_agent`/`restart_agent` (Popen/SIGINT) with:
```python
SERVICE = os.environ.get("TTS_SERVICE_NAME", "tt-station-agentd.service")
def _systemctl(verb): subprocess.run(["systemctl", "--user", verb, SERVICE], check=False)
```
`start`→`_systemctl("start")`, etc. Remove the child `self.proc` bookkeeping and the `close-request→stop_agent` handler (closing the panel must NOT stop the service now). `TTS_AUTOSTART=1` → `_systemctl("start")`.

- [ ] **Step 2: Consume `tt console --snapshot` for all state**

Replace the child-stdout code parsing and per-endpoint polls with a single poll of:
```python
out = subprocess.run([TT_BIN, "console", "--snapshot", "--ctrl-port", CTRL_PORT],
                     capture_output=True, text=True)
snap = json.loads(out.stdout) if out.returncode == 0 else None
```
Render service state, pairing code+TTL (`snap["pairing"]`), status/endpoint/serving, and active profile from `snap`. This is the single source of truth shared with the TUI.

- [ ] **Step 3: Profile switch via drop-in**

The dropdown's "apply" now calls (either) `tt` helper or writes the drop-in + `systemctl --user restart`. Simplest: shell `systemctl --user` after writing `~/.config/systemd/user/<SERVICE>.d/profile.conf` (same content as `render_profile_dropin`), OR call a `tt console --set-profile <name>` if that flag is added. For v1, writing the drop-in from Python + `daemon-reload` + `restart` is acceptable and mirrors `render_profile_dropin` exactly (keep the format identical).

- [ ] **Step 4: Verify (panel is GUI — not unit-tested)**

- `python3 -m py_compile box-panel/tt-station-panel.py` → OK.
- A throwaway check (in /tmp, not committed) that the snapshot-parsing helper handles a sample JSON and a `None` (agent down) without raising.
- Note live click-through is owner-run.

- [ ] **Step 5: Update README + commit**

Document the systemd model + `TTS_SERVICE_NAME` in `box-panel/README.md`. Then:
```bash
git add box-panel/tt-station-panel.py box-panel/README.md
git commit -m "feat(panel): migrate to systemd model + shared tt console --snapshot state"
```

---

### Task 9: Docs — console reference + project docs

**Files:**
- Create: `docs/reference/tt-console.md`
- Modify: `CLAUDE.md`, `docs/reference/` index or README as appropriate

- [ ] **Step 1: Write `docs/reference/tt-console.md`**

Cover: what `tt console` is (SSH operator TUI), the systemd user-service model (`install-service`, `enable-linger` for boot survival), keybindings, `--snapshot` JSON contract (the `BoxLifecycleSnapshot` shape), configurable tool names (`TTS_TT_BIN`/`TTS_AGENT_BIN`/`TTS_SERVICE_NAME`), and the reset-needs-localhost-token precondition + `pair-localhost`.

- [ ] **Step 2: Update CLAUDE.md**

Add `tt console` to the CLI section and note the panel now shares its state machine + runs the agent under `systemctl --user`. Note the `deploy/tt-station-agentd.service` unit.

- [ ] **Step 3: Commit**

```bash
git add docs/reference/tt-console.md CLAUDE.md
git commit -m "docs: tt console operator TUI reference + project-doc updates"
```

---

## Recommended execution order

1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 (dependency order; each builds on the prior). Pull `origin/main` between tasks (the macOS agent pushes often; watch for the SSH-key handshake).

## Self-review notes

- **Spec coverage:** ToolNames/configurable names (T1), snapshot types (T2), parsers+derive (T3), collector+degradation (T4), actions+unit (T5), command+snapshot-JSON+install (T6), TUI (T7), panel migration (T8), docs (T9). All spec sections map to a task.
- **Grep-first placeholders:** three spots require confirming real values before coding — `PAIRING_TTL_SECS` (agent routes), the code-issued journal log wording (agent), and `ServingStatus`/`StatusResponse`/`ServingList` field+variant names (libttstation). Each step says to grep first; these are lookups, not guesses.
- **Type consistency:** `BoxLifecycleSnapshot`/`ServiceState`/`PairingState`/`LifecycleState`/`ToolNames`/`LifecycleEnv` are defined once in Shared Types and referenced identically in T3–T8.
- **Auth centralization:** reset + pair-localhost reuse existing `tt` fns via `LifecycleActions`/`console/mod.rs`, not duplicated — the single touchpoint the SSH-key handshake will later swap.
