// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The job queue end to end: upload a 10-minute recording, the runner cuts it
//! into Whisper-sized chunks, a stub Whisper transcribes each, and the stitched
//! transcript lands in the job, the Library (asked for), and a notification;
//! the recording is deleted. Also: a restart resumes at the next chunk, and
//! members see only their own jobs and the shared ones.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{extract::Multipart, routing::post, Json as AxJson, Router};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

mod support;

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
        .with_config_dir(scratch.clone())
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
        .post(format!(
            "{base}/jobs/transcribe?name=Team%20sync.mp3&documents=true"
        ))
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
    // The recording was only needed for the transcript; the progress note too.
    let folder = scratch.join("jobs").join(&id);
    assert!(!folder.join("input.mp3").exists(), "audio deleted");
    assert!(!folder.join("progress.json").exists());
    assert!(folder.join("transcript.json").exists());
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

fn tone(path: &std::path::Path, seconds: u32) {
    let made = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("sine=frequency=440:duration={seconds}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(made.success(), "needs ffmpeg");
}

#[tokio::test]
async fn an_interrupted_transcription_resumes_at_the_next_chunk() {
    use tt_station_agentd::jobs::{JobStatus, Jobs, NewJob, Progress, Segment, When};
    let scratch = tempfile::tempdir().unwrap();
    let id = "abcdef012345";
    let folder = scratch.path().join("jobs").join(id);
    std::fs::create_dir_all(&folder).unwrap();
    // Three minutes of tone: pieces at 0, 55, 110 and 165 s.
    tone(&folder.join("input.mp3"), 180);
    {
        // The agent that went away: two chunks done, saved as it went.
        let before = Jobs::open(scratch.path().join("jobs"));
        before.add_with(
            id.into(),
            "transcribe",
            "Standup.mp3",
            When::Now,
            NewJob {
                keep_audio: Some(true),
                ..NewJob::default()
            },
        );
        before.update(id, |j| {
            j.status = JobStatus::Running;
            j.total = 4;
            j.done = 2;
        });
        let seg = |t: f64, text: &str| Segment {
            start: t,
            end: t + 1.0,
            text: text.into(),
        };
        let progress = Progress {
            offsets: vec![0.0, 55.0, 110.0, 165.0],
            done: 2,
            segments: vec![seg(1.0, "Earlier one."), seg(56.0, "Earlier two.")],
        };
        std::fs::write(
            folder.join("progress.json"),
            serde_json::to_vec(&progress).unwrap(),
        )
        .unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let whisper = stub_whisper(calls.clone(), Arc::default()).await;
    let jobs = Jobs::open(scratch.path().join("jobs"));
    let back = jobs.get(id).unwrap();
    assert_eq!((back.status, back.done), (JobStatus::Queued, 2));
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle))
        .with_config_dir(scratch.path().to_path_buf())
        .with_jobs(jobs);
    state.adopt_serving(Endpoint {
        base_url: whisper,
        model: "openai/whisper-large-v3".into(),
        requires_key: false,
    });
    tokio::spawn(tt_station_agentd::jobs::run_forever(state.clone()));
    let deadline = Instant::now() + Duration::from_secs(60);
    let job = loop {
        let job = state.jobs().unwrap().get(id).unwrap();
        if matches!(job.status, JobStatus::Done | JobStatus::Failed) {
            break job;
        }
        assert!(Instant::now() < deadline, "{job:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    };
    assert_eq!(job.status, JobStatus::Done, "{job:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "only the two chunks left");
    assert_eq!((job.done, job.total), (4, 4));
    let text = std::fs::read_to_string(folder.join("transcript.txt")).unwrap();
    assert_eq!(
        text,
        "[00:00:01] Earlier one.\n[00:00:56] Earlier two.\n[00:01:51] Part 1.\n[00:02:46] Part 2."
    );
    assert!(folder.join("input.mp3").exists(), "kept, as asked");
    assert!(!folder.join("progress.json").exists());
}

#[tokio::test]
async fn members_see_their_own_jobs_and_shared_ones_owners_see_all() {
    let scratch = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle))
        .with_config_dir(scratch.path().to_path_buf())
        .with_jobs(tt_station_agentd::jobs::Jobs::open(
            scratch.path().join("jobs"),
        ));
    let base = serve(app(state.clone())).await;
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    let other = support::pair(&base, &state).await;
    let client = reqwest::Client::new();
    let upload = |token: String, name: &'static str| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let v: Value = client
                .post(format!("{base}/jobs/transcribe?name={name}"))
                .bearer_auth(token)
                .body(vec![1u8; 16])
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["id"].as_str().unwrap().to_string()
        }
    };
    let ids = |token: String| {
        let (client, base) = (client.clone(), base.clone());
        async move {
            let v: Value = client
                .get(format!("{base}/jobs"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let mut ids: Vec<String> = v["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|j| j["id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        }
    };
    let mine = upload(member.clone(), "mine.m4a").await;
    let owners = upload(owner.clone(), "owners.m4a").await;
    let mut both = vec![mine.clone(), owners.clone()];
    both.sort();
    assert_eq!(ids(owner.clone()).await, both, "owners see every job");
    assert_eq!(ids(member.clone()).await, vec![mine.clone()]);
    assert!(ids(other.clone()).await.is_empty());
    // Someone else's job is "no such job", for results and deletes alike.
    let res = client
        .get(format!("{base}/jobs/{owners}/result"))
        .bearer_auth(&member)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = client
        .delete(format!("{base}/jobs/{mine}"))
        .bearer_auth(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    // Only the starter (or an owner) shares it.
    let res = client
        .post(format!("{base}/jobs/{mine}/share"))
        .bearer_auth(&other)
        .json(&json!({ "shared": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let res = client
        .post(format!("{base}/jobs/{mine}/share"))
        .bearer_auth(&member)
        .json(&json!({ "shared": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(ids(other.clone()).await, vec![mine.clone()]);
    let job = state.jobs().unwrap().get(&mine).unwrap();
    assert!(job.started_by.is_some() && job.shared);
}
