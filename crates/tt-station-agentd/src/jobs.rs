// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Long jobs, queued on the box: hand it a two-hour meeting recording and walk
//! away. Today's kind is **transcription** with Whisper; the queue is the part
//! that matters:
//!
//! - Jobs run in order, one at a time, and survive the app closing and the
//!   agent restarting (each lives in `$TT_CONFIG_DIR/jobs/<id>/`).
//! - A job needs a mode (`crate::modes`): Whisper takes the whole box. If the
//!   box is idle the queue switches to it by itself. If it's busy with
//!   something else, a `now` job waits until the owner says go
//!   (`POST /jobs/{id}/run`), and a `tonight` job runs between 1 and 6 am --
//!   after which the queue puts back whatever was serving.
//! - Whisper takes at most 5 minutes of audio per request, so recordings are cut
//!   with ffmpeg into 4.5-minute 16 kHz mono pieces, transcribed in order, and
//!   stitched back with their timestamps shifted.
//! - When it's done: a notification and a transcript (text, SRT, JSON). The
//!   recording itself is deleted unless the person chose to keep it
//!   (`keep_audio`, else the household's `crate::export::DataSettings`), and the
//!   transcript goes into the shared Library only when asked (`documents`, else
//!   the household default, off).
//! - Progress is kept per chunk (`progress.json`), so an agent restart resumes
//!   at the next chunk instead of starting the recording over.
//! - Each job remembers the device that started it. Members see their own jobs
//!   and the ones shared with the household; owners see all (see [`visible`]).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, CallerAuth, ErrorResponse, Phase};

/// Longest piece sent to Whisper. tt-media-server documents 300 s with
/// preprocessing, but the live server transcribes only the first 60 s of a
/// request and silently drops the rest (seen 2026-10-07), so stay under that.
pub const MAX_CHUNK_SECONDS: f64 = 55.0;
/// Shortest piece worth cutting at a pause for.
pub const MIN_CHUNK_SECONDS: f64 = 20.0;

/// Silences in `ffmpeg -af silencedetect` output, as their midpoints.
pub fn parse_silences(ffmpeg_stderr: &str) -> Vec<f64> {
    let mut starts = Vec::new();
    let mut mids = Vec::new();
    for line in ffmpeg_stderr.lines() {
        if let Some(v) = line.split("silence_start: ").nth(1) {
            if let Ok(t) = v.split_whitespace().next().unwrap_or("").parse::<f64>() {
                starts.push(t);
            }
        } else if let Some(v) = line.split("silence_end: ").nth(1) {
            if let (Some(start), Ok(end)) = (
                starts.pop(),
                v.split_whitespace().next().unwrap_or("").parse::<f64>(),
            ) {
                mids.push((start.max(0.0) + end) / 2.0);
            }
        }
    }
    mids
}

/// Where to cut a recording of `duration` seconds: at a pause when one falls
/// between [`MIN_CHUNK_SECONDS`] and [`MAX_CHUNK_SECONDS`] into the piece (the
/// latest such), else hard at the maximum -- so words are rarely split.
pub fn choose_cuts(duration: f64, silences: &[f64]) -> Vec<f64> {
    let mut cuts = Vec::new();
    let mut from = 0.0;
    while duration - from > MAX_CHUNK_SECONDS {
        let cut = silences
            .iter()
            .copied()
            .filter(|t| *t > from + MIN_CHUNK_SECONDS && *t <= from + MAX_CHUNK_SECONDS)
            .fold(None, |best: Option<f64>, t| {
                Some(best.map_or(t, |b| b.max(t)))
            })
            .unwrap_or(from + MAX_CHUNK_SECONDS);
        cuts.push(cut);
        from = cut;
    }
    cuts
}
/// Biggest upload accepted.
pub const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// In line.
    Queued,
    /// Its mode isn't serving and the box is busy: needs the owner's go-ahead
    /// (or tonight's window).
    Waiting,
    /// Loading the mode it needs.
    Switching,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum When {
    Now,
    Tonight,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    /// `transcribe`
    pub kind: String,
    /// The file's name, as uploaded.
    pub title: String,
    pub when: When,
    pub status: JobStatus,
    /// Pieces done / total (transcription chunks).
    pub done: u32,
    pub total: u32,
    /// Recording length, once known.
    pub duration_s: Option<f64>,
    pub created: u64,
    pub started: Option<u64>,
    pub finished: Option<u64>,
    pub error: Option<String>,
    /// Add the transcript to the shared Library. `None`: the household default
    /// when it finishes ([`crate::export::DataSettings::transcripts_to_library`]).
    #[serde(default)]
    pub to_documents: Option<bool>,
    /// Keep the recording after it's transcribed. `None`: the household
    /// default ([`crate::export::DataSettings::keep_audio`], off).
    #[serde(default)]
    pub keep_audio: Option<bool>,
    /// The mode the queue switched away from, to put back afterwards.
    #[serde(default)]
    pub restore: Option<String>,
    /// The paired device that started it; `None` for the box itself (and the
    /// recorder, which doesn't say), which only owners see unless shared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    /// That device's name, for lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by_name: Option<String>,
    /// Shared with everyone on the box, not just its starter and the owners.
    #[serde(default)]
    pub shared: bool,
}

/// How a new job was asked for: the per-job choices (each `None` falls back to
/// the household default) and who asked.
#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub to_documents: Option<bool>,
    pub keep_audio: Option<bool>,
    pub started_by: Option<String>,
    pub started_by_name: Option<String>,
    pub shared: bool,
}

/// Whether `caller` may see (and act on) `job`: owners see every job; anyone
/// else their own and the shared ones.
pub(crate) fn visible(job: &Job, caller: &crate::routes::Caller) -> bool {
    caller.owner()
        || job.shared
        || (job.started_by.is_some() && job.started_by == caller.device_id())
}

/// Chunk-by-chunk progress of a transcription, kept beside the job so a
/// restart picks up at the next chunk. `offsets` are the pieces' start times:
/// cutting is deterministic, and a different cut (a changed input, or a newer
/// agent cutting differently) means starting over.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub offsets: Vec<f64>,
    pub done: usize,
    pub segments: Vec<Segment>,
}

/// Where a transcription picks up: the chunks already done and their segments,
/// when the saved progress was made from the same cut; else from the start.
pub fn resume_point(saved: Option<Progress>, offsets: &[f64]) -> (usize, Vec<Segment>) {
    match saved {
        Some(p)
            if p.offsets.len() == offsets.len()
                && p.done <= offsets.len()
                && p.offsets
                    .iter()
                    .zip(offsets)
                    .all(|(a, b)| (a - b).abs() < 1e-3) =>
        {
            (p.done, p.segments)
        }
        _ => (0, Vec::new()),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One transcribed stretch, with times in seconds from the recording's start.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Whisper's `verbose_json` answer for one chunk → segments shifted by `offset`.
/// tt-media-server names segment times `start_time`/`end_time`; OpenAI's API
/// says `start`/`end`. Both are read.
pub fn segments_from_whisper(answer: &Value, offset: f64, chunk_len: f64) -> Vec<Segment> {
    let time = |s: &Value, ours: &str, openai: &str| {
        s.get(ours)
            .or_else(|| s.get(openai))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            + offset
    };
    match answer["segments"].as_array() {
        Some(segments) if !segments.is_empty() => segments
            .iter()
            .filter_map(|s| {
                let text = s["text"].as_str()?.trim().to_string();
                (!text.is_empty()).then(|| Segment {
                    start: time(s, "start_time", "start"),
                    end: time(s, "end_time", "end"),
                    text,
                })
            })
            .collect(),
        _ => answer["text"]
            .as_str()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(|text| {
                vec![Segment {
                    start: offset,
                    end: offset + chunk_len,
                    text: text.to_string(),
                }]
            })
            .unwrap_or_default(),
    }
}

fn clock(seconds: f64, millis_sep: Option<char>) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (
        total_ms / 3_600_000,
        total_ms / 60_000 % 60,
        total_ms / 1000 % 60,
        total_ms % 1000,
    );
    match millis_sep {
        Some(sep) => format!("{h:02}:{m:02}:{s:02}{sep}{ms:03}"),
        None => format!("{h:02}:{m:02}:{s:02}"),
    }
}

/// A readable transcript: `[00:12:30] text`, one paragraph per segment.
pub fn to_text(segments: &[Segment]) -> String {
    segments
        .iter()
        .map(|s| format!("[{}] {}", clock(s.start, None), s.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// SubRip subtitles.
pub fn to_srt(segments: &[Segment]) -> String {
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!(
                "{}\n{} --> {}\n{}\n",
                i + 1,
                clock(s.start, Some(',')),
                clock(s.end, Some(',')),
                s.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether it's within tonight's window (1:00-5:59 local).
pub fn in_night_window(hour: u32) -> bool {
    (1..6).contains(&hour)
}

fn local_hour() -> u32 {
    // SAFETY: localtime_r only writes into the provided struct.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_hour as u32
    }
}

pub struct Jobs {
    dir: PathBuf,
    jobs: Mutex<Vec<Job>>,
    /// Ids the owner asked to cancel while running.
    cancelled: Mutex<Vec<String>>,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").field("dir", &self.dir).finish()
    }
}

impl Jobs {
    /// Load the queue from `dir`. Jobs interrupted mid-run go back in line,
    /// keeping the chunks they'd finished (see [`Progress`]).
    pub fn open(dir: impl Into<PathBuf>) -> Jobs {
        let dir = dir.into();
        let mut jobs: Vec<Job> = std::fs::read_dir(&dir)
            .map(|it| {
                it.flatten()
                    .filter_map(|e| crate::fsio::read_json_or_backup(&e.path().join("job.json")))
                    .collect()
            })
            .unwrap_or_default();
        for job in &mut jobs {
            if matches!(job.status, JobStatus::Running | JobStatus::Switching) {
                job.status = JobStatus::Queued;
            }
        }
        jobs.sort_by_key(|j| j.created);
        Jobs {
            dir,
            jobs: Mutex::new(jobs),
            cancelled: Mutex::new(Vec::new()),
        }
    }

    pub fn job_dir(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn list(&self) -> Vec<Job> {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .iter()
            .rev()
            .cloned()
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .iter()
            .find(|j| j.id == id)
            .cloned()
    }

    fn save(&self, job: &Job) {
        crate::fsio::save_json(
            &self.job_dir(&job.id).join("job.json"),
            job,
            crate::fsio::PRIVATE,
        );
    }

    pub fn update(&self, id: &str, change: impl FnOnce(&mut Job)) -> Option<Job> {
        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        let job = jobs.iter_mut().find(|j| j.id == id)?;
        change(job);
        let job = job.clone();
        drop(jobs);
        self.save(&job);
        Some(job)
    }

    /// Queue a job whose input file is already in [`Jobs::job_dir`]. For callers
    /// that don't know who asked or what they chose (the recorder): `true`
    /// leaves the Library and the audio to the household defaults, `false`
    /// keeps the transcript out of the Library.
    pub fn add(&self, id: String, kind: &str, title: &str, when: When, to_documents: bool) -> Job {
        self.add_with(
            id,
            kind,
            title,
            when,
            NewJob {
                to_documents: (!to_documents).then_some(false),
                ..NewJob::default()
            },
        )
    }

    /// Queue a job whose input file is already in [`Jobs::job_dir`].
    pub fn add_with(&self, id: String, kind: &str, title: &str, when: When, new: NewJob) -> Job {
        let job = Job {
            id,
            kind: kind.to_string(),
            title: title.to_string(),
            when,
            status: JobStatus::Queued,
            done: 0,
            total: 0,
            duration_s: None,
            created: unix_now(),
            started: None,
            finished: None,
            error: None,
            to_documents: new.to_documents,
            keep_audio: new.keep_audio,
            restore: None,
            started_by: new.started_by,
            started_by_name: new.started_by_name,
            shared: new.shared,
        };
        self.save(&job);
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .push(job.clone());
        job
    }

    /// Cancel (if waiting or running) or delete (if finished) a job.
    pub fn remove(&self, id: &str) -> bool {
        let Some(job) = self.get(id) else {
            return false;
        };
        if matches!(job.status, JobStatus::Running | JobStatus::Switching) {
            self.cancelled
                .lock()
                .expect("jobs mutex poisoned")
                .push(id.to_string());
            self.update(id, |j| j.status = JobStatus::Cancelled);
            return true;
        }
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .retain(|j| j.id != id);
        let _ = std::fs::remove_dir_all(self.job_dir(id));
        true
    }

    fn is_cancelled(&self, id: &str) -> bool {
        self.cancelled
            .lock()
            .expect("jobs mutex poisoned")
            .iter()
            .any(|c| c == id)
    }

    /// Whether any job is waiting or running.
    pub fn has_pending(&self) -> bool {
        self.next().is_some()
    }

    /// Delete finished jobs (done, failed, cancelled) that finished before
    /// `cutoff` (unix seconds), with their files. Returns how many went.
    pub fn expire(&self, cutoff: u64) -> usize {
        let old: Vec<String> = self
            .list()
            .into_iter()
            .filter(|j| {
                matches!(
                    j.status,
                    JobStatus::Done | JobStatus::Failed | JobStatus::Cancelled
                ) && j.finished.unwrap_or(j.created) < cutoff
            })
            .map(|j| j.id)
            .collect();
        for id in &old {
            self.remove(id);
        }
        old.len()
    }

    /// Pick up a job whose folder was just written (an import): load its
    /// `job.json` and add or replace it in the queue. Only finished jobs are
    /// taken, so an import never starts work.
    pub fn adopt(&self, id: &str) -> Option<Job> {
        let job: Job = crate::fsio::read_json_or_backup(&self.job_dir(id).join("job.json"))?;
        if job.id != id || job.status != JobStatus::Done {
            return None;
        }
        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        jobs.retain(|j| j.id != id);
        jobs.push(job.clone());
        jobs.sort_by_key(|j| j.created);
        Some(job)
    }

    /// The next job to work on, oldest first.
    fn next(&self) -> Option<Job> {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .iter()
            .find(|j| {
                matches!(
                    j.status,
                    JobStatus::Queued
                        | JobStatus::Waiting
                        | JobStatus::Switching
                        | JobStatus::Running
                )
            })
            .cloned()
    }
}

/// What the runner should do with the next job, given the box's state.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Whisper is serving: transcribe.
    Run,
    /// Nothing is serving (or it's tonight's window): switch to transcription.
    Switch,
    /// Busy with something else: wait for the owner (or the night).
    Wait,
    /// Whisper is loading: give it time.
    Hold,
}

pub fn plan(job: &Job, whisper_serving: bool, phase: Phase, go_ahead: bool, hour: u32) -> Plan {
    if whisper_serving {
        return Plan::Run;
    }
    match phase {
        Phase::Starting | Phase::Stopping => Plan::Hold,
        Phase::Idle | Phase::Failed => Plan::Switch,
        Phase::Serving => {
            if go_ahead || (job.when == When::Tonight && in_night_window(hour)) {
                Plan::Switch
            } else {
                Plan::Wait
            }
        }
    }
}

/// Owner go-aheads (`POST /jobs/{id}/run`), by job id.
static GO_AHEAD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The queue runner: every few seconds, move the oldest job along.
pub async fn run_forever(state: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let Some(jobs) = state.jobs() else { continue };
        let Some(job) = jobs.next() else {
            continue;
        };
        let whisper = serving_whisper(&state).await;
        let life = state.lifecycle();
        let go = GO_AHEAD.lock().expect("go mutex").contains(&job.id);
        // The chips are busy (a model, Lab, Orchard): transcribe on the CPU
        // instead of switching, unless the owner asked for the chips.
        let chips_busy = state.chips_holder().is_some()
            || matches!(
                life.phase,
                Phase::Serving | Phase::Starting | Phase::Stopping
            );
        if whisper.is_none() && chips_busy && !go && job.status != JobStatus::Switching {
            if let Some(cpu) = crate::voice::cpu_whisper().await {
                let id = job.id.clone();
                transcribe(&state, &jobs, job, &cpu).await;
                GO_AHEAD.lock().expect("go mutex").retain(|g| g != &id);
                continue;
            }
        }
        if state.chips_holder().is_some() {
            continue;
        }
        match plan(&job, whisper.is_some(), life.phase, go, local_hour()) {
            Plan::Hold => {}
            Plan::Wait => {
                if job.status != JobStatus::Waiting {
                    jobs.update(&job.id, |j| j.status = JobStatus::Waiting);
                }
            }
            Plan::Switch => {
                if job.status == JobStatus::Switching {
                    continue; // already asked; waiting for it to load
                }
                let previous = life
                    .model
                    .clone()
                    .or(life.served_id.clone())
                    .filter(|_| life.phase == Phase::Serving);
                jobs.update(&job.id, |j| {
                    j.status = JobStatus::Switching;
                    j.restore = previous.clone();
                });
                if let Err(e) = crate::modes::start(&state, "transcribe").await {
                    jobs.update(&job.id, |j| {
                        j.status = JobStatus::Failed;
                        j.error = Some(e);
                    });
                }
            }
            Plan::Run => {
                let base = whisper.expect("checked");
                let id = job.id.clone();
                transcribe(&state, &jobs, job, &base).await;
                GO_AHEAD.lock().expect("go mutex").retain(|g| g != &id);
                // Queue drained: put back what was serving before the queue took over.
                if jobs.next().is_none() {
                    restore_previous(&state, &jobs).await;
                }
            }
        }
    }
}

/// After the last job: start whatever the queue switched away from.
async fn restore_previous(state: &AppState, jobs: &Jobs) {
    let restore = jobs
        .list()
        .into_iter()
        .filter(|j| j.status == JobStatus::Done || j.status == JobStatus::Failed)
        .filter_map(|j| j.restore.clone().map(|r| (j.id.clone(), r)))
        .next();
    if let Some((id, model)) = restore {
        jobs.update(&id, |j| j.restore = None);
        let model = crate::notify::display_model(&model);
        eprintln!("tt-station-agentd: jobs done; starting {model} again");
        let _ = crate::routes::begin_serve(state, &model, false).await;
    }
}

/// The Whisper server's root URL, when one is serving.
async fn serving_whisper(state: &AppState) -> Option<String> {
    let endpoint = state.endpoint()?;
    let life = state.lifecycle();
    let probe = state.clone();
    let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    let named = [life.model, life.served_id, Some(endpoint.model.clone())]
        .into_iter()
        .flatten()
        .any(|m| m.to_lowercase().contains("whisper"));
    (life.phase == Phase::Serving && (kind == "speech" || named)).then(|| {
        endpoint
            .base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string()
    })
}

/// Probe a media file's duration (seconds) with ffprobe.
fn duration(path: &Path) -> Option<f64> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A piece of the recording: its file, where it starts, and how long it is.
pub struct Piece {
    pub path: PathBuf,
    pub offset: f64,
    pub length: f64,
}

/// Cut `input` into 16 kHz mono WAV pieces in `dir`, at pauses (see
/// [`choose_cuts`]).
fn split(input: &Path, dir: &Path, duration: f64) -> Result<Vec<Piece>, String> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let detect = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(input)
        .args([
            "-vn",
            "-af",
            "silencedetect=noise=-35dB:d=0.35",
            "-f",
            "null",
            "-",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run ffmpeg: {e}"))?;
    let cuts = choose_cuts(
        duration,
        &parse_silences(&String::from_utf8_lossy(&detect.stderr)),
    );
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(input)
        .args([
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "segment",
        ]);
    if cuts.is_empty() {
        cmd.args(["-segment_time", "100000"]);
    } else {
        let times: Vec<String> = cuts.iter().map(|c| format!("{c:.3}")).collect();
        cmd.args(["-segment_times", &times.join(",")]);
    }
    let out = cmd
        .arg(dir.join("chunk%04d.wav"))
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run ffmpeg: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "couldn't read that recording: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .unwrap_or("")
        ));
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return Err("that file has no audio".to_string());
    }
    let mut bounds = vec![0.0];
    bounds.extend(cuts.iter().copied());
    Ok(paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let offset = bounds.get(i).copied().unwrap_or(0.0);
            let end = bounds.get(i + 1).copied().unwrap_or(duration);
            Piece {
                path,
                offset,
                length: (end - offset).max(0.0),
            }
        })
        .collect())
}

async fn transcribe(state: &AppState, jobs: &Jobs, job: Job, base: &str) {
    let dir = jobs.job_dir(&job.id);
    let Some(input) = std::fs::read_dir(&dir).ok().and_then(|it| {
        it.flatten()
            .map(|e| e.path())
            .find(|p| p.file_stem().is_some_and(|s| s == "input"))
    }) else {
        jobs.update(&job.id, |j| {
            j.status = JobStatus::Failed;
            j.error = Some("the uploaded file is missing".into());
        });
        return;
    };
    let chunk_dir = dir.join("chunks");
    let (input2, chunk_dir2) = (input.clone(), chunk_dir.clone());
    let split_result = tokio::task::spawn_blocking(move || {
        let length =
            duration(&input2).ok_or_else(|| "couldn't read that recording's length".to_string())?;
        split(&input2, &chunk_dir2, length).map(|c| (c, Some(length)))
    })
    .await
    .unwrap_or_else(|e| Err(e.to_string()));
    let (chunks, length) = match split_result {
        Ok(v) => v,
        Err(e) => {
            jobs.update(&job.id, |j| {
                j.status = JobStatus::Failed;
                j.error = Some(e);
                j.finished = Some(unix_now());
            });
            return;
        }
    };
    let offsets: Vec<f64> = chunks.iter().map(|p| p.offset).collect();
    let progress_path = dir.join("progress.json");
    let (start, mut segments) =
        resume_point(crate::fsio::read_json_or_backup(&progress_path), &offsets);
    if start > 0 {
        eprintln!(
            "tt-station-agentd: job {}: resuming at chunk {} of {}",
            job.id,
            start + 1,
            chunks.len()
        );
    }
    jobs.update(&job.id, |j| {
        j.status = JobStatus::Running;
        j.started.get_or_insert(unix_now());
        j.total = chunks.len() as u32;
        j.done = start as u32;
        j.duration_s = length;
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .expect("client");
    let on_cpu = crate::voice::cpu_whisper().await.as_deref() == Some(base);
    let api_key =
        std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
    for (i, piece) in chunks.iter().enumerate().skip(start) {
        let chunk = &piece.path;
        if jobs.is_cancelled(&job.id) {
            let _ = std::fs::remove_dir_all(&chunk_dir);
            return;
        }
        let offset = piece.offset;
        let bytes = match tokio::fs::read(chunk).await {
            Ok(b) => b,
            Err(e) => {
                jobs.update(&job.id, |j| {
                    j.status = JobStatus::Failed;
                    j.error = Some(e.to_string());
                });
                return;
            }
        };
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(bytes)
                    .file_name(format!("chunk{i:04}.wav"))
                    .mime_str("audio/wav")
                    .expect("mime"),
            )
            .text("response_format", "verbose_json")
            .text("return_timestamps", "true");
        // The CPU Whisper serves several models: name the accurate one.
        let form = if on_cpu {
            form.text("model", crate::voice::RECORDING_MODEL)
        } else {
            form
        };
        let answer = client
            .post(format!("{base}/v1/audio/transcriptions"))
            .bearer_auth(&api_key)
            .multipart(form)
            .send()
            .await;
        let answer = match answer {
            Ok(r) if r.status().is_success() => r.json::<Value>().await.map_err(|e| e.to_string()),
            Ok(r) => Err(format!(
                "Whisper answered {}: {}",
                r.status(),
                r.text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect::<String>()
            )),
            Err(e) => Err(format!("couldn't reach Whisper: {e}")),
        };
        match answer {
            Ok(value) => {
                segments.extend(segments_from_whisper(&value, offset, piece.length));
                // The chunk is on disk before it counts, so a restart resumes
                // after it (and never skips one).
                let progress = Progress {
                    offsets: offsets.clone(),
                    done: i + 1,
                    segments: segments.clone(),
                };
                let (path, partial) = (dir.join("transcript.txt"), to_text(&segments));
                let progress_file = progress_path.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    use crate::fsio::{write_atomic, write_json, PRIVATE};
                    let _ = write_json(&progress_file, &progress, PRIVATE);
                    // Keep partial results, so a long job shows progress.
                    write_atomic(&path, partial.as_bytes(), PRIVATE)
                })
                .await;
                jobs.update(&job.id, |j| j.done = i as u32 + 1);
            }
            Err(e) => {
                jobs.update(&job.id, |j| {
                    j.status = JobStatus::Failed;
                    j.error = Some(e);
                    j.finished = Some(unix_now());
                });
                return;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&chunk_dir);
    let _ = std::fs::remove_file(&progress_path);
    let text = to_text(&segments);
    // What was said in a meeting: readable by the box's user only.
    let transcript_json = serde_json::to_vec_pretty(
        &json!({ "title": job.title, "duration_s": length, "segments": segments }),
    )
    .unwrap_or_default();
    let (srt, folder) = (to_srt(&segments), dir.clone());
    let text_copy = text.clone();
    let _ = tokio::task::spawn_blocking(move || {
        use crate::fsio::{write_atomic, PRIVATE};
        let _ = write_atomic(
            &folder.join("transcript.txt"),
            text_copy.as_bytes(),
            PRIVATE,
        );
        let _ = write_atomic(&folder.join("transcript.srt"), srt.as_bytes(), PRIVATE);
        let _ = write_atomic(&folder.join("transcript.json"), &transcript_json, PRIVATE);
    })
    .await;
    let household = crate::export::settings(state);
    // The recording was only needed to make the transcript.
    let keep_audio = job.keep_audio.unwrap_or(household.keep_audio);
    if !keep_audio {
        let _ = std::fs::remove_file(&input);
    }
    let to_documents = job.to_documents.unwrap_or(household.transcripts_to_library);
    if to_documents && !text.is_empty() {
        if let Some(docs) = state.docs() {
            let name = format!("Transcript: {}", job.title);
            let now = unix_now();
            let _ = tokio::task::spawn_blocking(move || docs.add(&name, &text, now)).await;
        }
    }
    let done = jobs.update(&job.id, |j| {
        j.status = JobStatus::Done;
        j.finished = Some(unix_now());
        j.to_documents = Some(to_documents);
        j.keep_audio = Some(keep_audio);
    });
    if let Some(done) = done {
        let minutes = done
            .duration_s
            .map(|d| format!(" ({} min)", (d / 60.0).round() as u64))
            .unwrap_or_default();
        state.notifier().emit(crate::notify::Draft {
            kind: "job_done",
            title: format!("Transcript ready: {}", done.title),
            body: format!(
                "{}{} transcribed on your box.{}",
                done.title,
                minutes,
                if to_documents {
                    " Ask the assistant about it."
                } else {
                    ""
                }
            ),
        });
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn queue(state: &AppState) -> Result<std::sync::Arc<Jobs>, (StatusCode, Json<ErrorResponse>)> {
    state
        .jobs()
        .ok_or_else(|| err(StatusCode::CONFLICT, "This agent has no job queue."))
}

/// The job `id`, if `caller` may see it; else `404`, the same as a job that
/// doesn't exist (a member doesn't learn other people's job ids).
fn job_for(
    jobs: &Jobs,
    id: &str,
    caller: &crate::routes::Caller,
) -> Result<Job, (StatusCode, Json<ErrorResponse>)> {
    jobs.get(id)
        .filter(|j| visible(j, caller))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such job."))
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    pub name: String,
    #[serde(default)]
    pub when: Option<String>,
    /// Add the transcript to the shared Library (default: the household's).
    #[serde(default)]
    pub documents: Option<bool>,
    /// Keep the recording after it's transcribed (default: the household's).
    #[serde(default)]
    pub keep_audio: Option<bool>,
    /// Let everyone on the box see the job and its transcript.
    #[serde(default)]
    pub shared: Option<bool>,
}

/// `POST /jobs/transcribe?name=meeting.m4a&when=now|tonight[&documents=true]
/// [&keep_audio=true][&shared=true]` (authed), body = the recording (any format
/// ffmpeg reads). `201` with the job.
pub(crate) async fn post_transcribe(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    axum::extract::Query(q): axum::extract::Query<UploadQuery>,
    body: axum::body::Body,
) -> Result<(StatusCode, Json<Job>), (StatusCode, Json<ErrorResponse>)> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let jobs = queue(&state)?;
    let title: String = q
        .name
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if title.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Give the recording a name."));
    }
    let when = match q.when.as_deref() {
        Some("tonight") => When::Tonight,
        _ => When::Now,
    };
    let id = crate::pairing::issue_pair_id()[..12].to_string();
    let dir = jobs.job_dir(&id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let ext = Path::new(&title)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin")
        .to_lowercase();
    let path = dir.join(format!("input.{ext}"));
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let mut stream = body.into_data_stream();
    let mut written = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| err(StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")))?;
        written += chunk.len();
        if written > MAX_UPLOAD_BYTES {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "That recording is too big (4 GB at most).",
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    file.flush().await.ok();
    if written == 0 {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err(err(StatusCode::BAD_REQUEST, "The upload was empty."));
    }
    let new = NewJob {
        to_documents: q.documents,
        keep_audio: q.keep_audio,
        started_by: caller.device_id(),
        started_by_name: caller.device.as_ref().map(|d| d.name.clone()),
        shared: q.shared.unwrap_or(false),
    };
    let job = jobs.add_with(id, "transcribe", &title, when, new);
    Ok((StatusCode::CREATED, Json(job)))
}

/// `GET /jobs` (authed): newest first; a member's own and the shared ones,
/// every job for an owner.
pub(crate) async fn get_jobs(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let jobs = queue(&state)?;
    let mine: Vec<Job> = jobs
        .list()
        .into_iter()
        .filter(|j| visible(j, &caller))
        .collect();
    Ok(Json(json!({ "jobs": mine })))
}

#[derive(Debug, Deserialize)]
pub struct ShareBody {
    pub shared: bool,
}

/// `POST /jobs/{id}/share` `{shared}` (authed): share a job and its transcript
/// with everyone on the box, or stop. Its starter or an owner.
pub(crate) async fn post_share(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ShareBody>,
) -> Result<Json<Job>, (StatusCode, Json<ErrorResponse>)> {
    let jobs = queue(&state)?;
    let job = job_for(&jobs, &id, &caller)?;
    let starter = job.started_by.is_some() && job.started_by == caller.device_id();
    if !(caller.owner() || starter) {
        return Err(err(
            StatusCode::FORBIDDEN,
            "Only whoever started this job, or an owner, can share it.",
        ));
    }
    let job = jobs
        .update(&id, |j| j.shared = body.shared)
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such job."))?;
    crate::audit::record_as(
        &state,
        &caller.actor(),
        "job_share",
        &id,
        if body.shared { "shared" } else { "private" },
    );
    Ok(Json(job))
}

/// `POST /jobs/{id}/run` (authed): the owner's go-ahead to switch the box now.
pub(crate) async fn post_run_now(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let jobs = queue(&state)?;
    job_for(&jobs, &id, &caller)?;
    GO_AHEAD.lock().expect("go mutex").push(id.clone());
    jobs.update(&id, |j| {
        if j.status == JobStatus::Waiting {
            j.status = JobStatus::Queued;
        }
    });
    Ok(Json(json!({ "status": "go" })))
}

/// `DELETE /jobs/{id}` (authed): cancel or delete.
pub(crate) async fn delete_job(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let jobs = queue(&state)?;
    job_for(&jobs, &id, &caller)?;
    if jobs.remove(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(err(StatusCode::NOT_FOUND, "No such job."))
    }
}

#[derive(Debug, Deserialize)]
pub struct ResultQuery {
    #[serde(default)]
    pub format: Option<String>,
}

/// `GET /jobs/{id}/result?format=txt|srt|json` (authed): the transcript (also
/// the partial one while it runs).
pub(crate) async fn get_result(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<ResultQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Ok(jobs) = queue(&state) else {
        return err(StatusCode::CONFLICT, "No job queue.").into_response();
    };
    if job_for(&jobs, &id, &caller).is_err() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return err(StatusCode::NOT_FOUND, "No such job.").into_response();
    }
    let (file, content_type) = match q.format.as_deref() {
        Some("srt") => ("transcript.srt", "application/x-subrip"),
        Some("json") => ("transcript.json", "application/json"),
        _ => ("transcript.txt", "text/plain; charset=utf-8"),
    };
    match tokio::fs::read(jobs.job_dir(&id).join(file)).await {
        Ok(bytes) => ([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response(),
        Err(_) => err(StatusCode::NOT_FOUND, "No transcript yet.").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(when: When) -> Job {
        Job {
            id: "a".into(),
            kind: "transcribe".into(),
            title: "m.m4a".into(),
            when,
            status: JobStatus::Queued,
            done: 0,
            total: 0,
            duration_s: None,
            created: 0,
            started: None,
            finished: None,
            error: None,
            to_documents: None,
            keep_audio: None,
            restore: None,
            started_by: None,
            started_by_name: None,
            shared: false,
        }
    }

    #[test]
    fn the_queue_switches_waits_or_runs_as_the_box_allows() {
        let now = job(When::Now);
        let tonight = job(When::Tonight);
        assert_eq!(plan(&now, true, Phase::Serving, false, 14), Plan::Run);
        assert_eq!(
            plan(&now, false, Phase::Idle, false, 14),
            Plan::Switch,
            "idle box: just go"
        );
        assert_eq!(
            plan(&now, false, Phase::Serving, false, 14),
            Plan::Wait,
            "busy: ask first"
        );
        assert_eq!(
            plan(&now, false, Phase::Serving, true, 14),
            Plan::Switch,
            "owner said go"
        );
        assert_eq!(plan(&tonight, false, Phase::Serving, false, 14), Plan::Wait);
        assert_eq!(
            plan(&tonight, false, Phase::Serving, false, 2),
            Plan::Switch,
            "2 am"
        );
        assert_eq!(plan(&now, false, Phase::Starting, false, 14), Plan::Hold);
    }

    #[test]
    fn chunks_are_stitched_on_one_timeline() {
        // tt-media-server's field names (OpenAI's `start`/`end` also work).
        let first = json!({ "segments": [
            { "id": 0, "speaker": "SPEAKER_00", "start_time": 0.0, "end_time": 4.2, "text": " Welcome everyone." },
            { "start": 4.2, "end": 9.0, "text": " " }
        ]});
        let second = json!({ "text": " Let's begin with the roadmap. " });
        let mut all = segments_from_whisper(&first, 0.0, 270.0);
        all.extend(segments_from_whisper(&second, 270.0, 120.0));
        assert_eq!(all.len(), 2, "blank segments dropped");
        assert_eq!(all[1].start, 270.0);
        assert_eq!(all[1].end, 390.0);
        assert_eq!(
            to_text(&all),
            "[00:00:00] Welcome everyone.\n[00:04:30] Let's begin with the roadmap."
        );
        assert!(to_srt(&all).starts_with("1\n00:00:00,000 --> 00:00:04,200\nWelcome everyone.\n"));
    }

    #[test]
    fn jobs_persist_and_interrupted_ones_go_back_in_line() {
        let dir = std::env::temp_dir().join(format!("tt-jobs-{}", std::process::id()));
        let jobs = Jobs::open(&dir);
        let j = jobs.add("abc123".into(), "transcribe", "call.m4a", When::Now, true);
        jobs.update(&j.id, |j| {
            j.status = JobStatus::Running;
            j.done = 3;
        });
        let reopened = Jobs::open(&dir);
        let back = reopened.get("abc123").unwrap();
        // Back in line, keeping the chunks it had done (see `Progress`).
        assert_eq!((back.status, back.done), (JobStatus::Queued, 3));
        assert!(reopened.remove("abc123"));
        assert!(Jobs::open(&dir).list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cuts_fall_in_pauses_under_whisper_s_limit() {
        // Pauses every 10 s: each cut is the latest pause within 55 s.
        let silences: Vec<f64> = (1..60).map(|i| i as f64 * 10.0 + 0.2).collect();
        let cuts = choose_cuts(300.0, &silences);
        assert_eq!(cuts, vec![50.2, 100.2, 150.2, 200.2, 250.2]);
        // No pauses at all: hard cuts at the limit.
        assert_eq!(choose_cuts(120.0, &[]), vec![55.0, 110.0]);
        // Short recordings aren't cut.
        assert!(choose_cuts(54.0, &[10.0]).is_empty());
        // Every piece is within the limit.
        let mut prev = 0.0;
        for c in choose_cuts(1000.0, &[3.0, 70.0, 71.0, 400.0]) {
            assert!(c - prev <= MAX_CHUNK_SECONDS + 1e-9);
            prev = c;
        }
    }

    #[test]
    fn silences_parse_from_ffmpeg() {
        let stderr = "[silencedetect @ 0x1] silence_start: 12.5\n[silencedetect @ 0x1] silence_end: 13.1 | silence_duration: 0.6\n\
                      [silencedetect @ 0x1] silence_start: -0.01\n[silencedetect @ 0x1] silence_end: 0.4 | silence_duration: 0.41";
        assert_eq!(parse_silences(stderr), vec![12.8, 0.2]);
    }

    #[test]
    fn night_window() {
        assert!(in_night_window(1) && in_night_window(5));
        assert!(!in_night_window(0) && !in_night_window(6) && !in_night_window(14));
    }

    fn device(id: &str, owner: bool) -> crate::routes::Caller {
        crate::routes::Caller {
            local: false,
            device: Some(crate::sessions::Session {
                id: id.into(),
                token_sha256: String::new(),
                name: id.into(),
                platform: None,
                created: None,
                last_used: 0,
                owner,
            }),
        }
    }

    #[test]
    fn members_see_their_own_and_shared_jobs_owners_see_all() {
        let mut mine = job(When::Now);
        mine.started_by = Some("phone-a".into());
        let mut theirs = mine.clone();
        theirs.started_by = Some("phone-b".into());
        let mut boxes = mine.clone();
        boxes.started_by = None; // the recorder, or the box itself
        let (a, owner) = (device("phone-a", false), device("laptop", true));
        assert!(visible(&mine, &a));
        assert!(!visible(&theirs, &a), "someone else's");
        assert!(!visible(&boxes, &a), "the box's own: owners only");
        theirs.shared = true;
        assert!(visible(&theirs, &a), "shared with the household");
        for j in [&mine, &theirs, &boxes] {
            assert!(visible(j, &owner));
        }
        let local = crate::routes::Caller {
            local: true,
            device: None,
        };
        assert!(visible(&boxes, &local), "the box's own user is an owner");
    }

    #[test]
    fn a_restart_resumes_after_the_last_saved_chunk() {
        let offsets = [0.0, 55.0, 110.0];
        let seg = |t: f64| Segment {
            start: t,
            end: t + 1.0,
            text: format!("at {t}"),
        };
        let saved = Progress {
            offsets: offsets.to_vec(),
            done: 2,
            segments: vec![seg(0.0), seg(55.0)],
        };
        let (start, segments) = resume_point(Some(saved.clone()), &offsets);
        assert_eq!((start, segments.len()), (2, 2));
        // Cut differently (another input, another cutter): start over.
        assert_eq!(resume_point(Some(saved.clone()), &[0.0, 50.0, 110.0]).0, 0);
        assert_eq!(resume_point(Some(saved), &[0.0, 55.0]).0, 0);
        assert_eq!(resume_point(None, &offsets).0, 0);
    }

    #[test]
    fn old_finished_jobs_expire_and_imported_ones_are_adopted() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(tmp.path());
        for (id, status, finished) in [
            ("old", JobStatus::Done, 100),
            ("new", JobStatus::Done, 10_000),
            ("oldrun", JobStatus::Queued, 0),
        ] {
            jobs.add(id.into(), "transcribe", "t", When::Now, true);
            jobs.update(id, |j| {
                j.status = status;
                j.finished = (finished > 0).then_some(finished);
                j.created = 50;
            });
        }
        assert_eq!(jobs.expire(5_000), 1);
        assert!(jobs.get("old").is_none() && !tmp.path().join("old").exists());
        assert!(jobs.get("new").is_some());
        assert!(
            jobs.get("oldrun").is_some(),
            "unfinished work is never expired"
        );
        assert!(jobs.has_pending());

        // A job folder written by an import is picked up; an unfinished one isn't.
        let mut imported = job(When::Now);
        imported.id = "imp".into();
        imported.status = JobStatus::Done;
        crate::fsio::write_json(&tmp.path().join("imp/job.json"), &imported, 0o600).unwrap();
        assert!(jobs.adopt("imp").is_some());
        assert!(jobs.get("imp").is_some());
        imported.id = "imp2".into();
        imported.status = JobStatus::Queued;
        crate::fsio::write_json(&tmp.path().join("imp2/job.json"), &imported, 0o600).unwrap();
        assert!(jobs.adopt("imp2").is_none());
    }

    #[test]
    fn the_recorder_s_jobs_leave_library_and_audio_to_the_household() {
        let tmp = tempfile::tempdir().unwrap();
        let jobs = Jobs::open(tmp.path());
        let j = jobs.add("r1".into(), "transcribe", "Meeting", When::Now, true);
        assert_eq!((j.to_documents, j.keep_audio), (None, None));
        let j = jobs.add("r2".into(), "transcribe", "Meeting", When::Now, false);
        assert_eq!(j.to_documents, Some(false));
        // Older job files said `to_documents: true`: they still read.
        let old: Job = serde_json::from_value(json!({
            "id": "o", "kind": "transcribe", "title": "t", "when": "now", "status": "done",
            "done": 1, "total": 1, "duration_s": null, "created": 1, "started": null,
            "finished": 2, "error": null, "to_documents": true
        }))
        .unwrap();
        assert_eq!(
            (old.to_documents, old.keep_audio, old.shared),
            (Some(true), None, false)
        );
    }
}
