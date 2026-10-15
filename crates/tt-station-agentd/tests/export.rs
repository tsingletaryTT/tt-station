// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! "Your data" over HTTP: only owners export, import and change the data
//! settings; an export downloads as a .tar.gz that a dry-run import previews
//! on another box and a real import restores; both are in the activity log.

use std::sync::Arc;

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

mod support;

struct Idle;

impl ServingBackend for Idle {
    fn start_forcing(&self, _: &str, _: bool) -> anyhow::Result<Endpoint> {
        anyhow::bail!("unused")
    }
    fn stop(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
    fn list_models(&self) -> anyhow::Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

fn box_at(root: &std::path::Path) -> AppState {
    AppState::new("qb2".into(), "4xBH".into(), Arc::new(Idle))
        .with_config_dir(root.to_path_buf())
        .with_audit(tt_station_agentd::audit::AuditLog::open(
            root.join("audit.jsonl"),
        ))
        .with_memories(tt_station_agentd::memory::Memories::open(
            root.join("memories.json"),
        ))
        .with_jobs(tt_station_agentd::jobs::Jobs::open(root.join("jobs")))
}

async fn serve(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    base
}

#[tokio::test]
async fn owners_export_and_import_members_cannot() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    let old = box_at(&a);
    old.memories().add("Prefers metric units", 1, None).unwrap();
    std::fs::write(a.join("app-keys.json"), "SECRET-app-key").unwrap();
    let base = serve(old.clone()).await;
    let owner = support::pair(&base, &old).await;
    let member = support::pair(&base, &old).await;
    let client = reqwest::Client::new();

    for (method, path) in [
        (reqwest::Method::POST, "/export"),
        (reqwest::Method::POST, "/import?dry_run=true"),
        (reqwest::Method::PUT, "/data/settings"),
    ] {
        let r = client
            .request(method, format!("{base}{path}"))
            .bearer_auth(&member)
            .json(&json!({ "retention_days": 30 }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "{path}");
    }
    // Anyone may read the settings (off by default); owners change them.
    let s: Value = client
        .get(format!("{base}/data/settings"))
        .bearer_auth(&member)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["settings"]["keep_audio"], false);
    assert_eq!(s["settings"]["retention_days"], Value::Null);
    assert_eq!(s["owner"], false);
    let s: Value = client
        .put(format!("{base}/data/settings"))
        .bearer_auth(&owner)
        .json(&json!({ "retention_days": 30, "restart_last_model": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(s["retention_days"], 30);
    assert_eq!(s["restart_last_model"], true);
    let r = client
        .put(format!("{base}/data/settings"))
        .bearer_auth(&owner)
        .json(&json!({ "retention_days": 99999 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    let r = client
        .post(format!("{base}/export"))
        .bearer_auth(&owner)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "application/gzip");
    assert!(r.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .contains("tt-station-qb2-"));
    let archive = r.bytes().await.unwrap();
    assert!(!String::from_utf8_lossy(&archive).contains("SECRET"));

    // Another box: preview, then restore.
    let new = box_at(&b);
    let base_b = serve(new.clone()).await;
    let owner_b = support::pair(&base_b, &new).await;
    let preview: Value = client
        .post(format!("{base_b}/import?dry_run=true"))
        .bearer_auth(&owner_b)
        .body(archive.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preview["dry_run"], true);
    assert_eq!(preview["preview"]["categories"]["memories"]["added"], 1);
    assert_eq!(preview["preview"]["categories"]["prefs"]["added"], 1);
    assert!(
        new.memories().list().is_empty(),
        "a preview changes nothing"
    );
    let done: Value = client
        .post(format!("{base_b}/import"))
        .bearer_auth(&owner_b)
        .body(archive)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(done["dry_run"], false, "{done}");
    assert_eq!(new.memories().list()[0].text, "Prefers metric units");
    assert_eq!(
        tt_station_agentd::export::settings(&new).retention_days,
        Some(30)
    );
    // Not an export: refused, nothing written.
    let r = client
        .post(format!("{base_b}/import"))
        .bearer_auth(&owner_b)
        .body(b"not a tarball".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);

    let actions = |state: &AppState| -> Vec<(String, String)> {
        state
            .audit()
            .page(0, 100)
            .0
            .into_iter()
            .map(|e| (e.action, e.result))
            .collect()
    };
    let a_log = actions(&old);
    assert!(a_log.contains(&("export".into(), "ok".into())), "{a_log:?}");
    assert!(a_log.contains(&("data_settings".into(), "ok".into())));
    assert!(a_log.contains(&("denied".into(), "denied".into())));
    let b_log = actions(&new);
    assert!(b_log.contains(&("import".into(), "ok".into())), "{b_log:?}");
    assert!(b_log.contains(&("import".into(), "refused".into())));
}
