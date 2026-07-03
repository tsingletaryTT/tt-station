//! Integration tests for the agent control routes (Task 10): `POST /run`,
//! `POST /stop`, `GET /endpoint`, all gated behind bearer auth on top of the
//! pairing tokens Task 7 mints.
//!
//! Like `tests/pairing.rs` and `tests/serving.rs`, these exercise the real
//! axum `Router` end to end (ephemeral port, real HTTP), with the
//! `ServingBackend` swapped for a `DockerBackend` wired to the shared
//! `FakeRunner` test double (see `tests/support/mod.rs`) so no real `docker`
//! binary or HTTP health probe is ever touched.

use std::sync::Arc;
use std::time::Duration;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::docker::DockerBackend;
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// Spin up the real router on an ephemeral port, backed by a `DockerBackend`
/// wired to a fresh `FakeRunner` that reports healthy on the very first
/// probe (so `/run` resolves immediately rather than polling). Returns the
/// `AppState` (so tests can read `/status` state Rust-side if ever needed)
/// and the base URL.
async fn spawn() -> (AppState, String) {
    let runner = FakeRunner::new(0);
    let backend: Arc<dyn ServingBackend> = Arc::new(
        DockerBackend::new(
            "tenstorrent/tt-inference-server:latest".to_string(),
            "127.0.0.1".to_string(),
            8080,
            Box::new(runner),
        )
        .with_health_poll(5, Duration::from_millis(1)),
    );

    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let router = app(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (state, format!("http://{addr}"))
}

/// Pair against a freshly-spawned agent (the same two-step dance
/// `tests/pairing.rs` exercises) and return a valid bearer token.
async fn pair(client: &reqwest::Client, state: &AppState, base: &str) -> String {
    let init_resp: serde_json::Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .expect("POST /pair/init failed")
        .json()
        .await
        .expect("init response was not valid JSON");
    let pair_id = init_resp["pair_id"]
        .as_str()
        .expect("pair_id missing")
        .to_string();

    let code = state
        .last_code(&pair_id)
        .expect("expected a pending code for the freshly-issued pair_id");

    let complete_resp: serde_json::Value = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .expect("POST /pair/complete failed")
        .json()
        .await
        .expect("complete response was not valid JSON");

    complete_resp["token"]
        .as_str()
        .expect("token missing")
        .to_string()
}

/// `POST /run` with a valid bearer token should return an `endpoint` whose
/// `model` matches the request, and `/status` should flip to
/// `serving:<model>`.
#[tokio::test]
async fn run_with_valid_bearer_returns_endpoint_and_flips_status() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = run_resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["endpoint"]["model"], "llama3");
    assert_eq!(body["endpoint"]["requires_key"], false);
    assert!(body["endpoint"]["base_url"]
        .as_str()
        .expect("base_url missing")
        .contains("8080"));

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "serving:llama3");
}

/// `POST /run` without an `Authorization` header must be rejected with 401,
/// and must not start anything.
#[tokio::test]
async fn run_without_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let run_resp = client
        .post(format!("{base}/run"))
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `POST /run` with a garbage bearer token (never minted by `/pair/complete`)
/// must also be rejected with 401.
#[tokio::test]
async fn run_with_invalid_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth("not-a-real-token")
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `GET /endpoint` while idle (nothing ever `/run`) must return 409, even
/// with a valid bearer token.
#[tokio::test]
async fn endpoint_while_idle_returns_409() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
}

/// `GET /endpoint` after a successful `/run` should return the same
/// `base_url` `/run` handed back.
#[tokio::test]
async fn endpoint_after_run_returns_base_url() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["model"], "llama3");
    assert!(body["base_url"]
        .as_str()
        .expect("base_url missing")
        .contains("8080"));
}

/// `POST /stop` after a `/run` should flip `/status` back to `idle`, and a
/// subsequent `/endpoint` should 409 again.
#[tokio::test]
async fn stop_flips_status_back_to_idle() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    let stop_resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /stop failed");
    assert_eq!(stop_resp.status(), reqwest::StatusCode::OK);

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "idle");

    let endpoint_resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(endpoint_resp.status(), reqwest::StatusCode::CONFLICT);
}

/// `POST /stop` without a bearer token must be rejected with 401.
#[tokio::test]
async fn stop_without_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let resp = client
        .post(format!("{base}/stop"))
        .send()
        .await
        .expect("POST /stop failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}
