// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! How a model download is going: bytes on the box against the full size,
//! how fast they're arriving and how long is left.
//!
//! Sizes on the box are read from the Hugging Face cache. A finished file there
//! can be a link into a shared blob store (`hub/blobs/..`), so links in a
//! repo's `blobs/` are followed; `.incomplete` files count while they're being
//! written, and ones abandoned by an earlier, failed download don't.
//! Full sizes come from the Hub, once per repo.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{json, Value};

/// A partial file untouched for this long belongs to a download that stopped.
const STALE: Duration = Duration::from_secs(10 * 60);
/// Speed is measured over this much recent history. Long, because bytes land in
/// bursts: Xet assembles pieces of files in memory and writes them out in big
/// chunks (seen live: 5 GB at once, then nothing for two minutes on a 38 MB/s link).
const WINDOW: Duration = Duration::from_secs(15 * 60);
/// Less history than this, and there's no speed yet.
const SETTLE: Duration = Duration::from_secs(90);

/// The Hugging Face hub cache, the way huggingface_hub finds it.
pub fn hub_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("HF_HUB_CACHE") {
        return PathBuf::from(p);
    }
    if let Some(p) = std::env::var_os("HF_HOME") {
        return PathBuf::from(p).join("hub");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache/huggingface/hub")
}

/// Bytes of `repo` on the box.
pub fn repo_on_disk(hub: &Path, repo: &str) -> u64 {
    let blobs = hub
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("blobs");
    let now = SystemTime::now();
    std::fs::read_dir(blobs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            // Follows a link into the shared store.
            let m = std::fs::metadata(e.path()).ok().filter(|m| m.is_file())?;
            if e.file_name().to_string_lossy().ends_with(".incomplete") {
                let age = m
                    .modified()
                    .ok()
                    .and_then(|t| now.duration_since(t).ok())
                    .unwrap_or_default();
                if age > STALE {
                    return None;
                }
            }
            Some(written(&m))
        })
        .sum()
}

/// Bytes of the files under `dir` (any depth), following nothing.
pub fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_bytes(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| written(&m)).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// What a file holds: its length, or less when it's preallocated and only partly written.
fn written(m: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    m.len().min(m.blocks() * 512)
}

/// A repo's full size: when it was asked for, and the answer once it's in.
type Total = (Instant, Option<u64>);

static TOTALS: LazyLock<Mutex<HashMap<String, Total>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The full size of `repo` on the Hub, if known yet. The first ask looks it up
/// in the background (on the caller's runtime), so callers never wait on the network.
pub fn repo_total(repo: &str) -> Option<u64> {
    let mut totals = TOTALS.lock().expect("totals");
    match totals.get(repo) {
        Some((_, Some(n))) => return Some(*n),
        // Asked recently and not answered (or not answerable): don't ask again yet.
        Some((at, None)) if at.elapsed() < Duration::from_secs(60) => return None,
        _ => {}
    }
    totals.insert(repo.to_string(), (Instant::now(), None));
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        let repo = repo.to_string();
        rt.spawn(async move {
            let n = fetch_total(&repo).await;
            TOTALS
                .lock()
                .expect("totals")
                .insert(repo, (Instant::now(), n));
        });
    }
    None
}

async fn fetch_total(repo: &str) -> Option<u64> {
    let mut req = reqwest::Client::new()
        .get(format!(
            "https://huggingface.co/api/models/{repo}?blobs=true"
        ))
        .timeout(Duration::from_secs(15));
    if let Some(t) = crate::serving::secrets::hf_token() {
        req = req.bearer_auth(t);
    }
    let v: Value = req.send().await.ok()?.json().await.ok()?;
    let n: u64 = v["siblings"]
        .as_array()?
        .iter()
        .filter_map(|s| s["size"].as_u64())
        .sum();
    (n > 0).then_some(n)
}

/// A download's recent progress: when, and the bytes done by then.
type Samples = VecDeque<(Instant, u64)>;

static SAMPLES: LazyLock<Mutex<HashMap<String, Samples>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Bytes per second over the last few seconds of `key`'s download, given what's done now.
fn speed(key: &str, done: u64) -> Option<f64> {
    let now = Instant::now();
    let mut all = SAMPLES.lock().expect("samples");
    all.retain(|_, s| {
        s.back()
            .is_some_and(|(t, _)| now.duration_since(*t) < Duration::from_secs(1800))
    });
    let s = all.entry(key.to_string()).or_default();
    // Started over (files removed): the history no longer applies.
    if s.back().is_some_and(|(_, b)| *b > done) {
        s.clear();
    }
    s.push_back((now, done));
    if s.len() > 2000 {
        s.pop_front();
    }
    while s.len() > 2
        && s.front()
            .is_some_and(|(t, _)| now.duration_since(*t) > WINDOW)
    {
        s.pop_front();
    }
    let (t0, b0) = *s.front()?;
    let secs = now.duration_since(t0).as_secs_f64();
    (secs >= SETTLE.as_secs_f64()).then(|| (done - b0) as f64 / secs)
}

/// `{done_bytes, total_bytes, percent, bytes_per_s, eta_s}` for a download
/// identified by `key`. `total` is None until it's known; then percent and the
/// time left are too.
pub fn progress(key: &str, done: u64, total: Option<u64>) -> Value {
    let rate = speed(key, done);
    // Never past the end: a cache can hold a little more than the repo lists.
    let done = total.map_or(done, |t| done.min(t));
    let percent = total
        .filter(|t| *t > 0)
        .map(|t| (done as f64 / t as f64 * 1000.0).round() / 10.0);
    let eta = match (total, rate) {
        (Some(t), Some(r)) if r > 64_000.0 => Some(((t - done) as f64 / r).round() as u64),
        _ => None,
    };
    json!({ "done_bytes": done, "total_bytes": total, "percent": percent, "bytes_per_s": rate.map(|r| r.round() as u64), "eta_s": eta })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tt-dl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn counts_linked_blobs_and_live_partials_but_not_stale_ones() {
        let hub = tmp("hub");
        let blobs = hub.join("models--org--m/blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(hub.join("blobs/ab")).unwrap();
        // A finished file kept in the shared store, linked from the repo.
        std::fs::write(hub.join("blobs/ab/abc"), vec![1u8; 3000]).unwrap();
        std::os::unix::fs::symlink("../../blobs/ab/abc", blobs.join("abc")).unwrap();
        // A plain finished file and one being written now.
        std::fs::write(blobs.join("def"), vec![1u8; 2000]).unwrap();
        std::fs::write(blobs.join("ghi.1234.incomplete"), vec![1u8; 500]).unwrap();
        // One left by a download that stopped an hour ago.
        let old = blobs.join("jkl.5678.incomplete");
        std::fs::write(&old, vec![1u8; 9000]).unwrap();
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        assert_eq!(repo_on_disk(&hub, "org/m"), 5500);
        assert_eq!(repo_on_disk(&hub, "org/missing"), 0);
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn progress_has_a_percent_only_with_a_total_and_never_passes_it() {
        let p = progress("t1", 250, Some(1000));
        assert_eq!(p["percent"], 25.0);
        assert_eq!(p["total_bytes"], 1000);
        assert!(p["eta_s"].is_null(), "no speed from a single reading");
        let p = progress("t2", 1200, Some(1000));
        assert_eq!(p["done_bytes"], 1000);
        assert_eq!(p["percent"], 100.0);
        let p = progress("t3", 5, None);
        assert!(p["percent"].is_null() && p["total_bytes"].is_null());
    }
}
