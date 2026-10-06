//! Finding and vetting Tenstorrent's **official** `tt` CLI (`tenstorrent/tt-cli`).
//!
//! tt-station delegates to the official CLI wherever it can (`tt model list`, `tt serve`,
//! `tt model ps`, `tt model stop`). Two programs need to find it: the `tt-station local`
//! command and the agent daemon's `tt` serving backend. Both use this module, so they agree on
//! where to look and on what counts as the official binary.
//!
//! **Why a binary named `tt` has to be checked.** On the owner's Mac, `~/.local/bin/tt` was once a
//! stale copy of tt-station's own CLI from before it was renamed. That copy shadowed the official
//! tool. It had no `--version` flag and answered `--help` with "Operator CLI for tt-station". The
//! official CLI answers `--version` with `tt <semver>` (for example `tt 1.0.1`). Anything else is
//! treated as not the official CLI, and callers refuse it with a message that says so.
//!
//! **Naming rule.** `tt` belongs to the official CLI. Nothing tt-station ships is named `tt`.
//! [`OFFICIAL_TT_DEFAULT_BIN`] is the only place in this repo that spells the official binary's
//! name, so that a rename upstream is a one-line change here.

use std::path::{Path, PathBuf};

/// Environment variable that names the official CLI's binary explicitly. Tests point it at a
/// fake. When it is set, no search happens.
pub const OFFICIAL_TT_ENV: &str = "TTS_OFFICIAL_TT_BIN";

/// The official CLI's binary name. This is the only definition of it in the repo; see the module
/// doc for why.
pub const OFFICIAL_TT_DEFAULT_BIN: &str = "tt";

/// Read the version from the official CLI's `--version` output.
///
/// Returns `Some(version)` only when the first line is `tt <semver>`: the literal `tt`, one
/// space, then `MAJOR.MINOR.PATCH` in digits, optionally followed by a `-pre` or `+build` suffix
/// made of letters, digits, `.` and `-`. `"tt 1.0.1"` gives `Some("1.0.1")`. `"tt-cli 0.0.3"`,
/// `"Operator CLI for tt-station"` and an empty string give `None`.
///
/// The caller must also check that the process exited successfully. This function sees only the
/// text.
pub fn parse_version(stdout: &str) -> Option<String> {
    let first = stdout.lines().next().unwrap_or("").trim();
    let version = first.strip_prefix("tt ")?;
    is_semver(version).then(|| version.to_string())
}

/// `MAJOR.MINOR.PATCH[(-|+)suffix]`, with each of the three core parts made only of digits.
fn is_semver(v: &str) -> bool {
    // Split off an optional pre-release or build suffix at the first '-' or '+'.
    let (core, suffix) = match v.find(['-', '+']) {
        Some(i) => (&v[..i], Some(&v[i + 1..])),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let core_ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    let suffix_ok = suffix.is_none_or(|s| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
    });
    core_ok && suffix_ok
}

/// The error text for a binary that did not answer `--version` with `tt <semver>`. Shared so that
/// `tt-station local` and the agent give the same advice.
pub fn not_official_message(bin: &str, got: &str) -> String {
    let first = got.lines().next().unwrap_or("").trim();
    format!(
        "`{bin}` is not the official Tenstorrent CLI: `{bin} --version` gave {first:?}, \
         expected `tt <semver>`. Check `which -a tt` for a shadowing binary, then: \
         uv tool install tenstorrent"
    )
}

/// Where the official `tt` lives: the first `tt` on `PATH`, else uv's and Homebrew's install
/// locations.
///
/// The fallback matters because two of our callers run with a short `PATH`. The macOS app runs
/// `tt-station` with launchd's minimal `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`), and a
/// `systemctl --user` unit gets a similarly short one. Neither includes `~/.local/bin`, which is
/// where `uv tool install tenstorrent` puts the official CLI.
///
/// A `tt` that is on `PATH` always wins, even if it turns out not to be the official one. The
/// caller then refuses it loudly instead of quietly preferring another copy, because a shadowed
/// `tt` is a problem the operator should see.
pub fn locate(path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Option<String> {
    let is_exe = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    let on_path = path
        .map(|p| {
            std::env::split_paths(p)
                .map(|d| d.join(OFFICIAL_TT_DEFAULT_BIN))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let fallbacks = home
        .map(|h| h.join(".local/bin").join(OFFICIAL_TT_DEFAULT_BIN))
        .into_iter()
        .chain(
            ["/opt/homebrew/bin", "/usr/local/bin"]
                .map(|d| PathBuf::from(d).join(OFFICIAL_TT_DEFAULT_BIN)),
        );
    on_path
        .into_iter()
        .chain(fallbacks)
        .find(|p| is_exe(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// The binary to run as the official CLI, before any version check: [`OFFICIAL_TT_ENV`] if set
/// and non-empty, else [`locate`] against this process's `PATH` and `HOME`, else the bare
/// [`OFFICIAL_TT_DEFAULT_BIN`] (which then fails with a clear "could not run" error if it is not
/// on `PATH` either).
pub fn resolve_bin() -> String {
    if let Some(explicit) = std::env::var(OFFICIAL_TT_ENV)
        .ok()
        .filter(|s| !s.is_empty())
    {
        return explicit;
    }
    locate(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    )
    .unwrap_or_else(|| OFFICIAL_TT_DEFAULT_BIN.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_real_official_version_line() {
        // Captured from this box: `~/.local/bin/tt --version` (2026-10-05).
        assert_eq!(parse_version("tt 1.0.1\n").as_deref(), Some("1.0.1"));
        assert_eq!(
            parse_version("tt 2.10.0-rc.1").as_deref(),
            Some("2.10.0-rc.1")
        );
        assert_eq!(
            parse_version("tt 1.0.1+abc.1").as_deref(),
            Some("1.0.1+abc.1")
        );
    }

    #[test]
    fn rejects_anything_that_is_not_tt_semver() {
        for bad in [
            "tt-cli 0.0.3",
            "Operator CLI for tt-station",
            "",
            "tt ",
            "tt 1",
            "tt 1.0",
            "tt v1.0.1",
            "tt 1.0.x",
            "tt 1.0.1-",
            "tt 1.0.1 extra",
            "TT 1.0.1",
            "tt  1.0.1",
        ] {
            assert_eq!(parse_version(bad), None, "{bad:?} must be refused");
        }
    }

    #[test]
    fn refusal_message_names_the_expected_form() {
        let m = not_official_message("/usr/bin/tt", "tt-cli 0.0.3\nmore");
        assert!(m.contains("tt <semver>"), "{m}");
        assert!(m.contains("tt-cli 0.0.3"), "{m}");
        assert!(!m.contains("more"), "only the first line is quoted: {m}");
    }

    #[test]
    fn locate_prefers_path_then_falls_back_to_uv_location() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tt = bin.join("tt");
        std::fs::write(&tt, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tt, std::fs::Permissions::from_mode(0o755)).unwrap();

        // A GUI-style PATH without ~/.local/bin still finds uv's install.
        let gui_path = std::ffi::OsString::from("/nonexistent-a:/nonexistent-b");
        assert_eq!(
            locate(Some(&gui_path), Some(home.path())),
            Some(tt.to_string_lossy().into_owned())
        );

        // Something on PATH wins over the fallback (and is then vetted by the caller).
        let other = tempfile::tempdir().unwrap();
        let other_tt = other.path().join("tt");
        std::fs::write(&other_tt, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&other_tt, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            locate(Some(other.path().as_os_str()), Some(home.path())),
            Some(other_tt.to_string_lossy().into_owned())
        );
    }
}
