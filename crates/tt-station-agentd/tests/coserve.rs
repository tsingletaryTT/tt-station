// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Co-serving (`/coserve`): chat on one P300 card, images on the other. Fake
//! backends record the placement they were pinned to; a stub tt CLI supplies a
//! catalog where only some models fit a single `p300`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{extract::Json as AxJson, routing::post, Router};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::coserve::Companion;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::{Placement, ServingBackend};
use tt_station_agentd::ttcli::TtCli;

const CATALOG: &str = r#"{
  "device": "p300",
  "models": [
    { "name": "Chat-8B", "hf_repo": "org/Chat-8B", "model_type": "llm",
      "devices": { "p300": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "llama3_json" },
                   "p300x2": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "llama3_json" } } },
    { "name": "Big-32B", "hf_repo": "org/Big-32B", "model_type": "llm",
      "devices": { "p300x2": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "hermes" } } },
    { "name": "Flux-S", "hf_repo": "org/Flux-S", "model_type": "image",
      "devices": { "p300": { "status": "COMPLETE", "supported": true },
                   "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Flux-Missing", "hf_repo": "org/Flux-Missing", "model_type": "image",
      "devices": { "p300": { "status": "COMPLETE", "supported": true } } }
  ]
}"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tt-station-coserve-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stub_tt(dir: &Path) -> Vec<String> {
    let catalog = dir.join("catalog.json");
    std::fs::write(&catalog, CATALOG).unwrap();
    let script = dir.join("tt.sh");
    std::fs::write(
        &script,
        format!(
            "[ \"$1\" = \"--json\" ] && shift\ncase \"$1 $2\" in\n  \"model list\") cat '{}';;\n  *) exit 2;;\nesac\n",
            catalog.display()
        ),
    )
    .unwrap();
    vec!["sh".to_string(), script.display().to_string()]
}

/// A complete download: a snapshot holding a weight file.
fn downloaded(hf: &Path, repo: &str) {
    let dir = hf
        .join("hub")
        .join(format!("models--{}", repo.replace('/', "--")));
    std::fs::create_dir_all(dir.join("snapshots/abc")).unwrap();
    std::fs::write(dir.join("snapshots/abc/model.safetensors"), vec![1u8; 4096]).unwrap();
}

async fn stub_media_server() -> String {
    let router = Router::new().route(
        "/v1/images/generations",
        post(
            |headers: axum::http::HeaderMap, AxJson(body): AxJson<Value>| async move {
                AxJson(json!({
                    "images": ["aGk="],
                    "echo": body["prompt"],
                    "guidance": body["guidance_scale"],
                    "auth": headers.get("authorization").and_then(|v| v.to_str().ok()),
                }))
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("{base}/v1")
}

/// Serves after a short delay and records the placement it was pinned to.
struct FakeBackend {
    base_url: String,
    served_id: String,
    placement: Mutex<Option<Placement>>,
    started_with: Mutex<Vec<(String, Option<Placement>)>>,
    serving: Mutex<Option<String>>,
}

impl FakeBackend {
    fn new(base_url: &str, served_id: &str) -> Arc<Self> {
        Arc::new(FakeBackend {
            base_url: base_url.to_string(),
            served_id: served_id.to_string(),
            placement: Mutex::new(None),
            started_with: Mutex::new(Vec::new()),
            serving: Mutex::new(None),
        })
    }
}

impl ServingBackend for FakeBackend {
    fn start_forcing(&self, model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        std::thread::sleep(Duration::from_millis(100));
        let placement = self.placement.lock().unwrap().clone();
        self.started_with
            .lock()
            .unwrap()
            .push((model.to_string(), placement));
        *self.serving.lock().unwrap() = Some(self.served_id.clone());
        Ok(Endpoint {
            base_url: self.base_url.clone(),
            model: self.served_id.clone(),
            requires_key: false,
        })
    }
    fn stop(&self, _model: &str) -> anyhow::Result<()> {
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
    fn set_placement(&self, placement: Option<Placement>) -> bool {
        *self.placement.lock().unwrap() = placement;
        true
    }
    fn placement(&self) -> Option<Placement> {
        self.placement.lock().unwrap().clone()
    }
}

struct Harness {
    client: reqwest::Client,
    base: String,
    token: String,
    main: Arc<FakeBackend>,
    side: Arc<FakeBackend>,
    _scratch: Scratch,
}

impl Harness {
    async fn new() -> Self {
        let scratch = Scratch::new();
        let hf = scratch.0.join("hf");
        downloaded(&hf, "org/Chat-8B");
        downloaded(&hf, "org/Big-32B");
        downloaded(&hf, "org/Flux-S");
        let media = stub_media_server().await;
        let main = FakeBackend::new("http://127.0.0.1:9/v1", "org/Chat-8B");
        let side = FakeBackend::new(&media, "tt-flux-s");
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), main.clone())
            .with_device_mesh(Some("p300x2".into()))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_tt_cli(Arc::new(TtCli::new(stub_tt(&scratch.0), &hf)))
            .with_companion(Companion::new(side.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

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
        Harness {
            client,
            base,
            token: done["token"].as_str().unwrap().to_string(),
            main,
            side,
            _scratch: scratch,
        }
    }

    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}{path}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    /// Poll `GET /coserve` until `done` holds (or fail after a few seconds).
    async fn wait(&self, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let v = self.get("/coserve").await;
            if done(&v) {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out: {v}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

#[tokio::test]
async fn choices_are_the_models_that_fit_half_the_box() {
    let h = Harness::new().await;
    let v = h.get("/coserve").await;
    assert_eq!(v["available"], true, "{v}");
    assert_eq!(v["split"], false);
    assert_eq!(v["layout"], json!({ "chat": "0,1", "image": "2,3", "second": "2,3" }));
    let names = |k: &str| -> Vec<(String, bool)> {
        v["choices"][k]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_string(),
                    c["ready"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(
        names("chat"),
        vec![("Chat-8B".to_string(), true)],
        "Big-32B needs all four chips"
    );
    assert_eq!(
        names("image"),
        vec![
            ("Flux-S".to_string(), true),
            ("Flux-Missing".to_string(), false)
        ]
    );
    assert!(v["choices"]["image"][0]["note"]
        .as_str()
        .unwrap()
        .contains("Known issue"));
    assert!(v["choices"]["chat"][0].get("note").is_none());
}

#[tokio::test]
async fn coserve_pins_each_model_to_its_card_and_routes_images_to_the_companion() {
    let h = Harness::new().await;

    let unauthed = h
        .client
        .post(format!("{}/coserve", h.base))
        .json(&json!({ "chat": "Chat-8B", "image": "Flux-S" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthed.status(), 401);
    let too_big = h
        .post("/coserve", json!({ "chat": "Big-32B", "image": "Flux-S" }))
        .await;
    assert_eq!(too_big.status(), 400);
    assert!(
        too_big.text().await.unwrap().contains("Chat-8B"),
        "names the options"
    );
    let missing = h
        .post(
            "/coserve",
            json!({ "chat": "Chat-8B", "image": "Flux-Missing" }),
        )
        .await;
    assert_eq!(missing.status(), 409);

    let ok = h
        .post(
            "/coserve",
            json!({ "chat": "org/Chat-8B", "image": "Flux-S" }),
        )
        .await;
    assert_eq!(ok.status(), 202);
    let v = h
        .wait(|v| v["chat"]["phase"] == "serving" && v["image"]["phase"] == "serving")
        .await;
    assert_eq!(v["split"], true);
    assert_eq!(v["image"]["model"], "Flux-S");

    let pins = |b: &FakeBackend| b.started_with.lock().unwrap().clone();
    assert_eq!(
        pins(&h.main),
        vec![(
            "Chat-8B".to_string(),
            Some(Placement {
                device: "p300".into(),
                device_ids: "0,1".into()
            })
        )]
    );
    assert_eq!(
        pins(&h.side),
        vec![(
            "Flux-S".to_string(),
            Some(Placement {
                device: "p300".into(),
                device_ids: "2,3".into()
            })
        )]
    );

    // Images go to the companion even though the main slot serves chat.
    let image: Value = h
        .post("/generate/image", json!({ "prompt": "a red fox" }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(image["echo"], "a red fox", "{image}");
    assert_eq!(
        image["guidance"], 3.5,
        "required by the media server; defaulted"
    );
    assert_eq!(image["auth"], "Bearer your-secret-key");
    let serve = h.get("/serve").await;
    assert_eq!(serve["kind"], "chat");
    assert_eq!(serve["split"], true);
    assert_eq!(serve["companion"]["phase"], "serving");

    // An ordinary start takes the whole box back: companion stopped, un-pinned.
    let whole = h.post("/serve", json!({ "model": "Big-32B" })).await;
    assert_eq!(whole.status(), 202);
    let v = h
        .wait(|v| v["chat"]["phase"] == "serving" && v["chat"]["model"] == "Big-32B")
        .await;
    assert_eq!(v["split"], false, "{v}");
    assert_eq!(v["image"]["phase"], "idle");
    assert_eq!(pins(&h.main).last().unwrap().1, None, "whole box again");
    assert!(h.side.serving.lock().unwrap().is_none());
}

#[tokio::test]
async fn stop_stops_both_halves() {
    let h = Harness::new().await;
    assert_eq!(
        h.post("/coserve", json!({ "chat": "Chat-8B", "image": "Flux-S" }))
            .await
            .status(),
        202
    );
    h.wait(|v| v["chat"]["phase"] == "serving" && v["image"]["phase"] == "serving")
        .await;
    let stopped = h.post("/stop", json!({})).await;
    assert!(stopped.status().is_success());
    let v = h.get("/coserve").await;
    assert_eq!(v["split"], false, "{v}");
    assert_eq!(v["image"]["phase"], "idle");
    assert!(h.main.placement().is_none());
}
