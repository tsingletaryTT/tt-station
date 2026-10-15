// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Discover: what's new in the Tenstorrent ecosystem, from tt-awesome's feed
//! (`docs.tenstorrent.com/tt-awesome/feeds/feed.json`, JSON Feed 1.1): new
//! releases, new projects, and articles, each with a short written summary.
//!
//! The agent fetches it (so the apps never load remote pages), keeps the last
//! good copy on disk (so it still shows offline), and adds what only the box
//! knows: which releases are for tools installed here, and whether the
//! installed version is behind.
//!
//! Only plain text leaves here: the feed's HTML is never passed to the apps.
//!
//! - `GET /discover`: `{ items, updated, stale, source }`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth};

const FEED: &str = "https://docs.tenstorrent.com/tt-awesome/feeds/feed.json";
const SITE: &str = "https://docs.tenstorrent.com/tt-awesome/";
/// How long a fetched feed is fresh (it's rebuilt a few times a day).
const FRESH: Duration = Duration::from_secs(30 * 60);

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root"))
}

fn cache_file(state: &AppState) -> PathBuf {
    state.config_dir().join("discover/feed.json")
}

/// `owner/repo` and the tag, from a GitHub release URL.
fn release_of(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(5, '/');
    let (owner, repo, kind, tag_word, tag) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (kind == "releases" && tag_word == "tag").then(|| (format!("{owner}/{repo}"), tag.replace("%2B", "+")))
}

/// `v6.7.0`, `ttkmd-2.11.0`, `6.1.0` → [6, 7, 0]. None if it isn't a version.
fn version_parts(v: &str) -> Option<Vec<u64>> {
    let start = v.find(|c: char| c.is_ascii_digit())?;
    let core: String = v[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let parts: Vec<u64> = core.split('.').filter(|p| !p.is_empty()).map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// Is `installed` older than `latest`? None when either isn't a version.
fn behind(installed: &str, latest: &str) -> Option<bool> {
    let (mut a, mut b) = (version_parts(installed)?, version_parts(latest)?);
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    Some(a < b)
}

/// A pip package's version in the tt tools' environment, from its metadata folder.
fn pip_version(package: &str) -> Option<String> {
    let lib = home().join(".tenstorrent-venv/lib");
    let want = format!("{}-", package.replace('-', "_"));
    for py in std::fs::read_dir(lib).ok()?.flatten() {
        for e in std::fs::read_dir(py.path().join("site-packages")).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(rest) = name.strip_prefix(&want).and_then(|r| r.strip_suffix(".dist-info")) {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn command_version(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().find(|w| version_parts(w).is_some()).map(str::to_string)
}

/// The versions of Tenstorrent tools on this box, by GitHub repo.
fn installed_versions() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (repo, pkg) in [("tenstorrent/tt-smi", "tt-smi"), ("tenstorrent/tt-flash", "tt-flash"), ("tenstorrent/tt-umd", "tt-umd"), ("tenstorrent/tt-tools-common", "tt-tools-common")] {
        if let Some(v) = pip_version(pkg) {
            out.push((repo, v));
        }
    }
    if let Ok(v) = std::fs::read_to_string("/sys/module/tenstorrent/version") {
        out.push(("tenstorrent/tt-kmd", v.trim().to_string()));
    }
    if let Some(v) = command_version("tt-toplike", &["--version"]) {
        out.push(("tenstorrent/tt-toplike", v));
    }
    let tt = home().join(".tenstorrent-venv/bin/tt");
    if let Some(v) = tt.to_str().and_then(|t| command_version(t, &["--version"])) {
        out.push(("tenstorrent/tt-cli", v));
    }
    // The tt CLI's managed tt-inference-server checkouts: the newest one.
    let newest = std::fs::read_dir(home().join(".local/share/tenstorrent/tools"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("tt-inference-server-").map(str::to_string))
        .max_by(|a, b| version_parts(a).cmp(&version_parts(b)));
    if let Some(v) = newest {
        out.push(("tenstorrent/tt-inference-server", v));
    }
    out
}

/// Markdown links `[text](url)` → `text` (links open from the item, not inline).
fn unlink(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find("](") else { break };
        let Some(end) = after[close + 2..].find(')') else { break };
        out.push_str(&rest[..open]);
        out.push_str(&after[..close]);
        rest = &after[close + 2 + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Plain text with the feed's light Markdown (`code`, **bold**) left as is,
/// whitespace tidied, at most `n` characters.
fn tidy(s: &str, n: usize) -> String {
    let flat = unlink(s).split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        flat
    } else {
        let cut: String = flat.chars().take(n).collect();
        format!("{}…", cut.rsplit_once(' ').map(|(a, _)| a).unwrap_or(&cut))
    }
}

/// Feed items, as the apps show them.
fn shape(feed: &Value, versions: &[(&str, String)]) -> Vec<Value> {
    let mut items = Vec::new();
    for i in feed["items"].as_array().into_iter().flatten() {
        let url = i["url"].as_str().unwrap_or("").to_string();
        if !url.starts_with("https://") {
            continue; // links open outside the app: https only
        }
        let tags: Vec<String> = i["tags"].as_array().into_iter().flatten().filter_map(|t| t.as_str().map(str::to_string)).collect();
        let kind = ["release", "entry", "article"].into_iter().find(|k| tags.iter().any(|t| t == k)).unwrap_or("entry");
        let release = (kind == "release").then(|| release_of(&url)).flatten();
        let mine = release.as_ref().and_then(|(repo, tag)| {
            versions.iter().find(|(r, _)| r.eq_ignore_ascii_case(repo)).map(|(_, v)| (v.clone(), behind(v, tag)))
        });
        let topics: Vec<&String> = tags
            .iter()
            .filter(|t| !matches!(t.as_str(), "release" | "entry" | "article" | "official" | "community" | "affiliated"))
            .collect();
        items.push(json!({
            "id": i["id"], "url": url, "kind": kind,
            "title": tidy(i["title"].as_str().unwrap_or(""), 140),
            "summary": tidy(i["summary"].as_str().unwrap_or(""), 600),
            "date": i["date_published"],
            "official": tags.iter().any(|t| t == "official"),
            "topics": topics,
            "project": release.as_ref().map(|(repo, _)| repo.clone()),
            "version": release.as_ref().map(|(_, tag)| tag.clone()),
            // A release for a tool on this box: what's installed, and whether it's behind.
            "installed": mine.as_ref().map(|(v, _)| v.clone()),
            "update": mine.as_ref().and_then(|(_, b)| *b),
        }));
    }
    items
}

static CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

/// `GET /discover`.
pub(crate) async fn get_discover(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    if let Some((at, v)) = CACHE.lock().expect("discover").clone() {
        if at.elapsed() < FRESH {
            return Json(v);
        }
    }
    let fetched: Option<Value> = match reqwest::Client::new().get(FEED).timeout(Duration::from_secs(20)).send().await {
        Ok(r) if r.status().is_success() => r.json().await.ok(),
        _ => None,
    };
    let file = cache_file(&state);
    let (feed, stale) = match fetched {
        Some(f) if f["items"].is_array() => {
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(&file));
            let _ = std::fs::write(&file, serde_json::to_vec(&f).unwrap_or_default());
            (Some(f), false)
        }
        // Offline (or the feed is down): the last good copy, marked as such.
        _ => (std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()), true),
    };
    let updated = std::fs::metadata(&file).and_then(|m| m.modified()).ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    let versions = tokio::task::spawn_blocking(installed_versions).await.unwrap_or_default();
    let v = json!({
        "items": feed.as_ref().map(|f| shape(f, &versions)).unwrap_or_default(),
        "updated": updated, "stale": stale, "available": feed.is_some(),
        "source": SITE,
    });
    if !stale {
        *CACHE.lock().expect("discover") = Some((Instant::now(), v.clone()));
    }
    Json(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_release_urls() {
        assert_eq!(
            release_of("https://github.com/tenstorrent/tt-smi/releases/tag/v6.7.0"),
            Some(("tenstorrent/tt-smi".into(), "v6.7.0".into()))
        );
        assert_eq!(release_of("https://github.com/tenstorrent/tt-operator"), None);
    }

    #[test]
    fn compares_versions() {
        assert_eq!(behind("6.1.0", "v6.7.0"), Some(true));
        assert_eq!(behind("2.11.0", "ttkmd-2.11.0"), Some(false));
        assert_eq!(behind("v0.18.0", "v0.24.0"), Some(true));
        assert_eq!(behind("0.13.10", "v0.13.5"), Some(false));
        assert_eq!(behind("1.0", "1.0.1"), Some(true));
        assert_eq!(behind("x", "v1"), None);
    }

    #[test]
    fn shapes_items_without_html() {
        let feed = json!({"items": [
            {"id": "a", "url": "https://github.com/tenstorrent/tt-smi/releases/tag/v6.7.0", "title": "tt-smi v6.7.0",
             "content_html": "<script>x</script>", "summary": "Faster   telemetry.", "date_published": "2026-10-05T00:00:00Z",
             "tags": ["dev-tools", "official", "release"]},
            {"id": "b", "url": "javascript:alert(1)", "title": "bad", "summary": "", "tags": ["entry"]},
        ]});
        let items = shape(&feed, &[("tenstorrent/tt-smi", "6.1.0".into())]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["summary"], "Faster telemetry.");
        assert_eq!(items[0]["update"], true);
        assert_eq!(items[0]["installed"], "6.1.0");
        assert!(items[0].get("content_html").is_none());
        assert_eq!(unlink("usage ([#32](https://github.com/x/y/pull/32)) now"), "usage (#32) now");
    }
}
