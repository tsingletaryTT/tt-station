// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Community models on a box without `tt-model` (tt-model-manager): the
//! list says so, and the catalog, details, downloads, starts and removals
//! are refused with that reason, before anything is written or run. A bad
//! package name is still a bad name first.
//!
//! `HOME` is an empty scratch folder for this whole binary (set once, before
//! any test touches the agent), so the real `~/.local/bin/tt-model` isn't
//! found. Its own binary, apart from `tests/community_packages.rs`, whose
//! scratch home has a fake `tt-model`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use reqwest::Method as M;
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

const MISSING: &str = "tt-model (tt-model-manager) isn't installed on this box.";

fn empty_home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-no-tt-model-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        dir
    })
}

#[tokio::test]
async fn without_tt_model_everything_says_its_missing_and_nothing_is_written() {
    let home = empty_home();
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(config.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let token = support::pair(&base, &state).await;
    let client = reqwest::Client::new();
    let call = |method: M, path: &str, body: Value| {
        let mut req = client
            .request(method.clone(), format!("{base}{path}"))
            .bearer_auth(&token);
        if method == M::POST {
            req = req.json(&body);
        }
        async move {
            let r = req.send().await.unwrap();
            (
                r.status().as_u16(),
                r.json::<Value>().await.unwrap_or(Value::Null),
            )
        }
    };

    let (status, body) = call(M::GET, "/community", Value::Null).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["available"], false);
    assert_eq!(body["installed"], json!([]));
    assert_eq!(body["pulls"], json!([]));

    let id = "tt-community/some-model";
    for (method, path, body) in [
        (M::GET, "/community/catalog".to_string(), Value::Null),
        (M::GET, format!("/community/info/{id}"), Value::Null),
        (M::POST, "/community/pull".to_string(), json!({ "id": id })),
        (
            M::POST,
            "/community/serve".to_string(),
            json!({ "id": id, "anyway": true }),
        ),
        (M::DELETE, format!("/community/{id}"), Value::Null),
    ] {
        let (status, out) = call(method.clone(), &path, body).await;
        assert_eq!(status, 409, "{method} {path}: {out}");
        assert_eq!(out["error"], MISSING, "{method} {path}");
    }
    // A bad name is refused as one, whether or not tt-model is there.
    let (status, out) = call(M::POST, "/community/pull", json!({ "id": "../x" })).await;
    assert_eq!(status, 400, "{out}");

    assert!(!config.path().join("community").exists(), "no work folder");
    assert!(!home.join(".cache/tt-model").exists());
    let _ = std::fs::remove_dir_all(home);
}
