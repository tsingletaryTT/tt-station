//! Integration tests for the pairing flow (`POST /pair/init`, `POST
//! /pair/complete`) added in Task 7.
//!
//! These exercise the real axum `Router` end to end, same pattern as
//! `tests/status.rs`: bind an ephemeral port, spawn the router, hit it with
//! `reqwest`. The one addition is `AppState::last_code`, a test-only hook
//! (see the `test-hooks` feature / self-dependency in Cargo.toml) that lets
//! the test read the code the agent generated for a given `pair_id` --
//! standing in for a human reading the code off the box's screen.

use tt_station_agentd::routes::{app, AppState};

/// Spin up the real router on an ephemeral port and hand back both the
/// `AppState` (so the test can call the `last_code` hook) and the base URL.
async fn spawn() -> (AppState, String) {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        "docker".to_string(),
    );
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

/// `POST /pair/init` should return 200 with a JSON body containing a
/// non-empty `pair_id`.
#[tokio::test]
async fn init_returns_a_pair_id() {
    let (_state, base) = spawn().await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .expect("POST /pair/init failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    let pair_id = body["pair_id"]
        .as_str()
        .expect("pair_id missing or not a string");
    assert!(!pair_id.is_empty());
}

/// The happy path: init a pair, read the code back out via the test hook
/// (standing in for a human typing what the box displayed), complete with
/// that exact code, and get a bearer token back.
#[tokio::test]
async fn complete_with_correct_code_returns_a_token() {
    let (state, base) = spawn().await;
    let client = reqwest::Client::new();

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

    let complete_resp = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .expect("POST /pair/complete failed");
    assert_eq!(complete_resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = complete_resp
        .json()
        .await
        .expect("response was not valid JSON");
    let token = body["token"]
        .as_str()
        .expect("token missing or not a string");
    assert!(!token.is_empty());
}

/// A wrong code for a real, unexpired pair_id must be rejected with 401 --
/// and must NOT mint a token.
#[tokio::test]
async fn complete_with_wrong_code_returns_401() {
    let (_state, base) = spawn().await;
    let client = reqwest::Client::new();

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

    let complete_resp = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": "000000" }))
        .send()
        .await
        .expect("POST /pair/complete failed");
    assert_eq!(complete_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// An unknown `pair_id` (never issued, or already consumed/expired) must
/// also be rejected with 401 rather than panicking or 500ing.
#[tokio::test]
async fn complete_with_unknown_pair_id_returns_401() {
    let (_state, base) = spawn().await;
    let client = reqwest::Client::new();

    let complete_resp = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": "does-not-exist", "code": "123456" }))
        .send()
        .await
        .expect("POST /pair/complete failed");
    assert_eq!(complete_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// An expired pair_id must be rejected with 401 even when the code
/// presented is the correct one -- the TTL is a hard cutoff, not just a
/// "prefer unexpired" hint. Uses the `insert_expired_pair` test hook to
/// seed a pair whose expiry is already in the past, rather than sleeping
/// for the real `PAIR_TTL` (120s) in a test.
#[tokio::test]
async fn complete_with_expired_pair_returns_401_even_with_correct_code() {
    let (state, base) = spawn().await;
    let client = reqwest::Client::new();

    state.insert_expired_pair("expired-pair-id", "123456");

    let complete_resp = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": "expired-pair-id", "code": "123456" }))
        .send()
        .await
        .expect("POST /pair/complete failed");
    assert_eq!(complete_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// A `pair_id` that racks up `MAX_PAIR_ATTEMPTS` wrong guesses in a row gets
/// invalidated -- so even a subsequent attempt with the CORRECT code is
/// rejected with 401. This is the anti-brute-force cap: without it, a LAN
/// client could just try all 10^6 codes for a pair_id within the 120s TTL.
#[tokio::test]
async fn complete_locks_out_pair_id_after_max_wrong_attempts() {
    let (state, base) = spawn().await;
    let client = reqwest::Client::new();

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

    // Burn through the attempt cap with wrong guesses. Each one must be
    // rejected with 401 just like the single-wrong-guess case.
    for _ in 0..tt_station_agentd::routes::MAX_PAIR_ATTEMPTS {
        let resp = client
            .post(format!("{base}/pair/complete"))
            .json(&serde_json::json!({ "pair_id": pair_id, "code": "000000" }))
            .send()
            .await
            .expect("POST /pair/complete failed");
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    // The pair_id is now locked out: even the CORRECT code no longer works.
    let final_resp = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .expect("POST /pair/complete failed");
    assert_eq!(final_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}
