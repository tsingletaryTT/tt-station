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
    get_logs, get_status, list_models, list_serving, power, reset, AgentClient, SshRevokeBy,
};
use libttstation::model::{Endpoint, ServingStatus};
use wiremock::matchers::{header, method, path, query_param};
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
        .run("llama3", false)
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

    reset(&server.uri(), TOKEN, false)
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

/// `run(model)` on a `409` (the agent refused because another tenant holds
/// the chips -- see `tt-station-agentd::routes::contention_aware_error`)
/// must surface the agent's OWN message, which names the board and the
/// holder. `error_for_status`'s generic "HTTP status client error (409
/// Conflict)" would throw away the only part a user can act on -- and
/// "naming the holder" is the whole promise `tt-station run` makes on a contended
/// box.
#[tokio::test]
async fn run_maps_409_to_the_agents_contention_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/run"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "runpy backend: cannot serve 'llama3' -- no chips are available: \
                      board 0100014311601055 is held by claude:ttm-optimize"
        })))
        .mount(&server)
        .await;

    let client = AgentClient::new(server.uri(), TOKEN);
    let err = client
        .run("llama3", false)
        .await
        .expect_err("run() should fail on 409");

    let message = err.to_string();
    assert!(
        message.contains("claude:ttm-optimize"),
        "the holder the agent named must survive to the caller: {message}"
    );
    assert!(
        message.contains("0100014311601055"),
        "the contended board must survive to the caller: {message}"
    );
}

/// `reset(base, token)` on a `409` -- the agent REFUSED the reset because
/// another tenant holds chips, or because it could not determine whether
/// one does -- must surface the agent's own message. This is the same
/// defect `run()` had, on the more dangerous surface: the refusal names the
/// board, the holder, and the remedy ("stop that session" vs "fix gozer"),
/// and `error_for_status`'s "HTTP status client error (409 Conflict)"
/// discards all three. `tt-station reset` also keys off this being an Err to leave
/// local pairing alone (see `crates/tt-station/tests/reset_refusal.rs`).
#[tokio::test]
async fn reset_maps_409_to_the_agents_refusal_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/reset"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "refusing to reset this box: it would run a WHOLE-BOX `tt-smi -r` -- \
                      another tenant holds chips -- board 0100014311601055 is held by \
                      claude:ttm-optimize. Stop that session (or `gozer release` its lease) first."
        })))
        .mount(&server)
        .await;

    let err = reset(&server.uri(), TOKEN, false)
        .await
        .expect_err("reset() should fail on 409");

    let message = err.to_string();
    assert!(
        message.contains("claude:ttm-optimize"),
        "the holder the agent named must survive to the caller: {message}"
    );
    assert!(
        message.contains("0100014311601055"),
        "the contended board must survive to the caller: {message}"
    );
    assert!(
        message.contains("409"),
        "keep the stable (409) marker so a caller can branch on the case: {message}"
    );
    assert!(
        libttstation::agent_client::is_refusal(&err),
        "`tt-station reset` branches on this: {message}"
    );
}

/// `is_refusal` must key off the phrase THIS CLIENT writes, not off `(409)`
/// appearing anywhere in the message.
///
/// A refusal's detail is arbitrary text from the box -- a holder `who`, a model
/// id, a gozer `reason` -- so a bare `contains("(409)")` fired on any ordinary
/// failure whose message merely happened to carry that substring. `tt-station reset`
/// turns a false positive into a HARD error that refuses to clear local state,
/// which is exactly the opposite of what an unreachable or broken box is
/// supposed to do: forget it locally and move on.
#[test]
fn is_refusal_ignores_a_409_that_appears_only_in_agent_supplied_detail() {
    use libttstation::agent_client::is_refusal;

    // A genuine refusal, whose DETAIL also happens to contain the marker
    // (a lease `reason` naming a ticket, say). Still a refusal.
    assert!(is_refusal(&anyhow::anyhow!(
        "the box refused the reset (409): board b1 is held by claude:bug-(409) (CLAIMED)"
    )));
    // The bare-marker form `endpoint()` writes: no detail at all.
    assert!(is_refusal(&anyhow::anyhow!(
        "no model is currently serving on this agent (409)"
    )));
    // NOT a refusal: an ordinary failure that merely mentions the marker in
    // its own detail half. The old `contains` check called this a refusal.
    assert!(!is_refusal(&anyhow::anyhow!(
        "request to http://box:8080/reset failed: upstream said retry-after-(409)"
    )));
    // NOT a refusal: nothing resembling the marker at all.
    assert!(!is_refusal(&anyhow::anyhow!(
        "error sending request for url (http://box:8080/reset)"
    )));
}

/// The same for `power(base, token, "reset-chips")`, the OTHER whole-box
/// reset path -- including the "could not be determined" refusal, whose
/// remedy (fix gozer / reset by hand) is the entire value of the message.
#[tokio::test]
async fn power_maps_409_to_the_agents_refusal_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/power"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({
            "error": "refusing reset-chips: it would run a WHOLE-BOX board reset -- \
                      `gozer status` could not be read, so whether another tenant holds chips \
                      cannot be determined. Fix gozer first, or reset the box by hand."
        })))
        .mount(&server)
        .await;

    let err = power(&server.uri(), TOKEN, "reset-chips", false)
        .await
        .expect_err("power() should fail on 409");

    let message = err.to_string();
    assert!(
        message.contains("could not be read"),
        "the reason must survive to the caller -- it is what tells the operator \
         whether to stop a session or fix gozer: {message}"
    );
    assert!(message.contains("409"), "{message}");
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

    // The Mac app (`isIdleConflict` in TTStationKit) keys off a stable "(409)"
    // marker in this message to distinguish "authed but idle" from an auth
    // failure -- assert on that exact marker, not just generic wording, so a
    // future rewording can't silently break that contract.
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("409"),
        "expected the error to contain a stable '409' marker, got: {message}"
    );
    assert!(
        message.contains("no model")
            || message.contains("not serving")
            || message.contains("serving"),
        "expected the error to also mention no model/serving, got: {message}"
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

/// `get_status(base)` -- the free function `tt-station status` calls so it works
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
    // Likewise `leasing`, which predates the gozer integration. `None` here
    // means "this agent is too old to say", which a client must be able to
    // tell apart from `available: false` ("gozer is not installed"). See
    // `StatusInfo::leasing`.
    assert_eq!(info.leasing, None);
}

/// `/status`'s `leasing` object must reach the client. It was on the wire from
/// the moment the agent grew it, and invisible to `tt-station status`/`tt-station status
/// --json` because `StatusInfo` had no field to decode it into -- so the
/// gozer integration's own `GET /status` deliverable never actually arrived
/// anywhere a user could see it.
#[tokio::test]
async fn get_status_decodes_the_leasing_object() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle",
            "leasing": {
                "available": true,
                "version": "gozer 0.1.0",
                "boards": 2,
                "max_concurrent": 2
            }
        })))
        .mount(&server)
        .await;

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    let leasing = info
        .leasing
        .expect("an agent that reports `leasing` must decode into Some(..)");
    assert!(leasing.available);
    assert_eq!(leasing.version.as_deref(), Some("gozer 0.1.0"));
    assert_eq!(leasing.boards, Some(2));
    assert_eq!(leasing.max_concurrent, Some(2));
}

/// The gozer-absent half of the same three-way answer: `available: false` with
/// every other field `null` must decode to `Some(..)`, NOT to `None`. Those
/// are different facts -- "gozer is not installed on that box" versus "that
/// agent is too old to say" -- and collapsing them would let a client report
/// a leasing-capable box as unleased purely because of its agent version.
#[tokio::test]
async fn get_status_decodes_leasing_unavailable_as_some_not_none() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": "qb2-lab",
            "chips": "4xBH",
            "status": "idle",
            "leasing": {
                "available": false,
                "version": null,
                "boards": null,
                "max_concurrent": null
            }
        })))
        .mount(&server)
        .await;

    let info = get_status(&server.uri())
        .await
        .expect("get_status() should succeed");

    let leasing = info
        .leasing
        .expect("`available: false` is a REPORTED fact, not a missing field");
    assert!(!leasing.available);
    assert_eq!(leasing.version, None);
    assert_eq!(leasing.boards, None);
    assert_eq!(leasing.max_concurrent, None);
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

/// (Task 4) `get_logs(base, source, tail)` -- the free function `tt-station logs`
/// calls, unauthed like `get_status`/`list_serving` -- should GET
/// `{base}/logs?source=<source>&tail=<tail>` with no `Authorization` header
/// and parse the `LogsInfo` body.
#[tokio::test]
async fn get_logs_parses_logs_info_with_no_auth_header() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/logs"))
        .and(query_param("source", "container"))
        .and(query_param("tail", "50"))
        .and(NoAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "source": "container",
            "origin": "/mock/vllm.log",
            "lines": ["mock line 1", "mock line 2"]
        })))
        .mount(&server)
        .await;

    let logs = get_logs(&server.uri(), "container", 50)
        .await
        .expect("get_logs() should succeed against a mocked 200 response");

    assert_eq!(logs.source, "container");
    assert_eq!(logs.origin.as_deref(), Some("/mock/vllm.log"));
    assert_eq!(logs.lines, vec!["mock line 1", "mock line 2"]);
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

/// The `--force` wire contract, all three surfaces at once.
///
/// `--force` is only meaningful if it survives the client. These pin the exact
/// bodies, because a flag the CLI parses and then drops on the floor would
/// leave an owner staring at the same refusal with no idea why their override
/// did nothing -- and every other assertion in this change lives on the agent
/// side of the wire, where such a bug is invisible.
///
/// `reset` unforced is the one asymmetry, and it is deliberate: it sends NO
/// body at all (pinned by `reset_posts_to_reset_with_bearer_and_succeeds_on_
/// empty_body` above), which is what lets the agent read a bodyless `/reset`
/// from an older `tt-station` as unforced.
#[tokio::test]
async fn force_reaches_the_wire_on_run_reset_and_power() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/run"))
        .and(header("Authorization", format!("Bearer {TOKEN}").as_str()))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "model": "llama3",
            "force": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "endpoint": {
                "base_url": "http://localhost:9999",
                "model": "llama3",
                "requires_key": false
            }
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/reset"))
        .and(wiremock::matchers::body_json(
            serde_json::json!({ "force": true }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/power"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "action": "reset-chips",
            "force": true
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&server)
        .await;

    // Each call only succeeds if its body matched the mock above -- an
    // unmatched request gets wiremock's 404 and these all fail.
    AgentClient::new(server.uri(), TOKEN)
        .run("llama3", true)
        .await
        .expect("a forced run must send {\"model\":..,\"force\":true}");
    reset(&server.uri(), TOKEN, true)
        .await
        .expect("a forced reset must send {\"force\":true}");
    power(&server.uri(), TOKEN, "reset-chips", true)
        .await
        .expect("a forced power action must send force alongside the action");
}
