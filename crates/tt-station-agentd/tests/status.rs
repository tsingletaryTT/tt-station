//! Integration test for `GET /status` on the agentd HTTP skeleton.
//!
//! This starts the real axum `Router` (via the `app()` helper the crate
//! exposes precisely so tests can do this) bound to an OS-assigned ephemeral
//! port, with no mDNS side effects -- mDNS advertisement is `main.rs`'s job,
//! not the router's.

use std::sync::Arc;

use libttstation::model::ServingStatus;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

/// `GET /status` should return 200 with a JSON body whose `name` field
/// matches the name the `AppState` was constructed with, and whose `status`
/// field is the TXT string form (`idle`) of the initial `ServingStatus`.
#[tokio::test]
async fn status_returns_name_and_idle_status() {
    // `/status` doesn't touch the backend at all -- `DstackBackend`'s
    // no-op stub is enough of a `ServingBackend` for this test.
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    );
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
    // No `.with_device_mesh(..)` was applied, so this should serialize as
    // `null` -- never fatal, never a fabricated guess.
    assert_eq!(body["device_mesh"], serde_json::Value::Null);
    // Same reasoning for `.with_mac(..)` (Task 3, Wake-on-LAN): never
    // applied here, so this must also serialize as `null`.
    assert_eq!(body["mac"], serde_json::Value::Null);
}

/// `GET /status` on a state built with `.with_device_mesh(Some(..))` should
/// echo that mesh label verbatim, so Task 3's `tt --json status` can carry it
/// to the app without its own `tt-smi` access.
#[tokio::test]
async fn status_reports_device_mesh_when_set() {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
    .with_device_mesh(Some("p300x2".to_string()));
    let router = app(state);

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
    assert_eq!(body["device_mesh"], "p300x2");
}

/// `GET /status` on a state built with `.with_mac(Some(..))` should echo that
/// MAC verbatim -- mirrors `status_reports_device_mesh_when_set` exactly, so
/// the Mac app can send a Wake-on-LAN magic packet to this box when it's off
/// (Task 3).
#[tokio::test]
async fn status_reports_mac_when_set() {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
    .with_mac(Some("aa:bb:cc:dd:ee:ff".to_string()));
    let router = app(state);

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
    assert_eq!(body["mac"], "aa:bb:cc:dd:ee:ff");
}
