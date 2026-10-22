// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Modes (`/modes`): intent-level choices over a real-name catalog, learned
//! load times, and starting a mode.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;
use tt_station_agentd::ttcli::TtCli;

const CATALOG: &str = r#"{
  "device": "p300x2",
  "models": [
    { "name": "Qwen3-32B", "hf_repo": "org/Qwen3-32B", "model_type": "llm",
      "devices": { "p300x2": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "hermes" } } },
    { "name": "Llama-3.1-8B-Instruct", "hf_repo": "org/Llama-3.1-8B-Instruct", "model_type": "llm",
      "devices": { "p300": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "llama3_json" },
                   "p300x2": { "status": "FUNCTIONAL", "supported": true, "tool_call_parser": "llama3_json" } } },
    { "name": "FLUX.1-schnell", "hf_repo": "org/FLUX.1-schnell", "model_type": "image",
      "devices": { "p300": { "status": "COMPLETE", "supported": true },
                   "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Wan2.2-T2V-A14B-Diffusers", "hf_repo": "org/Wan2.2-T2V-A14B-Diffusers", "model_type": "video",
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } }
  ]
}"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tt-station-modes-test-{}-{}",
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

/// Serves after a short delay and records what it started.
struct FakeBackend {
    base_url: String,
    served_id: String,
    started_with: Mutex<Vec<String>>,
    serving: Mutex<Option<String>>,
}

impl FakeBackend {
    fn new(base_url: &str, served_id: &str) -> Arc<Self> {
        Arc::new(FakeBackend {
            base_url: base_url.to_string(),
            served_id: served_id.to_string(),
            started_with: Mutex::new(Vec::new()),
            serving: Mutex::new(None),
        })
    }
}

impl ServingBackend for FakeBackend {
    fn start_forcing(&self, model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        std::thread::sleep(Duration::from_millis(100));
        self.started_with.lock().unwrap().push(model.to_string());
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
}

struct Harness {
    client: reqwest::Client,
    base: String,
    token: String,
    main: Arc<FakeBackend>,
    _scratch: Scratch,
}

impl Harness {
    async fn new() -> Self {
        let scratch = Scratch::new();
        let hf = scratch.0.join("hf");
        downloaded(&hf, "org/Qwen3-32B");
        downloaded(&hf, "org/Llama-3.1-8B-Instruct");
        downloaded(&hf, "org/FLUX.1-schnell");
        let main = FakeBackend::new("http://127.0.0.1:9/v1", "Qwen/Qwen3-32B");
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), main.clone())
            .with_device_mesh(Some("p300x2".into()))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_tt_cli(Arc::new(TtCli::new(stub_tt(&scratch.0), &hf)));
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
            _scratch: scratch,
        }
    }

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

    async fn post(&self, path: &str, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn modes_pick_models_report_downloads_start_and_learn_load_times() {
    let h = Harness::new().await;
    let v = h.get("/modes").await;
    let modes = v["modes"].as_array().unwrap();
    let get = |id: &str| {
        modes
            .iter()
            .find(|m| m["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(get("chat")["models"], json!(["Qwen3-32B"]), "{v}");
    assert_eq!(get("chat")["ready"], true);
    assert_eq!(get("images")["models"], json!(["FLUX.1-schnell"]));
    assert_eq!(get("transcribe"), Value::Null, "not in this catalog");
    for gone in ["chat_images", "video"] {
        assert_eq!(get(gone), Value::Null, "{gone} is not a mode");
    }
    assert_eq!(get("chat")["eta_s"], Value::Null, "never loaded yet");

    let video = h.post("/modes/video", json!({})).await;
    assert_eq!(video.status(), 409);
    assert!(video
        .text()
        .await
        .unwrap()
        .contains("no \\\"video\\\" mode"));

    let chat = h.post("/modes/chat", json!({})).await;
    assert_eq!(chat.status(), 202);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let v = h.get("/modes").await;
        let chat = v["modes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "chat")
            .cloned()
            .unwrap();
        if chat["active"] == true {
            // A real (fake, 100 ms) load rounds to 0 s, which isn't recorded;
            // the started model is what matters here.
            break;
        }
        assert!(Instant::now() < deadline, "{v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.main.started_with.lock().unwrap()[0], "Qwen3-32B");
    let again = h.post("/modes/chat", json!({})).await;
    assert_eq!(again.status(), 200, "already active");
}
