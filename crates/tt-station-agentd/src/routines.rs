// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Routines: jobs the box does on its own, written as a goal.
//!
//! A routine is a goal in a person's words ("every weekday at 8:30, check the
//! chips and the logs and tell me if anything needs attention"), when to run
//! it (by hand, daily, weekdays, every few hours), and which actions it may
//! take without asking. A run hands the goal to the served model with the
//! box's tools; the model works out the steps, the runner carries them out
//! and records each one, and the run ends with a short report.
//!
//! Looking things up (status, models, logs, the web, a question to the
//! served model) never needs approval. Changing the box (start or stop a
//! model, download one, reset the chips) asks the person first -- a
//! notification, answered in TT Station -- unless the routine was given
//! that action ahead of time, for runs nobody watches (overnight).
//!
//! Long operations are waited out by the runner, not the model: a download
//! or a model start can take an hour, and the model is told when it's done.
//! While models are switched the runner waits for one to serve again before
//! asking the model anything.
//!
//! One run at a time: runs share the chips.
//!
//! - `GET /routines`, `POST /routines`, `PUT /routines/{id}`, `DELETE /routines/{id}`
//! - `POST /routines/{id}/run`: run it now.
//! - `GET /routines/{id}/runs`, `GET /routines/runs/{run}`
//! - `POST /routines/runs/{run}/approve` `{allow}`, `POST /routines/runs/{run}/stop`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Tool rounds in one run before it must report.
const MAX_ROUNDS: usize = 30;
/// Runs kept on disk.
const KEEP_RUNS: usize = 200;

// ---------------------------------------------------------------- data

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Schedule {
    /// `manual` | `daily` | `weekdays` | `hourly`
    #[serde(default = "manual")]
    pub kind: String,
    /// `HH:MM`, local time (daily, weekdays).
    #[serde(default)]
    pub time: Option<String>,
    /// Hours between runs (hourly).
    #[serde(default)]
    pub every_hours: Option<u32>,
}

fn manual() -> String {
    "manual".into()
}

/// What a routine may do without asking: the action tools it was given.
pub const ACTIONS: [&str; 4] = ["start_model", "stop_model", "download_model", "reset_chips"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub schedule: Schedule,
    /// Actions it may take without asking (a subset of [`ACTIONS`]).
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub created: u64,
    #[serde(default)]
    pub next_run: Option<u64>,
    #[serde(default)]
    pub last_run: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub ts: u64,
    /// `tool` (a look-up), `action` (changes the box), `wait`, `note`
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: Value,
    pub result: String,
    pub ok: bool,
    /// How long it took, seconds.
    #[serde(default)]
    pub seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub tool: String,
    pub args: Value,
    pub summary: String,
    pub since: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub routine: String,
    pub name: String,
    /// `manual` or `schedule`
    pub trigger: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    /// `running` | `waiting` (for the person) | `done` | `failed` | `stopped`
    pub status: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub report: Option<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
    /// What it's doing right now ("Waiting for the download", "Thinking").
    #[serde(default)]
    pub activity: Option<String>,
}

// ---------------------------------------------------------------- storage

fn dir(state: &AppState) -> PathBuf {
    state.config_dir().join("routines")
}

static ROUTINES: LazyLock<Mutex<Option<Vec<Routine>>>> = LazyLock::new(|| Mutex::new(None));

fn with_routines<T>(state: &AppState, f: impl FnOnce(&mut Vec<Routine>) -> T) -> T {
    let mut held = ROUTINES.lock().expect("routines");
    let list = held.get_or_insert_with(|| {
        std::fs::read(dir(state).join("routines.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    });
    let before = serde_json::to_vec(&*list).unwrap_or_default();
    let out = f(list);
    let after = serde_json::to_vec_pretty(&*list).unwrap_or_default();
    if serde_json::to_vec(&*list).unwrap_or_default() != before {
        let _ = std::fs::create_dir_all(dir(state));
        let tmp = dir(state).join("routines.json.tmp");
        if std::fs::write(&tmp, after).is_ok() {
            let _ = std::fs::rename(&tmp, dir(state).join("routines.json"));
        }
    }
    out
}

fn run_path(state: &AppState, id: &str) -> PathBuf {
    dir(state).join("runs").join(format!("{id}.json"))
}

/// Runs in progress, kept here and written through to disk.
static LIVE: LazyLock<Mutex<HashMap<String, Run>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn save_run(state: &AppState, run: &Run) {
    let path = run_path(state, &run.id);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&path, serde_json::to_vec_pretty(run).unwrap_or_default());
    LIVE.lock().expect("routines").insert(run.id.clone(), run.clone());
}

fn update_run(state: &AppState, id: &str, f: impl FnOnce(&mut Run)) -> Option<Run> {
    let mut run = LIVE.lock().expect("routines").get(id).cloned().or_else(|| load_run(state, id))?;
    f(&mut run);
    save_run(state, &run);
    Some(run)
}

fn load_run(state: &AppState, id: &str) -> Option<Run> {
    if let Some(r) = LIVE.lock().expect("routines").get(id) {
        return Some(r.clone());
    }
    std::fs::read(run_path(state, id)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

/// All runs on disk, newest first.
fn all_runs(state: &AppState) -> Vec<Run> {
    let mut runs: Vec<Run> = std::fs::read_dir(dir(state).join("runs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice::<Run>(&b).ok())
        .map(|r| LIVE.lock().expect("routines").get(&r.id).cloned().unwrap_or(r))
        .collect();
    runs.sort_by(|a, b| b.started.cmp(&a.started));
    runs
}

/// Oldest runs beyond what's kept.
fn prune_runs(state: &AppState) {
    for old in all_runs(state).iter().skip(KEEP_RUNS) {
        let _ = std::fs::remove_file(run_path(state, &old.id));
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

// ---------------------------------------------------------------- schedules

/// Local time pieces for a unix time.
fn local(t: u64) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes into the provided struct.
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    tm
}

/// The unix time of a local date and time (normalizing overflow, e.g. day 32).
fn local_to_unix(mut tm: libc::tm) -> u64 {
    tm.tm_isdst = -1;
    // SAFETY: mktime reads and normalizes the struct we own.
    let t = unsafe { libc::mktime(&mut tm) };
    t.max(0) as u64
}

fn parse_hhmm(s: &str) -> Option<(i32, i32)> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some((h, m))
}

/// When a schedule next runs after `after` (unix seconds), or None for manual.
pub fn next_after(s: &Schedule, after: u64) -> Option<u64> {
    match s.kind.as_str() {
        "hourly" => Some(after + u64::from(s.every_hours.unwrap_or(1).clamp(1, 168)) * 3600),
        "daily" | "weekdays" => {
            let (h, m) = parse_hhmm(s.time.as_deref().unwrap_or("08:00"))?;
            let base = local(after);
            for day in 0..8 {
                let mut tm = base;
                tm.tm_mday += day;
                tm.tm_hour = h;
                tm.tm_min = m;
                tm.tm_sec = 0;
                let t = local_to_unix(tm);
                if t <= after {
                    continue;
                }
                let wday = local(t).tm_wday;
                if s.kind == "weekdays" && (wday == 0 || wday == 6) {
                    continue;
                }
                return Some(t);
            }
            None
        }
        _ => None,
    }
}

/// "Every weekday at 08:30", for lists and notifications.
pub fn describe(s: &Schedule) -> String {
    match s.kind.as_str() {
        "daily" => format!("Every day at {}", s.time.as_deref().unwrap_or("08:00")),
        "weekdays" => format!("Weekdays at {}", s.time.as_deref().unwrap_or("08:00")),
        "hourly" => match s.every_hours.unwrap_or(1) {
            1 => "Every hour".into(),
            n => format!("Every {n} hours"),
        },
        _ => "When you run it".into(),
    }
}

// ---------------------------------------------------------------- the tools a run may use

/// Look-ups from the assistant that make sense unattended. (Not desktop
/// control: nobody is at the screen when a routine runs at 3 a.m.)
const BORROWED: [&str; 9] = ["box_status", "list_models", "recent_logs", "web_search", "read_webpage", "research", "research_check", "search_documents", "remember"];

fn tools() -> Value {
    let mut out: Vec<Value> = crate::assistant::tool_definitions()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t["function"]["name"].as_str().is_some_and(|n| BORROWED.contains(&n)))
        .collect();
    let def = |name: &str, desc: &str, params: Value| json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } });
    let none = json!({ "type": "object", "properties": {} });
    out.extend([
        def("community_models", "The community model packages on this box (with whether their weights are all here and whether they fit), downloads in progress, and which one is serving.", none.clone()),
        def("ask_model", "Send one prompt to the model serving right now and get its reply, with its speed (tokens per second) and time to first token. Use it to test a model.",
            json!({ "type": "object", "properties": { "prompt": { "type": "string" }, "max_tokens": { "type": "integer" } }, "required": ["prompt"] })),
        def("wait_until_ready", "Wait for the model that's starting to finish loading (or fail). Call it after start_model.",
            json!({ "type": "object", "properties": { "minutes": { "type": "integer", "description": "Give up after this long (default 45)." } } })),
        def("wait_for_download", "Wait for a community model's download to finish (or fail). Call it after download_model.",
            json!({ "type": "object", "properties": { "id": { "type": "string" }, "minutes": { "type": "integer", "description": "Give up after this long (default 240)." } }, "required": ["id"] })),
        def("start_model", "ACTION: start a model on the chips, stopping whatever serves now. `model` is a community package id (namespace/name, from community_models) or a model name from list_models. Then call wait_until_ready.",
            json!({ "type": "object", "properties": { "model": { "type": "string" } }, "required": ["model"] })),
        def("evaluate_model", "Run the full check-up on the model serving now (tool use, code, reasoning, following instructions, long documents, speed; about 5-10 minutes). Results are saved on the Models page, where models are compared.", none.clone()),
        def("stop_model", "ACTION: stop the model that's serving.", none.clone()),
        def("download_model", "ACTION: download a community model package with its weights (can be hundreds of GB). Then call wait_for_download.",
            json!({ "type": "object", "properties": { "id": { "type": "string", "description": "namespace/name" } }, "required": ["id"] })),
        def("reset_chips", "ACTION: reset the Tenstorrent chips (stops whatever is serving).", none),
    ]);
    json!(out)
}

fn is_action(tool: &str) -> bool {
    ACTIONS.contains(&tool)
}

/// What an action will do, in a person's words.
fn action_summary(tool: &str, args: &Value) -> String {
    match tool {
        "start_model" => format!("Start {}", args["model"].as_str().unwrap_or("a model")),
        "stop_model" => "Stop the model that's serving".into(),
        "download_model" => format!("Download {}", args["id"].as_str().unwrap_or("a model")),
        "reset_chips" => "Reset the chips".into(),
        other => other.to_string(),
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Runs one look-up or action; (ok, what it found).
async fn run_tool(state: &AppState, run_id: &str, name: &str, args: &Value) -> (bool, String) {
    match name {
        "community_models" => {
            let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
            let installed: Vec<Value> = c["installed"].as_array().into_iter().flatten().map(|p| json!({
                "id": p["id"], "fits": p["fits"], "fit": p["fit"], "weights_on_box": p["weights_on_box"], "can_use_tools": p["tools"],
            })).collect();
            let pulls: Vec<Value> = c["pulls"].as_array().into_iter().flatten().map(|d| json!({
                "id": d["id"], "running": d["running"], "paused": d["paused"], "failed": d["failed"], "progress": d["progress"]["percent"],
            })).collect();
            (true, json!({ "installed": installed, "downloads": pulls, "serving": c["serving"], "starting": c["starting"]["id"] }).to_string())
        }
        "ask_model" => ask_model(state, args).await,
        "evaluate_model" => {
            set_activity(state, run_id, "Evaluating the model");
            match crate::evals::run(state).await {
                Ok(e) if e.status == "done" => (true, json!({ "summary": crate::evals::headline(&e), "overall_percent": e.overall.map(|o| (o * 100.0).round()), "speed": e.speed, "context": e.context }).to_string()),
                Ok(e) => (false, format!("The evaluation didn't finish: {}", e.error.unwrap_or_default())),
                Err(e) => (false, e),
            }
        }
        "wait_until_ready" => {
            let minutes = args["minutes"].as_u64().unwrap_or(45).clamp(1, 180);
            set_activity(state, run_id, "Waiting for the model to load");
            wait_until_ready(state, run_id, minutes).await
        }
        "wait_for_download" => {
            let id = args["id"].as_str().unwrap_or("").to_string();
            let minutes = args["minutes"].as_u64().unwrap_or(240).clamp(1, 24 * 60);
            set_activity(state, run_id, &format!("Waiting for {} to download", short(&id)));
            wait_for_download(state, run_id, &id, minutes).await
        }
        "start_model" => start_model(state, args["model"].as_str().unwrap_or("")).await,
        "stop_model" => match state.stop_serving().await {
            Ok(()) => (true, "Stopped the model that was serving.".into()),
            Err(e) => (false, format!("Couldn't stop it: {e:#}")),
        },
        "download_model" => {
            let id = args["id"].as_str().unwrap_or("").to_string();
            let body = crate::community::IdBody { id: id.clone(), stop_serving: false, anyway: false };
            match crate::community::post_pull(State(state.clone()), BearerAuth, Json(body)).await {
                Ok(_) => (true, format!("Downloading {id}. Call wait_for_download next.")),
                Err((_, Json(v))) => (false, v["error"].as_str().unwrap_or("couldn't start the download").to_string()),
            }
        }
        "reset_chips" => {
            let call = crate::assistant::ToolCall { id: String::new(), name: "reset_chips".into(), args: Ok(json!({})) };
            confirm_through_assistant(state, &call).await
        }
        other if BORROWED.contains(&other) => {
            let call = crate::assistant::ToolCall { id: String::new(), name: other.into(), args: Ok(args.clone()) };
            let (result, _) = crate::assistant::run_read_only(state, &call).await;
            let ok = result.get("error").is_none();
            (ok, result.to_string())
        }
        other => (false, format!("There's no tool called {other}.")),
    }
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Starts a community package by id, or a catalog model by name.
pub(crate) async fn start_model(state: &AppState, model: &str) -> (bool, String) {
    let model = model.trim();
    if model.is_empty() {
        return (false, "Say which model to start.".into());
    }
    let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
    let package = c["installed"].as_array().into_iter().flatten().filter_map(|p| p["id"].as_str()).find(|id| {
        id.eq_ignore_ascii_case(model) || short(id).eq_ignore_ascii_case(model)
    }).map(str::to_string);
    if let Some(id) = package {
        let body = crate::community::IdBody { id: id.clone(), stop_serving: true, anyway: false };
        return match crate::community::post_serve(State(state.clone()), BearerAuth, Json(body)).await {
            Ok(_) => (true, format!("Starting {id}. Call wait_until_ready next.")),
            Err((_, Json(v))) => (false, v["error"].as_str().unwrap_or("it wouldn't start").to_string()),
        };
    }
    let call = crate::assistant::ToolCall { id: String::new(), name: "run_model".into(), args: Ok(json!({ "model": model })) };
    confirm_through_assistant(state, &call).await
}

/// An assistant action, proposed and approved at once (the routine's own
/// approval already happened).
async fn confirm_through_assistant(state: &AppState, call: &crate::assistant::ToolCall) -> (bool, String) {
    let current = state.current_model().unwrap_or_default();
    match crate::assistant::propose(state, call, &current).await {
        Ok((pending, _)) => {
            let Some(action_id) = pending["action_id"].as_str().map(str::to_string) else {
                return (false, "It couldn't be set up.".into());
            };
            match crate::assistant::confirm(state, crate::assistant::ConfirmRequest { action_id, approve: true }).await {
                Ok(r) => (true, r.summary),
                Err(e) => (false, format!("{e:?}")),
            }
        }
        Err(reason) => (false, reason),
    }
}

async fn ask_model(state: &AppState, args: &Value) -> (bool, String) {
    let Some(ep) = state.endpoint() else { return (false, "No model is serving.".into()) };
    let prompt = args["prompt"].as_str().unwrap_or("").to_string();
    if prompt.trim().is_empty() {
        return (false, "Give it a prompt.".into());
    }
    let body = json!({
        "model": ep.model, "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": args["max_tokens"].as_u64().unwrap_or(400).clamp(16, 4096),
        "stream": true, "stream_options": { "include_usage": true },
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let started = Instant::now();
    let resp = reqwest::Client::new()
        .post(crate::assistant::chat_completions_url(&ep.base_url))
        .json(&body)
        .timeout(Duration::from_secs(600))
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => return (false, format!("The model answered {}: {}", r.status(), cut(&r.text().await.unwrap_or_default(), 300))),
        Err(e) => return (false, format!("Couldn't reach the model: {e}")),
    };
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let (mut text, mut first, mut tokens, mut buf) = (String::new(), None::<f64>, 0u64, Vec::<u8>::new());
    while let Some(Ok(chunk)) = stream.next().await {
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else { continue };
            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                if first.is_none() && !t.is_empty() {
                    first = Some(started.elapsed().as_secs_f64());
                }
                text.push_str(t);
            }
            if let Some(n) = v["usage"]["completion_tokens"].as_u64() {
                tokens = n;
            }
        }
    }
    let total = started.elapsed().as_secs_f64();
    let ttft = first.unwrap_or(total);
    let speed = if tokens > 1 && total > ttft { (tokens - 1) as f64 / (total - ttft) } else { 0.0 };
    let reply = crate::assistant::strip_think(&text);
    (true, json!({
        "reply": cut(&reply, 3000), "model": ep.model, "tokens": tokens,
        "tokens_per_second": (speed * 10.0).round() / 10.0, "seconds_to_first_token": (ttft * 10.0).round() / 10.0,
    }).to_string())
}

async fn wait_until_ready(state: &AppState, run_id: &str, minutes: u64) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    // A start that's just been asked for may not show as starting for a moment.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if stopping(run_id) {
            return (false, "Stopped.".into());
        }
        let life = state.lifecycle();
        match life.phase {
            Phase::Serving => {
                let model = life.served_id.or(life.model).unwrap_or_default();
                return (true, format!("{model} is serving and ready."));
            }
            Phase::Failed => return (false, format!("It didn't start: {}", life.error.unwrap_or_default())),
            Phase::Idle => return (false, "Nothing is starting (and nothing is serving).".into()),
            _ => {}
        }
        if let Some(stuck) = crate::community::stalled_s() {
            return (false, format!("The start looks stuck: no progress for {} minutes.", stuck / 60));
        }
        if Instant::now() > deadline {
            return (false, format!("It still wasn't ready after {minutes} minutes."));
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn wait_for_download(state: &AppState, run_id: &str, id: &str, minutes: u64) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    loop {
        if stopping(run_id) {
            return (false, "Stopped.".into());
        }
        let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
        let pull = c["pulls"].as_array().into_iter().flatten().find(|d| d["id"] == id).cloned();
        match pull {
            Some(d) if d["running"] == true => {
                let pct = d["progress"]["percent"].as_f64().map(|p| format!(" ({p:.0}%)")).unwrap_or_default();
                set_activity(state, run_id, &format!("Downloading {}{pct}", short(id)));
            }
            Some(d) if d["failed"] == true => return (false, format!("The download failed: {}", d["line"].as_str().unwrap_or(""))),
            Some(d) if d["paused"] == true => return (false, "The download is paused.".into()),
            _ => {
                let here = c["installed"].as_array().into_iter().flatten().any(|p| p["id"] == id);
                return if here { (true, format!("{id} is downloaded.")) } else { (false, format!("{id} isn't downloading, and it isn't on the box.")) };
            }
        }
        if Instant::now() > deadline {
            return (false, format!("It was still downloading after {minutes} minutes."));
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

// ---------------------------------------------------------------- running

static STOP: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(|| Mutex::new(Default::default()));
static DECISIONS: LazyLock<Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// The run going on now (one at a time).
static CURRENT: Mutex<Option<String>> = Mutex::new(None);

fn stopping(run_id: &str) -> bool {
    STOP.lock().expect("routines").contains(run_id)
}

fn set_activity(state: &AppState, run_id: &str, what: &str) {
    update_run(state, run_id, |r| r.activity = Some(what.to_string()));
}

fn system_prompt(r: &Routine) -> String {
    let t = local(now());
    let days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    format!(
        "You carry out a routine on a Tenstorrent QuietBox (four Blackhole chips) for TT Station, with the tools given. \
It's {} {:04}-{:02}-{:02}, {:02}:{:02}. Nobody may be watching: work it out yourself, one tool call at a time.\n\
- Find out before you act: check what's serving and what's on the box (box_status, community_models, list_models).\n\
- Tools marked ACTION change the box; the person may have to approve them. If one is declined, adapt or stop, and say so in your report.\n\
- After start_model, call wait_until_ready; after download_model, call wait_for_download. These can take a long time; that's fine.\n\
- To test a model thoroughly, use evaluate_model (it's graded automatically and saved for comparison); for a quick look, ask_model.\n\
- If the routine changes which model is serving, put the one that served before back at the end unless the goal says otherwise.\n\
- Never invent results. If something fails, say what and why.\n\
- Finish with your report for the person, in Markdown: a one-line headline first, then what you did, what you found (with numbers), and anything that needs their attention.\n\n\
The routine, \"{}\":\n{}",
        days[t.tm_wday.clamp(0, 6) as usize], t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, r.name, r.goal
    )
}

/// Starts a run of `r` in the background; refuses if one is going.
fn start(state: &AppState, r: &Routine, trigger: &str) -> Result<Run, String> {
    let mut current = CURRENT.lock().expect("routines");
    if let Some(id) = current.as_ref() {
        let name = load_run(state, id).map(|r| r.name).unwrap_or_default();
        return Err(format!("Another routine is running ({name}). One runs at a time, since they share the chips."));
    }
    let run = Run {
        id: format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]),
        routine: r.id.clone(),
        name: r.name.clone(),
        trigger: trigger.into(),
        started: now(),
        finished: None,
        status: "running".into(),
        steps: vec![],
        report: None,
        pending: None,
        activity: Some("Starting".into()),
    };
    save_run(state, &run);
    *current = Some(run.id.clone());
    drop(current);
    with_routines(state, |list| {
        if let Some(x) = list.iter_mut().find(|x| x.id == r.id) {
            x.last_run = Some(run.id.clone());
        }
    });
    let (st, rt, id) = (state.clone(), r.clone(), run.id.clone());
    tokio::spawn(async move {
        let outcome = execute(&st, &rt, &id).await;
        let (status, report) = match outcome {
            Ok(report) => ("done", report),
            Err(e) if stopping(&id) => ("stopped", format!("Stopped. {e}")),
            Err(e) => ("failed", e),
        };
        let run = update_run(&st, &id, |r| {
            r.status = status.into();
            r.finished = Some(now());
            r.report = Some(report.clone());
            r.pending = None;
            r.activity = None;
        });
        STOP.lock().expect("routines").remove(&id);
        *CURRENT.lock().expect("routines") = None;
        prune_runs(&st);
        if let Some(run) = run {
            let headline = headline(&report);
            st.notifier().emit(crate::notify::Draft {
                kind: "routine",
                title: match status { "done" => format!("{} finished", run.name), "stopped" => format!("{} stopped", run.name), _ => format!("{} didn't finish", run.name) },
                body: cut(&headline, 200),
            });
        }
    });
    Ok(run)
}

/// The run itself: the model plans, the runner acts. The report, or why it failed.
async fn execute(state: &AppState, r: &Routine, run_id: &str) -> Result<String, String> {
    let mut messages = vec![json!({ "role": "system", "content": system_prompt(r) }), json!({ "role": "user", "content": "Carry out the routine now." })];
    let tools = tools();
    let client = reqwest::Client::new();
    for round in 0..=MAX_ROUNDS {
        if stopping(run_id) {
            return Err("Stopped by you.".into());
        }
        // A model to think with (one may be starting after a switch).
        set_activity(state, run_id, "Thinking");
        let ep = match state.endpoint() {
            Some(ep) => ep,
            None => {
                set_activity(state, run_id, "Waiting for a model to serve");
                let (ok, why) = wait_until_ready(state, run_id, 30).await;
                if !ok {
                    return Err(format!("No model to think with: {why}"));
                }
                state.endpoint().ok_or("No model is serving.")?
            }
        };
        let last = round == MAX_ROUNDS;
        let mut body = json!({
            "model": ep.model, "messages": messages, "max_tokens": 2048, "temperature": 0.2,
            "chat_template_kwargs": { "enable_thinking": false },
        });
        if !last {
            body["tools"] = tools.clone();
            body["tool_choice"] = json!("auto");
        }
        let resp = client.post(crate::assistant::chat_completions_url(&ep.base_url)).json(&body).timeout(Duration::from_secs(600)).send().await;
        let v: Value = match resp {
            Ok(x) if x.status().is_success() => x.json().await.map_err(|e| e.to_string())?,
            Ok(x) => return Err(format!("The model refused ({}): {}", x.status(), cut(&x.text().await.unwrap_or_default(), 300))),
            Err(e) => return Err(format!("Couldn't reach the model: {e}")),
        };
        let message = v["choices"][0]["message"].clone();
        let content = crate::assistant::strip_think(message["content"].as_str().unwrap_or("")).trim().to_string();
        let calls = if last { vec![] } else { crate::assistant::parse_tool_calls(&message) };
        if calls.is_empty() {
            if content.is_empty() {
                return Err("The model gave no report.".into());
            }
            return Ok(content);
        }
        if !content.is_empty() {
            update_run(state, run_id, |run| run.steps.push(Step { ts: now(), kind: "note".into(), tool: None, args: Value::Null, result: cut(&content, 2000), ok: true, seconds: 0 }));
        }
        messages.push(json!({ "role": "assistant", "content": content, "tool_calls": message["tool_calls"] }));
        for call in calls {
            let args = call.args.clone().unwrap_or_else(|_| json!({}));
            let began = Instant::now();
            let (ok, result) = if call.args.is_err() {
                (false, "Those arguments weren't valid JSON; call it again.".to_string())
            } else if is_action(&call.name) && !r.allowed.contains(&call.name) {
                match ask_person(state, r, run_id, &call.name, &args).await {
                    Some(true) => run_tool(state, run_id, &call.name, &args).await,
                    Some(false) => (false, "The person declined. Don't try this again in this run.".into()),
                    None => return Err("Stopped while waiting for your OK.".into()),
                }
            } else {
                run_tool(state, run_id, &call.name, &args).await
            };
            let kind = if is_action(&call.name) { "action" } else if call.name.starts_with("wait_") { "wait" } else { "tool" };
            update_run(state, run_id, |run| {
                run.steps.push(Step { ts: now(), kind: kind.into(), tool: Some(call.name.clone()), args: args.clone(), result: cut(&result, 2000), ok, seconds: began.elapsed().as_secs() });
            });
            messages.push(json!({ "role": "tool", "tool_call_id": call.id, "name": call.name, "content": cut(&result, 6000) }));
        }
    }
    Err("It ran out of steps without a report.".into())
}

/// Whether a routine is running now.
pub(crate) fn is_running() -> bool {
    CURRENT.lock().expect("routines").is_some()
}

/// The routine run's question for the person, if it's waiting on one (`GET /approvals`).
pub(crate) fn pending_approval(state: &AppState) -> Option<Value> {
    let id = CURRENT.lock().expect("routines").clone()?;
    let run = load_run(state, &id)?;
    let p = run.pending?;
    Some(json!({ "id": format!("routine:{id}"), "kind": "routine", "run": id, "where": run.name, "summary": p.summary, "tool": p.tool, "input": p.args, "since": p.since, "can_always": false }))
}

/// Asks the person to OK an action: Some(answer), or None if the run was stopped.
async fn ask_person(state: &AppState, r: &Routine, run_id: &str, tool: &str, args: &Value) -> Option<bool> {
    let summary = action_summary(tool, args);
    let (tx, rx) = tokio::sync::oneshot::channel();
    DECISIONS.lock().expect("routines").insert(run_id.to_string(), tx);
    update_run(state, run_id, |run| {
        run.status = "waiting".into();
        run.pending = Some(Pending { tool: tool.into(), args: args.clone(), summary: summary.clone(), since: now() });
        run.activity = Some("Waiting for your OK".into());
    });
    state.notifier().emit_with(
        crate::notify::Draft { kind: "routine", title: format!("{} needs your OK", r.name), body: format!("{summary}?") },
        Some(json!({ "approval": { "id": format!("routine:{run_id}"), "kind": "routine", "run": run_id, "summary": summary, "can_always": false } })),
    );
    let answer = rx.await.ok();
    update_run(state, run_id, |run| {
        run.status = "running".into();
        run.pending = None;
        run.activity = None;
    });
    answer.filter(|_| !stopping(run_id))
}

/// Runs scheduled routines when they're due. Started once, at agent start.
pub async fn schedule_forever(state: AppState) {
    // Runs that were going when the agent stopped can't carry on.
    for mut run in all_runs(&state).into_iter().filter(|r| r.finished.is_none()) {
        run.status = "failed".into();
        run.finished = Some(now());
        run.pending = None;
        run.activity = None;
        run.report = Some("TT Station restarted while this was running, so it stopped.".into());
        save_run(&state, &run);
        LIVE.lock().expect("routines").remove(&run.id);
    }
    loop {
        let due: Vec<Routine> = with_routines(&state, |list| {
            let t = now();
            for r in list.iter_mut() {
                if r.next_run.is_none() && r.enabled {
                    r.next_run = next_after(&r.schedule, t);
                }
            }
            list.iter().filter(|r| r.enabled && r.next_run.is_some_and(|n| n <= t)).cloned().collect()
        });
        for r in due {
            let started = start(&state, &r, "schedule");
            with_routines(&state, |list| {
                if let Some(x) = list.iter_mut().find(|x| x.id == r.id) {
                    x.next_run = next_after(&x.schedule, now());
                }
            });
            if let Err(e) = started {
                state.notifier().emit(crate::notify::Draft { kind: "routine", title: format!("{} didn't run", r.name), body: e });
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// A report's headline: its "Headline:" line, else its first heading, else
/// its first line -- not a preamble like "All checks complete. Here's the report."
pub fn headline(report: &str) -> String {
    let clean = |l: &str| l.trim().trim_start_matches('#').trim().trim_matches('*').trim().to_string();
    let lines: Vec<&str> = report.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if let Some(l) = lines.iter().find(|l| l.to_lowercase().contains("headline:")) {
        let after = l.splitn(2, |c| c == ':').nth(1).unwrap_or(l);
        return clean(after);
    }
    if let Some(l) = lines.iter().find(|l| l.starts_with('#')) {
        return clean(l);
    }
    lines.first().map(|l| clean(l)).unwrap_or_default()
}

// ---------------------------------------------------------------- routes

fn view(state: &AppState, r: &Routine) -> Value {
    let last = r.last_run.as_deref().and_then(|id| load_run(state, id));
    json!({
        "id": r.id, "name": r.name, "goal": r.goal, "schedule": r.schedule, "when": describe(&r.schedule),
        "allowed": r.allowed, "enabled": r.enabled, "created": r.created, "next_run": if r.enabled { r.next_run } else { None },
        "last": last.map(|l| json!({ "id": l.id, "status": l.status, "started": l.started, "finished": l.finished,
            "headline": l.report.as_deref().map(headline) })),
    })
}

/// `GET /routines`
pub(crate) async fn get_routines(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let list = with_routines(&state, |l| l.clone());
    let current = CURRENT.lock().expect("routines").clone();
    Json(json!({
        "routines": list.iter().map(|r| view(&state, r)).collect::<Vec<_>>(),
        "running": current.and_then(|id| load_run(&state, &id)),
        "actions": ACTIONS,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RoutineBody {
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn check(b: &RoutineBody) -> Result<(), (StatusCode, Json<Value>)> {
    if b.name.trim().is_empty() || b.goal.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Give it a name and say what it should do."));
    }
    if !["manual", "daily", "weekdays", "hourly"].contains(&b.schedule.kind.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "Pick when it runs."));
    }
    if matches!(b.schedule.kind.as_str(), "daily" | "weekdays") && parse_hhmm(b.schedule.time.as_deref().unwrap_or("")).is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "Give a time like 08:30."));
    }
    if let Some(bad) = b.allowed.iter().find(|a| !is_action(a)) {
        return Err(err(StatusCode::BAD_REQUEST, format!("{bad} isn't an action a routine can take.")));
    }
    Ok(())
}

/// `POST /routines`
pub(crate) async fn post_routine(State(state): State<AppState>, _auth: BearerAuth, Json(b): Json<RoutineBody>) -> Reply {
    check(&b)?;
    let r = Routine {
        id: crate::pairing::issue_pair_id()[..10].to_string(),
        name: b.name.trim().chars().take(80).collect(),
        goal: b.goal.trim().chars().take(4000).collect(),
        next_run: next_after(&b.schedule, now()),
        schedule: b.schedule,
        allowed: b.allowed,
        enabled: b.enabled.unwrap_or(true),
        created: now(),
        last_run: None,
    };
    with_routines(&state, |l| l.push(r.clone()));
    Ok(Json(json!({ "routine": view(&state, &r) })))
}

/// `PUT /routines/{id}`
pub(crate) async fn put_routine(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<RoutineBody>) -> Reply {
    check(&b)?;
    let updated = with_routines(&state, |l| {
        let r = l.iter_mut().find(|r| r.id == id)?;
        r.name = b.name.trim().chars().take(80).collect();
        r.goal = b.goal.trim().chars().take(4000).collect();
        if r.schedule != b.schedule || b.enabled.is_some_and(|e| e && !r.enabled) {
            r.next_run = next_after(&b.schedule, now());
        }
        r.schedule = b.schedule;
        r.allowed = b.allowed;
        if let Some(e) = b.enabled {
            r.enabled = e;
        }
        Some(r.clone())
    })
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such routine."))?;
    Ok(Json(json!({ "routine": view(&state, &updated) })))
}

/// `DELETE /routines/{id}`: the routine and its runs.
pub(crate) async fn delete_routine(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if CURRENT.lock().expect("routines").as_ref().and_then(|r| load_run(&state, r)).is_some_and(|r| r.routine == id) {
        return Err(err(StatusCode::CONFLICT, "It's running. Stop it first."));
    }
    let gone = with_routines(&state, |l| {
        let before = l.len();
        l.retain(|r| r.id != id);
        l.len() != before
    });
    if !gone {
        return Err(err(StatusCode::NOT_FOUND, "No such routine."));
    }
    for run in all_runs(&state).into_iter().filter(|r| r.routine == id) {
        let _ = std::fs::remove_file(run_path(&state, &run.id));
    }
    Ok(Json(json!({ "deleted": true })))
}

/// `POST /routines/{id}/run`
pub(crate) async fn post_run(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let r = with_routines(&state, |l| l.iter().find(|r| r.id == id).cloned()).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such routine."))?;
    if state.endpoint().is_none() && !matches!(state.lifecycle().phase, Phase::Starting) {
        return Err(err(StatusCode::CONFLICT, "No model is serving, and a routine needs one to think with. Start one in Models."));
    }
    let run = start(&state, &r, "manual").map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "run": run })))
}

/// `GET /routines/{id}/runs`: newest first, without their steps.
pub(crate) async fn get_runs(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Json<Value> {
    let runs: Vec<Value> = all_runs(&state)
        .into_iter()
        .filter(|r| r.routine == id)
        .take(30)
        .map(|r| json!({ "id": r.id, "status": r.status, "trigger": r.trigger, "started": r.started, "finished": r.finished, "steps": r.steps.len(),
            "headline": r.report.as_deref().map(headline) }))
        .collect();
    Json(json!({ "runs": runs }))
}

/// `GET /routines/runs/{run}`
pub(crate) async fn get_run(State(state): State<AppState>, _auth: BearerAuth, UrlPath(run): UrlPath<String>) -> Reply {
    if !valid_id(&run) {
        return Err(err(StatusCode::NOT_FOUND, "No such run."));
    }
    let r = load_run(&state, &run).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such run."))?;
    Ok(Json(json!({ "run": r })))
}

#[derive(Debug, Deserialize)]
pub struct Decision {
    pub allow: bool,
}

/// `POST /routines/runs/{run}/approve` `{allow}`
pub(crate) async fn post_approve(State(state): State<AppState>, _auth: BearerAuth, UrlPath(run): UrlPath<String>, Json(d): Json<Decision>) -> Reply {
    let tx = DECISIONS.lock().expect("routines").remove(&run).ok_or_else(|| err(StatusCode::CONFLICT, "It isn't waiting for an answer any more."))?;
    let _ = tx.send(d.allow);
    let _ = state;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /routines/runs/{run}/stop`
pub(crate) async fn post_stop(State(state): State<AppState>, _auth: BearerAuth, UrlPath(run): UrlPath<String>) -> Reply {
    if load_run(&state, &run).is_none_or(|r| r.finished.is_some()) {
        return Err(err(StatusCode::CONFLICT, "It isn't running."));
    }
    STOP.lock().expect("routines").insert(run.clone());
    // Waiting for an answer: the stop is the answer.
    if let Some(tx) = DECISIONS.lock().expect("routines").remove(&run) {
        let _ = tx.send(false);
    }
    set_activity(&state, &run, "Stopping after the current step");
    Ok(Json(json!({ "stopping": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_find_their_next_time() {
        let at = |s: &Schedule, t: u64| next_after(s, t);
        let manual = Schedule { kind: "manual".into(), ..Default::default() };
        assert_eq!(at(&manual, 1000), None);
        let hourly = Schedule { kind: "hourly".into(), every_hours: Some(6), ..Default::default() };
        assert_eq!(at(&hourly, 1000), Some(1000 + 6 * 3600));
        let daily = Schedule { kind: "daily".into(), time: Some("08:30".into()), ..Default::default() };
        let now = super::now();
        let next = at(&daily, now).unwrap();
        assert!(next > now && next <= now + 25 * 3600);
        let tm = local(next);
        assert_eq!((tm.tm_hour, tm.tm_min), (8, 30));
        let weekdays = Schedule { kind: "weekdays".into(), time: Some("23:59".into()), ..Default::default() };
        let mut t = now;
        for _ in 0..10 {
            t = at(&weekdays, t).unwrap();
            assert!(!matches!(local(t).tm_wday, 0 | 6), "never on a weekend");
        }
        assert_eq!(at(&Schedule { kind: "daily".into(), time: Some("25:00".into()), ..Default::default() }, now), None);
    }

    #[test]
    fn finds_the_headline_past_a_preamble() {
        let report = "All checks complete. Here's the report.\n\n## Morning health check — all clear\n\n**Headline: Box is healthy — Qwen3.8-27B is serving.**\n\nDetails…";
        assert_eq!(headline(report), "Box is healthy — Qwen3.8-27B is serving.");
        assert_eq!(headline("Preamble.\n# Big news\nmore"), "Big news");
        assert_eq!(headline("Just one line."), "Just one line.");
        assert_eq!(headline(""), "");
    }

    #[test]
    fn says_when_a_routine_runs() {
        assert_eq!(describe(&Schedule { kind: "weekdays".into(), time: Some("08:30".into()), ..Default::default() }), "Weekdays at 08:30");
        assert_eq!(describe(&Schedule { kind: "hourly".into(), every_hours: Some(1), ..Default::default() }), "Every hour");
        assert_eq!(describe(&Schedule { kind: "manual".into(), ..Default::default() }), "When you run it");
    }

    #[test]
    fn offers_lookups_and_actions_but_not_desktop_control() {
        let names: Vec<String> = tools().as_array().unwrap().iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect();
        for want in ["box_status", "community_models", "ask_model", "wait_until_ready", "start_model", "download_model"] {
            assert!(names.iter().any(|n| n == want), "missing {want}");
        }
        for never in ["control_computer", "open_app", "start_recording"] {
            assert!(!names.iter().any(|n| n == never), "{never} has no place in an unattended run");
        }
        assert!(is_action("start_model") && !is_action("ask_model"));
    }
}
