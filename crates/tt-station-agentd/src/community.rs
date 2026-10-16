// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Community models: tt-model packages from Tenstorrent's community catalog
//! on Hugging Face. Each is a self-contained container (its model code, vLLM
//! and the Tenstorrent plugin inside), so it runs on any box with the right
//! chips, whatever tt-metal the box has.
//!
//! Driven through the `tt-model` CLI (tt-model-manager), which owns the
//! format: `search` the catalog, `info` a package (its manifest and whether it
//! fits this box), `pull` it with its weights, `serve` it on the chips.
//!
//! - `GET /community`: packages on the box, downloads in progress.
//! - `GET /community/catalog`: the catalog for Blackhole, with a fit guess.
//! - `GET /community/info/{ns}/{name}`: one package in detail (size, chips, access).
//! - `POST /community/pull`, `POST /community/serve`, `DELETE /community/{ns}/{name}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// This box: 2× p300c, 4 Blackhole chips.
const BOX_CHIPS: u64 = 4;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

fn tt_model() -> Option<PathBuf> {
    let p = home().join(".local/bin/tt-model");
    p.is_file().then_some(p)
}

fn need_tt_model() -> Result<PathBuf, (StatusCode, Json<Value>)> {
    tt_model().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "tt-model (tt-model-manager) isn't installed on this box.",
        )
    })
}

/// The manifest `tt-model pull` keeps for a package
/// (`~/.cache/tt-model/pulled/<org>__<name>/`; `tt-kernel` on a box from before the rename).
fn pulled_manifest(id: &str) -> Option<Value> {
    let cache = home().join(".cache");
    let root = [cache.join("tt-model"), cache.join("tt-kernel")]
        .into_iter()
        .find(|d| d.is_dir())?;
    let bytes = std::fs::read(
        root.join("pulled")
            .join(id.replacen('/', "__", 1))
            .join("tt_kernel_manifest.json"),
    )
    .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// vLLM flags TT Station adds to a package's own, after its id on `tt-model
/// serve` (which hands what it doesn't know on to `vllm serve`):
///
/// - `--enable-prompt-tokens-details`, so a reply's usage says how much of the
///   prompt came from the prefix cache (`prompt_tokens_details.cached_tokens`,
///   which the Code speed record reads). vLLM has had it since 0.6.4 (#10174,
///   November 2024), before the platform-plugin API (#11602) that every
///   `vllm-plugin` package needs, so all of those take it.
///
/// Only for `kind: vllm-plugin`: a `tt-dit-server` package isn't vLLM, and a
/// `vllm-fork` one doesn't pass loose flags on to vLLM. Nothing for a package
/// whose manifest can't be read.
pub fn extra_vllm_args(manifest: Option<&Value>) -> Vec<String> {
    match manifest.map(|m| &m["container"]["kind"]) {
        Some(k) if k == "vllm-plugin" => vec!["--enable-prompt-tokens-details".into()],
        _ => vec![],
    }
}

/// `org/name` with the characters Hugging Face allows, nothing else.
fn valid_id(id: &str) -> bool {
    let mut parts = id.split('/');
    let ok = |p: Option<&str>| {
        p.is_some_and(|p| {
            !p.is_empty()
                && p.len() <= 96
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none() && !id.contains("..")
}

fn slug(id: &str) -> String {
    id.replace('/', "__")
}

fn work_dir(state: &AppState, id: &str) -> PathBuf {
    state.config_dir().join("community").join(slug(id))
}

fn unit(id: &str) -> String {
    format!(
        "tt-station-pull-{}",
        slug(id).replace('.', "-").to_lowercase()
    )
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The newest non-empty line of a log, as a person would read it.
/// Why a start failed, from tt-model's output: the error inside its framed
/// report ("│  Log · ERROR: RuntimeError: …"), not the frame's bottom border.
fn failure_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let lines: Vec<String> = text
        .lines()
        .flat_map(|l| l.split('\r'))
        .map(|l| {
            strip_ansi(l)
                .trim()
                .trim_matches(|c: char| "│╭╮╰╯─ ".contains(c))
                .trim()
                .to_string()
        })
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .rev()
        .find(|l| l.to_lowercase().contains("error"))
        .or_else(|| lines.last())
        .map(|l| l.trim_start_matches("Log ·").trim().to_string())
}

fn last_line(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .flat_map(|l| l.split('\r'))
        .map(|l| strip_ansi(l).trim().to_string())
        .rfind(|l| !l.is_empty())
}

/// Will a package needing `chips` chips described as `hardware` run here?
pub fn fits(chips: Option<u64>, hardware: &str) -> (bool, String) {
    let h = hardware.to_lowercase();
    if h.contains("galaxy")
        || h.contains("t3k")
        || h.contains("tg")
        || h.contains("wormhole")
        || h.starts_with("n150")
        || h.starts_with("n300")
    {
        return (false, format!("Made for {hardware}, not this box."));
    }
    match chips {
        Some(n) if n > BOX_CHIPS => (false, format!("Needs {n} chips; this box has {BOX_CHIPS}.")),
        Some(n) if n == BOX_CHIPS => (true, "Uses all 4 chips".into()),
        Some(1) => (true, "Uses 1 chip".into()),
        Some(n) => (true, format!("Uses {n} chips")),
        None => (true, String::new()),
    }
}

/// A fit guess from a package's name alone (the catalog listing has no more).
fn fit_from_name(id: &str) -> (Option<bool>, String) {
    let n = id.to_lowercase();
    for (tag, chips) in [
        ("p300x2", 4),
        ("p150x4", 4),
        ("qb2", 4),
        ("blackholex4", 4),
        ("p150x2", 2),
        ("p300", 2),
        ("p150", 1),
    ] {
        if n.contains(tag) {
            let (ok, label) = fits(Some(chips), tag);
            return (Some(ok), label);
        }
    }
    if n.contains("galaxy") {
        return (Some(false), "Made for a Galaxy, not this box.".into());
    }
    (None, String::new())
}

/// Packages on the box, from tt-model's own index.
fn installed() -> Vec<Value> {
    let db: Value = std::fs::read(home().join(".cache/tt-model/installed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut out = Vec::new();
    for (id, e) in db.as_object().into_iter().flatten() {
        let manifest: Value = e["manifest"]
            .as_str()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let serve = &manifest["container"]["serve"];
        let hardware = serve["hardware"].as_str().unwrap_or("").to_string();
        let chips = manifest["device_count"].as_u64();
        let (ok, fit) = fits(chips, &hardware);
        out.push(json!({
            "id": id, "name": id.rsplit('/').next().unwrap_or(id),
            "container": e["container"].as_bool().unwrap_or(false),
            "profile": e["profile"], "hardware": hardware, "chips": chips,
            "fits": ok, "fit": fit,
            "weights": manifest["weights"]["repo_id"],
            "tools": serve["capabilities"]["tool_parser"].is_string(),
            "max_num_seqs": serve["max_num_seqs"],
        }));
    }
    out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    out
}

/// `GET /community`.
pub(crate) async fn get_community(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let st = state.clone();
    Json(
        tokio::task::spawn_blocking(move || {
            let mut on_box = installed();
            for p in &mut on_box {
                let id = p["id"].as_str().unwrap_or("").to_string();
                p["check"] = std::fs::read(work_dir(&st, &id).join("check.json"))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or(Value::Null);
            }
            // Downloads: one folder per package TT Station has pulled.
            let mut pulls = Vec::new();
            for e in std::fs::read_dir(st.config_dir().join("community")).into_iter().flatten().flatten() {
                let id = e.file_name().to_string_lossy().replacen("__", "/", 1);
                let dir = e.path();
                let running = unit_active(&unit(&id));
                let exit = std::fs::read_to_string(dir.join("pull.exit")).ok().map(|t| t.trim().to_string());
                // Stopped before it finished, by Pause or by the box shutting down:
                // it never wrote how it ended.
                let paused = !running
                    && exit.is_none()
                    && (dir.join("paused").exists() || std::fs::metadata(dir.join("pull.log")).is_ok_and(|m| m.len() > 0));
                // Paused, but everything arrived anyway (another package fetched the same weights).
                let finished = paused
                    && on_box.iter().any(|p| p["id"] == id.as_str()
                        && p["weights"].as_str().is_none_or(|w| crate::native_vllm::weights_complete(w).0));
                if finished {
                    for f in ["pull.log", "paused"] {
                        let _ = std::fs::remove_file(dir.join(f));
                    }
                    continue;
                }
                if running || paused || exit.as_deref().is_some_and(|e| e != "0") {
                    let progress = if running {
                        pull_progress(&id, &dir.join("pull.log"))
                    } else if paused {
                        let mut p = pull_progress(&id, &dir.join("pull.log"));
                        p["bytes_per_s"] = Value::Null;
                        p["eta_s"] = Value::Null;
                        p
                    } else {
                        Value::Null
                    };
                    pulls.push(json!({
                        "id": id, "running": running, "failed": !running && exit.is_some(),
                        "paused": paused,
                        // Paused here, rather than stopped by a shutdown or a crash.
                        "paused_by_you": paused && dir.join("paused").exists(),
                        "line": last_line(&dir.join("pull.log")),
                        "progress": progress,
                    }));
                }
            }
            let life = st.lifecycle();
            json!({
                "available": tt_model().is_some(),
                "installed": on_box,
                "pulls": pulls,
                // Only one model serves at a time, and starting any other stops
                // this one (clearing the record), so a record plus a live model is ours.
                "serving": current(&st, &life),
                // A package starting: its newest boot step, for the card.
                "starting": starting(&st, &life),
                "stopping": mine(&state).stopping.lock().expect("community").clone().map(|(id, since)| json!({ "id": id, "since": since })),
                // The last start that failed (until the next one), for its card.
                "failed": preparing(&state).and_then(|p| p.error.map(|e| json!({ "id": p.id, "error": e }))),
            })
        })
        .await
        .unwrap_or_else(|_| json!({ "installed": [], "pulls": [] })),
    )
}

/// A package starting: since the click, and what it's doing (the agent's own
/// steps first, then the newest line `tt-model serve` printed).
fn starting(state: &AppState, life: &crate::routes::Lifecycle) -> Option<Value> {
    let prep = preparing(state).filter(|p| p.error.is_none());
    if let Some(p) = prep.as_ref().filter(|p| !p.launched) {
        return Some(json!({ "id": p.id, "since": p.since, "step": p.step }));
    }
    let (id, _) = serving(state).filter(|_| matches!(life.phase, Phase::Starting))?;
    let since = prep.filter(|p| p.id == id).map_or(life.since, |p| p.since);
    let step = last_line(&work_dir(state, &id).join("serve.log"))
        .unwrap_or_else(|| "Starting the container".into());
    Some(json!({ "id": id, "since": since, "step": step, "stalled_s": stalled_s(state) }))
}

/// A fetched list: when it was fetched (as an instant and in Unix seconds), and its items.
type Fetched = (Instant, u64, Vec<Value>);

/// Fetched lists, by scope (`catalog` | `all`): when, and what.
static CATALOG: std::sync::LazyLock<Mutex<HashMap<bool, Fetched>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// A community package (from the last catalog listings fetched) whose name is
/// this model's: someone has already packaged it to run here.
pub fn package_for(model: &str) -> Option<String> {
    let norm = |s: &str| {
        s.rsplit('/')
            .next()
            .unwrap_or(s)
            .to_ascii_lowercase()
            .replace(['_', '.'], "-")
    };
    let want = norm(model);
    let lists = CATALOG.lock().expect("community");
    lists
        .values()
        .flat_map(|(_, _, list)| list.iter())
        .filter_map(|m| m["id"].as_str())
        .find(|id| {
            let have = norm(id);
            have == want
                || have
                    .strip_suffix("-blackhole")
                    .or_else(|| have.strip_suffix("-p150"))
                    .is_some_and(|h| h == want)
        })
        .map(str::to_string)
}

#[derive(Debug, Deserialize, Default)]
pub struct CatalogQuery {
    /// Every published package, not just the curated community catalog.
    #[serde(default)]
    pub all: bool,
    /// Ask Hugging Face again instead of using the last list.
    #[serde(default)]
    pub refresh: bool,
}

/// `GET /community/catalog[?all=true][&refresh=true]`: community packages for
/// Blackhole, from Hugging Face via `tt-model search` (kept 10 minutes).
pub(crate) async fn get_catalog(
    _auth: BearerAuth,
    axum::extract::Query(q): axum::extract::Query<CatalogQuery>,
) -> Reply {
    let bin = need_tt_model()?;
    if !q.refresh {
        if let Some((at, fetched, items)) = CATALOG.lock().expect("catalog").get(&q.all).cloned() {
            if at.elapsed() < Duration::from_secs(600) {
                return Ok(Json(
                    json!({ "models": items, "fetched_at": fetched, "all": q.all }),
                ));
            }
        }
    }
    let all = q.all;
    let out = tokio::task::spawn_blocking(move || {
        let mut args = vec!["search", "--arch", "blackhole", "--limit", "500", "--json"];
        if !all {
            args.insert(1, "--catalog");
        }
        Command::new(bin).args(&args).stdin(Stdio::null()).output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let list: Vec<Value> = serde_json::from_slice(&out.stdout).map_err(|_| {
        err(
            StatusCode::BAD_GATEWAY,
            "Couldn't read the community catalog (is the box online?).",
        )
    })?;
    let items: Vec<Value> = list
        .into_iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let (ok, fit) = fit_from_name(&id);
            Some(json!({
                "id": id, "name": id.rsplit('/').next().unwrap_or(&id), "author": id.split('/').next().unwrap_or(""),
                "updated": m["last_modified"], "fits": ok, "fit": fit,
            }))
        })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    CATALOG
        .lock()
        .expect("catalog")
        .insert(all, (Instant::now(), now, items.clone()));
    if q.refresh {
        // Details too: a package may have been re-published since.
        INFO.lock().expect("info").clear();
    }
    Ok(Json(
        json!({ "models": items, "fetched_at": now, "all": all }),
    ))
}

static INFO: std::sync::LazyLock<Mutex<HashMap<String, (Instant, Value)>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The JSON manifest `tt-model info` prints, and its verdict line.
fn parse_info(text: &str) -> (Option<Value>, Option<(bool, String)>) {
    let clean = strip_ansi(text);
    let manifest = clean.find('{').and_then(|start| {
        let mut depth = 0i32;
        let mut in_str = false;
        let mut escaped = false;
        for (i, c) in clean[start..].char_indices() {
            match c {
                _ if escaped => escaped = false,
                '\\' if in_str => escaped = true,
                '"' => in_str = !in_str,
                '{' if !in_str => depth += 1,
                '}' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        return serde_json::from_str(&clean[start..start + i + 1]).ok();
                    }
                }
                _ => {}
            }
        }
        None
    });
    let verdict = clean
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.starts_with('✓') || l.starts_with('✗') || l.starts_with('!'))
        .map(|l| {
            (
                l.starts_with('✓'),
                l.trim_start_matches(['✓', '✗', '!']).trim().to_string(),
            )
        });
    (manifest, verdict)
}

/// Sum of a Hugging Face repo's file sizes (bytes), or None.
async fn repo_bytes(client: &reqwest::Client, repo: &str, token: Option<&str>) -> Option<u64> {
    let mut req = client
        .get(format!(
            "https://huggingface.co/api/models/{repo}?blobs=true"
        ))
        .timeout(Duration::from_secs(15));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let v: Value = req.send().await.ok()?.json().await.ok()?;
    Some(
        v["siblings"]
            .as_array()?
            .iter()
            .filter_map(|s| s["size"].as_u64())
            .sum(),
    )
}

/// Can this box's Hugging Face token download `repo`? (Gated models need the
/// license accepted on the website first.) None when it can't tell.
async fn weights_access(
    client: &reqwest::Client,
    repo: &str,
    token: Option<&str>,
) -> (bool, Option<bool>) {
    let meta: Value = match client
        .get(format!("https://huggingface.co/api/models/{repo}"))
        .timeout(Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or(Value::Null),
        Err(_) => return (false, None),
    };
    let gated = !matches!(meta["gated"], Value::Bool(false) | Value::Null);
    if !gated {
        return (false, Some(true));
    }
    let Some(t) = token else {
        return (true, Some(false));
    };
    let ok = client
        .head(format!(
            "https://huggingface.co/{repo}/resolve/main/config.json"
        ))
        .bearer_auth(t)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map(|r| r.status().is_success() || r.status().is_redirection())
        .ok();
    (true, ok)
}

/// Free space where Hugging Face weights and Docker images land (the home disk).
fn disk_free() -> Option<u64> {
    let out = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(home())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

/// Where a package download is: `tt-model pull --with-weights` fetches the
/// package into a temporary folder, loads its container image into Docker, then
/// fetches the weights into the Hugging Face cache. Its log says which step it's on.
fn pull_step(log: &str) -> (&'static str, Option<String>) {
    let lines: Vec<String> = log
        .lines()
        .flat_map(|l| l.split('\r'))
        .map(|l| strip_ansi(l).trim().to_string())
        .collect();
    let weights = lines.iter().find_map(|l| {
        let rest = l
            .strip_prefix("weights ")
            .or_else(|| l.strip_prefix("✓ weights "))?;
        Some(rest.split(['@', ' ']).next()?.to_string()).filter(|r| r.contains('/'))
    });
    let step = if lines.iter().any(|l| l.starts_with("✓ weights ")) {
        "finishing"
    } else if weights.is_some() {
        "weights"
    } else if lines.iter().any(|l| l.starts_with("docker load")) {
        "image"
    } else {
        "package"
    };
    (step, weights)
}

/// The temporary folder a package is downloading into right now, if any.
fn package_download_dir() -> Option<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .ok()?
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("tmp")
                && e.path().join(".cache/huggingface").is_dir()
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .filter(|(t, _)| t.elapsed().is_ok_and(|d| d < Duration::from_secs(6 * 3600)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// A running package download: its step, and bytes against the whole (the package plus its weights).
fn pull_progress(id: &str, log: &Path) -> Value {
    let (step, from_log) = pull_step(&std::fs::read_to_string(log).unwrap_or_default());
    let details = INFO.lock().expect("info").get(id).map(|(_, v)| v.clone());
    if details.is_none() {
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(info(id.to_string()));
        }
    }
    let weights = from_log.or_else(|| {
        details
            .as_ref()
            .and_then(|d| d["weights"].as_str().map(str::to_string))
    });
    let package_total = crate::download::repo_total(id);
    let weights_total = weights.as_deref().and_then(crate::download::repo_total);
    let package_done = match step {
        "package" => package_download_dir()
            .map(|d| crate::download::dir_bytes(&d))
            .unwrap_or(0),
        _ => package_total.unwrap_or(0),
    };
    let weights_done = weights
        .as_deref()
        .map(|w| crate::download::repo_on_disk(&crate::download::hub_dir(), w))
        .unwrap_or(0);
    // A package without weights of its own has nothing more to fetch.
    let no_weights = details.as_ref().is_some_and(|d| d["weights"].is_null());
    let total = match (package_total, weights_total) {
        (Some(p), Some(w)) => Some(p + w),
        (Some(p), None) if no_weights => Some(p),
        _ => None,
    };
    let mut p = crate::download::progress(
        &format!("community:{id}"),
        package_done + weights_done,
        total,
    );
    p["step"] = json!(step);
    p["weights"] = json!(weights);
    p
}

/// `GET /community/info/{ns}/{name}`: one package, in a person's terms.
pub(crate) async fn get_info(
    _auth: BearerAuth,
    UrlPath((ns, name)): UrlPath<(String, String)>,
) -> Reply {
    info(format!("{ns}/{name}")).await
}

/// One package's details (cached ten minutes).
async fn info(id: String) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    if let Some((at, v)) = INFO.lock().expect("info").get(&id).cloned() {
        if at.elapsed() < Duration::from_secs(600) {
            return Ok(Json(v));
        }
    }
    let bin = need_tt_model()?;
    let i = id.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(bin)
            .args(["info", &i])
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let (manifest, verdict) = parse_info(&text);
    let Some(m) = manifest else {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!(
                "Couldn't read {id}'s details: {}",
                text.lines().last().unwrap_or("no answer")
            ),
        ));
    };
    let serve = &m["container"]["serve"];
    let hardware = serve["hardware"].as_str().unwrap_or("").to_string();
    let chips = m["device_count"].as_u64();
    let (ok, fit) = fits(chips, &hardware);
    let weights = m["weights"]["repo_id"].as_str().map(str::to_string);
    let token = crate::bringup::hf_token();
    let client = reqwest::Client::new();
    let package_bytes = repo_bytes(&client, &id, token.as_deref()).await;
    let (weights_bytes, (gated, access)) = match &weights {
        Some(w) => (
            repo_bytes(&client, w, token.as_deref()).await,
            weights_access(&client, w, token.as_deref()).await,
        ),
        None => (None, (false, Some(true))),
    };
    let v = json!({
        "id": id, "container": m["container"].is_object(),
        "hardware": hardware, "chips": chips, "fits": ok, "fit": fit,
        "compatible": verdict.as_ref().map(|(ok, _)| *ok), "verdict": verdict.map(|(_, t)| t),
        "weights": weights, "gated": gated, "weights_access": access, "has_token": token.is_some(),
        // What a download costs: the package, plus the weights unless they're already here.
        "package_bytes": package_bytes,
        "weights_bytes": weights_bytes,
        "weights_on_box": weights.as_deref().is_some_and(|w| crate::native_vllm::weights_complete(w).0),
        "disk_free_bytes": disk_free(),
        "tools": serve["capabilities"]["tool_parser"].is_string(),
        "reasoning": serve["capabilities"]["reasoning_parser"].is_string(),
        "max_model_len": serve["max_model_len"], "max_num_seqs": serve["max_num_seqs"],
        "built": m["container"]["built"]["created_at"],
    });
    if let Some(n) = package_bytes {
        crate::download::know_total(&id, n);
    }
    if let (Some(w), Some(n)) = (&weights, weights_bytes) {
        crate::download::know_total(w, n);
    }
    INFO.lock()
        .expect("info")
        .insert(id, (Instant::now(), v.clone()));
    Ok(Json(v))
}

#[derive(Debug, Deserialize)]
pub struct IdBody {
    pub id: String,
    #[serde(default)]
    pub stop_serving: bool,
    /// Start even though its weights aren't all on the box (they download inside the model).
    #[serde(default)]
    pub anyway: bool,
    /// The serve profile to start (`tt-model serve --profile`); none for the box's pick.
    #[serde(default)]
    pub profile: Option<String>,
}

/// `POST /community/pull`: download a package and its weights, in the background.
pub(crate) async fn post_pull(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<IdBody>,
) -> Reply {
    if !valid_id(&body.id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    let bin = need_tt_model()?;
    let u = unit(&body.id);
    if unit_active(&u) {
        return Ok(Json(json!({ "pulling": true })));
    }
    // Room for it? Checked before anything starts, from its details (cached
    // when the app just showed them). Unknown sizes or free space refuse nothing.
    if let Ok(Json(details)) = info(body.id.clone()).await {
        let here = details["weights"].as_str().map_or(0, |w| {
            crate::download::repo_on_disk(&crate::download::hub_dir(), w)
        });
        if let (Some(need), Some(free)) =
            (crate::download::package_need(&details, here), disk_free())
        {
            let name = body.id.rsplit('/').next().unwrap_or(&body.id);
            crate::download::check_room(name, need, free)
                .map_err(|e| err(StatusCode::CONFLICT, e))?;
        }
    }
    let dir = work_dir(&state, &body.id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (log, exit) = (dir.join("pull.log"), dir.join("pull.exit"));
    let _ = std::fs::write(&log, "");
    let _ = std::fs::remove_file(&exit);
    let _ = std::fs::remove_file(dir.join("paused"));
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let script = format!(
        "{} pull {} --with-weights; echo $? > {}",
        q(&bin.display().to_string()),
        q(&body.id),
        q(&exit.display().to_string())
    );
    let path = format!(
        "{}:{}",
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={u}"))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"));
    // The Hugging Face token as a private file (`HF_TOKEN_PATH`), never the
    // value in the unit's environment, where `systemctl --user show` prints it.
    cmd.args(crate::serving::secrets::launch_hf_token_args());
    let out = cmd
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
    // Its size and its weights' size, for the progress bar.
    tokio::spawn(info(body.id.clone()));
    state.notifier().record(crate::notify::Draft {
        kind: "model",
        title: format!(
            "Downloading {}",
            body.id.rsplit('/').next().unwrap_or(&body.id)
        ),
        body: "A community model, with its weights. Start it from Models when it's done.".into(),
    });
    Ok(Json(json!({ "pulling": true })))
}

/// `POST /community/pull/pause {id}`: stop a download, keeping what it has. Every
/// finished file stays; a resume (`POST /community/pull`) picks up from there and
/// redoes only the files that were partway through.
pub(crate) async fn post_pause(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<IdBody>,
) -> Reply {
    if !valid_id(&body.id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    let u = unit(&body.id);
    if !unit_active(&u) {
        return Err(err(StatusCode::CONFLICT, "It isn't downloading."));
    }
    let dir = work_dir(&state, &body.id);
    let _ = std::fs::write(dir.join("paused"), now_secs().to_string());
    let out = tokio::task::spawn_blocking(move || {
        Command::new("systemctl")
            .args(["--user", "stop", &u])
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dir.join("paused"));
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(Json(json!({ "paused": true })))
}

#[derive(Debug, Deserialize)]
pub struct CancelBody {
    pub id: String,
    /// Also delete the weights it downloaded (unless another package on the box uses them).
    #[serde(default)]
    pub delete_files: bool,
}

/// `POST /community/pull/cancel {id, delete_files}`: stop a download (running or
/// paused) and forget it; with `delete_files`, free the space its weights took.
pub(crate) async fn post_cancel(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<CancelBody>,
) -> Reply {
    if !valid_id(&body.id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    let id = body.id.clone();
    let dir = work_dir(&state, &id);
    let weights = pull_step(&std::fs::read_to_string(dir.join("pull.log")).unwrap_or_default())
        .1
        .or_else(|| {
            INFO.lock()
                .expect("info")
                .get(&id)
                .and_then(|(_, v)| v["weights"].as_str().map(str::to_string))
        });
    let freed = tokio::task::spawn_blocking(move || {
        let u = unit(&id);
        if unit_active(&u) {
            let _ = Command::new("systemctl")
                .args(["--user", "stop", &u])
                .stdin(Stdio::null())
                .status();
        }
        for f in ["pull.log", "pull.exit", "paused"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
        let Some(w) = weights.filter(|_| body.delete_files) else {
            return Ok(0);
        };
        // Not if another package on the box needs the same weights.
        if installed()
            .iter()
            .any(|p| p["id"] != id.as_str() && p["weights"] == w.as_str())
        {
            return Err(
                "Another model on the box uses the same weights, so they were kept.".to_string(),
            );
        }
        Ok(delete_repo(&crate::download::hub_dir(), &w))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    match freed {
        Ok(n) => Ok(Json(json!({ "cancelled": true, "freed_bytes": n }))),
        Err(why) => Ok(Json(
            json!({ "cancelled": true, "freed_bytes": 0, "kept": why }),
        )),
    }
}

/// Deletes a repo from the Hugging Face cache, and the files it linked to in the
/// shared blob store that no other repo links to. Returns the bytes freed.
fn delete_repo(hub: &Path, repo: &str) -> u64 {
    let root = hub.join(format!("models--{}", repo.replace('/', "--")));
    let canon = |p: &Path| std::fs::canonicalize(p).ok();
    let targets = |blobs: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(blobs)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_symlink()))
            .filter_map(|e| canon(&e.path()))
            .collect()
    };
    let mine = targets(&root.join("blobs"));
    let mut others = std::collections::HashSet::new();
    for e in std::fs::read_dir(hub).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().starts_with("models--") && e.path() != root {
            others.extend(targets(&e.path().join("blobs")));
        }
    }
    let mut freed = crate::download::dir_bytes(&root);
    let _ = std::fs::remove_dir_all(&root);
    let store = canon(hub).unwrap_or_else(|| hub.to_path_buf());
    for t in mine
        .into_iter()
        .filter(|t| !others.contains(t) && t.starts_with(&store))
    {
        freed += std::fs::metadata(&t).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&t);
    }
    freed
}

/// What `crate::community` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// What a community package started: (package id, the model it serves).
    /// Kept on disk too (`community/serving.json`), so an agent restart still
    /// knows which container to stop.
    serving: Mutex<Option<(String, String)>>,
    /// The start being prepared (chips reset, old model stopped) before launch.
    prep: Mutex<Option<Prep>>,
    /// Where `recover` hands a stuck start to `restarter`, once it runs.
    restart: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<(String, String, u64)>>,
    /// The start being watched: (package, when it last showed progress).
    stall: Mutex<Option<(String, Instant)>>,
    /// A package being stopped (since when): its container shutting down, then
    /// its chips reset, which takes a few minutes.
    stopping: Mutex<Option<(String, u64)>>,
    /// When a stop last reset every board (by releasing their leases).
    just_reset: Mutex<Option<Instant>>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().community
}

fn serving_file(state: &AppState) -> PathBuf {
    state.config_dir().join("community/serving.json")
}

fn serving(state: &AppState) -> Option<(String, String)> {
    let mut held = mine(state).serving.lock().expect("community");
    if held.is_none() {
        *held = std::fs::read(serving_file(state))
            .ok()
            .and_then(|b| serde_json::from_slice::<(String, String)>(&b).ok());
    }
    held.clone()
}

/// `meta-models/Muse-Glimmer-30B` and `muse-glimmer-30b` are the same model.
fn same_model(a: &str, b: &str) -> bool {
    let short = |s: &str| s.rsplit('/').next().unwrap_or(s).to_ascii_lowercase();
    a.eq_ignore_ascii_case(b) || short(a) == short(b)
}

/// The community package serving right now: the record, but only while the
/// served model is that package's model. A record left behind by a model
/// started some other way is stale, and cleared here.
/// The community package serving (or starting) now, if one is.
pub(crate) fn serving_package(state: &AppState, life: &crate::routes::Lifecycle) -> Option<String> {
    current(state, life)
}

fn current(state: &AppState, life: &crate::routes::Lifecycle) -> Option<String> {
    let (id, model) = serving(state)?;
    match life.phase {
        Phase::Starting => {
            (life.model.as_deref().is_none_or(|m| same_model(m, &model))).then_some(id)
        }
        Phase::Serving => {
            let served = life
                .served_id
                .as_deref()
                .or(life.model.as_deref())
                .unwrap_or("");
            if same_model(served, &model) {
                Some(id)
            } else {
                set_serving(state, None);
                None
            }
        }
        _ => None,
    }
}

fn set_serving(state: &AppState, now: Option<(String, String)>) {
    let file = serving_file(state);
    match &now {
        Some(v) => {
            let _ = std::fs::create_dir_all(file.parent().unwrap_or(Path::new(".")));
            let _ = std::fs::write(&file, serde_json::to_vec(v).unwrap_or_default());
        }
        None => {
            let _ = std::fs::remove_file(&file);
        }
    }
    *mine(state).serving.lock().expect("community") = now;
}

/// `POST /community/serve`: run a downloaded package on the chips; Chat picks it up.
pub(crate) async fn post_serve(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<IdBody>,
) -> Reply {
    if !valid_id(&body.id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    let bin = need_tt_model()?;
    let Some(pkg) = installed()
        .into_iter()
        .find(|p| p["id"] == body.id.as_str())
    else {
        return Err(err(StatusCode::NOT_FOUND, "Download it first."));
    };
    if pkg["fits"] == false {
        return Err(err(
            StatusCode::CONFLICT,
            pkg["fit"]
                .as_str()
                .unwrap_or("It doesn't fit this box.")
                .to_string(),
        ));
    }
    if let Some(p) = &body.profile {
        crate::codemodels::profile_ok(&body.id, p).map_err(|e| err(StatusCode::CONFLICT, e))?;
    }
    if let Some(holder) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, holder));
    }
    // Weights only partly downloaded: tt-model would fetch the rest inside the
    // container, silently, while the chips sit idle. Download them first instead.
    if let Some(w) = pkg["weights"].as_str() {
        let (complete, have, need) = crate::native_vllm::weights_complete(w);
        if !complete && !body.anyway {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": if need > 0 { format!("Only {have} of its {need} weight files are on the box. Download the rest first (with progress), then start it: starting now would download them inside the model while the chips sit idle.") }
                             else { "Its weights aren't on the box yet. Download them first (with progress), then start it.".to_string() },
                    "needs_weights": true, "have": have, "need": need,
                })),
            ));
        }
    }
    let life = state.lifecycle();
    if (!matches!(life.phase, Phase::Idle | Phase::Failed) || crate::coserve::is_split(&state))
        && !body.stop_serving
    {
        return Err((
            StatusCode::CONFLICT,
            Json(
                json!({ "error": "Starting it replaces what's serving now. Stop it first?", "needs_stop": true }),
            ),
        ));
    }
    if let Some((id, _)) = mine(&state).stopping.lock().expect("community").clone() {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "{} is still stopping (its chips are being reset). Try again in a minute.",
                short_id(&id)
            ),
        ));
    }
    if let Some(p) = preparing(&state).filter(|p| !p.launched && p.error.is_none()) {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{} is already starting.", short_id(&p.id)),
        ));
    }
    let replace =
        !matches!(life.phase, Phase::Idle | Phase::Failed) || crate::coserve::is_split(&state);
    set_preparing(
        &state,
        Some(Prep {
            id: body.id.clone(),
            since: now_secs(),
            step: if replace {
                "Stopping the model that's serving".into()
            } else {
                "Resetting the chips".into()
            },
            launched: false,
            error: None,
            profile: body.profile.clone(),
        }),
    );
    // The rest takes a minute or more (a stop, a chip reset): answer now, and
    // let the card follow along through `GET /community`'s `starting`.
    let model = pkg["weights"].as_str().unwrap_or(&body.id).to_string();
    let short = model_short(&pkg);
    let started = body.id.clone();
    tokio::spawn(async move {
        if let Err(e) = launch(&state, &body.id, &bin, model, short, replace).await {
            eprintln!(
                "tt-station-agentd: community: {} didn't start: {e}",
                body.id
            );
            update_preparing(&state, &body.id, |p| p.error = Some(e.clone()));
            state.notifier().emit(crate::notify::Draft {
                kind: "model_failed",
                title: format!("{} didn't start", short_id(&body.id)),
                body: e,
            });
        }
    });
    Ok(Json(json!({ "starting": started })))
}

/// A start in progress, from the click until `tt-model serve` is running (and,
/// with `launched`, on through its boot), or how it failed.
#[derive(Clone, Debug)]
struct Prep {
    id: String,
    since: u64,
    step: String,
    launched: bool,
    error: Option<String>,
    /// The profile asked for (`IdBody::profile`), if one was.
    profile: Option<String>,
}

fn preparing(state: &AppState) -> Option<Prep> {
    mine(state).prep.lock().expect("community").clone()
}

fn set_preparing(state: &AppState, p: Option<Prep>) {
    *mine(state).prep.lock().expect("community") = p;
}

fn update_preparing(state: &AppState, id: &str, f: impl FnOnce(&mut Prep)) {
    if let Some(p) = mine(state)
        .prep
        .lock()
        .expect("community")
        .as_mut()
        .filter(|p| p.id == id)
    {
        f(p);
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// From `tt-model profiles <id>`: the profile to serve on this box, when the
/// package's default is made for other hardware and one of its profiles is made
/// for this box (2x p300c). None to keep the default.
fn profile_for_this_box(listing: &str) -> Option<String> {
    let mine = |hw: &str| {
        matches!(
            hw.to_ascii_lowercase().as_str(),
            "p300x2" | "qb2" | "quietbox2"
        )
    };
    // "  • c32k (default)  p150x4 · P150x4  seqs None · len -"
    let rows: Vec<(String, bool, String)> = strip_ansi(listing)
        .lines()
        .filter_map(|l| l.trim().strip_prefix('•'))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let name = words.next()?.to_string();
            let mut next = words.next()?;
            let default = next == "(default)";
            if default {
                next = words.next()?;
            }
            Some((name, default, next.to_string()))
        })
        .collect();
    if rows.iter().any(|(_, default, hw)| *default && mine(hw)) {
        return None;
    }
    let fits: Vec<&String> = rows
        .iter()
        .filter(|(_, _, hw)| mine(hw))
        .map(|(n, _, _)| n)
        .collect();
    fits.iter()
        .find(|n| n.contains("quietbox2") || n.contains("qb2"))
        .or(fits.first())
        .map(|n| n.to_string())
}

/// The starts of a package's container names (`tt-model-<name>-<profile>`). The
/// name is the package's own (its image's), which needn't match the repo's:
/// `sjettTT/qwen3.8-flash-next_p150x4` runs as `tt-model-qwen3.8-flash-next-c32k-quietbox2`.
fn container_prefixes(id: &str) -> Vec<String> {
    let db: Value = std::fs::read(home().join(".cache/tt-model/installed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut names = vec![short_id(id).to_string()];
    if let Some(n) = db[id]["image"].as_str().and_then(image_name) {
        names.push(n);
    }
    names.into_iter().map(|n| format!("tt-model-{n}")).collect()
}

/// `tt-model/qwen3.8-flash-next:6e8c55ef7f2f` → `qwen3.8-flash-next`.
fn image_name(image: &str) -> Option<String> {
    let repo = image.rsplit_once(':').map_or(image, |(r, _)| r);
    let name = repo.rsplit('/').next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// Is the package's container running?
async fn container_up(id: &str) -> bool {
    let prefixes = container_prefixes(id);
    tokio::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .await
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| prefixes.iter().any(|p| l.starts_with(p.as_str())))
        })
        .unwrap_or(false)
}

/// Stop what's serving (if asked), reset the chips, and start `tt-model serve`
/// under the serving unit. Errors are what to tell the person.
async fn launch(
    state: &AppState,
    id: &str,
    bin: &Path,
    model: String,
    short: String,
    replace: bool,
) -> Result<(), String> {
    if replace {
        crate::coserve::leave_split(state).await;
        state
            .stop_serving()
            .await
            .map_err(|e| format!("Couldn't stop the model that was serving: {e:#}"))?;
        update_preparing(state, id, |p| p.step = "Resetting the chips".into());
    }
    // Always a full reset before a start, even right after a stop. Stopping a
    // community model resets each board as gozer releases it, but a start that
    // followed only that failed in tt-metal's device discovery
    // (`get_num_devices`: "unordered_map::at"); the same start after a full
    // `tt-smi -r` came up fine. A minute well spent.
    let _ = chips_just_reset(state);
    {
        let reset = state.clone();
        tokio::task::spawn_blocking(move || reset.run_power_command(crate::power::PowerAction::ResetChips, false))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("The chip reset (tt-smi -r) didn't work, so the model wasn't started: {e:#}. Try again; if it keeps failing, restart the box."))?;
    }
    // tt-model counts chips held by any of its containers: wait for the one that was
    // just stopped to be gone, or this start would find the chips "in use".
    for _ in 0..60 {
        let held = tokio::process::Command::new("docker")
            .args([
                "ps",
                "-q",
                "--filter",
                "label=org.tenstorrent.tt-model.devices",
            ])
            .output()
            .await
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false);
        if !held {
            break;
        }
        update_preparing(state, id, |p| {
            p.step = "Waiting for the last model to let go of the chips".into()
        });
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    // Leases from a start before this one (an agent restart, a reboot) belong to
    // nothing now: let them go, so this start's are the only ones on record.
    for lease in take_leases(state) {
        release_lease(&lease).await;
    }
    update_preparing(state, id, |p| p.step = "Starting the container".into());
    // tt-model always serves a package's default profile; a package made for several
    // boards may default to another one (sjettTT's Flash Next defaults to 4x p150).
    let profile = {
        let (bin, id) = (bin.to_path_buf(), id.to_string());
        tokio::task::spawn_blocking(move || {
            Command::new(bin)
                .args(["profiles", &id])
                .stdin(Stdio::null())
                .output()
        })
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|o| profile_for_this_box(&String::from_utf8_lossy(&o.stdout)))
    };
    let profile = preparing(state)
        .filter(|p| p.id == id)
        .and_then(|p| p.profile)
        .or(profile);
    // Small files (tokenizer, config) as real copies rather than cache links:
    // some packages' servers check their tokenizer files and refuse a link.
    {
        let weights = model.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::download::unlink_small_files(&crate::download::hub_dir(), &weights)
        })
        .await;
    }
    let dir = work_dir(state, id);
    let _ = std::fs::create_dir_all(&dir);
    let log = dir.join("serve.log");
    let _ = std::fs::write(&log, ""); // this start's lines only, so the card shows its steps
    let path = format!(
        "{}:{}",
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // In the foreground under the serving unit: stopping the unit stops the container cleanly.
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={}", crate::orchard::HARVEST_UNIT))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"));
    // No HF_TOKEN: tt-model forwards it into the container's environment
    // (`--env HF_TOKEN`), where `docker inspect` shows it for as long as the
    // model runs, and a `--local-only` serve downloads nothing. The host-side
    // tt-model gets the token as a private file (`HF_TOKEN_PATH`), which it
    // doesn't forward; a container that does want it still finds
    // `$HF_HOME/token` in the Hugging Face cache tt-model mounts.
    cmd.args(crate::serving::secrets::launch_hf_token_args());
    // With gozer: start `tt-model serve` behind a gate and lease the chips to
    // its own process before it runs (`gozer::lease_gated_unit`), instead of
    // waiting to adopt them -- which a root-owned container's chips never
    // allow, since gozer cannot see its fds.
    let gate = crate::gozer::unit_gate(state, "community");
    if let Some(fifo) = &gate {
        cmd.args(crate::gozer::gated_argv(fifo, &[]));
    }
    cmd.arg(bin).args([
        "serve",
        id,
        "--local-only",
        "--no-update-check",
        "--port",
        &state.serving_port().to_string(),
    ]);
    if let Some(p) = &profile {
        cmd.args(["--profile", p]);
    }
    // After everything tt-model reads itself: what's left goes to vLLM.
    cmd.args(extra_vllm_args(pulled_manifest(id).as_ref()));
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "Couldn't start it: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if let Some(fifo) = gate {
        let lease =
            crate::gozer::lease_started_unit(state, crate::orchard::HARVEST_UNIT, fifo, &short)
                .await
                .map_err(|why| format!("Couldn't start it: {why}"))?;
        let file = crate::gozer::LeaseFile(leases_file(state));
        file.add(&lease);
    }
    set_serving(state, Some((id.to_string(), model.clone())));
    crate::orchard::adopt_when_serving(state, state.serving_port(), model);
    update_preparing(state, id, |p| p.launched = true);
    let _ = std::fs::remove_file(dir.join("check.json"));
    tokio::spawn(adopt_chips(state.clone(), id.to_string(), short));
    tokio::spawn(check_replies(state.clone(), id.to_string()));
    tokio::spawn(watch_boot(state.clone(), id.to_string()));
    Ok(())
}

/// While it boots: if `tt-model serve` has exited and no container is left,
/// the start failed. Say so (with tt-model's last line) instead of showing
/// "Starting" for the hour `adopt_when_serving` waits.
/// A start with no sign of progress this long is reported as stuck.
const STALL_AFTER: Duration = Duration::from_secs(10 * 60);
/// ...and this long, it's stopped and the last model that worked comes back.
const RECOVER_AFTER: Duration = Duration::from_secs(20 * 60);

fn last_good_file(state: &AppState) -> PathBuf {
    state.config_dir().join("community/last_good.json")
}

/// The last community package that started and answered properly.
pub fn last_good(state: &AppState) -> Option<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(last_good_file(state)).ok()?).ok()?;
    v["id"].as_str().map(str::to_string)
}

/// Starts the last good model again after a stuck start was stopped (see `recover`).
pub async fn restarter(state: AppState) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String, u64)>();
    let _ = mine(&state).restart.set(tx);
    while let Some((stuck, back, minutes)) = rx.recv().await {
        let body = IdBody {
            id: back.clone(),
            stop_serving: true,
            anyway: false,
            profile: None,
        };
        let ok = post_serve(State(state.clone()), BearerAuth, Json(body))
            .await
            .is_ok();
        state.notifier().emit(crate::notify::Draft {
            kind: "model_failed",
            title: format!("{} got stuck starting", short_id(&stuck)),
            body: if ok {
                format!("No progress for {minutes} minutes, so it was stopped (chips reset) and {} is starting again.", short_id(&back))
            } else {
                format!("No progress for {minutes} minutes, so it was stopped and its chips reset. {} couldn't be started again; start a model in Models.", short_id(&back))
            },
        });
    }
}

/// A start stuck long enough: stop it (its chips are reset) and put the last
/// model that worked back, so the box isn't left hung with nothing to use.
async fn recover(state: &AppState, stuck: &str, minutes: u64) {
    eprintln!("tt-station-agentd: community: {stuck}: stuck for {minutes} min; stopping it and going back to the last model that worked");
    if let Err(e) = state.stop_serving().await {
        eprintln!("tt-station-agentd: community: couldn't stop {stuck}: {e:#}");
    }
    let back = last_good(state)
        .filter(|g| g != stuck && installed().iter().any(|p| p["id"] == g.as_str()));
    // Started by the restarter: a start can't start one from inside its own watch.
    if let (Some(g), Some(tx)) = (&back, mine(state).restart.get()) {
        let _ = tx.send((stuck.to_string(), g.clone(), minutes));
        return;
    }
    let restarted = false;
    state.notifier().emit(crate::notify::Draft {
        kind: "model_failed",
        title: format!("{} got stuck starting", short_id(stuck)),
        body: match (&back, restarted) {
            (Some(g), true) => format!("No progress for {minutes} minutes, so it was stopped (chips reset) and {} is starting again.", short_id(g)),
            _ => format!("No progress for {minutes} minutes, so it was stopped and its chips reset. Start a model in Models."),
        },
    });
}

/// How long the starting package has shown no progress, once that's past
/// `STALL_AFTER` (seconds); None while it's moving, or nothing is starting.
pub fn stalled_s(state: &AppState) -> Option<u64> {
    mine(state)
        .stall
        .lock()
        .expect("community")
        .as_ref()
        .map(|(_, at)| at.elapsed())
        .filter(|d| *d >= STALL_AFTER)
        .map(|d| d.as_secs())
}

/// Signs that a start is getting somewhere, which a hung one doesn't give:
/// the container printing anything but its once-a-minute "still starting"
/// heartbeat, or its caches changing size. (Not CPU: a start stuck in a driver
/// call spun a whole core for 23 minutes.)
async fn boot_progress(id: &str, since: u64) -> (bool, u64) {
    let prefixes = container_prefixes(id);
    let names = tokio::process::Command::new("docker")
        .args(["ps", "--format", "{{.Names}}"])
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut printed = false;
    if let Some(name) = names
        .lines()
        .find(|l| prefixes.iter().any(|p| l.starts_with(p.as_str())))
    {
        let out = tokio::process::Command::new("docker")
            .args(["logs", "--since", &since.to_string(), name])
            .output()
            .await;
        if let Ok(o) = out {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            printed = text
                .lines()
                .any(|l| !l.trim().is_empty() && !l.contains("starting_still"));
        }
    }
    let cache = container_prefixes(id).last().map(|p| {
        home()
            .join(".cache/tt-model")
            .join(p.trim_start_matches("tt-model-"))
    });
    let bytes = match cache {
        Some(dir) => tokio::task::spawn_blocking(move || crate::download::dir_bytes(&dir))
            .await
            .unwrap_or(0),
        None => 0,
    };
    (printed, bytes)
}

async fn watch_boot(state: AppState, id: String) {
    *mine(&state).stall.lock().expect("community") = Some((id.clone(), Instant::now()));
    let mut checked = now_secs();
    let mut cache_bytes = 0u64;
    let mut ticks = 0u32;
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let life = state.lifecycle();
        if !matches!(life.phase, Phase::Starting)
            || serving(&state).map(|(i, _)| i) != Some(id.clone())
        {
            *mine(&state).stall.lock().expect("community") = None;
            return;
        }
        if unit_active(crate::orchard::HARVEST_UNIT) || container_up(&id).await {
            ticks += 1;
            // Every minute: has it got anywhere since the last look?
            if ticks.is_multiple_of(6) {
                let (printed, bytes) = boot_progress(&id, checked).await;
                checked = now_secs();
                let moved = printed || (bytes != cache_bytes && cache_bytes != 0);
                cache_bytes = bytes;
                // How long it's been quiet (the lock is let go before anything waits).
                let quiet = {
                    let mut stall = mine(&state).stall.lock().expect("community");
                    if moved {
                        *stall = Some((id.clone(), Instant::now()));
                        None
                    } else {
                        stall
                            .as_ref()
                            .filter(|(i, _)| *i == id)
                            .map(|(_, at)| at.elapsed())
                    }
                };
                if let Some(quiet) = quiet {
                    if quiet >= RECOVER_AFTER {
                        *mine(&state).stall.lock().expect("community") = None;
                        recover(&state, &id, quiet.as_secs() / 60).await;
                        return;
                    }
                    if quiet >= STALL_AFTER && quiet < STALL_AFTER + Duration::from_secs(60) {
                        eprintln!("tt-station-agentd: community: {id}: no progress for {} min; the start looks stuck", quiet.as_secs() / 60);
                        state.notifier().emit(crate::notify::Draft {
                            kind: "model",
                            title: format!("{} looks stuck", short_id(&id)),
                            body: format!("Its start has shown no progress for {} minutes. Stop it in Models (its chips are reset), then try again or pick another model.", quiet.as_secs() / 60),
                        });
                    }
                }
            }
            continue;
        }
        *mine(&state).stall.lock().expect("community") = None;
        let line = failure_line(&work_dir(&state, &id).join("serve.log"))
            .unwrap_or_else(|| "tt-model stopped".into());
        let msg = format!("It stopped while starting: {line}");
        eprintln!("tt-station-agentd: community: {id}: {msg}");
        set_serving(&state, None);
        let _paused = state.pause_readings();
        for lease in take_leases(&state) {
            release_lease(&lease).await;
        }
        state.set_lifecycle(Phase::Failed, life.model, None, Some(msg.clone()));
        update_preparing(&state, &id, |p| p.error = Some(msg.clone()));
        return; // the lifecycle's Failed notifies
    }
}

/// At startup: a community package started before the agent restarted keeps
/// serving in its container. Take it back as the served model if it still
/// answers; forget it if it doesn't.
pub async fn readopt(state: AppState) {
    let Some((id, model)) = serving(&state) else {
        return;
    };
    if !matches!(state.lifecycle().phase, Phase::Idle | Phase::Failed) {
        return;
    }
    let base = format!("http://127.0.0.1:{}/v1", state.serving_port());
    let answered: Option<Value> =
        crate::served::list(&crate::served::local_root(&state), Duration::from_secs(5))
            .await
            .filter(|listing| listing.success)
            .map(|listing| listing.body);
    // Something else on the port (another model started since) isn't ours.
    let served_id = answered
        .as_ref()
        .and_then(|v| v["data"][0]["id"].as_str().map(str::to_string));
    let answered = answered.filter(|_| served_id.as_deref().is_some_and(|s| same_model(s, &model)));
    match answered {
        Some(v) => {
            let served = v["data"][0]["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or(model);
            let short = served.rsplit('/').next().unwrap_or(&served).to_string();
            state.adopt_serving(libttstation::model::Endpoint {
                base_url: base,
                model: served,
                requires_key: false,
            });
            eprintln!("tt-station-agentd: community: {id} is still serving; adopted it");
            // Its chips too, if no lease covers them yet (a no-op when they're already ours).
            if !leases_file(&state).exists() {
                tokio::spawn(adopt_chips(state.clone(), id.clone(), short));
            }
        }
        None => {
            // Not answering yet: still booting if its container is up (wait for it), else gone.
            let up = container_up(&id).await;
            if up {
                eprintln!("tt-station-agentd: community: {id} is still starting; waiting for it");
                crate::orchard::adopt_when_serving(&state, state.serving_port(), model.clone());
                let short = model.rsplit('/').next().unwrap_or(&model).to_string();
                tokio::spawn(adopt_chips(state.clone(), id.clone(), short));
                tokio::spawn(check_replies(state.clone(), id.clone()));
                tokio::spawn(watch_boot(state.clone(), id));
            } else {
                set_serving(&state, None);
            }
        }
    }
}

/// Stop the community package that's serving, if one is: `tt-model serve`
/// leaves its container running once the model is up, so stopping the serving
/// unit isn't enough. `tt-model stop` sends SIGTERM first, so the chips close
/// cleanly. Called whenever the agent stops serving.
pub async fn stop_package(state: &AppState) {
    let Some((id, _)) = serving(state) else {
        return;
    };
    set_serving(state, None);
    *mine(state).stopping.lock().expect("community") = Some((id.clone(), now_secs()));
    let leases = take_leases(state);
    // The container closing the chips, then gozer resetting each board: no
    // tt-smi readings meanwhile (they fail, and they slow the resets down).
    let _paused = state.pause_readings();
    if let Some(bin) = tt_model() {
        let out = tokio::process::Command::new(bin)
            .args(["stop", &id])
            .stdin(Stdio::null())
            .output()
            .await;
        match out {
            Ok(o) => {
                let said = strip_ansi(&format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ));
                if !o.status.success() {
                    eprintln!("tt-station-agentd: couldn't stop {id}: {}", said.trim());
                }
                // Its server ignored SIGTERM and was killed: the chips were
                // left mid-use. tt-model warns that a force-killed teardown
                // can leave them unusable until the box restarts.
                if said.contains("did not exit on SIGTERM") {
                    let failed = said.contains("did NOT complete") || said.contains("NOT reset");
                    eprintln!(
                        "tt-station-agentd: community: {id} had to be force-stopped (reset {})",
                        if failed { "failed" } else { "ran" }
                    );
                    state.notifier().emit(crate::notify::Draft {
                        kind: "model_failed",
                        title: format!("{} had to be force-stopped", short_id(&id)),
                        body: if failed {
                            "It didn't shut down when asked, and the chip reset after it didn't finish. Restart the box before starting another model.".into()
                        } else {
                            "It didn't shut down when asked, so it was killed and its chips reset. If the next model hangs while starting, restart the box.".into()
                        },
                    });
                }
            }
            Err(e) => eprintln!("tt-station-agentd: couldn't stop {id}: {e}"),
        }
    }
    // Give the chips back (gozer resets the boards as it releases).
    let mut all = !leases.is_empty();
    for lease in &leases {
        all &= release_lease(lease).await;
    }
    // Every board was just reset: the next start needn't reset them again.
    if all && leases.len() as u64 * 2 >= BOX_CHIPS {
        *mine(state).just_reset.lock().expect("community") = Some(Instant::now());
    }
    *mine(state).stopping.lock().expect("community") = None;
}

fn chips_just_reset(state: &AppState) -> bool {
    mine(state)
        .just_reset
        .lock()
        .expect("community")
        .take()
        .is_some_and(|t| t.elapsed() < Duration::from_secs(120))
}

async fn release_lease(lease: &str) -> bool {
    let out = tokio::process::Command::new("gozer")
        .args(["release", lease, "--json"])
        .stdin(Stdio::null())
        .output()
        .await;
    let ok = out.is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("tt-station-agentd: community: couldn't release lease {lease}; `gozer reconcile` clears it");
    }
    ok
}

/// `meta-models/Muse-Glimmer-30B` → `Muse-Glimmer-30B`, for the lease's `who`.
fn model_short(pkg: &Value) -> String {
    let m = pkg["weights"]
        .as_str()
        .or(pkg["id"].as_str())
        .unwrap_or("community-model");
    m.rsplit('/').next().unwrap_or(m).to_string()
}

fn leases_file(state: &AppState) -> PathBuf {
    state.config_dir().join("community/leases.json")
}

/// Replace (or drop) one of the community package's lease ids -- the lease
/// reconciler re-attaching or releasing it. `false` when `old` isn't one.
pub(crate) fn swap_lease(state: &AppState, old: &str, new: Option<&str>) -> bool {
    crate::gozer::LeaseFile(leases_file(state)).swap(old, new)
}

fn take_leases(state: &AppState) -> Vec<String> {
    let file = leases_file(state);
    let ids: Vec<String> = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let _ = std::fs::remove_file(file);
    ids
}

/// A community package runs its model in tt-model's container, which opens
/// the chips without a gozer lease, so gozer (and everyone who asks it) sees
/// them as held by an unknown process. Once the server opens them, adopt them
/// under this agent's own `tt-station:<port>:<model>` name, the same as the
/// models it serves itself; stopping the package releases them.
async fn adopt_chips(state: AppState, id: String, model: String) {
    let who = format!(
        "{}{}:{model}",
        crate::gozer::WHO_PREFIX,
        state.serving_port()
    );
    let deadline = Instant::now() + Duration::from_secs(45 * 60);
    while Instant::now() < deadline {
        // Stopped, failed or replaced meanwhile: nothing to adopt.
        if serving(&state).map(|(i, _)| i) != Some(id.clone())
            || matches!(state.lifecycle().phase, Phase::Idle | Phase::Failed)
        {
            return;
        }
        let status: Option<Value> = tokio::process::Command::new("gozer")
            .args(["status", "--json"])
            .stdin(Stdio::null())
            .output()
            .await
            .ok()
            .and_then(|o| serde_json::from_slice(&o.stdout).ok());
        let chips: Vec<&Value> = status
            .as_ref()
            .and_then(|s| s["chips"].as_array())
            .into_iter()
            .flatten()
            .collect();
        // Already ours (an earlier adopt, before an agent restart): done.
        if !chips.is_empty()
            && leases_file(&state).exists()
            && chips
                .iter()
                .all(|c| c["who"].as_str() == Some(who.as_str()))
        {
            return;
        }
        // One adopt per board (gozer leases whole boards) whose chips a process holds unleased.
        let mut boards: Vec<String> = Vec::new();
        for c in chips {
            if c["state"] == "BUSY-UNTRACKED" && c["who"].is_null() {
                if let (Some(board), Some(bdf)) = (c["board"].as_str(), c["bdf"].as_str()) {
                    if !boards.iter().any(|b| b.starts_with(board)) {
                        boards.push(format!("{board} {bdf}"));
                    }
                }
            }
        }
        if !boards.is_empty() {
            let mut ids = Vec::new();
            for b in &boards {
                let bdf = b.split_whitespace().nth(1).unwrap_or("");
                let out = tokio::process::Command::new("gozer")
                    .args([
                        "adopt",
                        "--json",
                        "--who",
                        &who,
                        "--reason",
                        &format!("community model {id}"),
                        bdf,
                    ])
                    .stdin(Stdio::null())
                    .output()
                    .await;
                match out
                    .ok()
                    .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
                {
                    Some(v) if v["adopted"] == true => {
                        ids.extend(v["lease_id"].as_str().map(str::to_string))
                    }
                    Some(v) => eprintln!(
                        "tt-station-agentd: community: couldn't adopt {bdf} for {id}: {v}"
                    ),
                    None => {
                        eprintln!("tt-station-agentd: community: gozer adopt {bdf} gave no answer")
                    }
                }
            }
            if !ids.is_empty() {
                let mut all: Vec<String> = std::fs::read(leases_file(&state))
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_default();
                all.extend(ids.iter().cloned());
                let _ = std::fs::write(
                    leases_file(&state),
                    serde_json::to_vec(&all).unwrap_or_default(),
                );
                eprintln!("tt-station-agentd: community: {id} now holds its chips under gozer lease(s) {}", ids.join(", "));
                return;
            }
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

/// Is this reply what a person should see? Raw chat-format tokens or nothing
/// at all mean the package's chat template or parsers don't match its model.
fn reply_ok(content: &str) -> bool {
    let c = content.trim();
    !c.is_empty() && !c.contains("<|") && !c.contains("|>") && !c.starts_with("to=")
}

/// Once a community model is serving, send it one short message: a package
/// can boot fine and still answer in raw tokens (a chat template or parser
/// that doesn't match its model), which breaks Chat and Tori. Say so.
async fn check_replies(state: AppState, id: String) {
    let deadline = Instant::now() + Duration::from_secs(3600);
    while !matches!(state.lifecycle().phase, Phase::Serving) {
        if Instant::now() > deadline
            || matches!(state.lifecycle().phase, Phase::Idle | Phase::Failed)
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    let base = format!("http://127.0.0.1:{}/v1", state.serving_port());
    let client = crate::localhttp::local();
    let model = crate::served::local(&state, Duration::from_secs(10))
        .await
        .and_then(|served| served.model);
    let Some(model) = model else { return };
    let reply = client
        .post(format!("{base}/chat/completions"))
        .json(&json!({ "model": model, "max_tokens": 64,
            "messages": [{ "role": "user", "content": "Reply with one short friendly sentence." }] }))
        .timeout(Duration::from_secs(180))
        .send()
        .await;
    let content = match reply {
        Ok(r) => r.json::<Value>().await.ok().map(|v| {
            v["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("")
                .to_string()
        }),
        Err(_) => None,
    };
    let ok = content.as_deref().is_some_and(reply_ok);
    let sample: String = content.unwrap_or_default().chars().take(160).collect();
    let _ = std::fs::write(
        work_dir(&state, &id).join("check.json"),
        json!({ "ok": ok, "sample": sample }).to_string(),
    );
    if ok {
        // The model to go back to when a later start gets stuck.
        let _ = std::fs::write(
            last_good_file(&state),
            json!({ "id": id, "at": now_secs() }).to_string(),
        );
    }
    if !ok {
        eprintln!("tt-station-agentd: community: {id} answered a test message badly: {sample:?}");
        state.notifier().emit(crate::notify::Draft {
            kind: "model_failed",
            title: format!("{} isn't answering properly", id.rsplit('/').next().unwrap_or(&id)),
            body: "It's running, but its replies come back garbled (its package's chat format doesn't match the model). Chat and Tori may not work with it.".into(),
        });
    }
}

/// `DELETE /community/{ns}/{name}`: remove a downloaded package (its weights stay
/// in the Hugging Face cache, shared with other models).
pub(crate) async fn delete_package(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath((ns, name)): UrlPath<(String, String)>,
) -> Reply {
    let id = format!("{ns}/{name}");
    if !valid_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "That isn't a package name."));
    }
    let life = state.lifecycle();
    if current(&state, &life).as_deref() == Some(id.as_str()) {
        return Err(err(StatusCode::CONFLICT, "It's serving. Stop it first."));
    }
    let bin = need_tt_model()?;
    let i = id.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(bin)
            .args(["rm", &i])
            .stdin(Stdio::null())
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        let text = strip_ansi(&format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            text.lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("couldn't remove it")
                .to_string(),
        ));
    }
    let _ = std::fs::remove_dir_all(work_dir(&state, &id));
    Ok(Json(json!({ "removed": id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_vllm_plugin_packages_for_cached_token_counts() {
        let kind = |k: &str| json!({ "container": { "kind": k, "serve": { "args": [] } } });
        assert_eq!(
            extra_vllm_args(Some(&kind("vllm-plugin"))),
            ["--enable-prompt-tokens-details"]
        );
        assert!(
            extra_vllm_args(Some(&kind("tt-dit-server"))).is_empty(),
            "not vLLM"
        );
        assert!(
            extra_vllm_args(Some(&kind("vllm-fork"))).is_empty(),
            "loose flags don't reach its vLLM"
        );
        assert!(extra_vllm_args(Some(&json!({}))).is_empty());
        assert!(
            extra_vllm_args(None).is_empty(),
            "no manifest, nothing guessed"
        );
    }

    #[test]
    fn matches_served_ids() {
        assert!(same_model(
            "meta-models/Muse-Glimmer-30B",
            "meta-models/Muse-Glimmer-30B"
        ));
        assert!(same_model(
            "muse-glimmer-30b",
            "meta-models/Muse-Glimmer-30B"
        ));
        assert!(!same_model(
            "tt-flux.1-schnell",
            "meta-models/Muse-Glimmer-30B"
        ));
    }

    #[test]
    fn spots_garbled_replies() {
        assert!(reply_ok("Hello there!"));
        assert!(!reply_ok(" to=self<|message|>Say hello"));
        assert!(!reply_ok("   "));
        assert!(!reply_ok("<|channel|>analysis"));
    }

    #[test]
    fn deleting_weights_keeps_files_another_model_links_to() {
        let hub = std::env::temp_dir().join(format!("tt-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&hub);
        for r in ["models--a--x/blobs", "models--b--y/blobs", "blobs/aa"] {
            std::fs::create_dir_all(hub.join(r)).unwrap();
        }
        std::fs::write(hub.join("blobs/aa/only-x"), vec![0u8; 100]).unwrap();
        std::fs::write(hub.join("blobs/aa/shared"), vec![0u8; 50]).unwrap();
        std::os::unix::fs::symlink(
            "../../blobs/aa/only-x",
            hub.join("models--a--x/blobs/only-x"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../blobs/aa/shared",
            hub.join("models--a--x/blobs/shared"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../blobs/aa/shared",
            hub.join("models--b--y/blobs/shared"),
        )
        .unwrap();
        std::fs::write(hub.join("models--a--x/blobs/part.incomplete"), vec![0u8; 7]).unwrap();
        assert_eq!(delete_repo(&hub, "a/x"), 107);
        assert!(!hub.join("models--a--x").exists());
        assert!(!hub.join("blobs/aa/only-x").exists());
        assert!(hub.join("blobs/aa/shared").exists(), "y still links to it");
        let _ = std::fs::remove_dir_all(&hub);
    }

    #[test]
    fn names_a_package_container_after_its_image() {
        assert_eq!(
            image_name("tt-model/qwen3.8-flash-next:6e8c55ef7f2f").as_deref(),
            Some("qwen3.8-flash-next")
        );
        assert_eq!(image_name("tt-model/x").as_deref(), Some("x"));
        assert_eq!(
            image_name("registry.example:5000/tt-model/y:1").as_deref(),
            Some("y")
        );
    }

    #[test]
    fn reads_why_a_start_failed_from_inside_the_frame() {
        let dir = std::env::temp_dir().join(format!("tt-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("serve.log");
        std::fs::write(&log, "  ✓ container started\n╭─ serve x — the container exited ─╮\n│  The engine stopped during boot.   │\n│  Log · ERROR:    RuntimeError: the chat server ended with status 1 │\n│                                    │\n╰────────────────────────────────────╯\n").unwrap();
        assert_eq!(
            failure_line(&log).as_deref(),
            Some("ERROR:    RuntimeError: the chat server ended with status 1")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serves_the_profile_made_for_this_box() {
        let flash = "  • c32k (default)  p150x4 · P150x4  seqs None · len -\n  • c64k            p150x4 · P150x4  seqs None · len -\n  • c32k-quietbox   p150x4 · P150x4  seqs None · len -\n  • c32k-quietbox2  p300x2 · P300x2  seqs None · len -\n";
        assert_eq!(
            profile_for_this_box(flash).as_deref(),
            Some("c32k-quietbox2")
        );
        let ours = "  • fast (default)  p300x2 · P300x2  seqs 1 · len 65536\n  • big  p150x8 · P150x8  seqs 1 · len -\n";
        assert_eq!(profile_for_this_box(ours), None);
        assert_eq!(
            profile_for_this_box("  • only (default)  p150x4 · P150x4  seqs None · len -\n"),
            None
        );
        assert_eq!(profile_for_this_box(""), None);
    }

    #[test]
    fn reads_which_step_a_package_download_is_on() {
        assert_eq!(pull_step(""), ("package", None));
        assert_eq!(pull_step("docker load tt-model/x:abc…\n").0, "image");
        let log = "✓ docker load tt-model/x:abc  1m 27s\nweights Qwen/Qwen3-Coder-30B-A3B-Instruct@b2cff646 — resuming a partial \ndownload…\n";
        assert_eq!(
            pull_step(log),
            (
                "weights",
                Some("Qwen/Qwen3-Coder-30B-A3B-Instruct".to_string())
            )
        );
        let done = format!(
            "{log}\u{1b}[32m✓\u{1b}[0m weights Qwen/Qwen3-Coder-30B-A3B-Instruct@b2cff646\n"
        );
        assert_eq!(pull_step(&done).0, "finishing");
    }

    #[test]
    fn package_names() {
        assert!(valid_id("tt-hous/qwen3.8-flash-next-p300x2"));
        assert!(!valid_id("../etc"));
        assert!(!valid_id("a/b/c"));
        assert!(!valid_id("a/b c"));
        assert!(!valid_id("a/.."));
    }

    #[test]
    fn what_fits_a_quietbox() {
        assert!(fits(Some(4), "p300x2").0);
        assert!(fits(Some(1), "p150").0);
        assert!(!fits(Some(32), "galaxy").0);
        assert!(!fits(Some(8), "p150x8").0);
        assert_eq!(fit_from_name("changh95/qwen3.8-27b-p300x2").0, Some(true));
        assert_eq!(fit_from_name("changh95/qwen3-32b-bh-galaxy").0, Some(false));
        assert_eq!(fit_from_name("tt-hous/clef").0, None);
    }

    #[test]
    fn reads_tt_model_info() {
        let text = "{\n \"device_count\": 4,\n \"name\": \"x {y}\",\n \"container\": {\"serve\": {\"hardware\": \"p300x2\"}}\n}\n\n\u{1b}[32m✓\u{1b}[0m compatible with the local environment\n";
        let (m, v) = parse_info(text);
        assert_eq!(m.unwrap()["device_count"], 4);
        assert_eq!(
            v,
            Some((true, "compatible with the local environment".to_string()))
        );
    }
}
