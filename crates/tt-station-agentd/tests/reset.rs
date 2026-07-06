//! Integration tests for `POST /reset` (reset-to-fresh, for demos): the
//! bearer-guarded route that returns a box to a fresh-install state -- stop
//! any serving container, reset the board, clear ALL issued bearer tokens,
//! and go back to `idle`.
//!
//! Like `tests/control.rs`, these drive the real axum `Router` end to end
//! (ephemeral port, real HTTP). The backend is a `RunPyBackend` wired to the
//! shared `FakeRunner` test double (see `tests/support/mod.rs`) -- the one
//! backend that actually issues a container stop and a board reset on
//! `reset`, so these tests can assert on the exact `docker`/`tt-smi` argv it
//! shells out.

use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// Build an `AppState` backed by a `RunPyBackend` wired to `runner`. The
/// backend's `service_port` (8000, the `RunPyConfig` default) is what
/// `stop_serving_containers` builds its `docker ps --filter publish=<port>`
/// query around, so a test that wants a container "found" registers its
/// `docker ps` output against that runner before handing it in here.
fn fresh_state_with(runner: FakeRunner) -> AppState {
    let backend: Arc<dyn ServingBackend> =
        Arc::new(RunPyBackend::new(RunPyConfig::default(), Box::new(runner)));
    AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend)
}

/// A fresh, process-unique temp `authorized_keys` PATH (parent dir created,
/// file itself absent until something writes it) -- mirrors
/// `tests/ssh_authorize.rs`'s `temp_authorized_keys` helper so this test
/// never touches a real `~/.ssh`.
fn temp_authorized_keys(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tt-station-reset-ssh-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir for authorized_keys fixture");
    dir.join("authorized_keys")
}

/// Bind `state`'s router to an ephemeral port and serve it in the background,
/// handing back the base URL. Mirrors `tests/control.rs`'s `serve`.
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

/// Complete the two-step pairing dance against a freshly-spawned agent and
/// return a valid bearer token -- same helper shape as `tests/control.rs`.
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

/// `POST /reset` with a valid bearer must: return 200, actually issue the
/// container stop (`docker ps` + `docker stop <id>`) and board reset
/// (`tt-smi -r`), invalidate the caller's now-stale token (a later `/run`
/// with it is 401), and leave `/status` idle.
#[tokio::test]
async fn reset_with_valid_bearer_stops_serving_resets_board_and_invalidates_token() {
    let client = reqwest::Client::new();

    // Make `docker ps` report a live container so `stop_serving_containers`
    // has something to `docker stop` -- proving the stop path really runs.
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "container-abc123");

    let state = fresh_state_with(runner.clone());
    let base = serve(state.clone()).await;
    let token = pair(&client, &state, &base).await;

    let reset_resp = client
        .post(format!("{base}/reset"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /reset failed");
    assert_eq!(reset_resp.status(), reqwest::StatusCode::OK);

    // The stop + board-reset commands must have been issued.
    let commands = runner.commands();
    let issued = |needle: &str| commands.iter().any(|cmd| cmd.join(" ").contains(needle));
    assert!(
        issued("docker ps"),
        "reset should query for serving containers: {commands:?}"
    );
    assert!(
        issued("docker stop container-abc123"),
        "reset should stop the found serving container: {commands:?}"
    );
    assert!(
        issued("tt-smi -r"),
        "reset should reset the board: {commands:?}"
    );

    // The caller's own token is now invalid: a bearer-gated call is 401.
    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // And the box reports idle.
    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "idle");
}

/// `POST /reset` must also revoke every `ttstation:<label>`-tagged SSH key
/// the pair flow ever installed (see `authkeys::revoke_all_ttstation`) --
/// the whole point of the demo is that a reset costs the Mac its keyless
/// SSH access, not just its bearer token. An unrelated, non-`ttstation`
/// line (another app's key) must survive the reset untouched.
#[tokio::test]
async fn reset_revokes_all_ttstation_ssh_keys_but_keeps_unrelated_ones() {
    let client = reqwest::Client::new();

    let ssh_path = temp_authorized_keys("revokes-keys");
    let runner = FakeRunner::new(0);
    let state = fresh_state_with(runner).with_ssh_target(ssh_path.clone(), "ttuser".to_string());
    let base = serve(state.clone()).await;
    let token = pair(&client, &state, &base).await;

    // Install a ttstation-tagged key (as the pair flow's SSH-authorize step
    // would) and a manually-added, unrelated key with no ttstation marker.
    let authorize_resp = client
        .post(format!("{base}/ssh/authorize"))
        .bearer_auth(&token)
        .json(&serde_json::json!({
            "public_key": "ssh-ed25519 AAAAMACKEY mac@laptop",
            "label": "mac:2026-07-05"
        }))
        .send()
        .await
        .expect("POST /ssh/authorize failed");
    assert_eq!(authorize_resp.status(), reqwest::StatusCode::OK);

    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&ssh_path)
            .expect("append unrelated key");
        writeln!(f, "ssh-ed25519 AAAAOTHERAPP other-app@elsewhere").unwrap();
    }

    let body_before = std::fs::read_to_string(&ssh_path).expect("read authorized_keys");
    assert!(body_before.contains("AAAAMACKEY"));
    assert!(body_before.contains("AAAAOTHERAPP"));

    let reset_resp = client
        .post(format!("{base}/reset"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /reset failed");
    assert_eq!(reset_resp.status(), reqwest::StatusCode::OK);

    let body_after = std::fs::read_to_string(&ssh_path).expect("read authorized_keys");
    assert!(
        !body_after.contains("AAAAMACKEY"),
        "reset should revoke the ttstation-tagged key: {body_after:?}"
    );
    assert!(
        body_after.contains("AAAAOTHERAPP"),
        "reset must not touch unrelated, non-ttstation keys: {body_after:?}"
    );
}

/// `POST /reset` without a bearer token must be rejected with 401 and must
/// NOT touch the backend (no stop/reset commands issued).
#[tokio::test]
async fn reset_without_bearer_returns_401_and_touches_nothing() {
    let client = reqwest::Client::new();

    let runner = FakeRunner::new(0);
    let state = fresh_state_with(runner.clone());
    let base = serve(state).await;

    let resp = client
        .post(format!("{base}/reset"))
        .send()
        .await
        .expect("POST /reset failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    assert!(
        runner.commands().is_empty(),
        "an unauthorized /reset must not run any backend command: {:?}",
        runner.commands()
    );
}

/// A `/run` (status -> serving) followed by `/reset` must flip `/status`
/// back to `idle` -- proving the reset genuinely clears serving state, not
/// just that a never-served box happens to already be idle.
#[tokio::test]
async fn reset_flips_status_from_serving_back_to_idle() {
    let client = reqwest::Client::new();

    let runner = FakeRunner::new(0);
    let state = fresh_state_with(runner.clone());
    let base = serve(state.clone()).await;
    let token = pair(&client, &state, &base).await;

    // Start serving so status is `serving:<model>` before the reset.
    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::OK);

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "serving:llama3");

    // Reset, then confirm we're back to idle.
    let reset_resp = client
        .post(format!("{base}/reset"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /reset failed");
    assert_eq!(reset_resp.status(), reqwest::StatusCode::OK);

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "idle");
}
