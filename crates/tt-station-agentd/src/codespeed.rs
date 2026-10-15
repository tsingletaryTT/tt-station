// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! How fast Code runs on the box, step by step, and which model it runs on.
//!
//! Every model step of a Code session (one `/v1/messages` generation, see
//! `crate::anthropic`) leaves a [`Step`]: the prompt it read (the whole
//! conversation so far), how much of that the server had cached, the time to
//! its first token and its decode speed. A session keeps its last few steps,
//! for the speed strip in the Code window. The box also keeps a small book of
//! what it measured per model, so the "Model for Code" picker can say how fast
//! a model it has run before actually was here.
//!
//! Whether the server caches prompt prefixes matters more than anything else
//! for an agent: each step re-sends the whole conversation, so without the
//! cache every step reads all of it again. [`prefix_cache`] works it out from
//! what the server says (cached tokens in its answers, its `/metrics`) and
//! failing that from the flags it was launched with.
//!
//! - `GET /code/sessions/{id}/speed` (authed): a session's recent steps, its
//!   context against the compaction point and the window, and the prefix cache.
//! - `GET /code/models` (authed): the models Code can use on this box, each with
//!   its window, prefix caching, measured speed, and what choosing it would do.
//!   Choosing one that isn't serving goes through `POST /serve`, like Models.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// Steps kept per session: enough for the strip's recent-steps table.
const KEEP: usize = 40;
/// Sessions with steps kept at once (the least recently active go first).
const SESSIONS: usize = 32;
/// Speeds kept per model in the book; their median is what the picker shows.
const SAMPLES: usize = 24;

// ---------------------------------------------------------------- steps

/// One model step of a Code session.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Step {
    /// Unix seconds it ended.
    pub at: u64,
    /// The served model that answered.
    pub model: Option<String>,
    /// The prompt it read: the context in use at this step.
    pub prompt_tokens: u64,
    /// How much of the prompt the server had cached, when it says.
    pub cached_tokens: Option<u64>,
    pub output_tokens: u64,
    /// Request to first token: queueing plus reading the uncached prompt.
    pub ttft_ms: Option<u64>,
    /// Generated tokens a second, from the first token on.
    pub decode_tps: Option<f64>,
    pub seconds: f64,
}

impl Step {
    pub fn measured(
        model: Option<String>,
        started: Instant,
        first: Option<Instant>,
        ended: Instant,
        prompt: u64,
        cached: Option<u64>,
        output: u64,
    ) -> Step {
        let ttft = first.map(|f| f.saturating_duration_since(started));
        // Decode speed over the tokens after the first, in the time after it: too
        // few tokens or too short a stretch says nothing.
        let decode_tps = first
            .map(|f| ended.saturating_duration_since(f).as_secs_f64())
            .filter(|s| *s >= 0.2 && output >= 8)
            .map(|s| ((output - 1) as f64 / s * 10.0).round() / 10.0);
        Step {
            at: unix_now(),
            model,
            prompt_tokens: prompt,
            cached_tokens: cached,
            output_tokens: output,
            ttft_ms: ttft.map(|d| d.as_millis() as u64),
            decode_tps,
            seconds: (ended.saturating_duration_since(started).as_secs_f64() * 10.0).round() / 10.0,
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Session id → its recent steps (oldest first), and when it last had one.
type Recent = (Instant, VecDeque<Step>);
static STEPS: LazyLock<Mutex<HashMap<String, Recent>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Keep a finished step for its session, and in the model's book.
pub fn record(session: &str, step: Step) {
    // The session id comes from a request header: only ids shaped like ours.
    if session.is_empty()
        || session.len() > 40
        || !session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return;
    }
    {
        let mut all = STEPS.lock().expect("steps");
        if !all.contains_key(session) && all.len() >= SESSIONS {
            if let Some(oldest) = all
                .iter()
                .min_by_key(|(_, (t, _))| *t)
                .map(|(k, _)| k.clone())
            {
                all.remove(&oldest);
            }
        }
        let (at, list) = all
            .entry(session.to_string())
            .or_insert_with(|| (Instant::now(), VecDeque::new()));
        *at = Instant::now();
        list.push_back(step.clone());
        while list.len() > KEEP {
            list.pop_front();
        }
    }
    if let Some(model) = &step.model {
        book_add(model, &step);
    }
}

/// A session's recent steps, oldest first.
pub fn steps(session: &str) -> Vec<Step> {
    STEPS
        .lock()
        .expect("steps")
        .get(session)
        .map(|(_, l)| l.iter().cloned().collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------- the speed book

/// What the box measured for one model in Code.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Measured {
    decode_tps: VecDeque<f64>,
    ttft_ms: VecDeque<u64>,
    steps: u64,
    /// The server reported cached prompt tokens for it at least once.
    #[serde(default)]
    cached_seen: bool,
    /// The last prefix-cache answer [`prefix_cache`] found while it served.
    #[serde(default)]
    prefix_cache: Option<bool>,
    at: u64,
}

static BOOK: LazyLock<Mutex<HashMap<String, Measured>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Where the book lives (`code-speed.json` in the agent's config directory),
/// once a Code route has run. Tests never set it, so they write nothing.
static BOOK_PATH: OnceLock<PathBuf> = OnceLock::new();
static BOOK_SAVED: Mutex<Option<Instant>> = Mutex::new(None);

/// Load the book from disk the first time a route needs it.
fn attach(state: &AppState) {
    let path = state.config_dir().join("code-speed.json");
    if BOOK_PATH.set(path.clone()).is_ok() {
        if let Some(saved) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, Measured>>(&b).ok())
        {
            let mut book = BOOK.lock().expect("book");
            for (k, v) in saved {
                book.entry(k).or_insert(v);
            }
        }
    }
}

fn book_add(model: &str, step: &Step) {
    {
        let mut book = BOOK.lock().expect("book");
        let m = book.entry(model.to_string()).or_default();
        if let Some(t) = step.decode_tps {
            m.decode_tps.push_back(t);
        }
        if let Some(t) = step.ttft_ms {
            m.ttft_ms.push_back(t);
        }
        while m.decode_tps.len() > SAMPLES {
            m.decode_tps.pop_front();
        }
        while m.ttft_ms.len() > SAMPLES {
            m.ttft_ms.pop_front();
        }
        m.steps += 1;
        m.cached_seen |= step.cached_tokens.is_some_and(|c| c > 0);
        m.at = step.at;
    }
    save_book(false);
}

fn book_prefix(model: &str, on: bool) {
    let changed = {
        let mut book = BOOK.lock().expect("book");
        let m = book.entry(model.to_string()).or_default();
        let changed = m.prefix_cache != Some(on);
        m.prefix_cache = Some(on);
        changed
    };
    if changed {
        save_book(true);
    }
}

/// Write the book, at most every half minute unless `now`: a step every few
/// seconds needn't mean a write every few seconds.
fn save_book(now: bool) {
    let Some(path) = BOOK_PATH.get() else { return };
    {
        let mut saved = BOOK_SAVED.lock().expect("book saved");
        if !now && saved.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
            return;
        }
        *saved = Some(Instant::now());
    }
    let body = serde_json::to_vec_pretty(&*BOOK.lock().expect("book")).unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn median_f(v: &VecDeque<f64>) -> Option<f64> {
    let mut s: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if s.is_empty() {
        return None;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    Some(s[s.len() / 2])
}

fn median_u(v: &VecDeque<u64>) -> Option<u64> {
    let mut s: Vec<u64> = v.iter().copied().collect();
    s.sort_unstable();
    s.get(s.len() / 2).copied()
}

/// The book's entry for `model`, matched like model names are elsewhere.
fn measured(model: &str) -> Option<Measured> {
    BOOK.lock()
        .expect("book")
        .iter()
        .find(|(k, _)| same_model(k, model))
        .map(|(_, v)| v.clone())
}

fn speed_json(m: &Measured) -> Value {
    json!({
        "decode_tps": median_f(&m.decode_tps),
        "ttft_s": median_u(&m.ttft_ms).map(|ms| (ms as f64 / 100.0).round() / 10.0),
        "steps": m.steps,
        "at": m.at,
    })
}

/// Two names for the same model: `openai/gpt-oss-20b`, `gpt-oss-20b`, `tt-gpt-oss-20b`.
pub fn same_model(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let short = s.rsplit('/').next().unwrap_or(s).to_lowercase();
        short
            .strip_prefix("tt-")
            .map(str::to_string)
            .unwrap_or(short)
    };
    !a.is_empty() && !b.is_empty() && norm(a) == norm(b)
}

// ---------------------------------------------------------------- prefix caching

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    On,
    Off,
    Unknown,
}

/// Whether a server caches prompt prefixes, and how we know.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrefixCache {
    pub state: Cache,
    /// In a few words, for a tooltip: "server metrics", "launch flags"…
    pub how: String,
}

impl PrefixCache {
    fn new(state: Cache, how: &str) -> Self {
        PrefixCache {
            state,
            how: how.to_string(),
        }
    }
}

/// From vLLM's `/metrics`: `Some(true)` once its prefix-cache queries counter
/// has moved, `Some(false)` when prompts went through and it never did,
/// `None` when it doesn't export the counter (`--disable-log-stats`) or hasn't
/// read a prompt yet.
pub fn cache_from_metrics(text: &str) -> Option<bool> {
    let present = text
        .lines()
        .any(|l| l.starts_with("vllm:prefix_cache_queries"));
    if !present {
        return None;
    }
    let c = crate::inference::parse_vllm_metrics(text)?;
    if c.prefix_queries_total > 0 {
        Some(true)
    } else if c.prompt_tokens_total > 0 {
        Some(false)
    } else {
        None
    }
}

/// From a vLLM server's command line.
pub fn cache_from_flags(args: &[String]) -> PrefixCache {
    let mut asked: Option<bool> = None;
    let mut draft = false;
    for a in args {
        let flag = a.split('=').next().unwrap_or(a).replace('_', "-");
        match flag.as_str() {
            "--enable-prefix-caching" => asked = Some(true),
            "--no-enable-prefix-caching" => asked = Some(false),
            "--speculative-config" | "--speculative-model" => draft = true,
            _ => {}
        }
    }
    match (asked, draft) {
        (Some(false), _) => PrefixCache::new(Cache::Off, "turned off in its launch flags"),
        (Some(true), false) => PrefixCache::new(Cache::On, "turned on in its launch flags"),
        (Some(true), true) => PrefixCache::new(
            Cache::Unknown,
            "asked for at launch, but a draft model can turn it off",
        ),
        (None, _) => PrefixCache::new(Cache::Unknown, "not set in its launch flags"),
    }
}

/// The vLLM process serving `model` on `port`, among command lines: the one
/// that names the model (`--served-model-name`, else `--model`), or failing
/// that the one listening on the port (the last `--port` counts, as in argparse).
pub fn find_server(
    cmdlines: &[Vec<String>],
    model: &str,
    port: Option<u16>,
) -> Option<Vec<String>> {
    let is_vllm = |a: &[String]| a.iter().take(4).any(|x| x.contains("vllm"));
    let value = |a: &[String], flag: &str| -> Option<String> {
        let mut found = None;
        for (i, x) in a.iter().enumerate() {
            if x == flag {
                found = a.get(i + 1).cloned();
            } else if let Some(v) = x.strip_prefix(&format!("{flag}=")) {
                found = Some(v.to_string());
            }
        }
        found
    };
    let servers: Vec<&Vec<String>> = cmdlines.iter().filter(|a| is_vllm(a)).collect();
    servers
        .iter()
        .find(|a| {
            value(a, "--served-model-name")
                .or_else(|| value(a, "--model"))
                .is_some_and(|n| same_model(&n, model))
        })
        .or_else(|| {
            servers.iter().find(|a| {
                port.is_some() && value(a, "--port").and_then(|p| p.parse::<u16>().ok()) == port
            })
        })
        .map(|a| (*a).clone())
}

/// Every process's command line (containers' too: they share the host's `/proc`).
/// Only read for flags here; never sent anywhere, since a command line can hold secrets.
fn cmdlines() -> Vec<Vec<String>> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    dir.flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .filter(|b| !b.is_empty())
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect()
        })
        .collect()
}

/// The server's root (`http://127.0.0.1:8003`) from an endpoint's base URL.
fn root(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

fn port_of(base_url: &str) -> Option<u16> {
    let after = base_url.split("://").nth(1)?;
    let host = after.split('/').next()?;
    host.rsplit_once(':').and_then(|(_, p)| p.parse().ok())
}

/// The answer for each model, for half a minute: it reads `/metrics` and `/proc`.
static DETECTED: LazyLock<Mutex<HashMap<String, (Instant, PrefixCache)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Does the server answering `model` at `base_url` cache prompt prefixes?
pub async fn prefix_cache(model: &str, base_url: &str) -> PrefixCache {
    if let Some((at, p)) = DETECTED.lock().expect("detected").get(model) {
        if at.elapsed() < Duration::from_secs(30) {
            return p.clone();
        }
    }
    let found = detect(model, base_url).await;
    if found.state != Cache::Unknown {
        book_prefix(model, found.state == Cache::On);
    }
    DETECTED
        .lock()
        .expect("detected")
        .insert(model.to_string(), (Instant::now(), found.clone()));
    found
}

async fn detect(model: &str, base_url: &str) -> PrefixCache {
    // It answered with cached tokens: nothing says it better.
    if measured(model).is_some_and(|m| m.cached_seen) {
        return PrefixCache::new(Cache::On, "the server reported cached prompt tokens");
    }
    let metrics = crate::localhttp::local()
        .get(format!("{}/metrics", root(base_url)))
        .timeout(Duration::from_secs(3))
        .send()
        .await;
    if let Ok(r) = metrics {
        if let Ok(text) = r.text().await {
            if let Some(on) = cache_from_metrics(&text) {
                return PrefixCache::new(
                    if on { Cache::On } else { Cache::Off },
                    "the server's metrics",
                );
            }
        }
    }
    let (m, port) = (model.to_string(), port_of(base_url));
    tokio::task::spawn_blocking(move || {
        find_server(&cmdlines(), &m, port).map(|a| cache_from_flags(&a))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| PrefixCache::new(Cache::Unknown, "the server doesn't say"))
}

/// What the box last found for a model that isn't serving now.
fn remembered_cache(model: &str) -> PrefixCache {
    match measured(model) {
        Some(m) if m.cached_seen || m.prefix_cache == Some(true) => {
            PrefixCache::new(Cache::On, "seen when it last served here")
        }
        Some(m) if m.prefix_cache == Some(false) => {
            PrefixCache::new(Cache::Off, "seen when it last served here")
        }
        _ => PrefixCache::new(Cache::Unknown, "not seen serving here yet"),
    }
}

// ---------------------------------------------------------------- the window

/// The served model's context window, from its `/v1/models`, for a minute.
type Known = (Instant, Option<u64>);
static CONTEXT: LazyLock<Mutex<HashMap<String, Known>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn served_context(model: &str, base_url: &str) -> Option<u64> {
    if let Some(c) = crate::coding::known_context(model) {
        return Some(c);
    }
    if let Some((at, c)) = CONTEXT.lock().expect("context").get(model) {
        if at.elapsed() < Duration::from_secs(60) {
            return *c;
        }
    }
    let c = crate::served::list(&root(base_url), Duration::from_secs(3))
        .await
        .and_then(|listing| {
            let data = listing.body["data"].as_array()?;
            data.iter()
                .find(|d| d["id"].as_str().is_some_and(|id| same_model(id, model)))
                .or(data.first())?["max_model_len"]
                .as_u64()
        });
    CONTEXT
        .lock()
        .expect("context")
        .insert(model.to_string(), (Instant::now(), c));
    c
}

// ---------------------------------------------------------------- choosing the model

/// What choosing a model for Code would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", content = "why", rename_all = "snake_case")]
pub enum Choice {
    /// Code already runs on it: nothing to do.
    InUse,
    /// Start it on the box, stopping what serves there now.
    Switch,
    /// Not now (something else is starting or has the chips): why.
    Busy(String),
    /// It can't be Code's model here: why.
    Unavailable(String),
}

/// One model the picker offers.
pub struct Candidate<'a> {
    pub name: &'a str,
    /// Its weights are all on the box (or its container brings them).
    pub on_box: bool,
    /// It makes tool calls, which Code needs.
    pub tools: bool,
}

/// What the box is doing with its serving slot.
pub struct Serving<'a> {
    /// The model Code's requests go to now.
    pub code_model: Option<&'a str>,
    pub phase: Phase,
    /// The model starting or stopping (`phase` starting/stopping).
    pub changing: Option<&'a str>,
    /// Something else that has the chips (a bring-up, a lab run).
    pub holder: Option<&'a str>,
}

/// Whether picking `c` needs a switch, and whether one can happen now.
pub fn decide(c: &Candidate, s: &Serving) -> Choice {
    if s.code_model.is_some_and(|m| same_model(m, c.name)) {
        return Choice::InUse;
    }
    if !c.on_box {
        return Choice::Unavailable(
            "Its weights aren't on the box. Download it in Models first.".into(),
        );
    }
    if !c.tools {
        return Choice::Unavailable(
            "It can't make tool calls, which Code needs to read, edit and run things.".into(),
        );
    }
    match s.phase {
        Phase::Starting if s.changing.is_some_and(|m| same_model(m, c.name)) => {
            Choice::Busy("It's starting now.".into())
        }
        Phase::Starting => Choice::Busy(format!(
            "{} is starting. Wait for it to finish first.",
            s.changing.unwrap_or("Another model")
        )),
        Phase::Stopping => {
            Choice::Busy("The box is stopping a model. Try again in a moment.".into())
        }
        _ => match s.holder {
            Some(h) => Choice::Busy(h.to_string()),
            None => Choice::Switch,
        },
    }
}

// ---------------------------------------------------------------- routes

/// `GET /code/sessions/{id}/speed`.
pub(crate) async fn get_speed(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if !crate::workspace::exists(&state, &id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No such session." })),
        ));
    }
    attach(&state);
    let endpoint = crate::gateway::llm_endpoint(&state, None, true).await;
    let (context, cache) = match &endpoint {
        Some(e) => (
            served_context(&e.model, &e.base_url).await,
            Some(prefix_cache(&e.model, &e.base_url).await),
        ),
        None => (None, None),
    };
    // The same figures Claude Code was launched with (`coding::code_window`).
    let (window, compact_at) = crate::coding::code_window(&state, context);
    Ok(Json(json!({
        "model": endpoint.as_ref().map(|e| e.model.clone()),
        "window": window,
        "compact_at": compact_at,
        "prefix_cache": cache,
        "steps": steps(&id),
    })))
}

/// `GET /code/models`.
pub(crate) async fn get_models(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    attach(&state);
    let endpoint = crate::gateway::llm_endpoint(&state, None, true).await;
    let life = state.lifecycle();
    let device = state.device_mesh().map(str::to_string);
    let catalog = match state.tt_cli() {
        Some(tt) => tokio::task::spawn_blocking(move || {
            tt.catalog(device.as_deref(), false)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string())),
        None => Err("The box has no model catalog (the tt CLI isn't set up).".to_string()),
    };
    let code_model = endpoint.as_ref().map(|e| e.model.clone());
    let changing = matches!(life.phase, Phase::Starting | Phase::Stopping)
        .then(|| life.model.clone())
        .flatten();
    let serving = Serving {
        code_model: code_model.as_deref(),
        phase: life.phase,
        changing: changing.as_deref(),
        holder: state.chips_holder(),
    };

    let mut models: Vec<Value> = Vec::new();
    // What Code runs on now comes first, from the server itself: its real window.
    if let Some(e) = &endpoint {
        let context = served_context(&e.model, &e.base_url).await;
        let cache = prefix_cache(&e.model, &e.base_url).await;
        let listed = catalog
            .as_ref()
            .ok()
            .and_then(|c| c.models.iter().find(|m| same_model(&m.name, &e.model)));
        models.push(json!({
            "name": listed.map(|m| m.name.clone()).unwrap_or_else(|| e.model.clone()),
            "served_id": e.model,
            "context": context.or(listed.and_then(|m| m.max_context)),
            "prefix_cache": cache,
            "speed": measured(&e.model).map(|m| speed_json(&m)),
            "params_b": listed.and_then(|m| m.param_count),
            "choice": Choice::InUse,
            "eta_s": Value::Null,
        }));
    }
    if let Ok(cat) = &catalog {
        let mut rest: Vec<&crate::ttcli::CatalogModel> = cat
            .models
            .iter()
            .filter(|m| m.supported && matches!(m.model_type.as_str(), "llm" | "vlm"))
            .filter(|m| m.weights.ready())
            .filter(|m| {
                !code_model
                    .as_deref()
                    .is_some_and(|c| same_model(c, &m.name))
            })
            .collect();
        rest.sort_by_key(|m| m.name.to_lowercase());
        for m in rest {
            let choice = decide(
                &Candidate {
                    name: &m.name,
                    on_box: m.weights.ready(),
                    tools: m.tool_call_parser.is_some(),
                },
                &serving,
            );
            models.push(json!({
                "name": m.name,
                "served_id": Value::Null,
                "context": m.max_context,
                "prefix_cache": remembered_cache(&m.name),
                "speed": measured(&m.name).map(|s| speed_json(&s)),
                "params_b": m.param_count,
                "choice": choice,
                "eta_s": state.load_times().estimate(&m.name),
            }));
        }
    }
    Json(json!({
        "code_model": code_model,
        "serve": { "phase": life.phase, "model": life.model, "since": life.since, "error": life.error,
                   "eta_s": (life.phase == Phase::Starting).then(|| life.model.as_deref().and_then(|m| state.load_times().estimate(m))).flatten() },
        "catalog_error": catalog.err(),
        "models": models,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    const SERVING: Serving<'static> = Serving {
        code_model: Some("openai/gpt-oss-20b"),
        phase: Phase::Serving,
        changing: None,
        holder: None,
    };

    #[test]
    fn the_model_code_runs_on_needs_no_switch() {
        let c = Candidate {
            name: "gpt-oss-20b",
            on_box: true,
            tools: true,
        };
        assert_eq!(decide(&c, &SERVING), Choice::InUse);
        // Even with something holding the chips: nothing would be disturbed.
        let held = Serving {
            holder: Some("A bring-up has the chips."),
            ..SERVING
        };
        assert_eq!(decide(&c, &held), Choice::InUse);
    }

    #[test]
    fn another_model_on_the_box_is_a_switch() {
        let c = Candidate {
            name: "Qwen3-32B",
            on_box: true,
            tools: true,
        };
        assert_eq!(decide(&c, &SERVING), Choice::Switch);
        // Nothing serving, or the last start failed: still a start.
        assert_eq!(
            decide(
                &c,
                &Serving {
                    code_model: None,
                    phase: Phase::Idle,
                    ..SERVING
                }
            ),
            Choice::Switch
        );
        assert_eq!(
            decide(
                &c,
                &Serving {
                    code_model: None,
                    phase: Phase::Failed,
                    ..SERVING
                }
            ),
            Choice::Switch
        );
    }

    #[test]
    fn a_model_that_cant_serve_code_is_unavailable() {
        let missing = Candidate {
            name: "Qwen3-32B",
            on_box: false,
            tools: true,
        };
        assert!(
            matches!(decide(&missing, &SERVING), Choice::Unavailable(w) if w.contains("Download"))
        );
        let no_tools = Candidate {
            name: "Llama-3.1-8B",
            on_box: true,
            tools: false,
        };
        assert!(
            matches!(decide(&no_tools, &SERVING), Choice::Unavailable(w) if w.contains("tool calls"))
        );
    }

    #[test]
    fn no_switch_while_the_box_is_busy() {
        let c = Candidate {
            name: "Qwen3-32B",
            on_box: true,
            tools: true,
        };
        let starting_it = Serving {
            code_model: None,
            phase: Phase::Starting,
            changing: Some("Qwen3-32B"),
            holder: None,
        };
        assert_eq!(
            decide(&c, &starting_it),
            Choice::Busy("It's starting now.".into())
        );
        let starting_other = Serving {
            changing: Some("Llama-3.3-70B-Instruct"),
            ..starting_it
        };
        assert!(
            matches!(decide(&c, &starting_other), Choice::Busy(w) if w.starts_with("Llama-3.3-70B-Instruct is starting"))
        );
        let stopping = Serving {
            phase: Phase::Stopping,
            changing: None,
            ..SERVING
        };
        assert!(matches!(decide(&c, &stopping), Choice::Busy(_)));
        let held = Serving {
            holder: Some("A model bring-up is using the chips."),
            ..SERVING
        };
        assert_eq!(
            decide(&c, &held),
            Choice::Busy("A model bring-up is using the chips.".into())
        );
    }

    #[test]
    fn a_choice_serializes_as_action_and_why() {
        assert_eq!(
            serde_json::to_value(Choice::InUse).unwrap(),
            json!({ "action": "in_use" })
        );
        assert_eq!(
            serde_json::to_value(Choice::Busy("x".into())).unwrap(),
            json!({ "action": "busy", "why": "x" })
        );
    }

    #[test]
    fn model_names_match_across_spellings() {
        assert!(same_model("openai/gpt-oss-20b", "gpt-oss-20b"));
        assert!(same_model("tt-gpt-oss-20b", "GPT-OSS-20B"));
        assert!(!same_model("Qwen/Qwen3-32B", "Qwen3-8B"));
        assert!(!same_model("", ""));
    }

    #[test]
    fn launch_flags_say_whether_it_caches() {
        // The gpt-oss-20b server on this box, as launched.
        let live = args("/opt/tt-venv/bin/python -m vllm.entrypoints.openai.api_server --model /hf/hub/x --served-model-name openai/gpt-oss-20b \
            --port 8000 --max-model-len 131072 --enable-prefix-caching --enable-chunked-prefill --disable-log-stats --port 8003");
        assert_eq!(cache_from_flags(&live).state, Cache::On);
        assert_eq!(
            cache_from_flags(&args("vllm serve m --no-enable-prefix-caching")).state,
            Cache::Off
        );
        assert_eq!(
            cache_from_flags(&args(
                "vllm serve m --enable-prefix-caching --no-enable-prefix-caching"
            ))
            .state,
            Cache::Off
        );
        // A draft model (DFlash, EAGLE…) can keep it from working whatever was asked.
        assert_eq!(
            cache_from_flags(&args(
                "vllm serve m --enable-prefix-caching --speculative-config {}"
            ))
            .state,
            Cache::Unknown
        );
        assert_eq!(
            cache_from_flags(&args("vllm serve m --speculative_config={}")).state,
            Cache::Unknown
        );
        assert_eq!(
            cache_from_flags(&args("vllm serve m")).state,
            Cache::Unknown
        );
    }

    #[test]
    fn the_server_is_found_by_name_then_port() {
        let lines = vec![
            args("/usr/bin/bash -c sleep"),
            args("python -m vllm.entrypoints.openai.api_server --model /hf/a --served-model-name Qwen/Qwen3-32B --port 8000"),
            args("python -m vllm.entrypoints.openai.api_server --model /hf/b --served-model-name openai/gpt-oss-20b --port 8000 --port 8003 --enable-prefix-caching"),
        ];
        let found = find_server(&lines, "gpt-oss-20b", None).unwrap();
        assert!(found.contains(&"--enable-prefix-caching".to_string()));
        // By port when no name matches (the last --port wins).
        assert_eq!(
            find_server(&lines, "something-else", Some(8003)),
            Some(lines[2].clone())
        );
        assert_eq!(find_server(&lines, "something-else", Some(9999)), None);
        // Not a vLLM process: never.
        assert_eq!(
            find_server(
                &[args("grep --served-model-name gpt-oss-20b")],
                "gpt-oss-20b",
                None
            ),
            None
        );
    }

    #[test]
    fn metrics_say_whether_it_caches() {
        let on = "vllm:prompt_tokens_total{model_name=\"M\"} 900.0\nvllm:prefix_cache_queries_total{model_name=\"M\"} 800.0\nvllm:prefix_cache_hits_total{model_name=\"M\"} 600.0\n";
        assert_eq!(cache_from_metrics(on), Some(true));
        let off = "vllm:prompt_tokens_total{model_name=\"M\"} 900.0\nvllm:prefix_cache_queries_total{model_name=\"M\"} 0.0\n";
        assert_eq!(cache_from_metrics(off), Some(false));
        // Nothing read yet, or no counter at all (`--disable-log-stats`, as on this box).
        assert_eq!(
            cache_from_metrics("vllm:prefix_cache_queries_total{model_name=\"M\"} 0.0\n"),
            None
        );
        assert_eq!(
            cache_from_metrics("vllm:tool_call_parser_invocations_total 3.0\n"),
            None
        );
    }

    #[test]
    fn a_step_measures_first_token_and_decode() {
        let t0 = Instant::now();
        let first = t0 + Duration::from_millis(1500);
        let end = first + Duration::from_secs(2);
        let s = Step::measured(
            Some("m".into()),
            t0,
            Some(first),
            end,
            40_000,
            Some(39_000),
            81,
        );
        assert_eq!(s.ttft_ms, Some(1500));
        assert_eq!(s.decode_tps, Some(40.0));
        assert_eq!(s.seconds, 3.5);
        // A one-token answer has no decode speed worth showing.
        assert_eq!(
            Step::measured(None, t0, Some(first), end, 10, None, 1).decode_tps,
            None
        );
        assert_eq!(
            Step::measured(None, t0, None, end, 10, None, 0).ttft_ms,
            None
        );
    }

    #[test]
    fn steps_are_kept_per_session_and_capped() {
        let t0 = Instant::now();
        for i in 0..(KEEP + 5) {
            record(
                "test-ring-a",
                Step::measured(
                    Some("test-ring-model".into()),
                    t0,
                    Some(t0),
                    t0 + Duration::from_secs(1),
                    i as u64,
                    None,
                    20,
                ),
            );
        }
        let got = steps("test-ring-a");
        assert_eq!(got.len(), KEEP);
        assert_eq!(got.last().unwrap().prompt_tokens, (KEEP + 4) as u64);
        // Ids that aren't ours are ignored.
        record("../../etc", Step::measured(None, t0, None, t0, 1, None, 1));
        assert!(steps("../../etc").is_empty());
        let m = measured("test-ring-model").unwrap();
        assert_eq!(m.decode_tps.len(), SAMPLES);
        assert_eq!(median_f(&m.decode_tps), Some(19.0));
    }

    #[test]
    fn urls_split_into_root_and_port() {
        assert_eq!(root("http://127.0.0.1:8003/v1"), "http://127.0.0.1:8003");
        assert_eq!(root("http://127.0.0.1:8003/v1/"), "http://127.0.0.1:8003");
        assert_eq!(port_of("http://127.0.0.1:8003/v1"), Some(8003));
        assert_eq!(port_of("http://box/v1"), None);
    }
}
