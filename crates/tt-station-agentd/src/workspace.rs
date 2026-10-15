// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Code: agentic coding in TT Station's own window, on the box's model.
//!
//! Each session drives Claude Code in its structured mode (`--input-format
//! stream-json --output-format stream-json`): what it says, every tool it
//! uses and every result come back as events the window draws, and each
//! permission request (`can_use_tool`) waits for the person's answer in the
//! window (Allow, Always, Deny) or on the phone. Claude Code reaches the box's
//! model through its Anthropic API (`crate::anthropic`).
//!
//! **The sandbox** (the default): Claude Code runs in a container that can
//! see only the project folder, with no internet, no LAN, no way to the box
//! itself except the model API (through `tt-code-gate`, which forwards
//! `/v1/…` and nothing else), no Linux capabilities, no privilege escalation,
//! a read-only system, resource limits, and no Tenstorrent devices. What an
//! agent can still do: change files in the project (review them in Changes;
//! commit or discard) and use the model. Without the sandbox it runs as the
//! person, like a terminal, and always asks first.
//!
//! A session's conversation is kept by Claude Code (`--resume`), so it
//! carries on after the app or this agent restarts.
//!
//! - `GET /code`: sessions, whether the sandbox is set up, the model check.
//! - `POST /code/setup`: build the sandbox image and start the gate.
//! - `POST /code/sessions` `{folder, sandbox, policy}`; `DELETE /code/sessions/{id}`.
//! - `GET /code/sessions/{id}?after=N`: the session and its events after N.
//! - `POST /code/sessions/{id}/message` `{text}`; `/approve` `{request_id, allow, always}`;
//!   `/interrupt`; `/stop`; `/commit` `{message}`; `/discard`.
//! - `GET /code/sessions/{id}/changes`: git status and diff of the project.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::routes::{AppState, BearerAuth};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Bump the tag when DOCKERFILE changes: the next sandbox session builds the new one.
const IMAGE: &str = "tt-code-sandbox:3";
const NETWORK: &str = "tt-code-sandbox";
const GATE: &str = "tt-code-gate";
/// Where Rust builds in the sandbox go: a volume of their own, never the
/// project's `target/` (a sandbox build there would throw away the host's cache).
const CARGO_VOLUME: &str = "tt-code-cargo";
const DOCKERFILE: &str = r#"FROM debian:bookworm-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      git ca-certificates python3 python-is-python3 python3-venv python3-pip python3-pytest ripgrep procps less nano curl make build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd -g 1000 coder && useradd -m -u 1000 -g 1000 -s /bin/bash coder \
    && git config --system safe.directory '*'
USER coder
WORKDIR /work
"#;
/// The gate: forwards the model API (`/v1/…`) to the box and refuses everything else.
/// A client that goes away (Stop) ends the model request too; an upstream that
/// fails mid-reply ends only that reply, never the gate.
const GATE_JS: &str = r#"const http=require("http");const T=new URL(process.env.TARGET);http.createServer((q,s)=>{if(!q.url.startsWith("/v1/")){s.writeHead(403,{"content-type":"text/plain"});return s.end("Only the model API is reachable from the sandbox.");}const p=http.request({host:T.hostname,port:T.port,path:q.url,method:q.method,headers:{...q.headers,host:T.host}},r=>{r.on("error",()=>s.destroy());if(!s.headersSent)s.writeHead(r.statusCode,r.headers);r.pipe(s);});p.on("error",e=>{if(!s.headersSent){s.writeHead(502,{"content-type":"text/plain"});s.end(String(e));}else s.destroy();});s.on("close",()=>p.destroy());q.on("error",()=>p.destroy());q.pipe(p);}).listen(8765,"0.0.0.0");"#;
/// Bump when GATE_JS changes: a gate from another version (or for another port) is replaced.
const GATE_VERSION: &str = "2";

fn root(state: &AppState) -> PathBuf {
    state.config_dir().join("code")
}

fn dir(state: &AppState, id: &str) -> PathBuf {
    root(state).join(id)
}

// ---------------------------------------------------------------- sessions on disk

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub folder: String,
    pub title: String,
    pub created: u64,
    pub sandbox: bool,
    /// ask | edits | auto (auto only in the sandbox)
    pub policy: String,
    #[serde(default)]
    pub claude_session: Option<String>,
    #[serde(default)]
    pub last: u64,
    /// What it may do without asking, decided with "Always": tool names, or `Bash:<first word>`.
    #[serde(default)]
    pub always: Vec<String>,
    /// Let the model think before acting (better on hard tasks, much slower).
    #[serde(default)]
    pub think: bool,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn load(state: &AppState, id: &str) -> Option<Session> {
    std::fs::read(dir(state, id).join("session.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn save(state: &AppState, s: &Session) {
    let _ = std::fs::write(dir(state, &s.id).join("session.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
}

/// Changes a session on disk under one lock: two writers (an approval's "Always"
/// and a turn ending, say) can't each save over the other's change.
fn update(state: &AppState, id: &str, f: impl FnOnce(&mut Session)) -> Option<Session> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _held = LOCK.lock().expect("code");
    let mut s = load(state, id)?;
    f(&mut s);
    save(state, &s);
    Some(s)
}

static SEQ: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Append one event to the session's record.
fn event(state: &AppState, id: &str, mut e: Value) {
    use std::io::Write;
    let path = dir(state, id).join("events.jsonl");
    let mut seqs = SEQ.lock().expect("code");
    let seq = seqs.entry(id.to_string()).or_insert_with(|| std::fs::read_to_string(&path).map(|t| t.lines().count() as u64).unwrap_or(0));
    *seq += 1;
    e["seq"] = json!(*seq);
    e["ts"] = json!(now());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{e}");
    }
}

fn events_after(state: &AppState, id: &str, after: u64) -> Vec<Value> {
    // Event n is line n: skip straight to the cursor (a long session has hundreds
    // of thousands of lines, and this runs about once a second while it works).
    std::fs::read_to_string(dir(state, id).join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .skip(after.saturating_sub(1) as usize)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["seq"].as_u64().unwrap_or(0) > after)
        .collect()
}

// ---------------------------------------------------------------- the running agent

struct Proc {
    /// To Claude Code, through its holder (`code_holder.py`).
    stdin: tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>,
    /// Permission requests waiting for an answer: request id → (tool, input).
    pending: Mutex<HashMap<String, (String, Value)>>,
    busy: std::sync::atomic::AtomicBool,
    container: Option<String>,
    /// The holder's systemd unit: Claude Code lives in it, apart from this agent.
    unit: String,
    /// The session's folder for the holder's state (busy, questions waiting).
    home: PathBuf,
    /// Edits waiting for their result: tool use id → file, relative to the folder.
    edits: Mutex<HashMap<String, String>>,
    /// Files edited since the turn began.
    turn: Mutex<Vec<String>>,
    /// Claude Code said hello (`system/init`): it started, whatever happens next.
    inited: AtomicBool,
    /// Started with thinking on (it reads that once, at start).
    think: bool,
    /// Streamed text and thinking not yet written as events: written in pieces a
    /// few times a second, not one event per token.
    buf: Mutex<Streamed>,
    /// Stop was pressed during this turn.
    interrupted: AtomicBool,
}

#[derive(Default)]
struct Streamed {
    text: String,
    thinking: String,
    at: Option<Instant>,
}

impl Proc {
    /// Working or not, kept on disk too: an agent that restarts mid-turn knows.
    fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
        let _ = std::fs::write(self.home.join("holder.busy"), if busy { "1" } else { "0" });
    }

    fn take_busy(&self) -> bool {
        let was = self.busy.swap(false, Ordering::SeqCst);
        let _ = std::fs::write(self.home.join("holder.busy"), "0");
        was
    }

    /// The questions waiting for the person, kept on disk for the same reason.
    fn save_pending(&self) {
        let pending: HashMap<String, (String, Value)> = self.pending.lock().expect("code").clone();
        let _ = std::fs::write(self.home.join("holder.pending.json"), serde_json::to_vec(&pending).unwrap_or_default());
    }
}

/// The systemd unit that holds a session's Claude Code.
fn holder_unit(id: &str) -> String {
    format!("tt-code-agent-{id}")
}

const HOLDER_PY: &str = include_str!("code_holder.py");

static PROCS: LazyLock<Mutex<HashMap<String, Arc<Proc>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn proc_of(id: &str) -> Option<Arc<Proc>> {
    PROCS.lock().expect("code").get(id).cloned()
}

/// Forgets `p` as the session's process, unless another has taken its place.
fn forget_proc(id: &str, p: &Arc<Proc>) {
    let mut procs = PROCS.lock().expect("code");
    if procs.get(id).is_some_and(|cur| Arc::ptr_eq(cur, p)) {
        procs.remove(id);
    }
}

/// Sessions whose Claude Code is being started right now (busy, for the window).
static STARTING: LazyLock<Mutex<std::collections::HashSet<String>>> = LazyLock::new(|| Mutex::new(Default::default()));

/// One start at a time per session: two messages at once must not start two Claude Codes.
fn start_lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    LOCKS.lock().expect("code").entry(id.to_string()).or_default().clone()
}

/// Claude Code's program, and the package folder it lives in (mounted into the sandbox).
fn claude_bin() -> Option<(PathBuf, PathBuf)> {
    let candidates = [home().join(".local/bin/claude"), PathBuf::from("/usr/local/bin/claude"), PathBuf::from("/usr/bin/claude")];
    let real = candidates.iter().find(|p| p.exists()).and_then(|p| std::fs::canonicalize(p).ok())?;
    let pkg = real.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| real.parent().unwrap_or(Path::new("/")).to_path_buf());
    Some((real, pkg))
}

/// The box's own Node.js (for JavaScript and TypeScript projects), mounted read-only into the sandbox.
fn node_home() -> Option<PathBuf> {
    [home().join(".local/share/node/current"), PathBuf::from("/usr/local/lib/nodejs")]
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .find(|p| p.join("bin/node").is_file())
}

fn docker_ok(args: &[&str]) -> bool {
    std::process::Command::new("docker").args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// (image built, gate running: this version, for this port)
fn sandbox_ready(port: u16) -> (bool, bool) {
    let image = docker_ok(&["image", "inspect", IMAGE]) && docker_ok(&["volume", "inspect", CARGO_VOLUME]);
    let want = format!("true {GATE_VERSION}:{port}");
    let gate = std::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}} {{index .Config.Labels \"tt-station.gate\"}}", GATE])
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == want);
    (image, gate)
}

static SETUP: LazyLock<Mutex<Option<(String, Option<String>)>>> = LazyLock::new(|| Mutex::new(None));
/// One setup at a time, whoever asks (the Set up button, a first message).
static SETTING_UP: AtomicBool = AtomicBool::new(false);

/// Sets the sandbox up, or waits for the setup already under way. A failure is
/// recorded for the Code window either way, and the next try starts afresh.
async fn ensure_sandbox(port: u16) -> Result<(), String> {
    loop {
        if SETTING_UP.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            let r = tokio::task::spawn_blocking(move || setup_sandbox(port)).await.map_err(|e| e.to_string()).and_then(|r| r);
            *SETUP.lock().expect("code") = r.as_ref().err().map(|e| ("Failed".to_string(), Some(e.clone())));
            SETTING_UP.store(false, Ordering::SeqCst);
            return r;
        }
        // Someone else is setting it up: wait for them, then see how it went.
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !SETTING_UP.load(Ordering::SeqCst) {
            if tokio::task::spawn_blocking(move || sandbox_ready(port)).await.is_ok_and(|(i, g)| i && g) {
                return Ok(());
            }
            if let Some((_, Some(e))) = SETUP.lock().expect("code").clone() {
                return Err(e);
            }
        }
    }
}

/// Build the sandbox image (once) and start the gate.
fn setup_sandbox(port: u16) -> Result<(), String> {
    let step = |s: &str| *SETUP.lock().expect("code") = Some((s.to_string(), None));
    let (image, _) = sandbox_ready(port);
    if !image {
        step("Building the sandbox (a one-time download of about 370 MB)");
        let mut child = std::process::Command::new("docker")
            .args(["build", "-q", "-t", IMAGE, "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("docker isn't available: {e}"))?;
        if let Some(mut i) = child.stdin.take() {
            use std::io::Write;
            let _ = i.write_all(DOCKERFILE.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("the build failed").to_string());
        }
    }
    // The Rust build volume, owned by the sandbox's user.
    if !docker_ok(&["volume", "inspect", CARGO_VOLUME]) {
        if !docker_ok(&["volume", "create", CARGO_VOLUME])
            || !docker_ok(&["run", "--rm", "--user", "0", "-v", &format!("{CARGO_VOLUME}:/v"), IMAGE, "chown", "1000:1000", "/v"])
        {
            return Err("couldn't make the sandbox's build volume".into());
        }
    }
    step("Starting the gate");
    // The sandbox's network has no address on the box and no way out: only the gate is on it.
    if !docker_ok(&["network", "inspect", NETWORK]) && !docker_ok(&["network", "create", "--internal", "-o", "com.docker.network.bridge.inhibit_ipv4=true", NETWORK]) {
        return Err("couldn't make the sandbox's network".into());
    }
    let _ = docker_ok(&["network", "create", "tt-apps"]);
    let (_, gate) = sandbox_ready(port);
    if !gate {
        let _ = docker_ok(&["rm", "-f", GATE]);
        let label = format!("tt-station.gate={GATE_VERSION}:{port}");
        let target = format!("TARGET=http://host.docker.internal:{port}");
        if !docker_ok(&["pull", "-q", "node:22-alpine"]) && !docker_ok(&["image", "inspect", "node:22-alpine"]) {
            return Err("couldn't download the gate's image".into());
        }
        let ok = docker_ok(&[
            "run", "-d", "--name", GATE, "--restart", "unless-stopped", "--label", "tt-station.service=code-gate", "--label", &label,
            "--network", "tt-apps", "--add-host", "host.docker.internal:host-gateway", "--read-only", "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--memory", "256m", "--pids-limit", "64", "--user", "node",
            "-e", &target, "node:22-alpine", "node", "-e", GATE_JS,
        ]) && docker_ok(&["network", "connect", "--alias", "box", NETWORK, GATE]);
        if !ok {
            return Err("couldn't start the gate".into());
        }
    }
    *SETUP.lock().expect("code") = None;
    Ok(())
}

/// What the agent is told about where it works (added to Claude Code's own instructions).
fn setting(sandbox: bool) -> String {
    let mut t = String::from("You are working on a Tenstorrent QuietBox through TT Station, on a model the box serves itself. The person reads your progress in TT Station's Code window and approves edits and commands there.\n\
How to work well here:\n\
- Keep to the person's request. Look only at the files you need: find them with `rg` (ripgrep) in Bash, e.g. `rg -n \"function Spark\" src/` or `rg --files | rg chart`, instead of listing or reading whole trees.\n\
- Your context is limited, so read big files in parts: use Read with offset and limit around the lines `rg -n` found, not the whole file. Keep command output short (pipe through `head`, `tail` or `rg`).\n\
- First work out exactly what the request refers to. When it names a part of the app (a page, tab, screen, command), search for it by name (`rg -n -i \"code\" --files-with-matches`, `rg --files | rg -i page`) and go with the most specific match; say which files you chose and why before changing anything. If two parts could fit and the choice matters, ask instead of guessing.\n\
- Make focused edits with the Edit tool. Never replace an existing file with Write: Write is only for new files.\n\
- After changing code, check it: run the project's tests, type checker or build if there is one, and fix what fails. If you can't run the check (the tool isn't available here), say so plainly in your summary: never say a change works when you couldn't check it.\n\
- When you're done, say in a few sentences what you changed and how you checked it. Don't create notes, summaries or memory files (MEMORY.md, SUMMARY.md, CHANGES.md…) unless asked: the person reads your summary here.\n");
    if sandbox {
        t.push_str("You are in a sandbox: the project is at /work and it is the only folder you can see; there is no internet access (no package installs, no git push); the project's .git is read-only (git status, diff and log work; don't commit, the person commits from the Changes view); python3, pip (offline), pytest, git, ripgrep, make, curl, Node.js with npm and npx (offline: they run what's already in node_modules, e.g. `npx tsc --noEmit`), and Rust with cargo (offline, with the crates the box already has: `cargo check`, `cargo test`, `cargo clippy`) are available. If something needs the network, say so instead of retrying.\n");
    }
    t
}

/// Whether a project has Jupyter notebooks (a quick look, skipping the usual bulk).
fn has_notebooks(root: &Path) -> bool {
    let mut stack = vec![(root.to_path_buf(), 0)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            seen += 1;
            if seen > 5000 {
                return false;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".ipynb") {
                return true;
            }
            if depth < 4 && !name.starts_with('.') && !["node_modules", "target", "venv", ".venv", "dist", "build"].contains(&name.as_ref()) && e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((e.path(), depth + 1));
            }
        }
    }
    false
}

fn mode(policy: &str, sandbox: bool) -> &'static str {
    match (policy, sandbox) {
        ("edits", _) => "acceptEdits",
        ("auto", true) => "bypassPermissions",
        _ => "manual",
    }
}

/// Start Claude Code for a session (resuming its conversation if it had one).
async fn spawn(state: &AppState, s: &Session) -> Result<Arc<Proc>, String> {
    let (bin, pkg) = claude_bin().ok_or("Code's agent engine isn't installed on the box (npm i -g @anthropic-ai/claude-code).")?;
    let key = tokio::task::spawn_blocking({ let st = state.clone(); move || crate::coding::key(&st) }).await.map_err(|e| e.to_string())??;
    let cfg = dir(state, &s.id).join("claude");
    std::fs::create_dir_all(&cfg).map_err(|e| e.to_string())?;
    let cj = cfg.join(".claude.json");
    if !cj.exists() {
        let _ = std::fs::write(&cj, r#"{"hasCompletedOnboarding": true}"#);
    }
    // Claude Code compacts its history before this: smaller contexts keep each step fast
    // (the whole conversation is read again at every step).
    // The whole conversation is read again at every step, so a smaller window keeps
    // steps fast; too small, and a real task fills it faster than it can summarize
    // (Claude Code stops: "Autocompact is thrashing").
    let context = crate::coding::check(state).await["context"].as_u64().unwrap_or(131_072).min(131_072);
    // Its answers fit in what's left of the context: Claude Code asks for large
    // output budgets by default, and prompt plus budget past the context is refused.
    let output = (context / 4).clamp(4096, 16_384);
    let mut args: Vec<String> = ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages",
        "--permission-prompt-tool", "stdio", "--permission-mode", mode(&s.policy, s.sandbox)].iter().map(|a| a.to_string()).collect();
    args.push("--append-system-prompt".into());
    args.push(setting(s.sandbox));
    // Only the tools coding needs here. Claude Code offers about twenty (agents,
    // schedules, worktrees, messages, plan mode…) that can't help in this window,
    // and every one is described in the prompt the model re-reads at each step.
    // No web from the sandbox: it has no internet.
    let mut tools = vec!["Bash", "Read", "Edit", "Write"];
    // Its description costs ~450 tokens at every step: only for projects with notebooks.
    if has_notebooks(Path::new(&s.folder)) {
        tools.push("NotebookEdit");
    }
    if !s.sandbox {
        tools.extend(["WebFetch", "WebSearch"]);
    }
    args.push("--tools".into());
    args.push(tools.join(","));
    if let Some(c) = &s.claude_session {
        args.push("--resume".into());
        args.push(c.clone());
    }
    let envs: Vec<(String, String)> = vec![
        ("ANTHROPIC_AUTH_TOKEN".into(), key),
        ("ANTHROPIC_API_KEY".into(), String::new()),
        ("ANTHROPIC_MODEL".into(), "quietbox".into()),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL".into(), "quietbox".into()),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL".into(), "quietbox".into()),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL".into(), "quietbox".into()),
        ("CLAUDE_CODE_MAX_CONTEXT_TOKENS".into(), context.to_string()),
        // Summarize long before the window is full: past ~64K tokens a local model
        // starts inventing tools and writing its calls as text.
        ("CLAUDE_CODE_AUTO_COMPACT_WINDOW".into(), context.min(crate::coding::COMPACT_WINDOW).to_string()),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), output.to_string()),
        // No single file read or command output may take a big bite of the context:
        // a large file is read in parts (offset, limit) instead.
        ("CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS".into(), "8000".into()),
        ("BASH_MAX_OUTPUT_LENGTH".into(), "15000".into()),
        // So the box knows which session a model request is for (its activity line).
        ("ANTHROPIC_CUSTOM_HEADERS".into(), format!("x-tt-session: {}", s.id)),
        ("DISABLE_TELEMETRY".into(), "1".into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(), "1".into()),
        ("DISABLE_AUTOUPDATER".into(), "1".into()),
        ("MAX_THINKING_TOKENS".into(), if s.think { "6000".into() } else { "0".into() }),
        // No per-request header line at the top of the system prompt: it changes every
        // request, so the server could never reuse (prefix-cache) the conversation's start.
        ("CLAUDE_CODE_ATTRIBUTION_HEADER".into(), "0".into()),
        // No __pycache__ folders showing up as "changes" after every test run.
        ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
    ];
    let container = format!("tt-code-{}", s.id);
    let port = state.ctrl_port();
    let cmd = if s.sandbox {
        let (image, gate) = tokio::task::spawn_blocking(move || sandbox_ready(port)).await.unwrap_or((false, false));
        if !image || !gate {
            ensure_sandbox(port).await?;
        }
        let _ = tokio::process::Command::new("docker").args(["rm", "-f", &container]).output().await;
        let inner_bin = format!("/opt/claude-code/{}", bin.strip_prefix(&pkg).map(|p| p.display().to_string()).unwrap_or_else(|_| "claude".into()));
        let mut c = tokio::process::Command::new("docker");
        c.args(["run", "-i", "--rm", "--name", &container, "--label", "tt-station.service=code",
            "--network", NETWORK, "--read-only", "--tmpfs", "/tmp:size=2g", "--tmpfs", "/home/coder:uid=1000,gid=1000,size=1g",
            "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "512", "--memory", "8g", "--cpus", "8",
            "-v", &format!("{}:/work", s.folder),
            "-v", &format!("{}:/opt/claude-code:ro", pkg.display()),
            "-v", &format!("{}:/home/coder/.tt-claude", cfg.display()),
            "-e", "CLAUDE_CONFIG_DIR=/home/coder/.tt-claude", "-e", "ANTHROPIC_BASE_URL=http://box:8765", "-e", "HOME=/home/coder"]);
        // The project's git directories go in read-only: the model can read
        // history, but can't change how git behaves when the box runs it
        // (crate::safegit). Commits are made from Changes, outside.
        let read_only = git_dirs(Path::new(&s.folder));
        for (rel, _) in &read_only {
            c.arg("-v").arg(format!("{}:/work/{rel}:ro", Path::new(&s.folder).join(rel).display()));
        }
        let ro: Vec<PathBuf> = read_only.into_iter().filter_map(|(_, d)| d).collect();
        std::fs::write(dir(state, &s.id).join(GIT_RO), serde_json::to_vec(&ro).unwrap_or_default()).map_err(|e| e.to_string())?;
        // The box's Rust toolchain and downloaded crates, read-only, offline; builds
        // go to their own volume.
        let (rustup, cargo) = (home().join(".rustup"), home().join(".cargo"));
        if rustup.is_dir() && cargo.join("bin").is_dir() {
            c.args(["-v", &format!("{}:/opt/rustup:ro", rustup.display()), "-v", &format!("{}:/opt/cargo-bin:ro", cargo.join("bin").display())]);
            if cargo.join("registry").is_dir() {
                c.args(["-v", &format!("{}:/home/coder/.cargo/registry:ro", cargo.join("registry").display())]);
            }
            c.args(["-v", &format!("{CARGO_VOLUME}:/cargo-target"),
                "-e", "RUSTUP_HOME=/opt/rustup", "-e", "CARGO_HOME=/home/coder/.cargo", "-e", "CARGO_TARGET_DIR=/cargo-target", "-e", "CARGO_NET_OFFLINE=true",
                "-e", "PATH=/opt/cargo-bin:/usr/local/bin:/usr/bin:/bin"]);
        }
        if let Some(node) = node_home() {
            c.args(["-v", &format!("{}:/opt/node:ro", node.display()),
                "-e", "PATH=/opt/cargo-bin:/opt/node/bin:/usr/local/bin:/usr/bin:/bin", "-e", "npm_config_cache=/tmp/npm-cache", "-e", "npm_config_offline=true"]);
        }
        // Names only: docker takes each value from its own environment (the
        // holder's spec), so the model key never shows in a process list.
        for (k, v) in &envs {
            c.arg("-e").arg(k).env(k, v);
        }
        c.arg(IMAGE).arg(inner_bin).args(&args);
        c
    } else {
        let mut c = tokio::process::Command::new(&bin);
        c.args(&args)
            .current_dir(&s.folder)
            .env("CLAUDE_CONFIG_DIR", &cfg)
            .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", state.ctrl_port()));
        for (k, v) in &envs {
            c.env(k, v);
        }
        c
    };
    // Claude Code runs in a holder of its own (a systemd unit), not as this agent's
    // child: restarting the agent no longer ends a turn, or a question waiting
    // for the person. The holder is told what to run in a spec file it deletes.
    let home = dir(state, &s.id);
    let unit = holder_unit(&s.id);
    let std_cmd = cmd.as_std();
    let argv: Vec<String> = std::iter::once(std_cmd.get_program()).chain(std_cmd.get_args()).map(|a| a.to_string_lossy().to_string()).collect();
    let env: HashMap<String, String> = std_cmd.get_envs().filter_map(|(k, v)| Some((k.to_string_lossy().to_string(), v?.to_string_lossy().to_string()))).collect();
    let cwd = std_cmd.get_current_dir().map(|d| d.display().to_string());
    let socket = home.join("holder.sock");
    let spec = json!({
        "argv": argv, "env": env, "cwd": cwd, "socket": socket, "journal": home.join("holder.journal"), "stderr": home.join("stderr.log"),
    });
    let spec_path = home.join("holder.spec.json");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&spec_path).map_err(|e| e.to_string())?;
        f.write_all(spec.to_string().as_bytes()).map_err(|e| e.to_string())?;
    }
    let script = root(state).join("code_holder.py");
    std::fs::write(&script, HOLDER_PY).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&socket);
    for f in ["holder.busy", "holder.pending.json", "holder.seen"] {
        let _ = std::fs::remove_file(home.join(f));
    }
    let _ = tokio::process::Command::new("systemctl").args(["--user", "stop", &unit]).output().await;
    let path = format!("{}:{}", home_dir_bin(), std::env::var("PATH").unwrap_or_default());
    let out = tokio::process::Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet", &format!("--unit={unit}"), &format!("--setenv=PATH={path}"), "python3"])
        .arg(&script)
        .arg(&spec_path)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("couldn't start the agent: {e}"))?;
    if !out.status.success() {
        return Err(format!("couldn't start the agent: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    attach(state, &s.id, unit, s.sandbox.then(|| container.clone()), s.think, 0, false, HashMap::new()).await
}

fn home_dir_bin() -> String {
    home().join(".local/bin").display().to_string()
}

/// Connects to a session's holder and reads Claude Code from line `seen` on.
#[allow(clippy::too_many_arguments)]
async fn attach(state: &AppState, id: &str, unit: String, container: Option<String>, think: bool, seen: u64, busy: bool,
    pending: HashMap<String, (String, Value)>) -> Result<Arc<Proc>, String> {
    let home = dir(state, id);
    let stream = tokio::net::UnixStream::connect(home.join("holder.sock")).await.map_err(|e| format!("couldn't reach the agent's holder: {e}"))?;
    let (read, mut write) = stream.into_split();
    write.write_all(format!("ATTACH {seen}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let proc = Arc::new(Proc {
        stdin: tokio::sync::Mutex::new(write),
        pending: Mutex::new(pending),
        busy: AtomicBool::new(busy),
        container,
        unit,
        home: home.clone(),
        edits: Mutex::new(HashMap::new()),
        turn: Mutex::new(Vec::new()),
        // Picked up mid-session: it had started.
        inited: AtomicBool::new(seen > 0),
        think,
        buf: Mutex::new(Streamed::default()),
        interrupted: AtomicBool::new(false),
    });
    PROCS.lock().expect("code").insert(id.to_string(), proc.clone());
    let st = state.clone();
    let id = id.to_string();
    let p = proc.clone();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(read).lines();
        let mut n = seen;
        while let Ok(Some(line)) = lines.next_line().await {
            n += 1;
            if line.starts_with("#EXIT") {
                break;
            }
            if let Ok(m) = serde_json::from_str::<Value>(&line) {
                handle(&st, &id, &p, m).await;
            }
            // How far this agent has read: a restarted one picks up from here.
            let _ = std::fs::write(p.home.join("holder.seen"), n.to_string());
        }
        // The holder ended (Claude Code exited), or went away.
        flush(&st, &id, &p);
        forget_proc(&id, &p);
        expire_pending(&st, &id, &p);
        if !p.inited.load(Ordering::SeqCst) {
            // It never started: say why (from what it printed), not "send a message to carry on".
            let why = std::fs::read_to_string(dir(&st, &id).join("stderr.log")).unwrap_or_default();
            let why = why.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it exited before saying anything").to_string();
            if why.contains("No conversation found") {
                update(&st, &id, |s| s.claude_session = None);
                event(&st, &id, json!({ "kind": "error", "text": "Its earlier conversation couldn't be found, so the next message starts a fresh one in this project." }));
            } else {
                event(&st, &id, json!({ "kind": "error", "text": format!("The agent didn't start: {why}") }));
            }
            p.set_busy(false);
        } else if p.take_busy() {
            event(&st, &id, json!({ "kind": "error", "text": "The agent stopped in the middle of the turn. Send a message to carry on." }));
        }
        event(&st, &id, json!({ "kind": "idle" }));
    });
    Ok(proc)
}

/// Writes what's streamed so far as events.
fn flush(state: &AppState, id: &str, p: &Proc) {
    let (text, thinking) = {
        let mut b = p.buf.lock().expect("code");
        b.at = Some(Instant::now());
        (std::mem::take(&mut b.text), std::mem::take(&mut b.thinking))
    };
    if !thinking.is_empty() {
        event(state, id, json!({ "kind": "thinking", "text": thinking }));
    }
    if !text.is_empty() {
        event(state, id, json!({ "kind": "delta", "text": text }));
    }
}

/// Streamed text or thinking: kept, and written a few times a second.
fn stream(state: &AppState, id: &str, p: &Proc, text: &str, thinking: bool) {
    let due = {
        let mut b = p.buf.lock().expect("code");
        if thinking { b.thinking.push_str(text) } else { b.text.push_str(text) }
        b.at.is_none_or(|t| t.elapsed() >= Duration::from_millis(150))
    };
    if due {
        flush(state, id, p);
    }
}

/// Requests nobody can answer any more (the turn ended, it was stopped, Claude
/// Code withdrew them or exited): no longer waiting, in the window and the list.
fn expire_pending(state: &AppState, id: &str, p: &Proc) {
    let gone: Vec<String> = p.pending.lock().expect("code").drain().map(|(k, _)| k).collect();
    p.save_pending();
    for rid in gone {
        event(state, id, json!({ "kind": "decided", "request_id": rid, "allow": false, "always": false, "expired": true }));
    }
}

/// Starts Claude Code for the session unless it's running, one start at a time;
/// restarts it first if its thinking setting has changed since it started.
async fn running(state: &AppState, id: &str) -> Result<Arc<Proc>, String> {
    let lock = start_lock(id);
    let _held = lock.lock().await;
    let s = load(state, id).ok_or("That session is gone.")?;
    if let Some(p) = proc_of(id) {
        if p.think == s.think || p.busy.load(Ordering::SeqCst) {
            return Ok(p);
        }
        // Thinking changed while it was working: it takes effect now, between turns.
        let q = p.clone();
        tokio::task::spawn_blocking(move || kill(&q)).await.ok();
        forget_proc(id, &p);
    }
    STARTING.lock().expect("code").insert(id.to_string());
    let r = spawn(state, &s).await;
    STARTING.lock().expect("code").remove(id);
    r
}

async fn send(p: &Proc, v: &Value) -> Result<(), String> {
    let mut w = p.stdin.lock().await;
    w.write_all(format!("{v}\n").as_bytes()).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}

/// Shell syntax that runs more than the command it starts with (a chain, a pipe,
/// a substitution, a redirect): such a command is only ever allowed exactly.
fn compound(cmd: &str) -> bool {
    ["&&", "||", ";", "|", "$(", "`", ">", "<", "\n", "&"].iter().any(|t| cmd.contains(t))
}

/// Commands whose name alone is safe to always allow: they read, list or test.
/// (Not `find`: `-delete` and `-exec` change things.)
const READ_ONLY: &[&str] = &["ls", "cat", "head", "tail", "wc", "grep", "rg", "tree", "pwd", "echo", "pytest", "diff", "stat", "file", "which", "du", "sort", "uniq", "cut", "nl"];
/// Tools whose first word means nothing alone: always allowed by their first two
/// words (`npm test`, `cargo build`, `git status`), or three for `python3 -m pytest`.
const BY_SUBCOMMAND: &[&str] = &["npm", "npx", "cargo", "make", "go", "git", "uv", "pnpm", "yarn"];

/// What "Always" remembers for a tool use, or None when it can't be remembered
/// safely. Edits are one rule. A command is remembered by its name when that's
/// safe (`Bash:pytest`), by its subcommand for build tools (`Bash:npm test`),
/// and otherwise exactly (`Bash=python3 build.py`); a chained or redirected
/// command only ever exactly.
fn always_rule(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Some("edits".into()),
        "Bash" => {
            let cmd = input["command"].as_str().unwrap_or("").trim();
            if cmd.is_empty() {
                return None;
            }
            let words: Vec<&str> = cmd.split_whitespace().collect();
            if compound(cmd) {
                return Some(format!("Bash={cmd}"));
            }
            if READ_ONLY.contains(&words[0]) {
                return Some(format!("Bash:{}", words[0]));
            }
            if words.len() >= 3 && words[0].starts_with("python") && words[1] == "-m" {
                return Some(format!("Bash:{} -m {}", words[0], words[2]));
            }
            if words.len() >= 2 && BY_SUBCOMMAND.contains(&words[0]) && !words[1].starts_with('-') {
                return Some(format!("Bash:{} {}", words[0], words[1]));
            }
            Some(format!("Bash={cmd}"))
        }
        other => Some(other.to_string()),
    }
}

/// Does one of the session's "Always" rules cover this tool use?
fn allowed_by(rules: &[String], tool: &str, input: &Value) -> bool {
    match tool {
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => rules.iter().any(|r| r == "edits" || r == tool),
        "Bash" => {
            let cmd = input["command"].as_str().unwrap_or("").trim();
            rules.iter().any(|r| {
                if let Some(exact) = r.strip_prefix("Bash=") {
                    return exact == cmd;
                }
                // A prefix rule never covers a chained or redirected command.
                let Some(prefix) = r.strip_prefix("Bash:") else { return false };
                !compound(cmd) && !prefix.is_empty() && (cmd == prefix || cmd.starts_with(&format!("{prefix} ")))
            })
        }
        other => rules.iter().any(|r| r == other),
    }
}

/// The file an editing tool call changes, relative to the session's folder.
fn edited_file(s: &Session, tool: &str, input: &Value) -> Option<String> {
    if !matches!(tool, "Edit" | "MultiEdit" | "Write" | "NotebookEdit") {
        return None;
    }
    let path = input["file_path"].as_str().or_else(|| input["notebook_path"].as_str())?;
    let root = if s.sandbox { "/work/".to_string() } else { format!("{}/", s.folder.trim_end_matches('/')) };
    let rel = path.strip_prefix(&root)?.trim_start_matches('/');
    (!rel.is_empty() && !rel.split('/').any(|c| c == "..")).then(|| rel.to_string())
}

fn short_tool(name: &str, input: &Value) -> String {
    let file = |k: &str| input[k].as_str().map(|p| p.trim_start_matches("/work/").to_string()).unwrap_or_default();
    match name {
        "Bash" => format!("Run `{}`", input["command"].as_str().unwrap_or("")),
        "Edit" | "MultiEdit" => format!("Edit {}", file("file_path")),
        "Write" => format!("Write {}", file("file_path")),
        "Read" => format!("Read {}", file("file_path")),
        other => other.to_string(),
    }
}

/// One line from Claude Code.
async fn handle(state: &AppState, id: &str, p: &Arc<Proc>, m: Value) {
    // Anything other than streamed text: write the streamed text first, so
    // events stay in the order they happened.
    let streamed = m["type"] == "stream_event"
        && m["event"]["type"] == "content_block_delta"
        && matches!(m["event"]["delta"]["type"].as_str(), Some("text_delta" | "thinking_delta"));
    if !streamed {
        flush(state, id, p);
    }
    match m["type"].as_str() {
        Some("system") if m["subtype"] == "init" => {
            p.inited.store(true, Ordering::SeqCst);
            if let Some(cs) = m["session_id"].as_str() {
                if load(state, id).is_some_and(|s| s.claude_session.as_deref() != Some(cs)) {
                    update(state, id, |s| s.claude_session = Some(cs.to_string()));
                }
            }
        }
        Some("system") if m["subtype"] == "api_retry" => {
            let (n, of) = (m["attempt"].as_u64().unwrap_or(0), m["max_retries"].as_u64().unwrap_or(0));
            let why = m["error_status"].as_u64().map(|c| format!(" (it answered {c})")).unwrap_or_default();
            event(state, id, json!({ "kind": "notice", "text": format!("The model isn't answering{why}; trying again ({n} of {of})…") }));
        }
        Some("system") if m["subtype"] == "compact_boundary" => {
            event(state, id, json!({ "kind": "notice", "text": "The conversation got long, so it summarized what came before to make room." }));
        }
        Some("system") if m["subtype"] == "permission_denied" => {
            event(state, id, json!({ "kind": "notice", "text": format!("Not allowed: {}", m["tool_name"].as_str().unwrap_or("a tool")) }));
        }
        Some("stream_event") => {
            let e = &m["event"];
            if e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta" {
                if let Some(t) = e["delta"]["text"].as_str() {
                    stream(state, id, p, t, false);
                }
            }
            if e["type"] == "content_block_delta" && e["delta"]["type"] == "thinking_delta" {
                if let Some(t) = e["delta"]["thinking"].as_str() {
                    stream(state, id, p, t, true);
                }
            }
            if e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use" {
                event(state, id, json!({ "kind": "preparing", "name": e["content_block"]["name"] }));
            }
            if e["type"] == "message_start" {
                // Busy whenever Claude Code is working, however the turn began (a queued
                // message, a resumed turn), not only when this window sent one.
                p.set_busy(true);
                event(state, id, json!({ "kind": "step" }));
            }
        }
        Some("assistant") => {
            for c in m["message"]["content"].as_array().into_iter().flatten() {
                match c["type"].as_str() {
                    Some("text") if !c["text"].as_str().unwrap_or("").trim().is_empty() => event(state, id, json!({ "kind": "text", "text": c["text"] })),
                    Some("tool_use") => {
                        if let (Some(tid), Some(file)) = (c["id"].as_str(), load(state, id).and_then(|s| edited_file(&s, c["name"].as_str().unwrap_or(""), &c["input"]))) {
                            p.edits.lock().expect("code").insert(tid.to_string(), file);
                        }
                        event(state, id, json!({ "kind": "tool", "id": c["id"], "name": c["name"], "input": c["input"] }))
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            for c in m["message"]["content"].as_array().into_iter().flatten() {
                if c["type"] == "tool_result" {
                    let text = match &c["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                        _ => String::new(),
                    };
                    let text: String = if text.len() > 20_000 { format!("{}\n[…cut]", text.chars().take(20_000).collect::<String>()) } else { text };
                    let file = c["tool_use_id"].as_str().and_then(|t| p.edits.lock().expect("code").remove(t));
                    if let Some(file) = file.filter(|_| c["is_error"] != true) {
                        let mut turn = p.turn.lock().expect("code");
                        if !turn.contains(&file) {
                            turn.push(file);
                        }
                    }
                    event(state, id, json!({ "kind": "tool_result", "id": c["tool_use_id"], "output": text, "is_error": c["is_error"] == true }));
                }
            }
        }
        Some("control_request") if m["request"]["subtype"] == "can_use_tool" => {
            let rid = m["request_id"].as_str().unwrap_or("").to_string();
            let tool = m["request"]["tool_name"].as_str().unwrap_or("").to_string();
            let input = m["request"]["input"].clone();
            let auto = load(state, id).is_some_and(|s| allowed_by(&s.always, &tool, &input));
            if auto {
                let _ = send(p, &json!({ "type": "control_response", "response": { "subtype": "success", "request_id": rid, "response": { "behavior": "allow", "updatedInput": input } } })).await;
                return;
            }
            p.pending.lock().expect("code").insert(rid.clone(), (tool.clone(), input.clone()));
            p.save_pending();
            event(state, id, json!({ "kind": "approval", "request_id": rid, "tool": tool, "input": input, "summary": short_tool(&tool, &input),
                "rule": always_rule(&tool, &input) }));
            let folder = load(state, id).map(|s| s.title).unwrap_or_default();
            let approval = json!({ "approval": { "id": format!("code:{id}:{rid}"), "kind": "code", "session": id, "request_id": rid, "summary": short_tool(&tool, &input), "can_always": always_rule(&tool, &input).is_some() } });
            state.notifier().emit_with(crate::notify::Draft { kind: "coding", title: format!("TT Station needs your OK in {folder}"), body: short_tool(&tool, &input) }, Some(approval));
        }
        // Claude Code withdrew a question (the turn moved on): nobody needs to answer it.
        Some("control_cancel_request") => {
            if let Some(rid) = m["request_id"].as_str() {
                let gone = p.pending.lock().expect("code").remove(rid).is_some();
                p.save_pending();
                if gone {
                    event(state, id, json!({ "kind": "decided", "request_id": rid, "allow": false, "always": false, "expired": true }));
                }
            }
        }
        // Anything else it asks of a host (hooks, SDK tools): not offered here, and
        // it must hear so rather than wait forever.
        Some("control_request") => {
            if let Some(rid) = m["request_id"].as_str() {
                let _ = send(p, &json!({ "type": "control_response", "response": { "subtype": "error", "request_id": rid,
                    "error": format!("TT Station doesn't support {}", m["request"]["subtype"].as_str().unwrap_or("that")) } })).await;
            }
        }
        Some("result") => {
            p.set_busy(false);
            expire_pending(state, id, p);
            let files = std::mem::take(&mut *p.turn.lock().expect("code"));
            let ok = m["subtype"] == "success" && m["is_error"] != true;
            let stopped = p.interrupted.swap(false, Ordering::SeqCst);
            // Why it didn't finish, in words: its message, its errors, or what kind of end it was.
            let why = m["result"].as_str().filter(|t| !t.trim().is_empty()).map(str::to_string)
                .or_else(|| m["errors"].as_array().map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).filter(|t| !t.is_empty()))
                .unwrap_or_else(|| match m["subtype"].as_str() {
                    Some("error_max_turns") => "It reached the most steps one request may take. Send \"continue\" to carry on.".into(),
                    Some("error_during_execution") if stopped => "Stopped.".into(),
                    Some("error_during_execution") => "Something went wrong while it was working.".into(),
                    _ => String::new(),
                });
            event(state, id, json!({ "kind": "done", "ok": ok, "stopped": stopped && !ok,
                "duration_ms": m["duration_ms"], "turns": m["num_turns"], "files": files,
                "input_tokens": m["usage"]["input_tokens"], "output_tokens": m["usage"]["output_tokens"], "text": why,
                "denied": m["permission_denials"].as_array().map_or(0, Vec::len) }));
            if let Some(s) = update(state, id, |s| s.last = now()) {
                // What this turn edited; failing that (a change made with a shell
                // command), whatever git sees as changed.
                let n = if files.is_empty() { crate::safegit::changed_files(Path::new(&s.folder), reach(state, &s).as_ref()).len() } else { files.len() };
                let body = match files.as_slice() {
                    [one] => format!("Edited {one}. Review it in TT Station → Code."),
                    [] if n > 0 => format!("{n} changed file{} to review in TT Station → Code.", if n == 1 { "" } else { "s" }),
                    [] => "Done; no files changed.".into(),
                    many => format!("Edited {} files. Review them in TT Station → Code.", many.len()),
                };
                state.notifier().emit(crate::notify::Draft { kind: "coding", title: format!("TT Station finished in {}", s.title), body });
            }
        }
        _ => {}
    }
}

/// At startup: sessions whose Claude Code is still running in its holder are
/// picked up where this agent left off (a turn in progress carries on, a
/// question still waits). Sandboxes with no holder are leftovers, removed.
pub async fn resume(state: AppState) {
    let mut kept: Vec<String> = Vec::new();
    for e in std::fs::read_dir(root(&state)).into_iter().flatten().flatten() {
        let id = e.file_name().to_string_lossy().to_string();
        let Some(s) = load(&state, &id) else { continue };
        let unit = holder_unit(&id);
        let live = tokio::process::Command::new("systemctl").args(["--user", "is-active", "--quiet", &unit]).status().await.is_ok_and(|x| x.success());
        if !live || !e.path().join("holder.sock").exists() {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        let seen: u64 = read("holder.seen").trim().parse().unwrap_or(0);
        let busy = read("holder.busy").trim() == "1";
        let pending: HashMap<String, (String, Value)> = serde_json::from_str(&read("holder.pending.json")).unwrap_or_default();
        let container = s.sandbox.then(|| format!("tt-code-{id}"));
        match attach(&state, &id, unit, container.clone(), s.think, seen, busy, pending).await {
            Ok(_) => {
                eprintln!("tt-station-agentd: code: picked session {id} back up (line {seen}{})", if busy { ", mid-turn" } else { "" });
                kept.extend(container);
            }
            Err(err) => eprintln!("tt-station-agentd: code: couldn't pick session {id} back up: {err}"),
        }
    }
    let ids = tokio::process::Command::new("docker")
        .args(["ps", "-a", "--filter", "label=tt-station.service=code", "--format", "{{.Names}}"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    let orphans: Vec<String> = ids.into_iter().filter(|n| !kept.contains(n)).collect();
    if !orphans.is_empty() {
        let _ = tokio::process::Command::new("docker").arg("rm").arg("-f").args(&orphans).output().await;
    }
}

// ---------------------------------------------------------------- routes

/// Whether any Code session is in the middle of a turn.
pub(crate) fn any_busy() -> bool {
    PROCS.lock().expect("code").values().any(|p| p.busy.load(Ordering::SeqCst))
}

/// Every question a Code session is waiting on the person for (`GET /approvals`).
pub(crate) fn pending_approvals(state: &AppState) -> Vec<Value> {
    let procs: Vec<(String, Arc<Proc>)> = PROCS.lock().expect("code").iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut out = Vec::new();
    for (id, p) in procs {
        let title = load(state, &id).map(|s| s.title).unwrap_or_default();
        for (rid, (tool, input)) in p.pending.lock().expect("code").iter() {
            out.push(json!({
                "id": format!("code:{id}:{rid}"), "kind": "code", "session": id, "request_id": rid, "where": title,
                "summary": short_tool(tool, input), "tool": tool, "input": input, "can_always": always_rule(tool, input).is_some(),
            }));
        }
    }
    out
}

fn describe(state: &AppState, s: &Session) -> Value {
    let p = proc_of(&s.id);
    json!({
        "id": s.id, "folder": s.folder, "title": s.title, "created": s.created, "last": s.last, "sandbox": s.sandbox, "policy": s.policy,
        "running": p.is_some(),
        "busy": p.as_ref().is_some_and(|p| p.busy.load(Ordering::SeqCst)) || STARTING.lock().expect("code").contains(&s.id),
        "waiting": p.as_ref().map_or(0, |p| p.pending.lock().expect("code").len()),
        "changes": crate::safegit::changed_files(Path::new(&s.folder), reach(state, s).as_ref()).len(),
        "always": s.always, "think": s.think,
    })
}

/// `GET /code`.
pub(crate) async fn get_code(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let check = crate::coding::check(&state).await;
    let st = state.clone();
    let port = state.ctrl_port();
    let (sessions, sandbox) = tokio::task::spawn_blocking(move || {
        let mut v: Vec<Session> = std::fs::read_dir(root(&st)).into_iter().flatten().flatten()
            .filter_map(|e| load(&st, &e.file_name().to_string_lossy())).collect();
        v.sort_by(|a, b| b.last.max(b.created).cmp(&a.last.max(a.created)));
        let (image, gate) = sandbox_ready(port);
        (v.iter().map(|s| describe(&st, s)).collect::<Vec<_>>(), json!({ "image": image, "gate": gate }))
    }).await.unwrap_or_default();
    let setup = SETUP.lock().expect("code").clone();
    Json(json!({
        "sessions": sessions, "check": check, "claude": claude_bin().is_some(), "home": home(),
        "sandbox": { "ready": sandbox["image"] == true && sandbox["gate"] == true, "image": sandbox["image"], "gate": sandbox["gate"],
                     "step": setup.as_ref().map(|(s, _)| s.clone()), "error": setup.and_then(|(_, e)| e) },
    }))
}

/// `POST /code/setup`.
pub(crate) async fn post_setup(State(state): State<AppState>, _auth: BearerAuth) -> Reply {
    if !SETTING_UP.load(Ordering::SeqCst) {
        *SETUP.lock().expect("code") = Some(("Starting".into(), None));
        let port = state.ctrl_port();
        tokio::spawn(async move {
            let _ = ensure_sandbox(port).await;
        });
    }
    Ok(Json(json!({ "setting_up": true })))
}

#[derive(Debug, Deserialize)]
pub struct NewSession {
    pub folder: String,
    #[serde(default = "yes")]
    pub sandbox: bool,
    #[serde(default)]
    pub policy: Option<String>,
    #[serde(default)]
    pub think: bool,
}

/// Where a sandboxed session's launch noted the git directories it mounted read-only.
const GIT_RO: &str = "git-ro.json";

/// The project's `.git` (directory or file), or each of its projects' when it's
/// a folder of projects: relative path, and the directory's real path when it is one.
fn git_dirs(folder: &Path) -> Vec<(String, Option<PathBuf>)> {
    let mut v: Vec<String> = projects_inside(folder).into_iter().map(|p| format!("{p}/.git")).collect();
    if folder.join(".git").exists() {
        v = vec![".git".into()];
    }
    // Can't be mounted (crate::coding::allowed_folder).
    v.retain(|r| !r.contains([':', ',', '\n']));
    v.into_iter().map(|r| {
        let p = folder.join(&r);
        let d = p.is_dir().then(|| std::fs::canonicalize(&p).ok()).flatten();
        (r, d)
    }).collect()
}

/// What a sandboxed session's model can write, for the git run here (crate::safegit).
fn reach(state: &AppState, s: &Session) -> Option<crate::safegit::Reach> {
    s.sandbox.then(|| crate::safegit::Reach {
        folder: PathBuf::from(&s.folder),
        read_only: std::fs::read(dir(state, &s.id).join(GIT_RO)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default(),
    })
}

/// Git repositories directly inside `folder`, when it isn't one itself (a folder of projects).
fn projects_inside(folder: &Path) -> Vec<String> {
    if folder.join(".git").exists() {
        return vec![];
    }
    let mut v: Vec<String> = std::fs::read_dir(folder).into_iter().flatten().flatten()
        .filter(|e| e.path().join(".git").exists())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

fn yes() -> bool {
    true
}

/// `POST /code/sessions`.
pub(crate) async fn post_session(State(state): State<AppState>, _auth: BearerAuth, Json(b): Json<NewSession>) -> Reply {
    let folder = crate::coding::allowed_folder(&b.folder).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let policy = b.policy.unwrap_or_else(|| "ask".into());
    if !["ask", "edits", "auto"].contains(&policy.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "Pick how much it may do without asking."));
    }
    if policy == "auto" && !b.sandbox {
        return Err(err(StatusCode::BAD_REQUEST, "\"Don't ask\" is only for the sandbox: on the box itself it always asks before commands."));
    }
    let inside = projects_inside(&folder);
    if inside.len() > 1 {
        return Err((StatusCode::CONFLICT, Json(json!({
            "error": format!("That folder holds {} projects ({}). Pick one of them: the agent works much better (and faster) in one project.", inside.len(), inside.iter().take(5).cloned().collect::<Vec<_>>().join(", ")),
            "projects": inside.iter().map(|n| folder.join(n).display().to_string()).collect::<Vec<_>>(),
        }))));
    }
    let id = format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]);
    std::fs::create_dir_all(dir(&state, &id)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let title = folder.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "project".into());
    let s = Session { id: id.clone(), folder: folder.display().to_string(), title, created: now(), sandbox: b.sandbox, policy, claude_session: None, last: now(), always: vec![], think: b.think };
    save(&state, &s);
    event(&state, &id, json!({ "kind": "start", "sandbox": s.sandbox, "folder": s.folder }));
    Ok(Json(json!({ "session": describe(&state, &s) })))
}

#[derive(Debug, Deserialize)]
pub struct After {
    #[serde(default)]
    pub after: u64,
}

/// `GET /code/sessions/{id}?after=N`.
pub(crate) async fn get_session(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Query(q): Query<After>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let st = state.clone();
    tokio::task::spawn_blocking(move || {
        let s = load(&st, &id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
        let pending: Vec<String> = proc_of(&id).map(|p| p.pending.lock().expect("code").keys().cloned().collect()).unwrap_or_default();
        let busy = proc_of(&id).is_some_and(|p| p.busy.load(Ordering::SeqCst));
        // This session's model requests only (Claude Code tags them; Chat and others aren't).
        let activity: Vec<Value> = if busy {
            crate::anthropic::activity().into_iter().filter(|a| a["session"] == id.as_str()).collect()
        } else {
            vec![]
        };
        Ok(Json(json!({ "session": describe(&st, &s), "events": events_after(&st, &id, q.after), "pending": pending, "activity": activity })))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub text: String,
    /// Stop the turn that's going first, so this starts a new one (rather than
    /// joining the running turn at its next step).
    #[serde(default)]
    pub interrupt: bool,
}

/// `POST /code/sessions/{id}/message`.
pub(crate) async fn post_message(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Message>) -> Reply {
    let s = load(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let text = b.text.trim().to_string();
    if text.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Say what to do."));
    }
    if state.endpoint().is_none() {
        return Err(err(StatusCode::CONFLICT, "No model is serving. Start one in Models first."));
    }
    // Running already: straight to it.
    if let Some(p) = proc_of(&id).filter(|p| p.think == s.think || p.busy.load(Ordering::SeqCst)) {
        // "Stop & send": end the running turn, then this starts a new one.
        if b.interrupt && p.busy.load(Ordering::SeqCst) {
            if !p.interrupted.swap(true, Ordering::SeqCst) {
                let rid = crate::pairing::issue_pair_id();
                let _ = send(&p, &json!({ "type": "control_request", "request_id": rid, "request": { "subtype": "interrupt" } })).await;
                expire_pending(&state, &id, &p);
                event(&state, &id, json!({ "kind": "notice", "text": "Stopping…", "interrupt": true }));
            }
            for _ in 0..150 {
                if !p.busy.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        // Mid-turn, Claude Code reads it at its next step, inside the running turn
        // (it doesn't wait for the turn to end): the window shows it there.
        let mid_turn = p.busy.load(Ordering::SeqCst);
        event(&state, &id, json!({ "kind": "user", "text": text, "mid_turn": mid_turn }));
        p.set_busy(true);
        if let Err(e) = send(&p, &json!({ "type": "user", "message": { "role": "user", "content": text } })).await {
            // It had just exited: start it again with this message.
            forget_proc(&id, &p);
            start_with(state.clone(), id.clone(), text, s.sandbox);
            let _ = e;
        }
        return Ok(Json(json!({ "sent": true })));
    }
    // Starting it can take a while (a first sandbox setup is a download): the message
    // is recorded now and goes to Claude Code once it's up, so a closed window or a
    // dropped request doesn't lose it.
    event(&state, &id, json!({ "kind": "user", "text": text }));
    start_with(state.clone(), id.clone(), text, s.sandbox);
    Ok(Json(json!({ "sent": true, "starting": true })))
}

/// Starts the session's Claude Code (in the background) and gives it `text`.
fn start_with(state: AppState, id: String, text: String, sandbox: bool) {
    STARTING.lock().expect("code").insert(id.clone());
    tokio::spawn(async move {
        if sandbox && !SETTING_UP.load(Ordering::SeqCst) {
            let port = state.ctrl_port();
            if !tokio::task::spawn_blocking(move || sandbox_ready(port).0).await.unwrap_or(true) {
                event(&state, &id, json!({ "kind": "notice", "text": "Setting up the sandbox the first time (a one-time download of about 370 MB)…" }));
            }
        }
        let started = running(&state, &id).await;
        STARTING.lock().expect("code").remove(&id);
        match started {
            Ok(p) => {
                p.set_busy(true);
                if let Err(e) = send(&p, &json!({ "type": "user", "message": { "role": "user", "content": text } })).await {
                    p.set_busy(false);
                    event(&state, &id, json!({ "kind": "error", "text": format!("Couldn't give it your message: {e}") }));
                    event(&state, &id, json!({ "kind": "idle" }));
                }
            }
            Err(e) => {
                event(&state, &id, json!({ "kind": "error", "text": format!("Couldn't start the agent: {e}") }));
                event(&state, &id, json!({ "kind": "idle" }));
            }
        }
    });
}

#[derive(Debug, Deserialize)]
pub struct Approve {
    pub request_id: String,
    pub allow: bool,
    #[serde(default)]
    pub always: bool,
}

/// `POST /code/sessions/{id}/approve`.
pub(crate) async fn post_approve(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Approve>) -> Reply {
    let p = proc_of(&id).ok_or_else(|| err(StatusCode::CONFLICT, "That request isn't waiting any more."))?;
    let taken = p.pending.lock().expect("code").remove(&b.request_id);
    p.save_pending();
    let (tool, input) = taken.ok_or_else(|| err(StatusCode::CONFLICT, "That request isn't waiting any more."))?;
    let response = if b.allow {
        json!({ "behavior": "allow", "updatedInput": input })
    } else {
        json!({ "behavior": "deny", "message": "The person said no. Ask them what to do instead, or try another way." })
    };
    send(&p, &json!({ "type": "control_response", "response": { "subtype": "success", "request_id": b.request_id, "response": response } })).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    let rule = always_rule(&tool, &input).filter(|_| b.allow && b.always);
    if let Some(k) = &rule {
        update(&state, &id, |s| {
            if !s.always.contains(k) {
                s.always.push(k.clone());
            }
        });
    }
    event(&state, &id, json!({ "kind": "decided", "request_id": b.request_id, "allow": b.allow, "always": rule.is_some() }));
    Ok(Json(json!({ "ok": true })))
}

/// `POST /code/sessions/{id}/interrupt`: stop the current turn.
pub(crate) async fn post_interrupt(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let p = proc_of(&id).ok_or_else(|| err(StatusCode::CONFLICT, "It isn't working on anything."))?;
    if !p.busy.load(Ordering::SeqCst) {
        return Ok(Json(json!({ "ok": true, "idle": true })));
    }
    // Once per turn, however many times Stop is pressed.
    if p.interrupted.swap(true, Ordering::SeqCst) {
        return Ok(Json(json!({ "ok": true })));
    }
    let rid = crate::pairing::issue_pair_id();
    send(&p, &json!({ "type": "control_request", "request_id": rid, "request": { "subtype": "interrupt" } })).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    expire_pending(&state, &id, &p);
    event(&state, &id, json!({ "kind": "notice", "text": "Stopping…", "interrupt": true }));
    Ok(Json(json!({ "ok": true })))
}

fn kill(p: &Proc) {
    if let Some(c) = &p.container {
        let _ = std::process::Command::new("docker").args(["rm", "-f", c]).stdin(Stdio::null()).output();
    }
    let _ = std::process::Command::new("systemctl").args(["--user", "stop", &p.unit]).stdin(Stdio::null()).output();
}

/// `POST /code/sessions/{id}/stop`: end Claude Code for this session (the conversation is kept).
pub(crate) async fn post_stop(State(_state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if let Some(p) = proc_of(&id) {
        forget_proc(&id, &p);
        tokio::task::spawn_blocking(move || kill(&p)).await.ok();
    }
    Ok(Json(json!({ "stopped": true })))
}

/// `DELETE /code/sessions/{id}`: forget a session (the project's files stay).
pub(crate) async fn delete_session(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_id(&id) || load(&state, &id).is_none() {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    if let Some(p) = proc_of(&id) {
        forget_proc(&id, &p);
        tokio::task::spawn_blocking(move || kill(&p)).await.ok();
    }
    let _ = std::fs::remove_dir_all(dir(&state, &id));
    SEQ.lock().expect("code").remove(&id);
    Ok(Json(json!({ "deleted": true })))
}

/// Git in the session's project, or a folder in it: see crate::safegit.
fn git(reach: Option<&crate::safegit::Reach>, folder: &str, args: &[&str]) -> Result<String, String> {
    crate::safegit::git(Path::new(folder), reach, args)
}

/// `GET /code/sessions/{id}/changes`.
pub(crate) async fn get_changes(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let s = load(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let events = dir(&state, &id).join("events.jsonl");
    let r = reach(&state, &s);
    let v = tokio::task::spawn_blocking(move || {
        let r = r.as_ref();
        let repo = git(r, &s.folder, &["rev-parse", "--is-inside-work-tree"]).is_ok();
        if !repo {
            return edited_changes(r, &s, &events);
        }
        let diff_of = |rev: &[&str]| git(r, &s.folder, &[&["diff"], crate::safegit::DIFF, rev].concat());
        let mut diff = diff_of(&["HEAD"]).or_else(|_| diff_of(&[])).unwrap_or_default();
        let files = crate::safegit::changed_files(Path::new(&s.folder), r);
        for f in files.iter().filter(|l| l.starts_with("??")).take(20) {
            let name = f[3..].trim();
            if let Ok(text) = std::fs::read_to_string(Path::new(&s.folder).join(name)) {
                if text.len() < 40_000 {
                    diff.push_str(&format!("\ndiff --git a/{name} b/{name}\nnew file\n--- /dev/null\n+++ b/{name}\n"));
                    for l in text.lines() {
                        diff.push_str(&format!("+{l}\n"));
                    }
                }
            }
        }
        crate::coding::cut(&mut diff, 400_000);
        json!({ "repo": repo, "files": files, "diff": diff, "log": git(r, &s.folder, &["log", "--oneline", "-8"]).unwrap_or_default() })
    }).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(v))
}

/// Changes for a folder that isn't a git repository (say, a folder of projects):
/// the files Claude Code edited, each diffed in the repository it's in, if any.
fn edited_changes(r: Option<&crate::safegit::Reach>, s: &Session, events: &Path) -> Value {
    let root = Path::new(&s.folder);
    let mut files = Vec::new();
    let mut diff = String::new();
    for rel in &edited_in(s, events) {
        let path = root.join(rel);
        let Some(dir) = path.parent().map(|d| d.display().to_string()) else { continue };
        let abs = path.display().to_string();
        let status = if !path.exists() {
            "D".to_string()
        } else {
            match git(r, &dir, &["status", "--porcelain", "--ignore-submodules=all", "--", &abs]) {
                // Committed since, or edited back to how it was.
                Ok(st) if st.trim().is_empty() => continue,
                Ok(st) => st.trim().split_whitespace().next().unwrap_or("M").to_string(),
                Err(_) => "M".to_string(),
            }
        };
        files.push(format!("{status:>2} {rel}"));
        if diff.len() > 400_000 {
            continue;
        }
        match git(r, &dir, &[&["diff"], crate::safegit::DIFF, &["HEAD", "--", &abs]].concat()) {
            Ok(d) if !d.trim().is_empty() => diff.push_str(&d),
            _ => {
                if let Ok(text) = std::fs::read_to_string(&path).map(|t| t.chars().take(40_000).collect::<String>()) {
                    diff.push_str(&format!("\ndiff --git a/{rel} b/{rel}\n--- /dev/null\n+++ b/{rel}\n"));
                    for l in text.lines() {
                        diff.push_str(&format!("+{l}\n"));
                    }
                }
            }
        }
    }
    crate::coding::cut(&mut diff, 400_000);
    json!({ "repo": false, "edited": true, "files": files, "diff": diff, "log": "" })
}

/// Every file the session's editing tool calls changed, from its event log, in order.
fn edited_in(s: &Session, events: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(events).unwrap_or_default();
    let mut pending = HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for e in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let tid = e["id"].as_str().unwrap_or("").to_string();
        match e["kind"].as_str() {
            Some("tool") => {
                if let Some(f) = edited_file(s, e["name"].as_str().unwrap_or(""), &e["input"]) {
                    pending.insert(tid, f);
                }
            }
            Some("tool_result") if e["is_error"] != true => {
                if let Some(f) = pending.remove(&tid).filter(|f| !out.contains(f)) {
                    out.push(f);
                }
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct Commit {
    pub message: String,
}

/// `POST /code/sessions/{id}/commit`: commit everything that changed.
pub(crate) async fn post_commit(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Commit>) -> Reply {
    let s = load(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let msg = if b.message.trim().is_empty() { "Changes made in TT Station on the QuietBox".to_string() } else { b.message.trim().to_string() };
    let r = reach(&state, &s);
    let out = tokio::task::spawn_blocking(move || {
        let r = r.as_ref();
        // This folder only: in a subfolder of a bigger repository, not everyone else's changes.
        git(r, &s.folder, &["add", "-A", "--", "."])?;
        git(r, &s.folder, &["-c", "user.name=TT Station", "-c", "user.email=tt-station@localhost", "commit", "-q", "-m", &msg])?;
        git(r, &s.folder, &["log", "--oneline", "-1"])
    }).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| err(StatusCode::CONFLICT, e))?;
    event(&state, &id, json!({ "kind": "notice", "text": format!("Committed: {}", out.trim()) }));
    Ok(Json(json!({ "commit": out.trim() })))
}

/// `POST /code/sessions/{id}/discard`: put every tracked file back as it was at
/// the last commit, and delete the new files the agent made. Other untracked
/// files (the person's own, never committed) are left alone.
pub(crate) async fn post_discard(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let s = load(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    let events = dir(&state, &id).join("events.jsonl");
    let r = reach(&state, &s);
    let removed = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let r = r.as_ref();
        git(r, &s.folder, &["checkout", "--", "."])?;
        let made: std::collections::HashSet<String> = edited_in(&s, &events).into_iter().collect();
        let untracked = git(r, &s.folder, &["ls-files", "--others", "--exclude-standard", "--", "."])?;
        let mut n = 0;
        for f in untracked.lines().map(str::trim).filter(|f| made.contains(*f)) {
            if std::fs::remove_file(Path::new(&s.folder).join(f)).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }).await.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?.map_err(|e| err(StatusCode::CONFLICT, e))?;
    let also = if removed > 0 { format!(", and deleted the {removed} new file{} it made", if removed == 1 { "" } else { "s" }) } else { String::new() };
    event(&state, &id, json!({ "kind": "notice", "text": format!("Put every file back as it was at the last commit{also}.") }));
    Ok(Json(json!({ "discarded": true, "removed": removed })))
}

#[derive(Debug, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub think: Option<bool>,
}

/// `POST /code/sessions/{id}/settings`: `{think}`. Takes effect from the next message.
pub(crate) async fn post_settings(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Settings>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let s = update(&state, &id, |s| {
        if let Some(t) = b.think {
            s.think = t;
        }
    })
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    // Claude Code reads it when it starts: restart it quietly between turns (the
    // conversation resumes); mid-turn, the next message does (`running`).
    if let Some(p) = proc_of(&id).filter(|p| p.think != s.think && !p.busy.load(Ordering::SeqCst)) {
        forget_proc(&id, &p);
        tokio::task::spawn_blocking(move || kill(&p)).await.ok();
    }
    Ok(Json(json!({ "session": describe(&state, &s) })))
}

/// The always-allowed list, without one entry.
#[derive(Debug, Deserialize)]
pub struct Forget {
    pub rule: String,
}

/// `POST /code/sessions/{id}/forget`: stop always allowing something.
pub(crate) async fn post_forget(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Forget>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let s = update(&state, &id, |s| s.always.retain(|r| r != &b.rule)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    Ok(Json(json!({ "always": s.always })))
}

#[derive(Debug, Deserialize)]
pub struct Rename {
    pub title: String,
}

/// `POST /code/sessions/{id}/rename`: give the session a name of your own.
pub(crate) async fn post_rename(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Rename>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such session."));
    }
    let title = b.title.trim().to_string();
    if title.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Give it a name."));
    }
    if title.len() > 120 {
        return Err(err(StatusCode::BAD_REQUEST, "Keep the name under 120 characters."));
    }
    let s = update(&state, &id, |s| s.title = title.clone())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such session."))?;
    Ok(Json(json!({ "session": describe(&state, &s) })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(folder: &str, sandbox: bool) -> Session {
        Session { id: "t".into(), folder: folder.into(), title: "t".into(), created: 0, sandbox, policy: "ask".into(), claude_session: None, last: 0, always: vec![], think: false }
    }

    #[test]
    fn always_rules_never_cover_more_than_they_say() {
        let cmd = |c: &str| json!({ "command": c });
        assert_eq!(always_rule("Bash", &cmd("pytest -q")).as_deref(), Some("Bash:pytest"));
        assert_eq!(always_rule("Bash", &cmd("npm test -- --watch=false")).as_deref(), Some("Bash:npm test"));
        assert_eq!(always_rule("Bash", &cmd("python3 -m pytest tests")).as_deref(), Some("Bash:python3 -m pytest"));
        assert_eq!(always_rule("Bash", &cmd("cd src && make")).as_deref(), Some("Bash=cd src && make"));
        assert_eq!(always_rule("Bash", &cmd("rm -rf build")).as_deref(), Some("Bash=rm -rf build"));
        assert_eq!(always_rule("Write", &json!({})).as_deref(), Some("edits"));
        let rules = vec!["Bash:pytest".to_string(), "Bash:git status".to_string(), "Bash=cd src && make".to_string(), "Bash:cd".to_string()];
        assert!(allowed_by(&rules, "Bash", &cmd("pytest -x tests/")));
        assert!(allowed_by(&rules, "Bash", &cmd("git status --short")));
        assert!(allowed_by(&rules, "Bash", &cmd("cd src && make")));
        // A rule from before (`Bash:cd`) still never covers a chain.
        assert!(!allowed_by(&rules, "Bash", &cmd("cd / && rm -rf ~")));
        assert!(!allowed_by(&rules, "Bash", &cmd("pytest; rm -rf ~")));
        assert!(!allowed_by(&rules, "Bash", &cmd("git push --force")));
        assert!(!allowed_by(&rules, "Bash", &cmd("pytestx")));
        assert!(!allowed_by(&rules, "Edit", &json!({})));
        assert!(allowed_by(&["edits".to_string()], "MultiEdit", &json!({})));
    }

    #[test]
    fn knows_which_file_an_edit_changes() {
        let boxed = session("/home/u/code", true);
        assert_eq!(edited_file(&boxed, "Edit", &json!({ "file_path": "/work/tt-station/desktop/src/ui.tsx" })).as_deref(), Some("tt-station/desktop/src/ui.tsx"));
        assert_eq!(edited_file(&boxed, "Write", &json!({ "file_path": "/work/new.py" })).as_deref(), Some("new.py"));
        assert_eq!(edited_file(&boxed, "Read", &json!({ "file_path": "/work/a.py" })), None);
        assert_eq!(edited_file(&boxed, "Edit", &json!({ "file_path": "/etc/passwd" })), None);
        assert_eq!(edited_file(&boxed, "Edit", &json!({ "file_path": "/work/../etc/passwd" })), None);
        let host = session("/home/u/code/", false);
        assert_eq!(edited_file(&host, "MultiEdit", &json!({ "file_path": "/home/u/code/x/y.rs" })).as_deref(), Some("x/y.rs"));
        assert_eq!(edited_file(&host, "Edit", &json!({ "file_path": "/home/u/codex/y.rs" })), None);
    }

    #[test]
    fn lists_edits_that_worked_from_the_event_log() {
        let dir = std::env::temp_dir().join(format!("tt-ws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("events.jsonl");
        let lines = [
            json!({ "kind": "tool", "id": "1", "name": "Edit", "input": { "file_path": "/work/a.py" } }),
            json!({ "kind": "tool_result", "id": "1", "is_error": false }),
            json!({ "kind": "tool", "id": "2", "name": "Edit", "input": { "file_path": "/work/b.py" } }),
            json!({ "kind": "tool_result", "id": "2", "is_error": true }),
            json!({ "kind": "tool", "id": "3", "name": "Read", "input": { "file_path": "/work/c.py" } }),
            json!({ "kind": "tool_result", "id": "3", "is_error": false }),
            json!({ "kind": "tool", "id": "4", "name": "Write", "input": { "file_path": "/work/a.py" } }),
            json!({ "kind": "tool_result", "id": "4", "is_error": false }),
        ];
        std::fs::write(&log, lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")).unwrap();
        assert_eq!(edited_in(&session("/x", true), &log), vec!["a.py".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_git_directories_the_sandbox_gets_read_only() {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("sub/.git")).unwrap();
        assert_eq!(git_dirs(&repo), vec![(".git".to_string(), Some(std::fs::canonicalize(repo.join(".git")).unwrap()))]);
        // A folder of projects: each one's; a linked worktree's `.git` file too, with no directory to trust.
        let many = t.path().join("many");
        std::fs::create_dir_all(many.join("a/.git")).unwrap();
        std::fs::create_dir_all(many.join("b")).unwrap();
        std::fs::write(many.join("b/.git"), "gitdir: /elsewhere\n").unwrap();
        std::fs::create_dir_all(many.join("c")).unwrap();
        let got = git_dirs(&many);
        assert_eq!(got, vec![("a/.git".to_string(), Some(std::fs::canonicalize(many.join("a/.git")).unwrap())), ("b/.git".to_string(), None)]);
        assert!(git_dirs(&t.path().join("none")).is_empty());
    }
}
