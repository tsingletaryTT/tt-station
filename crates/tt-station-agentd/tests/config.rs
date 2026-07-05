//! Integration test for `GET /config` -- the agent's redacted serving-config
//! summary (Task 5 of the agentd-config-profiles plan).
//!
//! Follows the same shape as `tests/status.rs`/`tests/telemetry.rs`: start
//! the real axum `Router` (via `app()`) on an ephemeral port, with `AppState`
//! configured via a builder (`with_config_summary`, sibling to
//! `with_serving_config`/`with_telemetry_config`), then hit the route with a
//! real HTTP client.
//!
//! `/config` is UNAUTHED, like `/status`/`/models`/`/serving` -- the panel
//! and Mac app need to read the active/available profiles without pairing
//! first -- and the response is asserted to be exactly the `ConfigSummary`
//! the state was built with, including a raw-body check that "hf_token"
//! never appears: `ConfigSummary` has no field for one, so this is a
//! by-construction guarantee, not a redaction step the route could get wrong.

use std::sync::Arc;

use libttstation::model::ConfigSummary;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

/// `GET /config` should return 200 with a JSON body that round-trips back
/// to the exact `ConfigSummary` the `AppState` was built with, and the raw
/// response body must never contain "hf_token".
#[tokio::test]
async fn get_config_returns_summary_without_secrets() {
    let summary = ConfigSummary {
        active_profile: Some("stable".into()),
        available_profiles: vec!["stable".into()],
        backend: "runpy".into(),
        serving_host: "qb2-lab.local".into(),
        serving_port: 8003,
        serving_image: Some("img:0.14.0".into()),
        tt_inference_repo: Some("/home/x/tt-inference-server".into()),
        tt_device: None,
    };

    // `/config` doesn't touch the serving backend at all -- `DstackBackend`'s
    // no-op stub is enough of a `ServingBackend` for this test.
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
    .with_config_summary(summary.clone());
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let url = format!("http://{addr}/config");
    let resp = reqwest::get(&url).await.expect("GET /config failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body = resp.text().await.expect("failed to read response body");
    assert!(
        !body.contains("hf_token"),
        "response body must never carry hf_token: {body}"
    );

    let got: ConfigSummary =
        serde_json::from_str(&body).expect("response was not a valid ConfigSummary");
    assert_eq!(got, summary);
}
