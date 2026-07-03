//! Unit tests for the `libttstation::agent_client` client (Task 11) -- the
//! counterpart to the bearer-guarded control routes `tt-station-agentd`
//! exposes (Task 10): `GET /status`, `POST /run`, `POST /stop`, and
//! `GET /endpoint`.
//!
//! Run against a `wiremock` mock server rather than a real `tt-station-agentd`
//! instance, same rationale as `tests/pairing_client.rs`: libttstation sits
//! *below* agentd in the dependency graph, and wiremock still exercises the
//! real `reqwest` request/response path (method, path, headers, body).
//!
//! Every test asserts the `Authorization: Bearer <token>` header is present
//! with the exact configured token -- that's the one behavior all four
//! methods share and the brief calls out explicitly.

use libttstation::agent_client::{get_status, list_models, reset, AgentClient};
use libttstation::model::{Endpoint, ServingStatus};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "tok-abc123";

/// Custom `wiremock` matcher asserting a request carries NO `Authorization`
/// header at all -- the positive-side counterpart to every other test in
/// this file, which matches ON `header("Authorization", ...)`. Used by
/// [`get_status_sends_no_authorization_header`] below to prove
/// `get_status` (the free-function, unauthed counterpart to
/// `AgentClient::status`) really doesn't attach a bearer token, not just
/// that the mock happens to accept requests regardless of headers.
struct NoAuthorizationHeader;

impl Match for NoAuthorizationHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

/// `status()` should GET `{base}/status` with the bearer header and parse
/// the `status` field (`serving:<model>` / `idle`) via `ServingStatus::from_txt`.
#[tokio::test]
async fn status_parses_serving_status_from_response_body() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "serving:llama3"
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let status = client
        .status()
        .await
        .expect("status() should succeed against a mocked 200 response");

    assert_eq!(status, ServingStatus::Serving("llama3".to_string()));
}

/// `status()` should also parse the `idle` case correctly (not just
/// `serving:<model>`).
#[tokio::test]
async fn status_parses_idle_from_response_body() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle"
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let status = client.status().await.expect("status() should succeed");

    assert_eq!(status, ServingStatus::Idle);
}

/// `run(model)` should POST `{"model": "..."}` to `{base}/run` with the
/// bearer header and return the nested `endpoint`.
#[tokio::test]
async fn run_posts_model_and_returns_nested_endpoint() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/run"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": {
                "base_url": "http://localhost:9999",
                "model": "llama3",
                "requires_key": false
            }
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let endpoint = client
        .run("llama3")
        .await
        .expect("run() should succeed against a mocked 200 response");

    assert_eq!(
        endpoint,
        Endpoint {
            base_url: "http://localhost:9999".to_string(),
            model: "llama3".to_string(),
            requires_key: false,
        }
    );
}

/// `stop()` should POST to `{base}/stop` with the bearer header and succeed
/// on an empty `{}` response body.
#[tokio::test]
async fn stop_succeeds_on_empty_response_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/stop"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    client.stop().await.expect("stop() should succeed");
}

/// `reset(base, token)` -- the free-function counterpart to the agent's
/// bearer-guarded `POST /reset` -- should POST to `{base}/reset` WITH the
/// bearer header and succeed on an empty `{}` response body, same shape as
/// `AgentClient::stop`.
#[tokio::test]
async fn reset_posts_to_reset_with_bearer_and_succeeds_on_empty_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/reset"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    reset(&server.uri(), TOKEN)
        .await
        .expect("reset() should succeed against a mocked 200 response");
}

/// `endpoint()` should GET `{base}/endpoint` with the bearer header and
/// parse the `Endpoint` on 200.
#[tokio::test]
async fn endpoint_returns_endpoint_on_200() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/endpoint"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "base_url": "http://localhost:9999",
            "model": "llama3",
            "requires_key": true
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let endpoint = client
        .endpoint()
        .await
        .expect("endpoint() should succeed against a mocked 200 response");

    assert_eq!(
        endpoint,
        Endpoint {
            base_url: "http://localhost:9999".to_string(),
            model: "llama3".to_string(),
            requires_key: true,
        }
    );
}

/// `endpoint()` on a `409` (nothing currently serving, per the agent's own
/// `GET /endpoint` semantics) must map to a clear `Err` mentioning that no
/// model is serving, not a generic "409" error or a panic trying to parse a
/// body that isn't an `Endpoint`.
#[tokio::test]
async fn endpoint_maps_409_to_no_model_serving_error() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/endpoint"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let err = client
        .endpoint()
        .await
        .expect_err("endpoint() should fail on 409");

    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("no model")
            || message.contains("not serving")
            || message.contains("serving"),
        "expected the error to mention no model/serving, got: {message}"
    );
}

/// `list_models(base)` should GET `{base}/models` -- UNAUTHED, unlike every
/// `AgentClient` method above -- and parse the `ModelsResponse` body.
#[tokio::test]
async fn list_models_parses_models_response_with_no_auth_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "release_version": "0.12.0",
            "models": [
                { "name": "Qwen/Qwen3-32B", "devices": ["P300X2", "T3K"] },
                { "name": "Qwen/Qwen3-8B", "devices": ["P150X4"] }
            ]
        })))
        .mount(&server)
        .await;

    let resp = list_models(&server.uri())
        .await
        .expect("list_models() should succeed against a mocked 200 response");

    assert_eq!(resp.release_version.as_deref(), Some("0.12.0"));
    assert_eq!(resp.models.len(), 2);
    assert_eq!(resp.models[0].name, "Qwen/Qwen3-32B");
    assert_eq!(resp.models[0].devices, vec!["P300X2", "T3K"]);
}

/// `get_status(base)` -- the free-function, UNAUTHED counterpart to
/// `AgentClient::status()` that `tt status` now calls so it works against
/// an unpaired box -- should GET `{base}/status` with no `Authorization`
/// header and parse the `serving:<model>` case via `ServingStatus::from_txt`,
/// same as `AgentClient::status()` does.
#[tokio::test]
async fn get_status_parses_serving_status_with_no_auth_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "serving:meta-llama/Llama-3.3-70B-Instruct"
        })))
        .mount(&server)
        .await;

    let status = get_status(&server.uri())
        .await
        .expect("get_status() should succeed against a mocked 200 response");

    assert_eq!(
        status,
        ServingStatus::Serving("meta-llama/Llama-3.3-70B-Instruct".to_string())
    );
}

/// `get_status(base)` should also parse the `idle` case correctly, still
/// with no `Authorization` header required.
#[tokio::test]
async fn get_status_parses_idle_with_no_auth_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle"
        })))
        .mount(&server)
        .await;

    let status = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    assert_eq!(status, ServingStatus::Idle);
}
