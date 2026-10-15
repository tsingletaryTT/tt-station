// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Integration tests for the tt-CLI model-manager routes: `GET /tt/models`,
//! `GET /tt/pulls`, `POST /tt/pull`, `POST /tt/pull/cancel`.
//!
//! The official `tt` is replaced by a stub shell script, run through `sh` (so
//! nothing exec's a freshly written file -- that races ETXTBSY under parallel
//! tests). The stub serves a canned catalog and "downloads" by writing a file
//! into a temp HF cache, so no network and no hardware are involved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;
use tt_station_agentd::ttcli::TtCli;

const CATALOG: &str = r#"{
  "device": "p300x2",
  "models": [
    { "name": "Qwen3-32B", "hf_repo": "Qwen/Qwen3-32B", "model_type": "llm", "cached": true,
      "cache_size_bytes": 1000, "min_disk_gb": 1, "param_count": 32,
      "devices": { "p300x2": { "engines": ["vLLM"], "status": "FUNCTIONAL", "supported": true,
                               "tool_call_parser": "hermes" } } },
    { "name": "Small-1B", "hf_repo": "org/Small-1B", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 0.001,
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Broken-1B", "hf_repo": "org/Broken-1B", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 0.001,
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Gated-1B", "hf_repo": "org/Gated-1B", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 0.001,
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Slow-7B", "hf_repo": "org/Slow-7B", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 0.001,
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Huge-999B", "hf_repo": "org/Huge-999B", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 1000000000,
      "devices": { "p300x2": { "status": "COMPLETE", "supported": true } } },
    { "name": "Other-Mesh", "hf_repo": "org/Other-Mesh", "model_type": "llm", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 0.001,
      "devices": { "n150": { "status": "COMPLETE", "supported": true } } },
    { "name": "whisper-large-v3", "hf_repo": "openai/whisper-large-v3", "model_type": "audio", "cached": false,
      "cache_size_bytes": 0, "min_disk_gb": 15,
      "devices": { "p300x2": { "engines": ["media"], "status": "COMPLETE", "supported": true } } }
  ]
}"#;

/// A scratch dir unique per test (a counter, not a clock: these run as
/// parallel threads of one process), removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tt-station-ttcli-test-{}-{}",
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

/// Write the stub `tt` and return how to invoke it.
fn stub_tt(dir: &Path, hf: &Path) -> Vec<String> {
    let catalog = dir.join("catalog.json");
    std::fs::write(&catalog, CATALOG).unwrap();
    let script = dir.join("tt.sh");
    std::fs::write(
        &script,
        format!(
            r#"[ "$1" = "--json" ] && shift
case "$1" in
  --version) echo "tt 9.9.9"; exit 0;;
esac
case "$1 $2" in
  "model list") cat '{catalog}';;
  "model pull")
    case "$3" in
      Broken-1B) echo "Downloading..." ; echo "401: access to org/Broken-1B is gated" >&2; exit 3;;
      Gated-1B) printf '{{"error": {{"what": "Model Gated-1B is gated on HuggingFace.", "next_step": "Accept the license at https://huggingface.co/org/Gated-1B and log in."}}}}\n'; exit 1;;
      Slow-7B) sleep 30;;
      *) mkdir -p '{hf}/hub/models--org--'"$3"'/blobs'
         head -c 2048 /dev/zero > '{hf}/hub/models--org--'"$3"'/blobs/weights';;
    esac;;
  *) echo "stub tt: unexpected $*" >&2; exit 2;;
esac
"#,
            catalog = catalog.display(),
            hf = hf.display(),
        ),
    )
    .unwrap();
    vec!["sh".to_string(), script.display().to_string()]
}

struct IdleBackend;

impl ServingBackend for IdleBackend {
    fn start_forcing(&self, _model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        anyhow::bail!("not used in these tests")
    }
    fn stop(&self, _model: &str) -> anyhow::Result<()> {
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

struct Harness {
    client: reqwest::Client,
    base: String,
    token: String,
    _scratch: Scratch,
}

impl Harness {
    async fn new(with_tt: bool) -> Self {
        let scratch = Scratch::new();
        let hf = scratch.0.join("hf");
        // Qwen3-32B fully downloaded (a weight file in its snapshot); the rest absent.
        let qwen = hf.join("hub/models--Qwen--Qwen3-32B/snapshots/rev1");
        std::fs::create_dir_all(&qwen).unwrap();
        std::fs::write(qwen.join("model.safetensors"), [0u8; 64]).unwrap();
        let mut state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(IdleBackend))
            .with_device_mesh(Some("p300x2".into()));
        if with_tt {
            let tt = TtCli::new(stub_tt(&scratch.0, &hf), &hf).with_log_dir(scratch.0.join("logs"));
            state = state.with_tt_cli(Arc::new(tt));
        }
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
            _scratch: scratch,
        }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let r = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, model: &str) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&json!({ "model": model }))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    /// Poll `/tt/pulls` until `model`'s job leaves `running` (or time out).
    async fn wait_for_pull(&self, model: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (_, body) = self.get("/tt/pulls").await;
            let job = body["pulls"]
                .as_array()
                .unwrap()
                .iter()
                .find(|j| j["model"] == model)
                .cloned()
                .expect("job listed");
            if job["state"] != "running" || Instant::now() > deadline {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test]
async fn routes_answer_409_without_the_tt_cli() {
    let h = Harness::new(false).await;
    assert_eq!(h.get("/tt/models").await.0, 409);
    assert_eq!(h.get("/tt/pulls").await.0, 409);
    assert_eq!(h.post("/tt/pull", "Qwen3-32B").await.0, 409);
}

#[tokio::test]
async fn catalog_lists_this_mesh_with_cached_state_and_disk() {
    let h = Harness::new(true).await;
    let (status, body) = h.get("/tt/models").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["device"], "p300x2");
    assert!(body["disk_free_bytes"].as_u64().is_some_and(|b| b > 0));
    let models = body["models"].as_array().unwrap();
    assert_eq!(models.len(), 8);
    let qwen = models.iter().find(|m| m["name"] == "Qwen3-32B").unwrap();
    assert_eq!(qwen["cached"], true);
    assert_eq!(qwen["supported"], true);
    assert_eq!(qwen["tool_call_parser"], "hermes");
    let other = models.iter().find(|m| m["name"] == "Other-Mesh").unwrap();
    assert_eq!(other["supported"], false);
    // The agent's own measurement, not tt's `cached` flag.
    assert_eq!(qwen["weights"], "complete");
    // Disk usage, so at least the bytes written (rounded up to filesystem blocks).
    assert!(qwen["downloaded_bytes"].as_u64().unwrap() >= 64, "{qwen}");
    let small = models.iter().find(|m| m["name"] == "Small-1B").unwrap();
    assert_eq!(small["weights"], "missing");
    let whisper = models
        .iter()
        .find(|m| m["name"] == "whisper-large-v3")
        .unwrap();
    assert_eq!(whisper["weights"], "container");
}

#[tokio::test]
async fn container_fetched_models_need_no_pull() {
    let h = Harness::new(true).await;
    let (s, b) = h.post("/tt/pull", "whisper-large-v3").await;
    assert_eq!((s, b["status"].as_str()), (200, Some("not_needed")), "{b}");
}

#[tokio::test]
async fn starting_a_model_that_is_not_downloaded_is_refused() {
    let h = Harness::new(true).await;
    for route in ["/run", "/serve"] {
        let (s, b) = h.post(route, "Small-1B").await;
        assert_eq!(s, 409, "{route}: {b}");
        assert!(
            b["error"].as_str().unwrap().contains("isn't downloaded"),
            "{route}: {b}"
        );
    }
    // Speech models bring their own weights: not refused for that reason. (The
    // stub backend can't actually serve, so /serve just accepts the request.)
    let (s, b) = h.post("/serve", "whisper-large-v3").await;
    assert_eq!(s, 202, "{b}");
}

#[tokio::test]
async fn pull_needs_a_paired_client() {
    let h = Harness::new(true).await;
    let r = h
        .client
        .post(format!("{}/tt/pull", h.base))
        .json(&json!({ "model": "Small-1B" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}

#[tokio::test]
async fn pull_refuses_what_it_should() {
    let h = Harness::new(true).await;
    let (s, b) = h.post("/tt/pull", "Qwen3-32B").await;
    assert_eq!((s, b["status"].as_str()), (200, Some("already_downloaded")));
    assert_eq!(h.post("/tt/pull", "nope-0B").await.0, 404);
    let (s, b) = h.post("/tt/pull", "Other-Mesh").await;
    assert_eq!(s, 400, "{b}");
    let (s, b) = h.post("/tt/pull", "Huge-999B").await;
    assert_eq!(s, 409, "{b}");
    assert!(b["error"].as_str().unwrap().contains("free disk"));
}

#[tokio::test]
async fn pull_downloads_in_the_background_and_reports_bytes() {
    let h = Harness::new(true).await;
    // By HF repo id: resolved to the catalog name the job is keyed by.
    let (s, b) = h.post("/tt/pull", "org/Small-1B").await;
    assert_eq!(
        (s, b["status"].as_str(), b["model"].as_str()),
        (202, Some("started"), Some("Small-1B"))
    );
    let job = h.wait_for_pull("Small-1B").await;
    assert_eq!(job["state"], "done", "{job}");
    assert!(job["bytes_on_disk"].as_u64().unwrap() >= 2048, "{job}");
    assert!(job["finished_at"].as_u64().is_some());
}

#[tokio::test]
async fn failed_pull_reports_what_tt_said() {
    let h = Harness::new(true).await;
    assert_eq!(h.post("/tt/pull", "Broken-1B").await.0, 202);
    let job = h.wait_for_pull("Broken-1B").await;
    assert_eq!(job["state"], "failed", "{job}");
    assert!(job["error"].as_str().unwrap().contains("gated"), "{job}");
}

#[tokio::test]
async fn gated_pull_says_what_to_do_and_links_the_license() {
    let h = Harness::new(true).await;
    assert_eq!(h.post("/tt/pull", "Gated-1B").await.0, 202);
    let job = h.wait_for_pull("Gated-1B").await;
    assert_eq!(job["state"], "failed", "{job}");
    assert!(
        job["error"]
            .as_str()
            .unwrap()
            .contains("Accept its license"),
        "{job}"
    );
    assert_eq!(job["action_url"], "https://huggingface.co/org/Gated-1B");
}

#[tokio::test]
async fn running_pull_refuses_a_duplicate_and_can_be_cancelled() {
    let h = Harness::new(true).await;
    assert_eq!(h.post("/tt/pull", "Slow-7B").await.0, 202);
    assert_eq!(h.post("/tt/pull", "Slow-7B").await.0, 409);
    let (s, b) = h.post("/tt/pull/cancel", "org/Slow-7B").await;
    assert_eq!((s, b["status"].as_str()), (200, Some("cancelled")));
    let job = h.wait_for_pull("Slow-7B").await;
    assert_eq!(job["state"], "cancelled", "{job}");
    assert_eq!(h.post("/tt/pull/cancel", "Slow-7B").await.0, 404);
}
