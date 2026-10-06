//! The `tt` backend behind the real axum `Router`: `/serving`, `/status` and `/stop` after an
//! agent restart. Task 4 ran the backend on hardware and found that `/serving` never listed a
//! tt-model serve (docker discovery recognises only tt-inference-server images), and that a
//! restarted agent reported idle and its `/stop` did nothing while the container kept serving.
//!
//! `tt` and gozer are the shared `FakeRunner` (no real `tt`, no hardware). `GET /serving` also
//! runs real docker discovery on whatever box runs the tests, so these tests count only the
//! entries they control (the agent entry, and entries on the fixture's port).

use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::tt::{TtBackend, TtConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// `tt --json model ps` captured live in Task 4 (one tt-model bundle on port 20010), verbatim.
const PS_LIVE_BUNDLE: &str = r#"{
  "served": [
    {
      "name": "changh95/qwen3.8-27b-p300x2",
      "backend": "model-manager",
      "container": "tt-model-qwen3.8-27b-p300x2-batch32",
      "container_id": "68a7a7fd598f",
      "image": "tt-model/qwen3.8-27b-p300x2:0becf4834925",
      "port": 20010,
      "base_url": "http://127.0.0.1:20010/v1",
      "status": "running",
      "health": "healthy",
      "started_at": "2026-10-06T02:33:40Z",
      "uptime_s": 286,
      "profile": "batch32",
      "served_id": "Qwen/Qwen3.8-27B"
    }
  ],
  "probed": true
}"#;

const PS_EMPTY: &str = r#"{"served": [], "probed": true}"#;

fn capability() -> tt_station_agentd::gozer::Capability {
    tt_station_agentd::gozer::Capability {
        path: "gozer".to_string(),
        version: "gozer 0.3.3".to_string(),
    }
}

/// A freshly started agent (the "after a restart" case): a new `TtBackend` that has recorded
/// nothing, an `AppState` that starts idle, serving port 20010.
fn restarted_agent(runner: &FakeRunner, gozer: bool) -> AppState {
    restarted_agent_with_backend(runner, gozer).0
}

/// [`restarted_agent`], also handing back the backend so a test can play the startup sweep.
fn restarted_agent_with_backend(runner: &FakeRunner, gozer: bool) -> (AppState, Arc<TtBackend>) {
    let config = TtConfig {
        tt_bin: "/fake/tt".to_string(),
        serving_host: "qb2-lab.local".to_string(),
        service_port: 20010,
        ..TtConfig::default()
    };
    let backend = Arc::new(
        TtBackend::new(config, Box::new(runner.clone())).with_gozer(gozer.then(capability)),
    );
    let dyn_backend: Arc<dyn ServingBackend> = backend.clone();
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), dyn_backend)
        .with_serving_config("qb2-lab.local".to_string(), 20010);
    (state, backend)
}

async fn serve(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

async fn pair(client: &reqwest::Client, state: &AppState, base: &str) -> String {
    let init: serde_json::Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pair_id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&pair_id).expect("pending code");
    let done: serde_json::Value = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    done["token"].as_str().unwrap().to_string()
}

async fn get_json(url: String) -> (reqwest::StatusCode, serde_json::Value) {
    let resp = reqwest::get(url).await.unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap())
}

/// Index of the first recorded command containing `needle` (space-joined argv).
fn pos(runner: &FakeRunner, needle: &str) -> Option<usize> {
    runner
        .commands()
        .iter()
        .position(|c| c.join(" ").contains(needle))
}

#[tokio::test]
async fn serving_lists_the_tt_backends_serve_once_as_the_agent() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 0, PS_LIVE_BUNDLE, "");
    let base = serve(restarted_agent(&runner, false)).await;

    let (status, body) = get_json(format!("{base}/serving")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let entries = body["serving"].as_array().unwrap();
    let agent: Vec<_> = entries.iter().filter(|e| e["source"] == "agent").collect();
    assert_eq!(agent.len(), 1, "exactly one agent entry: {body}");
    assert_eq!(agent[0]["model"], "Qwen/Qwen3.8-27B");
    assert_eq!(agent[0]["base_url"], "http://qb2-lab.local:20010/v1");
    assert_eq!(agent[0]["host_port"], 20010);
    assert_eq!(agent[0]["container"], "tt-model-qwen3.8-27b-p300x2-batch32");
    let on_port = entries.iter().filter(|e| e["host_port"] == 20010).count();
    assert_eq!(on_port, 1, "one server, one entry: {body}");
}

#[tokio::test]
async fn serving_still_answers_when_tt_fails() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 1, "", "docker: cannot connect");
    let base = serve(restarted_agent(&runner, false)).await;
    let (status, body) = get_json(format!("{base}/serving")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        body["serving"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["source"] != "agent"),
        "discovery only: {body}"
    );

    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("model ps", "No such file or directory");
    let base = serve(restarted_agent(&runner, false)).await;
    let (status, _) = get_json(format!("{base}/serving")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
}

/// A restarted agent that finds a row on its own serving port reports it, hands back its
/// endpoint, and `/stop` stops it by the row's name before releasing any lease.
#[tokio::test]
async fn a_restarted_agent_reports_and_stops_its_running_serve() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 0, PS_LIVE_BUNDLE, "");
    runner.set_run_capturing("gozer release", 0, r#"{"released":true}"#, "");
    let (state, backend) = restarted_agent_with_backend(&runner, true);
    // The startup sweep adopted the previous process's lease.
    assert!(backend.adopt_lease("lease-old", "tt-station:20010:changh95/qwen3.8-27b-p300x2"));
    let base = serve(state.clone()).await;

    let (_, body) = get_json(format!("{base}/status")).await;
    assert_eq!(body["status"], "serving:Qwen/Qwen3.8-27B", "{body}");

    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let endpoint: serde_json::Value = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(endpoint["base_url"], "http://qb2-lab.local:20010/v1");
    assert_eq!(endpoint["model"], "Qwen/Qwen3.8-27B");

    let resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let stop = pos(&runner, "model stop changh95/qwen3.8-27b-p300x2")
        .expect("tt model stop with the row's name (not the weights id)");
    let release = pos(&runner, "gozer release lease-old").expect("lease released");
    assert!(
        stop < release,
        "stop before release: {:?}",
        runner.commands()
    );
}

/// `/stop` straight after a restart, with no `/status` in between, still stops the serve.
#[tokio::test]
async fn stop_right_after_a_restart_stops_the_running_serve() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 0, PS_LIVE_BUNDLE, "");
    let state = restarted_agent(&runner, false);
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(pos(&runner, "model stop changh95/qwen3.8-27b-p300x2").is_some());
}

#[tokio::test]
async fn a_restarted_agent_with_nothing_on_its_port_stays_idle() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 0, PS_EMPTY, "");
    let base = serve(restarted_agent(&runner, false)).await;
    let (_, body) = get_json(format!("{base}/status")).await;
    assert_eq!(body["status"], "idle");

    // A row on a different port is somebody else's server, not ours.
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("model ps", 0, &PS_LIVE_BUNDLE.replace("20010", "20099"), "");
    let base = serve(restarted_agent(&runner, false)).await;
    let (_, body) = get_json(format!("{base}/status")).await;
    assert_eq!(body["status"], "idle");
}
