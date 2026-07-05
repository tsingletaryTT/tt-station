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

use libttstation::agent_client::{
    get_status, list_models, list_serving, reset, AgentClient, SshRevokeBy,
};
use libttstation::model::{Endpoint, ServingStatus};
use wiremock::matchers::{header, method, path};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

const TOKEN: &str = "tok-abc123";

/// Custom `wiremock` matcher asserting a request carries NO `Authorization`
/// header at all -- the positive-side counterpart to every other test in
/// this file, which matches ON `header("Authorization", ...)`. Used by
/// [`get_status_sends_no_authorization_header`] below to prove
/// `get_status` really doesn't attach a bearer token, not just that the mock
/// happens to accept requests regardless of headers.
struct NoAuthorizationHeader;

impl Match for NoAuthorizationHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
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

/// `list_serving(base)` should GET `{base}/serving` -- UNAUTHED, like
/// `list_models` -- and parse the `ServingList` body (every live
/// `tt-inference-server` `/v1` endpoint the box reports).
#[tokio::test]
async fn list_serving_parses_serving_list_with_no_auth_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/serving"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "serving": [
                {
                    "model": "meta-llama/Llama-3.3-70B-Instruct",
                    "base_url": "http://127.0.0.1:8000/v1",
                    "host_port": 8000,
                    "container": "tt-agent-llama",
                    "source": "agent"
                },
                {
                    "model": "Qwen/Qwen3-32B",
                    "base_url": "http://127.0.0.1:8003/v1",
                    "host_port": 8003,
                    "container": "tt-studio-qwen",
                    "source": "external"
                }
            ]
        })))
        .mount(&server)
        .await;

    let list = list_serving(&server.uri())
        .await
        .expect("list_serving() should succeed against a mocked 200 response");

    assert_eq!(list.serving.len(), 2);
    assert_eq!(list.serving[0].model, "meta-llama/Llama-3.3-70B-Instruct");
    assert_eq!(list.serving[0].host_port, 8000);
    assert_eq!(list.serving[0].source, "agent");
    assert_eq!(list.serving[1].base_url, "http://127.0.0.1:8003/v1");
    assert_eq!(list.serving[1].source, "external");
}

/// `get_status(base)` -- the free function `tt status` calls so it works
/// against an unpaired box -- should GET `{base}/status` with no
/// `Authorization` header and parse the `serving:<model>` case via
/// `ServingStatus::from_txt`.
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

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed against a mocked 200 response");

    assert_eq!(
        info.status,
        ServingStatus::Serving("meta-llama/Llama-3.3-70B-Instruct".to_string())
    );
    // The mocked response above omits `device_mesh` entirely (it predates
    // Task 2) -- confirm the missing key deserializes to `None` rather than
    // erroring.
    assert_eq!(info.device_mesh, None);
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

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    assert_eq!(info.status, ServingStatus::Idle);
}

/// (Task 3) `get_status(base)` should decode a present `device_mesh` string
/// straight through from the agent's `/status` payload, unmodified.
#[tokio::test]
async fn get_status_parses_device_mesh_when_present() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle",
            "device_mesh": "p300x2"
        })))
        .mount(&server)
        .await;

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    assert_eq!(info.device_mesh, Some("p300x2".to_string()));
}

/// (Task 3) An explicit JSON `null` for `device_mesh` (what the agent sends
/// when its own detection failed/didn't run -- see
/// `tt-station-agentd::routes::StatusResponse`) must decode to `None`, same
/// as an omitted key.
#[tokio::test]
async fn get_status_parses_null_device_mesh_as_none() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle",
            "device_mesh": null
        })))
        .mount(&server)
        .await;

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    assert_eq!(info.device_mesh, None);
}

/// (Task 3) `ssh_authorize(public_key, label)` should POST
/// `{"public_key": "...", "label": "..."}` to `{base}/ssh/authorize` with
/// the bearer header, and decode the agent's `{authorized, ssh_user,
/// already_present}` response body -- mirroring `run`'s
/// authed-POST-with-body-decode shape exactly (see
/// `tt-station-agentd::routes::ssh_authorize`/`SshAuthorizeResponse`, Task 2).
#[tokio::test]
async fn ssh_authorize_posts_body_and_decodes_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ssh/authorize"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "public_key": "ssh-ed25519 AAAA... test",
            "label": "taylors-mac"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "authorized": true,
            "ssh_user": "ttuser",
            "already_present": false
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let result = client
        .ssh_authorize("ssh-ed25519 AAAA... test", "taylors-mac")
        .await
        .expect("ssh_authorize() should succeed against a mocked 200 response");

    assert!(result.authorized);
    assert_eq!(result.ssh_user, "ttuser");
    assert!(!result.already_present);
}

/// (Task 3) `ssh_revoke(SshRevokeBy::Label(...))` should DELETE
/// `{base}/ssh/authorize` with the bearer header and a `{"label": "..."}`
/// body, succeeding on the agent's `{"revoked": true}` response -- mirroring
/// `tt-station-agentd::routes::ssh_revoke`'s label-identified path.
#[tokio::test]
async fn ssh_revoke_by_label_sends_delete_with_label_body() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/ssh/authorize"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "label": "taylors-mac"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "revoked": true
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    client
        .ssh_revoke(SshRevokeBy::Label("taylors-mac".to_string()))
        .await
        .expect("ssh_revoke() should succeed against a mocked 200 response");
}

/// (Task 3) `ssh_revoke(SshRevokeBy::PublicKey(...))` should send the same
/// DELETE, but with a `{"public_key": "..."}` body instead of `label`.
#[tokio::test]
async fn ssh_revoke_by_public_key_sends_delete_with_public_key_body() {
    let server = MockServer::start().await;

    Mock::given(method("DELETE"))
        .and(path("/ssh/authorize"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "public_key": "ssh-ed25519 AAAA... test"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "revoked": true
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    client
        .ssh_revoke(SshRevokeBy::PublicKey(
            "ssh-ed25519 AAAA... test".to_string(),
        ))
        .await
        .expect("ssh_revoke() should succeed against a mocked 200 response");
}
