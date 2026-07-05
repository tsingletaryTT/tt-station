//! Pure, security-critical core for the (opt-in) "install my SSH key on the
//! box" pairing feature: validate an SSH *public* key, idempotently append it
//! to an `authorized_keys` file (tagged with a `ttstation:<label>` marker so
//! it can be found again), and revoke it later.
//!
//! Deliberately has **no** knowledge of the agent's HTTP routes, auth
//! tokens, or where `~/.ssh` actually lives on disk -- callers (Task 2's
//! authed route) pass in whatever `Path` they like, which is what makes this
//! module trivial to unit test without ever touching a real home directory.
//!
//! Security posture, spelled out because this module writes to a file that
//! grants SSH login:
//! - [`validate_public_key`] is public-key-only: it rejects anything that
//!   looks like private-key material (`BEGIN ... PRIVATE KEY`), multi-line
//!   input (a pasted private key or multiple keys smuggled in as one field),
//!   empty input, and unrecognized key types.
//! - [`authorize`] always re-validates the key itself (never trust that a
//!   caller upstream already did), dedupes on the key's base64 blob (not the
//!   trailing comment, which is attacker/user controlled and easy to vary),
//!   and never writes a duplicate line for a key that's already present.
//! - Both [`authorize`] and [`revoke`] enforce `0700`/`0600` permissions on
//!   the `.ssh` directory and `authorized_keys` file respectively (on unix)
//!   every time they touch them, not just on first creation -- so a
//!   permissions drift (e.g. a stray `chmod` or a restrictive umask leaving
//!   the file group-readable) self-heals on the next call instead of
//!   silently persisting.

use std::fs;
use std::io::Write as _;
use std::path::Path;

/// An invalid or disallowed SSH public key. Carries a human-readable reason
/// (never the offending key material itself, in case the "key" is actually
/// a fragment of private-key material -- callers should log the variant's
/// `Display`, not the original input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthKeyError(String);

impl std::fmt::Display for AuthKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AuthKeyError {}

/// Result of [`authorize`]: whether the key was newly written or was already
/// present (by blob) and left untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizeOutcome {
    Added,
    AlreadyPresent,
}

/// Which line(s) [`revoke`] should remove: match by the key's base64 blob
/// (dedupe identity), or by the trailing `ttstation:<label>` marker that
/// [`authorize`] appends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revoke {
    Blob(String),
    Label(String),
}

/// SSH public-key type prefixes this module will accept. Deliberately an
/// allow-list (not a deny-list of "isn't a private key") -- anything not on
/// this list is rejected, including key types that may exist but this
/// codebase has never had reason to support.
fn is_known_key_type(key_type: &str) -> bool {
    matches!(key_type, "ssh-ed25519" | "ssh-rsa")
        || key_type.starts_with("ecdsa-sha2-")
        || key_type.starts_with("sk-ssh-")
        || key_type.starts_with("sk-ecdsa-")
}

/// Validate that `s` is a single-line SSH *public* key and return it trimmed
/// of surrounding whitespace. Rejects:
/// - empty input,
/// - anything containing `PRIVATE KEY` (case-insensitive) -- covers PEM/
///   OpenSSH private-key headers regardless of exact casing,
/// - multi-line input (a private key smuggled in, or more than one public
///   key pasted as a single field -- callers should split those themselves
///   and validate each line individually if that's ever a legitimate case),
/// - a first field that isn't a recognized key-type prefix,
/// - a missing/empty second field (the base64 key material).
pub fn validate_public_key(s: &str) -> Result<&str, AuthKeyError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(AuthKeyError("empty key".into()));
    }

    // Check the *untrimmed* original too: private-key headers are usually
    // followed by more lines, but even a bare header line should never slip
    // through just because trimming happened to remove some whitespace.
    if s.to_ascii_uppercase().contains("PRIVATE KEY") {
        return Err(AuthKeyError(
            "private-key material is not accepted (public key only)".into(),
        ));
    }

    if trimmed.contains('\n') {
        return Err(AuthKeyError(
            "multi-line input is not a single public key".into(),
        ));
    }

    let mut fields = trimmed.split_whitespace();
    let key_type = fields
        .next()
        .ok_or_else(|| AuthKeyError("missing key type".into()))?;

    if !is_known_key_type(key_type) {
        return Err(AuthKeyError(format!(
            "unrecognized or unsupported key type {key_type:?}"
        )));
    }

    let blob_present = fields.next().map(|f| !f.is_empty()).unwrap_or(false);
    if !blob_present {
        return Err(AuthKeyError("missing key material (second field)".into()));
    }

    Ok(trimmed)
}

/// The base64 key-material field (the second whitespace-separated token) of
/// a public key line. This -- not the trailing comment/marker, which is
/// freely chosen and easy to vary -- is what identifies "the same key" for
/// dedupe purposes in [`authorize`] and blob-based [`revoke`].
///
/// Works on any line shape (a bare `validate_public_key` result, or a full
/// stored `authorized_keys` line with a trailing comment/marker) since it
/// only ever looks at the second field.
pub fn key_blob(pubkey: &str) -> Option<&str> {
    pubkey.split_whitespace().nth(1)
}

/// Idempotently append `pubkey` to the `authorized_keys` file at `path`,
/// tagged with a trailing `ttstation:<label>` marker (so [`revoke`] can find
/// it again by label without needing to remember the key itself).
///
/// - Re-validates `pubkey` itself (see [`validate_public_key`]) regardless
///   of whether the caller already did -- this is the actual disk-writing
///   half of the security boundary, so it never trusts an upstream check.
/// - Dedupes on [`key_blob`]: if a line with the same blob already exists,
///   returns `AlreadyPresent` and does not write anything (no duplicate
///   line, no permission churn beyond the self-healing chmod below).
/// - Creates the parent directory (`.ssh`) with `0700` and the file itself
///   with `0600` permissions (unix only) -- and re-asserts both on every
///   call that actually writes, so permissions drift self-heals rather than
///   silently persisting.
pub fn authorize(path: &Path, pubkey: &str, label: &str) -> anyhow::Result<AuthorizeOutcome> {
    let validated =
        validate_public_key(pubkey).map_err(|e| anyhow::anyhow!("invalid public key: {e}"))?;
    let new_blob = key_blob(validated)
        .ok_or_else(|| anyhow::anyhow!("validated key unexpectedly had no blob field"))?;

    let existing = read_existing(path)?;
    for line in existing.lines() {
        if key_blob(line) == Some(new_blob) {
            return Ok(AuthorizeOutcome::AlreadyPresent);
        }
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
            set_dir_perms(parent)?;
        }
    }

    let mut file = fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{validated} ttstation:{label}")?;
    drop(file);

    set_file_perms(path)?;

    Ok(AuthorizeOutcome::Added)
}

/// Remove any line(s) from the `authorized_keys` file at `path` that match
/// `which` (by key blob or by `ttstation:<label>` marker). A missing file,
/// or a file with no matching line, is not an error -- revoking something
/// that's already absent is the expected steady state, not a failure.
pub fn revoke(path: &Path, which: &Revoke) -> anyhow::Result<()> {
    let existing = match read_existing(path) {
        Ok(s) => s,
        // `read_existing` already maps "file not found" to an empty string
        // (see below), so this arm is unreachable in practice, but staying
        // defensive costs nothing.
        Err(e) => return Err(e),
    };

    if !path.exists() {
        // Nothing to revoke -- absent is success, not an error.
        return Ok(());
    }

    let kept: Vec<&str> = existing
        .lines()
        .filter(|line| !line_matches(line, which))
        .collect();

    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    fs::write(path, body)?;
    set_file_perms(path)?;

    Ok(())
}

/// Whether a stored `authorized_keys` line matches the given revoke target:
/// either its key blob equals the target blob, or it ends with the exact
/// `ttstation:<label>` marker token that [`authorize`] appends.
fn line_matches(line: &str, which: &Revoke) -> bool {
    match which {
        Revoke::Blob(blob) => key_blob(line) == Some(blob.as_str()),
        Revoke::Label(label) => line.trim_end().ends_with(&format!("ttstation:{label}")),
    }
}

/// Read `path`'s contents, treating "file does not exist" as an empty file
/// (the natural starting state for `authorize`/`revoke` on a box that has
/// never had this feature used) rather than an error.
fn read_existing(path: &Path) -> anyhow::Result<String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

/// Force `dir`'s permissions to `0700` (owner rwx only) -- the standard mode
/// `~/.ssh` is expected to have, and required by `sshd` on many systems to
/// even honor `authorized_keys` inside it. No-op on non-unix targets.
#[cfg(unix)]
fn set_dir_perms(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_dir_perms(_dir: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// Force `path`'s permissions to `0600` (owner rw only) -- `sshd` will
/// refuse to honor `authorized_keys` if it's group/world-readable on many
/// configurations, so this is re-asserted on every write. No-op on non-unix
/// targets.
#[cfg(unix)]
fn set_file_perms(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_file_perms(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ttauthkeys-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("authorized_keys")
    }

    #[test]
    fn accepts_ed25519_and_rejects_private_key() {
        assert!(validate_public_key("ssh-ed25519 AAAAC3Nz... user@host").is_ok());
        assert!(validate_public_key("ssh-rsa AAAAB3Nz... x").is_ok());
        assert!(validate_public_key("-----BEGIN OPENSSH PRIVATE KEY-----").is_err());
        assert!(validate_public_key("not a key").is_err());
        assert!(validate_public_key("ssh-ed25519 AAAA\nssh-ed25519 BBBB").is_err()); // multi-line
        assert!(validate_public_key("").is_err());
    }

    #[test]
    fn authorize_creates_and_is_idempotent() {
        let p = tmp("idem");
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1 alice@mac";
        assert!(matches!(authorize(&p, key, "mac:2026-07-05").unwrap(), AuthorizeOutcome::Added));
        // same key blob again -> AlreadyPresent, no duplicate line
        assert!(matches!(authorize(&p, key, "mac:2026-07-05").unwrap(), AuthorizeOutcome::AlreadyPresent));
        let body = fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches("AAAAC3NzaC1lZDI1").count(), 1);
        assert!(body.contains("ttstation:mac:2026-07-05"));
        // perms 0600 on file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn revoke_removes_only_matching_line() {
        let p = tmp("revoke");
        authorize(&p, "ssh-ed25519 AAAAKEEP keep@mac", "keep").unwrap();
        authorize(&p, "ssh-ed25519 AAAADROP drop@mac", "drop").unwrap();
        revoke(&p, &Revoke::Label("drop".into())).unwrap();
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("AAAAKEEP"));
        assert!(!body.contains("AAAADROP"));
        // revoking absent is ok
        assert!(revoke(&p, &Revoke::Label("nope".into())).is_ok());
    }
}
