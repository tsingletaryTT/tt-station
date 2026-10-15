// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Where the processes the agent starts end up when the agent stops.
//!
//! The agent's unit has `KillMode=process` (deploy/tt-station-agentd.service) so
//! a model download keeps going across an agent restart. The catch: everything
//! else the agent started stays behind in its cgroup too, and systemd reports
//! each one as a "left-over process" at the next start -- Hermes and Chrome
//! opened on the desktop (through `gtk-launch`/`xdg-open`, whose apps are the
//! launcher's children), and `tt-toplike` behind a terminal WebSocket. Two
//! fixes, by what the process is:
//!
//! * Something opened FOR the person (an app, a browser tab, a folder) is theirs
//!   and should outlive an agent restart, but not in the agent's cgroup:
//!   [`command`] starts it in its own transient scope (`systemd-run --user
//!   --scope`, like the Code holders' units), so it belongs to the session, not
//!   the agent.
//! * Something that only exists to serve one of the agent's connections (the
//!   `tt-toplike` behind `/toplike/term`) should die with the agent:
//!   [`die_with_parent`] asks the kernel to SIGTERM it when the agent goes.

use std::process::Command;

/// Is the agent running as a systemd unit (so `systemd-run --user` works)?
pub fn under_systemd() -> bool {
    std::env::var_os("INVOCATION_ID").is_some()
}

/// The argv that runs `program args` in its own scope, or as is.
pub fn argv(program: &str, args: &[&str], scoped: bool) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    if scoped {
        v.extend(
            ["systemd-run", "--user", "--scope", "--collect", "--quiet", "--"]
                .map(String::from),
        );
    }
    v.push(program.to_string());
    v.extend(args.iter().map(|a| a.to_string()));
    v
}

/// A `Command` for `program args` that runs in its own transient scope when the
/// agent is a systemd unit. `systemd-run --scope` execs the program in place, so
/// its exit status (and stderr) are the program's own.
pub fn command(program: &str, args: &[&str]) -> Command {
    let argv = argv(program, args, under_systemd());
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Have the kernel SIGTERM this child when the agent exits. Call from
/// `pre_exec`. (The signal follows the spawning thread; the agent spawns these
/// from a runtime worker thread, which lives as long as the agent.)
pub fn die_with_parent() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with PR_SET_PDEATHSIG and a signal number is
        // async-signal-safe and touches no memory.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_commands_run_through_systemd_run() {
        assert_eq!(
            argv("gtk-launch", &["google-chrome", "https://x"], true),
            [
                "systemd-run", "--user", "--scope", "--collect", "--quiet", "--",
                "gtk-launch", "google-chrome", "https://x"
            ]
        );
        assert_eq!(argv("xdg-open", &["https://x"], false), ["xdg-open", "https://x"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_marked_to_die_with_its_parent_gets_sigterm() {
        use std::os::unix::process::CommandExt;
        // PDEATHSIG follows the spawning thread: spawn a marked `sleep` from a
        // thread that then ends (standing in for the agent going away).
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        unsafe {
            cmd.pre_exec(die_with_parent);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let t = std::thread::spawn(move || {
            let child = cmd.spawn().unwrap();
            tx.send(child.id()).unwrap();
            // Not waited on here: the thread ending is the parent going away.
            std::mem::forget(child);
        });
        let pid = rx.recv().unwrap();
        t.join().unwrap();
        // The thread is gone, so the kernel sent SIGTERM; reap it.
        let mut status = 0;
        let waited = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
        assert_eq!(waited, pid as i32);
        assert!(libc::WIFSIGNALED(status), "status {status:#x}");
        assert_eq!(libc::WTERMSIG(status), libc::SIGTERM);
    }
}
