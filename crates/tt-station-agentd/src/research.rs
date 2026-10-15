// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Research: a question in, a cited report out, run on the box.
//!
//! A job plans sub-questions, searches (through SearXNG on the box when it's
//! installed, else the web search the assistant uses), reads the pages it
//! finds (public pages only: `crate::web` refuses the box's own networks),
//! has the served model pull out what each page says about the question,
//! looks for gaps and searches again, then writes a report whose every claim
//! points at a numbered source. Reading many pages at once is the box's
//! strength (prefill), so pages are read and noted in parallel.
//!
//! Each step is saved in the job's folder (`research/<id>/`), so a job the
//! agent was restarted in the middle of carries on from its last step, and a
//! follow-up starts from the sources its parent already read. One job runs
//! at a time; the rest wait their turn. A finished job notifies.
//!
//! - `GET /research`: the jobs, newest first, and whether the box is ready (`check`).
//! - `POST /research`: `{question, depth: quick|standard|deep, parent?}` starts one.
//! - `GET /research/{id}`: the job, its progress, its sources and report.
//! - `POST /research/{id}/stop`; `DELETE /research/{id}`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use futures_util::stream::{self, StreamExt};
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

// ---------------------------------------------------------------- jobs on disk

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    Quick,
    Standard,
    Deep,
}

impl Depth {
    /// (sub-questions, results per search, rounds of searching, pages at most)
    fn shape(self) -> (usize, usize, usize, usize) {
        match self {
            Depth::Quick => (4, 4, 1, 12),
            Depth::Standard => (6, 5, 2, 30),
            Depth::Deep => (8, 6, 3, 60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub question: String,
    pub depth: Depth,
    #[serde(default)]
    pub parent: Option<String>,
    pub created: u64,
    /// queued | running | done | failed | stopped
    pub state: String,
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub finished: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// "searxng" (on the box) or "web" (DuckDuckGo).
    #[serde(default)]
    pub search: Option<String>,
    /// The report's first summary sentence, for lists and the notification.
    #[serde(default)]
    pub summary: Option<String>,
    /// A name the person gave the report, overriding the question in lists.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub round: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Source {
    pub n: usize,
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    /// The search that found it.
    pub query: String,
    /// read | failed | pending
    pub status: String,
    #[serde(default)]
    pub relevant: bool,
    /// What it says about the question: short claims, each with its quote.
    #[serde(default)]
    pub notes: Vec<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

fn root(state: &AppState) -> PathBuf {
    state.config_dir().join("research")
}

fn dir(state: &AppState, id: &str) -> PathBuf {
    root(state).join(id)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_json<T: Serialize>(path: &Path, v: &T) {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(tmp, path);
    }
}

fn load_job(state: &AppState, id: &str) -> Option<Job> {
    read_json(&dir(state, id).join("job.json"))
}

fn save_job(state: &AppState, job: &Job) {
    write_json(&dir(state, &job.id).join("job.json"), job);
}

fn sources(state: &AppState, id: &str) -> Vec<Source> {
    read_json(&dir(state, id).join("sources.json")).unwrap_or_default()
}

/// One line of progress, as a person reads it.
fn event(state: &AppState, id: &str, kind: &str, text: impl Into<String>) {
    use std::io::Write;
    let line = json!({ "ts": now(), "kind": kind, "text": text.into() }).to_string();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir(state, id).join("events.jsonl")) {
        let _ = writeln!(f, "{line}");
    }
}

fn events(state: &AppState, id: &str, last: usize) -> Vec<Value> {
    let text = std::fs::read_to_string(dir(state, id).join("events.jsonl")).unwrap_or_default();
    let all: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    all[all.len().saturating_sub(last)..].to_vec()
}

fn all_jobs(state: &AppState) -> Vec<Job> {
    let mut jobs: Vec<Job> = std::fs::read_dir(root(state))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| read_json(&e.path().join("job.json")))
        .collect();
    jobs.sort_by(|a, b| b.created.cmp(&a.created));
    jobs
}

// ---------------------------------------------------------------- the box's fitness

/// The served chat model, its context length, and the search the jobs will use.
pub async fn check(state: &AppState) -> Value {
    let life = state.lifecycle();
    let serving = matches!(life.phase, Phase::Serving) && state.endpoint().is_some();
    let (model, context) = if serving {
        let url = format!("http://127.0.0.1:{}/v1/models", state.serving_port());
        let v: Option<Value> = match reqwest::Client::new().get(url).timeout(Duration::from_secs(5)).send().await {
            Ok(r) => r.json().await.ok(),
            Err(_) => None,
        };
        let first = v.as_ref().map(|v| v["data"][0].clone()).unwrap_or(Value::Null);
        (
            first["id"].as_str().map(str::to_string).or(life.served_id.clone()).or(state.endpoint().map(|e| e.model)),
            first["max_model_len"].as_u64(),
        )
    } else {
        (None, None)
    };
    let searxng = searxng_up().await;
    let (ok, level, why) = match (serving, context) {
        (false, _) if matches!(life.phase, Phase::Starting) => (false, "wait", "A model is starting. Research can start once it's serving.".to_string()),
        (false, _) => (false, "block", "Research runs on the model the box serves, and nothing is serving. Qwen3-32B is a good choice.".to_string()),
        (true, Some(c)) if c < 16_384 => (false, "block", format!("{} remembers only {}K tokens; research needs at least 16K (32K or more is better).", short(&model), c / 1024)),
        (true, Some(c)) if c < 32_768 => (true, "warn", format!("{} remembers {}K tokens, so reports draw on fewer sources. A model with 32K or more does better.", short(&model), c / 1024)),
        _ => (true, "ok", String::new()),
    };
    json!({
        "ok": ok, "level": level, "why": why, "model": model, "context": context,
        "search": if searxng { "searxng" } else { "web" },
        "search_note": if searxng { "Searches go through SearXNG on the box." } else { "Searches go to DuckDuckGo. Install SearXNG (Connect → Research) to search privately through the box." },
    })
}

fn short(model: &Option<String>) -> String {
    model.as_deref().map(|m| m.rsplit('/').next().unwrap_or(m).to_string()).unwrap_or_else(|| "The model".into())
}

const SEARXNG: &str = "http://127.0.0.1:3024";

/// SearXNG answering on the box; started first if it's installed but stopped.
async fn searxng_up() -> bool {
    let probe = || async {
        reqwest::Client::new().get(format!("{SEARXNG}/config")).timeout(Duration::from_secs(3)).send().await.is_ok_and(|r| r.status().is_success())
    };
    if probe().await {
        return true;
    }
    if !tokio::task::spawn_blocking(|| crate::apps::ensure_running("searxng")).await.unwrap_or(false) {
        return false;
    }
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if probe().await {
            return true;
        }
    }
    false
}

#[derive(Debug, Clone)]
struct Hit {
    url: String,
    title: String,
}

/// Search: SearXNG on the box when it answers, else the assistant's web search.
async fn search(query: &str, n: usize) -> (Vec<Hit>, &'static str) {
    if let Ok(r) = reqwest::Client::new()
        .get(format!("{SEARXNG}/search"))
        .query(&[("q", query), ("format", "json")])
        .timeout(Duration::from_secs(25))
        .send()
        .await
    {
        if let Ok(v) = r.json::<Value>().await {
            let hits: Vec<Hit> = v["results"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| Some(Hit { url: x["url"].as_str()?.to_string(), title: x["title"].as_str().unwrap_or("").to_string() }))
                .take(n)
                .collect();
            if !hits.is_empty() {
                return (hits, "searxng");
            }
        }
    }
    let hits = crate::web::search(query, n).await.unwrap_or_default();
    (hits.into_iter().map(|h| Hit { url: h.url, title: h.title }).collect(), "web")
}

// ---------------------------------------------------------------- the model

struct Llm {
    http: reqwest::Client,
    url: String,
    model: String,
}

impl Llm {
    fn new(state: &AppState) -> Result<Self, String> {
        let ep = state.endpoint().ok_or("nothing is serving")?;
        Ok(Llm {
            http: reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).timeout(Duration::from_secs(900)).build().map_err(|e| e.to_string())?,
            url: crate::assistant::chat_completions_url(&ep.base_url),
            model: ep.model,
        })
    }

    async fn ask(&self, system: &str, user: &str, max_tokens: u32) -> Result<String, String> {
        let body = json!({
            "model": self.model,
            "messages": [{ "role": "system", "content": system }, { "role": "user", "content": user }],
            "temperature": 0.2,
            "max_tokens": max_tokens,
            "chat_template_kwargs": { "enable_thinking": false },
        });
        let r = self.http.post(&self.url).json(&body).send().await.map_err(|e| format!("couldn't reach the model: {e}"))?;
        let status = r.status();
        let v: Value = r.json().await.map_err(|e| format!("the model's answer wasn't readable: {e}"))?;
        if !status.is_success() {
            return Err(format!("the model answered {status}: {}", v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or("")));
        }
        Ok(crate::assistant::strip_think(v["choices"][0]["message"]["content"].as_str().unwrap_or("")))
    }

    /// Ask for JSON; the first `{...}` in the answer.
    async fn ask_json(&self, system: &str, user: &str, max_tokens: u32) -> Result<Value, String> {
        let text = self.ask(system, user, max_tokens).await?;
        first_json(&text).ok_or_else(|| format!("the model didn't answer in the expected form: {}", text.chars().take(120).collect::<String>()))
    }
}

/// The first JSON object in `text` (models wrap it in prose or code fences).
pub fn first_json(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| serde_json::from_str(&text[start..=end]).ok()).flatten()
}

fn strings(v: &Value, key: &str, max: usize) -> Vec<String> {
    v[key].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(|s| s.trim().to_string())).filter(|s| !s.is_empty()).take(max).collect()
}

// ---------------------------------------------------------------- the run

static STOPS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
/// One job on the model at a time.
static RUNNER: LazyLock<Arc<tokio::sync::Mutex<()>>> = LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

fn stopped(id: &str) -> bool {
    STOPS.lock().expect("research").contains(id)
}

fn set_step(state: &AppState, job: &mut Job, step: &str) {
    job.step = step.to_string();
    save_job(state, job);
}

/// Start (or carry on with) a job in the background.
fn spawn(state: AppState, id: String) {
    tokio::spawn(async move {
        let _turn = RUNNER.clone().lock_owned().await;
        let Some(mut job) = load_job(&state, &id) else { return };
        if job.state != "queued" && job.state != "running" {
            return;
        }
        job.state = "running".into();
        save_job(&state, &job);
        // Just after an agent restart the serving model is still being picked back up.
        for _ in 0..36 {
            if state.endpoint().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let result = run(&state, &mut job).await;
        let mut job = load_job(&state, &id).unwrap_or(job);
        job.finished = Some(now());
        match result {
            Ok(()) => {
                job.state = "done".into();
                job.step = "Done".into();
                event(&state, &id, "done", "The report is ready.");
                state.notifier().emit(crate::notify::Draft {
                    kind: "research",
                    title: format!("Report ready: {}", clip(&job.question, 60)),
                    body: job.summary.clone().unwrap_or_else(|| "Your research report is ready in TT Station.".into()),
                });
            }
            Err(e) if stopped(&id) => {
                job.state = "stopped".into();
                job.step = "Stopped".into();
                event(&state, &id, "info", format!("Stopped. ({e})"));
            }
            Err(e) => {
                job.state = "failed".into();
                job.step = "Didn't finish".into();
                job.error = Some(e.clone());
                event(&state, &id, "error", e.clone());
                state.notifier().emit(crate::notify::Draft {
                    kind: "research",
                    title: format!("Research didn't finish: {}", clip(&job.question, 50)),
                    body: e,
                });
            }
        }
        STOPS.lock().expect("research").remove(&id);
        save_job(&state, &job);
    });
}

/// A Library file as a source: its passages that match the question and the
/// search that found it (a long PDF's first pages may not be the part that matters).
async fn library_text(state: &AppState, src: &Source) -> Result<(String, Option<String>, String), String> {
    let name = src.title.clone().unwrap_or_default();
    let hits = crate::library::search(state, &src.query, 8, Some(&name)).await;
    let mut hits: Vec<_> = hits.into_iter().filter(|h| h.document == name).collect();
    if hits.is_empty() {
        return Err("That file is no longer in the Library.".into());
    }
    hits.sort_by_key(|h| h.passage);
    let text: String = hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>().join("\n\n[…]\n\n");
    Ok((src.url.clone(), Some(name), text.chars().take(16_000).collect()))
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>().trim_end()) }
}

const SYSTEM: &str = "You are the research engine on a private AI box. You are careful and factual. You only use what the sources say, and you say when sources disagree or are missing.";

async fn run(state: &AppState, job: &mut Job) -> Result<(), String> {
    let id = job.id.clone();
    let folder = dir(state, &id);
    let llm = Llm::new(state)?;
    job.model = Some(llm.model.clone());
    let (n_sub, per_search, rounds, max_pages) = job.depth.shape();
    let check_stop = || if stopped(&id) { Err("stopped".to_string()) } else { Ok(()) };

    // What a follow-up already knows.
    let prior: Vec<Source> = job.parent.as_deref().map(|p| sources(state, p).into_iter().filter(|s| s.relevant).collect()).unwrap_or_default();
    let prior_text = notes_text(&prior, 6000);

    // 1. Plan.
    let plan_path = folder.join("plan.json");
    let mut queries: Vec<String> = match read_json::<Value>(&plan_path) {
        Some(p) => strings(&p, "sub_questions", n_sub),
        None => {
            set_step(state, job, "Planning");
            event(state, &id, "plan", "Breaking the question into parts…");
            let ask = format!(
                "Question: {q}\n{prior}\nPlan the research. Return JSON only: {{\"title\": \"a short title for the report\", \"sub_questions\": [up to {n} specific web search queries that together answer the question]}}",
                q = job.question,
                prior = if prior_text.is_empty() { String::new() } else { format!("This follows earlier research, which found:\n{prior_text}\nFocus on what that doesn't answer yet.\n") },
                n = n_sub
            );
            let v = llm.ask_json(SYSTEM, &ask, 700).await?;
            let qs = strings(&v, "sub_questions", n_sub);
            if qs.is_empty() {
                return Err("the model didn't plan any searches".into());
            }
            write_json(&plan_path, &v);
            for q in &qs {
                event(state, &id, "plan", format!("Will look into: {q}"));
            }
            qs
        }
    };
    check_stop()?;

    // 2-4. Search, read, reflect; rounds already done are skipped.
    let mut all: Vec<Source> = sources(state, &id);
    for round in job.round..rounds {
        if round > 0 {
            set_step(state, job, "Looking for gaps");
            let ask = format!(
                "Question: {}\nWhat the research found so far:\n{}\n\nWhat's missing, unclear or contradictory? Return JSON only: {{\"enough\": true or false, \"gaps\": [short descriptions], \"queries\": [up to 4 new web search queries to fill them]}}",
                job.question,
                notes_text(&all, 9000)
            );
            let v = llm.ask_json(SYSTEM, &ask, 600).await?;
            for g in strings(&v, "gaps", 4) {
                event(state, &id, "gap", format!("Missing: {g}"));
            }
            queries = strings(&v, "queries", 4);
            if v["enough"] == true || queries.is_empty() {
                event(state, &id, "info", "Enough to write the report.");
                job.round = rounds;
                save_job(state, job);
                break;
            }
        }
        check_stop()?;
        set_step(state, job, if round == 0 { "Searching" } else { "Searching again" });
        let mut seen: HashSet<String> = all.iter().map(|s| s.url.clone()).chain(prior.iter().map(|s| s.url.clone())).collect();
        for q in &queries {
            check_stop()?;
            let (hits, via) = search(q, per_search).await;
            job.search = Some(via.to_string());
            event(state, &id, "search", format!("Searched \"{q}\": {} results", hits.len()));
            for h in hits {
                if all.len() >= max_pages || !seen.insert(h.url.clone()) {
                    continue;
                }
                all.push(Source { n: all.len() + 1, url: h.url, title: Some(h.title).filter(|t| !t.is_empty()), query: q.clone(), status: "pending".into(), ..Default::default() });
            }
            // The person's own files (the Library) too: a couple of the best per search.
            let mut mine = 0;
            for h in crate::library::search(state, q, 6, None).await {
                let url = match &h.path { Some(p) => format!("file://{p}"), None => format!("doc:{}", h.document) };
                if mine >= 2 || h.score < 0.5 || all.len() >= max_pages + 4 || !seen.insert(url.clone()) {
                    continue;
                }
                mine += 1;
                event(state, &id, "search", format!("Your Library: {}", h.document));
                all.push(Source { n: all.len() + 1, url, title: Some(h.document), query: q.clone(), status: "pending".into(), ..Default::default() });
            }
        }
        write_json(&folder.join("sources.json"), &all);
        check_stop()?;

        // Read and note every pending page, several at once (the box reads fast).
        let pending: Vec<usize> = all.iter().enumerate().filter(|(_, s)| s.status == "pending").map(|(i, _)| i).collect();
        set_step(state, job, &format!("Reading {} pages", pending.len()));
        let shared = Arc::new(Mutex::new(std::mem::take(&mut all)));
        let llm = Arc::new(Llm { http: llm.http.clone(), url: llm.url.clone(), model: llm.model.clone() });
        let question = job.question.clone();
        stream::iter(pending)
            .for_each_concurrent(8, |i| {
                let shared = shared.clone();
                let llm = llm.clone();
                let question = question.clone();
                let state = state.clone();
                let id = id.clone();
                async move {
                    if stopped(&id) {
                        return;
                    }
                    let src = shared.lock().expect("sources")[i].clone();
                    let read = if src.url.starts_with("file://") || src.url.starts_with("doc:") {
                        library_text(&state, &src).await
                    } else {
                        crate::web::read_limited(&src.url, 16_000).await
                    };
                    let mut out = src.clone();
                    match read {
                        Err(e) => {
                            out.status = "failed".into();
                            out.error = Some(e);
                        }
                        Ok((_, title, text)) => {
                            out.title = title.or(out.title);
                            let ask = format!(
                                "Question: {question}\nThis page was found searching for: {}\nPage ({}):\n\"\"\"\n{text}\n\"\"\"\n\nWhat does this page say that helps answer the question? Return JSON only: {{\"relevant\": true or false, \"notes\": [up to 6 items of {{\"claim\": \"one factual sentence\", \"quote\": \"the exact words from the page that support it\"}}]}}. Use only this page.",
                                out.query,
                                out.url
                            );
                            match llm.ask_json(SYSTEM, &ask, 900).await {
                                Ok(v) => {
                                    out.notes = v["notes"].as_array().cloned().unwrap_or_default().into_iter().take(6).collect();
                                    out.relevant = v["relevant"] == true && !out.notes.is_empty();
                                    out.status = "read".into();
                                    event(&state, &id, "read", format!("Read {}{}", out.title.clone().unwrap_or_else(|| out.url.clone()), if out.relevant { format!(": {} findings", out.notes.len()) } else { ": nothing relevant".into() }));
                                }
                                Err(e) => {
                                    out.status = "failed".into();
                                    out.error = Some(e);
                                }
                            }
                        }
                    }
                    let mut all = shared.lock().expect("sources");
                    all[i] = out;
                    write_json(&dir(&state, &id).join("sources.json"), &*all);
                }
            })
            .await;
        all = std::mem::take(&mut *shared.lock().expect("sources"));
        check_stop()?;
        job.round = round + 1;
        save_job(state, job);
    }

    // 5. Write, from the notes, citing sources by number.
    let mut cited: Vec<Source> = prior.clone();
    for s in all.iter().filter(|s| s.relevant) {
        let mut s = s.clone();
        s.n = cited.len() + 1;
        cited.push(s);
    }
    for (i, s) in cited.iter_mut().enumerate() {
        s.n = i + 1;
    }
    if cited.is_empty() {
        return Err("none of the pages it read had anything on the question. Try asking it differently.".into());
    }
    set_step(state, job, "Writing the report");
    event(state, &id, "write", format!("Writing the report from {} sources…", cited.len()));
    let title = read_json::<Value>(&plan_path).and_then(|p| p["title"].as_str().map(str::to_string)).unwrap_or_else(|| job.question.clone());
    let budget = 40_000;
    let ask = format!(
        "Question: {q}\n\nNumbered sources and what each says:\n{notes}\n\nWrite a research report in Markdown:\n- Start with \"# {title}\".\n- Then \"## Summary\": 3-5 sentences that answer the question directly.\n- Then sections with \"##\" headings covering the findings. Use a table when comparing several things.\n- Then \"## Where sources disagree or are missing\".\n- Sources marked THE PERSON'S OWN FILE are the asker's own documents (their policy, notes, contract...): say \"your ...\" and put what they say first, ahead of general web sources.\n- Cite every claim with its source number in brackets, like [3] or [2][5]. Only cite the numbers above. Don't invent facts or sources.\n- Don't add a sources list; it is added after.",
        q = job.question,
        notes = notes_text(&cited, budget),
    );
    let mut report = llm.ask(SYSTEM, &ask, 4000).await?;
    report = clean_citations(&report, cited.len());
    let mut list = String::from("\n\n## Sources\n\n");
    for s in &cited {
        list.push_str(&format!("{}. [{}]({})\n", s.n, s.title.clone().unwrap_or_else(|| s.url.clone()).replace(['[', ']'], ""), s.url));
    }
    report.push_str(&list);
    std::fs::write(folder.join("report.md"), &report).map_err(|e| e.to_string())?;
    write_json(&folder.join("cited.json"), &cited);
    job.summary = summary_of(&report);
    save_job(state, job);
    Ok(())
}

/// The notes as numbered lines, within about `max_chars`.
fn notes_text(sources: &[Source], max_chars: usize) -> String {
    let mut out = String::new();
    for s in sources.iter().filter(|s| s.relevant) {
        let whose = if s.url.starts_with("file://") || s.url.starts_with("doc:") { "THE PERSON'S OWN FILE" } else { s.url.as_str() };
        let mut block = format!("[{}] {} ({whose})\n", s.n, s.title.clone().unwrap_or_default());
        for n in &s.notes {
            if let Some(c) = n["claim"].as_str() {
                block.push_str(&format!("  - {c}\n"));
            }
        }
        if out.len() + block.len() > max_chars {
            break;
        }
        out.push_str(&block);
    }
    out
}

/// Drop citations to source numbers that don't exist (`[0]`, `[99]`).
pub fn clean_citations(text: &str, count: usize) -> String {
    static CITE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\[(\d{1,3})\]").expect("valid regex"));
    CITE.replace_all(text, |c: &regex::Captures| {
        let n: usize = c[1].parse().unwrap_or(0);
        if (1..=count).contains(&n) { c[0].to_string() } else { String::new() }
    })
    .to_string()
}

/// The first sentence of the report's summary.
fn summary_of(report: &str) -> Option<String> {
    let after = report.split("## Summary").nth(1)?;
    let para = after.trim_start().split("\n\n").next()?.trim();
    let first = para.split_inclusive(". ").next().unwrap_or(para);
    static CITE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\s*\[\d{1,3}\]").expect("valid regex"));
    Some(clip(CITE.replace_all(first, "").trim(), 200))
}

// ---------------------------------------------------------------- routes

#[derive(Debug, Deserialize)]
pub struct StartBody {
    pub question: String,
    #[serde(default)]
    pub depth: Option<Depth>,
    #[serde(default)]
    pub parent: Option<String>,
}

/// `GET /research`.
pub(crate) async fn get_research(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let c = check(&state).await;
    let st = state.clone();
    let jobs = tokio::task::spawn_blocking(move || all_jobs(&st)).await.unwrap_or_default();
    Json(json!({ "jobs": jobs, "check": c }))
}

/// Start a job (the route, and the assistant's `research` tool).
pub async fn start(state: &AppState, body: StartBody) -> Result<Job, (StatusCode, Json<Value>)> {
    let question = body.question.trim().to_string();
    if question.len() < 8 {
        return Err(err(StatusCode::BAD_REQUEST, "Ask a full question."));
    }
    if question.len() > 2000 {
        return Err(err(StatusCode::BAD_REQUEST, "That question is too long; keep it under 2,000 characters."));
    }
    if let Some(p) = &body.parent {
        if !valid_id(p) || load_job(state, p).is_none() {
            return Err(err(StatusCode::NOT_FOUND, "The report to follow up on isn't there."));
        }
    }
    let c = check(state).await;
    if c["ok"] != true {
        return Err(err(StatusCode::CONFLICT, c["why"].as_str().unwrap_or("The box isn't ready for research.").to_string()));
    }
    let id = format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]);
    std::fs::create_dir_all(dir(state, &id)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let job = Job {
        id: id.clone(),
        question,
        depth: body.depth.unwrap_or(Depth::Standard),
        parent: body.parent,
        created: now(),
        state: "queued".into(),
        step: "Waiting its turn".into(),
        finished: None,
        error: None,
        model: c["model"].as_str().map(str::to_string),
        search: c["search"].as_str().map(str::to_string),
        summary: None,
        title: None,
        round: 0,
    };
    save_job(state, &job);
    event(state, &id, "info", format!("Researching on {} with {} search.", short(&job.model), if c["search"] == "searxng" { "private (SearXNG)" } else { "web" }));
    spawn(state.clone(), id);
    Ok(job)
}

/// `POST /research`.
pub(crate) async fn post_research(State(state): State<AppState>, _auth: BearerAuth, Json(body): Json<StartBody>) -> Reply {
    let job = start(&state, body).await?;
    Ok(Json(json!({ "job": job })))
}

/// `GET /research/{id}`.
pub(crate) async fn get_job(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "No such report."));
    }
    let job = load_job(&state, &id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such report."))?;
    let folder = dir(&state, &id);
    let srcs = sources(&state, &id);
    let cited: Vec<Source> = read_json(&folder.join("cited.json")).unwrap_or_default();
    Ok(Json(json!({
        "job": job,
        "events": events(&state, &id, 200),
        "report": std::fs::read_to_string(folder.join("report.md")).ok(),
        "sources": if cited.is_empty() { srcs.clone() } else { cited },
        "counts": {
            "found": srcs.len(),
            "read": srcs.iter().filter(|s| s.status == "read").count(),
            "relevant": srcs.iter().filter(|s| s.relevant).count(),
            "failed": srcs.iter().filter(|s| s.status == "failed").count(),
        },
        "plan": read_json::<Value>(&folder.join("plan.json")),
    })))
}

/// `POST /research/{id}/stop`.
pub(crate) async fn post_stop(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let mut job = load_job(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such report."))?;
    if job.state == "queued" {
        job.state = "stopped".into();
        job.step = "Stopped".into();
        save_job(&state, &job);
    }
    STOPS.lock().expect("research").insert(id);
    Ok(Json(json!({ "stopping": true })))
}

/// `DELETE /research/{id}`.
pub(crate) async fn delete_job(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>) -> Reply {
    let job = load_job(&state, &id).filter(|_| valid_id(&id)).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such report."))?;
    if job.state == "running" || job.state == "queued" {
        return Err(err(StatusCode::CONFLICT, "Stop it first."));
    }
    std::fs::remove_dir_all(dir(&state, &id)).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({ "deleted": true })))
}

#[derive(Debug, serde::Deserialize)]
pub struct Rename {
    pub title: String,
}

/// `POST /research/{id}/rename`: give the report a name of your own.
pub(crate) async fn post_rename(State(state): State<AppState>, _auth: BearerAuth, UrlPath(id): UrlPath<String>, Json(b): Json<Rename>) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such report."));
    }
    let title = b.title.trim().to_string();
    if title.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "Give it a name."));
    }
    if title.len() > 120 {
        return Err(err(StatusCode::BAD_REQUEST, "Keep the name under 120 characters."));
    }
    let mut job = load_job(&state, &id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such report."))?;
    job.title = Some(title.clone());
    save_job(&state, &job);
    Ok(Json(json!({ "job": job })))
}

/// At startup: carry on with jobs the agent stopped in the middle of.
pub fn resume(state: &AppState) {
    for job in all_jobs(state).into_iter().rev().filter(|j| j.state == "running" || j.state == "queued") {
        event(state, &job.id, "info", "Picking up where it left off.");
        let mut j = job.clone();
        j.state = "queued".into();
        save_job(state, &j);
        spawn(state.clone(), job.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_json_wrapped_in_prose() {
        let v = first_json("Sure! ```json\n{\"sub_questions\": [\"a\", \"b\"]}\n``` done").unwrap();
        assert_eq!(strings(&v, "sub_questions", 5), ["a", "b"]);
        assert!(first_json("no json here").is_none());
    }

    #[test]
    fn drops_citations_to_missing_sources() {
        assert_eq!(clean_citations("A [1] B [7] C [2][3]", 3), "A [1] B  C [2][3]");
    }

    #[test]
    fn summary_is_the_first_sentence() {
        let r = "# T\n\n## Summary\n\nFoxes hunt in snow [1]. They hear mice [2].\n\n## More";
        assert_eq!(summary_of(r).unwrap(), "Foxes hunt in snow.");
    }
}
