//! `tt console`: the operator TUI for managing this box's agent as a
//! systemd `--user` service, plus two non-interactive escape hatches used by
//! other tools instead of the TUI itself:
//!
//!   - `--snapshot`: print one [`libttstation::model::BoxLifecycleSnapshot`]
//!     as JSON and exit -- this is what the GTK box panel polls instead of
//!     re-implementing the systemctl/journalctl/HTTP collection logic in
//!     Python (see `console::env::collect_snapshot`).
//!   - `--install-service`: write/refresh the systemd unit file and exit --
//!     the one-time (or re-run-after-upgrade) setup step before the service
//!     can be started at all.
//!
//! Both flags run and return before anything TUI-related is touched; the
//! interactive TUI itself is `ui::run_tui` (Task 7 -- see that module's doc
//! for why it's a stub here).

pub mod actions;
pub mod env;
pub mod names;
pub mod state;
mod ui;

use env::RealLifecycleEnv;
use names::ToolNames;

/// Entry point for `tt console [--snapshot] [--install-service] [--ctrl-port
/// <port>]`, dispatched from `main.rs`'s `Command::Console` arm.
///
/// Exactly one of three things happens, in this priority order:
///   1. `install_service` -- resolve the agent binary's path and install the
///      systemd unit, then return. Takes priority over `--snapshot` so
///      `tt console --snapshot --install-service` (an odd combination, but
///      not one worth rejecting) does the more consequential action.
///   2. `snapshot` -- collect and print one [`BoxLifecycleSnapshot`] as JSON,
///      then return.
///   3. otherwise -- launch the interactive TUI (`ui::run_tui`, Task 7).
///
/// `json` is accepted (mirroring every other `tt` subcommand's global
/// `--json` flag) but unused today: `--snapshot`'s output is ALWAYS JSON
/// (that's the whole point of the flag), `--install-service` always prints a
/// human confirmation line, and the TUI has no non-interactive output mode.
/// It's threaded through anyway so Task 7 can decide whether the TUI itself
/// should honor it (e.g. a future non-interactive TUI status dump) without
/// another signature change here.
///
/// [`BoxLifecycleSnapshot`]: libttstation::model::BoxLifecycleSnapshot
pub fn run_console(
    ctrl_port: u16,
    snapshot: bool,
    install_service: bool,
    _json: bool,
) -> anyhow::Result<()> {
    let names = ToolNames::from_env();
    let env = RealLifecycleEnv {
        names: names.clone(),
        ctrl_port,
    };

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

    ui::run_tui(&env, &names, ctrl_port)
}

/// Resolve the absolute path to `agent_bin` for the systemd unit's
/// `ExecStart=` line -- systemd `--user` units need an absolute path (a bare
/// command name relies on `$PATH`, which a unit's minimal exec environment
/// may not carry the same way an interactive shell's does).
///
/// Resolution order:
///   1. A PATH scan: split `$PATH` on `:` and check `<dir>/<agent_bin>`
///      exists as a file. Deliberately NOT the `which` crate -- this is a
///      four-line scan and the crate's the only reason to add a new
///      dependency for one call site.
///   2. `$HOME/.local/bin/<agent_bin>`, if that file exists -- matches this
///      project's documented install location (see the top-level CLAUDE.md's
///      "Rust CLIs with a `~/.local/bin` copy" note).
///   3. The bare name, unresolved -- lets `install_service` still write
///      SOMETHING rather than fail outright; a systemd unit with a bare
///      `ExecStart=` will simply fail to start until the operator fixes it,
///      which is a clearer failure mode than refusing to install at all.
fn which_agent(agent_bin: &str) -> String {
    if let Some(path) = scan_path_for(agent_bin) {
        return path;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let candidate = format!("{home}/.local/bin/{agent_bin}");
    if std::path::Path::new(&candidate).exists() {
        candidate
    } else {
        agent_bin.to_string()
    }
}

/// Scan `$PATH` for `name`, returning the first `<dir>/<name>` that exists as
/// a file. Split out from [`which_agent`] so the directory-list logic is
/// testable independent of real env vars/filesystem state (see
/// `first_hit_in_dirs` below).
fn scan_path_for(name: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    first_hit_in_dirs(std::env::split_paths(&path_var), name)
}

/// Pure helper: given a sequence of candidate directories and a binary name,
/// return the first `<dir>/<name>` that exists as a file. Pulled out of
/// [`scan_path_for`] so tests can pass a fixed list of directories (e.g. a
/// `tempfile::tempdir()`) instead of depending on the real `$PATH`.
fn first_hit_in_dirs<I: IntoIterator<Item = std::path::PathBuf>>(
    dirs: I,
    name: &str,
) -> Option<String> {
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_hit_in_dirs_finds_executable_in_second_dir() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp2.path().join("tt-station-agentd"), b"").unwrap();

        let hit = first_hit_in_dirs(
            vec![tmp1.path().to_path_buf(), tmp2.path().to_path_buf()],
            "tt-station-agentd",
        );
        assert_eq!(
            hit,
            Some(
                tmp2.path()
                    .join("tt-station-agentd")
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    #[test]
    fn first_hit_in_dirs_returns_none_when_nothing_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let hit = first_hit_in_dirs(vec![tmp.path().to_path_buf()], "does-not-exist-anywhere");
        assert_eq!(hit, None);
    }

    /// `which_agent` must never panic/hang when the name is bogus and none of
    /// the fallbacks exist -- it degrades to the bare name.
    #[test]
    fn which_agent_falls_back_to_bare_name_when_unresolvable() {
        // A name astronomically unlikely to exist on `$PATH` or in
        // `~/.local/bin` on any real machine or CI runner.
        let name = "tt-station-agentd-does-not-exist-xyz123";
        assert_eq!(which_agent(name), name);
    }
}
