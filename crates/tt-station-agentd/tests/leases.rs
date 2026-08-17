//! Integration tests for `GET /leases` (Task 4 of the tt-station x tt-gozer
//! integration -- see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`) and the
//! `leasing` field Task 4 adds to `GET /status`.
//!
//! Unlike `tests/gozer.rs` (which injects a fake `CommandRunner` directly
//! into `gozer::snapshot_leases`), these routes shell out through a
//! hardcoded `RealCommandRunner` inside `spawn_blocking` -- same pattern as
//! `get_serving`/`collect_snapshot` in `routes.rs`. So instead of a fake
//! `CommandRunner`, these tests point `AppState::with_gozer`'s `Capability`
//! at a small STUB SCRIPT this file writes to a temp dir: a real
//! executable, but one that only ever prints canned `gozer status --json`
//! output -- never the real `gozer` binary, and never a mutating verb.
//! Every invocation is also recorded to a counter file, so the caching test
//! below asserts on something that would actually change if the cache
//! broke (a second file line), not just on the returned value.

use std::sync::Arc;
use std::time::Duration;

use tt_station_agentd::gozer::Capability;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

/// Write an executable shell-script stand-in for `gozer` at `dir/gozer-stub.sh`
/// that:
/// - appends one line to `counter_path` on EVERY invocation (so a test can
///   read the line count back to prove how many times it was actually run
///   -- see the module doc's note on not trusting return values alone), and
/// - prints `status_json` to stdout, exit 0, when called as `<script> status
///   --json` (the only invocation `snapshot_leases` ever makes).
///
/// A bare `sh` script, not a Rust helper binary: cheap to write per test,
/// and the counter file is plain-text so a failing test's on-disk state is
/// trivial to inspect by hand.
fn write_stub_gozer(dir: &std::path::Path, status_json: &str, counter_path: &std::path::Path) -> std::path::PathBuf {
    let script_path = dir.join("gozer-stub.sh");
    let script = format!(
        "#!/bin/sh\necho invoked >> \"{counter}\"\nif [ \"$1\" = \"status\" ]; then\n  cat <<'STUBEOF'\n{json}\nSTUBEOF\nfi\n",
        counter = counter_path.display(),
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

/// A fresh, process-and-test-unique temp dir under `$TMPDIR`, removed at the
/// start (never at the end -- these are tiny text files, and leaving them is
/// easier to debug a failure with than a `Drop` that might race a still-
/// running background task).
fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tt-station-leases-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// A two-board, four-chip `gozer status --json` payload -- one board
/// `CLAIMED` by a remote agent, one `FREE`. Same shape used in
/// `tests/gozer.rs`/`tests/runpy.rs`'s fixtures.
const STATUS_JSON_TWO_BOARDS: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false},
    {"dev_index":1,"bdf":"0000:02:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false},
    {"dev_index":2,"bdf":"0000:03:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,
     "reason":null,"pids_holding":[],"overstayed":false},
    {"dev_index":3,"bdf":"0000:04:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,
     "reason":null,"pids_holding":[],"overstayed":false}],
    "queue":[]}"#;

fn fresh_state() -> AppState {
    AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
}

/// Bind `state`'s router to an ephemeral port and serve it in the
/// background, handing back the base URL -- same helper shape as
/// `tests/ssh_authorize.rs`/`tests/status.rs`.
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

/// Pair against a freshly-spawned agent (the `/pair/init` + `/pair/complete`
/// dance every authed-route test in this crate uses) and return a valid
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

/// With no `.with_gozer(..)` applied, `GET /leases` must report
/// `available: false, leases: []` -- NOT an error, NOT a 5xx -- per the
/// design's explicit "unavailable marker, not an error" requirement. Also
/// proves the route is bearer-gated: a request with no token must be
/// rejected before ever reaching the (in this case, gozer-absent) handler
/// logic.
#[tokio::test]
async fn leases_reports_unavailable_when_gozer_absent_and_requires_auth() {
    let state = fresh_state();
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();

    // No bearer at all: must be rejected, not silently answered.
    let unauthed = client
        .get(format!("{base}/leases"))
        .send()
        .await
        .expect("GET /leases (no auth) failed to even connect");
    assert_ne!(
        unauthed.status(),
        reqwest::StatusCode::OK,
        "an unauthenticated GET /leases must not succeed"
    );

    let token = pair(&client, &state, &base).await;
    let resp = client
        .get(format!("{base}/leases"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /leases failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["available"], false);
    assert_eq!(body["leases"], serde_json::json!([]));
}

/// With gozer present (a stub script standing in for the real binary),
/// `GET /leases` must report `available: true` and every chip from the
/// canned `gozer status --json` payload, mapped field-for-field -- `chip`
/// from `dev_index`, `state` preserved verbatim (`CLAIMED`/`FREE`, not
/// renamed), `who`/`reason` passed through, and `since` ALWAYS `null` (gozer
/// never reports one -- see `LeaseEntry`'s doc comment).
#[tokio::test]
async fn leases_reports_lease_state_when_gozer_present() {
    let dir = temp_dir("present");
    let counter = dir.join("invocations.log");
    let stub = write_stub_gozer(&dir, STATUS_JSON_TWO_BOARDS, &counter);

    let state = fresh_state().with_gozer(Some(Capability {
        path: stub.to_string_lossy().into_owned(),
        version: "gozer 0.1.0".to_string(),
    }));
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let resp = client
        .get(format!("{base}/leases"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /leases failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["available"], true);
    let leases = body["leases"].as_array().expect("leases must be an array");
    assert_eq!(leases.len(), 4);

    let held = &leases[0];
    assert_eq!(held["chip"], 0);
    assert_eq!(held["bdf"], "0000:01:00.0");
    assert_eq!(held["board"], "0100014311601055");
    assert_eq!(held["state"], "CLAIMED");
    assert_eq!(held["who"], "claude:ttm-optimize");
    assert_eq!(held["reason"], "bringup");
    assert_eq!(
        held["since"],
        serde_json::Value::Null,
        "gozer's status never reports a start time -- this must never be invented"
    );

    let free = &leases[2];
    assert_eq!(free["chip"], 2);
    assert_eq!(free["state"], "FREE");
    assert_eq!(free["who"], serde_json::Value::Null);
    assert_eq!(free["reason"], serde_json::Value::Null);
}

/// Two `GET /leases` calls made back-to-back must collapse to ONE actual
/// `gozer status` invocation -- proven by the stub's own invocation counter,
/// not by return values (two stale-but-identical responses would pass a
/// value-only assertion even with no caching at all). After the cache's TTL
/// elapses, a THIRD call must trigger a fresh invocation -- proving this is
/// really a time-bounded cache, not a permanent memoization that would let
/// a Mac never see a real lease change.
#[tokio::test]
async fn leases_are_cached_within_ttl_and_refetched_after() {
    let dir = temp_dir("cache");
    let counter = dir.join("invocations.log");
    let stub = write_stub_gozer(&dir, STATUS_JSON_TWO_BOARDS, &counter);

    let state = fresh_state().with_gozer(Some(Capability {
        path: stub.to_string_lossy().into_owned(),
        version: "gozer 0.1.0".to_string(),
    }));
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    let count = |path: &std::path::Path| -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };

    for _ in 0..3 {
        let resp = client
            .get(format!("{base}/leases"))
            .bearer_auth(&token)
            .send()
            .await
            .expect("GET /leases failed");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
    assert_eq!(
        count(&counter),
        1,
        "3 rapid calls within the cache TTL must shell out to gozer exactly once"
    );

    // Sleep past the cache's short TTL, then call again: this MUST be a
    // fresh invocation, or a real lease change on the box would never
    // become visible to a client.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let resp = client
        .get(format!("{base}/leases"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /leases failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        count(&counter),
        2,
        "a call after the TTL elapsed must trigger a second invocation"
    );
}

/// `GET /status` gains a `leasing` object. With no gozer, it must report
/// `available: false` and null-out the rest rather than fabricating boards/
/// version/capacity for hardware that was never probed.
#[tokio::test]
async fn status_reports_leasing_unavailable_when_gozer_absent() {
    let state = fresh_state();
    let base = serve(state).await;
    let resp = reqwest::get(format!("{base}/status"))
        .await
        .expect("GET /status failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["leasing"]["available"], false);
    assert_eq!(body["leasing"]["version"], serde_json::Value::Null);
    assert_eq!(body["leasing"]["boards"], serde_json::Value::Null);
    assert_eq!(body["leasing"]["max_concurrent"], serde_json::Value::Null);
}

/// With gozer present, `/status`'s `leasing` must report the probed
/// version, plus `boards`/`max_concurrent` derived from the same
/// `gozer status --json` payload `GET /leases` uses (2 distinct board
/// serials in `STATUS_JSON_TWO_BOARDS`, board-grain, so `max_concurrent`
/// equals `boards`).
#[tokio::test]
async fn status_reports_leasing_details_when_gozer_present() {
    let dir = temp_dir("status-present");
    let counter = dir.join("invocations.log");
    let stub = write_stub_gozer(&dir, STATUS_JSON_TWO_BOARDS, &counter);

    let state = fresh_state().with_gozer(Some(Capability {
        path: stub.to_string_lossy().into_owned(),
        version: "gozer 0.1.0".to_string(),
    }));
    let base = serve(state).await;

    let resp = reqwest::get(format!("{base}/status"))
        .await
        .expect("GET /status failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["leasing"]["available"], true);
    assert_eq!(body["leasing"]["version"], "gozer 0.1.0");
    assert_eq!(body["leasing"]["boards"], 2);
    assert_eq!(body["leasing"]["max_concurrent"], 2);
}
