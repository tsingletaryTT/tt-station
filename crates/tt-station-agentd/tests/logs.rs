//! Integration test for `GET /logs` on the agentd HTTP skeleton.
//!
//! Mirrors `tests/status.rs`'s ephemeral-port harness. Exercises the three
//! contract cases: newest-file tail on the `run` source, empty-but-200 when
//! no log file exists yet, and 409 when no tt-inference-server repo is
//! configured (e.g. the dstack backend).

use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

async fn spawn(state: AppState) -> String {
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn logs_run_source_returns_newest_file_tail() {
    let repo = tempfile::tempdir().unwrap();
    let run_dir = repo.path().join("workflow_logs/run_logs");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("old.log"), "old1\nold2\n").unwrap();
    std::fs::write(run_dir.join("new.log"), "a\nb\nc\n").unwrap();
    // make new.log newest
    let t = filetime::FileTime::from_unix_time(2_000_000, 0);
    filetime::set_file_mtime(run_dir.join("new.log"), t).unwrap();
    let t0 = filetime::FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(run_dir.join("old.log"), t0).unwrap();

    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path());
    let base = spawn(state).await;

    let body: serde_json::Value = reqwest::get(format!("{base}/logs?source=run&tail=2"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["source"], "run");
    assert_eq!(body["lines"], serde_json::json!(["b", "c"]));
    assert!(body["origin"].as_str().unwrap().ends_with("new.log"));
}

#[tokio::test]
async fn logs_no_file_yields_empty_lines_null_origin() {
    let repo = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path());
    let base = spawn(state).await;
    let resp = reqwest::get(format!("{base}/logs?source=container"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["lines"], serde_json::json!([]));
    assert!(body["origin"].is_null());
}

#[tokio::test]
async fn logs_no_repo_configured_is_conflict() {
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend));
    let base = spawn(state).await;
    let resp = reqwest::get(format!("{base}/logs?source=run"))
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
}
