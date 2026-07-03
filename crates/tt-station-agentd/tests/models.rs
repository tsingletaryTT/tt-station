//! Integration test for `GET /models` on the agentd HTTP skeleton.
//!
//! Like `tests/status.rs`, starts the real axum `Router` on an ephemeral
//! port with no mDNS side effects, and -- since `/models` is UNAUTHED, same
//! as `/status` -- calls it with a plain `reqwest::get`, no bearer token.

use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::TempModelSpec;

/// `GET /models` with no `Authorization` header at all must still succeed
/// (unauthed, like `/status`) and return the `RunPyBackend`'s
/// `model_spec.json`-derived catalog.
#[tokio::test]
async fn models_returns_catalog_from_runpy_backend_with_no_auth() {
    let fixture = TempModelSpec::write(
        r#"{ "release_version": "0.12.0",
             "model_specs": { "Qwen/Qwen3-32B": { "P300X2": {"vLLM": {}}, "T3K": {"vLLM": {}} } } }"#,
    );
    let config = RunPyConfig {
        model_spec_path: Some(fixture.path()),
        ..Default::default()
    };
    let backend: Arc<dyn ServingBackend> =
        Arc::new(RunPyBackend::new(config, Box::new(RealCommandRunnerStub)));

    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let resp = reqwest::get(format!("http://{addr}/models"))
        .await
        .expect("GET /models failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["release_version"], "0.12.0");
    assert_eq!(body["models"][0]["name"], "Qwen/Qwen3-32B");
    assert_eq!(body["models"][0]["devices"][0], "P300X2");
}

/// `GET /models` against the default `ServingBackend::list_models` impl
/// (exercised here via `DstackBackend`, which doesn't override it) must
/// return 200 with an empty catalog, not an error -- a backend with no
/// model spec of its own isn't a failure case.
#[tokio::test]
async fn models_returns_empty_catalog_for_backend_without_override() {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    );
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let resp = reqwest::get(format!("http://{addr}/models"))
        .await
        .expect("GET /models failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["release_version"], serde_json::Value::Null);
    assert_eq!(
        body["models"]
            .as_array()
            .expect("models should be an array")
            .len(),
        0
    );
}

/// `RunPyBackend::list_models` never actually shells out (it only reads a
/// file), so this stub `CommandRunner` is never called for anything in
/// this test -- it exists purely to satisfy `RunPyBackend::new`'s
/// constructor, which needs SOME `Box<dyn CommandRunner>` even though
/// `/models` never exercises `start`/`stop`.
struct RealCommandRunnerStub;

impl tt_station_agentd::serving::docker::CommandRunner for RealCommandRunnerStub {
    fn run(&self, _args: &[&str]) -> anyhow::Result<String> {
        unreachable!("list_models never shells out")
    }

    fn health_ok(&self, _url: &str) -> bool {
        unreachable!("list_models never probes health")
    }
}
