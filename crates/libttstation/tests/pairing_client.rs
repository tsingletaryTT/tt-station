//! Unit tests for the `libttstation::pairing` client (Task 8), run against a
//! `wiremock` mock server rather than a real `tt-station-agentd` instance --
//! that keeps this crate's tests from depending on the agent crate at all
//! (libttstation sits *below* agentd/tt in the dependency graph) while still
//! exercising the real `reqwest` request/response path.
//!
//! Covers the three cases the brief calls out:
//!   - `/pair/init` 200 -> `pair_id` parsed out of the JSON body.
//!   - `/pair/complete` 200 (for the right request body) -> `token` parsed
//!     out of the JSON body.
//!   - `/pair/complete` 401 -> a clear `Err`, not a panic or a parsed body.

use libttstation::pairing::{pair_complete, pair_init};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `pair_init` should POST to `{base}/pair/init` and return the `pair_id`
/// from the JSON response body.
#[tokio::test]
async fn pair_init_returns_pair_id_from_response_body() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/pair/init"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "pair_id": "abc"
        })))
        .mount(&server)
        .await;

    let pair_id = pair_init(&server.uri())
        .await
        .expect("pair_init should succeed against a mocked 200 response");

    assert_eq!(pair_id, "abc");
}

/// `pair_complete` should POST `{pair_id, code}` to `{base}/pair/complete`
/// and return the `token` from the JSON response body when the server
/// accepts the code.
#[tokio::test]
async fn pair_complete_returns_token_for_correct_code() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/pair/complete"))
        .and(body_json(serde_json::json!({
            "pair_id": "abc",
            "code": "042817"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "tok123"
        })))
        .mount(&server)
        .await;

    let token = pair_complete(&server.uri(), "abc", "042817")
        .await
        .expect("pair_complete should succeed against a mocked 200 response");

    assert_eq!(token, "tok123");
}

/// A 401 from `/pair/complete` (wrong or expired code) must surface as an
/// `Err`, not a panic and not a successfully-parsed empty token.
#[tokio::test]
async fn pair_complete_returns_err_on_401() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/pair/complete"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let result = pair_complete(&server.uri(), "abc", "000000").await;

    assert!(
        result.is_err(),
        "expected an Err for a 401 response, got {result:?}"
    );
}
