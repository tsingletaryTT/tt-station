// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Routines (`src/routines.rs`) through the API: making, checking, changing
//! and removing them; a run asking the person before an action it wasn't
//! given (allowed, declined, or stopped while it waits); a model that
//! refuses; one run at a time; and run ids that can't reach outside the
//! runs directory.
//!
//! The served model is a scripted stub, as in `tests/routines.rs`. Runs are
//! one at a time per process (the routines module keeps the current run in a
//! global), so the tests that run one take `RUNS` first.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, routing::post, Json, Router};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

mod support;

const SERVED: &str = "Qwen/Qwen3-32B";

/// Held by each test that starts a run.
static RUNS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Default)]
struct StubLlm {
    script: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    /// Answer every request with this status and body instead.
    fail: Arc<Mutex<Option<(u16, String)>>>,
}

impl StubLlm {
    async fn serve(&self) -> String {
        async fn completions(
            State(stub): State<StubLlm>,
            Json(body): Json<Value>,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let n = {
                let mut reqs = stub.requests.lock().unwrap();
                reqs.push(body);
                reqs.len()
            };
            if let Some((code, text)) = stub.fail.lock().unwrap().clone() {
                return (axum::http::StatusCode::from_u16(code).unwrap(), text).into_response();
            }
            let message = {
                let script = stub.script.lock().unwrap();
                script.get(n - 1).or(script.last()).cloned().unwrap()
            };
            Json(
                json!({ "choices": [{ "index": 0, "message": message, "finish_reason": "stop" }] }),
            )
            .into_response()
        }
        let router = Router::new()
            .route("/v1/chat/completions", post(completions))
            .with_state(self.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{addr}/v1")
    }
}

struct FakeBackend {
    base_url: String,
    calls: Arc<Mutex<Vec<String>>>,
    serving: Mutex<Option<String>>,
}

impl ServingBackend for FakeBackend {
    fn start_forcing(&self, model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        self.calls.lock().unwrap().push(format!("start:{model}"));
        *self.serving.lock().unwrap() = Some(model.to_string());
        Ok(Endpoint {
            base_url: self.base_url.clone(),
            model: model.to_string(),
            requires_key: false,
        })
    }
    fn stop(&self, _model: &str) -> anyhow::Result<()> {
        self.calls.lock().unwrap().push("stop".to_string());
        *self.serving.lock().unwrap() = None;
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(match &*self.serving.lock().unwrap() {
            Some(m) => ServingStatus::Serving(m.clone()),
            None => ServingStatus::Idle,
        })
    }
    fn list_models(&self) -> anyhow::Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

fn says(content: &str) -> Value {
    json!({ "role": "assistant", "content": content })
}

fn calls(name: &str, args: Value) -> Value {
    json!({ "role": "assistant", "content": null, "tool_calls": [{
        "id": format!("call-{name}"), "type": "function",
        "function": { "name": name, "arguments": args.to_string() } }] })
}

struct Box_ {
    base: String,
    token: String,
    config: std::path::PathBuf,
    llm: StubLlm,
    backend: Arc<Mutex<Vec<String>>>,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(tag: &str) -> Self {
        let llm = StubLlm::default();
        let llm_base = llm.serve().await;
        let backend_calls: Arc<Mutex<Vec<String>>> = Arc::default();
        let backend = Arc::new(FakeBackend {
            base_url: llm_base,
            calls: backend_calls.clone(),
            serving: Mutex::new(None),
        });
        let config =
            std::env::temp_dir().join(format!("tt-routines-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), backend)
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_config_dir(config.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let token = support::pair(&base, &state).await;
        Box_ {
            base,
            token,
            config,
            llm,
            backend: backend_calls,
            c: reqwest::Client::new(),
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .c
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn serve_model(&self) {
        let (status, body) = self
            .send("POST", "/run", Some(json!({ "model": SERVED })))
            .await;
        assert_eq!(status, 200, "{body}");
    }

    async fn make(&self, mut body: Value) -> String {
        if body.get("schedule").is_none() {
            body["schedule"] = json!({ "kind": "manual" });
        }
        let (status, r) = self.send("POST", "/routines", Some(body)).await;
        assert_eq!(status, 200, "{r}");
        r["routine"]["id"].as_str().unwrap().to_string()
    }

    async fn run(&self, id: &str) -> String {
        let (status, started) = self
            .send("POST", &format!("/routines/{id}/run"), None)
            .await;
        assert_eq!(status, 200, "{started}");
        started["run"]["id"].as_str().unwrap().to_string()
    }

    async fn run_when(&self, run_id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let mut run = Value::Null;
        for _ in 0..100 {
            run = self
                .send("GET", &format!("/routines/runs/{run_id}"), None)
                .await
                .1["run"]
                .clone();
            if done(&run) {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the run never got there: {run}");
    }

    /// Until the process-wide "a routine is running" clears.
    async fn idle(&self) {
        for _ in 0..100 {
            if self.send("GET", "/routines", None).await.1["running"].is_null() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("a routine is still running");
    }
}

#[tokio::test]
async fn a_routine_is_checked_before_it_is_saved() {
    let b = Box_::new("check").await;
    for (body, error) in [
        (
            json!({ "name": " ", "goal": "x" }),
            "Give it a name and say what it should do.",
        ),
        (
            json!({ "name": "x", "goal": "" }),
            "Give it a name and say what it should do.",
        ),
        (
            json!({ "name": "x", "goal": "y", "schedule": { "kind": "monthly" } }),
            "Pick when it runs.",
        ),
        (
            json!({ "name": "x", "goal": "y", "schedule": { "kind": "daily" } }),
            "Give a time like 08:30.",
        ),
        (
            json!({ "name": "x", "goal": "y", "schedule": { "kind": "weekdays", "time": "24:00" } }),
            "Give a time like 08:30.",
        ),
        (
            json!({ "name": "x", "goal": "y", "schedule": { "kind": "manual" }, "allowed": ["start_model", "remember"] }),
            "remember isn't an action a routine can take.",
        ),
    ] {
        let (status, r) = b.send("POST", "/routines", Some(body.clone())).await;
        assert_eq!((status, &r["error"]), (400, &json!(error)), "{body}");
    }
    let r =
        b.c.post(format!("{}/routines", b.base))
            .json(&json!({ "name": "x", "goal": "y" }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn a_routine_is_made_changed_and_removed() {
    let b = Box_::new("crud").await;
    let (status, r) = b
        .send(
            "POST",
            "/routines",
            Some(json!({ "name": "  Nightly  ", "goal": " Check the box. ",
                "schedule": { "kind": "daily", "time": "03:15" }, "allowed": ["stop_model"] })),
        )
        .await;
    assert_eq!(status, 200, "{r}");
    let r = &r["routine"];
    let id = r["id"].as_str().unwrap().to_string();
    assert_eq!(r["name"], "Nightly");
    assert_eq!(r["goal"], "Check the box.");
    assert_eq!(r["when"], "Every day at 03:15");
    assert_eq!(r["allowed"], json!(["stop_model"]));
    assert_eq!(r["enabled"], true);
    assert!(r["next_run"].as_u64().unwrap() > 0);
    assert!(r["last"].is_null());

    // Saved privately.
    let saved = b.config.join("routines/routines.json");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&saved).unwrap().permissions().mode() & 0o077,
        0
    );
    let (_, list) = b.send("GET", "/routines", None).await;
    assert!(list["routines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["id"] == id));
    assert_eq!(
        list["actions"],
        json!(["start_model", "stop_model", "download_model", "reset_chips"])
    );

    // Hourly, and turned off: no next run shown.
    let (status, r) = b
        .send(
            "PUT",
            &format!("/routines/{id}"),
            Some(json!({ "name": "Every few hours", "goal": "Check.",
                "schedule": { "kind": "hourly", "every_hours": 6 }, "enabled": false })),
        )
        .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["routine"]["when"], "Every 6 hours");
    assert_eq!(r["routine"]["enabled"], false);
    assert!(r["routine"]["next_run"].is_null());
    assert_eq!(r["routine"]["allowed"], json!([]));
    // Checked the same way as a new one.
    let (status, _) = b
        .send(
            "PUT",
            &format!("/routines/{id}"),
            Some(json!({ "name": "x", "goal": "y", "schedule": { "kind": "manual" }, "allowed": ["rm"] })),
        )
        .await;
    assert_eq!(status, 400);
    let (status, _) = b
        .send(
            "PUT",
            "/routines/no-such-routine",
            Some(json!({ "name": "x", "goal": "y", "schedule": { "kind": "manual" } })),
        )
        .await;
    assert_eq!(status, 404);

    assert_eq!(
        b.send("DELETE", &format!("/routines/{id}"), None).await,
        (200, json!({ "deleted": true }))
    );
    assert_eq!(
        b.send("DELETE", &format!("/routines/{id}"), None).await.0,
        404
    );
    let (_, list) = b.send("GET", "/routines", None).await;
    assert!(!list["routines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["id"] == id));
}

#[tokio::test]
async fn a_routine_without_a_schedule_is_run_by_hand() {
    let b = Box_::new("noschedule").await;
    let (status, r) = b
        .send(
            "POST",
            "/routines",
            Some(json!({ "name": "Ad hoc", "goal": "Check the box." })),
        )
        .await;
    assert_eq!(status, 200, "{r}");
    assert_eq!(r["routine"]["schedule"]["kind"], "manual");
    assert_eq!(r["routine"]["when"], "When you run it");
    assert!(r["routine"]["next_run"].is_null());
}

#[tokio::test]
async fn a_routine_does_not_run_without_a_model_to_think_with() {
    let b = Box_::new("nomodel").await;
    let id = b
        .make(json!({ "name": "Idle check", "goal": "Say hello." }))
        .await;
    let (status, r) = b.send("POST", &format!("/routines/{id}/run"), None).await;
    assert_eq!(status, 409, "{r}");
    assert!(r["error"].as_str().unwrap().contains("No model is serving"));
    assert_eq!(b.send("POST", "/routines/nope/run", None).await.0, 404);
    assert!(b.llm.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_action_it_was_not_given_waits_for_the_person_and_runs_when_allowed() {
    let _one = RUNS.lock().await;
    let b = Box_::new("allow").await;
    b.serve_model().await;
    *b.llm.script.lock().unwrap() = vec![
        calls("start_model", json!({ "model": SERVED })),
        says("# Restarted the model\n\nAll good."),
    ];
    let id = b
        .make(json!({ "name": "Restart", "goal": "Restart the model." }))
        .await;
    let run_id = b.run(&id).await;
    let run = b.run_when(&run_id, |r| r["status"] == "waiting").await;
    assert_eq!(run["pending"]["tool"], "start_model");
    assert_eq!(run["pending"]["summary"], format!("Start {SERVED}"));
    assert_eq!(run["activity"], "Waiting for your OK");
    let (_, approvals) = b.send("GET", "/approvals", None).await;
    assert!(
        approvals["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == format!("routine:{run_id}")),
        "{approvals}"
    );
    let starts_before = b.backend.lock().unwrap().len();

    assert_eq!(
        b.send(
            "POST",
            &format!("/routines/runs/{run_id}/approve"),
            Some(json!({ "allow": true }))
        )
        .await,
        (200, json!({ "ok": true }))
    );
    let run = b.run_when(&run_id, |r| r["finished"].is_u64()).await;
    assert_eq!(run["status"], "done", "{run}");
    assert_eq!(run["report"], "# Restarted the model\n\nAll good.");
    let step = &run["steps"][0];
    assert_eq!(
        (&step["kind"], &step["tool"]),
        (&json!("action"), &json!("start_model"))
    );
    // It ran (and found the model already serving), rather than being declined.
    assert_eq!(
        step["result"],
        format!("{SERVED} is already being served; nothing to do"),
        "{run}"
    );
    assert_eq!(b.backend.lock().unwrap().len(), starts_before);
    // Answered once.
    assert_eq!(
        b.send(
            "POST",
            &format!("/routines/runs/{run_id}/approve"),
            Some(json!({ "allow": true }))
        )
        .await
        .0,
        409
    );
    // The list shows its last run's headline, and its runs.
    let (_, list) = b.send("GET", "/routines", None).await;
    let r = list["routines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["id"] == id)
        .unwrap()
        .clone();
    assert_eq!(r["last"]["headline"], "Restarted the model");
    assert_eq!(r["last"]["status"], "done");
    let (_, runs) = b.send("GET", &format!("/routines/{id}/runs"), None).await;
    assert_eq!(runs["runs"][0]["id"], run_id.as_str());
    assert_eq!(runs["runs"][0]["steps"], 1);
    b.idle().await;
}

#[tokio::test]
async fn a_declined_action_is_not_run_and_the_model_is_told() {
    let _one = RUNS.lock().await;
    let b = Box_::new("decline").await;
    b.serve_model().await;
    *b.llm.script.lock().unwrap() = vec![
        calls("stop_model", json!({})),
        says("Headline: left the model running.\n\nYou said no."),
    ];
    let id = b
        .make(json!({ "name": "Stopper", "goal": "Stop the model.", "allowed": ["start_model"] }))
        .await;
    let run_id = b.run(&id).await;
    b.run_when(&run_id, |r| r["status"] == "waiting").await;
    b.send(
        "POST",
        &format!("/routines/runs/{run_id}/approve"),
        Some(json!({ "allow": false })),
    )
    .await;
    let run = b.run_when(&run_id, |r| r["finished"].is_u64()).await;
    assert_eq!(run["status"], "done", "{run}");
    assert_eq!(run["steps"][0]["ok"], false);
    assert!(run["steps"][0]["result"]
        .as_str()
        .unwrap()
        .starts_with("The person declined."));
    assert!(!b.backend.lock().unwrap().contains(&"stop".to_string()));
    let reqs = b.llm.requests.lock().unwrap().clone();
    let told = reqs[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(told["role"], "tool");
    assert!(told["content"].as_str().unwrap().contains("declined"));
    b.idle().await;
}

#[tokio::test]
async fn stopping_a_run_that_waits_for_the_person_ends_it() {
    let _one = RUNS.lock().await;
    let b = Box_::new("stop").await;
    b.serve_model().await;
    *b.llm.script.lock().unwrap() = vec![
        calls("download_model", json!({ "id": "acme/huge" })),
        says("never"),
    ];
    let id = b
        .make(json!({ "name": "Resetter", "goal": "Download a model." }))
        .await;
    let run_id = b.run(&id).await;
    b.run_when(&run_id, |r| r["status"] == "waiting").await;
    // Only one at a time; and a running routine can't be removed.
    let other = b
        .make(json!({ "name": "Other", "goal": "Anything." }))
        .await;
    let (status, r) = b
        .send("POST", &format!("/routines/{other}/run"), None)
        .await;
    assert_eq!(status, 409);
    assert!(r["error"]
        .as_str()
        .unwrap()
        .starts_with("Another routine is running (Resetter)."));
    assert_eq!(
        b.send("DELETE", &format!("/routines/{id}"), None).await.0,
        409
    );

    assert_eq!(
        b.send("POST", &format!("/routines/runs/{run_id}/stop"), None)
            .await,
        (200, json!({ "stopping": true }))
    );
    let run = b.run_when(&run_id, |r| r["finished"].is_u64()).await;
    assert_eq!(run["status"], "stopped", "{run}");
    assert_eq!(run["report"], "Stopped. Stopped while waiting for your OK.");
    assert!(run["pending"].is_null());
    assert_eq!(b.llm.requests.lock().unwrap().len(), 1);
    // Finished: nothing to stop.
    assert_eq!(
        b.send("POST", &format!("/routines/runs/{run_id}/stop"), None)
            .await
            .0,
        409
    );
    b.idle().await;
    // Its runs go with it.
    assert_eq!(
        b.send("DELETE", &format!("/routines/{id}"), None).await.0,
        200
    );
    assert_eq!(
        b.send("GET", &format!("/routines/runs/{run_id}"), None)
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn a_model_that_refuses_fails_the_run_with_its_reason() {
    let _one = RUNS.lock().await;
    let b = Box_::new("refuse").await;
    b.serve_model().await;
    *b.llm.fail.lock().unwrap() = Some((500, "out of KV cache".into()));
    let id = b.make(json!({ "name": "Doomed", "goal": "Try." })).await;
    let run_id = b.run(&id).await;
    let run = b.run_when(&run_id, |r| r["finished"].is_u64()).await;
    assert_eq!(run["status"], "failed", "{run}");
    let report = run["report"].as_str().unwrap();
    assert!(
        report.starts_with("The model refused (500") && report.contains("out of KV cache"),
        "{report}"
    );
    b.idle().await;
}

#[tokio::test]
async fn bad_tool_calls_are_reported_back_to_the_model_not_run() {
    let _one = RUNS.lock().await;
    let b = Box_::new("badcalls").await;
    b.serve_model().await;
    *b.llm.script.lock().unwrap() = vec![
        json!({ "role": "assistant", "content": "Let me look.", "tool_calls": [
            { "id": "c1", "type": "function", "function": { "name": "box_status", "arguments": "{not json" } },
            { "id": "c2", "type": "function", "function": { "name": "format_disk", "arguments": "{}" } } ] }),
        says(""),
    ];
    let id = b.make(json!({ "name": "Clumsy", "goal": "Look." })).await;
    let run_id = b.run(&id).await;
    let run = b.run_when(&run_id, |r| r["finished"].is_u64()).await;
    // An empty final answer is a failure, not an empty report.
    assert_eq!(run["status"], "failed", "{run}");
    assert_eq!(run["report"], "The model gave no report.");
    let steps = run["steps"].as_array().unwrap();
    assert_eq!(steps[0]["kind"], "note");
    assert_eq!(steps[0]["result"], "Let me look.");
    assert_eq!(steps[1]["ok"], false);
    assert!(steps[1]["result"]
        .as_str()
        .unwrap()
        .contains("weren't valid JSON"));
    assert_eq!(steps[2]["ok"], false);
    assert_eq!(steps[2]["result"], "There's no tool called format_disk.");
    b.idle().await;
}

#[tokio::test]
async fn run_ids_cannot_reach_outside_the_runs_directory() {
    let b = Box_::new("traverse").await;
    // A file next to the routines directory that isn't a run.
    // Any box that has run a routine has its runs directory.
    std::fs::create_dir_all(b.config.join("routines/runs")).unwrap();
    let voice = b.config.join("voice.json");
    std::fs::write(&voice, r#"{"voice":"am_puck","speed":1.0}"#).unwrap();
    for path in [
        "/routines/runs/..%2F..%2Fvoice",
        "/routines/runs/..%2F..%2Fvoice/stop",
    ] {
        let method = if path.ends_with("/stop") {
            "POST"
        } else {
            "GET"
        };
        let (status, _) = b.send(method, path, None).await;
        assert!(status == 404 || status == 409, "{path}: {status}");
    }
    assert!(
        voice.exists(),
        "a run id outside the runs directory moved {} aside",
        voice.display()
    );
    assert_eq!(
        std::fs::read_to_string(&voice).unwrap(),
        r#"{"voice":"am_puck","speed":1.0}"#
    );
}
