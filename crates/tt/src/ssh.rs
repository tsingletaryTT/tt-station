//! `tt ssh-authorize`'s guts: SSH key selection/generation, the install
//! label format, and the reusable "read/gen key, call the agent" routine.
//!
//! Split out from `main.rs` for two reasons: (1) the key-selection and
//! label-format logic is pure and worth unit-testing in isolation (see the
//! `tests` module below -- no filesystem fixture beyond a `tempfile` dir,
//! no network, no clock), and (2) [`authorize`] is meant to be called again
//! by Task 7's `tt pair --enable-ssh`, which already has an [`AgentClient`]
//! in hand and just wants "make sure my key is on this box" without
//! re-shelling out to `tt ssh-authorize` as a subprocess.
//!
//! NEVER reads or transmits the private half of a keypair -- every path
//! below only ever touches the `.pub` file.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use libttstation::agent_client::{AgentClient, SshAuthorizeResult};

/// Pick which public key file `tt ssh-authorize` should install, preferring
/// a modern ed25519 key over a legacy RSA one; `None` if `ssh_dir` has
/// neither. Pure over a directory path (no `$HOME` lookup inside) so it's
/// unit-testable against a fixture temp dir instead of the operator's real
/// `~/.ssh`.
pub fn select_public_key_path(ssh_dir: &Path) -> Option<PathBuf> {
    for name in ["id_ed25519.pub", "id_rsa.pub"] {
        let candidate = ssh_dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The `ttstation:<host>:<date>` marker `tt ssh-authorize` tags onto every
/// key it installs -- lets a later look at `authorized_keys` (or a
/// `--revoke` by label, though this CLI prefers revoking by key material --
/// see `cmd_ssh_revoke` in `main.rs`) tell which box/day authorized which
/// key. Pure (the date is a parameter, not computed in here) so it's
/// unit-testable without mocking the system clock.
pub fn ssh_label(host: &str, date: &str) -> String {
    format!("ttstation:{host}:{date}")
}

/// Today's date as `YYYY-MM-DD`, derived from the system clock.
///
/// This workspace has no date/time-formatting dependency (no `chrono`,
/// `time`, etc. -- see `crates/tt/Cargo.toml`) and pulling one in just for a
/// single `YYYY-MM-DD` string felt like overkill, so this hand-rolls the
/// Unix-days -> civil-calendar conversion via [`civil_from_unix_days`]
/// (Howard Hinnant's well-known `civil_from_days` algorithm -- proleptic
/// Gregorian, correct for any day count, no leap-year special-casing bugs).
/// `--date` on `tt ssh-authorize` lets an operator/script override this
/// entirely for determinism; `ssh_label`'s own unit tests pass a literal
/// date rather than depending on this function at all.
pub fn today_ymd() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    civil_from_unix_days(days)
}

/// Convert a day count since the Unix epoch (1970-01-01 = day 0) into
/// `YYYY-MM-DD`. Split out from [`today_ymd`] so the calendar math is
/// unit-testable against known day counts (see `tests::civil_from_unix_days_*`
/// below) without touching the system clock.
///
/// Algorithm: Howard Hinnant's `civil_from_days`
/// (<http://howardhinnant.github.io/date_algorithms.html>) -- a
/// closed-form proleptic-Gregorian conversion, not a loop over months, so
/// there's no off-by-one risk around leap years.
pub fn civil_from_unix_days(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day-of-era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year-of-era, [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day-of-year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month-index (0=Mar..11=Feb), [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // day-of-month, [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // real month, [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The outcome of a successful `authorize` call, independent of `--json`/
/// human-text formatting -- what both `tt ssh-authorize` and (eventually)
/// `tt pair --enable-ssh` need to report to their caller.
pub struct AuthorizeOutcome {
    pub authorized: bool,
    pub ssh_user: String,
    pub already_present: bool,
    pub public_key_path: PathBuf,
}

/// Resolve (generating if necessary) the operator's SSH public key under
/// `home/.ssh`, then hand it to `client` tagged with `ssh_label(host,
/// date)`. This is the ENTIRE "make sure my Mac's key is on this box"
/// routine -- `tt ssh-authorize` (`main.rs::cmd_ssh_authorize`) is a thin
/// wrapper that just resolves `client`/`home`/`date` and prints the result,
/// specifically so Task 7's `tt pair --enable-ssh` can call this directly
/// with the `AgentClient` it already built for pairing, instead of shelling
/// out to `tt ssh-authorize` as a subprocess.
///
/// NEVER reads the private key -- only `<key>.pub`'s contents are read and
/// sent.
pub async fn authorize(
    client: &AgentClient,
    home: &Path,
    host: &str,
    date: &str,
) -> Result<AuthorizeOutcome> {
    let ssh_dir = home.join(".ssh");

    let key_path = match select_public_key_path(&ssh_dir) {
        Some(p) => p,
        None => {
            generate_ed25519_keypair(&ssh_dir)?;
            select_public_key_path(&ssh_dir).ok_or_else(|| {
                anyhow::anyhow!(
                    "ssh-keygen ran but {} still has no id_ed25519.pub",
                    ssh_dir.display()
                )
            })?
        }
    };

    let public_key = read_public_key(&key_path)?;
    let label = ssh_label(host, date);

    let result: SshAuthorizeResult = client.ssh_authorize(&public_key, &label).await?;

    Ok(AuthorizeOutcome {
        authorized: result.authorized,
        ssh_user: result.ssh_user,
        already_present: result.already_present,
        public_key_path: key_path,
    })
}

/// Read a `.pub` file's contents as the single trimmed line `ssh_authorize`
/// expects to send -- split out from [`authorize`] mainly so the "this is
/// the ONLY place a key's bytes get read off disk" boundary is one small,
/// obviously-correct function instead of buried inline.
fn read_public_key(path: &Path) -> Result<String> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(contents.trim().to_string())
}

/// Generate a fresh ed25519 keypair at `<ssh_dir>/id_ed25519[.pub]` by
/// shelling out to the system `ssh-keygen` (present on both macOS and
/// Linux; not worth a Rust SSH-keygen dependency for a one-shot bootstrap
/// path). Owner-verified, not unit-tested -- it mutates real files under
/// `~/.ssh` and depends on an external binary being on `$PATH`.
///
/// `-N ""` = no passphrase: this key exists so an unattended `tt`
/// invocation can install/rotate it without a human typing a passphrase
/// every time, which is the whole point of this command existing. `-C`
/// tags the comment with the Mac's hostname purely so a human eyeballing
/// `authorized_keys` on the box later can tell whose key is whose --
/// that comment is never read back or parsed by anything in this codebase.
fn generate_ed25519_keypair(ssh_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(ssh_dir)
        .with_context(|| format!("creating {}", ssh_dir.display()))?;

    let key_path = ssh_dir.join("id_ed25519");
    let comment = format!("ttstation:{}", mac_hostname());

    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-N")
        .arg("")
        .arg("-f")
        .arg(&key_path)
        .arg("-C")
        .arg(&comment)
        .status()
        .context("running `ssh-keygen -t ed25519` -- is ssh-keygen on $PATH?")?;

    if !status.success() {
        anyhow::bail!("ssh-keygen exited with {status}");
    }
    Ok(())
}

/// The Mac's hostname, used only as the freshly generated key's `-C`
/// comment (cosmetic). This workspace has no `hostname`/`gethostname`
/// dependency, so this shells out to the `hostname` command -- the same
/// external-process pattern [`generate_ed25519_keypair`] already uses for
/// `ssh-keygen` -- rather than adding a new crate for one comment string.
/// Falls back to a fixed placeholder on any failure (missing binary,
/// non-UTF8 output, sandboxed CI) instead of failing the whole authorize
/// flow over a comment.
fn mac_hostname() -> String {
    Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mac".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No `.pub` file at all -> `None`. Uses a fresh `tempfile` dir so this
    /// never touches (or depends on the contents of) the real `~/.ssh`.
    #[test]
    fn select_public_key_path_none_when_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(select_public_key_path(dir.path()), None);
    }

    /// Only an RSA key present -> falls back to it.
    #[test]
    fn select_public_key_path_falls_back_to_rsa() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa.pub"), "ssh-rsa AAAA... rsa\n").unwrap();

        assert_eq!(
            select_public_key_path(dir.path()),
            Some(dir.path().join("id_rsa.pub"))
        );
    }

    /// Both present -> ed25519 wins (preferred over the legacy RSA key).
    #[test]
    fn select_public_key_path_prefers_ed25519_over_rsa() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("id_rsa.pub"), "ssh-rsa AAAA... rsa\n").unwrap();
        std::fs::write(
            dir.path().join("id_ed25519.pub"),
            "ssh-ed25519 AAAA... ed25519\n",
        )
        .unwrap();

        assert_eq!(
            select_public_key_path(dir.path()),
            Some(dir.path().join("id_ed25519.pub"))
        );
    }

    /// A directory that doesn't even exist yet (e.g. an operator who has
    /// never used SSH on this Mac) must behave exactly like an empty one --
    /// `None`, not an error/panic -- since `authorize` relies on this to
    /// decide whether to generate a key.
    #[test]
    fn select_public_key_path_none_when_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(select_public_key_path(&missing), None);
    }

    #[test]
    fn ssh_label_format() {
        assert_eq!(
            ssh_label("qb2-lab.local:8765", "2026-07-05"),
            "ttstation:qb2-lab.local:8765:2026-07-05"
        );
    }

    /// Epoch day 0 is the Unix epoch itself.
    #[test]
    fn civil_from_unix_days_epoch() {
        assert_eq!(civil_from_unix_days(0), "1970-01-01");
    }

    /// Day 10957 is 2000-01-01 -- a widely known reference point (2000 was
    /// exactly 7 leap years after 1970 across those 30 years), used here as
    /// an independent cross-check on the closed-form calendar math rather
    /// than trusting the same code that computes it.
    #[test]
    fn civil_from_unix_days_y2k() {
        assert_eq!(civil_from_unix_days(10_957), "2000-01-01");
    }

    /// Day 19782 is 2024-02-29 -- exercises the leap-day edge specifically
    /// (2024 is a leap year; this is exactly the day the closed-form
    /// month/day split is most likely to be off-by-one on if it were
    /// wrong).
    #[test]
    fn civil_from_unix_days_leap_day() {
        assert_eq!(civil_from_unix_days(19_782), "2024-02-29");
    }

    /// Day 10956 is 1999-12-31 -- the day *before* the Y2K reference point,
    /// so together with `civil_from_unix_days_y2k` this pins down the
    /// year-rollover boundary exactly.
    #[test]
    fn civil_from_unix_days_before_y2k() {
        assert_eq!(civil_from_unix_days(10_956), "1999-12-31");
    }
}
