//! Integration tests for the `options` field of `POST /run` and for
//! `GET /run/schema`.
//!
//! These drive the real axum `Router` over real HTTP (ephemeral port) with a
//! fake `ServingBackend` that RECORDS what the route handed it. That is the
//! point of `options_reach_the_backend`: it asserts the wiring from the HTTP
//! body to the backend call, which a validator-only test cannot see (a route
//! that parsed the options and then dropped them would still pass the
//! validator's own tests).

use std::sync::{Arc, Mutex};

use libttstation::model::{Endpoint, ServingStatus};
use libttstation::run_options::RunOptions;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

/// What the fake backend saw: one entry per `start_with_options` call.
#[derive(Default)]
struct Calls {
    seen: Vec<(String, bool, RunOptions)>,
}

/// A backend that overrides `start_with_options` and records its arguments.
struct RecordingBackend {
    calls: Arc<Mutex<Calls>>,
}

fn endpoint() -> Endpoint {
    Endpoint {
        base_url: "http://127.0.0.1:20001/v1".to_string(),
        model: "m".to_string(),
        requires_key: false,
    }
}

impl ServingBackend for RecordingBackend {
    fn start_forcing(&self, _model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        panic!("route must go through start_with_options");
    }
    fn start_with_options(
        &self,
        model: &str,
        force: bool,
        options: &RunOptions,
    ) -> anyhow::Result<Endpoint> {
        self.calls
            .lock()
            .unwrap()
            .seen
            .push((model.to_string(), force, options.clone()));
        Ok(endpoint())
    }
    fn stop(&self, _model: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
}

/// A backend that keeps the refusing DEFAULT `start_with_options`.
struct DefaultBackend;

impl ServingBackend for DefaultBackend {
    fn start_forcing(&self, _model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        Ok(endpoint())
    }
    fn stop(&self, _model: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
    fn backend_name(&self) -> &'static str {
        "fakebackend"
    }
}

async fn serve(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Complete the two-step pairing and return a bearer token (same shape as
/// `tests/reset.rs`).
async fn pair(client: &reqwest::Client, state: &AppState, base: &str) -> String {
    let init: serde_json::Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pair_id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&pair_id).expect("pending code");
    let done: serde_json::Value = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    done["token"].as_str().unwrap().to_string()
}

/// Boot an agent around `backend`; returns (client, base, token).
async fn boot(backend: Arc<dyn ServingBackend>) -> (reqwest::Client, String, String) {
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    (client, base, token)
}

#[tokio::test]
async fn old_client_body_without_options_still_runs() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (client, base, token) = boot(Arc::new(RecordingBackend { calls: calls.clone() })).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let c = calls.lock().unwrap();
    assert_eq!(c.seen.len(), 1);
    assert_eq!(c.seen[0].0, "m");
    assert!(!c.seen[0].1, "force defaults to false");
    assert!(c.seen[0].2.is_empty(), "no options sent means empty options");
}

#[tokio::test]
async fn force_still_reaches_the_backend() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (client, base, token) = boot(Arc::new(RecordingBackend { calls: calls.clone() })).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m", "force": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(calls.lock().unwrap().seen[0].1);
}

#[tokio::test]
async fn options_reach_the_backend() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (client, base, token) = boot(Arc::new(RecordingBackend { calls: calls.clone() })).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m", "options": { "port": 20001 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let c = calls.lock().unwrap();
    assert_eq!(c.seen.len(), 1);
    assert_eq!(
        c.seen[0].2.port,
        Some(20001),
        "the backend must receive the parsed options, not defaults"
    );
}

#[tokio::test]
async fn invalid_option_is_400_with_field_and_backend_not_called() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (client, base, token) = boot(Arc::new(RecordingBackend { calls: calls.clone() })).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m", "options": { "profile": "a b" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(msg.starts_with("profile: "), "got {msg:?}");
    assert_eq!(calls.lock().unwrap().seen.len(), 0);
}

#[tokio::test]
async fn non_object_options_is_400_not_422() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let (client, base, token) = boot(Arc::new(RecordingBackend { calls: calls.clone() })).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m", "options": 5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(calls.lock().unwrap().seen.len(), 0);
}

#[tokio::test]
async fn unsupported_backend_is_400_naming_the_backend() {
    let (client, base, token) = boot(Arc::new(DefaultBackend)).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m", "options": { "port": 20001 } }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("does not support run options"), "got {msg:?}");
    assert!(msg.contains("fakebackend"), "got {msg:?}");
}

#[tokio::test]
async fn default_backend_still_runs_without_options() {
    let (client, base, token) = boot(Arc::new(DefaultBackend)).await;
    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "m" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn run_schema_is_unauthed_and_lists_options() {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DefaultBackend),
    );
    let base = serve(state).await;
    let resp = reqwest::get(format!("{base}/run/schema")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(text.contains("\"device\""), "got {text}");
}
