//! Integration test for `GET /logs/stream` -- the WebSocket log-follow
//! endpoint (unauthed, like `GET /logs`/`GET /telemetry`).
//!
//! It starts the real axum `Router` (via `app()`) on an ephemeral port, with
//! `AppState` pointed at a temp directory standing in for a
//! `tt-inference-server` checkout (`with_log_source`). A real WebSocket
//! client connects to `/logs/stream?source=container&tail=2` against a
//! pre-seeded two-line log file, and asserts:
//!   1. it replays the last `tail` lines on connect (the two seeded lines,
//!      one per text frame), then
//!   2. it follows: a line appended to the file after connect arrives as a
//!      new frame within the 500ms `LOG_FOLLOW_INTERVAL` follow tick.
//!
//! Mirrors `tests/telemetry.rs`'s WS client harness
//! (`tokio_tungstenite::connect_async` + `futures_util::StreamExt`).

use std::sync::Arc;

use futures_util::StreamExt;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

#[tokio::test]
async fn logs_stream_replays_tail_then_follows() {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path().join("workflow_logs/docker_server");
    std::fs::create_dir_all(&dir).unwrap();
    let logfile = dir.join("vllm_test.log");
    std::fs::write(&logfile, "boot1\nboot2\n").unwrap();

    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path());
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("ws://{addr}/logs/stream?source=container&tail=2");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // first two frames = replayed tail
    let f1 = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let f2 = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(f1, "boot1");
    assert_eq!(f2, "boot2");

    // append a line; expect it to be followed
    use std::io::Write;
    let mut fh = std::fs::OpenOptions::new()
        .append(true)
        .open(&logfile)
        .unwrap();
    writeln!(fh, "boot3").unwrap();
    let f3 = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_text()
        .unwrap();
    assert_eq!(f3, "boot3");
}
