//! Integration test for `GET /status` on the agentd HTTP skeleton.
//!
//! This starts the real axum `Router` (via the `app()` helper the crate
//! exposes precisely so tests can do this) bound to an OS-assigned ephemeral
//! port, with no mDNS side effects -- mDNS advertisement is `main.rs`'s job,
//! not the router's.

use libttstation::model::ServingStatus;
use tt_station_agentd::routes::{app, AppState};

/// `GET /status` should return 200 with a JSON body whose `name` field
/// matches the name the `AppState` was constructed with, and whose `status`
/// field is the TXT string form (`idle`) of the initial `ServingStatus`.
#[tokio::test]
async fn status_returns_name_and_idle_status() {
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), "docker".to_string());
    let router = app(state);

    // Bind to an ephemeral port so the test doesn't collide with anything
    // else on the box (or with other test runs in parallel).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("http://{addr}/status");
    let resp = reqwest::get(&url).await.expect("GET /status failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["name"], "qb2-lab");
    assert_eq!(body["chips"], "4xBH");
    assert_eq!(body["status"], ServingStatus::Idle.to_txt());
}
