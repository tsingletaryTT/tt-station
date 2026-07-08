# Log Viewing for tt-station — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Surface the tt-inference-server serving logs (where remote model-start failures actually live) to the operator and the Mac, via an agent `GET /logs` route (+ WS follow), a `tt logs` CLI command, a `tt console` log pane, and journal breadcrumbs on serve.

**Architecture:** Both log sources are already **files** under `<repo>/workflow_logs/` — run.py streams the container's stdout/stderr to `docker_server/vllm_*.log` (persists after container death) and writes its own `run_logs/*.log`. So the whole feature is "resolve newest `*.log` in a dir, tail last N lines, follow by byte offset" — no `docker logs` subprocess. A new pure `logs` module in the agent holds the file logic; a new unauthed `/logs` route + `/logs/stream` WS expose it (mirroring `/serving` and `/telemetry`); the CLI and console consume the route; `runpy.rs` parses run.py's captured stdout for journal breadcrumbs.

**Tech Stack:** Rust (axum, tokio, tokio-tungstenite for the WS test/CLI-follow), clap, ratatui. Existing patterns: `AppState`/`Inner` + `with_*` builders (`routes.rs`), `telemetry_stream` WS loop, `discover_serving`, `FakeRunner`/`FakeEnv` test seams, `assert_cmd` + mock-box e2e.

## Global Constraints

- **CLI tool names stay configurable** — never hardcode `tt` / `tt-station-agentd` / service names in more than one place; reuse `crates/tt/src/console/names.rs::ToolNames` where a name is needed. (See the `configurable-cli-tool-names` memory.)
- **Reads are unauthed** — `/logs` and `/logs/stream` join the same unauthed group as `/telemetry`, `/serving`, `/status`, `/models`, `/config` (omit the `BearerAuth` extractor). Do NOT add auth to reads.
- **No right-side borders** in any TUI panel — `Borders::LEFT | Borders::BOTTOM` only (project rule, enforced in `ui.rs`).
- **Agent logging** is `eprintln!("tt-station-agentd: ...")` to stderr (→ journald). No `log`/`tracing` crate.
- **Redaction:** every log line emitted over the wire passes through a redactor masking obvious secrets (`hf_[A-Za-z0-9]{20,}`, `Bearer <token>`, `sk-…`). Cheap defense-in-depth; the surface stays unauthed.
- **TDD, frequent commits, DRY, YAGNI.** Run `cargo fmt` (pinned toolchain 1.96.0) before every commit so the workspace fmt gate stays green.
- **Tail defaults:** `DEFAULT_TAIL = 200`, `MAX_TAIL = 2000` (cap response size).
- Spec: `docs/superpowers/specs/2026-07-07-log-viewing-design.md`.

---

### Task 1: `logs` pure module in the agent

**Files:**
- Create: `crates/tt-station-agentd/src/logs.rs`
- Modify: `crates/tt-station-agentd/src/lib.rs` (add `pub mod logs;` — check the existing `mod`/`pub mod` list and match its style)
- Test: inline `#[cfg(test)]` in `logs.rs`

**Interfaces:**
- Consumes: nothing (pure module, std only).
- Produces:
  - `pub enum LogSource { Container, Run }` with `pub fn subdir(&self) -> &'static str` (`Container => "docker_server"`, `Run => "run_logs"`) and `pub fn parse(s: &str) -> Option<LogSource>` (`"container" => Container`, `"run" => Run`, else `None`).
  - `pub const DEFAULT_TAIL: usize = 200;` and `pub const MAX_TAIL: usize = 2000;`
  - `pub fn logs_dir(repo_dir: &std::path::Path, source: LogSource) -> std::path::PathBuf` → `repo_dir.join("workflow_logs").join(source.subdir())`
  - `pub fn newest_log_file(dir: &std::path::Path) -> std::io::Result<Option<std::path::PathBuf>>` → the `*.log` file in `dir` with the newest mtime, or `Ok(None)` if the dir is missing/empty.
  - `pub fn tail_lines(path: &std::path::Path, max: usize) -> std::io::Result<Vec<String>>` → last `max` lines of the file (each without trailing `\n`); `Ok(vec![])` for an empty file.
  - `pub fn read_new_lines(path: &std::path::Path, from_offset: u64) -> std::io::Result<(Vec<String>, u64)>` → lines fully written after byte `from_offset`, plus the new end offset. A trailing partial line (no `\n` yet) is NOT emitted and NOT counted in the returned offset (so it's re-read complete next tick).
  - `pub fn redact_line(line: &str) -> String` → masks secret substrings.

- [ ] **Step 1: Write failing tests for `LogSource` + path helpers**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn source_parse_and_subdir() {
        assert!(matches!(LogSource::parse("container"), Some(LogSource::Container)));
        assert!(matches!(LogSource::parse("run"), Some(LogSource::Run)));
        assert!(LogSource::parse("bogus").is_none());
        assert_eq!(LogSource::Container.subdir(), "docker_server");
        assert_eq!(LogSource::Run.subdir(), "run_logs");
    }

    #[test]
    fn logs_dir_joins_workflow_logs() {
        let d = logs_dir(Path::new("/repo"), LogSource::Container);
        assert_eq!(d, Path::new("/repo/workflow_logs/docker_server"));
    }
}
```

- [ ] **Step 2: Run tests, verify they fail to compile (symbols undefined)**

Run: `cargo test -p tt-station-agentd logs::tests -- --nocapture`
Expected: FAIL (unresolved `LogSource`, `logs_dir`, …).

- [ ] **Step 3: Implement `LogSource`, constants, and path helpers**

```rust
//! Pure file-based access to tt-inference-server's serving logs.
//!
//! run.py streams the serving container's stdout/stderr to
//! `<repo>/workflow_logs/docker_server/vllm_*.log` (this is where model-load
//! failures actually appear, and it persists after the container is removed),
//! and writes its own launch log to `<repo>/workflow_logs/run_logs/*.log`.
//! Everything here is "newest *.log in a dir, tail N lines, follow by offset" —
//! no `docker logs` subprocess. Kept pure (std only) so it unit-tests without a
//! router or a real box.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DEFAULT_TAIL: usize = 200;
pub const MAX_TAIL: usize = 2000;

/// Which serving-log stream to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    /// The container's stdout/stderr (`workflow_logs/docker_server/*.log`).
    Container,
    /// run.py's own launch log (`workflow_logs/run_logs/*.log`).
    Run,
}

impl LogSource {
    pub fn subdir(&self) -> &'static str {
        match self {
            LogSource::Container => "docker_server",
            LogSource::Run => "run_logs",
        }
    }

    pub fn parse(s: &str) -> Option<LogSource> {
        match s {
            "container" => Some(LogSource::Container),
            "run" => Some(LogSource::Run),
            _ => None,
        }
    }
}

pub fn logs_dir(repo_dir: &Path, source: LogSource) -> PathBuf {
    repo_dir.join("workflow_logs").join(source.subdir())
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p tt-station-agentd logs::tests`
Expected: the two tests PASS.

- [ ] **Step 5: Write failing tests for `newest_log_file`, `tail_lines`, `read_new_lines`, `redact_line`**

```rust
    #[test]
    fn newest_log_file_picks_newest_and_handles_missing() {
        use std::time::{Duration, SystemTime};
        let dir = tempfile::tempdir().unwrap();
        // missing subdir → None
        assert!(newest_log_file(&dir.path().join("nope")).unwrap().is_none());
        // empty dir → None
        assert!(newest_log_file(dir.path()).unwrap().is_none());
        let old = dir.path().join("a.log");
        let new = dir.path().join("b.log");
        std::fs::write(&old, "old\n").unwrap();
        std::fs::write(&new, "new\n").unwrap();
        // force distinct mtimes
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        filetime::set_file_mtime(&old, filetime::FileTime::from_system_time(base)).unwrap();
        filetime::set_file_mtime(&new, filetime::FileTime::from_system_time(base + Duration::from_secs(10))).unwrap();
        assert_eq!(newest_log_file(dir.path()).unwrap().unwrap(), new);
    }

    #[test]
    fn tail_lines_returns_last_n() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.log");
        std::fs::write(&p, "l1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(tail_lines(&p, 2).unwrap(), vec!["l3", "l4"]);
        assert_eq!(tail_lines(&p, 99).unwrap(), vec!["l1", "l2", "l3", "l4"]);
        let empty = dir.path().join("e.log");
        std::fs::write(&empty, "").unwrap();
        assert!(tail_lines(&empty, 5).unwrap().is_empty());
    }

    #[test]
    fn read_new_lines_skips_trailing_partial() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.log");
        std::fs::write(&p, "one\ntwo\npar").unwrap(); // "par" has no newline yet
        let (lines, off) = read_new_lines(&p, 0).unwrap();
        assert_eq!(lines, vec!["one", "two"]);
        // offset must sit right after "two\n", so the partial re-reads whole later
        assert_eq!(off, "one\ntwo\n".len() as u64);
        // append the rest of the partial line + a new one
        std::fs::write(&p, "one\ntwo\npartial\nthree\n").unwrap();
        let (lines2, _off2) = read_new_lines(&p, off).unwrap();
        assert_eq!(lines2, vec!["partial", "three"]);
    }

    #[test]
    fn redact_masks_known_secret_shapes() {
        assert_eq!(redact_line("token hf_abcdefghijklmnopqrstuvwx done"),
                   "token hf_*** done");
        assert_eq!(redact_line("Authorization: Bearer deadbeefcafebabe0123"),
                   "Authorization: Bearer ***");
        assert_eq!(redact_line("plain line, no secrets"), "plain line, no secrets");
    }
```

Add `tempfile`, `filetime` as `[dev-dependencies]` in `crates/tt-station-agentd/Cargo.toml` if not present (check first — `tempfile` is likely already a dev-dep given the temp-file tests referenced in the telemetry test).

- [ ] **Step 6: Run tests, verify they fail**

Run: `cargo test -p tt-station-agentd logs::tests`
Expected: FAIL (functions undefined).

- [ ] **Step 7: Implement the file helpers + redactor**

```rust
/// Newest `*.log` in `dir` by mtime. `Ok(None)` if `dir` is absent or has no logs.
pub fn newest_log_file(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let mtime = entry.metadata()?.modified()?;
        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
            best = Some((mtime, path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// Last `max` lines of `path` (newline-stripped). Reads the whole file; log
/// files here are bounded (run.py rotates per-serve) so this is fine.
pub fn tail_lines(path: &Path, max: usize) -> std::io::Result<Vec<String>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        lines.push(line?);
    }
    let start = lines.len().saturating_sub(max);
    Ok(lines.split_off(start))
}

/// Lines fully written after byte `from_offset`. Returns the lines and the new
/// end offset (positioned after the last complete line; a trailing partial line
/// is not emitted and not counted, so it re-reads whole on the next call).
pub fn read_new_lines(path: &Path, from_offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if from_offset >= len {
        return Ok((Vec::new(), from_offset.min(len)));
    }
    file.seek(SeekFrom::Start(from_offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    // Find the last newline; everything after it is a partial line.
    let last_nl = buf.iter().rposition(|&b| b == b'\n');
    let complete_len = match last_nl {
        Some(idx) => idx + 1, // include the newline
        None => 0,            // no complete line yet
    };
    let complete = &buf[..complete_len];
    let lines: Vec<String> = complete
        .split(|&b| b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    Ok((lines, from_offset + complete_len as u64))
}

/// Mask obvious secret shapes. Cheap defense-in-depth for an unauthed surface.
pub fn redact_line(line: &str) -> String {
    // hf_<20+ alnum> → hf_***
    let mut out = mask_prefixed(line, "hf_", 20);
    // sk-<20+ alnum> → sk-***
    out = mask_prefixed(&out, "sk-", 20);
    // "Bearer <token>" → "Bearer ***"
    out = mask_after(&out, "Bearer ");
    out
}

fn mask_prefixed(s: &str, prefix: &str, min_len: usize) -> String {
    let mut result = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(prefix) {
        let (head, tail) = rest.split_at(pos);
        result.push_str(head);
        let after = &tail[prefix.len()..];
        let tok_len = after.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
        if tok_len >= min_len {
            result.push_str(prefix);
            result.push_str("***");
            rest = &after[tok_len..];
        } else {
            result.push_str(prefix);
            rest = after;
        }
    }
    result.push_str(rest);
    result
}

fn mask_after(s: &str, marker: &str) -> String {
    match s.find(marker) {
        Some(pos) => {
            let after = &s[pos + marker.len()..];
            let tok_len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
                .count();
            if tok_len >= 8 {
                format!("{}{}***{}", &s[..pos], marker, &after[tok_len..])
            } else {
                s.to_string()
            }
        }
        None => s.to_string(),
    }
}
```

- [ ] **Step 8: Run tests, verify pass; fmt; commit**

Run: `cargo test -p tt-station-agentd logs::tests && cargo fmt`
Expected: all `logs::tests` PASS.

```bash
git add crates/tt-station-agentd/src/logs.rs crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/Cargo.toml
git commit -m "feat(agentd): pure logs module (newest-file, tail, follow-by-offset, redact)"
```

---

### Task 2: `GET /logs` plain route + repo dir on `AppState`

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (add `tt_inference_repo` to `Inner`, a `with_log_source` builder, an accessor, the `LogsResponse` type, the `get_logs` handler, and the route registration)
- Modify: `crates/tt-station-agentd/src/main.rs` (call `.with_log_source(repo_dir)` when the backend is runpy — locate where `AppState` is built and where the runpy repo dir is known)
- Test: `crates/tt-station-agentd/tests/logs.rs` (new)

**Interfaces:**
- Consumes (from Task 1): `crate::logs::{LogSource, logs_dir, newest_log_file, tail_lines, redact_line, DEFAULT_TAIL, MAX_TAIL}`.
- Consumes (existing, from the interface map): `AppState`/`Inner` with `with_*` builders using `Arc::get_mut` (template: `with_serving_config` at `routes.rs:359-370`); handler+State pattern `axum::extract::State<AppState>` (`get_serving` at `routes.rs:1555-1573`); query parsing via `RawQuery` (`telemetry_ws` at `routes.rs:1313-1332`); `app()` router at `routes.rs:1701-1718`.
- Produces:
  - `Inner.tt_inference_repo: Option<std::path::PathBuf>` (new field; default `None` in every `Inner` constructor).
  - `pub fn with_log_source(self, repo_dir: impl Into<PathBuf>) -> Self` on `AppState` (mirrors `with_serving_config`).
  - `fn tt_inference_repo(&self) -> Option<&std::path::Path>` accessor on `AppState`.
  - `LogsResponse { source: String, origin: Option<String>, lines: Vec<String> }` (`#[derive(Serialize)]`).
  - `async fn get_logs(State<AppState>, RawQuery) -> (StatusCode, Json<...>)` handler, registered as `.route("/logs", get(get_logs))`.

- [ ] **Step 1: Add the `tt_inference_repo` field + builder + accessor**

In `Inner` (near `serving_host`/`serving_port`, `routes.rs:109-224`), add:

```rust
    /// Path to the tt-inference-server checkout, when the runpy backend is
    /// active. `None` for backends without a workflow_logs dir (e.g. dstack).
    /// Enables the `/logs` routes to locate `workflow_logs/{docker_server,run_logs}`.
    tt_inference_repo: Option<std::path::PathBuf>,
```

Set `tt_inference_repo: None` in every place `Inner { .. }` is constructed (there is at least the one inside `AppState::new`; grep `Inner {` to find all — add the field to each). Then, alongside the other `with_*` builders (template `with_serving_config`, `routes.rs:359-370`):

```rust
    /// Point the `/logs` routes at a tt-inference-server checkout. Call before
    /// the state is cloned (uses `Arc::get_mut`), same as the other `with_*`.
    pub fn with_log_source(mut self, repo_dir: impl Into<std::path::PathBuf>) -> Self {
        if let Some(inner) = std::sync::Arc::get_mut(&mut self.inner) {
            inner.tt_inference_repo = Some(repo_dir.into());
        }
        self
    }

    fn tt_inference_repo(&self) -> Option<&std::path::Path> {
        self.inner.tt_inference_repo.as_deref()
    }
```

- [ ] **Step 2: Write the failing route test**

Create `crates/tt-station-agentd/tests/logs.rs` (mirror `tests/status.rs` setup — `AppState::new` + `app()` + ephemeral `TcpListener` + `tokio::spawn(axum::serve(...))`):

```rust
use std::sync::Arc;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::DstackBackend; // adjust path to the no-op backend used in tests/status.rs

async fn spawn(state: AppState) -> String {
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
    format!("http://{addr}")
}

#[tokio::test]
async fn logs_run_source_returns_newest_file_tail() {
    let repo = tempfile::tempdir().unwrap();
    let run_dir = repo.path().join("workflow_logs/run_logs");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("old.log"), "old1\nold2\n").unwrap();
    std::fs::write(run_dir.join("new.log"), "a\nb\nc\n").unwrap();
    // make new.log newest
    let t = filetime::FileTime::from_unix_time(2_000_000, 0);
    filetime::set_file_mtime(run_dir.join("new.log"), t).unwrap();
    let t0 = filetime::FileTime::from_unix_time(1_000_000, 0);
    filetime::set_file_mtime(run_dir.join("old.log"), t0).unwrap();

    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path());
    let base = spawn(state).await;

    let body: serde_json::Value = reqwest::get(format!("{base}/logs?source=run&tail=2"))
        .await.unwrap().json().await.unwrap();
    assert_eq!(body["source"], "run");
    assert_eq!(body["lines"], serde_json::json!(["b", "c"]));
    assert!(body["origin"].as_str().unwrap().ends_with("new.log"));
}

#[tokio::test]
async fn logs_no_file_yields_empty_lines_null_origin() {
    let repo = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path());
    let base = spawn(state).await;
    let resp = reqwest::get(format!("{base}/logs?source=container")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["lines"], serde_json::json!([]));
    assert!(body["origin"].is_null());
}

#[tokio::test]
async fn logs_no_repo_configured_is_conflict() {
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend));
    let base = spawn(state).await;
    let resp = reqwest::get(format!("{base}/logs?source=run")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
}
```

(Adjust the `DstackBackend` import path to whatever `tests/status.rs` uses. Add `filetime` to dev-deps if Task 1 didn't.)

- [ ] **Step 3: Run tests, verify they fail**

Run: `cargo test -p tt-station-agentd --test logs`
Expected: FAIL (route 404s / `with_log_source` maybe compiles but `/logs` unregistered).

- [ ] **Step 4: Implement `LogsResponse` + `get_logs` + register the route**

Add near the other response types:

```rust
#[derive(serde::Serialize)]
struct LogsResponse {
    source: String,
    /// Absolute path of the file being tailed, or `null` when nothing has been
    /// logged yet for this source.
    origin: Option<String>,
    lines: Vec<String>,
}
```

Handler (mirrors `get_serving`; parses `?source=`/`?tail=` from `RawQuery` like `telemetry_ws`; does the blocking file work in `spawn_blocking`):

```rust
async fn get_logs(
    axum::extract::State(state): axum::extract::State<AppState>,
    RawQuery(query): RawQuery,
) -> (StatusCode, Json<serde_json::Value>) {
    let (source_str, tail) = parse_logs_query(query.as_deref());
    let source = match crate::logs::LogSource::parse(&source_str) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("unknown source '{source_str}'") })),
            )
        }
    };
    let repo = match state.tt_inference_repo() {
        Some(p) => p.to_path_buf(),
        None => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "logs unavailable: no tt-inference-server repo configured (non-runpy backend)"
                })),
            )
        }
    };

    let resp = tokio::task::spawn_blocking(move || {
        let dir = crate::logs::logs_dir(&repo, source);
        let file = crate::logs::newest_log_file(&dir)?;
        let (origin, lines) = match file {
            Some(path) => {
                let lines = crate::logs::tail_lines(&path, tail)?
                    .iter()
                    .map(|l| crate::logs::redact_line(l))
                    .collect();
                (Some(path.display().to_string()), lines)
            }
            None => (None, Vec::new()),
        };
        Ok::<_, std::io::Error>(LogsResponse {
            source: source_str,
            origin,
            lines,
        })
    })
    .await;

    match resp {
        Ok(Ok(r)) => (StatusCode::OK, Json(serde_json::to_value(r).unwrap())),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "failed to read logs" })),
        ),
    }
}

/// Parse `?source=<>&tail=<>` from a raw query string. Defaults: source
/// "container", tail DEFAULT_TAIL, capped at MAX_TAIL.
fn parse_logs_query(query: Option<&str>) -> (String, usize) {
    let mut source = "container".to_string();
    let mut tail = crate::logs::DEFAULT_TAIL;
    if let Some(q) = query {
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("source=") {
                source = v.to_string();
            } else if let Some(v) = kv.strip_prefix("tail=") {
                if let Ok(n) = v.parse::<usize>() {
                    tail = n.min(crate::logs::MAX_TAIL);
                }
            }
        }
    }
    (source, tail)
}
```

Register in `app()` (`routes.rs:1704-1717`), in the unauthed group next to `/serving`:

```rust
        .route("/logs", get(get_logs))
```

Add a unit test for `parse_logs_query` inline (defaults; cap; explicit values).

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo test -p tt-station-agentd --test logs && cargo test -p tt-station-agentd parse_logs_query`
Expected: all PASS.

- [ ] **Step 6: Wire the repo dir in `main.rs`**

Find where `AppState` is constructed in `crates/tt-station-agentd/src/main.rs` and where the resolved runpy repo dir is available (the same value handed to `RunPyConfig::repo_dir`). When the backend is runpy, chain `.with_log_source(<repo_dir>)`. Guard so non-runpy backends stay `None`. (No new test here — covered by the live smoke in Step 7 + the route tests above with an explicit `with_log_source`.)

- [ ] **Step 7: Build, fmt, live smoke, commit**

Run: `cargo build --release -p tt-station-agentd && cargo fmt`
Then install + restart the service and smoke it (matches the project's deploy loop):

```bash
install -m 755 target/release/tt-station-agentd ~/.local/bin/tt-station-agentd
systemctl --user restart tt-station-agentd.service
curl -s 'http://127.0.0.1:8765/logs?source=container&tail=5'   # expect JSON with lines[] (may be empty when idle)
curl -s 'http://127.0.0.1:8765/logs?source=run&tail=5'
```

```bash
git add crates/tt-station-agentd/src/routes.rs crates/tt-station-agentd/src/main.rs crates/tt-station-agentd/tests/logs.rs
git commit -m "feat(agentd): GET /logs (unauthed) tails newest run/container log"
```

---

### Task 3: `GET /logs/stream` WebSocket follow

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (add `logs_ws` handler + `logs_stream` loop + route registration)
- Test: `crates/tt-station-agentd/tests/logs_stream.rs` (new; mirror `tests/telemetry.rs`)

**Interfaces:**
- Consumes: Task 1 helpers; Task 2's `parse_logs_query`, `AppState::tt_inference_repo`; the WS pattern from `telemetry_ws`/`telemetry_stream` (`routes.rs:1313-1449`) — `WebSocketUpgrade` + `RawQuery`, `ws.on_upgrade(...)`, `tokio::select!` between `ticker.tick()` and `socket.recv()`, `Message::Text`.
- Produces: `.route("/logs/stream", get(logs_ws))`.

- [ ] **Step 1: Write the failing WS test**

Create `crates/tt-station-agentd/tests/logs_stream.rs` (mirror `tests/telemetry.rs`: `app()` + ephemeral port + `tokio_tungstenite::connect_async` + `futures_util::StreamExt`):

```rust
use futures_util::StreamExt;
use std::sync::Arc;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::DstackBackend;

#[tokio::test]
async fn logs_stream_replays_tail_then_follows() {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path().join("workflow_logs/docker_server");
    std::fs::create_dir_all(&dir).unwrap();
    let logfile = dir.join("vllm_test.log");
    std::fs::write(&logfile, "boot1\nboot2\n").unwrap();

    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_log_source(repo.path())
        // fast follow tick for the test
        .with_telemetry_config(/* if a knob exists; else rely on a small default */);
    let router = app(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });

    let url = format!("ws://{addr}/logs/stream?source=container&tail=2");
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    // first two frames = replayed tail
    let f1 = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let f2 = ws.next().await.unwrap().unwrap().into_text().unwrap();
    assert_eq!(f1, "boot1");
    assert_eq!(f2, "boot2");

    // append a line; expect it to be followed
    use std::io::Write;
    let mut fh = std::fs::OpenOptions::new().append(true).open(&logfile).unwrap();
    writeln!(fh, "boot3").unwrap();
    let f3 = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await.unwrap().unwrap().unwrap().into_text().unwrap();
    assert_eq!(f3, "boot3");
}
```

(If `with_telemetry_config` is the interval knob, reuse it; otherwise use a fixed follow interval constant in the handler ~250ms and drop that line.)

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p tt-station-agentd --test logs_stream`
Expected: FAIL (route unregistered).

- [ ] **Step 3: Implement `logs_ws` + `logs_stream`**

```rust
async fn logs_ws(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let (source_str, tail) = parse_logs_query(query.as_deref());
    ws.on_upgrade(move |socket| logs_stream(socket, state, source_str, tail))
}

/// Follow-interval for the log tail poll. Short enough to feel live, long
/// enough to be cheap. (Mirrors telemetry's interval+Delay approach.)
const LOG_FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

async fn logs_stream(mut socket: WebSocket, state: AppState, source_str: String, tail: usize) {
    let source = match crate::logs::LogSource::parse(&source_str) {
        Some(s) => s,
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "error": format!("unknown source '{source_str}'") })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let repo = match state.tt_inference_repo() {
        Some(p) => p.to_path_buf(),
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "error": "logs unavailable: non-runpy backend" })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let dir = crate::logs::logs_dir(&repo, source);

    // Resolve the current newest file + replay its tail, tracking (path, offset).
    let mut cur_path: Option<std::path::PathBuf> = None;
    let mut offset: u64 = 0;

    // Replay tail synchronously on connect.
    if let Ok(Some(path)) = crate::logs::newest_log_file(&dir) {
        if let Ok(lines) = crate::logs::tail_lines(&path, tail) {
            for l in lines {
                if socket
                    .send(Message::Text(crate::logs::redact_line(&l).into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        cur_path = Some(path);
    }

    let mut ticker = tokio::time::interval(LOG_FOLLOW_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Re-resolve newest: a fresh serve creates a new timestamped file.
                let newest = crate::logs::newest_log_file(&dir).ok().flatten();
                if newest != cur_path {
                    // Rotated to a new file — replay it from the start.
                    cur_path = newest.clone();
                    offset = 0;
                }
                if let Some(path) = &cur_path {
                    match crate::logs::read_new_lines(path, offset) {
                        Ok((lines, new_off)) => {
                            offset = new_off;
                            for l in lines {
                                if socket
                                    .send(Message::Text(crate::logs::redact_line(&l).into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(_) => { /* file vanished mid-follow; re-resolve next tick */ cur_path = None; }
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
```

Register: `.route("/logs/stream", get(logs_ws))` in `app()`.

- [ ] **Step 4: Run test, verify pass**

Run: `cargo test -p tt-station-agentd --test logs_stream`
Expected: PASS (both replay and follow).

- [ ] **Step 5: Full agentd suite + clippy + fmt + commit**

Run: `cargo test -p tt-station-agentd && cargo clippy -p tt-station-agentd --all-targets -- -D warnings && cargo fmt`
Expected: green.

```bash
git add crates/tt-station-agentd/src/routes.rs crates/tt-station-agentd/tests/logs_stream.rs
git commit -m "feat(agentd): GET /logs/stream WebSocket follow (replay tail + offset-follow, rotation-aware)"
```

---

### Task 4: `tt logs` CLI command (+ mock-box `/logs`)

**Files:**
- Modify: `crates/tt/src/main.rs` (`Command::Logs` variant, dispatch arm, `cmd_logs`, `print_logs`)
- Modify: `crates/libttstation/src/agent_client.rs` (add `get_logs(base, source, tail) -> LogsInfo` unauthed helper; add `LogsInfo` model — locate the existing `get_status`/`get_serving` helpers and mirror)
- Modify: `crates/mock-box/src/...` (serve `GET /logs` returning canned lines, so the CLI e2e can exercise it)
- Test: `crates/tt/tests/e2e_mock.rs` (extend), inline unit test for `print_logs`

**Interfaces:**
- Consumes: `/logs` route (Task 2). CLI patterns: `Cli`/`Command` (`main.rs:67-187`), dispatch `match &cli.command` (`main.rs:307-420`), `cmd_status` unauthed GET (`main.rs:683-686`), `run_async` (`main.rs:426-430`), `--json` global.
- Produces: `Command::Logs { host: String, source: String, tail: usize, follow: bool }`; `libttstation::agent_client::get_logs`; `LogsInfo { source, origin: Option<String>, lines: Vec<String> }`.

- [ ] **Step 1: Add `LogsInfo` + `get_logs` in `libttstation::agent_client` with a failing unit test**

Mirror the existing unauthed `get_status`/`get_serving`. `LogsInfo` derives `Serialize, Deserialize, Debug`. `get_logs`:

```rust
pub async fn get_logs(base: &str, source: &str, tail: usize) -> anyhow::Result<LogsInfo> {
    let url = format!("{base}/logs?source={source}&tail={tail}");
    let resp = reqwest::get(&url).await?.error_for_status()?;
    Ok(resp.json().await?)
}
```

(Match the crate's actual error type / client construction used by the neighbors.)

- [ ] **Step 2: Add the `Logs` subcommand + dispatch + `cmd_logs` + `print_logs`**

Variant (`main.rs`, near `Serving`/`Status`):

```rust
    /// Tail the serving logs from the box (container or run.py logs).
    Logs {
        #[arg(long)]
        host: String,
        /// Which log stream: "container" (default) or "run".
        #[arg(long, default_value = "container")]
        source: String,
        /// How many trailing lines to fetch.
        #[arg(long, default_value_t = 200)]
        tail: usize,
        /// Stream new lines live (Ctrl-C to stop).
        #[arg(long)]
        follow: bool,
    },
```

Dispatch arm:

```rust
        Command::Logs { host, source, tail, follow } => {
            if *follow {
                run_async(cmd_logs_follow(host, source, *tail))?;
            } else {
                let logs = run_async(cmd_logs(host, source, *tail))?;
                print_logs(&logs, cli.json);
            }
        }
```

`cmd_logs` (unauthed):

```rust
async fn cmd_logs(host: &str, source: &str, tail: usize) -> Result<LogsInfo> {
    let base = format!("http://{host}");
    libttstation::agent_client::get_logs(&base, source, tail).await
}
```

`print_logs`: with `--json`, print `serde_json::to_string(&logs)`; else print `origin` as a header line (or "(no log yet)") then each line.

`cmd_logs_follow`: connect `ws://{host}/logs/stream?source={source}&tail={tail}` with `tokio_tungstenite::connect_async`, print each text frame until the stream ends / Ctrl-C. Add `tokio-tungstenite` + `futures-util` to `crates/tt/Cargo.toml` deps (they're already dev-deps in agentd; confirm/add to `tt`).

- [ ] **Step 3: Add `/logs` to mock-box**

In mock-box's request router, handle `GET /logs` → return `{"source":"container","origin":"/mock/vllm.log","lines":["mock line 1","mock line 2"]}` (respect `?tail=` loosely). Enough for the e2e assertion.

- [ ] **Step 4: Write the failing e2e + unit tests**

Extend `crates/tt/tests/e2e_mock.rs` (mirror the discover/models pattern, `e2e_mock.rs:105-124`):

```rust
    let logs_stdout = AssertCommand::cargo_bin("tt").unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "logs", "--host", &host, "--source", "container", "--tail", "50"])
        .assert().success().get_output().stdout.clone();
    let logs: serde_json::Value = serde_json::from_slice(&logs_stdout).unwrap();
    assert_eq!(logs["source"], "container");
    assert!(logs["lines"].as_array().unwrap().len() >= 1);
```

Inline unit test for `print_logs` formatting (JSON branch emits valid JSON; text branch prints each line).

- [ ] **Step 5: Run tests, verify fail then pass**

Run: `cargo test -p tt print_logs` then `cargo test -p tt --test e2e_mock -- --ignored`
Expected: unit fails→passes; e2e (with the new `/logs` mock) passes.

- [ ] **Step 6: fmt + commit**

Run: `cargo fmt`

```bash
git add crates/tt/src/main.rs crates/libttstation/src/agent_client.rs crates/mock-box crates/tt/tests/e2e_mock.rs crates/tt/Cargo.toml
git commit -m "feat(tt): tt logs [--source --tail --follow] over /logs (+ mock-box /logs)"
```

---

### Task 5: Journal breadcrumbs in `runpy.rs` (Part C)

**Files:**
- Modify: `crates/tt-station-agentd/src/serving/runpy.rs` (parse run.py stdout for artifacts; `eprintln!` breadcrumbs after launch; tail container log into journal on health-poll failure)
- Test: inline `#[cfg(test)]` in `runpy.rs`

**Interfaces:**
- Consumes: `run_in_dir_with_env` returns run.py's captured stdout (`runpy.rs:708-713`); the health-poll loop (`start`, `runpy.rs:749-795`); `crate::logs::tail_lines` (Task 1). run.py stdout contains (from the evidence): `Created Docker container ID: <id>`, `Docker logs are also streamed to log file: <path>`, `This log file is saved on local machine at: <path>`.
- Produces: `fn parse_run_artifacts(stdout: &str) -> RunArtifacts` (pure) with `RunArtifacts { container_id: Option<String>, container_log: Option<String>, run_log: Option<String> }`.

- [ ] **Step 1: Write failing test for `parse_run_artifacts`**

```rust
#[test]
fn parses_container_id_and_log_paths_from_runpy_stdout() {
    let out = "\
2026-07-07 13:52:50 - run_docker_server.py:352 - INFO: Created Docker container ID: 5d2dd4b5c9d9
2026-07-07 13:52:50 - run_docker_server.py:354 - INFO: Docker logs are also streamed to log file: /home/ttuser/code/tt-inference-server/workflow_logs/docker_server/vllm_x.log
2026-07-07 13:52:50 - run.py:731 - INFO: This log file is saved on local machine at: /home/ttuser/code/tt-inference-server/workflow_logs/run_logs/run_x.log";
    let a = parse_run_artifacts(out);
    assert_eq!(a.container_id.as_deref(), Some("5d2dd4b5c9d9"));
    assert!(a.container_log.as_deref().unwrap().ends_with("docker_server/vllm_x.log"));
    assert!(a.run_log.as_deref().unwrap().ends_with("run_logs/run_x.log"));
}

#[test]
fn parse_run_artifacts_tolerates_missing_fields() {
    let a = parse_run_artifacts("nothing useful here");
    assert!(a.container_id.is_none() && a.container_log.is_none() && a.run_log.is_none());
}
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p tt-station-agentd parse_run_artifacts`
Expected: FAIL (undefined).

- [ ] **Step 3: Implement `parse_run_artifacts`**

```rust
#[derive(Debug, Default)]
struct RunArtifacts {
    container_id: Option<String>,
    container_log: Option<String>,
    run_log: Option<String>,
}

/// Extract the container id + log paths run.py prints on a successful launch.
/// All fields optional — run.py output format may drift; missing = None.
fn parse_run_artifacts(stdout: &str) -> RunArtifacts {
    let mut a = RunArtifacts::default();
    for line in stdout.lines() {
        if let Some(rest) = line.split("Created Docker container ID:").nth(1) {
            a.container_id = Some(rest.trim().to_string());
        } else if let Some(rest) = line.split("Docker logs are also streamed to log file:").nth(1) {
            a.container_log = Some(rest.trim().to_string());
        } else if let Some(rest) = line.split("This log file is saved on local machine at:").nth(1) {
            a.run_log = Some(rest.trim().to_string());
        }
    }
    a
}
```

- [ ] **Step 4: Run test, verify pass**

Run: `cargo test -p tt-station-agentd parse_run_artifacts`
Expected: PASS.

- [ ] **Step 5: Emit breadcrumbs after launch + tail-on-failure**

After the `run_in_dir_with_env(...)` call in `start` captures run.py's stdout, parse it and log:

```rust
    let run_stdout = self.runner.run_in_dir_with_env(&self.config.repo_dir, &arg_refs, &[...])?;
    let artifacts = parse_run_artifacts(&run_stdout);
    if let Some(id) = &artifacts.container_id {
        eprintln!("tt-station-agentd: serving container id: {id} (docker logs -f {id})");
    }
    if let Some(p) = &artifacts.container_log {
        eprintln!("tt-station-agentd: container log: {p}");
    }
    if let Some(p) = &artifacts.run_log {
        eprintln!("tt-station-agentd: run.py log: {p}");
    }
```

In the health-poll failure path (where `start` returns the "never became ready" error, `runpy.rs:749-795`), before returning the error, best-effort tail the container log into the journal:

```rust
    // On failure, surface the container's own tail so `journalctl` explains why.
    if let Some(p) = &artifacts.container_log {
        if let Ok(lines) = crate::logs::tail_lines(std::path::Path::new(p), 20) {
            eprintln!("tt-station-agentd: last {} lines of container log ({p}):", lines.len());
            for l in lines {
                eprintln!("tt-station-agentd:   {}", crate::logs::redact_line(&l));
            }
        }
    }
```

(Thread `artifacts` into scope of the failure branch — capture it before the poll loop.)

- [ ] **Step 6: Build, full agentd suite, fmt, commit**

Run: `cargo test -p tt-station-agentd && cargo fmt`
Expected: green.

```bash
git add crates/tt-station-agentd/src/serving/runpy.rs
git commit -m "feat(agentd): journal breadcrumbs (container id + log paths) and tail container log on serve failure"
```

---

### Task 6: `tt console` log pane (Part B)

**Files:**
- Modify: the definition of `BoxLifecycleSnapshot` (grep for `struct BoxLifecycleSnapshot` — likely `crates/tt/src/console/state.rs` or `crates/libttstation`) — add `logs: Vec<String>` (default empty; `#[serde(default)]`)
- Modify: `crates/tt/src/console/env.rs` (`collect_snapshot` fetches `/logs?source=container&tail=<N>` via `env.http_get`, parses `LogsInfo`, fills `logs`; degrade to `vec![]` on any failure)
- Modify: `crates/tt/src/console/ui.rs` (add `log_lines(snap) -> Vec<String>` builder, a `Constraint`, and a `render_panel(frame, chunks[N], "logs", &log_lines(snap))` call in `draw`)
- Modify: `docs/reference/tt-console.md` (document the new `logs` field in the `--snapshot` JSON contract + the log pane)
- Test: inline in `ui.rs` (`log_lines` pure test + the `TestBackend` render test already covers the new pane) and `env.rs` (`FakeEnv` canned `/logs`)

**Interfaces:**
- Consumes: `/logs` route (Task 2); `LifecycleEnv::http_get` (`env.rs:30-60`), `collect_snapshot` (`env.rs:178-240`), `draw`/`render_panel` (`ui.rs:194-228`), `LogsInfo` (Task 4, from libttstation — reuse rather than redefine).
- Produces: `BoxLifecycleSnapshot.logs: Vec<String>`; `log_lines`.

- [ ] **Step 1: Add `logs` field + failing snapshot test**

Add `#[serde(default)] pub logs: Vec<String>,` to `BoxLifecycleSnapshot`; set `logs: vec![]` in every constructor/test fixture (grep for the struct literal). Add to `crates/tt/tests/console_snapshot.rs` (or the inline env test) an assertion that a `FakeEnv` returning a canned `/logs` body populates `snap.logs`.

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p tt console`
Expected: FAIL (field missing / not populated).

- [ ] **Step 3: Populate `logs` in `collect_snapshot`**

Mirror the `serving`/`config` fetches (`env.rs:208-217`):

```rust
    let logs = env
        .http_get("/logs?source=container&tail=20")
        .ok()
        .and_then(|body| serde_json::from_str::<libttstation::agent_client::LogsInfo>(&body).ok())
        .map(|l| l.lines)
        .unwrap_or_default();
```

Set `logs` in the returned `BoxLifecycleSnapshot`.

- [ ] **Step 4: Add `log_lines` + the pane in `draw`**

```rust
/// Last serving-log lines for the console log pane. Placeholder when empty.
fn log_lines(snap: &BoxLifecycleSnapshot) -> Vec<String> {
    if snap.logs.is_empty() {
        vec!["(no serving log yet)".to_string()]
    } else {
        snap.logs.clone()
    }
}
```

In `draw` (`ui.rs:194-212`), add a `Constraint::Min(4)` for the log pane (adjust the serving pane to `Constraint::Length(...)` or share `Min` sensibly so both grow) and:

```rust
    render_panel(frame, chunks[N], "logs", &log_lines(snap));
```

Keep it auto-tail (show the last lines that fit); manual scroll is a documented follow-up. Add a `log_lines` unit test (empty → placeholder; non-empty → passthrough).

- [ ] **Step 5: Update the `--snapshot` JSON contract doc**

In `docs/reference/tt-console.md`, add the `logs: string[]` field to the `BoxLifecycleSnapshot` schema section and note the log pane + its source (`/logs?source=container`).

- [ ] **Step 6: Run tests, verify pass; fmt; commit**

Run: `cargo test -p tt console && cargo fmt`
Expected: green (incl. the `TestBackend` render test now drawing the log pane).

```bash
git add crates/tt/src/console crates/tt/tests/console_snapshot.rs docs/reference/tt-console.md
# also the BoxLifecycleSnapshot definition file if it lives elsewhere
git commit -m "feat(tt console): serving-log pane sourced from /logs; logs[] added to snapshot"
```

---

### Task 7: Docs + agent route reference + CLAUDE.md

**Files:**
- Create: `docs/reference/logs.md` (the `/logs` + `/logs/stream` contract, `tt logs`, the console pane, and the "why" — the container-log visibility gap)
- Modify: `docs/reference/agentd-config.md` or the agent route list doc (add `/logs`, `/logs/stream` to the unauthed-reads list)
- Modify: `CLAUDE.md` (add log-viewing to the shipped-state map under Agent + CLI)
- Modify: `macos/README.md` (a short "View logs" note pointing at the fast-follow brief — option E)

**Interfaces:** none (docs only).

- [ ] **Step 1: Write `docs/reference/logs.md`**

Cover: the two sources (container = `docker_server/*.log`, run = `run_logs/*.log`) and why container is where failures live; the routes (`GET /logs?source=&tail=`, `GET /logs/stream?...`, unauthed, `LogsResponse` shape, `409` when non-runpy); `tt logs [--source --tail --follow]`; the console pane; redaction note; and the fast-follow list (external-container `docker logs` fallback, structured serve-phase in `/status`, macOS "View logs" button, console manual scroll).

- [ ] **Step 2: Update the route list + CLAUDE.md + macOS README**

Add `/logs` + `/logs/stream` to the agent's documented unauthed routes; add a one-paragraph "Log viewing" bullet to CLAUDE.md's shipped-state; add the macOS note.

- [ ] **Step 3: Commit**

```bash
git add docs/reference/logs.md docs/reference/agentd-config.md CLAUDE.md macos/README.md
git commit -m "docs: log-viewing (/logs, tt logs, console pane) reference + state map"
```

---

## Self-Review

**Spec coverage:** A (`/logs` plain) → Task 2; A (WS follow) → Task 3; B (`tt logs`) → Task 4; B (console pane) → Task 6; C (journal breadcrumbs) → Task 5; redaction → Task 1 (`redact_line`) used in Tasks 2/3/5; docs → Task 7. All spec sections covered.

**Placeholder scan:** every code step carries real code; test steps carry real assertions; commands are concrete. The one soft spot — the exact file/symbol paths for `DstackBackend` import (Task 2/3 tests), the `BoxLifecycleSnapshot` definition location (Task 6), and the `main.rs` `AppState`-construction site (Task 2 Step 6) — are called out explicitly as "grep/locate" because they weren't pinned in the interface map; the implementer resolves them from the named neighbors.

**Type consistency:** `LogSource`, `logs_dir`, `newest_log_file`, `tail_lines`, `read_new_lines`, `redact_line`, `DEFAULT_TAIL`, `MAX_TAIL` (Task 1) are used verbatim in Tasks 2/3/5. `LogsResponse` (agent, Task 2) vs `LogsInfo` (client/CLI, Task 4) are intentionally distinct types on either side of the wire with the same JSON shape (`source`, `origin`, `lines`); `LogsInfo` is reused by the console (Task 6) rather than redefined. `parse_logs_query` (Task 2) reused by Task 3.
