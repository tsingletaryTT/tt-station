//! `LifecycleActions`: the operator-facing verbs `tt console` exposes on top
//! of [`LifecycleEnv`] -- start/stop/restart the agent's systemd unit, pin a
//! profile via a systemd drop-in, and install the unit file itself.
//!
//! Every side effect goes through `LifecycleEnv::run` (so tests can assert
//! the exact argv a `FakeEnv` recorded) except the drop-in/unit file writes,
//! which go straight through `std::fs` -- there's no meaningful "fake" for
//! "write these bytes to this path" the way there is for "did you shell out
//! to systemctl with the right verb," and threading file I/O through the
//! trait too would just be indirection with nothing to fake against.
//!
//! Reset and pair-localhost are NOT here -- they reuse existing `tt` command
//! fns (`cmd_reset`, the pairing flow) and are wired as thin wrappers in a
//! later task, rather than duplicating the HTTP calls those fns already make.
//!
//! `tt` is a bin crate; nothing in `main.rs` constructs a [`LifecycleActions`]
//! yet (that's the `tt console` wiring task). Until then rustc's dead-code
//! lint would flag this whole module as unused -- same situation as
//! `console::names`/`console::state`/`console::env`, and the same fix: allow
//! it here and drop the allow once something calls it.
#![allow(dead_code)]

use crate::console::env::LifecycleEnv;
use crate::console::names::ToolNames;
use std::path::PathBuf;

/// The systemd unit template, baked into the binary at compile time so
/// `tt console install` never depends on a copy of `deploy/` being present
/// on the target box (the binary carries its own template). The four `../`
/// climb from `crates/tt/src/console/` (this file's directory) up through
/// `src/`, `tt/`, `crates/` to the repo root, where `deploy/` lives.
const UNIT_TEMPLATE: &str = include_str!("../../../../deploy/tt-station-agentd.service");

/// Fill the [`UNIT_TEMPLATE`] placeholders: `{{AGENT_BIN}}` with the absolute
/// path to the installed `tt-station-agentd` binary, and `{{PATH_ENV}}` with
/// the PATH the service should run with (systemd --user's default PATH omits
/// ~/.local/bin and any venv). Callers should treat a leftover `{{...}}` in the
/// result as a template bug -- see the `unit_template_fills_agent_bin_and_path`
/// test, which asserts both placeholders are gone.
pub fn render_unit(agent_bin_path: &str, path_env: &str) -> String {
    UNIT_TEMPLATE
        .replace("{{AGENT_BIN}}", agent_bin_path)
        .replace("{{PATH_ENV}}", path_env)
}

/// Render a systemd drop-in that pins the agent to a specific `--profile`.
/// The blank `ExecStart=` line before the real one is the systemd idiom for
/// *clearing* the unit's original `ExecStart=` before setting a new one --
/// without it, a drop-in's `ExecStart=` would be appended as an ADDITIONAL
/// command to run, not a replacement (systemd unit directives that support
/// multiple values accumulate across drop-ins by default).
///
/// `agent_bin` MUST be an absolute path, exactly like [`render_unit`]'s
/// `agent_bin_path` -- systemd `--user` resolves a non-absolute `ExecStart=`
/// against a fixed compiled-in search path that does NOT include
/// `~/.local/bin` or a repo's `./target/release`, so a drop-in built from a
/// bare binary name can silently fail to start the unit even though the base
/// unit (installed via [`render_unit`]) starts fine. Callers MUST resolve the
/// path the same way the base unit does (`super::which_agent`) before
/// calling this -- see [`LifecycleActions::set_profile`], which takes the
/// resolved path as a parameter rather than reading `self.names.agent_bin`
/// for exactly this reason.
pub fn render_profile_dropin(agent_bin: &str, profile: &str) -> String {
    format!("[Service]\nExecStart=\nExecStart={agent_bin} --profile {profile}\n")
}

/// Resolve `~/.config/systemd/user` (or `$XDG_CONFIG_HOME/systemd/user` when
/// that's set) as a pure function of already-read env values -- no
/// `std::env` access inside, so it's directly unit-testable without
/// mutating process-global env vars (which would race under the default
/// parallel `cargo test` harness; see `console::names::resolve` for the same
/// pattern applied to `ToolNames`).
fn resolve_config_systemd_user_dir(
    xdg_config_home: Option<String>,
    home: Option<String>,
) -> PathBuf {
    let base = xdg_config_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home.unwrap_or_default()).join(".config"));
    base.join("systemd").join("user")
}

/// Read the real env vars and delegate to [`resolve_config_systemd_user_dir`].
/// Not itself unit-tested (it touches `std::env`) -- the pure function above
/// carries the test coverage for the defaulting logic.
fn dirs_config_systemd_user() -> PathBuf {
    resolve_config_systemd_user_dir(
        std::env::var("XDG_CONFIG_HOME").ok(),
        std::env::var("HOME").ok(),
    )
}

/// Operator actions for `tt console`: start/stop/restart the agent's systemd
/// unit, pin a profile, and install the unit file. Every `systemctl` call
/// goes through `env.run` so tests can assert exact argv against a recording
/// fake; see the module doc for why the drop-in/unit file writes go straight
/// through `std::fs` instead.
pub struct LifecycleActions<'a> {
    env: &'a dyn LifecycleEnv,
    names: &'a ToolNames,
    /// `~/.config/systemd/user` (or `$XDG_CONFIG_HOME` equivalent), resolved
    /// once at construction. Overridable via [`LifecycleActions::with_config_dir`]
    /// (test-only) so file-I/O tests point at a `tempfile::tempdir()` instead
    /// of mutating `$XDG_CONFIG_HOME`/`$HOME` process-wide -- those are
    /// global process state, so mutating them in a test would race with
    /// every other test in this binary under the default parallel harness.
    config_dir: PathBuf,
}

impl<'a> LifecycleActions<'a> {
    pub fn new(env: &'a dyn LifecycleEnv, names: &'a ToolNames) -> Self {
        Self {
            env,
            names,
            config_dir: dirs_config_systemd_user(),
        }
    }

    /// Test-only constructor: same as [`LifecycleActions::new`] but with an
    /// explicit `config_dir` override, so `set_profile`/`install_service`
    /// tests can write into a `tempfile::tempdir()` without touching the
    /// real `~/.config` or racing on process-global env vars with sibling
    /// tests.
    #[cfg(test)]
    fn with_config_dir(
        env: &'a dyn LifecycleEnv,
        names: &'a ToolNames,
        config_dir: PathBuf,
    ) -> Self {
        Self {
            env,
            names,
            config_dir,
        }
    }

    pub fn start(&self) -> anyhow::Result<()> {
        self.env
            .run(&["systemctl", "--user", "start", &self.names.service_name])
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        self.env
            .run(&["systemctl", "--user", "stop", &self.names.service_name])
    }

    pub fn restart(&self) -> anyhow::Result<()> {
        self.env
            .run(&["systemctl", "--user", "restart", &self.names.service_name])
    }

    /// Pin the agent to `profile` by writing a systemd drop-in under
    /// `<config_dir>/<unit>.d/profile.conf`, reloading the systemd user
    /// manager so it picks up the new drop-in, then restarting the unit so
    /// the new `ExecStart=` actually takes effect.
    ///
    /// `agent_bin_path` MUST be the same absolute path used to install the
    /// base unit (see [`Self::install_service`] and `super::which_agent`) --
    /// this used to default to `self.names.agent_bin` (a bare binary name),
    /// which systemd `--user` cannot resolve on `ExecStart=` (it does not
    /// consult `$PATH`, `~/.local/bin`, or a repo's `./target/release`), so
    /// applying a profile could write a drop-in that made the unit fail to
    /// start. Callers resolve the path once (`super::which_agent(&names.
    /// agent_bin)`) and pass it in here, exactly like `install_service`
    /// already requires.
    pub fn set_profile(&self, profile: &str, agent_bin_path: &str) -> anyhow::Result<()> {
        let dir = self
            .config_dir
            .join(format!("{}.d", self.names.service_name));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("profile.conf"),
            render_profile_dropin(agent_bin_path, profile),
        )?;
        self.env.run(&["systemctl", "--user", "daemon-reload"])?;
        self.restart()
    }

    /// Install (or update) the unit file at `<config_dir>/<unit>`, then
    /// reload the systemd user manager. Idempotent: if the file already
    /// exists with identical content, it's left untouched (avoids bumping
    /// its mtime / triggering unnecessary reloads on every call).
    pub fn install_service(&self, agent_bin_path: &str) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let path = self.config_dir.join(&self.names.service_name);
        // Capture the operator's own PATH at install time and bake it into the
        // unit, so the service (which otherwise runs with systemd's minimal
        // PATH) can find tt-smi, docker, and the venv python3 it needs to serve.
        let path_env = std::env::var("PATH").unwrap_or_default();
        let content = render_unit(agent_bin_path, &path_env);
        let needs_write = !path.exists()
            || std::fs::read_to_string(&path)
                .map(|existing| existing != content)
                .unwrap_or(true);
        if needs_write {
            std::fs::write(&path, content)?;
        }
        self.env.run(&["systemctl", "--user", "daemon-reload"])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct RecEnv {
        calls: RefCell<Vec<Vec<String>>>,
    }
    impl LifecycleEnv for RecEnv {
        fn systemctl_show(&self, _: &str) -> anyhow::Result<String> {
            Ok(String::new())
        }
        fn journal_tail(&self, _: &str, _: u32) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn http_get(&self, _: &str) -> anyhow::Result<String> {
            anyhow::bail!("n/a")
        }
        fn run(&self, argv: &[&str]) -> anyhow::Result<()> {
            self.calls
                .borrow_mut()
                .push(argv.iter().map(|s| s.to_string()).collect());
            Ok(())
        }
        fn now_unix(&self) -> u64 {
            0
        }
    }
    fn rec_env() -> RecEnv {
        RecEnv {
            calls: RefCell::new(vec![]),
        }
    }
    fn names() -> ToolNames {
        ToolNames {
            tt_bin: "tt".into(),
            agent_bin: "tt-station-agentd".into(),
            service_name: "svc.service".into(),
        }
    }

    #[test]
    fn start_uses_systemctl_user() {
        let env = rec_env();
        LifecycleActions::new(&env, &names()).start().unwrap();
        assert_eq!(
            env.calls.borrow()[0],
            vec!["systemctl", "--user", "start", "svc.service"]
        );
    }

    #[test]
    fn stop_uses_systemctl_user() {
        let env = rec_env();
        LifecycleActions::new(&env, &names()).stop().unwrap();
        assert_eq!(
            env.calls.borrow()[0],
            vec!["systemctl", "--user", "stop", "svc.service"]
        );
    }

    #[test]
    fn restart_uses_systemctl_user() {
        let env = rec_env();
        LifecycleActions::new(&env, &names()).restart().unwrap();
        assert_eq!(
            env.calls.borrow()[0],
            vec!["systemctl", "--user", "restart", "svc.service"]
        );
    }

    #[test]
    fn drop_in_content_pins_profile() {
        // C1 regression: the drop-in's `ExecStart=` must carry the same
        // ABSOLUTE path the base unit uses -- a bare binary name here is a
        // unit that can fail to start (see the function doc).
        let content = render_profile_dropin("/home/x/.local/bin/tt-station-agentd", "bleeding");
        assert!(content.contains("[Service]"));
        assert!(content.contains("ExecStart=\n")); // reset then re-set
        assert!(
            content.contains("ExecStart=/home/x/.local/bin/tt-station-agentd --profile bleeding")
        );
    }

    #[test]
    fn unit_template_fills_agent_bin_and_path() {
        let unit = render_unit(
            "/home/x/.local/bin/tt-station-agentd",
            "/home/x/.local/bin:/usr/bin:/bin",
        );
        assert!(unit.contains("ExecStart=/home/x/.local/bin/tt-station-agentd"));
        assert!(unit.contains("Environment=PATH=/home/x/.local/bin:/usr/bin:/bin"));
        assert!(!unit.contains("{{AGENT_BIN}}"));
        assert!(!unit.contains("{{PATH_ENV}}"));
    }

    #[test]
    fn config_dir_prefers_xdg_config_home() {
        let dir = resolve_config_systemd_user_dir(
            Some("/custom/xdg".to_string()),
            Some("/home/someone".to_string()),
        );
        assert_eq!(dir, PathBuf::from("/custom/xdg/systemd/user"));
    }

    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let dir = resolve_config_systemd_user_dir(None, Some("/home/someone".to_string()));
        assert_eq!(dir, PathBuf::from("/home/someone/.config/systemd/user"));
    }

    #[test]
    fn config_dir_treats_empty_xdg_as_unset() {
        let dir =
            resolve_config_systemd_user_dir(Some(String::new()), Some("/home/someone".to_string()));
        assert_eq!(dir, PathBuf::from("/home/someone/.config/systemd/user"));
    }

    #[test]
    fn set_profile_writes_dropin_reloads_and_restarts() {
        let tmp = tempfile::tempdir().unwrap();
        let env = rec_env();
        let n = names();
        // Deliberately NOT `n.agent_bin` (a bare name) -- the whole point of
        // the C1 fix is that `set_profile` bakes in whatever absolute path
        // the caller resolved, not a bare binary name it can't itself derive.
        let agent_path = "/opt/tt/tt-station-agentd";
        LifecycleActions::with_config_dir(&env, &n, tmp.path().to_path_buf())
            .set_profile("bleeding", agent_path)
            .unwrap();

        let dropin_path = tmp.path().join("svc.service.d").join("profile.conf");
        let content = std::fs::read_to_string(&dropin_path).unwrap();
        assert!(content.contains("--profile bleeding"));
        assert!(content.contains(agent_path));
        assert!(content.contains(&format!("ExecStart={agent_path} --profile bleeding")));

        let calls = env.calls.borrow();
        assert_eq!(calls[0], vec!["systemctl", "--user", "daemon-reload"]);
        assert_eq!(
            calls[1],
            vec!["systemctl", "--user", "restart", "svc.service"]
        );
    }

    #[test]
    fn install_service_writes_unit_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let env = rec_env();
        let n = names();
        LifecycleActions::with_config_dir(&env, &n, tmp.path().to_path_buf())
            .install_service("/opt/tt/tt-station-agentd")
            .unwrap();

        let unit_path = tmp.path().join("svc.service");
        let content = std::fs::read_to_string(&unit_path).unwrap();
        assert!(content.contains("ExecStart=/opt/tt/tt-station-agentd"));
        assert!(content.contains("Environment=PATH="));
        assert!(!content.contains("{{AGENT_BIN}}"));
        assert!(!content.contains("{{PATH_ENV}}"));

        assert_eq!(
            env.calls.borrow()[0],
            vec!["systemctl", "--user", "daemon-reload"]
        );
    }

    #[test]
    fn install_service_is_idempotent_when_content_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let env = rec_env();
        let n = names();
        let actions = LifecycleActions::with_config_dir(&env, &n, tmp.path().to_path_buf());
        actions
            .install_service("/opt/tt/tt-station-agentd")
            .unwrap();

        let unit_path = tmp.path().join("svc.service");
        let mtime_before = std::fs::metadata(&unit_path).unwrap().modified().unwrap();

        // Re-install with the same content: file should not be rewritten
        // (mtime unchanged), but daemon-reload still runs.
        std::thread::sleep(std::time::Duration::from_millis(10));
        actions
            .install_service("/opt/tt/tt-station-agentd")
            .unwrap();
        let mtime_after = std::fs::metadata(&unit_path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
        assert_eq!(env.calls.borrow().len(), 2);
    }
}
