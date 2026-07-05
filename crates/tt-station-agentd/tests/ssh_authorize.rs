//! Integration tests for Task 2's authed `POST`/`DELETE /ssh/authorize`
//! routes: install/remove a paired client's SSH public key on the box's
//! `authorized_keys` file.
//!
//! Same shape as `tests/control.rs`'s `/run`/`/stop` tests -- the real axum
//! `Router` on an ephemeral port, paired via the two-step `/pair/init` +
//! `/pair/complete` dance -- but backed by a TEMP `authorized_keys` path via
//! `AppState::with_ssh_target`, so nothing here ever touches a real
//! `~/.ssh`.

use std::sync::Arc;
use std::time::Duration;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::docker::{DockerBackend, DockerConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// A fresh, process-unique temp `authorized_keys` PATH (parent dir created,
/// file itself absent until `authorize` writes it) -- unique per test name
/// so parallel `cargo test` runs never collide on the same file.
fn temp_authorized_keys(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tt-station-ssh-authorize-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir for authorized_keys fixture");
    dir.join("authorized_keys")
}

/// Build an `AppState` backed by a `DockerBackend` wired to a fresh
/// `FakeRunner` (same shape `tests/control.rs::fresh_state` uses -- these
/// tests don't exercise `/run`/`/stop`, but `AppState::new` needs *some*
/// backend), with the ssh target pointed at `ssh_path` as `"ttuser"`.
fn fresh_state(ssh_path: std::path::PathBuf) -> AppState {
    let runner = FakeRunner::new(0);
    let config = DockerConfig {
        image: "some/image:tag".to_string(),
        host: "127.0.0.1".to_string(),
        host_port: 8080,
        ..Default::default()
    };
    let backend: Arc<dyn ServingBackend> = Arc::new(
        DockerBackend::new(config, Box::new(runner)).with_health_poll(5, Duration::from_millis(1)),
    );

    AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend)
        .with_ssh_target(ssh_path, "ttuser".to_string())
}

/// Bind `state`'s router to an ephemeral port and serve it in the
/// background, handing back the base URL.
async fn serve(state: AppState) -> String {
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

async fn spawn(ssh_path: std::path::PathBuf) -> (AppState, String) {
    let state = fresh_state(ssh_path);
    let base = serve(state.clone()).await;
    (state, base)
}

/// Pair against a freshly-spawned agent (the same two-step dance
/// `tests/control.rs`/`tests/pairing.rs` exercise) and return a valid
/// bearer token.
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

/// `POST /ssh/authorize` with a valid key + valid bearer must return `200`,
/// `authorized: true`, `ssh_user: "ttuser"`, `already_present: false`, and
/// the temp file must actually contain the key.
#[tokio::test]
async fn authorize_with_valid_bearer_writes_key_and_reports_ssh_user() {
    let path = temp_authorized_keys("ok");
    let (state, base) = spawn(path.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/ssh/authorize"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1 test@mac",
            "label": "mac:test"
        }))
        .send()
        .await
        .expect("POST /ssh/authorize failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["authorized"], true);
    assert_eq!(body["ssh_user"], "ttuser");
    assert_eq!(body["already_present"], false);

    let contents =
        std::fs::read_to_string(&path).expect("authorized_keys should have been written");
    assert!(contents.contains("AAAAC3NzaC1lZDI1"));
    assert!(contents.contains("ttstation:mac:test"));
}

/// `POST /ssh/authorize` without an `Authorization` header must be rejected
/// (401/403 -- matching how `/run`'s own no-bearer test asserts: `401`), and
/// must not write anything.
#[tokio::test]
async fn authorize_without_bearer_is_rejected_and_writes_nothing() {
    let path = temp_authorized_keys("noauth");
    let (_state, base) = spawn(path.clone()).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/ssh/authorize"))
        .json(&serde_json::json!({
            "public_key": "ssh-ed25519 AAAAC3NzaC1lZDI1 test@mac",
            "label": "mac:test"
        }))
        .send()
        .await
        .expect("POST /ssh/authorize failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(
        !path.exists(),
        "unauthenticated request must not write the key"
    );
}

/// `POST /ssh/authorize` with private-key material in `public_key` must be
/// rejected with `400`, even with a valid bearer token.
#[tokio::test]
async fn authorize_with_private_key_body_returns_400() {
    let path = temp_authorized_keys("privkey");
    let (state, base) = spawn(path.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/ssh/authorize"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "public_key": "-----BEGIN OPENSSH PRIVATE KEY-----",
            "label": "mac:test"
        }))
        .send()
        .await
        .expect("POST /ssh/authorize failed");
    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!path.exists(), "a rejected key must never be written");
}

/// `DELETE /ssh/authorize` with a `label` must return `200 { revoked: true }`
/// and actually remove that key's line from the file.
#[tokio::test]
async fn revoke_with_label_returns_200_and_removes_line() {
    let path = temp_authorized_keys("revoke");
    let (state, base) = spawn(path.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/ssh/authorize"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "public_key": "ssh-ed25519 AAAADROPME drop@mac",
            "label": "mac:drop"
        }))
        .send()
        .await
        .expect("POST /ssh/authorize (setup) failed");

    let contents_before = std::fs::read_to_string(&path).expect("setup key should be present");
    assert!(contents_before.contains("AAAADROPME"));

    let resp = client
        .delete(format!("{base}/ssh/authorize"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "label": "mac:drop" }))
        .send()
        .await
        .expect("DELETE /ssh/authorize failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["revoked"], true);

    let contents_after =
        std::fs::read_to_string(&path).expect("authorized_keys should still exist");
    assert!(!contents_after.contains("AAAADROPME"));
}

/// `DELETE /ssh/authorize` without a bearer token must also be rejected.
#[tokio::test]
async fn revoke_without_bearer_is_rejected() {
    let path = temp_authorized_keys("revoke-noauth");
    let (_state, base) = spawn(path).await;
    let client = reqwest::Client::new();

    let resp = client
        .delete(format!("{base}/ssh/authorize"))
        .json(&serde_json::json!({ "label": "whatever" }))
        .send()
        .await
        .expect("DELETE /ssh/authorize failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}
