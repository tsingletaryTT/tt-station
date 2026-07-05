//! Fetch + 24h cache for the public Tenstorrent model-compatibility catalog
//! (`compatibility.json`).
//!
//! `libttstation::catalog::CompatCatalog` (parsing/mapping) is deliberately
//! I/O-free -- see that module's doc. This module is the I/O layer on top:
//! get the JSON from the network, cache it on disk for [`TTL`] seconds, and
//! degrade gracefully when the network is unavailable. `tt catalog` (a later
//! task) is the only caller; this module has no CLI surface of its own.
//!
//! ## Degradation contract
//!
//! [`load_catalog`] never panics and never returns an `Err` -- every failure
//! mode collapses into one of two outcomes:
//! - `(Some(catalog), stale)`: a catalog is available. `stale == true` means
//!   it came from a cache the [`TTL`] has already passed (a failed fetch
//!   fell back to whatever was last successfully cached, however old).
//! - `(None, false)`: no catalog is available at all (never fetched
//!   successfully, and there's no cache to fall back to). `stale` is always
//!   `false` here -- there's no data to call stale.
//!
//! This mirrors `libttstation::catalog::classify`'s own `catalog: Option<&_>`
//! + `catalog_stale: bool` contract: a caller that can't reach the network
//! (or is offline) still gets a usable classification, just flagged as
//! degraded rather than failing the whole command.
//!
//! Any JSON parse error -- whether on a freshly fetched body or on a cache
//! file already on disk -- makes that ONE source (fetch, or cache) count as
//! "unavailable" and falls through to the next fallback in the chain,
//! exactly like a network failure would. A byte-for-bit-flipped cache file
//! left behind by a previous crashed write, for instance, is treated the
//! same as no cache file at all.

// This module's only caller is the not-yet-written `tt catalog` command
// (Task 4). Until that lands and wires `load_catalog`/`is_fresh` into
// `main.rs`, every item here is legitimately dead code from `rustc`'s point
// of view -- silence the warning at the module level rather than sprinkling
// `#[allow(dead_code)]` over each function, and remove this once Task 4
// adds the caller.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libttstation::catalog::CompatCatalog;

/// The public catalog's well-known URL, served from Tenstorrent's CDN.
const URL: &str = "https://d1oi7xemha0dsy.cloudfront.net/data/compatibility.json";

/// How long a cached copy is considered fresh, in seconds (24h). Chosen to
/// match how often the upstream catalog realistically changes (new model
/// support lands on the order of days/weeks, not minutes) while still
/// picking up updates within a day of normal use -- an operator running
/// `tt catalog` daily always sees data at most a day stale without ever
/// paying for a network round-trip on every single invocation.
const TTL: u64 = 86400;

/// How long a single fetch attempt is allowed to hang before giving up and
/// falling back to whatever's cached (see [`load_catalog`]). Mirrors the
/// bounded-timeout pattern `main.rs`'s `build_probe_client` already uses for
/// `tt discover`'s manual-host probe -- a `reqwest::blocking::get` with no
/// timeout configured can hang far longer than any interactive CLI command
/// should tolerate.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Pure freshness check: is a file with mtime `mtime_secs` still within
/// `ttl_secs` of `now_secs`? Split out from [`load_catalog`] specifically so
/// it's unit-testable without touching the filesystem or clock -- every
/// input is a plain `u64`.
///
/// A `now_secs < mtime_secs` (a file mtime in the future, e.g. from a clock
/// that got wound back, or a cache file with a bogus/manipulated mtime) is
/// deliberately treated as NOT fresh rather than doing an unsigned
/// subtraction that would underflow/wrap -- there's no sensible "how long
/// until this expires" answer for a file the clock claims doesn't exist yet,
/// so the safe conservative choice is "treat it as stale, go refetch."
pub fn is_fresh(mtime_secs: u64, now_secs: u64, ttl_secs: u64) -> bool {
    now_secs >= mtime_secs && now_secs - mtime_secs < ttl_secs
}

/// Where the cached `compatibility.json` lives on disk.
///
/// Honors `$TT_CONFIG_DIR` when set -- the same test-isolation knob
/// `main.rs::build_store` already uses for `secrets.json` (see its doc) --
/// so a test that sets `TT_CONFIG_DIR` to a temp dir never touches this
/// operator's real cache, and a single env var isolates ALL of this CLI's
/// on-disk state for a test run, not just the secret store. Outside of
/// tests, falls back to `$HOME/.cache/tt-station/compatibility.json` -- the
/// XDG-ish cache location `libttstation::secrets`'s `config_dir` doesn't
/// cover (that's `~/.config`, for state that should survive `rm -rf
/// ~/.cache`; a re-fetchable 24h cache is exactly the opposite kind of
/// data).
fn cache_path() -> PathBuf {
    if let Ok(dir) = std::env::var("TT_CONFIG_DIR") {
        return PathBuf::from(dir).join("compatibility.json");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".cache")
        .join("tt-station")
        .join("compatibility.json")
}

/// Read+parse a catalog JSON file at `path`. `None` on ANY failure (missing
/// file, unreadable, invalid JSON) -- callers never need to distinguish
/// "file absent" from "file present but garbage"; both mean "this source
/// doesn't have a usable catalog right now" per this module's degradation
/// contract (see the module doc).
fn parse_catalog_file(path: &Path) -> Option<CompatCatalog> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A file's modification time, in whole seconds since the Unix epoch.
/// `None` if the file doesn't exist, its metadata/mtime can't be read (rare,
/// platform-dependent), or -- vanishingly unlikely, but `duration_since` can
/// fail -- the mtime somehow predates `UNIX_EPOCH`.
fn file_mtime_secs(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// The current wall-clock time, in whole seconds since the Unix epoch.
/// Falls back to `0` if the clock is somehow before the epoch (never
/// happens in practice) rather than panicking -- worst case this makes
/// [`is_fresh`] see an absurdly small `now_secs` and correctly decide
/// nothing is fresh, which just means "go fetch," never a crash.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Fetch `compatibility.json`'s raw body over the network. Returns the raw
/// `String` (not the parsed [`CompatCatalog`]) so the caller can write the
/// EXACT bytes served to the cache file -- `CompatCatalog`/`CompatModel`
/// only derive `Deserialize` (see `libttstation::catalog`'s doc: it's
/// deliberately I/O-free and one-directional, parse-only), so there is no
/// `Serialize` impl to round-trip through even if we wanted to re-emit the
/// parsed structure instead of the original response.
///
/// `None` on any failure: client-build failure, connection/timeout error, a
/// non-2xx status, or a body read error. `load_catalog` doesn't need to
/// distinguish which -- every case means "no fresh data this attempt, fall
/// back to the cache."
fn fetch_remote() -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_TIMEOUT)
        .build()
        .ok()?;
    let resp = client.get(URL).send().ok()?.error_for_status().ok()?;
    resp.text().ok()
}

/// Best-effort cache write: create `path`'s parent directory (`~/.cache/
/// tt-station/`, typically absent on a fresh install) if needed, then write
/// `body` to `path`. Failures (permissions, disk full, read-only fs, ...)
/// are swallowed rather than propagated -- a fetch that succeeded should
/// still hand the caller a usable, freshly-fetched catalog even if this
/// machine can't persist it for next time; the only cost of a failed write
/// is re-fetching again next call instead of hitting a warm cache.
fn write_cache(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = fs::write(path, body);
}

/// Get the compatibility catalog, offline-tolerant, per this module's
/// degradation contract (see the module doc for the full `(Option<_>,
/// stale)` contract).
///
/// - `file_override`, when given, wins outright: read+parse exactly that
///   file, ignore the network and the on-disk cache entirely, `stale`
///   always `false` (there's no "cache age" concept for a caller-supplied
///   fixture). This is the fast path `tt catalog --catalog-file <path>`
///   (Task 4) and its e2e test are meant to exercise instead of a real
///   network call.
/// - Otherwise: if `!refresh` and the cache file exists and is fresh (see
///   [`is_fresh`]/[`TTL`]), parse and return it straight from disk -- no
///   network round-trip on the common case (an operator running `tt
///   catalog` more than once within the TTL window).
/// - Otherwise (no fresh cache, `refresh` requested, or the fresh cache
///   failed to parse): fetch from [`URL`]. On success, write the cache and
///   return the freshly parsed catalog (`stale = false`). On failure (or a
///   fetched body that fails to parse), fall back to whatever's cached on
///   disk -- even if stale -- and return it with `stale = true`. If there's
///   no cache to fall back to either, `(None, false)`.
pub fn load_catalog(
    refresh: bool,
    file_override: Option<&Path>,
) -> (Option<CompatCatalog>, bool) {
    if let Some(path) = file_override {
        return (parse_catalog_file(path), false);
    }

    let cache = cache_path();

    // Fast path: an existing, fresh, PARSEABLE cache satisfies the request
    // without touching the network at all. A cache that's fresh by mtime
    // but fails to parse (corrupt/truncated) deliberately falls through to
    // the fetch below rather than returning `(None, false)` here -- per the
    // module doc, a parse failure just makes THIS source unavailable, it
    // doesn't end the whole lookup.
    if !refresh {
        if let Some(mtime) = file_mtime_secs(&cache) {
            if is_fresh(mtime, now_secs(), TTL) {
                if let Some(catalog) = parse_catalog_file(&cache) {
                    return (Some(catalog), false);
                }
            }
        }
    }

    // No fresh usable cache (or a refresh was requested) -- try the
    // network. A successful fetch that fails to PARSE is treated exactly
    // like a failed fetch (see `fetch_remote`'s doc: this module doesn't
    // distinguish "why" the network attempt didn't produce usable data) and
    // falls through to the same stale-cache fallback below.
    if let Some(body) = fetch_remote() {
        if let Ok(catalog) = serde_json::from_str::<CompatCatalog>(&body) {
            write_cache(&cache, &body);
            return (Some(catalog), false);
        }
    }

    // Fetch unavailable (or unparseable): fall back to whatever's on disk,
    // however stale -- offline tolerance is the whole point of this cache.
    match parse_catalog_file(&cache) {
        Some(catalog) => (Some(catalog), true),
        None => (None, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_ttl() {
        assert!(is_fresh(1000, 1000 + 86399, 86400)); // within TTL
        assert!(!is_fresh(1000, 1000 + 86401, 86400)); // expired
        assert!(!is_fresh(2000, 1000, 86400)); // future mtime -> treat as not fresh
    }

    /// Boundary check: exactly `ttl_secs` old is expired, not fresh --
    /// `is_fresh` uses `<`, not `<=`, so a cache written exactly 24h ago
    /// (to the second) must be treated as due for a refetch.
    #[test]
    fn freshness_exact_ttl_boundary_is_expired() {
        assert!(!is_fresh(1000, 1000 + 86400, 86400));
    }

    /// `now_secs == mtime_secs` (a file that was just written this instant)
    /// is the freshest possible cache and must be fresh.
    #[test]
    fn freshness_zero_age_is_fresh() {
        assert!(is_fresh(1000, 1000, 86400));
    }

    const SAMPLE_CATALOG_JSON: &str = r#"{"models":[
        {"id":"qwen3-8b","display_name":"Qwen3-8B","family":"Qwen","tasks":[],
         "compatibility":[{"hardware":"Quietbox 2","chip_set":"Blackhole",
         "hardware_family":"Quietbox","status":"Supported",
         "software":["tt-inference-server"]}]}
    ]}"#;

    /// `file_override` is the fixture fast path Task 4's e2e test drives
    /// (no network, no real cache dir touched): a valid file parses and
    /// comes back non-stale.
    #[test]
    fn load_catalog_file_override_parses_and_is_never_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("compatibility.json");
        std::fs::write(&path, SAMPLE_CATALOG_JSON).unwrap();

        let (catalog, stale) = load_catalog(false, Some(&path));
        let catalog = catalog.expect("valid fixture file must parse");
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "qwen3-8b");
        assert!(!stale);
    }

    /// `file_override` pointing at a missing/invalid file degrades to
    /// `(None, false)` -- never panics, never propagates an `Err` (there's
    /// no `Result` in this function's signature at all).
    #[test]
    fn load_catalog_file_override_missing_file_returns_none_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");

        let (catalog, stale) = load_catalog(false, Some(&path));
        assert!(catalog.is_none());
        assert!(!stale);
    }

    /// `file_override` pointing at a syntactically-invalid JSON file is a
    /// parse-error source -- same "(None, false), not a panic" outcome as
    /// a missing file (see the module doc: any parse error makes that
    /// source count as unavailable).
    #[test]
    fn load_catalog_file_override_invalid_json_returns_none_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.json");
        std::fs::write(&path, "not valid json").unwrap();

        let (catalog, stale) = load_catalog(false, Some(&path));
        assert!(catalog.is_none());
        assert!(!stale);
    }

    /// `cache_path()` honors `$TT_CONFIG_DIR` (this module's test-isolation
    /// knob, matching `main.rs::build_store`'s convention for
    /// `secrets.json`) -- pointing it at a temp dir must never touch this
    /// operator's real `~/.cache/tt-station/`.
    #[test]
    fn cache_path_honors_tt_config_dir() {
        // Guard against parallel test interference by holding this test's
        // env mutation to itself; Rust test binaries run in threads within
        // one process, so this is a best-effort check of the *shape* of the
        // path (join behavior), not a race-free assertion about the global
        // env var. Set + immediately read within the same expression window.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TT_CONFIG_DIR", dir.path());
        let path = cache_path();
        std::env::remove_var("TT_CONFIG_DIR");

        assert_eq!(path, dir.path().join("compatibility.json"));
    }

    /// Owner-verification smoke test for the real network path: a genuine
    /// `reqwest::blocking::get(URL)` against the live CDN, isolated to a
    /// throwaway `TT_CONFIG_DIR` cache dir so it never touches this
    /// machine's real `~/.cache/tt-station/`. `#[ignore]`d so `cargo test`
    /// (and CI) never depends on network access -- run explicitly with
    /// `cargo test -p tt catalog::tests::manual_live_fetch_smoke -- --ignored
    /// --nocapture` to confirm the real endpoint is reachable and parses.
    /// This is the one piece of this module Task 3's brief calls out as
    /// "owner-verified" rather than unit-testable in CI (see the task
    /// report for the run this went through).
    #[test]
    #[ignore]
    fn manual_live_fetch_smoke() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("TT_CONFIG_DIR", dir.path());
        let (catalog, stale) = load_catalog(true, None); // force a real fetch
        std::env::remove_var("TT_CONFIG_DIR");

        let catalog = catalog.expect("live fetch against the real CDN should succeed");
        assert!(!stale, "a successful fresh fetch is never stale");
        assert!(
            !catalog.models.is_empty(),
            "the real compatibility.json should list at least one model"
        );
        eprintln!("fetched {} models from {URL}", catalog.models.len());

        // The fetch should also have populated the cache file.
        assert!(dir.path().join("compatibility.json").exists());
    }
}
