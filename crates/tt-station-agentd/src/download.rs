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

/// Files this size or larger in a snapshot stay links: they're weights.
const SMALL: u64 = 64 * 1024 * 1024;

/// Replaces the links in `repo`'s snapshots that point at small files (tokenizer,
/// config, chat template) with real copies of them; weights stay links. Some
/// model servers verify their tokenizer files and refuse a link (sjettTT's Flash
/// Next: "tokenizer artifact must be a non-symlink regular file"), and every file
/// in a Hugging Face cache snapshot is one. A copy reads the same to everything
/// else.
///
/// Also turns the repo's own blobs that are links into a shared blob store
/// (`hub/blobs/..`) into hard links of those files: the same data, no extra
/// space, but one link from a snapshot instead of two, which is the standard
/// layout. Flash Next's server opens weights with one link allowed and failed
/// on the second ("Too many levels of symbolic links").
///
/// Returns how many files it changed.
pub fn unlink_small_files(hub: &Path, repo: &str) -> usize {
    let repo_dir = hub.join(format!("models--{}", repo.replace('/', "--")));
    let snapshots = repo_dir.join("snapshots");
    let mut copied = 0;
    for e in std::fs::read_dir(repo_dir.join("blobs"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = e.path();
        if !e.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(&path) else {
            continue;
        };
        if !std::fs::metadata(&target).is_ok_and(|m| m.is_file()) {
            continue;
        }
        // Linked beside it, then swapped in: never a moment without the file.
        let tmp = path.with_file_name(format!(".{}.tt-link", e.file_name().to_string_lossy()));
        let _ = std::fs::remove_file(&tmp);
        if std::fs::hard_link(&target, &tmp).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
            copied += 1;
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&snapshots)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = e.path();
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                dirs.push(path);
                continue;
            }
            if !kind.is_symlink() {
                continue;
            }
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue;
            };
            if !std::fs::metadata(&target).is_ok_and(|m| m.is_file() && m.len() < SMALL) {
                continue;
            }
            // Copy beside it, then swap it in: never a moment without the file.
            let tmp = path.with_file_name(format!(".{}.tt-copy", e.file_name().to_string_lossy()));
            if std::fs::copy(&target, &tmp).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
                copied += 1;
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
    copied
}

static TOTALS: LazyLock<Mutex<HashMap<String, (Instant, Option<u64>)>>> =
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

/// Records a size learned elsewhere (a package's details).
pub fn know_total(repo: &str, bytes: u64) {
    TOTALS
        .lock()
        .expect("totals")
        .insert(repo.to_string(), (Instant::now(), Some(bytes)));
}

async fn fetch_total(repo: &str) -> Option<u64> {
    let mut req = reqwest::Client::new()
        .get(format!(
            "https://huggingface.co/api/models/{repo}?blobs=true"
        ))
        .timeout(Duration::from_secs(15));
    if let Some(t) = crate::bringup::hf_token() {
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

static SAMPLES: LazyLock<Mutex<HashMap<String, VecDeque<(Instant, u64)>>>> =
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

/// Refuse a download that can't fit before it starts, rather than after it has
/// filled the disk (and everything else on it): `need` bytes still to fetch
/// against `free` where they'll land.
pub fn check_room(what: &str, need: u64, free: u64) -> Result<(), String> {
    if need <= free {
        return Ok(());
    }
    let gb = |b: u64| b as f64 / 1e9;
    Err(format!(
        "{what} needs about {:.1} GB of free disk to download; the box has {:.1} GB free. Free up {:.1} GB (remove models you don't use) and try again.",
        gb(need),
        gb(free),
        gb(need - free)
    ))
}

/// What a community package download still needs on disk, from its details
/// (`community::info`): the package, plus its weights less `weights_here`
/// already in the cache (a resumed download), unless they're all here. `None`
/// when the sizes aren't known (then nothing is refused).
pub fn package_need(info: &Value, weights_here: u64) -> Option<u64> {
    let package = info["package_bytes"].as_u64()?;
    if info["weights_on_box"].as_bool() == Some(true) || info["weights"].is_null() {
        return Some(package);
    }
    let weights = info["weights_bytes"].as_u64()?;
    Some(package + weights.saturating_sub(weights_here))
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
    fn copies_small_linked_files_and_leaves_weights_linked() {
        let hub = tmp("unlink");
        let repo = hub.join("models--org--m");
        let snap = repo.join("snapshots/rev1");
        std::fs::create_dir_all(snap.join("sub")).unwrap();
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        std::fs::write(repo.join("blobs/tok"), b"{\"tokenizer\": 1}").unwrap();
        std::fs::write(repo.join("blobs/cfg"), b"{}").unwrap();
        let big = std::fs::File::create(repo.join("blobs/weights")).unwrap();
        big.set_len(SMALL + 1).unwrap(); // sparse: no real disk used
        std::os::unix::fs::symlink("../../blobs/tok", snap.join("tokenizer.json")).unwrap();
        std::os::unix::fs::symlink("../../../blobs/cfg", snap.join("sub/config.json")).unwrap();
        std::os::unix::fs::symlink("../../blobs/weights", snap.join("model.safetensors")).unwrap();
        // A blob kept in the shared store, linked from the repo's blobs.
        std::fs::create_dir_all(hub.join("blobs/ab")).unwrap();
        std::fs::File::create(hub.join("blobs/ab/shard"))
            .unwrap()
            .set_len(SMALL + 1)
            .unwrap(); // sparse
        std::os::unix::fs::symlink("../../blobs/ab/shard", repo.join("blobs/shard")).unwrap();
        std::os::unix::fs::symlink("../../blobs/shard", snap.join("model-00001.safetensors"))
            .unwrap();
        assert_eq!(unlink_small_files(&hub, "org/m"), 3); // the shard's link, then the two small snapshot files
        use std::os::unix::fs::MetadataExt;
        let shard = std::fs::symlink_metadata(repo.join("blobs/shard")).unwrap();
        assert!(shard.is_file(), "one link from the snapshot, not two");
        assert_eq!(
            shard.ino(),
            std::fs::metadata(hub.join("blobs/ab/shard")).unwrap().ino(),
            "the same file, no copy"
        );
        // The snapshot's link to a weight stays a link; it reads the weights.
        assert!(
            std::fs::symlink_metadata(snap.join("model-00001.safetensors"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(snap.join("model-00001.safetensors"))
                .unwrap()
                .len(),
            SMALL + 1
        );
        let tok = std::fs::symlink_metadata(snap.join("tokenizer.json")).unwrap();
        assert!(tok.is_file() && !tok.file_type().is_symlink());
        assert_eq!(
            std::fs::read(snap.join("tokenizer.json")).unwrap(),
            b"{\"tokenizer\": 1}"
        );
        assert!(std::fs::symlink_metadata(snap.join("sub/config.json"))
            .unwrap()
            .is_file());
        assert!(std::fs::symlink_metadata(snap.join("model.safetensors"))
            .unwrap()
            .file_type()
            .is_symlink());
        // Nothing left to do the second time; a missing repo is fine.
        assert_eq!(unlink_small_files(&hub, "org/m"), 0);
        assert_eq!(unlink_small_files(&hub, "org/missing"), 0);
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn a_download_that_cant_fit_is_refused_with_the_numbers() {
        assert!(check_room("Qwen3-32B", 60_000_000_000, 61_000_000_000).is_ok());
        let e = check_room("Qwen3-32B", 65_000_000_000, 20_000_000_000).unwrap_err();
        assert!(e.contains("65.0 GB"), "{e}");
        assert!(e.contains("20.0 GB free"), "{e}");
        assert!(e.contains("Free up 45.0 GB"), "{e}");
    }

    #[test]
    fn a_package_needs_its_size_plus_the_weights_not_yet_here() {
        let info = json!({ "package_bytes": 10, "weights": "org/w", "weights_bytes": 100, "weights_on_box": false });
        assert_eq!(package_need(&info, 0), Some(110));
        assert_eq!(package_need(&info, 40), Some(70), "a resumed download");
        let here = json!({ "package_bytes": 10, "weights": "org/w", "weights_bytes": 100, "weights_on_box": true });
        assert_eq!(package_need(&here, 0), Some(10));
        let unknown = json!({ "package_bytes": 10, "weights": "org/w", "weights_bytes": null });
        assert_eq!(package_need(&unknown, 0), None);
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
