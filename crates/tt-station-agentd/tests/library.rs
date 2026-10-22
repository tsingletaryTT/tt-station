// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The Library (`src/library.rs`) through the API: which folders may be
//! added (inside home, not hidden, not overlapping another), indexing them
//! into the document store and searching what they say, keeping current on
//! a reindex (new, changed and deleted files), a folder that went away, and
//! removing one.
//!
//! `HOME` is a scratch folder for this whole test binary. The Library keeps
//! its folder list in a process-wide cache, so each test uses its own
//! folders (no name a prefix of another's). Search blends in meaning vectors
//! when the box's embedding service answers; the assertions here hold
//! either way.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tt_station_agentd::docs::DocStore;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::NullBackend;

mod support;

fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-library-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir
    })
}

/// `files` written under `~/<name>`.
fn folder(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let root = home().join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    root
}

struct Box_ {
    base: String,
    token: String,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(tag: &str) -> Self {
        home();
        let config =
            std::env::temp_dir().join(format!("tt-library-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(NullBackend))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_config_dir(config.clone())
            .with_docs(DocStore::open(config.join("docs")));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let token = support::pair(&base, &state).await;
        Box_ {
            base,
            token,
            c: reqwest::Client::new(),
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .c
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn add(&self, path: &Path) -> (u16, Value) {
        self.send(
            "POST",
            "/library/sources",
            Some(json!({ "path": path.display().to_string() })),
        )
        .await
    }

    /// The source once an index pass after `after` has finished.
    async fn indexed(&self, id: &str, after: u64) -> Value {
        let mut s = Value::Null;
        for _ in 0..200 {
            let (_, lib) = self.send("GET", "/library", None).await;
            s = lib["sources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["id"] == id)
                .cloned()
                .unwrap_or(Value::Null);
            let done = s["activity"].is_null()
                && (s["last_indexed"].as_u64().is_some_and(|t| t >= after)
                    || s["error"].is_string());
            if done {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("never indexed: {s}");
    }

    async fn paths_for(&self, query: &str) -> Vec<String> {
        let (_, r) = self
            .send("POST", "/library/search", Some(json!({ "query": query })))
            .await;
        r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["path"].as_str().map(str::to_string))
            .collect()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn only_ordinary_folders_inside_home_are_added() {
    let b = Box_::new("allowed").await;
    let outside = tempfile::tempdir().unwrap();
    let hidden = folder(".secrets-lib", &[("k.txt", "key")]);
    let mut cases = vec![
        (
            outside.path().to_path_buf(),
            "Pick a project folder inside your home folder.",
        ),
        (
            home().to_path_buf(),
            "Pick a project folder inside your home folder.",
        ),
        (home().join("not-there"), "That folder doesn't exist."),
    ];
    cases.push((hidden, "hidden folder"));
    for (path, why) in cases {
        let (status, r) = b.add(&path).await;
        assert_eq!(status, 400, "{}: {r}", path.display());
        assert!(r["error"].as_str().unwrap().contains(why), "{r}");
    }
    let r =
        b.c.post(format!("{}/library/sources", b.base))
            .json(&json!({ "path": "/tmp" }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn a_folder_is_indexed_searched_and_kept_current() {
    let b = Box_::new("index").await;
    let root = folder(
        "lease-papers",
        &[
            (
                "lease.md",
                "Pets are allowed with a refundable deposit of 300 dollars.",
            ),
            (
                "notes/boiler.txt",
                "The boiler was serviced in March by Quillfeather Heating.",
            ),
            (
                "node_modules/pkg/readme.md",
                "quillfeather should never be indexed from here",
            ),
            (".hidden/x.md", "quillfeather hidden"),
            ("photo.jpg", "not text"),
        ],
    );
    let t0 = now();
    let (status, r) = b.add(&root).await;
    assert_eq!(status, 200, "{r}");
    let id = r["source"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["source"]["path"], root.display().to_string());
    let s = b.indexed(&id, t0).await;
    assert!(s["error"].is_null(), "{s}");
    assert_eq!(s["files"], 2, "{s}");

    let hits = b.paths_for("Quillfeather boiler").await;
    assert_eq!(
        hits.first().map(String::as_str),
        Some(root.join("notes/boiler.txt").display().to_string().as_str()),
        "{hits:?}"
    );
    assert!(hits
        .iter()
        .all(|p| !p.contains("node_modules") && !p.contains(".hidden")));
    let (_, lib) = b.send("GET", "/library", None).await;
    assert_eq!(lib["docs"]["per_source"][&id][0], 2, "{lib}");
    assert_eq!(lib["home"], home().display().to_string());

    // A file goes, another arrives, one changes.
    std::fs::remove_file(root.join("notes/boiler.txt")).unwrap();
    std::fs::write(
        root.join("gutters.md"),
        "Gutters cleaned by Marrowbrook Roofing.",
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(
        root.join("lease.md"),
        "Pets need the landlord's written consent.",
    )
    .unwrap();
    let t1 = now();
    assert_eq!(
        b.send("POST", &format!("/library/sources/{id}/reindex"), None)
            .await,
        (200, json!({ "indexing": true }))
    );
    let s = b.indexed(&id, t1).await;
    assert_eq!(s["files"], 2, "{s}");
    assert!(b
        .paths_for("Quillfeather")
        .await
        .iter()
        .all(|p| !p.ends_with("boiler.txt")));
    assert!(b
        .paths_for("Marrowbrook Roofing")
        .await
        .contains(&root.join("gutters.md").display().to_string()));
    let (_, r) = b
        .send(
            "POST",
            "/library/search",
            Some(json!({ "query": "landlord written consent" })),
        )
        .await;
    let lease: Vec<&str> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["path"].as_str().is_some_and(|p| p.ends_with("lease.md")))
        .filter_map(|h| h["text"].as_str())
        .collect();
    assert!(
        !lease.is_empty() && lease.iter().all(|t| !t.contains("deposit")),
        "{r}"
    );

    // Removed: its documents go, its files stay.
    let (status, r) = b
        .send("DELETE", &format!("/library/sources/{id}"), None)
        .await;
    assert_eq!((status, &r["removed"]), (200, &json!(2)), "{r}");
    assert!(root.join("lease.md").exists());
    assert!(b.paths_for("Marrowbrook").await.is_empty());
    assert_eq!(
        b.send("DELETE", &format!("/library/sources/{id}"), None)
            .await
            .0,
        404
    );
    assert_eq!(
        b.send("POST", &format!("/library/sources/{id}/reindex"), None)
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn folders_inside_each_other_are_refused_but_neighbours_are_not() {
    let b = Box_::new("overlap").await;
    let parent = folder("research-area", &[("a.md", "alpha")]);
    let child = folder("research-area/sub", &[("b.md", "beta")]);
    let neighbour = folder("research-area-2", &[("c.md", "gamma")]);
    let (status, r) = b.add(&parent).await;
    assert_eq!(status, 200, "{r}");
    let id = r["source"]["id"].as_str().unwrap().to_string();
    let (status, r) = b.add(&child).await;
    assert_eq!(status, 409, "{r}");
    assert!(r["error"].as_str().unwrap().contains("overlaps"));
    let (status, r) = b.add(&parent).await;
    assert_eq!(status, 409, "the same folder twice: {r}");
    // A sibling whose name starts the same isn't inside it.
    let (status, r) = b.add(&neighbour).await;
    assert_eq!(status, 200, "{r}");
    let other = r["source"]["id"].as_str().unwrap().to_string();
    for id in [id, other] {
        b.send("DELETE", &format!("/library/sources/{id}"), None)
            .await;
    }
}

#[tokio::test]
async fn a_folder_that_went_away_says_so() {
    let b = Box_::new("gone").await;
    let root = folder("vanishing-box", &[("x.md", "Zephyrine notes")]);
    let t0 = now();
    let (_, r) = b.add(&root).await;
    let id = r["source"]["id"].as_str().unwrap().to_string();
    b.indexed(&id, t0).await;
    std::fs::remove_dir_all(&root).unwrap();
    b.send("POST", &format!("/library/sources/{id}/reindex"), None)
        .await;
    // Done only once it has an error.
    let s = b.indexed(&id, u64::MAX).await;
    assert_eq!(s["error"], "That folder isn't there any more.", "{s}");
    b.send("DELETE", &format!("/library/sources/{id}"), None)
        .await;
}
