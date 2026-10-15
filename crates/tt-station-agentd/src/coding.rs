// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Coding sessions: a coding agent (Claude Code, OpenCode, Aider) working in a
//! project folder on the box, on the box's own model, watched and steered from
//! TT Station on the desktop or the phone.
//!
//! Each session is the agent's own terminal app, held by a small session host
//! (`code_session.py`, its own systemd unit, so it outlives the app and this
//! agent) and streamed over a WebSocket. Claude Code reaches the model through
//! the box's Anthropic API (`crate::anthropic`), the others through its OpenAI
//! one, all with one app key ("Coding sessions"). Claude Code gets its own
//! settings folder, so a person's own Claude account and settings aren't touched.
//!
//! Sessions run as the box's user, in the chosen folder, like a terminal would:
//! each agent asks before it runs commands or edits (its own permission prompts).
//!
//! - `GET /coding`: agents, sessions, recent folders, and whether the served
//!   model suits coding agents (`check`).
//! - `POST /coding/sessions`: `{agent, folder, task?}`; `POST /coding/sessions/{id}/stop`;
//!   `DELETE /coding/sessions/{id}`; `GET /coding/sessions/{id}/changes` (git status and diff).
//! - `GET /coding/folders?path=`: folders to pick from; `POST /coding/folders`: `{parent, name}`
//!   makes a new project (a git repository).
//! - `GET /coding/term?token=&session=&cols=&rows=` (WebSocket): the session's terminal.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::routes::{AppState, BearerAuth};

/// Where Claude Code compacts a session on the box: local models get unreliable
/// with tools well before their full window (gpt-oss-20b at ~85K wrote calls as text).
pub const COMPACT_WINDOW: u64 = 65_536;

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

const HOST: &str = include_str!("code_session.py");

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn root(state: &AppState) -> PathBuf {
    state.config_dir().join("coding")
}

// ---------------------------------------------------------------- agents

struct Agent {
    id: &'static str,
    name: &'static str,
    blurb: &'static str,
    /// Where its program may be.
    bins: &'static [&'static str],
}

const AGENTS: &[Agent] = &[
    Agent { id: "claude", name: "Claude Code", blurb: "Anthropic's coding agent, on the box's model through its Anthropic API. Plans, edits files, runs commands and tests, asking you first.", bins: &["~/.local/bin/claude", "/usr/local/bin/claude", "/usr/bin/claude"] },
    Agent { id: "opencode", name: "OpenCode", blurb: "The open-source coding agent for the terminal, with plan and build modes.", bins: &["~/.opencode/bin/opencode", "~/.local/bin/opencode", "/usr/local/bin/opencode"] },
    Agent { id: "aider", name: "Aider", blurb: "Pair programming that commits each change to git. Downloads itself the first time (through uv).", bins: &["~/.local/bin/uvx", "/usr/local/bin/uvx", "/usr/bin/uvx"] },
    Agent { id: "shell", name: "Terminal", blurb: "A plain terminal in the folder: run tests, git, or anything else.", bins: &["/bin/bash"] },
];

fn find_bin(a: &Agent) -> Option<PathBuf> {
    a.bins.iter().map(|b| PathBuf::from(b.replacen('~', &home().display().to_string(), 1))).find(|p| p.is_file())
}

/// The one app key coding sessions use (made once, kept 0600).
pub(crate) fn key(state: &AppState) -> Result<String, String> {
    let path = root(state).join("key");
    if let Ok(k) = std::fs::read_to_string(&path) {
        let k = k.trim().to_string();
        if state.app_keys().check(&k, now()) {
            return Ok(k);
        }
    }
    let (_, k) = state.app_keys().create("Coding sessions", now())?;
    std::fs::create_dir_all(root(state)).map_err(|e| e.to_string())?;
    std::fs::write(&path, &k).map_err(|e| e.to_string())?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    Ok(k)
}

/// The command and environment that start `agent` on the box's model.
fn launch(state: &AppState, agent: &Agent, bin: &Path, task: Option<&str>, folder: &Path) -> Result<(Vec<String>, Vec<(String, String)>), String> {
    let base = format!("http://127.0.0.1:{}", state.ctrl_port());
    let k = key(state)?;
    let b = bin.display().to_string();
    Ok(match agent.id {
        "claude" => {
            // Its own settings folder: the person's own Claude account and settings stay as they are.
            let dir = root(state).join("claude-home");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            // Onboarding done, and the folder trusted: picking it in TT Station is that choice.
            let cfg = dir.join(".claude.json");
            let mut c: Value = std::fs::read(&cfg).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
            c["hasCompletedOnboarding"] = json!(true);
            if c["theme"].is_null() {
                c["theme"] = json!("dark");
            }
            if !c["projects"].is_object() {
                c["projects"] = json!({});
            }
            let f = folder.display().to_string();
            if !c["projects"][&f].is_object() {
                c["projects"][&f] = json!({});
            }
            c["projects"][&f]["hasTrustDialogAccepted"] = json!(true);
            let _ = std::fs::write(&cfg, serde_json::to_vec_pretty(&c).unwrap_or_default());
            // Its context window: the served model's own (it can't know "quietbox").
            let window = FIT.lock().expect("fit").as_ref().and_then(|(_, _, v)| v["context"].as_u64()).unwrap_or(131_072);
            let mut cmd = vec![b];
            if let Some(t) = task.filter(|t| !t.trim().is_empty()) {
                cmd.push(t.to_string());
            }
            let env = [
                ("CLAUDE_CONFIG_DIR", dir.display().to_string()),
                ("ANTHROPIC_BASE_URL", base),
                ("ANTHROPIC_AUTH_TOKEN", k),
                ("ANTHROPIC_MODEL", "quietbox".into()),
                ("ANTHROPIC_DEFAULT_OPUS_MODEL", "quietbox".into()),
                ("ANTHROPIC_DEFAULT_SONNET_MODEL", "quietbox".into()),
                ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "quietbox".into()),
                ("DISABLE_TELEMETRY", "1".into()),
                ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
                ("ANTHROPIC_API_KEY", String::new()),
                ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", window.min(1_000_000).to_string()),
                ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", window.min(COMPACT_WINDOW).to_string()),
            ];
            (cmd, env.into_iter().map(|(a, b)| (a.to_string(), b)).collect())
        }
        "opencode" => {
            let cfg = root(state).join("opencode.json");
            let model = state.endpoint().map(|e| e.model).unwrap_or_else(|| "quietbox".into());
            let config = json!({
                "$schema": "https://opencode.ai/config.json",
                "model": "quietbox/quietbox",
                "provider": { "quietbox": {
                    "npm": "@ai-sdk/openai-compatible", "name": "QuietBox",
                    "options": { "baseURL": format!("{base}/v1"), "apiKey": k },
                    "models": { "quietbox": { "name": format!("QuietBox ({})", model.rsplit('/').next().unwrap_or(&model)), "limit": { "context": 131072, "output": 16384 } } }
                } }
            });
            std::fs::write(&cfg, serde_json::to_vec_pretty(&config).unwrap_or_default()).map_err(|e| e.to_string())?;
            (vec![b], vec![("OPENCODE_CONFIG".into(), cfg.display().to_string())])
        }
        "aider" => (
            vec![b, "--from".into(), "aider-chat".into(), "aider".into(), "--model".into(), "openai/quietbox".into(),
                 "--openai-api-base".into(), format!("{base}/v1"), "--no-show-model-warnings".into(), "--no-check-update".into()],
            vec![("OPENAI_API_KEY".into(), k)],
        ),
        _ => (vec![b, "-l".into()], vec![]),
    })
}

// ---------------------------------------------------------------- sessions

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    pub folder: String,
    #[serde(default)]
    pub task: Option<String>,
    pub created: u64,
    #[serde(default)]
    pub ended: Option<u64>,
    #[serde(default)]
    pub exit: Option<i32>,
}

fn sessions_dir(state: &AppState) -> PathBuf {
    root(state).join("sessions")
}

fn unit(id: &str) -> String {
    format!("tt-code-{id}")
}

fn sock(state: &AppState, id: &str) -> PathBuf {
    sessions_dir(state).join(id).join("term.sock")
}

fn load(state: &AppState, id: &str) -> Option<Session> {
    std::fs::read(sessions_dir(state).join(id).join("session.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(state: &AppState, s: &Session) {
    let _ = std::fs::write(sessions_dir(state).join(&s.id).join("session.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
}

fn unit_active(id: &str) -> bool {
    Command::new("systemctl").args(["--user", "is-active", "--quiet", &unit(id)]).status().is_ok_and(|s| s.success())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A session's record, with whether it still runs (its exit code once it doesn't).
fn refresh(state: &AppState, mut s: Session) -> Session {
    if s.ended.is_none() && !unit_active(&s.id) {
        s.ended = Some(now());
        s.exit = std::fs::read_to_string(sessions_dir(state).join(&s.id).join("term.log.exit")).ok().and_then(|t| t.trim().parse().ok());
        save(state, &s);
    }
    s
}

fn all(state: &AppState) -> Vec<Session> {
    let mut v: Vec<Session> = std::fs::read_dir(sessions_dir(state))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| load(state, &e.file_name().to_string_lossy()))
        .map(|s| refresh(state, s))
        .collect();
    v.sort_by(|a, b| b.created.cmp(&a.created));
    v
}

/// `git status --porcelain` lines in the folder (empty when it isn't a repository).
pub(crate) fn changed_files(folder: &str) -> Vec<String> {
    Command::new("git")
        .args(["-C", folder, "status", "--porcelain"])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn describe(s: &Session) -> Value {
    let name = AGENTS.iter().find(|a| a.id == s.agent).map_or(s.agent.as_str(), |a| a.name);
    json!({
        "id": s.id, "agent": s.agent, "agent_name": name, "folder": s.folder, "task": s.task,
        "created": s.created, "ended": s.ended, "exit": s.exit, "live": s.ended.is_none(),
        "changes": changed_files(&s.folder).len(),
    })
}

// ---------------------------------------------------------------- the model's fitness

static FIT: LazyLock<Mutex<Option<(String, Instant, Value)>>> = LazyLock::new(|| Mutex::new(None));

/// The served model's context, from the last check of it (no new request).
pub(crate) fn known_context(model: &str) -> Option<u64> {
    FIT.lock().expect("fit").as_ref().filter(|(m, _, _)| m == model).and_then(|(_, _, v)| v["context"].as_u64())
}

/// Does the served model make clean tool calls, and how much does it remember?
/// One small tool-call request; the answer is kept for ten minutes per model.
pub(crate) async fn check(state: &AppState) -> Value {
    let Some(ep) = state.endpoint() else {
        return json!({ "ok": false, "level": "block", "why": "No model is serving. Coding agents run on the box's model: start one in Models (Qwen3-Coder-Next or Qwen3-32B are good fits)." });
    };
    if let Some((m, at, v)) = FIT.lock().expect("fit").clone() {
        if m == ep.model && at.elapsed() < Duration::from_secs(600) {
            return v;
        }
    }
    let models_url = format!("http://127.0.0.1:{}/v1/models", state.serving_port());
    let context = match reqwest::Client::new().get(models_url).timeout(Duration::from_secs(5)).send().await {
        Ok(r) => r.json::<Value>().await.ok().and_then(|v| v["data"][0]["max_model_len"].as_u64()),
        Err(_) => None,
    };
    let body = json!({
        "model": ep.model, "max_tokens": 400, "temperature": 0,
        "messages": [{ "role": "user", "content": "Read the file src/main.rs. Use the tool." }],
        "tools": [{ "type": "function", "function": { "name": "read_file", "description": "Read a file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } } }],
        "tool_choice": "auto",
    });
    let started = Instant::now();
    let r = reqwest::Client::new().post(crate::assistant::chat_completions_url(&ep.base_url)).json(&body).timeout(Duration::from_secs(120)).send().await;
    let secs = started.elapsed().as_secs_f32();
    let reply: Option<Value> = match r {
        Ok(r) => r.json().await.ok(),
        Err(_) => None,
    };
    let call = reply.as_ref().map(|v| v["choices"][0]["message"]["tool_calls"][0].clone()).unwrap_or(Value::Null);
    let args_ok = call["function"]["arguments"].as_str().and_then(|a| serde_json::from_str::<Value>(a).ok()).is_some_and(|a| a["path"].as_str().is_some_and(|p| p.contains("main.rs")));
    let tools_ok = call["function"]["name"] == "read_file" && args_ok;
    let short = ep.model.rsplit('/').next().unwrap_or(&ep.model).to_string();
    let (ok, level, why) = match (reply.is_some(), tools_ok, context) {
        (false, _, _) => (false, "block", format!("{short} didn't answer a test request.")),
        (true, false, _) => (false, "warn", format!("{short} answered a test request without a proper tool call, so coding agents will struggle: they work by calling tools. A coder model (Qwen3-Coder-Next) is a better fit.")),
        (true, true, Some(c)) if c < 32_768 => (true, "warn", format!("{short} makes tool calls, but remembers only {}K tokens; coding agents need 64K or more for real projects.", c / 1024)),
        _ => (true, "ok", format!("{short} makes clean tool calls{}.", context.map(|c| format!(" and remembers {}K tokens", c / 1024)).unwrap_or_default())),
    };
    let v = json!({ "ok": ok, "level": level, "why": why, "model": ep.model, "context": context, "tool_calls": tools_ok, "seconds": secs });
    *FIT.lock().expect("fit") = Some((ep.model.clone(), Instant::now(), v.clone()));
    v
}

// ---------------------------------------------------------------- routes

/// `GET /coding`.
pub(crate) async fn get_coding(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let c = check(&state).await;
    let st = state.clone();
    let (agents, sessions, recent) = tokio::task::spawn_blocking(move || {
        let agents: Vec<Value> = AGENTS.iter().map(|a| json!({ "id": a.id, "name": a.name, "blurb": a.blurb, "available": find_bin(a).is_some() })).collect();
        let sessions = all(&st);
        let mut recent: Vec<String> = Vec::new();
        for s in &sessions {
            if !recent.contains(&s.folder) && Path::new(&s.folder).is_dir() {
                recent.push(s.folder.clone());
            }
        }
        recent.truncate(6);
        (agents, sessions.iter().map(describe).collect::<Vec<_>>(), recent)
    })
    .await
    .unwrap_or_default();
    Json(json!({ "agents": agents, "sessions": sessions, "recent": recent, "check": c, "home": home() }))
}

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub agent: String,
    pub folder: String,
    #[serde(default)]
    pub task: Option<String>,
}

/// Only folders in the user's home (not the home itself, not hidden ones' insides).
pub(crate) fn allowed_folder(folder: &str) -> Result<PathBuf, String> {
    let p = std::fs::canonicalize(folder).map_err(|_| "That folder doesn't exist.".to_string())?;
    let h = std::fs::canonicalize(home()).unwrap_or_else(|_| home());
    if !p.is_dir() || !p.starts_with(&h) || p == h {
        return Err("Pick a project folder inside your home folder.".into());
    }
    // Not a hidden folder (~/.ssh, ~/.config holding keys and TT Station's own
    // state): an agent, sandboxed or not, gets the whole folder.
    if p.strip_prefix(&h).is_ok_and(|r| r.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))) {
        return Err("That's a hidden folder (it may hold keys or settings). Pick a project folder.".into());
    }
    // A colon or comma would break the folder's mount into the sandbox.
    if p.to_string_lossy().contains([':', ',', '\n']) {
        return Err("That folder's name has a colon, comma or line break in it, which the sandbox can't mount. Rename it or pick another.".into());
    }
    Ok(p)
}

/// Cuts `s` to at most `max` bytes, at a character boundary.
pub(crate) fn cut(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s.truncate(n);
    true
}

/// `POST /coding/sessions`.
pub(crate) async fn post_session(State(state): State<AppState>, _auth: BearerAuth, Json(body): Json<StartBody>) -> Reply {
    let agent = AGENTS.iter().find(|a| a.id == body.agent).ok_or_else(|| err(StatusCode::BAD_REQUEST, "Pick one of the listed agents."))?;
    let bin = find_bin(agent).ok_or_else(|| err(StatusCode::CONFLICT, format!("{} isn't installed on the box.", agent.name)))?;
    // A plain shell may start in the home folder itself (it runs as the user
    // anyway); the agents get a project folder.
    let home_dir = std::fs::canonicalize(home()).unwrap_or_else(|_| home());
    let folder = match std::fs::canonicalize(&body.folder) {
        Ok(p) if agent.id == "shell" && p == home_dir => p,
        _ => allowed_folder(&body.folder).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
    };
    if agent.id != "shell" && state.endpoint().is_none() {
        return Err(err(StatusCode::CONFLICT, "No model is serving. Start one in Models first."));
    }
    let task = body.task.map(|t| t.chars().take(4000).collect::<String>());
    let st = state.clone();
    let id = format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]);
    let sid = id.clone();
    let s = tokio::task::spawn_blocking(move || -> Result<Session, String> {
        let (cmd, env) = launch(&st, agent, &bin, task.as_deref(), &folder)?;
        let dir = sessions_dir(&st).join(&sid);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let session = Session { id: sid.clone(), agent: agent.id.into(), folder: folder.display().to_string(), task: task.clone(), created: now(), ended: None, exit: None };
        save(&st, &session);
        let path = format!("{}:{}:{}", home().join(".local/bin").display(), home().join(".opencode/bin").display(), std::env::var("PATH").unwrap_or_default());
        let mut run = Command::new("systemd-run");
        run.args(["--user", "--collect", "--quiet", &format!("--unit={}", unit(&sid)), &format!("--working-directory={}", folder.display())])
            .arg(format!("--setenv=PATH={path}"))
            .arg(format!("--setenv=HOME={}", home().display()));
        for (k, v) in &env {
            run.arg(format!("--setenv={k}={v}"));
        }
        let out = run
            .args(["python3", "-c", HOST, &sock(&st, &sid).display().to_string(), &dir.join("term.log").display().to_string(), "120", "36", "--"])
            .args(&cmd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!("couldn't start it: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(session)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "session": describe(&s) })))
}

/// `POST /coding/sessions/{id}/stop`.
pub(crate) async fn post_stop(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_id(&id) || load(&state, &id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let _ = Command::new("systemctl").args(["--user", "stop", &unit(&id)]).status();
    Ok(Json(json!({ "stopped": true })))
}

/// `DELETE /coding/sessions/{id}`: forget an ended session (its folder and files stay).
pub(crate) async fn delete_session(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_id(&id) || load(&state, &id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    if unit_active(&id) {
        return Err(err(StatusCode::CONFLICT, "Stop it first."));
    }
    let _ = std::fs::remove_dir_all(sessions_dir(&state).join(&id));
    Ok(Json(json!({ "deleted": true })))
}

/// `GET /coding/sessions/{id}/changes`: what the agent changed (git status and diff).
pub(crate) async fn get_changes(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let s = load(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let v = tokio::task::spawn_blocking(move || {
        let git = |args: &[&str]| Command::new("git").arg("-C").arg(&s.folder).args(args).stdin(Stdio::null()).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).to_string());
        let is_repo = git(&["rev-parse", "--is-inside-work-tree"]).is_some();
        let mut diff = git(&["diff", "HEAD"]).or_else(|| git(&["diff"])).unwrap_or_default();
        // New files git doesn't track yet, shown whole.
        for f in changed_files(&s.folder).iter().filter(|l| l.starts_with("??")).take(20) {
            let name = f[3..].trim();
            let p = Path::new(&s.folder).join(name);
            if let Ok(text) = std::fs::read_to_string(&p) {
                if text.len() < 40_000 {
                    diff.push_str(&format!("\ndiff --git a/{name} b/{name}\nnew file\n--- /dev/null\n+++ b/{name}\n"));
                    for l in text.lines() {
                        diff.push_str(&format!("+{l}\n"));
                    }
                }
            }
        }
        if cut(&mut diff, 400_000) {
            diff.push_str("\n[…the rest is cut off]");
        }
        let log = git(&["log", "--oneline", "-8"]).unwrap_or_default();
        json!({ "repo": is_repo, "files": changed_files(&s.folder), "diff": diff, "log": log })
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(v))
}

#[derive(Debug, Deserialize)]
pub struct FolderQuery {
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET /coding/folders?path=`: the folders inside one (your home by default).
pub(crate) async fn get_folders(_auth: BearerAuth, Query(q): Query<FolderQuery>) -> Reply {
    let h = home();
    let path = q.path.map(PathBuf::from).unwrap_or_else(|| h.clone());
    let p = std::fs::canonicalize(&path).map_err(|_| err(StatusCode::NOT_FOUND, "That folder doesn't exist."))?;
    if !p.starts_with(std::fs::canonicalize(&h).unwrap_or(h.clone())) {
        return Err(err(StatusCode::BAD_REQUEST, "Only folders in your home folder."));
    }
    let mut dirs: Vec<Value> = std::fs::read_dir(&p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| json!({ "name": e.file_name().to_string_lossy(), "path": e.path().display().to_string(), "git": e.path().join(".git").exists() }))
        .collect();
    dirs.sort_by(|a, b| a["name"].as_str().unwrap_or("").to_lowercase().cmp(&b["name"].as_str().unwrap_or("").to_lowercase()));
    Ok(Json(json!({ "path": p.display().to_string(), "parent": p.parent().filter(|_| p != std::fs::canonicalize(home()).unwrap_or_default()).map(|x| x.display().to_string()), "git": p.join(".git").exists(), "folders": dirs })))
}

#[derive(Debug, Deserialize)]
pub struct NewFolder {
    pub parent: String,
    pub name: String,
}

/// `POST /coding/folders`: a new project folder, as a git repository.
pub(crate) async fn post_folder(_auth: BearerAuth, Json(b): Json<NewFolder>) -> Reply {
    let name = b.name.trim();
    if name.is_empty() || name.len() > 80 || name.starts_with('.') || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_ .".contains(c)) {
        return Err(err(StatusCode::BAD_REQUEST, "Use letters, numbers, spaces, - and _ in the name."));
    }
    let parent = std::fs::canonicalize(&b.parent).map_err(|_| err(StatusCode::NOT_FOUND, "That folder doesn't exist."))?;
    if !parent.starts_with(std::fs::canonicalize(home()).unwrap_or_else(|_| home())) {
        return Err(err(StatusCode::BAD_REQUEST, "Only in your home folder."));
    }
    let p = parent.join(name);
    if p.exists() {
        return Err(err(StatusCode::CONFLICT, "A folder with that name is already there."));
    }
    std::fs::create_dir_all(&p).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = Command::new("git").args(["init", "-q"]).current_dir(&p).status();
    Ok(Json(json!({ "path": p.display().to_string() })))
}

#[derive(Debug, Deserialize)]
pub struct TermQuery {
    pub token: String,
    pub session: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// `GET /coding/term` (WebSocket): a session's terminal.
pub(crate) async fn term_ws(ws: WebSocketUpgrade, State(state): State<AppState>, Query(q): Query<TermQuery>) -> Response {
    if !state.is_valid_token(&q.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !valid_id(&q.session) || load(&state, &q.session).is_none() {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    let path = sock(&state, &q.session);
    let (cols, rows) = (q.cols.unwrap_or(120).clamp(40, 400), q.rows.unwrap_or(36).clamp(10, 200));
    ws.on_upgrade(move |socket| bridge(socket, path, cols, rows))
}

async fn bridge(mut socket: WebSocket, path: PathBuf, cols: u16, rows: u16) {
    let conn = match tokio::net::UnixStream::connect(&path).await {
        Ok(c) => c,
        Err(_) => {
            let _ = socket.send(Message::Binary("\r\n\x1b[2m[this session has ended]\x1b[0m\r\n".as_bytes().to_vec().into())).await;
            return;
        }
    };
    let (mut rd, mut wr) = conn.into_split();
    let _ = wr.write_all(format!("r {cols} {rows}\n").as_bytes()).await;
    let mut buf = vec![0u8; 65536];
    loop {
        tokio::select! {
            n = rd.read(&mut buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => if socket.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() { break },
                }
            }
            msg = socket.recv() => {
                let Some(Ok(msg)) = msg else { break };
                let Message::Text(t) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&t) else { continue };
                let line = match v["type"].as_str() {
                    Some("input") => format!("i {}\n", base64::engine::general_purpose::STANDARD.encode(v["data"].as_str().unwrap_or("").as_bytes())),
                    Some("resize") => format!("r {} {}\n", v["cols"].as_u64().unwrap_or(120).clamp(40, 400), v["rows"].as_u64().unwrap_or(36).clamp(10, 200)),
                    _ => continue,
                };
                if wr.write_all(line.as_bytes()).await.is_err() { break }
            }
        }
    }
}

/// Watch sessions; when one ends, say so (with how many files it changed).
pub async fn watch_forever(state: AppState) {
    let mut live: HashSet<String> = HashSet::new();
    let mut first = true;
    loop {
        let st = state.clone();
        let sessions = tokio::task::spawn_blocking(move || all(&st)).await.unwrap_or_default();
        for s in &sessions {
            if s.ended.is_none() {
                live.insert(s.id.clone());
            } else if live.remove(&s.id) && !first {
                let name = AGENTS.iter().find(|a| a.id == s.agent).map_or("The agent", |a| a.name);
                let folder = Path::new(&s.folder).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                let n = changed_files(&s.folder).len();
                state.notifier().emit(crate::notify::Draft {
                    kind: "coding",
                    title: format!("{name} finished in {folder}"),
                    body: if n > 0 { format!("{n} changed file{} to review in TT Station → Code.", if n == 1 { "" } else { "s" }) } else { "No changed files.".into() },
                });
            }
        }
        first = false;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
