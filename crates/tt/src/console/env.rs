//! `LifecycleEnv`: the fakeable seam between `tt console`'s pure lifecycle
//! logic (`console::state`) and the outside world (systemctl, journalctl,
//! the agent's HTTP control API, and spawning helper commands).
//!
//! Every I/O `tt console` needs goes through this trait so tests can swap in
//! a `FakeEnv` instead of touching a real systemd instance or an agent
//! process. [`collect_snapshot`] is the one function that walks the trait and
//! assembles a [`BoxLifecycleSnapshot`] -- see its doc comment for the
//! degrade-on-agent-down contract that makes "the agent isn't running" a
//! normal, representable state rather than an error.
//!
//! `run_console` (`console::mod`) constructs the real [`RealLifecycleEnv`]
//! and every method here is reachable from `main()` through it, so this
//! module carries no blanket `#![allow(dead_code)]` -- see the M5 cleanup
//! note in the final-review report for why one used to be here.

use crate::console::names::ToolNames;
use crate::console::state::{parse_pairing, parse_service_state};
use anyhow::Result;
use libttstation::model::{
    BoxLifecycleSnapshot, ConfigSummary, LogsInfo, ServingList, ServingStatus,
};
use serde::Deserialize;

/// The fakeable seam for every side effect `tt console` needs: reading
/// systemd unit state, tailing the agent's journal, hitting its HTTP control
/// API, and (for actions, a later task) spawning a command. Kept as five
/// small verbs rather than one "run a shell command" primitive so a
/// `FakeEnv` can answer each concern independently in tests -- e.g. "the
/// service is active but the HTTP API is down" (agent wedged) is exactly as
/// easy to construct as "everything is fine."
pub trait LifecycleEnv {
    /// `systemctl --user show <unit> -p ActiveState -p SubState`'s raw
    /// stdout, for [`parse_service_state`] to parse. Errors (e.g. `systemctl`
    /// missing, or the unit doesn't exist) are the caller's to degrade --
    /// see [`collect_snapshot`].
    fn systemctl_show(&self, unit: &str) -> Result<String>;

    /// The last `lines` lines of `journalctl --user -u <unit> -o cat` for
    /// `unit`, oldest first -- fed to [`parse_pairing`] to find the most
    /// recent pairing code the agent logged.
    fn journal_tail(&self, unit: &str, lines: u32) -> Result<Vec<String>>;

    /// `GET <path>` against the agent's control API, returning the raw
    /// response body. `Err` covers both "couldn't connect" (agent down) and
    /// "connected but got a non-2xx" -- [`collect_snapshot`] treats both the
    /// same way (that field degrades to `None`/empty), so this trait doesn't
    /// need to distinguish them.
    fn http_get(&self, path: &str) -> Result<String>;

    /// Spawn `argv[0]` with `argv[1..]` and wait for it to exit, `Err` on a
    /// non-zero exit code. Used by `tt console`'s operator actions (start/
    /// stop/restart the service, etc. -- a later task); [`collect_snapshot`]
    /// itself never calls this.
    fn run(&self, argv: &[&str]) -> Result<()>;

    /// Current wall-clock time as a Unix timestamp. Threaded through to
    /// [`parse_pairing`] for future use -- see that function's doc comment
    /// on why today's `-o cat` journal tail gives it nothing to compute an
    /// age from yet.
    fn now_unix(&self) -> u64;

    /// Whether the polkit rule that lets `POST /power`'s machine ops (and the
    /// box panel's local power row) run without an interactive auth prompt is
    /// installed -- see [`crate::console::state::POLKIT_POWER_RULE_PATH`] and
    /// `docs/reference/power-controls.md`. Given a default body (a real
    /// `Path::exists()` check) rather than requiring every implementer to
    /// answer it: [`RealLifecycleEnv`] gets the real answer for free, and a
    /// fake `LifecycleEnv` in tests can override it to force either branch
    /// without touching the filesystem.
    fn polkit_power_rule_present(&self) -> bool {
        std::path::Path::new(crate::console::state::POLKIT_POWER_RULE_PATH).exists()
    }
}

/// The real [`LifecycleEnv`]: shells out to `systemctl`/`journalctl`, and
/// speaks HTTP to the agent's own control API on `127.0.0.1:<ctrl_port>`
/// (the agent binds control there -- see `tt-station-agentd`'s server setup;
/// `tt console` runs on the box itself, so `127.0.0.1` is always correct,
/// unlike the CLI's `discover`/`pair` flows which target a remote host).
///
/// No `names: ToolNames` field here (there used to be one) -- every method
/// below takes the unit name/argv it needs explicitly per call, and the
/// callers that DO need a [`ToolNames`] (`console::mod`'s `run_console`,
/// `collect_snapshot`, `LifecycleActions`, the TUI's key handlers) already
/// hold their own, resolved once via `ToolNames::from_env()`. A `names`
/// field here was dead weight: nothing in this file ever read `self.names`.
#[derive(Debug, Clone)]
pub struct RealLifecycleEnv {
    pub ctrl_port: u16,
}

impl LifecycleEnv for RealLifecycleEnv {
    fn systemctl_show(&self, unit: &str) -> Result<String> {
        let out = std::process::Command::new("systemctl")
            .args([
                "--user",
                "show",
                unit,
                "-p",
                "ActiveState",
                "-p",
                "SubState",
            ])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn journal_tail(&self, unit: &str, lines: u32) -> Result<Vec<String>> {
        let out = std::process::Command::new("journalctl")
            .args([
                "--user",
                "-u",
                unit,
                "-n",
                &lines.to_string(),
                "--no-pager",
                "-o",
                "cat",
            ])
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect())
    }

    fn http_get(&self, path: &str) -> Result<String> {
        let url = format!("http://127.0.0.1:{}{}", self.ctrl_port, path);
        Ok(reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?
            .get(url)
            .send()?
            .error_for_status()?
            .text()?)
    }

    fn run(&self, argv: &[&str]) -> Result<()> {
        let status = std::process::Command::new(argv[0])
            .args(&argv[1..])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("command failed: {argv:?}")
        }
    }

    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// The agent's real `GET /status` wire shape (`tt-station-agentd::routes::
/// StatusResponse` / `get_status`), mirrored here rather than imported: it's
/// a `pub` route-handler-local struct on the server side, and the sibling
/// client helper `libttstation::agent_client::get_status` deserializes into
/// its OWN function-local (non-`pub`) struct of the same shape before
/// throwing `name`/`chips` away and returning just a [`libttstation::model::
/// StatusInfo`] (`status` + `device_mesh`) -- neither is reusable from here.
/// `tt console` needs `name`/`chips` too (they're `BoxLifecycleSnapshot`
/// fields), so this struct exists purely to decode the same wire bytes;
/// see the task-4 report for why a shared `pub` type isn't threaded through
/// instead (a good follow-up for whoever touches this next).
#[derive(Debug, Deserialize)]
struct StatusWire {
    name: String,
    chips: String,
    /// TXT string form (`idle` / `serving:<model>`) -- parsed with
    /// [`ServingStatus::from_txt`] below.
    status: String,
    // `device_mesh` isn't part of `BoxLifecycleSnapshot` (Task 2 didn't add
    // a field for it there), so it's intentionally not decoded here.
}

/// Assemble a [`BoxLifecycleSnapshot`] by walking `env`. This is the one
/// place that decides how "the agent is down" is represented: it is a
/// NORMAL state, not an error, so this function has no `Result` return at
/// all. `service` still reflects whatever `systemctl` reports (the service
/// unit and the HTTP API are independent signals -- e.g. `active` but wedged
/// answers no HTTP), `reachable` is exactly "did `GET /status` succeed," and
/// every other HTTP-sourced field (`status`/`chips`/`name` from `/status`,
/// `config` from `/config`, `serving` from `/serving`) independently
/// degrades to `None`/empty on its own connection error OR a body that
/// doesn't parse as the expected shape -- one field's failure never blocks
/// the others. Each `.ok()`/`.unwrap_or_default()` below is deliberate:
/// resist the urge to `?`-propagate any of them.
pub fn collect_snapshot(env: &dyn LifecycleEnv, names: &ToolNames) -> BoxLifecycleSnapshot {
    // systemd state: `Unknown` (not an error) if `systemctl` itself fails
    // (missing binary, unit doesn't exist, etc.) -- `parse_service_state`
    // already maps unrecognized/garbage text to `Unknown`, so an `Err` here
    // just gets the same treatment via `unwrap_or_default`-style handling.
    let service = env
        .systemctl_show(&names.service_name)
        .map(|out| parse_service_state(&out))
        .unwrap_or(libttstation::model::ServiceState::Unknown);

    // `reachable` is defined as exactly "did `GET /status` succeed" --
    // independent of whether the body then parses into `StatusWire`.
    let status_body = env.http_get("/status").ok();
    let reachable = status_body.is_some();

    let status_wire: Option<StatusWire> = status_body
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok());
    let name = status_wire.as_ref().map(|w| w.name.clone());
    let chips = status_wire.as_ref().map(|w| w.chips.clone());
    // A status string the agent sent that doesn't round-trip through
    // `ServingStatus::from_txt` (shouldn't happen against a real agent, but
    // this is untrusted network input) degrades to `None` rather than
    // failing the whole snapshot.
    let status = status_wire
        .as_ref()
        .and_then(|w| ServingStatus::from_txt(&w.status).ok());

    // `/config` and `/serving` are independent GETs -- either can fail (or
    // parse-fail) on its own without affecting the other or `/status`.
    let config: Option<ConfigSummary> = env
        .http_get("/config")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let serving = env
        .http_get("/serving")
        .ok()
        .and_then(|s| serde_json::from_str::<ServingList>(&s).ok())
        .map(|l| l.serving)
        .unwrap_or_default();

    // Pairing comes from the journal, not HTTP -- an unreachable agent can
    // still have a readable journal (e.g. it crashed after logging a code).
    let journal = env
        .journal_tail(&names.service_name, 40)
        .unwrap_or_default();
    let pairing = parse_pairing(&journal, env.now_unix());

    // `/logs` (Task 2, dogfooded here rather than reading files directly):
    // the last 20 lines of the container's serving log for the log pane
    // (`ui::log_lines`). Same independent-degrade contract as `/config`/
    // `/serving` above -- a non-2xx (e.g. 409 "no repo configured" on a
    // non-runpy backend), a connection failure, or a body that doesn't parse
    // as `LogsInfo` all fall through to `vec![]` via `unwrap_or_default`,
    // never failing the rest of the snapshot.
    let logs = env
        .http_get("/logs?source=container&tail=20")
        .ok()
        .and_then(|body| serde_json::from_str::<LogsInfo>(&body).ok())
        .map(|l| l.lines)
        .unwrap_or_default();

    // Polkit rule presence (Task 9): a plain existence check, not an HTTP/
    // systemctl/journal probe, so it degrades the same informational way as
    // everything else here -- `polkit_power_advisory` just turns the bool
    // into `None`/`Some(message)`.
    let polkit_power_advisory =
        crate::console::state::polkit_power_advisory(env.polkit_power_rule_present());

    BoxLifecycleSnapshot {
        service,
        reachable,
        name,
        chips,
        status,
        // `/endpoint` is authed and this collector runs unauthenticated
        // probes only (no token in scope here) -- v1 leaves this `None`.
        // See the task-4 report.
        endpoint: None,
        serving,
        config,
        pairing,
        logs,
        polkit_power_advisory,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::ServiceState;

    struct FakeEnv {
        show: String,
        journal: Vec<String>,
        http: std::collections::HashMap<String, anyhow::Result<String>>,
        /// Overrides [`LifecycleEnv::polkit_power_rule_present`]'s default
        /// (real) filesystem check, so tests can force either branch without
        /// touching `/etc/polkit-1/rules.d/`. `true` in every existing
        /// literal below -- those tests predate Task 9 and don't care about
        /// this field, so `true` (rule present, no advisory noise) keeps
        /// their assertions unaffected.
        polkit_rule_present: bool,
    }
    impl LifecycleEnv for FakeEnv {
        fn systemctl_show(&self, _u: &str) -> anyhow::Result<String> {
            Ok(self.show.clone())
        }
        fn journal_tail(&self, _u: &str, _n: u32) -> anyhow::Result<Vec<String>> {
            Ok(self.journal.clone())
        }
        fn http_get(&self, path: &str) -> anyhow::Result<String> {
            match self.http.get(path) {
                Some(Ok(s)) => Ok(s.clone()),
                Some(Err(_)) | None => Err(anyhow::anyhow!("down")),
            }
        }
        fn run(&self, _a: &[&str]) -> anyhow::Result<()> {
            Ok(())
        }
        fn now_unix(&self) -> u64 {
            1000
        }
        fn polkit_power_rule_present(&self) -> bool {
            self.polkit_rule_present
        }
    }

    #[test]
    fn agent_down_degrades_but_keeps_service_state() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http: std::collections::HashMap::new(), // all GETs error
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert_eq!(snap.service, ServiceState::Active);
        assert!(!snap.reachable);
        assert!(snap.status.is_none() && snap.config.is_none() && snap.serving.is_empty());
    }

    #[test]
    fn healthy_agent_populates_status_and_reachable() {
        let mut http = std::collections::HashMap::new();
        http.insert(
            "/status".to_string(),
            Ok(r#"{"name":"qb2-lab","chips":"4xBH","status":"idle"}"#.to_string()),
        );
        http.insert("/serving".to_string(), Ok(r#"{"serving":[]}"#.to_string()));
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http,
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert!(snap.reachable);
        assert_eq!(snap.name.as_deref(), Some("qb2-lab"));
    }

    #[test]
    fn pairing_code_surfaces_from_journal_even_when_agent_down() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec!["tt-station-agentd: pairing code: 042817".to_string()],
            http: std::collections::HashMap::new(),
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert!(!snap.reachable);
        assert_eq!(snap.pairing.map(|p| p.code), Some("042817".to_string()));
    }

    /// A canned `/logs?source=container&tail=20` body must populate
    /// `snap.logs` -- the pane dogfoods Task 2's `/logs` route via
    /// `LifecycleEnv::http_get`, exactly like `/config`/`/serving` above.
    #[test]
    fn logs_populate_from_agent_logs_route() {
        let mut http = std::collections::HashMap::new();
        http.insert(
            "/logs?source=container&tail=20".to_string(),
            Ok(r#"{"source":"container","origin":"/var/log/vllm.log","lines":["line one","line two"]}"#.to_string()),
        );
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http,
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert_eq!(
            snap.logs,
            vec!["line one".to_string(), "line two".to_string()]
        );
    }

    /// The agent being unreachable (or `/logs` erroring/parse-failing) must
    /// degrade `logs` to empty, never fail the whole snapshot -- same
    /// contract as every other HTTP-sourced field here.
    #[test]
    fn logs_degrade_to_empty_when_agent_down() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http: std::collections::HashMap::new(),
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert!(snap.logs.is_empty());
    }

    /// The polkit-rule check flows straight into `snap.polkit_power_advisory`
    /// via `state::polkit_power_advisory` -- present means no advisory.
    #[test]
    fn polkit_advisory_absent_when_rule_present() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http: std::collections::HashMap::new(),
            polkit_rule_present: true,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        assert!(snap.polkit_power_advisory.is_none());
    }

    /// Missing rule -> a one-line advisory naming the doc, surfaced in the
    /// snapshot regardless of every other field's state (agent down here,
    /// same "informational, never fatal" contract as `logs`/`config`).
    #[test]
    fn polkit_advisory_present_when_rule_missing() {
        let env = FakeEnv {
            show: "ActiveState=active\nSubState=running\n".into(),
            journal: vec![],
            http: std::collections::HashMap::new(),
            polkit_rule_present: false,
        };
        let snap = collect_snapshot(&env, &crate::console::names::ToolNames::from_env());
        let advisory = snap
            .polkit_power_advisory
            .expect("missing rule must produce an advisory");
        assert!(advisory.contains("docs/reference/power-controls.md"));
    }
}
