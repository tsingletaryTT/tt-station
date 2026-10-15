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
//! That's everything the box's account can do -- read TT Station's own keys and
//! settings, use docker and sudo -- so starting, watching and typing into one is
//! an owner's (`routes::OwnerAuth`); members code in Code's sandbox
//! (`crate::workspace`), in a folder [`member_folder`] allows.
//!
//! - `GET /coding`: agents, sessions, recent folders, and whether the served
//!   model suits coding agents (`check`).
//! - `POST /coding/sessions`: `{agent, folder, task?}`; `POST /coding/sessions/{id}/stop`;
//!   `DELETE /coding/sessions/{id}`; `GET /coding/sessions/{id}/changes` (git status and diff).
//! - `GET /coding/folders?path=`: folders to pick from; `POST /coding/folders`: `{parent, name}`
//!   makes a new project (a git repository).
//! - `GET /coding/term?session=&cols=&rows=` (WebSocket, token in a header -- `routes::ws_token`): the session's terminal.

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

use crate::routes::{AppState, BearerAuth, CallerAuth};

/// Where Claude Code compacts a session on the box when the model's agent
/// reliability hasn't been measured (`crate::evals::compact_window`): local models
/// get unreliable with tools well before their full window (gpt-oss-20b at ~85K
/// wrote calls as text).
pub const COMPACT_WINDOW: u64 = 65_536;

/// The most context Claude Code gets on the box, whatever the model serves: the
/// whole conversation is read again at every step, so a bigger window only makes
/// steps slower.
pub const MAX_WINDOW: u64 = 131_072;

/// The window Claude Code runs with on the box and where it compacts, for the
/// served model's `context` (its `max_model_len`, `None` when unknown). Both
/// launch paths (Code in `workspace.rs`, coding sessions here) and Code's speed
/// panel (`codespeed.rs`) use this, so what the panel shows is what was launched.
pub fn code_window(state: &AppState, context: Option<u64>) -> (u64, u64) {
    window_for(
        context,
        crate::evals::compact_window(state, state.endpoint().map(|e| e.model).as_deref()),
    )
}

/// [`code_window`]'s arithmetic: the served context capped at [`MAX_WINDOW`], and
/// compaction at `compact` (measured per model) but never past the window.
pub fn window_for(context: Option<u64>, compact: u64) -> (u64, u64) {
    let window = context.unwrap_or(MAX_WINDOW).min(MAX_WINDOW);
    (window, window.min(compact))
}

/// How Claude Code runs on the box's model, for every launch of it: the extra
/// arguments and environment both Code (`workspace.rs`) and coding sessions
/// (here) start it with. Each caller adds what is its own (config folder, key,
/// session header, thinking).
///
/// `context` is the served model's; `web` offers the web tools (not from a
/// sandbox, which has no internet).
pub(crate) fn claude_tuning(
    state: &AppState,
    context: Option<u64>,
    folder: &Path,
    web: bool,
) -> (Vec<String>, Vec<(String, String)>) {
    let (window, compact_at) = code_window(state, context);
    // Its answers fit in what's left of the context: Claude Code asks for large
    // output budgets by default, and prompt plus budget past the context is refused.
    let output = (window / 4).clamp(4096, 16_384);
    // Only the tools coding needs. Claude Code offers about twenty (agents,
    // schedules, worktrees, messages, plan mode…) that can't help here, and every
    // one is described in the prompt the model re-reads at each step.
    let mut tools = vec!["Bash", "Read", "Edit", "Write"];
    // Its description costs ~450 tokens at every step: only for projects with notebooks.
    if crate::workspace::has_notebooks(folder) {
        tools.push("NotebookEdit");
    }
    if web {
        tools.extend(["WebFetch", "WebSearch"]);
    }
    // One argument: `--tools a b` would also take a task that follows as a tool.
    let args = vec![format!("--tools={}", tools.join(","))];
    let env = [
        ("ANTHROPIC_API_KEY", String::new()),
        ("ANTHROPIC_MODEL", "quietbox".into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", "quietbox".into()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", "quietbox".into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "quietbox".into()),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS", window.to_string()),
        // Summarize long before the window is full: past some size a local model
        // starts inventing tools and writing its calls as text. Where, is measured
        // per model (the agent reliability eval); 64K when it hasn't been.
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW", compact_at.to_string()),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", output.to_string()),
        // No single file read or command output may take a big bite of the context:
        // a large file is read in parts (offset, limit) instead.
        ("CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS", "8000".into()),
        ("BASH_MAX_OUTPUT_LENGTH", "15000".into()),
        // No per-request header line at the top of the system prompt: it changes every
        // request, so the server could never reuse (prefix-cache) the conversation's start.
        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0".into()),
        ("DISABLE_TELEMETRY", "1".into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
        ("DISABLE_AUTOUPDATER", "1".into()),
    ];
    (
        args,
        env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
    )
}

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

const HOST: &str = include_str!("code_session.py");

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
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
    a.bins
        .iter()
        .map(|b| PathBuf::from(b.replacen('~', &home().display().to_string(), 1)))
        .find(|p| p.is_file())
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
    // Whole or not at all, owner-only from the first byte: a half-written key
    // would be thrown away (and a new one made) at the next start.
    crate::fsio::write_atomic(&path, k.as_bytes(), crate::fsio::PRIVATE)
        .map_err(|e| e.to_string())?;
    Ok(k)
}

/// The command and environment that start `agent` on the box's model.
/// A launch: the command line, and the environment it runs with.
type Launch = (Vec<String>, Vec<(String, String)>);

fn launch(
    state: &AppState,
    agent: &Agent,
    bin: &Path,
    task: Option<&str>,
    folder: &Path,
) -> Result<Launch, String> {
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
            let mut c: Value = std::fs::read(&cfg)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_else(|| json!({}));
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
            // Claude Code keeps its account details in here too: owner-only.
            crate::fsio::save_json(&cfg, &c, crate::fsio::PRIVATE);
            // Its context window: the served model's own (it can't know "quietbox").
            let context = FIT
                .lock()
                .expect("fit")
                .as_ref()
                .and_then(|(_, _, v)| v["context"].as_u64());
            let (args, mut env) = claude_tuning(state, context, folder, true);
            let mut cmd = vec![b];
            cmd.extend(args);
            if let Some(t) = task.filter(|t| !t.trim().is_empty()) {
                cmd.push(t.to_string());
            }
            env.extend([
                ("CLAUDE_CONFIG_DIR".to_string(), dir.display().to_string()),
                ("ANTHROPIC_BASE_URL".to_string(), base),
                ("ANTHROPIC_AUTH_TOKEN".to_string(), k),
            ]);
            (cmd, env)
        }
        "opencode" => {
            let cfg = root(state).join("opencode.json");
            let model = state
                .endpoint()
                .map(|e| e.model)
                .unwrap_or_else(|| "quietbox".into());
            let config = json!({
                "$schema": "https://opencode.ai/config.json",
                "model": "quietbox/quietbox",
                "provider": { "quietbox": {
                    "npm": "@ai-sdk/openai-compatible", "name": "QuietBox",
                    "options": { "baseURL": format!("{base}/v1"), "apiKey": k },
                    "models": { "quietbox": { "name": format!("QuietBox ({})", model.rsplit('/').next().unwrap_or(&model)), "limit": { "context": 131072, "output": 16384 } } }
                } }
            });
            // It holds the app key: owner-only.
            crate::fsio::write_json(&cfg, &config, crate::fsio::PRIVATE)
                .map_err(|e| e.to_string())?;
            (
                vec![b],
                vec![("OPENCODE_CONFIG".into(), cfg.display().to_string())],
            )
        }
        "aider" => (
            vec![
                b,
                "--from".into(),
                "aider-chat".into(),
                "aider".into(),
                "--model".into(),
                "openai/quietbox".into(),
                "--openai-api-base".into(),
                format!("{base}/v1"),
                "--no-show-model-warnings".into(),
                "--no-check-update".into(),
            ],
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
    std::fs::read(sessions_dir(state).join(id).join("session.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(state: &AppState, s: &Session) {
    // Atomic, so a crash mid-save can't leave a session that no longer loads
    // (and so vanishes from the list while its unit keeps running).
    crate::fsio::save_json(
        &sessions_dir(state).join(&s.id).join("session.json"),
        s,
        crate::fsio::PRIVATE,
    );
}

fn unit_active(id: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", &unit(id)])
        .status()
        .is_ok_and(|s| s.success())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A session's record, with whether it still runs (its exit code once it doesn't).
fn refresh(state: &AppState, mut s: Session) -> Session {
    if s.ended.is_none() && !unit_active(&s.id) {
        s.ended = Some(now());
        s.exit = std::fs::read_to_string(sessions_dir(state).join(&s.id).join("term.log.exit"))
            .ok()
            .and_then(|t| t.trim().parse().ok());
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
    v.sort_by_key(|s| std::cmp::Reverse(s.created));
    v
}

/// `git status --porcelain` lines in the folder (empty when it isn't a repository).
pub(crate) fn changed_files(folder: &str) -> Vec<String> {
    crate::safegit::changed_files(Path::new(folder), None)
}

fn describe(s: &Session) -> Value {
    let name = AGENTS
        .iter()
        .find(|a| a.id == s.agent)
        .map_or(s.agent.as_str(), |a| a.name);
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
    FIT.lock()
        .expect("fit")
        .as_ref()
        .filter(|(m, _, _)| m == model)
        .and_then(|(_, _, v)| v["context"].as_u64())
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
    let context = match crate::localhttp::local()
        .get(models_url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v["data"][0]["max_model_len"].as_u64()),
        Err(_) => None,
    };
    let body = json!({
        "model": ep.model, "max_tokens": 400, "temperature": 0,
        "messages": [{ "role": "user", "content": "Read the file src/main.rs. Use the tool." }],
        "tools": [{ "type": "function", "function": { "name": "read_file", "description": "Read a file", "parameters": { "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] } } }],
        "tool_choice": "auto",
    });
    let started = Instant::now();
    let r = crate::localhttp::local()
        .post(crate::assistant::chat_completions_url(&ep.base_url))
        .json(&body)
        .timeout(Duration::from_secs(120))
        .send()
        .await;
    let secs = started.elapsed().as_secs_f32();
    let reply: Option<Value> = match r {
        Ok(r) => r.json().await.ok(),
        Err(_) => None,
    };
    let call = reply
        .as_ref()
        .map(|v| v["choices"][0]["message"]["tool_calls"][0].clone())
        .unwrap_or(Value::Null);
    let args_ok = call["function"]["arguments"]
        .as_str()
        .and_then(|a| serde_json::from_str::<Value>(a).ok())
        .is_some_and(|a| a["path"].as_str().is_some_and(|p| p.contains("main.rs")));
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
/// A member gets the agents but no sessions: those are owners' (they run as the box).
pub(crate) async fn get_coding(
    State(state): State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
) -> Json<Value> {
    let c = check(&state).await;
    let st = state.clone();
    let owner = caller.owner();
    let (agents, sessions, recent) = tokio::task::spawn_blocking(move || {
        let agents: Vec<Value> = AGENTS.iter().map(|a| json!({ "id": a.id, "name": a.name, "blurb": a.blurb, "available": find_bin(a).is_some() })).collect();
        let sessions = if owner { all(&st) } else { Vec::new() };
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
    Json(
        json!({ "agents": agents, "sessions": sessions, "recent": recent, "check": c, "home": home(), "owner": owner }),
    )
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
    if p.strip_prefix(&h).is_ok_and(|r| {
        r.components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    }) {
        return Err(
            "That's a hidden folder (it may hold keys or settings). Pick a project folder.".into(),
        );
    }
    // A colon or comma would break the folder's mount into the sandbox.
    if p.to_string_lossy().contains([':', ',', '\n']) {
        return Err("That folder's name has a colon, comma or line break in it, which the sandbox can't mount. Rename it or pick another.".into());
    }
    Ok(p)
}

/// Where the box keeps what it runs as its own account: TT Station's settings
/// and state, its programs (every folder on `PATH`, the agent's own), and the
/// checkouts its features run code from (tt-inference-server, tt-metal,
/// tt-orchard, tt-lab, tt-kernel, tt-model-manager, TT Station's source).
/// Real paths where they exist.
fn box_software(state: &AppState) -> Vec<PathBuf> {
    let h = home();
    let env_or = |var: &str, default: PathBuf| std::env::var_os(var).map_or(default, PathBuf::from);
    let mut v = vec![
        state.config_dir(),
        PathBuf::from(crate::config::default_tt_inference_repo()),
        crate::native_vllm::tt_metal(),
        h.join("tt-kernel-package-manager"),
        h.join("dispatch"),
        env_or("TT_ORCHARD_DIR", h.join("code/tt-orchard")),
        h.join("code/tt-lab"),
        h.join("code/tt-model-manager"),
        h.join("code/tt-station"),
        h.join("bin"),
    ];
    if let Some(lab) = std::env::var_os("TT_LAB_BIN") {
        // `<checkout>/_out/tt-lab`
        v.extend(Path::new(&lab).ancestors().nth(2).map(Path::to_path_buf));
    }
    // This agent's own source, when it was built on the box.
    v.extend(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .map(Path::to_path_buf),
    );
    v.extend(
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(Path::to_path_buf)),
    );
    v.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    v.into_iter()
        .filter(|p| p.is_absolute())
        .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
        .collect()
}

/// [`member_folder`]'s rule, for a real path `p`: inside `home` but not `home`
/// itself, no hidden folder on the way, and neither inside nor holding any of
/// `blocked` (a folder holding one would hand the sandbox that one too).
pub(crate) fn member_rule(home: &Path, blocked: &[PathBuf], p: &Path) -> Result<(), String> {
    let Ok(rel) = p.strip_prefix(home) else {
        return Err("Pick a project folder inside the home folder.".into());
    };
    if rel.as_os_str().is_empty() {
        return Err("Pick a project folder inside the home folder.".into());
    }
    if rel
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    {
        return Err(
            "That's a hidden folder (it may hold keys or settings). Pick a project folder.".into(),
        );
    }
    if blocked.iter().any(|b| p.starts_with(b) || b.starts_with(p)) {
        return Err("That folder is (or holds) software the box runs as its own account, or TT Station's own settings, so a member's sandbox can't have it. Pick or make another project folder in the home folder.".into());
    }
    Ok(())
}

/// A project folder a member may give Code's sandbox: [`allowed_folder`], and
/// not where the box keeps what it runs as its own account ([`box_software`]).
/// The sandbox gets the folder read-write, and the box's account later runs
/// what's in there (a checkout it serves from, a folder on its `PATH`), so a
/// member's sandbox writing it would be a way out of the sandbox.
pub(crate) fn member_folder(state: &AppState, folder: &str) -> Result<PathBuf, String> {
    let p = allowed_folder(folder)?;
    let h = std::fs::canonicalize(home()).unwrap_or_else(|_| home());
    member_rule(&h, &box_software(state), &p)?;
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

/// `POST /coding/sessions` (owners).
pub(crate) async fn post_session(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(body): Json<StartBody>,
) -> Reply {
    let agent = AGENTS
        .iter()
        .find(|a| a.id == body.agent)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "Pick one of the listed agents."))?;
    let bin = find_bin(agent).ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            format!("{} isn't installed on the box.", agent.name),
        )
    })?;
    // A plain shell may start in the home folder itself (it runs as the user
    // anyway); the agents get a project folder.
    let home_dir = std::fs::canonicalize(home()).unwrap_or_else(|_| home());
    let folder = match std::fs::canonicalize(&body.folder) {
        Ok(p) if agent.id == "shell" && p == home_dir => p,
        _ => allowed_folder(&body.folder).map_err(|e| err(StatusCode::BAD_REQUEST, e))?,
    };
    if agent.id != "shell" && state.endpoint().is_none() {
        return Err(err(
            StatusCode::CONFLICT,
            "No model is serving. Start one in Models first.",
        ));
    }
    let task = body.task.map(|t| t.chars().take(4000).collect::<String>());
    let st = state.clone();
    let id = format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]);
    let sid = id.clone();
    let s = tokio::task::spawn_blocking(move || -> Result<Session, String> {
        let (cmd, env) = launch(&st, agent, &bin, task.as_deref(), &folder)?;
        let dir = sessions_dir(&st).join(&sid);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let session = Session {
            id: sid.clone(),
            agent: agent.id.into(),
            folder: folder.display().to_string(),
            task: task.clone(),
            created: now(),
            ended: None,
            exit: None,
        };
        save(&st, &session);
        let path = format!(
            "{}:{}:{}",
            home().join(".local/bin").display(),
            home().join(".opencode/bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut run = Command::new("systemd-run");
        run.args([
            "--user",
            "--collect",
            "--quiet",
            &format!("--unit={}", unit(&sid)),
            &format!("--working-directory={}", folder.display()),
        ])
        .arg(format!("--setenv=PATH={path}"))
        .arg(format!("--setenv=HOME={}", home().display()));
        // Names only: systemd-run takes each value from its own environment,
        // so the key never shows in a process list.
        for (k, v) in &env {
            run.arg(format!("--setenv={k}")).env(k, v);
        }
        let out = run
            .args([
                "python3",
                "-c",
                HOST,
                &sock(&st, &sid).display().to_string(),
                &dir.join("term.log").display().to_string(),
                "120",
                "36",
                "--",
            ])
            .args(&cmd)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "couldn't start it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(session)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    crate::audit::record(
        &state,
        "host_run",
        &format!("{} in {}", agent.name, s.folder),
        "ok",
    );
    Ok(Json(json!({ "session": describe(&s) })))
}

/// `POST /coding/sessions/{id}/stop`.
pub(crate) async fn post_stop(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if !valid_id(&id) || load(&state, &id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &unit(&id)])
        .status();
    Ok(Json(json!({ "stopped": true })))
}

/// `DELETE /coding/sessions/{id}`: forget an ended session (its folder and files stay).
pub(crate) async fn delete_session(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
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
pub(crate) async fn get_changes(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let s = load(&state, &id)
        .filter(|_| valid_id(&id))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let v = tokio::task::spawn_blocking(move || {
        let git = |args: &[&str]| {
            crate::safegit::command(Path::new(&s.folder))
                .args(args)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        };
        let is_repo = git(&["rev-parse", "--is-inside-work-tree"]).is_some();
        let mut diff = git(&["diff", "--no-ext-diff", "--no-textconv", "HEAD"])
            .or_else(|| git(&["diff", "--no-ext-diff", "--no-textconv"]))
            .unwrap_or_default();
        // New files git doesn't track yet, shown whole.
        for f in changed_files(&s.folder)
            .iter()
            .filter(|l| l.starts_with("??"))
            .take(20)
        {
            let name = f[3..].trim();
            let p = Path::new(&s.folder).join(name);
            if let Ok(text) = std::fs::read_to_string(&p) {
                if text.len() < 40_000 {
                    diff.push_str(&format!(
                        "\ndiff --git a/{name} b/{name}\nnew file\n--- /dev/null\n+++ b/{name}\n"
                    ));
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
    let p = std::fs::canonicalize(&path)
        .map_err(|_| err(StatusCode::NOT_FOUND, "That folder doesn't exist."))?;
    if !p.starts_with(std::fs::canonicalize(&h).unwrap_or(h.clone())) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Only folders in your home folder.",
        ));
    }
    let mut dirs: Vec<Value> = std::fs::read_dir(&p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir() && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| json!({ "name": e.file_name().to_string_lossy(), "path": e.path().display().to_string(), "git": e.path().join(".git").exists() }))
        .collect();
    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    Ok(Json(
        json!({ "path": p.display().to_string(), "parent": p.parent().filter(|_| p != std::fs::canonicalize(home()).unwrap_or_default()).map(|x| x.display().to_string()), "git": p.join(".git").exists(), "folders": dirs }),
    ))
}

#[derive(Debug, Deserialize)]
pub struct NewFolder {
    pub parent: String,
    pub name: String,
}

/// `POST /coding/folders`: a new project folder, as a git repository. A
/// member's goes where [`member_folder`] would let it be used.
pub(crate) async fn post_folder(
    State(state): State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    Json(b): Json<NewFolder>,
) -> Reply {
    let name = b.name.trim();
    if name.is_empty()
        || name.len() > 80
        || name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_ .".contains(c))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Use letters, numbers, spaces, - and _ in the name.",
        ));
    }
    let parent = std::fs::canonicalize(&b.parent)
        .map_err(|_| err(StatusCode::NOT_FOUND, "That folder doesn't exist."))?;
    if !parent.starts_with(std::fs::canonicalize(home()).unwrap_or_else(|_| home())) {
        return Err(err(StatusCode::BAD_REQUEST, "Only in your home folder."));
    }
    let p = parent.join(name);
    if !caller.owner() {
        let h = std::fs::canonicalize(home()).unwrap_or_else(|_| home());
        member_rule(&h, &box_software(&state), &p).map_err(|e| err(StatusCode::FORBIDDEN, e))?;
    }
    if p.exists() {
        return Err(err(
            StatusCode::CONFLICT,
            "A folder with that name is already there.",
        ));
    }
    std::fs::create_dir_all(&p)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let _ = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&p)
        .status();
    Ok(Json(json!({ "path": p.display().to_string() })))
}

#[derive(Debug, Deserialize)]
pub struct TermQuery {
    pub session: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// `GET /coding/term` (WebSocket, owners): a session's terminal.
pub(crate) async fn term_ws(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<TermQuery>,
) -> Response {
    // The token in a header, not the URL (crate::routes::ws_token).
    let Some(token) = crate::routes::ws_token(&headers).filter(|t| state.is_valid_token(t)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    // A terminal is the box's own account: an owner's.
    match crate::routes::resolve_caller(&headers, false, &state) {
        Ok(c) if c.owner() => {}
        Ok(c) => {
            crate::audit::record_as(&state, &c.actor(), "denied", "GET /coding/term", "denied");
            return (StatusCode::FORBIDDEN, crate::routes::OWNER_ONLY).into_response();
        }
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    }
    // Removing this device (or resetting the box) closes its terminal too.
    let signed_out = state.signed_out(token);
    let ws = ws.protocols([crate::routes::WS_PROTOCOL]);
    if !valid_id(&q.session) || load(&state, &q.session).is_none() {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    let path = sock(&state, &q.session);
    let (cols, rows) = (
        q.cols.unwrap_or(120).clamp(40, 400),
        q.rows.unwrap_or(36).clamp(10, 200),
    );
    ws.on_upgrade(move |socket| bridge(socket, path, cols, rows, signed_out))
}

async fn bridge(
    mut socket: WebSocket,
    path: PathBuf,
    cols: u16,
    rows: u16,
    signed_out: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(signed_out);
    let conn = match tokio::net::UnixStream::connect(&path).await {
        Ok(c) => c,
        Err(_) => {
            let _ = socket
                .send(Message::Binary(
                    "\r\n\x1b[2m[this session has ended]\x1b[0m\r\n"
                        .as_bytes()
                        .to_vec()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let (mut rd, mut wr) = conn.into_split();
    let _ = wr.write_all(format!("r {cols} {rows}\n").as_bytes()).await;
    let mut buf = vec![0u8; 65536];
    loop {
        tokio::select! {
            () = &mut signed_out => {
                let _ = socket.send(Message::Close(Some(crate::routes::signed_out_frame()))).await;
                break;
            }
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
        let sessions = tokio::task::spawn_blocking(move || all(&st))
            .await
            .unwrap_or_default();
        for s in &sessions {
            if s.ended.is_none() {
                live.insert(s.id.clone());
            } else if live.remove(&s.id) && !first {
                let name = AGENTS
                    .iter()
                    .find(|a| a.id == s.agent)
                    .map_or("The agent", |a| a.name);
                let folder = Path::new(&s.folder)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                let n = changed_files(&s.folder).len();
                state.notifier().emit(crate::notify::Draft {
                    kind: "coding",
                    title: format!("{name} finished in {folder}"),
                    body: if n > 0 {
                        format!(
                            "{n} changed file{} to review in TT Station → Code.",
                            if n == 1 { "" } else { "s" }
                        )
                    } else {
                        "No changed files.".into()
                    },
                });
            }
        }
        first = false;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(dir.path().to_path_buf());
        (dir, state)
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn the_coding_key_is_written_whole_and_owner_only() {
        let (_dir, state) = temp_state();
        let k = key(&state).unwrap();
        let path = root(&state).join("key");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), k);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // The same key comes back while it's valid, and no temp file is left over.
        assert_eq!(key(&state).unwrap(), k);
        assert!(
            !names_in(&root(&state)).iter().any(|n| n.contains(".tmp-")),
            "{:?}",
            names_in(&root(&state))
        );
    }

    #[test]
    fn a_session_saves_and_loads_back_with_no_temp_files() {
        let (_dir, state) = temp_state();
        let s = Session {
            id: "123-abcdef".into(),
            agent: "claude".into(),
            folder: "/h/projects/site".into(),
            task: Some("fix the tests".into()),
            created: 123,
            ended: None,
            exit: None,
        };
        save(&state, &s);
        let mut back = load(&state, &s.id).unwrap();
        assert_eq!(back.task.as_deref(), Some("fix the tests"));
        assert_eq!(back.folder, s.folder);
        back.ended = Some(200);
        back.exit = Some(0);
        save(&state, &back);
        let again = load(&state, &s.id).unwrap();
        assert_eq!((again.ended, again.exit), (Some(200), Some(0)));
        let dir = sessions_dir(&state).join(&s.id);
        assert_eq!(names_in(&dir), vec!["session.json".to_string()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("session.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn members_folders_are_projects_not_the_box_own_software() {
        let home = Path::new("/h");
        let blocked = [
            PathBuf::from("/h/code/tt-metal"),
            PathBuf::from("/h/code/tt-station"),
            PathBuf::from("/h/bin"),
            PathBuf::from("/h/.config/tt-station"),
        ];
        let ok = |p: &str| member_rule(home, &blocked, Path::new(p));
        assert!(ok("/h/projects/site").is_ok());
        assert!(ok("/h/code/mine").is_ok());
        for refused in [
            "/h",                      // the whole home
            "/elsewhere/proj",         // outside it
            "/h/.ssh",                 // hidden: keys and settings
            "/h/projects/.secret/x",   // hidden on the way
            "/h/code",                 // holds the box's checkouts
            "/h/code/tt-metal/models", // inside one
            "/h/code/tt-station",      // the agent's own source
            "/h/bin",                  // on PATH
        ] {
            assert!(ok(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn the_box_software_includes_its_settings_and_programs() {
        let dir = std::env::temp_dir().join(format!("tt-box-software-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(dir.clone());
        let v = box_software(&state);
        assert!(v.contains(&std::fs::canonicalize(&dir).unwrap()));
        let exe = std::env::current_exe().unwrap();
        assert!(v.iter().any(|p| exe.starts_with(p)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_is_the_served_context_capped_and_compaction_fits_in_it() {
        assert_eq!(
            window_for(Some(131_072), COMPACT_WINDOW),
            (131_072, COMPACT_WINDOW)
        );
        assert_eq!(
            window_for(Some(262_144), COMPACT_WINDOW),
            (MAX_WINDOW, COMPACT_WINDOW)
        );
        assert_eq!(window_for(Some(32_768), COMPACT_WINDOW), (32_768, 32_768));
        assert_eq!(
            window_for(None, COMPACT_WINDOW),
            (MAX_WINDOW, COMPACT_WINDOW)
        );
        // A measured compaction point is used as is.
        assert_eq!(window_for(Some(131_072), 98_304), (131_072, 98_304));
    }

    #[test]
    fn every_claude_launch_is_tuned_for_the_prefix_cache_and_a_small_window() {
        let dir = std::env::temp_dir().join(format!("tt-claude-tuning-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(dir.clone());
        let (args, env) = claude_tuning(&state, Some(262_144), &dir, false);
        assert_eq!(args, ["--tools=Bash,Read,Edit,Write"]);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        // The window and compaction point are the ones the speed panel reports.
        let (window, compact_at) = code_window(&state, Some(262_144));
        assert_eq!(
            get("CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some(window.to_string().as_str())
        );
        assert_eq!(
            get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some(compact_at.to_string().as_str())
        );
        assert_eq!(get("CLAUDE_CODE_ATTRIBUTION_HEADER"), Some("0"));
        assert_eq!(get("CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS"), Some("8000"));
        assert_eq!(get("BASH_MAX_OUTPUT_LENGTH"), Some("15000"));
        assert_eq!(get("CLAUDE_CODE_MAX_OUTPUT_TOKENS"), Some("16384"));
        let (args, _) = claude_tuning(&state, None, &dir, true);
        assert_eq!(args, ["--tools=Bash,Read,Edit,Write,WebFetch,WebSearch"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
