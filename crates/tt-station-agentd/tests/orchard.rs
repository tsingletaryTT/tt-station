// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Orchard end to end against a fake tt-orchard checkout
//! (`tests/fixtures/fake-orchard`): setup and preflight through the bridge,
//! a bring-up that stops the serving model, holds the chips, reports each
//! stage, ends ripe and puts the model back; then pause, abort and a refusal.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::orchard::Orchard;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

/// Records starts and stops.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl ServingBackend for Recorder {
    fn start_forcing(&self, model: &str, _: bool) -> anyhow::Result<Endpoint> {
        self.calls.lock().unwrap().push(format!("start {model}"));
        Ok(Endpoint {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: model.into(),
            requires_key: false,
        })
    }
    fn stop(&self, model: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push(format!("stop {model}"));
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
    fn list_models(&self) -> anyhow::Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

struct Box {
    base: String,
    token: String,
    client: reqwest::Client,
}

impl Box {
    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
    async fn run(&self, name: &str) -> Value {
        self.get(&format!("/orchard/runs/{name}")).await
    }
    async fn wait_for(&self, name: &str, state: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let run = self.run(name).await;
            if run["state"] == state {
                return run;
            }
            assert!(Instant::now() < deadline, "waiting for {state}: {run}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_bring_up_holds_the_box_reports_each_stage_and_gives_the_box_back() {
    let scratch = std::env::temp_dir().join(format!("tt-orchard-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let checkout = scratch.join("tt-orchard");
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-orchard"),
        &checkout,
    );
    std::fs::create_dir_all(checkout.join("config")).unwrap();
    std::fs::write(
        checkout.join("config/bringup.json"),
        json!({ "runs_root": scratch.join("runs") }).to_string(),
    )
    .unwrap();

    let backend = Arc::new(Recorder::default());
    let orchard = Orchard::new(&checkout, "python3", scratch.join("agent"))
        .offline()
        .without_systemd()
        .poll_every(Duration::from_millis(150))
        .abort_grace(Duration::from_secs(1))
        .settle_after_run(Duration::from_millis(300));
    // Chip resets are recorded, never run: the default is the real `tt-smi -r`.
    let resets = scratch.join("resets");
    let record = vec![
        "sh".to_string(),
        "-c".to_string(),
        format!("echo reset >> {}", resets.display()),
    ];
    let never = vec!["false".to_string()];
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), backend.clone())
        .with_power_config(record, never.clone(), never.clone(), never)
        .with_orchard(orchard);
    state.adopt_serving(Endpoint {
        base_url: "http://127.0.0.1:9/v1".into(),
        model: "Qwen/Qwen3-8B".into(),
        requires_key: false,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    tokio::spawn(tt_station_agentd::orchard::watch_forever(state.clone()));

    let client = reqwest::Client::new();
    let init: Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pair_id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&pair_id).unwrap();
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let b = Box {
        base: base.clone(),
        token: done["token"].as_str().unwrap().into(),
        client,
    };

    // Setup and config come from tt-orchard's own Python.
    let o = b.get("/orchard").await;
    assert_eq!(o["installed"], true, "{o}");
    assert_eq!(o["version"], "9.9.9");
    assert_eq!(o["ready"], true, "{o}");
    assert_eq!(o["setup"][1]["name"], "tt-gozer");
    assert_eq!(o["stages"][2]["name"], "graft");

    // Preflight.
    let (code, pre) = b
        .post("/orchard/preflight", json!({ "model": "acme/Tiny-1" }))
        .await;
    assert_eq!(code, 200, "{pre}");
    assert_eq!(pre["blocked"], false);
    assert_eq!(pre["run_name"], "acme--tiny-1");
    assert_eq!(pre["download_gb"], 54.0);
    let (code, _) = b
        .post("/orchard/preflight", json!({ "model": "../../etc" }))
        .await;
    assert_eq!(code, 400);
    let (_, refused) = b
        .post("/orchard/preflight", json!({ "model": "refuse/x" }))
        .await;
    assert_eq!(refused["blocked"], true);

    // A serving model has to be stopped first, and only with a yes.
    let (code, body) = b
        .post("/orchard/runs", json!({ "model": "acme/Tiny-1" }))
        .await;
    assert_eq!(code, 409);
    assert_eq!(body["needs_stop"], true);
    assert!(
        body["error"].as_str().unwrap().contains("Qwen3-8B"),
        "{body}"
    );
    let (code, body) = b
        .post(
            "/orchard/runs",
            json!({ "model": "acme/Tiny-1", "stop_serving": true }),
        )
        .await;
    assert_eq!(code, 202, "{body}");
    assert_eq!(body["restore"], "Qwen3-8B");
    assert!(backend
        .calls
        .lock()
        .unwrap()
        .contains(&"stop Qwen/Qwen3-8B".to_string()));
    // The chips are reset before the run, after the model that used them stopped.
    assert_eq!(std::fs::read_to_string(&resets).unwrap().lines().count(), 1);

    // The chips are Orchard's until it ends.
    assert!(state.chips_holder().unwrap().contains("Orchard"));
    let (code, body) = b
        .post("/orchard/runs", json!({ "model": "acme/Other" }))
        .await;
    assert_eq!(code, 409, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("One bring-up at a time"));

    let run = b
        .wait_for("acme--tiny-1", "ready-for-operator-review")
        .await;
    assert_eq!(run["stages"].as_array().unwrap().len(), 4);
    assert_eq!(run["progress"]["overall"], 1.0, "{run}");
    assert!(run["progress"]["needs_you"]
        .as_str()
        .unwrap()
        .contains("harvest basket"));
    assert!(
        run["reports"]
            .as_array()
            .unwrap()
            .contains(&json!("results")),
        "{run}"
    );
    assert!(
        run["console"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == "ripe"),
        "{run}"
    );
    let results = b
        .client
        .get(format!("{base}/orchard/runs/acme--tiny-1/report/results"))
        .bearer_auth(&b.token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(results.contains("fake results"));
    let bad = b
        .client
        .get(format!("{base}/orchard/runs/..--x/report/results"))
        .bearer_auth(&b.token)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);

    // When it ends: the box is free, the old model comes back, and the owner heard about each stage.
    until("the old model to start again", || {
        backend
            .calls
            .lock()
            .unwrap()
            .contains(&"start Qwen3-8B".to_string())
    })
    .await;
    assert!(state.chips_holder().is_none());
    let titles: Vec<String> = state
        .notifier()
        .since(0)
        .into_iter()
        .map(|e| e.title)
        .collect();
    for want in [
        "🌰 Bringing up Tiny-1",
        "🔭 Tiny-1: survey passed",
        "🌱 Tiny-1: graft passed",
        "🍎 Tiny-1 is ripe",
    ] {
        assert!(
            titles.iter().any(|t| t == want),
            "{want} missing from {titles:?}"
        );
    }

    // Pause, resume and abort go to the run's supervisor.
    let (code, body) = b
        .post(
            "/orchard/runs",
            json!({ "model": "acme/slow-2", "stop_serving": true }),
        )
        .await;
    assert_eq!(code, 202, "{body}");
    b.wait_for("acme--slow-2", "running").await;
    let live = b.run("acme--slow-2").await;
    assert!(
        live["progress"]["title"]
            .as_str()
            .unwrap()
            .contains("Stage"),
        "{live}"
    );
    assert!(live["progress"]["hint"]
        .as_str()
        .unwrap()
        .starts_with("Nothing to do"));
    let (code, body) = b.post("/orchard/runs/acme--slow-2/pause", json!({})).await;
    assert_eq!(code, 202, "{body}");
    b.wait_for("acme--slow-2", "paused").await;
    assert!(
        state.chips_holder().is_some(),
        "a paused run still holds the chips"
    );
    let (code, _) = b.post("/orchard/runs/acme--slow-2/resume", json!({})).await;
    assert_eq!(code, 202);
    b.wait_for("acme--slow-2", "running").await;
    let (code, _) = b.post("/orchard/runs/acme--slow-2/abort", json!({})).await;
    assert_eq!(code, 202);
    b.wait_for("acme--slow-2", "aborted").await;
    until("the chips to be free", || state.chips_holder().is_none()).await;

    // A supervisor that never reads its control file (stuck in a coder boot)
    // is stopped with SIGTERM after the grace period.
    let (code, body) = b
        .post(
            "/orchard/runs",
            json!({ "model": "acme/stuck-3", "stop_serving": true }),
        )
        .await;
    assert_eq!(code, 202, "{body}");
    b.wait_for("acme--stuck-3", "running").await;
    let (code, _) = b.post("/orchard/runs/acme--stuck-3/abort", json!({})).await;
    assert_eq!(code, 202);
    b.wait_for("acme--stuck-3", "aborted").await;
    until("the chips to be free", || state.chips_holder().is_none()).await;

    // tt-orchard's own preflight refuses: the run never gets a ledger.
    let (code, _) = b
        .post(
            "/orchard/runs",
            json!({ "model": "refuse/x", "stop_serving": true }),
        )
        .await;
    assert_eq!(code, 202);
    b.wait_for("refuse--x", "refused").await;
    until("the refusal notification", || {
        state
            .notifier()
            .since(0)
            .iter()
            .any(|e| e.title == "x bring-up didn't start")
    })
    .await;

    // One reset per start.
    assert_eq!(std::fs::read_to_string(&resets).unwrap().lines().count(), 4);
    let list = b.get("/orchard").await;
    assert_eq!(list["runs"].as_array().unwrap().len(), 4, "{list}");
    let _ = std::fs::remove_dir_all(scratch);
}
