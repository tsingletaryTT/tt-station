//! `SecretStore` — per-box bearer token storage.
//!
//! After pairing with a box, `tt` needs to remember a bearer token per box
//! name so subsequent CLI invocations don't have to re-pair. Two
//! implementations exist:
//!
//! - [`FileStore`]: a JSON map of `box_name -> token` on disk, `0600` on
//!   unix. Compiled and usable everywhere (including this Linux dev/test
//!   environment, and as a deliberate fallback anywhere Keychain isn't
//!   available).
//! - `KeychainStore` (macOS only, see below): stores each token as a
//!   generic-password Keychain item (service `tt-station`, account = box
//!   name). Gated behind `#[cfg(target_os = "macos")]` so the
//!   `security-framework` dependency and its FFI never need to compile on
//!   Linux/CI.
//!
//! [`default_store`] picks the right one per-OS.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Abstraction over "somewhere safe to keep a per-box bearer token".
pub trait SecretStore {
    /// Store (or overwrite) the token for `box_name`.
    fn set(&self, box_name: &str, token: &str) -> Result<()>;
    /// Look up the token for `box_name`, if one has been stored.
    fn get(&self, box_name: &str) -> Result<Option<String>>;
    /// Remove the token for `box_name`, if present. Not an error if absent.
    fn delete(&self, box_name: &str) -> Result<()>;

    /// Remove EVERY stored token at once -- "forget all paired boxes on this
    /// machine", the local half of `tt reset`. Not an error if nothing is
    /// stored (an already-empty store is the desired end state).
    fn clear(&self) -> Result<()>;
}

/// File-backed `SecretStore`: a single JSON file holding a `box_name ->
/// token` map. Used everywhere `KeychainStore` isn't available (Linux, CI,
/// tests), and is the only implementation compiled on non-macOS targets.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// Create a store backed by the JSON file at `path`. The file itself is
    /// not created until the first [`FileStore::set`] call; `path`'s parent
    /// directory is created (if needed) at that point too.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Read the current `box_name -> token` map from disk, treating a
    /// missing file as an empty map.
    fn load(&self) -> Result<HashMap<String, String>> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents)
                .with_context(|| format!("parsing secrets file {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => {
                Err(e).with_context(|| format!("reading secrets file {}", self.path.display()))
            }
        }
    }

    /// Write `map` back to disk as JSON, creating the parent directory if
    /// necessary and restricting permissions to `0600` (owner read/write
    /// only) on unix, since this file holds bearer tokens in cleartext.
    fn save(&self, map: &HashMap<String, String>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating secrets dir {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(map).context("serializing secrets map")?;
        fs::write(&self.path, json)
            .with_context(|| format!("writing secrets file {}", self.path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&self.path, perms)
                .with_context(|| format!("setting permissions on {}", self.path.display()))?;
        }

        Ok(())
    }
}

impl SecretStore for FileStore {
    fn set(&self, box_name: &str, token: &str) -> Result<()> {
        let mut map = self.load()?;
        map.insert(box_name.to_string(), token.to_string());
        self.save(&map)
    }

    fn get(&self, box_name: &str) -> Result<Option<String>> {
        let map = self.load()?;
        Ok(map.get(box_name).cloned())
    }

    fn delete(&self, box_name: &str) -> Result<()> {
        let mut map = self.load()?;
        if map.remove(box_name).is_some() {
            self.save(&map)?;
        }
        Ok(())
    }

    /// Clear every stored token by deleting the whole secrets file. A
    /// missing file is already the desired end state, so `NotFound` is not
    /// an error. Deleting (rather than writing back an empty `{}`) matches a
    /// machine that never paired anything: `load` treats a missing file as
    /// an empty map, so subsequent `get`s just return `None`.
    fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(e).with_context(|| format!("removing secrets file {}", self.path.display()))
            }
        }
    }
}

/// macOS Keychain-backed `SecretStore`. Only compiled on macOS: this is
/// where the `security-framework` (Keychain Services FFI) dependency is
/// actually used, and the crate must not require it — or the `security`
/// framework — to build on Linux/CI.
#[cfg(target_os = "macos")]
pub struct KeychainStore;

#[cfg(target_os = "macos")]
impl KeychainStore {
    /// Service name under which every tt-station token is filed in the
    /// Keychain; the account name (per Keychain item) is the box name.
    const SERVICE: &'static str = "tt-station";
}

#[cfg(target_os = "macos")]
impl SecretStore for KeychainStore {
    fn set(&self, box_name: &str, token: &str) -> Result<()> {
        use security_framework::passwords::set_generic_password;
        set_generic_password(Self::SERVICE, box_name, token.as_bytes())
            .context("writing token to macOS Keychain")
    }

    fn get(&self, box_name: &str) -> Result<Option<String>> {
        use security_framework::base::Error as SfError;
        use security_framework::passwords::get_generic_password;
        match get_generic_password(Self::SERVICE, box_name) {
            Ok(bytes) => {
                let token =
                    String::from_utf8(bytes).context("Keychain token was not valid UTF-8")?;
                Ok(Some(token))
            }
            // errSecItemNotFound (-25300): no such item is not an error for
            // `get` — it just means nothing has been stored yet.
            Err(e) if e.code() == -25300 => Ok(None),
            Err(e) => Err(SfError::from(e)).context("reading token from macOS Keychain"),
        }
    }

    fn delete(&self, box_name: &str) -> Result<()> {
        use security_framework::passwords::delete_generic_password;
        match delete_generic_password(Self::SERVICE, box_name) {
            Ok(()) => Ok(()),
            // Deleting an absent item is not an error for our API.
            Err(e) if e.code() == -25300 => Ok(()),
            Err(e) => Err(e).context("deleting token from macOS Keychain"),
        }
    }

    /// Clear every tt-station token by enumerating this service's
    /// generic-password items and deleting each one -- the Keychain has no
    /// single "delete all for service" call, so `tt reset` has to find the
    /// accounts (box names) first, then delete them one at a time via the
    /// same `delete` path above.
    ///
    /// Enumeration filters strictly to `Self::SERVICE` so this only ever
    /// removes tt-station's own items, never any other app's Keychain
    /// entries. An empty Keychain (`errSecItemNotFound`) is the desired end
    /// state, not an error.
    fn clear(&self) -> Result<()> {
        use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

        let results = match ItemSearchOptions::new()
            .class(ItemClass::generic_password())
            .load_attributes(true)
            .limit(Limit::All)
            .search()
        {
            Ok(results) => results,
            // Nothing stored at all -- already the end state we want.
            Err(e) if e.code() == -25300 => return Ok(()),
            Err(e) => return Err(e).context("enumerating macOS Keychain items"),
        };

        // Collect the accounts (box names) whose service is ours, then
        // delete each via the trait's own `delete` (idempotent, absent-safe).
        for result in results {
            if let SearchResult::Dict(_) = &result {
                if let Some(attrs) = result.simplify_dict() {
                    let is_ours = attrs.get("svce").map(String::as_str) == Some(Self::SERVICE);
                    if is_ours {
                        if let Some(account) = attrs.get("acct") {
                            self.delete(account)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

/// The config directory tt-station's file-backed state (secrets, known
/// boxes, ...) lives under, honoring `$XDG_CONFIG_HOME` when set (Linux
/// convention) and falling back to `~/.config` (also the right answer on
/// macOS when we're using `FileStore` there, e.g. for tests).
fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("tt-station");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".config").join("tt-station")
}

/// Which token store to use. Chosen by [`resolve_store_kind`] from an env
/// override, a persisted marker, and the per-OS default -- see that
/// function's docs for the precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Keychain,
    File,
}

/// Parse a raw env/marker value ("file" / "keychain", case-insensitive,
/// surrounding whitespace ignored) into a [`StoreKind`]. Anything else --
/// including empty/missing -- doesn't match, so the caller falls through to
/// the next-lower-priority source.
fn parse_store_kind(raw: &str) -> Option<StoreKind> {
    match raw.trim().to_lowercase().as_str() {
        "file" => Some(StoreKind::File),
        "keychain" => Some(StoreKind::Keychain),
        _ => None,
    }
}

/// Pure store-kind resolution -- no env/filesystem reads happen here; the
/// caller ([`default_store`]) reads `TT_SECRET_STORE` and the marker file
/// and passes the results in, which is what makes this fully unit-testable.
///
/// Precedence, highest first:
/// 1. `env` (the value of `TT_SECRET_STORE`, if set): `"file"` -> `File`,
///    `"keychain"` -> `Keychain` (case-insensitive, trimmed). Wins over
///    everything else.
/// 2. else `marker` (the contents of the persisted marker file, if any):
///    same `"file"`/`"keychain"` mapping. Wins over the default.
/// 3. else the per-OS default: `Keychain` if `default_is_keychain`, else
///    `File`.
///
/// An unrecognized or empty `env` or `marker` value is *ignored* (falls
/// through to the next source), not treated as an error.
pub fn resolve_store_kind(
    env: Option<&str>,
    marker: Option<&str>,
    default_is_keychain: bool,
) -> StoreKind {
    if let Some(kind) = env.and_then(parse_store_kind) {
        return kind;
    }
    if let Some(kind) = marker.and_then(parse_store_kind) {
        return kind;
    }
    if default_is_keychain {
        StoreKind::Keychain
    } else {
        StoreKind::File
    }
}

/// Path to the persisted "which store to use" marker: a one-line file
/// (contents `file` or `keychain`) under the config dir. Lives alongside
/// `secrets.json` so both the terminal `tt` and the macOS app's
/// subprocess-launched `tt` (which doesn't inherit the launching shell's
/// env vars, but does share `$HOME` via launchd) resolve to the same file
/// and therefore agree on the same store.
fn marker_path() -> PathBuf {
    config_dir().join("secret_store")
}

/// Read and trim the marker file's contents, or `None` if it's absent or
/// unreadable (e.g. permissions) -- either way, [`default_store`] just
/// falls through to the next-lower-priority source.
fn marker_contents() -> Option<String> {
    fs::read_to_string(marker_path())
        .ok()
        .map(|s| s.trim().to_string())
}

/// The `SecretStore` `tt` should use by default: the macOS Keychain on
/// macOS, and a [`FileStore`] under the user's config dir everywhere else --
/// unless overridden by the `TT_SECRET_STORE` env var or a persisted marker
/// file (see [`resolve_store_kind`]), which lets macOS opt into the file
/// store to sidestep repeated Keychain "Always Allow" prompts caused by an
/// ad-hoc-signed binary's code identity changing on every rebuild.
pub fn default_store() -> Box<dyn SecretStore> {
    let default_is_keychain = cfg!(target_os = "macos");
    let env = std::env::var("TT_SECRET_STORE").ok();
    let marker = marker_contents();
    match resolve_store_kind(env.as_deref(), marker.as_deref(), default_is_keychain) {
        StoreKind::File => Box::new(FileStore::new(config_dir().join("secrets.json"))),
        #[cfg(target_os = "macos")]
        StoreKind::Keychain => Box::new(KeychainStore),
        // KeychainStore doesn't compile off-macOS; falling back to FileStore
        // here is a no-op in practice, since `default_is_keychain` is only
        // ever true on macOS, so `resolve_store_kind` can only return
        // `Keychain` on non-macOS if an explicit env/marker override asked
        // for it -- honor the intent as closely as this platform allows.
        #[cfg(not(target_os = "macos"))]
        StoreKind::Keychain => Box::new(FileStore::new(config_dir().join("secrets.json"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A small RAII temp-dir helper so tests never touch the user's real
    /// config directory. Avoids pulling in a `tempfile` dependency for
    /// something this small: a unique subdirectory of `std::env::temp_dir()`
    /// (pid + a per-process counter, so parallel `#[test]` threads don't
    /// collide), removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "tt-station-secrets-test-{}-{}",
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_store_path() -> (TempDir, PathBuf) {
        let dir = TempDir::new();
        let path = dir.path().join("secrets.json");
        (dir, path)
    }

    #[test]
    fn set_then_get_returns_the_token() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        store.set("qb2-it", "tok-123").unwrap();

        assert_eq!(store.get("qb2-it").unwrap(), Some("tok-123".to_string()));
    }

    #[test]
    fn get_on_unknown_box_returns_none() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        assert_eq!(store.get("never-paired").unwrap(), None);
    }

    #[test]
    fn delete_then_get_returns_none() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        store.set("qb2-it", "tok-123").unwrap();
        store.delete("qb2-it").unwrap();

        assert_eq!(store.get("qb2-it").unwrap(), None);
    }

    #[test]
    fn delete_on_unknown_box_is_not_an_error() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        store.delete("never-paired").unwrap();
    }

    #[test]
    fn clear_removes_all_stored_tokens() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        store.set("box-a", "tok-a").unwrap();
        store.set("box-b", "tok-b").unwrap();

        store.clear().unwrap();

        assert_eq!(store.get("box-a").unwrap(), None);
        assert_eq!(store.get("box-b").unwrap(), None);
    }

    #[test]
    fn clear_on_empty_store_is_not_an_error() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        // Never wrote anything -- the file doesn't exist yet.
        store.clear().unwrap();
    }

    #[test]
    fn set_creates_missing_parent_dir() {
        let (dir, _path) = temp_store_path();
        let nested_path = dir.path().join("nested").join("secrets.json");
        let store = FileStore::new(nested_path.clone());

        store.set("qb2-it", "tok-123").unwrap();

        assert!(nested_path.exists());
    }

    #[test]
    fn set_does_not_clobber_other_boxes() {
        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path);

        store.set("box-a", "tok-a").unwrap();
        store.set("box-b", "tok-b").unwrap();

        assert_eq!(store.get("box-a").unwrap(), Some("tok-a".to_string()));
        assert_eq!(store.get("box-b").unwrap(), Some("tok-b".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn file_has_0600_perms_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, path) = temp_store_path();
        let store = FileStore::new(path.clone());

        store.set("qb2-it", "tok-123").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    // -- resolve_store_kind: pure selector, no I/O -----------------------

    #[test]
    fn env_file_selects_file() {
        assert_eq!(
            resolve_store_kind(Some("file"), None, true),
            StoreKind::File
        );
    }

    #[test]
    fn env_file_is_case_and_whitespace_insensitive() {
        assert_eq!(
            resolve_store_kind(Some("FILE "), None, true),
            StoreKind::File
        );
        assert_eq!(
            resolve_store_kind(Some(" file"), None, true),
            StoreKind::File
        );
    }

    #[test]
    fn env_keychain_selects_keychain() {
        assert_eq!(
            resolve_store_kind(Some("keychain"), None, false),
            StoreKind::Keychain
        );
    }

    #[test]
    fn env_wins_over_conflicting_marker() {
        assert_eq!(
            resolve_store_kind(Some("keychain"), Some("file"), false),
            StoreKind::Keychain
        );
    }

    #[test]
    fn no_env_marker_file_selects_file() {
        assert_eq!(
            resolve_store_kind(None, Some("file"), true),
            StoreKind::File
        );
    }

    #[test]
    fn no_env_marker_keychain_selects_keychain() {
        assert_eq!(
            resolve_store_kind(None, Some("keychain"), false),
            StoreKind::Keychain
        );
    }

    #[test]
    fn neither_env_nor_marker_falls_back_to_default_keychain() {
        assert_eq!(resolve_store_kind(None, None, true), StoreKind::Keychain);
    }

    #[test]
    fn neither_env_nor_marker_falls_back_to_default_file() {
        assert_eq!(resolve_store_kind(None, None, false), StoreKind::File);
    }

    #[test]
    fn unrecognized_env_is_ignored_falls_through_to_marker() {
        assert_eq!(
            resolve_store_kind(Some("bogus"), Some("file"), true),
            StoreKind::File
        );
    }

    #[test]
    fn unrecognized_env_and_marker_falls_through_to_default() {
        assert_eq!(
            resolve_store_kind(Some("bogus"), Some("also-bogus"), true),
            StoreKind::Keychain
        );
        assert_eq!(
            resolve_store_kind(Some("bogus"), Some("also-bogus"), false),
            StoreKind::File
        );
    }
}
