//! The probes behind the agent's smarter defaults, behind one trait so tests can script them.
//!
//! `config::resolve_with` asks a [`Detect`] for a value only when no flag, profile or `[global]`
//! entry supplies it, so a box that sets everything runs none of these probes. [`SystemDetect`]
//! is the real implementation `main` uses. Unit tests use a fake, so nothing in the test suite
//! reads the real hostname, the network, `tt-smi` output or `PATH`.
//!
//! Every probe is best-effort and returns `None` on any failure. The caller owns the fallback
//! chain, and `--print-config` reports which link of it was used.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// How long `tt --version` may take before the official `tt` counts as not found. A healthy
/// `tt` answers in well under a second. A hung one must not hold up agent start-up.
pub const TT_VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `tt-smi -s` may take for chip detection. Matches the start-up device-mesh probe in
/// `main.rs`, which tolerates the same wedged-board case.
pub const TT_SMI_TIMEOUT: Duration = Duration::from_secs(10);

/// A valid official `tt` CLI: where it is and the version it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfficialTt {
    pub bin: String,
    pub version: String,
}

/// Everything the resolver may detect. Each method returns `None` when it cannot tell.
pub trait Detect {
    /// This machine's short hostname (first label, no dots).
    fn hostname(&self) -> Option<String>;
    /// The IPv4 address other machines on the LAN reach this box at.
    fn primary_ipv4(&self) -> Option<String>;
    /// The raw stdout of `<tt_smi_bin> -s`, or `None` when it cannot run, fails, or times out.
    /// The resolver derives `chips` from it and `main` derives `device_mesh` from the same text,
    /// so `tt-smi` runs once per start-up.
    fn tt_smi_snapshot(&self, tt_smi_bin: &str) -> Option<String>;
    /// The official `tt` CLI, when one is installed and answers `--version` with `tt <semver>`.
    /// `explicit_bin` is the configured `official_tt_bin`; `None` means search for it the way
    /// `libttstation::official_tt::resolve_bin` does.
    fn official_tt(&self, explicit_bin: Option<&str>) -> Option<OfficialTt>;
}

/// Detects nothing. Used by the plain `config::resolve`, which therefore behaves as it did
/// before smarter defaults existed.
pub struct NoDetect;

impl Detect for NoDetect {
    fn hostname(&self) -> Option<String> {
        None
    }
    fn primary_ipv4(&self) -> Option<String> {
        None
    }
    fn tt_smi_snapshot(&self, _tt_smi_bin: &str) -> Option<String> {
        None
    }
    fn official_tt(&self, _explicit_bin: Option<&str>) -> Option<OfficialTt> {
        None
    }
}

/// The real probes: the kernel hostname, `ip route get`, `tt-smi -s`, and `tt --version`.
pub struct SystemDetect;

impl Detect for SystemDetect {
    fn hostname(&self) -> Option<String> {
        let raw = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .ok()
            .or_else(|| run_with_timeout("hostname", &[], Duration::from_secs(2)))?;
        short_hostname(&raw)
    }

    fn primary_ipv4(&self) -> Option<String> {
        crate::net::primary_ipv4()
    }

    fn tt_smi_snapshot(&self, tt_smi_bin: &str) -> Option<String> {
        run_with_timeout(tt_smi_bin, &["-s"], TT_SMI_TIMEOUT)
    }

    fn official_tt(&self, explicit_bin: Option<&str>) -> Option<OfficialTt> {
        let bin = explicit_bin
            .map(str::to_string)
            .unwrap_or_else(libttstation::official_tt::resolve_bin);
        let stdout = run_with_timeout(&bin, &["--version"], TT_VERSION_TIMEOUT)?;
        let version = libttstation::official_tt::parse_version(&stdout)?;
        Some(OfficialTt { bin, version })
    }
}

/// The first label of a hostname, or `None` when the hostname is unusable.
///
/// Unusable means empty, `localhost` (a box that answers to it has no name of its own), the
/// kernel's `(none)` placeholder, all digits (an IP-literal hostname such as `10.0.0.5` would
/// otherwise become the name `10`), or anything outside letters, digits, `-` and `_` after
/// trimming (a name goes into an mDNS instance name and a URL, so odd characters are refused
/// rather than escaped).
pub fn short_hostname(raw: &str) -> Option<String> {
    let first = raw.trim().split('.').next().unwrap_or("");
    let usable = !first.is_empty()
        && !first.eq_ignore_ascii_case("localhost")
        && first != "(none)"
        && !first.chars().all(|c| c.is_ascii_digit())
        && first
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    usable.then(|| first.to_string())
}

/// Run `bin args...` and return its stdout, or `None` if it cannot start, exits non-zero, or
/// runs past `timeout` (in which case it is killed).
///
/// Stdout is drained on its own thread. A child that writes more than a pipe buffer (`tt-smi -s`
/// can) would otherwise block on the write and look like a hang.
pub fn run_with_timeout(bin: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    rx.recv_timeout(Duration::from_secs(2)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_hostname_keeps_the_first_label() {
        assert_eq!(short_hostname("qb2-lab\n").as_deref(), Some("qb2-lab"));
        assert_eq!(short_hostname("qb2-lab.local").as_deref(), Some("qb2-lab"));
        assert_eq!(short_hostname("a.b.c.example.com").as_deref(), Some("a"));
        assert_eq!(short_hostname("  Box_1  ").as_deref(), Some("Box_1"));
    }

    #[test]
    fn short_hostname_refuses_unusable_names() {
        for raw in [
            "",
            "\n",
            ".local",
            "localhost",
            "LocalHost.localdomain",
            "(none)",
            "10.0.0.5",
            "12345",
            "bad name",
            "a/b",
            "é",
        ] {
            assert_eq!(short_hostname(raw), None, "{raw:?}");
        }
    }

    // The next tests start `/bin/sh` and `sleep`: ordinary processes that touch no hardware,
    // network or PATH-dependent tool.

    #[test]
    fn run_with_timeout_returns_stdout_of_a_successful_command() {
        let out = run_with_timeout(
            "/bin/sh",
            &["-c", "printf 'tt 1.0.1\\n'"],
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("tt 1.0.1\n"));
    }

    #[test]
    fn run_with_timeout_drains_output_larger_than_a_pipe_buffer() {
        let out = run_with_timeout(
            "/bin/sh",
            &["-c", "head -c 300000 /dev/zero | tr '\\0' x"],
            Duration::from_secs(10),
        )
        .expect("large output must not look like a hang");
        assert_eq!(out.len(), 300_000);
    }

    #[test]
    fn run_with_timeout_treats_failure_and_hang_as_none() {
        assert_eq!(
            run_with_timeout("/bin/sh", &["-c", "exit 3"], Duration::from_secs(5)),
            None
        );
        assert_eq!(
            run_with_timeout("/nonexistent/bin", &[], Duration::from_secs(5)),
            None
        );
        let started = std::time::Instant::now();
        assert_eq!(
            run_with_timeout("/bin/sh", &["-c", "sleep 30"], Duration::from_millis(200)),
            None
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait for the child"
        );
    }
}
