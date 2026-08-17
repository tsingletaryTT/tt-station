//! Route-level tests for the two contention surfaces Task 5 adds to the
//! tt-station x tt-gozer integration (see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`):
//!
//! * `POST /run` under contention answers `409 Conflict` naming the board
//!   and the holder's `who` -- and NO duration, because gozer exposes none
//!   (`status --json` has no `since`) and agentd must not invent one.
//! * `POST /reset` and `POST /power {"action":"reset-chips"}` REFUSE while a
//!   lease this request does not own is held. Both are whole-box `tt-smi -r`
//!   paths; with two tenants, each would reset the other's chips.
//!
//! Two different fakes, on purpose, because the two paths reach gozer
//! differently:
//!
//! * the backend paths (`/run`, `/reset`) shell out through the backend's
//!   injected `CommandRunner`, so they use `FakeRunner` and can assert on
//!   the recorded argv;
//! * `AppState::run_power_command` shells out through a hardcoded
//!   `RealCommandRunner` (same as `get_serving`/`gozer_snapshot`), so that
//!   path points `with_gozer` at a STUB SCRIPT -- a real executable that
//!   only ever prints canned `gozer status --json`. Never the real `gozer`,
//!   and never a mutating verb.

use std::sync::Arc;

use tt_station_agentd::gozer::Capability;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// A `gozer status --json` payload with one board CLAIMED by another tenant
/// -- the contention/refusal fixture shared by every test here. Note it
/// carries `who` and NO `since`: naming a holder is possible, reporting a
/// duration is not.
const STATUS_JSON_FOREIGN_HOLD: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false},
    {"dev_index":1,"bdf":"0000:02:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,"reason":null,"pids_holding":[],"overstayed":false}],
    "queue":[]}"#;

/// The same box with every chip FREE -- the positive control for the
/// refusal tests (see `power_reset_chips_runs_when_no_foreign_lease_is_held`).
const STATUS_JSON_ALL_FREE: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"FREE","who":null,"pid":null,"reason":null,"pids_holding":[],"overstayed":false}],
    "queue":[]}"#;

/// The capability a successful startup probe would have produced, pointed at
/// whatever `path` the caller wants (a bare `"gozer"` for `FakeRunner`-backed
/// tests, which never spawn anything; a stub script for the power path,
/// which really does exec it).
fn capability(path: &str) -> Capability {
    Capability {
        path: path.to_string(),
        version: "gozer 0.1.0".to_string(),
    }
}

/// Build an `AppState` backed by a `RunPyBackend` wired to `runner`, with
/// leasing turned on. Mirrors `tests/reset.rs`'s `fresh_state_with`.
fn state_with(runner: FakeRunner) -> AppState {
    let backend: Arc<dyn ServingBackend> = Arc::new(
        RunPyBackend::new(RunPyConfig::default(), Box::new(runner))
            .with_gozer(Some(capability("gozer"))),
    );
    AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend)
}

/// Bind `state`'s router to an ephemeral port and serve it in the
/// background, handing back the base URL. Same helper shape as
/// `tests/reset.rs`/`tests/leases.rs`.
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

/// Complete the pairing dance and return a valid bearer token.
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

/// A fresh, process-and-test-unique temp dir under `$TMPDIR`.
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tt-station-contention-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write an executable stand-in for `gozer` that prints `status_json` for
/// `<script> status --json` and exits 0 for anything else. Mirrors
/// `tests/leases.rs`'s helper of the same name.
fn write_stub_gozer(dir: &std::path::Path, status_json: &str) -> std::path::PathBuf {
    let script_path = dir.join("gozer-stub.sh");
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then\n  cat <<'STUBEOF'\n{json}\nSTUBEOF\nfi\n",
        json = status_json,
    );
    std::fs::write(&script_path, script).expect("write stub gozer script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x stub gozer script");
    }
    script_path
}

/// Write an executable stand-in for the configured `reset-chips` command
/// that TOUCHES `marker` when run. The marker file is the side channel: a
/// test can prove the board reset did or did not actually fire, rather than
/// trusting a status code that a refusal-after-the-fact would also produce.
fn write_marker_command(dir: &std::path::Path, marker: &std::path::Path) -> std::path::PathBuf {
    let script_path = dir.join("reset-chips-stub.sh");
    let script = format!("#!/bin/sh\n: > \"{}\"\n", marker.display());
    std::fs::write(&script_path, script).expect("write marker command script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x marker command script");
    }
    script_path
}

/// `POST /run` when gozer reports no free chips must be a `409 Conflict`
/// naming the board and the holder -- NOT a generic 500, and NOT a duration
/// gozer never reported.
#[tokio::test]
async fn run_under_contention_returns_409_naming_the_holder_and_no_duration() {
    let runner = FakeRunner::new(0);
    // gozer's real unavailable payload: it names nobody, which is why agentd
    // cross-references `status`.
    runner.set_run_capturing(
        "gozer acquire",
        12,
        r#"{"granted":false,"queued":false}"#,
        "",
    );
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_FOREIGN_HOLD, "");

    let state = state_with(runner.clone());
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::CONFLICT,
        "contention is a conflict with the box's current state, not a server error"
    );
    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    let error = body["error"].as_str().expect("error field missing");
    assert!(
        error.contains("claude:ttm-optimize"),
        "the response must name the holder: {error}"
    );
    assert!(
        error.contains("0100014311601055"),
        "the response must name the contended board: {error}"
    );
    assert!(
        !error.contains("since") && !error.contains("ago"),
        "gozer exposes no lease start time -- agentd must not imply a \
         duration: {error}"
    );

    // Side channel: nothing on the box may have been touched.
    let commands = runner.commands();
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("tt-smi")),
        "a contended run must not reset anything: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("python3")),
        "a contended run must not launch run.py: {commands:?}"
    );
}

/// `POST /reset` while another tenant holds a lease must refuse with `409`
/// and name the holder -- and must NOT carry out its other half either. The
/// token check is the side channel: `/reset` clears every issued bearer
/// token, so a refusal that still unpaired the caller would be a partial
/// reset masquerading as a refusal.
#[tokio::test]
async fn reset_refuses_and_keeps_pairing_when_a_foreign_lease_is_held() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_FOREIGN_HOLD, "");

    let state = state_with(runner.clone());
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/reset"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /reset failed");

    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    let error = body["error"].as_str().expect("error field missing");
    assert!(
        error.contains("claude:ttm-optimize"),
        "the refusal must name the holder: {error}"
    );

    assert!(
        !runner
            .commands()
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("tt-smi")),
        "a refused reset must not reset a neighbour's chips: {:?}",
        runner.commands()
    );

    // The caller is still paired: the refusal did nothing at all.
    let after = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_ne!(
        after.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a refused /reset must not clear the caller's bearer token"
    );
}

/// `POST /power {"action":"reset-chips"}` is the OTHER whole-box `tt-smi -r`
/// path -- `PowerAction::ResetChips` is not a machine op, so it skips the
/// backend entirely and used to run with no lease check whatsoever. It must
/// refuse the same way, and the marker file proves the configured reset
/// command never ran.
#[tokio::test]
async fn power_reset_chips_refuses_when_a_foreign_lease_is_held() {
    let dir = temp_dir("power-refuse");
    let marker = dir.join("reset-ran");
    let reset_cmd = write_marker_command(&dir, &marker);
    let stub = write_stub_gozer(&dir, STATUS_JSON_FOREIGN_HOLD);

    let runner = FakeRunner::new(0);
    let state = state_with(runner)
        .with_gozer(Some(capability(&stub.to_string_lossy())))
        .with_power_config(
            vec![reset_cmd.to_string_lossy().into_owned()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
        );
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/power"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "action": "reset-chips" }))
        .send()
        .await
        .expect("POST /power failed");

    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    let error = body["error"].as_str().expect("error field missing");
    assert!(
        error.contains("claude:ttm-optimize"),
        "the refusal must name the holder: {error}"
    );
    assert!(
        !marker.exists(),
        "the configured reset-chips command must NOT have run -- it would have \
         reset the holder's chips"
    );
}

/// The positive control for the test above: same wiring, same stub, but
/// every chip FREE -- the reset must actually run. Without this, the
/// refusal test would pass just as well against a `/power` route that never
/// ran anything at all.
#[tokio::test]
async fn power_reset_chips_runs_when_no_foreign_lease_is_held() {
    let dir = temp_dir("power-allow");
    let marker = dir.join("reset-ran");
    let reset_cmd = write_marker_command(&dir, &marker);
    let stub = write_stub_gozer(&dir, STATUS_JSON_ALL_FREE);

    let runner = FakeRunner::new(0);
    let state = state_with(runner)
        .with_gozer(Some(capability(&stub.to_string_lossy())))
        .with_power_config(
            vec![reset_cmd.to_string_lossy().into_owned()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
        );
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/power"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "action": "reset-chips" }))
        .send()
        .await
        .expect("POST /power failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(
        marker.exists(),
        "an uncontended reset-chips must still run the configured command"
    );
}

/// With NO gozer capability at all (a box without gozer installed), the
/// `reset-chips` path must behave exactly as it always did: run the
/// configured command, no lease check, no gozer subprocess. This is the
/// fallback half of the refusal above.
#[tokio::test]
async fn power_reset_chips_without_gozer_is_unchanged() {
    let dir = temp_dir("power-no-gozer");
    let marker = dir.join("reset-ran");
    let reset_cmd = write_marker_command(&dir, &marker);

    let runner = FakeRunner::new(0);
    let backend: Arc<dyn ServingBackend> =
        Arc::new(RunPyBackend::new(RunPyConfig::default(), Box::new(runner)));
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend)
        .with_power_config(
            vec![reset_cmd.to_string_lossy().into_owned()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
            vec!["/bin/true".to_string()],
        );
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .post(format!("{base}/power"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "action": "reset-chips" }))
        .send()
        .await
        .expect("POST /power failed");

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(marker.exists(), "the configured command must have run");
}
