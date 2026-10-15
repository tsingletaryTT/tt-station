// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The box's voice: natural speech and fast listening on the CPU, so talking
//! to the box works while the chips run the chat model.
//!
//! - **Speaking**: Kokoro-82M (Apache-2.0) via Kokoro-FastAPI, in the
//!   `tt-voice-kokoro` container. Dozens of voices, faster than real time on
//!   the Ryzen.
//! - **Listening**: faster-whisper via Speaches, in `tt-voice-ears`, for the
//!   desktop app (the phone listens on-device).
//!
//! Both listen only on the box itself; clients reach them through the agent.
//!
//! - `GET /voice`: whether it's set up, the voices, the chosen voice and speed.
//! - `POST /voice/setup`: install and start both (in the background).
//! - `PUT /voice` `{voice?, speed?}`: the box's voice.
//! - `POST /voice/speak` `{text, voice?, speed?, format?}`: audio
//!   (`mp3` by default; `pcm` is 24 kHz 16-bit mono, for gapless playback).
//! - `POST /voice/listen` (body: a short recording): `{text}`.
//! - `DELETE /voice`: remove the containers (downloaded models are kept).

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse};

pub struct Service {
    pub container: &'static str,
    pub image: &'static str,
    /// Bound to 127.0.0.1 on the host.
    pub port: u16,
    pub container_port: u16,
    pub volume: Option<(&'static str, &'static str)>,
    pub env: &'static [(&'static str, &'static str)],
}

pub const SPEAK: Service = Service {
    container: "tt-voice-kokoro",
    image: "ghcr.io/remsky/kokoro-fastapi-cpu:latest@sha256:ee3111d6a2c903ed62f3b4fa19543c6901205ed39fce3c177e895b34a8386b9c",
    port: 8880,
    container_port: 8880,
    volume: None,
    env: &[],
};

pub const LISTEN: Service = Service {
    container: "tt-voice-ears",
    image: "ghcr.io/speaches-ai/speaches:latest-cpu@sha256:21e3df06d842fb7802ab470dd77c25f0e8c0d22950e8d8c6ae886e851af53ef8",
    port: 8881,
    container_port: 8000,
    volume: Some(("tt-voice-hf", "/home/ubuntu/.cache/huggingface/hub")),
    env: &[("WHISPER__CPU_THREADS", "8")],
};

/// Small, fast and accurate for spoken requests on a CPU.
pub const LISTEN_MODEL: &str = "Systran/faster-distil-whisper-small.en";
/// More accurate, for recordings (about 2.5× faster than real time on the CPU).
pub const RECORDING_MODEL: &str = "Systran/faster-distil-whisper-large-v3";
pub const DEFAULT_VOICE: &str = "af_heart";

/// The CPU Whisper's root URL, when it's up (the job queue uses it so
/// transcribing doesn't take the chips from the model).
pub async fn cpu_whisper() -> Option<String> {
    up(&LISTEN, "/health").await.then(|| base(&LISTEN))
}

/// Voices worth hearing first (Kokoro's best-rated).
const FAVOURITES: &[&str] = &[
    "af_heart",
    "af_bella",
    "am_michael",
    "am_fenrir",
    "bf_emma",
    "bm_george",
    "af_nicole",
    "am_puck",
];

/// What `crate::voice` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// Setting voice up: its step, or why it failed.
    setup: Mutex<Option<(String, Option<String>)>>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().voice
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

type Reply<T> = Result<T, (StatusCode, Json<ErrorResponse>)>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub voice: String,
    pub speed: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            voice: DEFAULT_VOICE.into(),
            speed: 1.0,
        }
    }
}

fn settings_path(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("voice.json")
}

pub fn settings(state: &AppState) -> Settings {
    crate::fsio::read_json_or_backup(&settings_path(state)).unwrap_or_default()
}

fn docker_state(container: &str) -> String {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", container])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "missing".into())
}

fn base(s: &Service) -> String {
    format!("http://127.0.0.1:{}", s.port)
}

/// "af_heart" → "Heart", with its accent and voice.
pub fn describe_voice(id: &str) -> Value {
    let mut parts = id.splitn(2, '_');
    let tag = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or(id);
    let name: String = name
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let accent = match tag.chars().next() {
        Some('a') => "American",
        Some('b') => "British",
        Some('e') => "Spanish",
        Some('f') => "French",
        Some('h') => "Hindi",
        Some('i') => "Italian",
        Some('j') => "Japanese",
        Some('p') => "Portuguese",
        Some('z') => "Mandarin",
        _ => "",
    };
    let voice = match tag.chars().nth(1) {
        Some('f') => "female",
        Some('m') => "male",
        _ => "",
    };
    json!({
        "id": id,
        "name": name,
        "accent": accent,
        "gender": voice,
        "english": matches!(tag.chars().next(), Some('a' | 'b')),
        "favourite": FAVOURITES.contains(&id),
    })
}

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_default()
}

async fn voices() -> Vec<Value> {
    let Ok(res) = client(Duration::from_secs(3))
        .get(format!("{}/v1/audio/voices", base(&SPEAK)))
        .send()
        .await
    else {
        return Vec::new();
    };
    let body: Value = res.json().await.unwrap_or_default();
    let mut ids: Vec<String> = body["voices"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().or_else(|| v["id"].as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    ids.sort_by_key(|id| {
        (
            FAVOURITES
                .iter()
                .position(|f| f == id)
                .unwrap_or(FAVOURITES.len()),
            !id.starts_with('a') && !id.starts_with('b'),
            id.clone(),
        )
    });
    ids.iter().map(|id| describe_voice(id)).collect()
}

async fn up(s: &Service, path: &str) -> bool {
    client(Duration::from_secs(2))
        .get(format!("{}{path}", base(s)))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// `GET /voice` (authed).
pub(crate) async fn get_voice(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let (speak_docker, listen_docker) = tokio::task::spawn_blocking(|| {
        (
            docker_state(SPEAK.container),
            docker_state(LISTEN.container),
        )
    })
    .await
    .unwrap_or_default();
    let (speak_up, listen_up) = tokio::join!(up(&SPEAK, "/health"), up(&LISTEN, "/health"));
    let setup = mine(&state).setup.lock().expect("voice setup").clone();
    let installed = speak_docker != "missing" || listen_docker != "missing";
    let state_word = match (&setup, speak_up && listen_up) {
        (Some((_, Some(_))), _) => "failed",
        (Some(_), _) => "installing",
        (None, true) => "ready",
        (None, false) if installed && speak_docker == "running" => "starting",
        (None, false) if installed => "stopped",
        _ => "not_installed",
    };
    let settings = settings(&state);
    Json(json!({
        "state": state_word,
        "step": setup.as_ref().map(|(s, _)| s.clone()),
        "error": setup.and_then(|(_, e)| e),
        "speak": speak_up,
        "listen": listen_up,
        "voice": settings.voice,
        "speed": settings.speed,
        "voices": if speak_up { voices().await } else { Vec::new() },
        "listen_model": LISTEN_MODEL,
    }))
}

fn run_container(s: &Service) -> Result<(), String> {
    match docker_state(s.container).as_str() {
        "running" => return Ok(()),
        "missing" => {}
        _ => {
            let o = Command::new("docker")
                .args(["start", s.container])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| e.to_string())?;
            return if o.status.success() {
                Ok(())
            } else {
                Err(last_line(&o.stderr, "docker start failed"))
            };
        }
    }
    let pulled = Command::new("docker")
        .args(["pull", s.image])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("docker isn't available: {e}"))?;
    if !pulled.status.success() {
        return Err(last_line(&pulled.stderr, "docker pull failed"));
    }
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "-d",
        "--name",
        s.container,
        "--label",
        "tt-station.voice=1",
        "--restart",
        "unless-stopped",
        "-p",
        &format!("127.0.0.1:{}:{}", s.port, s.container_port),
    ]);
    if let Some((volume, path)) = s.volume {
        cmd.args(["-v", &format!("{volume}:{path}")]);
    }
    for (k, v) in s.env {
        cmd.args(["-e", &format!("{k}={v}")]);
    }
    let o = cmd
        .arg(s.image)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(())
    } else {
        Err(last_line(&o.stderr, "docker run failed"))
    }
}

fn last_line(bytes: &[u8], fallback: &str) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .lines()
        .last()
        .unwrap_or(fallback)
        .to_string()
}

async fn wait_up(s: &Service, path: &str, secs: u64) -> bool {
    for _ in 0..secs {
        if up(s, path).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    false
}

async fn setup(state: &AppState) -> Result<(), String> {
    let set =
        |step: &str| *mine(state).setup.lock().expect("voice setup") = Some((step.into(), None));
    set("Downloading the voice");
    tokio::task::spawn_blocking(|| run_container(&SPEAK))
        .await
        .map_err(|e| e.to_string())??;
    set("Downloading speech recognition");
    tokio::task::spawn_blocking(|| run_container(&LISTEN))
        .await
        .map_err(|e| e.to_string())??;
    set("Starting up");
    if !wait_up(&LISTEN, "/health", 120).await {
        return Err("speech recognition didn't start; see `docker logs tt-voice-ears`".into());
    }
    set("Downloading the listening model");
    let res = client(Duration::from_secs(900))
        .post(format!("{}/v1/models/{LISTEN_MODEL}", base(&LISTEN)))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() && res.status() != StatusCode::CONFLICT {
        return Err(format!(
            "couldn't download {LISTEN_MODEL}: {}",
            res.text().await.unwrap_or_default()
        ));
    }
    set("Downloading the transcription model");
    let res = client(Duration::from_secs(1800))
        .post(format!("{}/v1/models/{RECORDING_MODEL}", base(&LISTEN)))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() && res.status() != StatusCode::CONFLICT {
        return Err(format!(
            "couldn't download {RECORDING_MODEL}: {}",
            res.text().await.unwrap_or_default()
        ));
    }
    set("Warming up the voice");
    if !wait_up(&SPEAK, "/health", 300).await {
        return Err("the voice didn't start; see `docker logs tt-voice-kokoro`".into());
    }
    Ok(())
}

/// `POST /voice/setup` (authed): 202 while it installs; poll `GET /voice`.
pub(crate) async fn post_setup(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Reply<(StatusCode, Json<Value>)> {
    {
        let mut guard = mine(&state).setup.lock().expect("voice setup");
        if guard.as_ref().is_some_and(|(_, e)| e.is_none()) {
            return Ok((StatusCode::ACCEPTED, Json(json!({ "state": "installing" }))));
        }
        *guard = Some(("Starting".into(), None));
    }
    tokio::spawn(async move {
        let result = setup(&state).await;
        let mut guard = mine(&state).setup.lock().expect("voice setup");
        *guard = match result {
            Ok(()) => None,
            Err(e) => Some(("Failed".into(), Some(e))),
        };
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "state": "installing" }))))
}

#[derive(Debug, Deserialize)]
pub struct SettingsBody {
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub speed: Option<f32>,
}

fn valid_voice(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '.')
}

/// `PUT /voice` (authed).
pub(crate) async fn put_voice(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<SettingsBody>,
) -> Reply<Json<Settings>> {
    let mut s = settings(&state);
    if let Some(v) = body.voice {
        if !valid_voice(&v) {
            return Err(err(StatusCode::BAD_REQUEST, "That isn't a voice."));
        }
        s.voice = v;
    }
    if let Some(speed) = body.speed {
        s.speed = speed.clamp(0.5, 2.0);
    }
    crate::fsio::write_json(&settings_path(&state), &s, crate::fsio::PRIVATE)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(s))
}

#[derive(Debug, Deserialize)]
pub struct SpeakBody {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub speed: Option<f32>,
    #[serde(default)]
    pub format: Option<String>,
}

/// `POST /voice/speak` (authed): the audio.
pub(crate) async fn post_speak(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<SpeakBody>,
) -> Reply<Response> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Nothing to say."));
    }
    if text.len() > 4000 {
        return Err(err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That's too long to say at once.",
        ));
    }
    let saved = settings(&state);
    let voice = body.voice.filter(|v| valid_voice(v)).unwrap_or(saved.voice);
    let speed = body.speed.unwrap_or(saved.speed).clamp(0.5, 2.0);
    let format = body
        .format
        .as_deref()
        .filter(|f| matches!(*f, "mp3" | "wav" | "pcm" | "opus" | "flac"))
        .unwrap_or("mp3");
    let res = client(Duration::from_secs(60))
        .post(format!("{}/v1/audio/speech", base(&SPEAK)))
        .json(&json!({
            "model": "kokoro",
            "input": text,
            "voice": voice,
            "speed": speed,
            "response_format": format,
            "stream": false,
        }))
        .send()
        .await
        .map_err(|_| {
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                "The box's voice isn't running. Set it up in Settings → Voice.",
            )
        })?;
    if !res.status().is_success() {
        let status = res.status();
        let detail = res.text().await.unwrap_or_default();
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!(
                "The voice failed ({status}): {}",
                detail.chars().take(300).collect::<String>()
            ),
        ));
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    let mime = match format {
        "wav" => "audio/wav",
        "pcm" => "audio/L16;rate=24000;channels=1",
        "opus" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "audio/mpeg",
    };
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}

/// `POST /voice/listen` (authed), body = a recording (webm, wav, m4a, …).
pub(crate) async fn post_listen(
    _auth: BearerAuth,
    headers: HeaderMap,
    body: Bytes,
) -> Reply<Json<Value>> {
    if body.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "The recording was empty."));
    }
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let ext = match mime.split(';').next().unwrap_or("") {
        "audio/webm" | "video/webm" => "webm",
        "audio/ogg" => "ogg",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "audio/mpeg" => "mp3",
        _ => "webm",
    };
    let part = reqwest::multipart::Part::bytes(body.to_vec())
        .file_name(format!("speech.{ext}"))
        .mime_str(&mime)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", LISTEN_MODEL)
        .text("response_format", "json");
    let res = client(Duration::from_secs(60))
        .post(format!("{}/v1/audio/transcriptions", base(&LISTEN)))
        .multipart(form)
        .send()
        .await
        .map_err(|_| {
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Speech recognition isn't running. Set up the voice in Settings → Voice.",
            )
        })?;
    if !res.status().is_success() {
        let status = res.status();
        let detail = res.text().await.unwrap_or_default();
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!(
                "Listening failed ({status}): {}",
                detail.chars().take(300).collect::<String>()
            ),
        ));
    }
    let v: Value = res
        .json()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(
        json!({ "text": v["text"].as_str().unwrap_or("").trim() }),
    ))
}

/// `DELETE /voice` (authed): remove both containers.
pub(crate) async fn delete_voice(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    tokio::task::spawn_blocking(|| {
        for s in [&SPEAK, &LISTEN] {
            let _ = Command::new("docker")
                .args(["rm", "-f", s.container])
                .stdin(Stdio::null())
                .output();
        }
    })
    .await
    .ok();
    *mine(&state).setup.lock().expect("voice setup") = None;
    Json(json!({ "state": "not_installed" }))
}

// ---------------------------------------------------------------------
// Hearing and speaking on the box itself (for the top-bar dropdown, which
// has no browser to record or play audio)
// ---------------------------------------------------------------------

static HEARING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static HEAR_CANCEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// "That's it": stop and transcribe what's been said (push-to-talk released).
static HEAR_FINISH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static HEAR_LEVEL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SAYING: Mutex<Option<u32>> = Mutex::new(None);

/// What `hear` found: 16 kHz mono samples of one utterance (empty: nothing said).
fn record_utterance(patience: Duration, ptt: bool) -> Result<Vec<i16>, String> {
    use std::io::Read;
    use std::sync::atomic::Ordering;
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "pulse",
            "-i",
            "default",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-f",
            "s16le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start the microphone: {e}"))?;
    let mut out = child.stdout.take().ok_or("no audio")?;
    let mut frame = vec![0u8; 960]; // 30 ms
    let mut pre: std::collections::VecDeque<i16> = std::collections::VecDeque::new();
    let mut said: Vec<i16> = Vec::new();
    // Push-to-talk records everything until released; otherwise wait for speech.
    let (mut floor, mut speaking, mut voiced_ms, mut quiet_ms) = (-55.0f64, ptt, 0u32, 0u32);
    let began = std::time::Instant::now();
    let result = loop {
        if HEAR_CANCEL.load(Ordering::Relaxed) {
            break Ok(Vec::new());
        }
        if HEAR_FINISH.load(Ordering::Relaxed) {
            break Ok(std::mem::take(&mut said));
        }
        if out.read_exact(&mut frame).is_err() {
            break Err("the microphone stopped".to_string());
        }
        let samples: Vec<i16> = frame
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let rms = (samples.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / samples.len() as f64)
            .sqrt()
            / 32768.0;
        let db = 20.0 * (rms + 1e-6).log10();
        HEAR_LEVEL.store((db as f32).to_bits(), Ordering::Relaxed);
        let loud = db > (floor + 14.0).max(-48.0);
        if !speaking {
            floor = if db < floor {
                floor * 0.7 + db * 0.3
            } else {
                floor * 0.98 + db * 0.02
            };
            pre.extend(&samples);
            while pre.len() > 16000 * 4 / 10 {
                pre.pop_front(); // keep 400 ms before speech starts
            }
            voiced_ms = if loud { voiced_ms + 30 } else { 0 };
            if voiced_ms > 150 {
                speaking = true;
                said.extend(pre.drain(..));
            } else if began.elapsed() > patience {
                break Ok(Vec::new());
            }
        } else {
            said.extend(&samples);
            quiet_ms = if loud { 0 } else { quiet_ms + 30 };
            if (!ptt && quiet_ms > 900) || said.len() > 16000 * 60 {
                break Ok(said);
            }
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    result
}

pub(crate) fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend(b"RIFF");
    out.extend((36 + data_len).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes()); // PCM
    out.extend(1u16.to_le_bytes()); // mono
    out.extend(16000u32.to_le_bytes());
    out.extend(32000u32.to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data_len.to_le_bytes());
    for s in samples {
        out.extend(s.to_le_bytes());
    }
    out
}

#[derive(Debug, Default, Deserialize)]
pub struct HearBody {
    /// How long to wait for speech to start, in seconds (default 8).
    #[serde(default)]
    pub patience_s: Option<f64>,
    /// Push-to-talk: record until `POST /voice/hear/finish`, however quiet.
    #[serde(default)]
    pub ptt: bool,
}

/// `POST /voice/hear` (authed): listen through the box's microphone until
/// someone has said something and stopped; `{text}` ("" if nobody spoke).
/// The box ticks softly first (`crate::chime::Chime::Listening`), so the room
/// knows the microphone opened. One request, a minute at most, nothing kept:
/// a tick rather than the recording chime, banner and notification.
pub(crate) async fn post_hear(
    _auth: BearerAuth,
    body: Option<Json<HearBody>>,
) -> Reply<Json<Value>> {
    use std::sync::atomic::Ordering;
    if HEARING.swap(true, Ordering::SeqCst) {
        return Err(err(StatusCode::CONFLICT, "Already listening."));
    }
    HEAR_CANCEL.store(false, Ordering::SeqCst);
    HEAR_FINISH.store(false, Ordering::SeqCst);
    let body = body.map(|b| b.0).unwrap_or_default();
    let patience = Duration::from_secs_f64(body.patience_s.unwrap_or(8.0).clamp(1.0, 60.0));
    let ptt = body.ptt;
    let samples = tokio::task::spawn_blocking(move || {
        // The room hears a soft tick as the microphone opens, so a request
        // from a phone elsewhere isn't silent listening. It plays to the end
        // before the microphone opens, so it isn't heard as speech.
        if let Err(e) = crate::chime::play_blocking(crate::chime::Chime::Listening) {
            eprintln!("tt-station-agentd: listening chime: {e}");
        }
        record_utterance(patience, ptt)
    })
    .await;
    HEARING.store(false, Ordering::SeqCst);
    HEAR_LEVEL.store((-100f32).to_bits(), Ordering::Relaxed);
    let samples = samples
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    // Under a quarter second is a tap, not speech.
    if samples.len() < 4000 {
        return Ok(Json(json!({ "text": "" })));
    }
    let part = reqwest::multipart::Part::bytes(wav(&samples))
        .file_name("speech.wav")
        .mime_str("audio/wav")
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model", LISTEN_MODEL);
    let res = client(Duration::from_secs(60))
        .post(format!("{}/v1/audio/transcriptions", base(&LISTEN)))
        .multipart(form)
        .send()
        .await
        .map_err(|_| {
            err(
                StatusCode::SERVICE_UNAVAILABLE,
                "Speech recognition isn't running.",
            )
        })?;
    let v: Value = res.json().await.unwrap_or_default();
    let text = v["text"].as_str().unwrap_or("").trim().to_string();
    // Whisper's usual inventions on near-silence.
    let junk = [
        "you",
        "you.",
        "thank you.",
        "thank you",
        "thanks for watching!",
        "bye.",
        ".",
    ];
    let text = if junk.contains(&text.to_lowercase().as_str()) {
        String::new()
    } else {
        text
    };
    Ok(Json(json!({ "text": text })))
}

/// `GET /voice/hear` (authed): `{hearing, level_db}` for drawing a level.
pub(crate) async fn get_hear(_auth: BearerAuth) -> Json<Value> {
    use std::sync::atomic::Ordering;
    Json(json!({
        "hearing": HEARING.load(Ordering::Relaxed),
        "level_db": f32::from_bits(HEAR_LEVEL.load(Ordering::Relaxed)),
        "saying": SAYING.lock().expect("saying").is_some(),
    }))
}

/// `POST /voice/hear/finish` (authed): stop listening and transcribe what was said.
pub(crate) async fn post_hear_finish(_auth: BearerAuth) -> Json<Value> {
    HEAR_FINISH.store(true, std::sync::atomic::Ordering::SeqCst);
    Json(json!({ "finishing": true }))
}

/// `DELETE /voice/hear` (authed): stop listening (and stop talking).
pub(crate) async fn delete_hear(_auth: BearerAuth) -> Json<Value> {
    HEAR_CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
    stop_saying();
    Json(json!({ "stopped": true }))
}

fn stop_saying() {
    if let Some(pid) = SAYING.lock().expect("saying").take() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
}

#[derive(Debug, Deserialize)]
pub struct SayBody {
    pub text: String,
    #[serde(default)]
    pub voice: Option<String>,
}

/// `POST /voice/say` (authed): say `text` out of the box's speakers; returns
/// when it's been said (or was stopped).
pub(crate) async fn post_say(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<SayBody>,
) -> Reply<Json<Value>> {
    let text: String = body.text.trim().chars().take(3000).collect();
    if text.is_empty() {
        return Ok(Json(json!({ "said": false })));
    }
    stop_saying();
    let saved = settings(&state);
    let voice = body.voice.filter(|v| valid_voice(v)).unwrap_or(saved.voice);
    let res = client(Duration::from_secs(60))
        .post(format!("{}/v1/audio/speech", base(&SPEAK)))
        .json(&json!({ "model": "kokoro", "input": text, "voice": voice, "speed": saved.speed, "response_format": "wav", "stream": false }))
        .send()
        .await
        .map_err(|_| err(StatusCode::SERVICE_UNAVAILABLE, "The box's voice isn't running."))?;
    let audio = res
        .bytes()
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    // A private, unguessable file (not `/tmp/tt-say-<pid>.wav`, which anyone
    // on the box could create first, or swap for a symlink).
    let path = tokio::task::spawn_blocking(move || -> std::io::Result<std::path::PathBuf> {
        use std::io::Write;
        let (mut f, path) = crate::fsio::private_temp_file("tt-say-", ".wav")?;
        f.write_all(&audio)?;
        Ok(path)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let path2 = path.clone();
    let played = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let mut child = Command::new("pw-play")
            .arg(&path2)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("couldn't play audio: {e}"))?;
        *SAYING.lock().expect("saying") = Some(child.id());
        let _ = child.wait();
        let mut saying = SAYING.lock().expect("saying");
        if *saying == Some(child.id()) {
            *saying = None;
        }
        Ok(())
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = tokio::fs::remove_file(&path).await;
    played.map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, e))?;
    Ok(Json(json!({ "said": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voices_get_friendly_names() {
        let v = describe_voice("af_heart");
        assert_eq!(v["name"], "Heart");
        assert_eq!(v["accent"], "American");
        assert_eq!(v["gender"], "female");
        assert_eq!(v["favourite"], true);
        let v = describe_voice("bm_george");
        assert_eq!(v["accent"], "British");
        assert_eq!(v["gender"], "male");
    }

    #[test]
    fn wav_header_is_right() {
        let w = wav(&[0, 1, -1]);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes([w[40], w[41], w[42], w[43]]), 6);
    }

    #[test]
    fn voice_ids_are_checked() {
        assert!(valid_voice("af_heart"));
        assert!(valid_voice("af_bella+af_sky"));
        assert!(!valid_voice("../etc"));
        assert!(!valid_voice(""));
    }
}
