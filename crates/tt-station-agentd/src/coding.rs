// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What Code (`crate::workspace`) shares with the rest of the agent about
//! running a coding agent on the box's own model: how Claude Code is tuned for
//! it ([`claude_tuning`], [`code_window`]), whether the served model suits it
//! ([`check`]), the one app key it reaches the model with ([`key`]), and which
//! project folders it may work in ([`allowed_folder`], and for a member's
//! sandbox [`member_folder`]).
//!
//! - `GET /coding/folders?path=`: folders to pick from; `POST /coding/folders`: `{parent, name}`
//!   makes a new project (a git repository).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

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
/// served model's `context` (its `max_model_len`, `None` when unknown). Code's
/// launch (`workspace.rs`) and its speed panel (`codespeed.rs`) both use this,
/// so what the panel shows is what was launched.
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
/// arguments and environment Code (`workspace.rs`) starts it with. The caller
/// adds what is its own (config folder, key, session header, thinking).
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

/// The one app key Code reaches the box's model with (made once, kept 0600).
pub(crate) fn key(state: &AppState) -> Result<String, String> {
    let path = root(state).join("key");
    if let Ok(k) = std::fs::read_to_string(&path) {
        let k = k.trim().to_string();
        if state.app_keys().check(&k, now()) {
            return Ok(k);
        }
    }
    let (_, k) = state.app_keys().create("Code", now())?;
    // Whole or not at all, owner-only from the first byte: a half-written key
    // would be thrown away (and a new one made) at the next start.
    crate::fsio::write_atomic(&path, k.as_bytes(), crate::fsio::PRIVATE)
        .map_err(|e| e.to_string())?;
    Ok(k)
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
/// tt-kernel, tt-model-manager, TT Station's source). Real paths where they
/// exist.
fn box_software(state: &AppState) -> Vec<PathBuf> {
    let h = home();
    let env_or = |var: &str, default: PathBuf| std::env::var_os(var).map_or(default, PathBuf::from);
    let mut v = vec![
        state.config_dir(),
        PathBuf::from(crate::config::default_tt_inference_repo()),
        env_or("TT_METAL_HOME", h.join("tt-metal")),
        h.join("tt-kernel-package-manager"),
        h.join("dispatch"),
        h.join("code/tt-model-manager"),
        h.join("code/tt-station"),
        h.join("bin"),
    ];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::NullBackend),
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
            std::sync::Arc::new(crate::serving::NullBackend),
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
            std::sync::Arc::new(crate::serving::NullBackend),
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
