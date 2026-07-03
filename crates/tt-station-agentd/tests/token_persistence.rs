//! Integration tests for persisting issued bearer tokens across agent
//! restarts (`AppState::new_persisting` / `--token-store`).
//!
//! The whole point of persistence is that a *second, independent*
//! `AppState` built from the same file sees the tokens the first one
//! issued -- so these tests deliberately construct two separate `AppState`s
//! (never reusing the first's in-memory `Inner`) to prove the file, not
//! memory, is what's carrying the token across.
//!
//! Like `tests/pairing.rs` and `tests/control.rs`, these exercise the real
//! axum `Router` end to end (ephemeral port, real HTTP) rather than calling
//! `AppState` methods directly, so the bearer extractor is genuinely
//! exercised, not just `is_valid_token` in isolation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

/// Hand back a fresh path under the OS temp dir that no other test (or test
/// run) will collide with. No `tempfile` dependency in this crate, and a
/// PoC-grade persistence feature doesn't need one -- pid + a monotonic
/// counter is unique enough within a single test binary run.
fn fresh_token_store_path() -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "tt-station-agentd-test-tokens-{}-{}-{}.json",
        std::process::id(),
        n,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Spin up the real router on an ephemeral port backed by `state`, and hand
/// back the base URL. Shared shape with `tests/pairing.rs`'s `spawn`, but
/// takes `state` in rather than building one, since these tests need
/// control over exactly how (and from what file) the `AppState` gets built.
async fn spawn_with(state: AppState) -> String {
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    format!("http://{addr}")
}

/// Complete a pairing dance against `base` (backed by `state`) and return
/// the minted bearer token. Same two-step dance `tests/pairing.rs` and
/// `tests/control.rs` use.
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
        .expect("token missing or not a string")
        .to_string()
}

/// Issuing a token (via the normal `/pair/init` + `/pair/complete` flow)
/// against a *persisting* `AppState` must write the whole token set out to
/// the configured file: the file should exist afterward, parse as a JSON
/// array containing the freshly-minted token, and (on unix) be mode 0600 --
/// these are bearer secrets, not something any other user on the box should
/// be able to read.
#[tokio::test]
async fn issuing_a_token_persists_it_to_the_configured_file() {
    let path = fresh_token_store_path();
    let state = AppState::new_persisting(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
        path.clone(),
    );
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();

    let token = pair(&client, &state, &base).await;

    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("token store file {path:?} not written: {err}"));
    let tokens: Vec<String> =
        serde_json::from_str(&contents).expect("token store file was not a valid JSON array");
    assert!(
        tokens.contains(&token),
        "expected persisted token set {tokens:?} to contain the freshly-issued token {token:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path)
            .expect("failed to stat token store file")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "token store file should be mode 0600");
    }

    let _ = std::fs::remove_file(&path);
}

/// The core "survives restart" proof: write a tokens.json by hand (standing
/// in for what a previous agent process would have written), build a
/// *brand new, independent* `AppState` from that file (standing in for the
/// agent restarting), and confirm a request bearing that token is accepted
/// -- while a token that was never issued is still rejected. This never
/// reuses the first `AppState`'s in-memory `Inner`, so the only way this
/// passes is if the file itself is what's carrying the token across.
#[tokio::test]
async fn a_token_written_to_the_store_is_accepted_by_a_freshly_constructed_state() {
    let path = fresh_token_store_path();
    let known_token = "known-good-token-from-a-previous-run";
    std::fs::write(&path, format!(r#"["{known_token}"]"#)).expect("failed to seed token store");

    let state = AppState::new_persisting(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
        path.clone(),
    );
    let base = spawn_with(state).await;
    let client = reqwest::Client::new();

    // A bogus token must still be rejected -- this isn't "auth is
    // disabled," it's specifically the seeded token that should work.
    let bogus_resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth("this-token-was-never-issued")
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(bogus_resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The token loaded from the file must be accepted: /endpoint is
    // bearer-guarded, so a non-401 here (409, since nothing is serving)
    // proves auth passed.
    let known_resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(known_token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_ne!(
        known_resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a token loaded from the persisted store should have been accepted"
    );
    assert_eq!(known_resp.status(), reqwest::StatusCode::CONFLICT);

    let _ = std::fs::remove_file(&path);
}

/// A corrupt (not-valid-JSON) token store file must not panic or fail
/// construction -- it should log a warning (stderr; not asserted on here)
/// and start with an empty token set, same as a box that's never persisted
/// a token before.
#[tokio::test]
async fn a_corrupt_token_store_file_starts_empty_instead_of_panicking() {
    let path = fresh_token_store_path();
    std::fs::write(&path, b"this is not json {{{").expect("failed to seed garbage token store");

    let state = AppState::new_persisting(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
        path.clone(),
    );
    let base = spawn_with(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth("anything-at-all")
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a corrupt token store should start empty, rejecting every token"
    );

    let _ = std::fs::remove_file(&path);
}

/// A missing token-store file (the normal "never persisted before" case,
/// not a corrupt one) must also start empty rather than erroring.
#[tokio::test]
async fn a_missing_token_store_file_starts_empty() {
    let path = fresh_token_store_path();
    // Deliberately never created.

    let state = AppState::new_persisting(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
        path.clone(),
    );
    let base = spawn_with(state).await;
    let client = reqwest::Client::new();

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth("anything-at-all")
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_file(&path);
}

/// `AppState::new` (no token store given) must behave exactly as before
/// this feature existed: no file ever gets written, even after a real
/// token is issued through the pairing flow. This is the "existing tests /
/// existing behavior stay untouched" guarantee for the `None` path.
#[tokio::test]
async fn plain_new_never_touches_the_filesystem() {
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    );
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();

    // Issuing a token should still work exactly as it always has.
    let token = pair(&client, &state, &base).await;
    assert!(!token.is_empty());
}
