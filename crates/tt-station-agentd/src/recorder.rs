// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Meeting recorder: record what the box hears (its microphone) and what it
//! plays (the meeting coming out of the speakers), mixed into one file, then
//! hand it to the job queue to transcribe like any uploaded recording.
//!
//! Capture is ffmpeg over PipeWire's Pulse interface: `default` is the
//! microphone, `@DEFAULT_MONITOR@` is whatever the default output plays. A
//! second, tiny output (8 kHz stereo PCM on stdout, mic left, meeting right)
//! drives the live level meters so you can see both sides are being heard.
//!
//! - `GET /record`: sources on the box, and the recording in progress if any
//!   (with who started it).
//! - `POST /record/start` `{title?, mic?, system?}`: start recording.
//! - `POST /record/stop` `{discard?}`: stop; queues a transcription job.
//!
//! Any paired device can start a recording, so the room is always told
//! ([`announce_start`], [`announce_end`]): the box chimes when a recording
//! starts and when it stops (`crate::chime`), the desktop app on the box shows
//! a banner while it runs (from `GET /record`), the other paired phones get a
//! "the box started recording" notification (`crate::notify`), and the
//! activity log (`crate::audit`) keeps who started and stopped it.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::jobs::When;
use crate::routes::{AppState, BearerAuth, ErrorResponse};

/// Longest single recording: four hours.
const MAX_SECONDS: u64 = 4 * 60 * 60;

struct Active {
    id: String,
    title: String,
    started: u64,
    clock: Instant,
    child: Child,
    path: PathBuf,
    mic: Option<String>,
    system: Option<String>,
    /// Latest RMS level per side, dBFS × 10 as bits of an f32.
    mic_level: Arc<AtomicU32>,
    system_level: Arc<AtomicU32>,
    /// Who started it, as people read it (a person's or a device's name).
    started_by: String,
    /// The room has been told it ended (it ended on its own).
    end_announced: bool,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The friendly name of the default PipeWire source or sink, if there is one.
fn describe(which: &str) -> Option<String> {
    let out = Command::new("wpctl")
        .args(["inspect", which])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |key: &str| {
        text.lines()
            .find(|l| l.contains(&format!("{key} = ")))
            .and_then(|l| l.split_once(" = "))
            .map(|(_, v)| v.trim().trim_matches('"').to_string())
    };
    // With no microphone, the "default source" is a sink's monitor.
    if which.contains("SOURCE") && field("media.class").is_some_and(|c| c != "Audio/Source") {
        return None;
    }
    field("node.description")
        .or_else(|| field("node.name"))
        .map(|name| tidy(&name))
}

/// "EMEET OfficeCore Luna Plus Mono" → "EMEET OfficeCore Luna Plus"; HDMI
/// outputs (a long controller name) → "HDMI".
pub fn tidy(name: &str) -> String {
    if name.contains("(HDMI)") {
        return "HDMI".into();
    }
    let mut name = name.trim();
    for suffix in [" Mono", " Analog Stereo", " Digital Stereo", " Stereo"] {
        name = name.strip_suffix(suffix).unwrap_or(name);
    }
    name.to_string()
}

pub fn microphone() -> Option<String> {
    describe("@DEFAULT_AUDIO_SOURCE@")
}

pub fn speakers() -> Option<String> {
    describe("@DEFAULT_AUDIO_SINK@")
}

fn level(bits: &AtomicU32) -> f32 {
    f32::from_bits(bits.load(Ordering::Relaxed))
}

/// Read 16-bit stereo PCM and keep a 100 ms RMS per channel, in dBFS.
fn meter(mut out: impl Read + Send + 'static, left: Arc<AtomicU32>, right: Arc<AtomicU32>) {
    std::thread::spawn(move || {
        // 8 kHz × 0.1 s × 2 channels × 2 bytes.
        let mut buf = vec![0u8; 3200];
        let db = |sum: f64, n: f64| {
            let rms = (sum / n).sqrt() / 32768.0;
            (20.0 * rms.max(1e-5).log10()) as f32
        };
        while out.read_exact(&mut buf).is_ok() {
            let (mut l, mut r) = (0f64, 0f64);
            for frame in buf.chunks_exact(4) {
                let a = i16::from_le_bytes([frame[0], frame[1]]) as f64;
                let b = i16::from_le_bytes([frame[2], frame[3]]) as f64;
                l += a * a;
                r += b * b;
            }
            left.store(db(l, 800.0).to_bits(), Ordering::Relaxed);
            right.store(db(r, 800.0).to_bits(), Ordering::Relaxed);
        }
    });
}

/// ffmpeg arguments for recording `mic` and/or `system` into `path`.
pub fn ffmpeg_args(mic: bool, system: bool, path: &str) -> Vec<String> {
    let mut args: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]
        .map(String::from)
        .to_vec();
    let mut inputs = Vec::new();
    for (on, device) in [(mic, "default"), (system, "@DEFAULT_MONITOR@")] {
        if on {
            args.extend(["-f", "pulse", "-i", device].map(String::from));
            inputs.push(inputs.len());
        }
    }
    let prep = |i: usize| {
        format!("[{i}:a]aresample=16000,aformat=channel_layouts=mono,asplit[m{i}][l{i}]")
    };
    let graph = match inputs.len() {
        2 => format!(
            "{};{};[m0][m1]amix=inputs=2:duration=longest:normalize=0[out];\
             [l0][l1]join=inputs=2:channel_layout=stereo,aresample=8000[lv]",
            prep(0),
            prep(1)
        ),
        // One side only: the meter shows it on its own channel, the other silent.
        _ => {
            let (left, right) = if mic {
                ("[l0]", "[z]")
            } else {
                ("[z]", "[l0]")
            };
            format!(
                "{};anullsrc=r=16000:cl=mono[z];{left}{right}join=inputs=2:channel_layout=stereo,aresample=8000[lv];[m0]anull[out]",
                prep(0)
            )
        }
    };
    let limit = MAX_SECONDS.to_string();
    args.extend(
        [
            "-filter_complex",
            &graph,
            "-map",
            "[out]",
            "-c:a",
            "libopus",
            "-b:a",
            "32k",
            "-t",
            &limit,
            path,
            "-map",
            "[lv]",
            "-t",
            &limit,
            "-f",
            "s16le",
            "-ac",
            "2",
            "pipe:1",
        ]
        .map(String::from),
    );
    args
}

fn status_of(active: &mut Option<Active>) -> Value {
    // A recording that hit its limit or lost its device ends on its own.
    let ended = active
        .as_mut()
        .is_some_and(|a| a.child.try_wait().ok().flatten().is_some());
    let recording = active.as_ref().map(|a| {
        json!({
            "id": a.id,
            "title": a.title,
            "started": a.started,
            "seconds": a.clock.elapsed().as_secs(),
            "mic": a.mic,
            "system": a.system,
            "mic_db": a.mic.as_ref().map(|_| level(&a.mic_level)),
            "system_db": a.system.as_ref().map(|_| level(&a.system_level)),
            "ended": ended,
            "max_seconds": MAX_SECONDS,
            "started_by": a.started_by,
        })
    });
    json!({
        "microphone": microphone(),
        "speakers": speakers(),
        "recording": recording,
    })
}

/// `GET /record` (authed).
pub(crate) async fn get_record(_auth: BearerAuth) -> Json<Value> {
    Json(
        tokio::task::spawn_blocking(|| status_of(&mut ACTIVE.lock().expect("recorder mutex")))
            .await
            .unwrap_or_else(|_| json!({})),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct StartBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub mic: Option<bool>,
    #[serde(default)]
    pub system: Option<bool>,
}

fn default_title() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "Meeting, {} {} {:02}:{:02}",
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// What the room and the household are told when a recording starts: a chime
/// on the box, an event for everyone (pushed to every paired phone but the
/// one that started it), and a line in the activity log.
fn announce_start(state: &AppState, by: &crate::notify::Requester, id: &str) {
    crate::chime::play(crate::chime::Chime::RecordingStarted);
    let who = by.label();
    state.notifier().emit_to(
        crate::notify::Draft {
            kind: "recording",
            title: format!("{} started recording", state.name()),
            body: format!(
                "Started by {who}. Everything said near the box is recorded until someone stops it."
            ),
        },
        Some(json!({ "recording": { "id": id, "started_by": who } })),
        crate::notify::Audience::Household,
        by.device.clone(),
    );
    crate::audit::record_as(state, &by.actor, "record_start", id, "ok");
}

/// The recording `id` ended: chime, and say so (recorded for `/events`, not
/// pushed: the start was the news). `how` is `stopped`, `discarded` or
/// `ended` (on its own: a device went away, or it hit [`MAX_SECONDS`]).
fn announce_end(state: &AppState, id: &str, how: &str) {
    crate::chime::play(crate::chime::Chime::RecordingStopped);
    state.notifier().record_to(
        crate::notify::Draft {
            kind: "recording_stopped",
            title: format!("{} stopped recording", state.name()),
            body: match how {
                "ended" => "The recording ended on its own.".to_string(),
                "discarded" => "The recording was thrown away.".to_string(),
                _ => "The recording is being transcribed.".to_string(),
            },
        },
        Some(json!({ "recording": { "id": id } })),
        crate::notify::Audience::Household,
        None,
    );
}

/// Until recording `id` is over: when it ends on its own (rather than being
/// stopped), tell the room.
fn watch_end(state: AppState, id: String) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let watched = id.clone();
            let ended = tokio::task::spawn_blocking(move || {
                let mut guard = ACTIVE.lock().expect("recorder mutex");
                match guard.as_mut() {
                    Some(a) if a.id == watched => {
                        if a.child.try_wait().ok().flatten().is_some() {
                            a.end_announced = true;
                            Some(true)
                        } else {
                            None
                        }
                    }
                    // Stopped (which announces it), or another recording.
                    _ => Some(false),
                }
            })
            .await
            .unwrap_or(Some(false));
            match ended {
                Some(true) => {
                    crate::audit::record_as(
                        &state,
                        &crate::audit::Actor::the_box(),
                        "record_end",
                        &id,
                        "ended",
                    );
                    announce_end(&state, &id, "ended");
                    return;
                }
                Some(false) => return,
                None => {}
            }
        }
    });
}

/// `POST /record/start` (authed).
pub(crate) async fn post_start(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    body: Option<Json<StartBody>>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ErrorResponse>)> {
    start(&state, body.map(|b| b.0).unwrap_or_default()).await
}

/// Start recording (for `POST /record/start` and Tori).
pub(crate) async fn start(
    state: &AppState,
    body: StartBody,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ErrorResponse>)> {
    let jobs = state
        .jobs()
        .ok_or_else(|| err(StatusCode::CONFLICT, "This agent has no job queue."))?;
    // Who's asking: the request's device and person (Tori's included), read
    // here because they don't follow the work onto another thread.
    let by = crate::notify::Requester::current(state);
    let started_by = by.label();
    let started = tokio::task::spawn_blocking(move || {
        let mut guard = ACTIVE.lock().expect("recorder mutex");
        if let Some(a) = guard.as_mut() {
            if a.child.try_wait().ok().flatten().is_none() {
                return Err(err(StatusCode::CONFLICT, "Already recording."));
            }
        }
        let mic_name = microphone();
        let want_mic = body.mic.unwrap_or(true) && mic_name.is_some();
        let want_system = body.system.unwrap_or(true);
        if !want_mic && !want_system {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "There's no microphone plugged in. Record the meeting audio, or plug one in.",
            ));
        }
        let title: String = body
            .title
            .as_deref()
            .unwrap_or("")
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(120)
            .collect();
        let title = if title.is_empty() {
            default_title()
        } else {
            title
        };
        let id = crate::pairing::issue_pair_id()[..12].to_string();
        let dir = jobs.job_dir(&id);
        // A recording of whoever was talking: owner-only. ffmpeg writes into a
        // file made here at 0600 (`-y` truncates it, keeping the mode), in a
        // 0700 folder.
        crate::fsio::ensure_private_dir(&dir)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let path = dir.join("input.ogg");
        crate::fsio::write_atomic(&path, b"", crate::fsio::PRIVATE)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut child = Command::new("ffmpeg")
            .args(ffmpeg_args(want_mic, want_system, &path.to_string_lossy()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&dir);
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("couldn't start ffmpeg: {e}"),
                )
            })?;
        let silent = (-100f32).to_bits();
        let (mic_level, system_level) = (
            Arc::new(AtomicU32::new(silent)),
            Arc::new(AtomicU32::new(silent)),
        );
        if let Some(out) = child.stdout.take() {
            meter(out, mic_level.clone(), system_level.clone());
        }
        // A missing or busy device makes ffmpeg quit at once.
        std::thread::sleep(Duration::from_millis(600));
        if let Ok(Some(code)) = child.try_wait() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The recorder stopped right away ({code}). Is the microphone in use?"),
            ));
        }
        *guard = Some(Active {
            id,
            title,
            started: unix_now(),
            clock: Instant::now(),
            child,
            path,
            mic: mic_name.filter(|_| want_mic),
            system: want_system.then(|| speakers().unwrap_or_else(|| "Speakers".into())),
            mic_level,
            system_level,
            started_by,
            end_announced: false,
        });
        Ok((StatusCode::CREATED, Json(status_of(&mut guard))))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;
    let id = started.1["recording"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    announce_start(state, &by, &id);
    watch_end(state.clone(), id);
    Ok(started)
}

#[derive(Debug, Default, Deserialize)]
pub struct StopBody {
    /// Throw the recording away instead of transcribing it.
    #[serde(default)]
    pub discard: bool,
}

/// `POST /record/stop` (authed): `{job}` once it's queued for transcription.
pub(crate) async fn post_stop(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    body: Option<Json<StopBody>>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    stop(&state, body.map(|b| b.0.discard).unwrap_or(false)).await
}

/// Stop recording and queue the transcript (for `POST /record/stop` and Tori).
pub(crate) async fn stop(
    state: &AppState,
    discard: bool,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let jobs = state
        .jobs()
        .ok_or_else(|| err(StatusCode::CONFLICT, "This agent has no job queue."))?;
    let active = ACTIVE
        .lock()
        .expect("recorder mutex")
        .take()
        .ok_or_else(|| err(StatusCode::CONFLICT, "Nothing is recording."))?;
    let Active {
        id,
        title,
        mut child,
        path,
        end_announced,
        ..
    } = active;
    let by = crate::notify::Requester::current(state);
    crate::audit::record_as(
        state,
        &by.actor,
        "record_stop",
        &id,
        if discard { "discarded" } else { "ok" },
    );
    // SIGINT lets ffmpeg finish the file properly.
    tokio::task::spawn_blocking(move || {
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    })
    .await
    .ok();
    // After ffmpeg has finished, so the chime isn't the recording's last sound.
    if !end_announced {
        announce_end(state, &id, if discard { "discarded" } else { "stopped" });
    }
    let dir = jobs.job_dir(&id);
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if discard || size < 2048 {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        if discard {
            return Ok(Json(json!({ "job": null, "discarded": true })));
        }
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "The recording came out empty, so there's nothing to transcribe.",
        ));
    }
    let job = jobs.add(id, "transcribe", &title, When::Now, true);
    Ok(Json(json!({ "job": job })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_are_mixed_and_metered() {
        let args = ffmpeg_args(true, true, "/x/input.ogg").join(" ");
        assert!(args.contains("-i default"));
        assert!(args.contains("-i @DEFAULT_MONITOR@"));
        assert!(args.contains("amix=inputs=2"));
        assert!(args.contains("/x/input.ogg"));
        assert!(args.ends_with("pipe:1"));
    }

    #[tokio::test]
    async fn a_recording_tells_the_household_and_the_log_who_started_it() {
        let state = AppState::new(
            "qb2-lab".to_string(),
            "4xBH".to_string(),
            Arc::new(crate::serving::dstack::DstackBackend),
        );
        let session = state
            .sessions()
            .pair("max-token", Some("Max's iPhone"), None, false);
        let max = state.people().add("Max").unwrap();
        state.people().assign("max-token", Some(&max.id)).unwrap();
        let by = crate::audit::ACTOR
            .scope(
                crate::audit::Actor {
                    device_id: Some(session.id.clone()),
                    device_name: session.name.clone(),
                },
                async { crate::notify::Requester::current(&state) },
            )
            .await;
        assert_eq!(by.label(), "Max");
        assert_eq!(by.device.as_deref(), Some(session.token_sha256.as_str()));

        announce_start(&state, &by, "rec-1");
        let e = state.notifier().since(0).pop().unwrap();
        assert_eq!(e.kind, "recording");
        assert_eq!(e.title, "qb2-lab started recording");
        assert!(e.body.contains("Started by Max"), "{}", e.body);
        assert_eq!(e.audience, crate::notify::Audience::Household);
        assert_eq!(
            e.quiet_for.as_deref(),
            Some(session.token_sha256.as_str()),
            "not pushed back to the phone that started it"
        );
        let push = crate::notify::push_payload(&e, "qb2-lab").to_string();
        assert!(push.contains("The box started recording") && !push.contains("Max"));

        announce_end(&state, "rec-1", "stopped");
        assert_eq!(
            state.notifier().since(e.seq).pop().unwrap().kind,
            "recording_stopped"
        );

        let (log, _) = state.audit().page(0, 10);
        let start = log.iter().find(|l| l.action == "record_start").unwrap();
        assert_eq!(start.device_name, "Max's iPhone");
        assert_eq!(start.device_id.as_deref(), Some(session.id.as_str()));
        assert_eq!(start.target, "rec-1");
    }

    #[test]
    fn device_names_are_tidied() {
        assert_eq!(
            tidy("EMEET OfficeCore Luna Plus Mono"),
            "EMEET OfficeCore Luna Plus"
        );
        assert_eq!(
            tidy("Rembrandt Radeon High Definition Audio Controller Digital Stereo (HDMI)"),
            "HDMI"
        );
    }

    #[test]
    fn meeting_audio_alone_meters_on_the_right() {
        let args = ffmpeg_args(false, true, "/x/input.ogg").join(" ");
        assert!(!args.contains("-i default"));
        assert!(args.contains("[z][l0]join"));
        assert!(!args.contains("amix"));
    }
}
