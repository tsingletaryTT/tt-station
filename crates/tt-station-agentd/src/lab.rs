// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Lab mode: [tt-lab](https://github.com/tenstorrent/tt-lab) on the phone.
//!
//! tt-lab runs gpt-oss-20b/120b directly on the Blackhole chips with its own
//! firmware -- no TT-Metal, no vLLM -- and can check every output logit
//! bit-for-bit against its host-side device proxy. It's a lab, not a server:
//! one prompt, greedy decoding, the chips opened exclusively.
//!
//! - `GET /lab`: is tt-lab built, which models are prepared, what's running,
//!   recent runs.
//! - `POST /lab/prepare {model}`: download the GGUF and build the `.ttq`
//!   sidecar, in the background, with progress.
//! - `POST /lab/run {model, prompt, tokens, tiles, check, profile,
//!   stop_serving}`: run it, streaming tt-lab's output as server-sent events.
//!   The chips must be idle: a serving model is refused unless the client
//!   confirmed stopping it (`stop_serving: true`). While a run holds the chips,
//!   the agent's tt-smi telemetry pauses, and model starts and chip resets are
//!   refused -- tt-lab owns the devices exclusively.
//!
//! With gozer installed, a run holds a whole-box lease (`gozer acquire --chips
//! all`, `who` `tt-station:<port>:lab`, owned by this agent) from before its
//! chip reset until tt-lab exits, then releases it (gozer resets the chips).
//! Chips held by anyone else refuse the run with a 409 naming the holder.
//! Without gozer, Lab runs unleased as it always has, and `GET /lab`'s
//! `leasing` says so.
//!
//! Preparing and running are an owner's (`routes::OwnerAuth`): tt-lab runs as
//! the box's own account, with the chips opened directly.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

use crate::routes::{AppState, BearerAuth, ErrorResponse, Phase};
use crate::serving::docker::RealCommandRunner;

/// A model tt-lab can run.
#[derive(Debug, Clone, Copy)]
pub struct LabModel {
    pub id: &'static str,
    pub title: &'static str,
    pub gguf: &'static str,
    pub ttq: &'static str,
    pub url: &'static str,
    pub download_gb: f32,
    /// Allowed `--tiles` on this box, best first.
    pub tiles: &'static [u32],
}

pub const MODELS: [LabModel; 2] = [
    LabModel {
        id: "gpt-oss-20b",
        title: "gpt-oss 20B",
        gguf: "gpt-oss-20b-MXFP4.gguf",
        ttq: "gpt-oss-20b.ttq",
        url: "https://huggingface.co/ggml-org/gpt-oss-20b-GGUF/resolve/main/gpt-oss-20b-MXFP4.gguf",
        download_gb: 12.1,
        tiles: &[32, 8],
    },
    LabModel {
        id: "gpt-oss-120b",
        title: "gpt-oss 120B",
        gguf: "gpt-oss-120b-MXFP4.gguf",
        ttq: "gpt-oss-120b.ttq",
        url:
            "https://huggingface.co/ggml-org/gpt-oss-120b-GGUF/resolve/main/gpt-oss-120b-MXFP4.gguf",
        download_gb: 63.4,
        tiles: &[32],
    },
];

pub fn model(id: &str) -> Option<&'static LabModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// A background download + requant.
#[derive(Debug, Clone, Serialize)]
pub struct PrepareJob {
    pub model: String,
    /// `downloading`, `requantizing`, `done`, `failed`
    pub stage: String,
    pub bytes: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
}

/// One finished run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub model: String,
    pub tiles: u32,
    pub prompt: String,
    pub check: bool,
    pub ok: bool,
    pub seconds: f64,
    pub at: u64,
    /// Key numbers tt-lab printed (see [`parse_stats`]).
    pub stats: Value,
    pub tail: Vec<String>,
}

pub struct Lab {
    pub binary: PathBuf,
    pub models_dir: PathBuf,
    job: Arc<Mutex<Option<PrepareJob>>>,
    running: Mutex<Option<Value>>,
    history: Mutex<VecDeque<RunSummary>>,
}

impl std::fmt::Debug for Lab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lab").field("binary", &self.binary).finish()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

impl Lab {
    pub fn new(binary: impl Into<PathBuf>, models_dir: impl Into<PathBuf>) -> Lab {
        Lab {
            binary: binary.into(),
            models_dir: models_dir.into(),
            job: Arc::new(Mutex::new(None)),
            running: Mutex::new(None),
            history: Mutex::new(VecDeque::new()),
        }
    }

    /// The default layout: `~/code/tt-lab/_out/tt-lab` (or `$TT_LAB_BIN`) and
    /// `~/models` (or `$TT_LAB_MODELS`).
    pub fn from_env() -> Lab {
        let home = std::env::var("HOME").unwrap_or_default();
        Lab::new(
            std::env::var("TT_LAB_BIN")
                .unwrap_or_else(|_| format!("{home}/code/tt-lab/_out/tt-lab")),
            std::env::var("TT_LAB_MODELS").unwrap_or_else(|_| format!("{home}/models")),
        )
    }

    pub fn installed(&self) -> bool {
        self.binary.is_file()
    }

    pub fn gguf(&self, m: &LabModel) -> PathBuf {
        self.models_dir.join(m.gguf)
    }

    pub fn ttq(&self, m: &LabModel) -> PathBuf {
        self.models_dir.join(m.ttq)
    }

    fn job(&self) -> Option<PrepareJob> {
        self.job.lock().expect("lab mutex poisoned").clone()
    }

    pub fn status(&self) -> Value {
        let job = self.job();
        let models: Vec<Value> = MODELS
            .iter()
            .map(|m| {
                let gguf = file_size(&self.gguf(m));
                let ttq = file_size(&self.ttq(m));
                let preparing = job.as_ref().filter(|j| j.model == m.id && matches!(j.stage.as_str(), "downloading" | "requantizing"));
                json!({
                    "id": m.id,
                    "title": m.title,
                    "download_gb": m.download_gb,
                    "tiles": m.tiles,
                    "downloaded": gguf.is_some() && preparing.is_none_or(|j| j.stage != "downloading"),
                    "ready": gguf.is_some() && ttq.is_some() && preparing.is_none(),
                    "preparing": preparing,
                })
            })
            .collect();
        json!({
            "installed": self.installed(),
            "binary": self.binary,
            "models": models,
            "job": job,
            "running": self.running.lock().expect("lab mutex poisoned").clone(),
            "history": self.history.lock().expect("lab mutex poisoned").iter().rev().cloned().collect::<Vec<_>>(),
        })
    }

    /// Start downloading/requantizing `m` in the background.
    pub fn prepare(&self, m: &'static LabModel) -> Result<(), String> {
        if !self.installed() {
            return Err("tt-lab isn't built on this box yet.".to_string());
        }
        if let Some(job) = self.job() {
            if matches!(job.stage.as_str(), "downloading" | "requantizing") {
                return Err(format!("Already preparing {}.", job.model));
            }
        }
        let gguf = self.gguf(m);
        let ttq = self.ttq(m);
        let binary = self.binary.clone();
        let job = Arc::clone(&self.job);
        let set = move |stage: &str, bytes: u64, total: Option<u64>, error: Option<String>| {
            *job.lock().expect("lab mutex poisoned") = Some(PrepareJob {
                model: m.id.to_string(),
                stage: stage.to_string(),
                bytes,
                total,
                error,
            });
        };
        let total = (m.download_gb as f64 * 1e9) as u64;
        set(
            if gguf.is_file() {
                "requantizing"
            } else {
                "downloading"
            },
            0,
            Some(total),
            None,
        );
        std::thread::spawn(move || {
            if let Some(dir) = gguf.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if !gguf.is_file() {
                let part = gguf.with_extension("gguf.part");
                let mut child = match std::process::Command::new("curl")
                    .args(["-sfL", "-C", "-", "-o"])
                    .arg(&part)
                    .arg(m.url)
                    .stdin(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(c) => c,
                    Err(e) => {
                        return set(
                            "failed",
                            0,
                            Some(total),
                            Some(format!("couldn't start the download: {e}")),
                        )
                    }
                };
                loop {
                    let bytes = file_size(&part).unwrap_or(0);
                    set("downloading", bytes, Some(total), None);
                    match child.try_wait() {
                        Ok(Some(status)) if status.success() => break,
                        Ok(Some(status)) => {
                            return set(
                                "failed",
                                bytes,
                                Some(total),
                                Some(format!(
                                    "the download stopped ({status}); try again to resume"
                                )),
                            )
                        }
                        Ok(None) => std::thread::sleep(std::time::Duration::from_secs(2)),
                        Err(e) => return set("failed", bytes, Some(total), Some(e.to_string())),
                    }
                }
                if let Err(e) = std::fs::rename(&part, &gguf) {
                    return set("failed", 0, Some(total), Some(e.to_string()));
                }
            }
            if !ttq.is_file() {
                set(
                    "requantizing",
                    file_size(&gguf).unwrap_or(0),
                    Some(total),
                    None,
                );
                let out = std::process::Command::new(&binary)
                    .args(["requant", "-m"])
                    .arg(&gguf)
                    .arg("-o")
                    .arg(&ttq)
                    .stdin(std::process::Stdio::null())
                    .output();
                match out {
                    Ok(o) if o.status.success() => {}
                    Ok(o) => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        return set(
                            "failed",
                            0,
                            Some(total),
                            Some(format!(
                                "requant failed: {}",
                                err.lines().last().unwrap_or("")
                            )),
                        );
                    }
                    Err(e) => return set("failed", 0, Some(total), Some(e.to_string())),
                }
            }
            set("done", file_size(&gguf).unwrap_or(0), Some(total), None);
        });
        Ok(())
    }

    fn record(&self, summary: RunSummary) {
        let mut history = self.history.lock().expect("lab mutex poisoned");
        history.push_back(summary);
        while history.len() > 20 {
            history.pop_front();
        }
    }
}

/// The numbers worth showing, from tt-lab's own output lines:
///
/// ```text
/// loaded TTQ tensors into device DRAM: 21.362 GiB, 32 channels, 4.320 s, peak ...
/// prefill: 17 tokens, 0.086 s, 198.060 t/s
/// generated: 200 tokens
/// decode: 199 evals, 1.305 s, 152.479 t/s
/// silicon token commands total 0.0115299 s, avg 0.00576496 s/token
/// silicon logits exact match: 2 tokens, 201088 values each
///   experts         2127441 cycles/token   42.3%       (with --profile)
/// host: setup 0.040 ms/token, command 6.288 ms/token, ...
/// ```
pub fn parse_stats(lines: &[String]) -> Value {
    let mut stats = serde_json::Map::new();
    let mut profile = Vec::new();
    let num = |s: &str| s.trim().trim_end_matches(',').parse::<f64>().ok();
    // The number right before `unit` in `line`.
    let before = |line: &str, unit: &str| -> Option<f64> {
        let idx = line.find(unit)?;
        num(line[..idx].trim_end().rsplit([' ', ',']).next()?)
    };
    for line in lines {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("loaded TTQ tensors into device DRAM:") {
            stats.insert("dram_gib".into(), json!(before(rest, "GiB")));
            let secs = rest
                .split(',')
                .find(|p| p.trim().ends_with(" s"))
                .and_then(|p| num(p.trim().trim_end_matches(" s")));
            stats.insert("preload_s".into(), json!(secs));
        } else if l.starts_with("prefill:") {
            stats.insert("prefill_tps".into(), json!(before(l, "t/s")));
        } else if l.starts_with("decode:") {
            stats.insert("decode_tps".into(), json!(before(l, "t/s")));
        } else if let Some(rest) = l.strip_prefix("generated:") {
            stats.insert("tokens".into(), json!(before(rest, "tokens")));
        } else if l.starts_with("silicon token commands total") {
            if let Some(s) = before(l, "s/token") {
                stats.insert("ms_per_token".into(), json!((s * 1e5).round() / 100.0));
            }
        } else if l.starts_with("host:") {
            if let Some(ms) = l
                .split(',')
                .find(|p| p.contains("command"))
                .and_then(|p| before(p, "ms/token"))
            {
                stats.insert("ms_per_token".into(), json!(ms));
            }
        } else if l.starts_with("silicon logits exact match") {
            stats.insert("check".into(), json!("exact"));
        } else if l.to_lowercase().contains("mismatch") {
            stats.insert("check".into(), json!("mismatch"));
        } else if l.contains("cycles/token") && l.ends_with('%') && !l.starts_with("total") {
            let parts: Vec<&str> = l.split_whitespace().collect();
            if let Some(cyc_at) = parts.iter().position(|p| *p == "cycles/token") {
                if cyc_at >= 2 {
                    profile.push(json!({
                        "stage": parts[..cyc_at - 1].join(" "),
                        "cycles": num(parts[cyc_at - 1]),
                        "percent": parts.last().and_then(|p| num(p.trim_end_matches('%'))),
                    }));
                }
            }
        }
    }
    if !profile.is_empty() {
        stats.insert("profile".into(), Value::Array(profile));
    }
    Value::Object(stats)
}

#[derive(Debug, Deserialize)]
pub struct RunRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub tokens: Option<u32>,
    #[serde(default)]
    pub tiles: Option<u32>,
    #[serde(default)]
    pub check: bool,
    #[serde(default)]
    pub profile: bool,
    /// The client confirmed stopping what's serving.
    #[serde(default)]
    pub stop_serving: bool,
}

#[derive(Debug, Deserialize)]
pub struct PrepareRequest {
    pub model: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

fn lab(state: &AppState) -> Result<Arc<Lab>, (StatusCode, Json<ErrorResponse>)> {
    state.lab().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "The silicon check (tt-lab) isn't available on this agent.",
        )
    })
}

/// `GET /lab` (authed).
pub(crate) async fn get_lab(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let lab = lab(&state)?;
    let mut status = lab.status();
    let life = state.lifecycle();
    status["leasing"] = if state.gozer().is_some() {
        json!({ "gozer": true, "note": "Runs hold every chip under a gozer lease." })
    } else {
        json!({
            "gozer": false,
            "note": "gozer isn't installed, so runs take no chip lease (other gozer users won't see the chips as held).",
        })
    };
    status["serving"] = json!(match life.phase {
        Phase::Idle | Phase::Failed => None,
        _ => life
            .model
            .or(life.served_id)
            .map(|m| crate::notify::display_model(&m)),
    });
    Ok(Json(status))
}

/// `POST /lab/prepare` (authed).
pub(crate) async fn post_prepare(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<PrepareRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ErrorResponse>)> {
    let lab = lab(&state)?;
    let m = model(&req.model).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("No lab model {}.", req.model),
        )
    })?;
    lab.prepare(m).map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "status": "preparing", "model": m.id })),
    ))
}

/// Clears the busy flag however a run ends.
struct Busy(AppState);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.set_lab_busy(false);
    }
}

/// A Lab run's lease: the gozer it came from and what was granted.
type LabLease = Option<(crate::gozer::Capability, crate::gozer::Grant)>;

/// Take the whole-box lease a Lab run holds (`gozer acquire --chips all` as
/// `tt-station:<port>:lab`, owned by this agent, which is tt-lab's parent).
/// `None` without gozer: Lab runs unleased, as it always has. Chips held by
/// anyone are a 409 naming the holder; a gozer that can't be run is a 500
/// rather than an unleased run.
async fn take_lab_lease(
    state: &AppState,
    m: &LabModel,
) -> Result<LabLease, (StatusCode, Json<ErrorResponse>)> {
    let Some(capability) = state.gozer().cloned() else {
        return Ok(None);
    };
    let port = state.serving_port();
    let reason = format!("TT Station Lab: {} silicon check", m.title);
    let cap = capability.clone();
    let (outcome, holder) = tokio::task::spawn_blocking(move || {
        let runner = RealCommandRunner;
        let outcome = crate::gozer::acquire_owned(
            &runner,
            &cap,
            crate::gozer::DEFAULT_LEASE_CHIPS,
            None,
            std::process::id(),
            &crate::gozer::own_who(port, crate::gozer::LAB_LEASE_TAG),
            &reason,
        );
        let holder = matches!(outcome, crate::gozer::Outcome::Unavailable { .. })
            .then(|| crate::gozer::contention_detail(&runner, &cap, port))
            .flatten();
        (outcome, holder)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match outcome {
        crate::gozer::Outcome::Granted(grant) => Ok(Some((capability, grant))),
        crate::gozer::Outcome::Unavailable { .. } => Err(err(
            StatusCode::CONFLICT,
            format!(
                "The silicon check needs every chip, and {}.",
                holder.unwrap_or_else(|| "some of them are in use".to_string())
            ),
        )),
        crate::gozer::Outcome::Failed(message) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("couldn't take a chip lease for the silicon check: {message}"),
        )),
    }
}

/// Release a Lab run's lease (gozer resets the chips as it does).
async fn give_back(lease: LabLease) {
    let Some((capability, grant)) = lease else {
        return;
    };
    let released = tokio::task::spawn_blocking(move || {
        crate::gozer::release(&RealCommandRunner, &capability, &grant.lease_id)
            .map_err(|e| format!("couldn't release the Lab lease '{}': {e:#}", grant.lease_id))
    })
    .await;
    match released {
        Ok(Ok(())) => {}
        Ok(Err(e)) => eprintln!("tt-station-agentd: lab: {e}"),
        Err(e) => eprintln!("tt-station-agentd: lab: lease release task failed: {e}"),
    }
}

/// `POST /lab/run` (authed): stream a tt-lab run as server-sent events --
/// `{type: "line", text, stream}` for each output line, then `{type: "done",
/// ok, exit_code, seconds, stats, previous}`.
pub(crate) async fn post_run(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<RunRequest>,
) -> Result<
    Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>,
    (StatusCode, Json<ErrorResponse>),
> {
    let lab = lab(&state)?;
    if !lab.installed() {
        return Err(err(
            StatusCode::CONFLICT,
            "tt-lab isn't built on this box yet.",
        ));
    }
    let m = model(&req.model).ok_or_else(|| {
        err(
            StatusCode::BAD_REQUEST,
            format!("No lab model {}.", req.model),
        )
    })?;
    let (gguf, ttq) = (lab.gguf(m), lab.ttq(m));
    if !gguf.is_file() || !ttq.is_file() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{} isn't prepared yet: download it first.", m.title),
        ));
    }
    let tiles = req.tiles.unwrap_or(m.tiles[0]);
    if !m.tiles.contains(&tiles) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!(
                "{} runs on {} tiles here.",
                m.title,
                m.tiles
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        ));
    }
    let prompt = req.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Give it a prompt."));
    }
    let tokens = req
        .tokens
        .unwrap_or(if req.check { 4 } else { 128 })
        .clamp(1, 2048);
    crate::audit::record(&state, "host_run", &format!("Lab run of {}", m.title), "ok");

    if let Some(holder) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, holder));
    }
    // Someone else's chip lease refuses the run before anything is stopped:
    // tt-lab opens every chip.
    if let Some(capability) = state.gozer().cloned() {
        let port = state.serving_port();
        let verdict = tokio::task::spawn_blocking(move || {
            crate::gozer::foreign_leases(&RealCommandRunner, &capability, port)
        })
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let crate::gozer::ForeignLeases::Held { holders, .. } = verdict {
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "The silicon check needs every chip, and {holders}. Wait for that to finish."
                ),
            ));
        }
    }
    // The chips must be idle: stop what's serving only when the user said so.
    let life = state.lifecycle();
    let previous = life.model.clone().or(life.served_id.clone());
    let split = crate::coserve::is_split(&state);
    if !matches!(life.phase, Phase::Idle | Phase::Failed) || split {
        if !req.stop_serving {
            return Err(err(
                StatusCode::CONFLICT,
                format!(
                    "The silicon check needs all the chips. Stop {} first?",
                    previous
                        .as_deref()
                        .map(crate::notify::display_model)
                        .unwrap_or_else(|| "the model".into())
                ),
            ));
        }
        state.stop_serving().await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't stop the model: {e:#}"),
            )
        })?;
    }
    if !state.try_set_lab_busy() {
        return Err(err(
            StatusCode::CONFLICT,
            "A lab run is already using the chips.",
        ));
    }
    let busy = Busy(state.clone());
    // Busy first, then the check: a `/run` that slipped in after the check
    // above is seen here, and one that comes after this sees the Lab holding
    // the chips (`chips_holder`, checked again as its start begins).
    if !matches!(state.lifecycle().phase, Phase::Idle | Phase::Failed)
        || crate::coserve::is_split(&state)
    {
        drop(busy);
        return Err(err(
            StatusCode::CONFLICT,
            "A model started meanwhile; the silicon check needs all the chips. Stop it first?",
        ));
    }
    // With gozer, the whole run -- the reset below included -- holds a
    // whole-box lease, so other gozer users see the chips taken.
    let lease = take_lab_lease(&state, m).await?;
    // tt-lab needs freshly reset chips after another runtime (TT-Metal) used
    // them -- otherwise its firmware never starts ("BRISC command did not
    // finish"; seen live after a Wan2.2 serve). The configured reset runs
    // as is: under the Lab's own lease with gozer (no second lease), and
    // exactly as before without it. `busy` already keeps every other reset,
    // serve and tt-smi reading off the chips.
    let reset_state = state.clone();
    let reset = tokio::task::spawn_blocking(move || {
        reset_state.run_configured_power_command(crate::power::PowerAction::ResetChips)
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))
    .and_then(|r| r);
    if let Err(e) = reset {
        give_back(lease).await;
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("couldn't reset the chips: {e:#}"),
        ));
    }

    let mut args: Vec<String> = vec!["run".into(), "-m".into(), gguf.display().to_string()];
    args.extend(["--device".into(), "--tiles".into(), tiles.to_string()]);
    args.extend(["--ttq".into(), ttq.display().to_string()]);
    args.extend(["-p".into(), prompt.clone(), "-n".into(), tokens.to_string()]);
    if req.check {
        args.push("--check".into());
    }
    if req.profile {
        args.push("--profile".into());
    }
    let spawned = tokio::process::Command::new(&lab.binary)
        .args(&args)
        .current_dir(
            lab.binary
                .parent()
                .and_then(Path::parent)
                .unwrap_or(Path::new("/")),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            give_back(lease).await;
            return Err(err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't start tt-lab: {e}"),
            ));
        }
    };
    *lab.running.lock().expect("lab mutex poisoned") = Some(json!({
        "model": m.id, "tiles": tiles, "prompt": prompt, "since": unix_now(), "check": req.check,
        "lease": lease.as_ref().map(|(_, grant)| grant.lease_id.clone()),
    }));

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let started = Instant::now();
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    for (pipe, name) in [
        (
            Box::new(stdout) as Box<dyn tokio::io::AsyncRead + Send + Unpin>,
            "out",
        ),
        (Box::new(stderr), "err"),
    ] {
        let tx = tx.clone();
        let lines = Arc::clone(&lines);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(pipe);
            let mut buf = Vec::new();
            // Split on newlines *and* carriage returns (progress output), and
            // flush partial lines so streamed tokens appear as they come.
            loop {
                let chunk = reader.fill_buf().await;
                let Ok(bytes) = chunk else { break };
                if bytes.is_empty() {
                    break;
                }
                let n = bytes.len();
                buf.extend_from_slice(bytes);
                reader.consume(n);
                let text = String::from_utf8_lossy(&buf).into_owned();
                buf.clear();
                for piece in text.split_inclusive(['\n', '\r']) {
                    let line = piece.trim_end_matches(['\n', '\r']).to_string();
                    if piece.ends_with(['\n', '\r']) {
                        lines.lock().expect("lab mutex poisoned").push(line.clone());
                    }
                    let _ = tx.send(json!({ "type": "text", "stream": name, "text": piece }));
                }
            }
        });
    }
    let lab_done = Arc::clone(&lab);
    let model_id = m.id.to_string();
    let check = req.check;
    tokio::spawn(async move {
        let status = child.wait().await;
        // Let the pipe readers drain.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let lines = lines.lock().expect("lab mutex poisoned").clone();
        let ok = status.as_ref().is_ok_and(|s| s.success());
        let stats = parse_stats(&lines);
        let seconds = started.elapsed().as_secs_f64();
        lab_done.record(RunSummary {
            model: model_id,
            tiles,
            prompt,
            check,
            ok,
            seconds,
            at: unix_now(),
            stats: stats.clone(),
            tail: lines.iter().rev().take(12).rev().cloned().collect(),
        });
        *lab_done.running.lock().expect("lab mutex poisoned") = None;
        // Hand the chips back (gozer resets them for the next tenant) before
        // the agent lets serves and readings near them again.
        give_back(lease).await;
        drop(busy);
        let _ = tx.send(json!({
            "type": "done",
            "ok": ok,
            "exit_code": status.ok().and_then(|s| s.code()),
            "seconds": seconds,
            "stats": stats,
            "previous": previous.as_deref().map(crate::notify::display_model),
        }));
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|v| {
            (
                Ok::<_, Infallible>(Event::default().data(v.to_string())),
                rx,
            )
        })
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_the_two_models_with_box_tilings() {
        assert_eq!(model("gpt-oss-20b").unwrap().tiles, &[32, 8]);
        assert_eq!(model("gpt-oss-120b").unwrap().tiles, &[32]);
        assert!(model("llama").is_none());
    }

    #[test]
    fn stats_read_real_tt_lab_output() {
        let out = "warning: chip 0 PCIe-aligned physical Tensix column 11 is unavailable (harvested); using logical column 1
loaded TTQ tensors into device DRAM: 21.362 GiB, 32 channels, 4.320 s, peak 724.5 of 4080 MiB (channel 7)
prefill: 17 tokens, 0.086 s, 198.060 t/s
generated: 200 tokens
decode: 199 evals, 1.305 s, 152.479 t/s
profile: 216 tokens, shard 0 BRISC timeline (includes waits and overlap)
  attention        696801 cycles/token   13.9%
  experts         2127441 cycles/token   42.3%
  total           5024579 cycles/token
host: setup 0.040 ms/token, command 6.288 ms/token, convert 0.019 ms/logits token (200 tokens)";
        let lines: Vec<String> = out.lines().map(str::to_string).collect();
        let s = parse_stats(&lines);
        assert_eq!(s["dram_gib"], 21.362);
        assert_eq!(s["preload_s"], 4.32);
        assert_eq!(s["prefill_tps"], 198.06);
        assert_eq!(s["decode_tps"], 152.479);
        assert_eq!(s["tokens"], 200.0);
        assert_eq!(s["ms_per_token"], 6.288);
        assert_eq!(s["profile"][1]["stage"], "experts");
        assert_eq!(s["profile"][1]["percent"], 42.3);
        assert_eq!(s["profile"].as_array().unwrap().len(), 2, "not the total");

        let check: Vec<String> = "silicon token commands total 0.0115299 s, avg 0.00576496 s/token
silicon logits exact match: 2 tokens, 201088 values each"
            .lines()
            .map(str::to_string)
            .collect();
        let c = parse_stats(&check);
        assert_eq!(c["ms_per_token"], 5.76);
        assert_eq!(c["check"], "exact");
        assert_eq!(
            parse_stats(&["logits MISMATCH at 3".to_string()])["check"],
            "mismatch"
        );
    }
}
