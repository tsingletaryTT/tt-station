// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Integration tests for the serving lifecycle (`GET`/`POST /serve`), the media
//! generation proxies (`/generate/*`), and the assistant's refusal to chat with a
//! non-chat model. A fake backend "serves" by pointing at a stub tt-media-server.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::Json as AxJson,
    http::header,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

/// A stub tt-media-server: images, video jobs, and FastAPI-style errors.
async fn stub_media_server() -> String {
    let router = Router::new()
        .route(
            "/v1/images/generations",
            post(|AxJson(body): AxJson<Value>| async move {
                if body["prompt"] == "fail" {
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        AxJson(json!({ "detail": "boom" })),
                    )
                        .into_response();
                }
                AxJson(
                    json!({ "images": ["aGk="], "generation_time": 1.5, "echo": body["prompt"] }),
                )
                .into_response()
            }),
        )
        .route(
            "/v1/videos/generations",
            post(|| async { AxJson(json!({ "id": "job1", "status": "queued" })) }),
        )
        .route(
            "/v1/videos/generations/job1",
            get(|| async { AxJson(json!({ "id": "job1", "status": "completed" })) }),
        )
        .route(
            "/v1/videos/generations/job1/download",
            get(|| async { ([(header::CONTENT_TYPE, "video/mp4")], "MP4DATA").into_response() }),
        )
        .route(
            "/v1/videos/jobs",
            get(|| async { AxJson(json!([{ "id": "job1", "status": "completed" }])) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("{base}/v1")
}

/// Starts after `delay`, then reports `served_id` (or fails with `error`).
struct FakeBackend {
    base_url: String,
    served_id: String,
    delay: Duration,
    error: Option<String>,
    serving: Mutex<Option<String>>,
}

impl ServingBackend for FakeBackend {
    fn start_forcing(&self, _model: &str, _force: bool) -> anyhow::Result<Endpoint> {
        std::thread::sleep(self.delay);
        if let Some(e) = &self.error {
            anyhow::bail!("{e}");
        }
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
}

impl Harness {
    async fn new(served_id: &str, delay_ms: u64, error: Option<&str>) -> Self {
        let media = stub_media_server().await;
        let backend: Arc<dyn ServingBackend> = Arc::new(FakeBackend {
            base_url: media,
            served_id: served_id.to_string(),
            delay: Duration::from_millis(delay_ms),
            error: error.map(str::to_string),
            serving: Mutex::new(None),
        });
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), backend)
            .with_device_mesh(Some("p300x2".into()))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000);
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

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    /// Poll `GET /serve` until its phase is `want` (or time out and return the last).
    async fn wait_phase(&self, want: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, body) = self.get("/serve").await;
            if body["phase"] == want || Instant::now() > deadline {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn serve(&self, model: &str) {
        let (s, b) = self.post("/serve", json!({ "model": model })).await;
        assert_eq!(s, 202, "{b}");
        assert_eq!(self.wait_phase("serving").await["phase"], "serving");
    }
}

#[tokio::test]
async fn serve_reports_starting_then_serving_with_its_kind() {
    let h = Harness::new("tt-flux.1-schnell", 300, None).await;
    let (_, idle) = h.get("/serve").await;
    assert_eq!(idle["phase"], "idle");

    let (s, b) = h.post("/serve", json!({ "model": "FLUX.1-schnell" })).await;
    assert_eq!((s, b["status"].as_str()), (202, Some("starting")));
    let (_, starting) = h.get("/serve").await;
    assert_eq!(starting["phase"], "starting");
    assert_eq!(starting["model"], "FLUX.1-schnell");

    let serving = h.wait_phase("serving").await;
    assert_eq!(serving["phase"], "serving", "{serving}");
    assert_eq!(serving["served_id"], "tt-flux.1-schnell");
    assert_eq!(serving["kind"], "image");
}

#[tokio::test]
async fn a_second_start_while_one_is_in_flight_is_refused() {
    let h = Harness::new("Qwen/Qwen3-32B", 400, None).await;
    assert_eq!(
        h.post("/serve", json!({ "model": "Qwen3-32B" })).await.0,
        202
    );
    let (s, b) = h
        .post("/serve", json!({ "model": "Llama-3.1-8B-Instruct" }))
        .await;
    assert_eq!(s, 409, "{b}");
    assert!(b["error"].as_str().unwrap().contains("already starting"));
}

#[tokio::test]
async fn a_failed_start_says_why() {
    let h = Harness::new("x", 50, Some("container exited: out of device memory")).await;
    assert_eq!(
        h.post("/serve", json!({ "model": "Huge-70B" })).await.0,
        202
    );
    let failed = h.wait_phase("failed").await;
    assert_eq!(failed["phase"], "failed", "{failed}");
    assert_eq!(failed["model"], "Huge-70B");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains("out of device memory"));
}

#[tokio::test]
async fn stop_returns_to_idle() {
    let h = Harness::new("Qwen/Qwen3-32B", 10, None).await;
    h.serve("Qwen3-32B").await;
    assert_eq!(h.post("/stop", json!({})).await.0, 200);
    assert_eq!(h.get("/serve").await.1["phase"], "idle");
}

/// The bug this fixes: a phone's `/run` request dies when the app is
/// backgrounded mid-load, and the model used to come up with the agent still
/// saying `idle`.
#[tokio::test]
async fn run_records_its_outcome_even_if_the_client_hangs_up() {
    let h = Harness::new("Qwen/Qwen3-32B", 300, None).await;
    let impatient = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap();
    let r = impatient
        .post(format!("{}/run", h.base))
        .bearer_auth(&h.token)
        .json(&json!({ "model": "Qwen3-32B" }))
        .send()
        .await;
    assert!(r.is_err(), "the client gave up before the model was ready");
    let serving = h.wait_phase("serving").await;
    assert_eq!(serving["phase"], "serving", "{serving}");
    let (_, status) = h.get("/status").await;
    assert_eq!(status["status"], "serving:Qwen/Qwen3-32B");
}

#[tokio::test]
async fn image_generation_is_proxied_to_the_media_server() {
    let h = Harness::new("tt-flux.1-schnell", 10, None).await;
    h.serve("FLUX.1-schnell").await;
    let (s, b) = h
        .post(
            "/generate/image",
            json!({ "prompt": "a quiet box in space", "num_inference_steps": 4 }),
        )
        .await;
    assert_eq!(s, 200, "{b}");
    assert_eq!(b["images"][0], "aGk=");
    assert_eq!(b["echo"], "a quiet box in space");
    // FastAPI's `detail` comes back as this API's `error`.
    let (s, b) = h.post("/generate/image", json!({ "prompt": "fail" })).await;
    assert_eq!((s, b["error"].as_str()), (500, Some("boom")));
}

#[tokio::test]
async fn generation_is_refused_for_the_wrong_kind_of_model() {
    let h = Harness::new("Qwen/Qwen3-32B", 10, None).await;
    let (s, b) = h.post("/generate/image", json!({ "prompt": "x" })).await;
    assert_eq!(s, 409, "nothing serving: {b}");
    h.serve("Qwen3-32B").await;
    let (s, b) = h.post("/generate/image", json!({ "prompt": "x" })).await;
    assert_eq!(s, 409, "{b}");
    assert!(b["error"].as_str().unwrap().contains("not image"), "{b}");
    assert_eq!(
        h.post("/generate/video", json!({ "prompt": "x" })).await.0,
        409
    );
}

#[tokio::test]
async fn video_jobs_submit_poll_and_download_through_the_agent() {
    let h = Harness::new(
        "/home/container_app_user/cache_root/Wan2.2-T2V-A14B-Diffusers",
        10,
        None,
    )
    .await;
    h.serve("Wan2.2-T2V-A14B-Diffusers").await;
    assert_eq!(h.get("/serve").await.1["kind"], "video");

    let (s, job) = h
        .post(
            "/generate/video",
            json!({ "prompt": "waves", "num_inference_steps": 12 }),
        )
        .await;
    assert_eq!((s, job["id"].as_str()), (200, Some("job1")), "{job}");
    let (s, status) = h.get("/generate/video/job1").await;
    assert_eq!((s, status["status"].as_str()), (200, Some("completed")));
    let (s, jobs) = h.get("/generate/videos").await;
    assert_eq!((s, jobs[0]["id"].as_str()), (200, Some("job1")));

    let r = h
        .client
        .get(format!("{}/generate/video/job1/download", h.base))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "video/mp4");
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"MP4DATA");
}

#[tokio::test]
async fn the_assistant_says_plainly_when_a_non_chat_model_is_serving() {
    let h = Harness::new("tt-flux.1-schnell", 10, None).await;
    h.serve("FLUX.1-schnell").await;
    let (s, b) = h
        .post(
            "/assistant",
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
        )
        .await;
    assert_eq!(s, 409, "{b}");
    assert!(
        b["error"].as_str().unwrap().contains("needs a chat model"),
        "{b}"
    );
}

#[tokio::test]
async fn generation_and_serve_need_a_paired_client() {
    let h = Harness::new("tt-flux.1-schnell", 10, None).await;
    for path in ["/serve", "/generate/image", "/generate/video"] {
        let r = h
            .client
            .post(format!("{}{path}", h.base))
            .json(&json!({ "model": "x", "prompt": "x" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401, "{path}");
    }
    // Reading the lifecycle is open, like `/status`.
    let r = h
        .client
        .get(format!("{}/serve", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
}
