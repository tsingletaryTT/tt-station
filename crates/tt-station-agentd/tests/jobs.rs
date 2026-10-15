// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The job queue end to end: upload a 10-minute recording, the runner cuts it
//! into Whisper-sized chunks, a stub Whisper transcribes each, and the stitched
//! transcript lands in the job, the owner's documents, and a notification.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{extract::Multipart, routing::post, Json as AxJson, Router};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

struct Idle;

impl ServingBackend for Idle {
    fn start_forcing(&self, _: &str, _: bool) -> anyhow::Result<Endpoint> {
        anyhow::bail!("unused")
    }
    fn stop(&self, _: &str) -> anyhow::Result<()> {
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

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// Stub Whisper: answers chunk n with "Part n." and records upload sizes.
async fn stub_whisper(calls: Arc<AtomicUsize>, sizes: Arc<Mutex<Vec<usize>>>) -> String {
    let router = Router::new().route(
        "/v1/audio/transcriptions",
        post(move |mut form: Multipart| {
            let calls = calls.clone();
            let sizes = sizes.clone();
            async move {
                let mut format = String::new();
                while let Some(field) = form.next_field().await.unwrap() {
                    match field.name() {
                        Some("file") => {
                            let len = field.bytes().await.unwrap().len();
                            sizes.lock().unwrap().push(len);
                        }
                        Some("response_format") => format = field.text().await.unwrap(),
                        _ => {}
                    }
                }
                assert_eq!(format, "verbose_json");
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                AxJson(json!({ "segments": [{ "id": 0, "speaker": "SPEAKER_00", "start_time": 1.0, "end_time": 3.0, "text": format!(" Part {n}.") }] }))
            }
        }),
    );
    let router = router.layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024));
    format!("{}/v1", serve(router).await)
}

#[tokio::test]
async fn a_long_recording_is_chunked_transcribed_stitched_and_filed() {
    // A 10-minute tone with no pauses: cut every 55 s into eleven chunks.
    let scratch = std::env::temp_dir().join(format!("tt-jobs-test-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    let audio = scratch.join("meeting.mp3");
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=600",
        ])
        .arg(&audio)
        .status()
        .unwrap();
    assert!(made.success(), "needs ffmpeg");

    let calls = Arc::new(AtomicUsize::new(0));
    let sizes: Arc<Mutex<Vec<usize>>> = Arc::default();
    let whisper = stub_whisper(calls.clone(), sizes.clone()).await;
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle))
        .with_jobs(tt_station_agentd::jobs::Jobs::open(scratch.join("jobs")))
        .with_docs(tt_station_agentd::docs::DocStore::open(
            scratch.join("docs"),
        ));
    state.adopt_serving(Endpoint {
        base_url: whisper,
        model: "openai/whisper-large-v3".into(),
        requires_key: false,
    });
    let base = serve(app(state.clone())).await;
    tokio::spawn(tt_station_agentd::jobs::run_forever(state.clone()));

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
    let token = done["token"].as_str().unwrap().to_string();

    let anon = client
        .post(format!("{base}/jobs/transcribe?name=x.mp3"))
        .body(vec![1u8])
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let created: Value = client
        .post(format!("{base}/jobs/transcribe?name=Team%20sync.mp3"))
        .bearer_auth(&token)
        .body(std::fs::read(&audio).unwrap())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["status"], "queued");

    let deadline = Instant::now() + Duration::from_secs(60);
    let job = loop {
        let list: Value = client
            .get(format!("{base}/jobs"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let job = list["jobs"][0].clone();
        if job["status"] == "done" || job["status"] == "failed" {
            break job;
        }
        assert!(Instant::now() < deadline, "{job}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(job["status"], "done", "{job}");
    assert_eq!(job["total"], 11, "a pause-free tone is cut every 55 s");
    assert_eq!(job["done"], 11);
    assert!((job["duration_s"].as_f64().unwrap() - 600.0).abs() < 1.0);
    assert_eq!(calls.load(Ordering::SeqCst), 11);
    // 55 s of 16 kHz mono 16-bit PCM is ~1.8 MB.
    assert!(sizes.lock().unwrap().iter().all(|s| *s < 2_000_000));

    let text = client
        .get(format!("{base}/jobs/{id}/result"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        text,
        (0..11)
            .map(|i| {
                let t = i * 55 + 1;
                format!("[00:{:02}:{:02}] Part {}.", t / 60, t % 60, i + 1)
            })
            .collect::<Vec<_>>()
            .join("\n")
    );
    let srt = client
        .get(format!("{base}/jobs/{id}/result?format=srt"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(srt.contains("00:00:56,000 --> 00:00:58,000"), "{srt}");

    let docs: Value = client
        .get(format!("{base}/docs"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(docs["documents"][0]["name"], "Transcript: Team sync.mp3");
    let events = state.notifier().since(0);
    assert!(events
        .iter()
        .any(|e| e.kind == "job_done" && e.title == "Transcript ready: Team sync.mp3"));

    let deleted = client
        .delete(format!("{base}/jobs/{id}"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let _ = std::fs::remove_dir_all(scratch);
}
