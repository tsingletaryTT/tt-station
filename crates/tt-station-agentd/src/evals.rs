// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Model check-ups: how good is the model that's serving, at the things this
//! box uses it for -- and which of the models tried so far to use for what.
//!
//! One evaluation runs six, automatically graded test sets against the
//! served model, plus the speed benchmark ([`crate::bench`]):
//!
//! - **Tool use**: picks the right tool with the right arguments, or none when
//!   none fits (what Chat, Tori, Routines and Code rely on).
//! - **Code**: writes Python functions; each is run against tests in the Code
//!   sandbox image, with no network.
//! - **Reasoning**: short problems with one right answer.
//! - **Following instructions**: JSON only, exact bullet counts, word limits.
//! - **Long documents**: finds one fact hidden at different depths of a long
//!   text sized to the model's context.
//! - **Agent reliability**: valid tool calls in Code-like sessions from 8K to
//!   120K tokens ([`crate::reliability`]), and from them the window Code
//!   sessions on this model compact at ([`compact_window`]).
//!
//! Results are kept per model (`config/evals/`), and [`recommend`] compares
//! the latest of each: best for Chat, for Code, for Research, and fastest.
//! A routine can step through several models overnight with its
//! `evaluate_model` tool.
//!
//! - `GET /evals`, `POST /evals`, `POST /evals/stop`
//! - `GET /evals/{id}`, `DELETE /evals/{id}` (with its trials file)
//!
//! A check-up holds the served model for minutes (about fifteen with agent
//! reliability), so starting and stopping one follows who may start and stop
//! models (`crate::model_control`, applied in the router).
//!
//! An evaluation in progress is saved after each step (the speed benchmark,
//! then each category). If the agent restarts mid-way, [`resume`] picks it up
//! at the next category when the same model is serving again (an agent restart
//! leaves the model running, and it's adopted). Otherwise -- a different
//! model, or nothing serving within a few minutes -- it's marked `interrupted`,
//! keeping the categories it finished, and isn't used for recommendations.
//! Resuming is better than starting over because a full check-up is tens of
//! minutes of chip time; falling back to `interrupted` keeps it honest, since
//! scores from two different models must never be mixed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth};

type Reply = Result<Json<Value>, ApiError>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

const SANDBOX_IMAGE: &str = "tt-code-sandbox:3";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Case {
    pub name: String,
    pub ok: bool,
    /// What went wrong, or what it answered.
    pub detail: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Category {
    pub key: String,
    pub label: String,
    pub passed: usize,
    pub total: usize,
    pub cases: Vec<Case>,
}

impl Category {
    pub fn score(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.passed as f64 / self.total as f64
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Speed {
    pub ttft_s: Option<f64>,
    pub single_tps: Option<f64>,
    pub batch_tps: Option<f64>,
    pub wh_per_million_tokens: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Eval {
    pub id: String,
    pub model: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    /// running | done | failed | stopped
    pub status: String,
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub speed: Speed,
    #[serde(default)]
    pub categories: Vec<Category>,
    /// 0..1, the categories' mean.
    #[serde(default)]
    pub overall: Option<f64>,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The agent reliability measurements, per prompt size.
    #[serde(default)]
    pub reliability: Option<crate::reliability::Report>,
    /// Agent reliability was asked for (so a resumed run still does it).
    #[serde(default)]
    pub agent: bool,
    /// How many times it was picked up again after an agent restart.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub resumed: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What `crate::evals` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// The evaluation running now, if any.
    current: Mutex<Option<Eval>>,
    /// Asked to stop the running evaluation.
    stop: AtomicBool,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().evals
}

fn dir(state: &AppState) -> PathBuf {
    state.config_dir().join("evals")
}

fn save(state: &AppState, e: &Eval) {
    save_in(&dir(state), e);
}

/// Writes an evaluation where [`all`] finds it (`evals/<id>.json`).
pub fn save_in(d: &std::path::Path, e: &Eval) {
    crate::fsio::save_json(&d.join(format!("{}.json", e.id)), e, crate::fsio::PRIVATE);
}

/// Every graded request of an agent reliability run, next to it (`evals/raw/<id>.json`).
pub fn save_trials(d: &std::path::Path, id: &str, trials: &[crate::reliability::Trial]) {
    crate::fsio::save_json(
        &d.join("raw").join(format!("{id}.json")),
        trials,
        crate::fsio::PRIVATE,
    );
}

pub fn new_id() -> String {
    format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6])
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn all(state: &AppState) -> Vec<Eval> {
    all_in(&dir(state))
}

/// Every evaluation saved in `d` (one `<id>.json` each), newest first. A file
/// that won't parse is moved aside rather than dropped (`crate::fsio`).
pub fn all_in(d: &std::path::Path) -> Vec<Eval> {
    let mut out: Vec<Eval> = std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| crate::fsio::read_json_or_backup(&p))
        .collect();
    out.sort_by_key(|e: &Eval| std::cmp::Reverse(e.started));
    out
}

fn progress(state: &AppState, f: impl FnOnce(&mut Eval)) {
    if let Some(e) = mine(state).current.lock().expect("evals").as_mut() {
        f(e);
    }
}

/// Save the evaluation in progress as it stands, so a restart can resume it.
fn checkpoint(state: &AppState) {
    let current = mine(state).current.lock().expect("evals").clone();
    if let Some(e) = current {
        save(state, &e);
    }
}

/// The categories an evaluation runs, in order: (key, what it says meanwhile).
pub const CATEGORIES: [(&str, &str); 5] = [
    ("tools", "Testing tool use"),
    ("code", "Testing code"),
    ("reasoning", "Testing reasoning"),
    ("instructions", "Testing instructions"),
    ("long", "Testing long documents"),
];

/// What to do with an evaluation that was running when the agent stopped.
#[derive(Debug, PartialEq)]
pub enum AfterRestart {
    /// The same model is serving: carry on at the next category.
    Resume,
    /// Something else (or nothing) is serving: keep what it finished.
    Interrupt,
}

pub fn after_restart(saved: &Eval, serving: Option<&str>) -> AfterRestart {
    if serving == Some(saved.model.as_str()) {
        AfterRestart::Resume
    } else {
        AfterRestart::Interrupt
    }
}

/// Mark an evaluation cut short by a restart, keeping its finished categories.
pub fn interrupt(e: &mut Eval) {
    e.status = "interrupted".into();
    e.finished = Some(now());
    e.activity = None;
    e.overall = None;
    e.error = Some(format!(
        "TT Station restarted during this check-up and {} wasn't serving afterwards; \
         {} of {} tests finished.",
        short(&e.model),
        e.categories.iter().filter(|c| c.key != "agent").count(),
        CATEGORIES.len()
    ));
}

/// At agent start: an evaluation that was running is resumed or marked
/// interrupted (see the module docs). Waits a few minutes for the model that
/// was serving to be adopted first.
pub async fn resume(state: AppState) {
    resume_within(state, Duration::from_secs(180)).await
}

/// [`resume`], waiting at most `wait` for the model to be serving again.
pub async fn resume_within(state: AppState, wait: Duration) {
    let saved: Vec<Eval> = all(&state)
        .into_iter()
        .filter(|e| e.status == "running")
        .collect();
    if saved.is_empty() {
        return;
    }
    let deadline = Instant::now() + wait;
    let mut serving = state.endpoint();
    while Instant::now() < deadline
        && !saved
            .iter()
            .any(|e| serving.as_ref().is_some_and(|ep| ep.model == e.model))
    {
        tokio::time::sleep(Duration::from_secs(5)).await;
        serving = state.endpoint();
    }
    // The newest one may carry on; any other is interrupted.
    let mut resumable: Option<Eval> = None;
    for mut e in saved {
        if resumable.is_none()
            && after_restart(&e, serving.as_ref().map(|ep| ep.model.as_str()))
                == AfterRestart::Resume
        {
            resumable = Some(e);
            continue;
        }
        interrupt(&mut e);
        save(&state, &e);
    }
    let Some(mut e) = resumable else { return };
    let Some(ep) = serving else { return };
    {
        let mut cur = mine(&state).current.lock().expect("evals");
        if cur.as_ref().is_some_and(|c| c.status == "running") {
            drop(cur);
            interrupt(&mut e);
            save(&state, &e);
            return;
        }
        e.resumed += 1;
        e.activity = Some("Picking up where it left off".into());
        *cur = Some(e.clone());
    }
    eprintln!(
        "tt-station-agentd: evaluation {}: resuming after {} of {} tests",
        e.id,
        e.categories.len(),
        CATEGORIES.len()
    );
    if let Err(why) = drive(&state, &ep.base_url, &ep.model, e.context, e.agent).await {
        eprintln!("tt-station-agentd: evaluation: {why}");
    }
}

// ---------------------------------------------------------------- talking to the model

struct Model {
    http: reqwest::Client,
    url: String,
    model: String,
}

impl Model {
    async fn chat(&self, body: Value) -> Result<Value, String> {
        let mut body = body;
        body["model"] = json!(self.model);
        body["temperature"] = json!(0.0);
        body["chat_template_kwargs"] = json!({ "enable_thinking": false });
        let r = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("couldn't reach the model: {e}"))?;
        let status = r.status();
        let v: Value = r
            .json()
            .await
            .map_err(|e| format!("unreadable answer: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "{status}: {}",
                v["error"]["message"]
                    .as_str()
                    .or(v["message"].as_str())
                    .unwrap_or("")
            ));
        }
        Ok(v["choices"][0]["message"].clone())
    }

    async fn ask(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        let m = self.chat(json!({ "messages": [{ "role": "user", "content": prompt }], "max_tokens": max_tokens })).await?;
        Ok(
            crate::assistant::strip_think(m["content"].as_str().unwrap_or(""))
                .trim()
                .to_string(),
        )
    }
}

fn clip(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

// ---------------------------------------------------------------- reasoning

const REASONING: &[(&str, &str, &[&str])] = &[
    ("Bat and ball", "A bat and a ball cost $1.10 in total. The bat costs $1.00 more than the ball. How much does the ball cost, in cents?", &["5"]),
    ("Widgets", "If 3 machines make 3 widgets in 3 minutes, how many minutes do 100 machines take to make 100 widgets?", &["3"]),
    ("Multiplication", "What is 17 × 23?", &["391"]),
    ("Trip time", "A train leaves at 9:40 and arrives at 13:15 the same day. How many minutes is the trip?", &["215"]),
    ("Counting letters", "How many times does the letter r appear in the words \"strawberry raspberry\"?", &["6"]),
    ("Ordering", "Alice is older than Bob. Carol is younger than Bob. Dave is older than Alice. Who is the second youngest?", &["bob"]),
    ("Calendar", "If today is Wednesday, what day of the week will it be 100 days from now?", &["friday"]),
    ("Geometry", "A rectangle has a perimeter of 30 and one side of length 6. What is its area?", &["54"]),
    ("Median", "What is the median of 12, 7, 3, 19, 8?", &["8"]),
    ("Capital", "What is the capital city of Australia?", &["canberra"]),
    ("Up and down", "A store raises a price by 20%, then lowers the new price by 20%. The final price is $96. What was the original price, in dollars?", &["100"]),
    ("Either but not both", "How many positive integers less than 100 are divisible by 3 or by 5, but not by both?", &["40"]),
    ("Socks", "A drawer has 3 red, 4 blue and 5 green socks. In the dark, how many socks must I take out to be sure two of them are the same color?", &["4"]),
    ("Digit sum", "What is the sum of the digits of 2 to the power 15?", &["26"]),
    ("Snail", "A snail climbs 3 m up a 10 m well each day and slips back 2 m each night. On which day does it reach the top?", &["8"]),
    ("Handshakes", "Everyone in a room shook hands with everyone else exactly once: 66 handshakes in all. How many people are in the room?", &["12"]),
    ("Nines", "Writing out every whole number from 1 to 100, how many times do you write the digit 9?", &["20"]),
    ("Sequence", "What number comes next: 2, 6, 12, 20, 30, ...?", &["42"]),
];

/// The text after the last "ANSWER:", lowercased, stripped of punctuation.
fn final_answer(text: &str) -> String {
    let lower = text.to_lowercase();
    let tail = lower.rsplit("answer:").next().unwrap_or(&lower);
    tail.trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .replace(['$', ',', '*'], "")
        .trim()
        .to_string()
}

fn answer_matches(text: &str, want: &[&str]) -> bool {
    let a = final_answer(text);
    let first = a
        .split(|c: char| c.is_whitespace() || c == '.')
        .next()
        .unwrap_or("");
    want.iter()
        .any(|w| first == *w || a == *w || a.starts_with(&format!("{w} ")))
}

async fn reasoning(m: &Arc<Model>) -> Category {
    let cases = futures_util::stream::iter(0..REASONING.len())
        .map(|i| {
            let m = m.clone();
            async move {
            let (name, q, want) = REASONING[i];
            let t = Instant::now();
            let prompt = format!("{q}\n\nWork it out briefly, then end with a final line of the form \"ANSWER: <answer>\" (just the number or word).");
            match m.ask(&prompt, 600).await {
                Ok(a) if !a.to_lowercase().contains("answer:") => Case { name: name.to_string(), ok: false, detail: format!("Never gave a final answer (right answer {})", want[0]), seconds: t.elapsed().as_secs_f64() },
                Ok(a) => Case { name: name.to_string(), ok: answer_matches(&a, want), detail: format!("Answered {}; right answer {}", clip(&final_answer(&a), 40), want[0]), seconds: t.elapsed().as_secs_f64() },
                Err(e) => Case { name: name.to_string(), ok: false, detail: e, seconds: t.elapsed().as_secs_f64() },
            }
            }
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    category("reasoning", "Reasoning", cases)
}

fn category(key: &str, label: &str, cases: Vec<Case>) -> Category {
    Category {
        key: key.into(),
        label: label.into(),
        passed: cases.iter().filter(|c| c.ok).count(),
        total: cases.len(),
        cases,
    }
}

// ---------------------------------------------------------------- instructions

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

type Check = fn(&str) -> Result<(), String>;

const INSTRUCTIONS: &[(&str, &str, Check)] = &[
    ("JSON array only", "List the three primary colors of paint as a JSON array of strings. Output only the JSON, nothing else.", |a| {
        match serde_json::from_str::<Vec<String>>(strip_fences(a)) {
            Ok(v) if v.len() == 3 => Ok(()),
            Ok(v) => Err(format!("{} items", v.len())),
            Err(_) => Err("not just a JSON array".into()),
        }
    }),
    ("JSON object only", "Make up a person and give a JSON object with exactly two keys: \"name\" (a string) and \"age\" (a number). Output only the JSON.", |a| {
        let v: Value = serde_json::from_str(strip_fences(a)).map_err(|_| "not just JSON".to_string())?;
        let o = v.as_object().ok_or("not an object")?;
        (o.len() == 2 && o.get("name").is_some_and(Value::is_string) && o.get("age").is_some_and(Value::is_number)).then_some(()).ok_or_else(|| "wrong keys or types".into())
    }),
    ("Exactly three bullets", "Describe the ocean in exactly 3 bullet points, each starting with \"- \". Write nothing else.", |a| {
        let lines: Vec<&str> = a.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let bullets = lines.iter().filter(|l| l.starts_with("- ")).count();
        (bullets == 3 && lines.len() == 3).then_some(()).ok_or_else(|| format!("{bullets} bullets in {} lines", lines.len()))
    }),
    ("All lowercase", "Answer using only lowercase letters (no capitals at all): what is the largest planet in our solar system?", |a| {
        if a.chars().any(char::is_uppercase) { return Err("used capitals".into()) }
        a.contains("jupiter").then_some(()).ok_or_else(|| "didn't say jupiter".into())
    }),
    ("Under 15 words", "In fewer than 15 words, what is photosynthesis?", |a| {
        let n = a.split_whitespace().count();
        (n < 15).then_some(()).ok_or_else(|| format!("{n} words"))
    }),
    ("Sorted JSON", "Output only a JSON object with one key, \"items\": a list of exactly 4 fruits in alphabetical order.", |a| {
        let v: Value = serde_json::from_str(strip_fences(a)).map_err(|_| "not just JSON".to_string())?;
        let items: Vec<String> = v["items"].as_array().ok_or("no items list")?.iter().filter_map(|x| x.as_str().map(str::to_lowercase)).collect();
        let mut sorted = items.clone();
        sorted.sort();
        (items.len() == 4 && items == sorted).then_some(()).ok_or_else(|| format!("{items:?}"))
    }),
    ("Reverse the words", "Write this sentence with its words in reverse order, and nothing else: the quick brown fox jumps", |a| {
        let words: Vec<String> = a.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase()).filter(|w| !w.is_empty()).collect();
        (words == ["jumps", "fox", "brown", "quick", "the"]).then_some(()).ok_or_else(|| format!("\"{}\"", clip(a, 40)))
    }),
    ("Yes or no", "Answer only yes or no: is 91 a prime number?", |a| {
        (a.trim().trim_matches(|c: char| !c.is_alphabetic()).eq_ignore_ascii_case("no")).then_some(()).ok_or_else(|| format!("\"{}\"", clip(a, 30)))
    }),
    ("One word", "Answer with a single word only: what color is a clear daytime sky?", |a| {
        let w: Vec<&str> = a.split_whitespace().collect();
        (w.len() == 1 && w[0].to_lowercase().trim_matches(|c: char| !c.is_alphabetic()) == "blue").then_some(()).ok_or_else(|| format!("\"{}\"", clip(a, 30)))
    }),
];

async fn instructions(m: &Arc<Model>) -> Category {
    let cases = futures_util::stream::iter(0..INSTRUCTIONS.len())
        .map(|i| {
            let m = m.clone();
            async move {
                let (name, q, check) = INSTRUCTIONS[i];
                let t = Instant::now();
                let (ok, detail) = match m.ask(q, 300).await {
                    Ok(a) => match check(&a) {
                        Ok(()) => (true, clip(&a, 80)),
                        Err(why) => (false, format!("{why}: {}", clip(&a, 80))),
                    },
                    Err(e) => (false, e),
                };
                Case {
                    name: name.to_string(),
                    ok,
                    detail,
                    seconds: t.elapsed().as_secs_f64(),
                }
            }
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    category("instructions", "Following instructions", cases)
}

// ---------------------------------------------------------------- tools

fn tools() -> Value {
    let f = |name: &str, desc: &str, props: Value, required: &[&str]| json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": { "type": "object", "properties": props, "required": required } } });
    json!([
        f(
            "get_weather",
            "Current weather for a city.",
            json!({ "city": { "type": "string" }, "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] } }),
            &["city"]
        ),
        f(
            "set_timer",
            "Start a countdown timer.",
            json!({ "minutes": { "type": "integer" } }),
            &["minutes"]
        ),
        f(
            "search_documents",
            "Search the user's own files and notes.",
            json!({ "query": { "type": "string" } }),
            &["query"]
        ),
        f(
            "send_email",
            "Send an email for the user.",
            json!({ "to": { "type": "string" }, "subject": { "type": "string" }, "body": { "type": "string" } }),
            &["to", "subject", "body"]
        ),
        f(
            "calculator",
            "Evaluate an arithmetic expression exactly.",
            json!({ "expression": { "type": "string" } }),
            &["expression"]
        ),
    ])
}

/// (the tool calls made, the reply text)
type ToolCheck = fn(&[(String, Value)], &str) -> Result<(), String>;

fn one<'a>(calls: &'a [(String, Value)], name: &str) -> Result<&'a Value, String> {
    match calls {
        [(n, a)] if n == name => Ok(a),
        [] => Err(format!("didn't call {name}")),
        _ => Err(format!(
            "called {}",
            calls
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn arg(a: &Value, k: &str) -> String {
    match &a[k] {
        Value::String(s) => s.to_lowercase(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

const TOOLS: &[(&str, &str, ToolCheck)] = &[
    (
        "Weather, with unit",
        "What's the weather in Paris right now, in celsius?",
        |c, _| {
            let a = one(c, "get_weather")?;
            (arg(a, "city").contains("paris") && arg(a, "unit") == "celsius")
                .then_some(())
                .ok_or_else(|| format!("arguments {a}"))
        },
    ),
    ("Timer", "Set a timer for 25 minutes.", |c, _| {
        let a = one(c, "set_timer")?;
        (a["minutes"].as_f64() == Some(25.0) || a["minutes"] == "25")
            .then_some(())
            .ok_or_else(|| format!("arguments {a}"))
    }),
    (
        "Own documents",
        "What does my lease say about having pets?",
        |c, _| {
            let a = one(c, "search_documents")?;
            (!arg(a, "query").trim().is_empty())
                .then_some(())
                .ok_or_else(|| "empty query".into())
        },
    ),
    (
        "Email",
        "Email bob@example.com with the subject \"Lunch\" saying I'll be 10 minutes late.",
        |c, _| {
            let a = one(c, "send_email")?;
            (arg(a, "to") == "bob@example.com"
                && arg(a, "subject").contains("lunch")
                && arg(a, "body").contains("late"))
            .then_some(())
            .ok_or_else(|| format!("arguments {a}"))
        },
    ),
    (
        "Calculator",
        "Use the calculator: what is 1234 × 5678?",
        |c, _| {
            let a = one(c, "calculator")?;
            let e = arg(a, "expression");
            (e.contains("1234") && e.contains("5678"))
                .then_some(())
                .ok_or_else(|| format!("arguments {a}"))
        },
    ),
    (
        "Two cities at once",
        "What's the weather in Tokyo and in Osaka?",
        |c, _| {
            let cities: Vec<String> = c
                .iter()
                .filter(|(n, _)| n == "get_weather")
                .map(|(_, a)| arg(a, "city"))
                .collect();
            (cities.iter().any(|x| x.contains("tokyo"))
                && cities.iter().any(|x| x.contains("osaka")))
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "asked for {}",
                    if cities.is_empty() {
                        "nothing".into()
                    } else {
                        cities.join(", ")
                    }
                )
            })
        },
    ),
    (
        "No tool for small talk",
        "Hi! How's your day going?",
        |c, _| {
            c.is_empty()
                .then_some(())
                .ok_or_else(|| format!("called {}", c[0].0))
        },
    ),
    (
        "No tool for general knowledge",
        "What's the chemical symbol for gold?",
        |c, _| {
            c.is_empty()
                .then_some(())
                .ok_or_else(|| format!("called {}", c[0].0))
        },
    ),
    (
        "Converts units",
        "Set a timer for an hour and a half.",
        |c, _| {
            let a = one(c, "set_timer")?;
            (a["minutes"].as_f64() == Some(90.0) || a["minutes"] == "90")
                .then_some(())
                .ok_or_else(|| format!("arguments {a}"))
        },
    ),
    (
        "Doesn't invent an address",
        "Email my landlord that the sink is fixed.",
        |c, _| match c {
            [] => Ok(()),
            [(n, _)] if n == "search_documents" => Ok(()),
            _ => Err(format!(
                "called {}",
                c.iter()
                    .map(|(n, a)| format!("{n}({a})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        },
    ),
    ("Uses a tool's result", "FOLLOWUP", |c, text| {
        if !c.is_empty() {
            return Err(format!("called {} again", c[0].0));
        }
        let t = text.to_lowercase();
        (t.contains("coat") || t.contains("warm"))
            .then_some(())
            .ok_or_else(|| format!("\"{}\"", clip(text, 60)))
    }),
];

async fn tool_use(m: &Arc<Model>) -> Category {
    let tools = tools();
    let cases = futures_util::stream::iter(0..TOOLS.len())
        .map(|i| {
            let tools = tools.clone();
            let m = m.clone();
            async move {
                let (name, q, check) = TOOLS[i];
                let t = Instant::now();
                let system = json!({ "role": "system", "content": "You are a helpful assistant. Use a tool when it's needed to answer; otherwise just answer. Never make up details the user didn't give." });
                let messages = if q == "FOLLOWUP" {
                    // A turn after a tool already answered: use what it said.
                    json!([system,
                        { "role": "user", "content": "What's the weather in Berlin?" },
                        { "role": "assistant", "content": "", "tool_calls": [{ "id": "call_1", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\": \"Berlin\", \"unit\": \"celsius\"}" } }] },
                        { "role": "tool", "tool_call_id": "call_1", "content": "{\"temperature\": -6, \"sky\": \"light snow\", \"wind_kmh\": 25}" },
                        { "role": "user", "content": "Should I take an umbrella or a warm coat?" }])
                } else {
                    json!([system, { "role": "user", "content": q }])
                };
                let body = json!({ "messages": messages, "tools": tools, "tool_choice": "auto", "max_tokens": 400 });
                let (ok, detail) = match m.chat(body).await {
                    Ok(msg) => {
                        let calls: Vec<(String, Value)> = msg["tool_calls"].as_array().into_iter().flatten().map(|c| {
                            let args = c["function"]["arguments"].as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
                            (c["function"]["name"].as_str().unwrap_or("").to_string(), args)
                        }).collect();
                        let text = crate::assistant::strip_think(msg["content"].as_str().unwrap_or(""));
                        match check(&calls, &text) {
                            Ok(()) => (true, if calls.is_empty() { "Answered directly".into() } else { calls.iter().map(|(n, a)| format!("{n}({a})")).collect::<Vec<_>>().join(", ") }),
                            Err(why) => (false, why),
                        }
                    }
                    Err(e) => (false, e),
                };
                Case { name: name.to_string(), ok, detail: clip(&detail, 160), seconds: t.elapsed().as_secs_f64() }
            }
        })
        .buffered(4)
        .collect::<Vec<_>>()
        .await;
    category("tools", "Tool use", cases)
}

// ---------------------------------------------------------------- code

const CODE: &[(&str, &str, &str)] = &[
    ("Palindrome", "Write a Python function `is_palindrome(s: str) -> bool` that ignores case and any character that isn't a letter or digit.",
     "assert is_palindrome('A man, a plan, a canal: Panama')\nassert not is_palindrome('hello')\nassert is_palindrome('')\nassert is_palindrome('No 1on')"),
    ("FizzBuzz", "Write a Python function `fizzbuzz(n: int) -> list[str]` returning the FizzBuzz strings for 1..n (\"Fizz\" for multiples of 3, \"Buzz\" for 5, \"FizzBuzz\" for both, else the number).",
     "assert fizzbuzz(5) == ['1','2','Fizz','4','Buzz']\nassert fizzbuzz(15)[-1] == 'FizzBuzz'\nassert fizzbuzz(0) == []"),
    ("Merge intervals", "Write a Python function `merge_intervals(intervals: list[list[int]]) -> list[list[int]]` that merges overlapping (or touching) intervals and returns them sorted by start.",
     "assert merge_intervals([[1,3],[2,6],[8,10],[15,18]]) == [[1,6],[8,10],[15,18]]\nassert merge_intervals([[1,4],[4,5]]) == [[1,5]]\nassert merge_intervals([]) == []\nassert merge_intervals([[5,7],[1,2]]) == [[1,2],[5,7]]"),
    ("Roman numerals", "Write a Python function `roman_to_int(s: str) -> int` converting a Roman numeral to an integer.",
     "assert roman_to_int('III') == 3\nassert roman_to_int('LVIII') == 58\nassert roman_to_int('MCMXCIV') == 1994\nassert roman_to_int('IV') == 4"),
    ("Word counts", "Write a Python function `top_words(text: str, k: int) -> list[tuple[str, int]]` returning the k most common words (lowercased; words are runs of letters) with their counts, most common first, ties broken alphabetically.",
     "assert top_words('the cat and the hat and the bat', 2) == [('the', 3), ('and', 2)]\nassert top_words('B a b A c', 3) == [('a', 2), ('b', 2), ('c', 1)]\nassert top_words('', 2) == []"),
    ("Flatten", "Write a Python function `flatten(x)` that flattens arbitrarily nested lists into one flat list, keeping order.",
     "assert flatten([1,[2,[3,[4]],5]]) == [1,2,3,4,5]\nassert flatten([]) == []\nassert flatten([[[]]]) == []\nassert flatten(['ab',['c']]) == ['ab','c']"),
    ("Expression evaluator", "Write a Python function `evaluate(expr: str) -> float` that evaluates arithmetic with + - * /, parentheses, unary minus and spaces, with the usual precedence. Don't use eval or exec.",
     "assert evaluate('2+3*4') == 14\nassert evaluate('(2+3)*4') == 20\nassert evaluate('10/4') == 2.5\nassert evaluate('-3 + 5') == 2\nassert evaluate('2*(3+(4-1))*2') == 24\nassert evaluate('8-3-2') == 3\nassert evaluate('-(2+1)*2') == -6"),
    ("LRU cache", "Write a Python class `LRUCache` with `__init__(self, capacity: int)`, `get(self, key) -> int` (returns -1 if missing) and `put(self, key, value)`, evicting the least recently used key when over capacity. Both get and put count as use.",
     "c = LRUCache(2)\nc.put(1, 1); c.put(2, 2)\nassert c.get(1) == 1\nc.put(3, 3)\nassert c.get(2) == -1\nc.put(4, 4)\nassert c.get(1) == -1 and c.get(3) == 3 and c.get(4) == 4\nc.put(3, 30)\nc.put(5, 5)\nassert c.get(4) == -1 and c.get(3) == 30"),
    ("Grid path", "Write a Python function `shortest_path(grid: list[list[int]]) -> int` giving the fewest steps (up/down/left/right) from the top-left to the bottom-right cell, where 1 is a wall and 0 is open; -1 if there's no way through. A 1x1 open grid takes 0 steps.",
     "assert shortest_path([[0,0,0],[1,1,0],[0,0,0]]) == 4\nassert shortest_path([[0,1],[1,0]]) == -1\nassert shortest_path([[0]]) == 0\nassert shortest_path([[0,0,0,0],[1,1,1,0],[0,0,0,0],[0,1,1,1],[0,0,0,0]]) == 13\nassert shortest_path([[1,0],[0,0]]) == -1"),
    ("Longest palindrome", "Write a Python function `longest_palindrome(s: str) -> str` returning the longest palindromic substring of s (the first one if there's a tie).",
     "assert longest_palindrome('cbbd') == 'bb'\nassert longest_palindrome('forgeeksskeegfor') == 'geeksskeeg'\nassert longest_palindrome('a') == 'a'\nassert longest_palindrome('') == ''\nassert longest_palindrome('abc') == 'a'"),
];

fn python_block(answer: &str) -> String {
    let mut blocks = answer.split("```").skip(1).step_by(2).map(|b| {
        b.strip_prefix("python")
            .or_else(|| b.strip_prefix("py"))
            .unwrap_or(b)
            .to_string()
    });
    blocks.next().unwrap_or_else(|| answer.to_string())
}

/// Runs the model's code and the tests in the sandbox image: no network, small
/// limits, gone afterwards.
async fn run_python(code: &str, tests: &str) -> Result<(), String> {
    // eval and exec are switched off after the model's code is defined, so its
    // functions can't lean on them (an "evaluator" that calls eval passes nothing).
    let program = format!(
        "{code}\n\nimport builtins as _b\n_b.eval = _b.exec = None\n{tests}\nprint('ok')\n"
    );
    let mut cmd = tokio::process::Command::new("docker");
    cmd.args([
        "run",
        "--rm",
        "-i",
        "--network",
        "none",
        "--memory",
        "256m",
        "--pids-limit",
        "64",
        "--cpus",
        "1",
        "--read-only",
        "--tmpfs",
        "/tmp",
        SANDBOX_IMAGE,
        "timeout",
        "10",
        "python3",
        "-",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start the sandbox: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin.write_all(program.as_bytes()).await;
    }
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| "took too long".to_string())?
        .map_err(|e| e.to_string())?;
    if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("ok") {
        Ok(())
    } else {
        let e = String::from_utf8_lossy(&out.stderr);
        let last = e
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        Err(if last.trim() == "AssertionError" {
            "a test failed".into()
        } else {
            clip(last, 120)
        })
    }
}

async fn code(m: &Arc<Model>) -> Category {
    let cases = futures_util::stream::iter(0..CODE.len())
        .map(|i| {
            let m = m.clone();
            async move {
            let (name, task, tests) = CODE[i];
            let t = Instant::now();
            let prompt = format!("{task}\n\nReply with only the code in one ```python block. Standard library only.");
            let (ok, detail) = match m.ask(&prompt, 900).await {
                Ok(a) => match run_python(&python_block(&a), tests).await {
                    Ok(()) => (true, "Passed its tests".into()),
                    Err(why) => (false, why),
                },
                Err(e) => (false, e),
            };
            Case { name: name.to_string(), ok, detail, seconds: t.elapsed().as_secs_f64() }
            }
        })
        .buffered(3)
        .collect::<Vec<_>>()
        .await;
    category("code", "Code", cases)
}

// ---------------------------------------------------------------- long documents

const FILLER: &[&str] = &[
    "The quarterly shipment to warehouse {n} arrived on schedule and was logged by the night crew.",
    "Meeting notes {n}: the committee reviewed the budget and postponed the decision on new chairs.",
    "Sensor {n} reported normal humidity readings throughout the afternoon.",
    "In section {n}, the manual describes how to replace the filter without tools.",
    "Customer ticket {n} was closed after the replacement part was delivered.",
    "Route {n} was rerouted for road work near the river bridge.",
];

/// A long text with `needle` at `depth` (0..1), about `tokens` long.
fn haystack(tokens: usize, depth: f64, needle: &str) -> String {
    let mut lines = Vec::new();
    let mut chars = 0;
    let mut n = 0;
    while chars < tokens * 4 {
        let l = FILLER[n % FILLER.len()].replace("{n}", &(100 + n * 7).to_string());
        chars += l.len() + 1;
        lines.push(l);
        n += 1;
    }
    let at = ((lines.len() as f64) * depth) as usize;
    lines.insert(at.min(lines.len()), needle.to_string());
    lines.join("\n")
}

async fn long_context(m: &Model, context: Option<u64>) -> Category {
    // Half the model's context (it has to read it and answer), at most 24K tokens.
    let tokens = (context.unwrap_or(8192) as usize / 2).clamp(2000, 24_000);
    let mut cases = Vec::new();
    for (i, depth) in [0.1, 0.5, 0.9].into_iter().enumerate() {
        let code = 4000 + i * 1111 + 7;
        let doc = haystack(
            tokens,
            depth,
            &format!("Important: the access code for the blue storage room is {code}."),
        );
        let t = Instant::now();
        let prompt = format!("{doc}\n\nIn the text above, what is the access code for the blue storage room? Answer with just the number.");
        let (ok, detail) = match m.ask(&prompt, 30).await {
            Ok(a) => (
                a.contains(&code.to_string()),
                format!("Answered {}", clip(&a, 40)),
            ),
            Err(e) => (false, e),
        };
        cases.push(Case {
            name: format!(
                "{}K tokens, fact {}% in",
                tokens / 1000,
                (depth * 100.0) as u32
            ),
            ok,
            detail,
            seconds: t.elapsed().as_secs_f64(),
        });
    }
    // Two facts far apart that only answer the question together.
    let mut doc = haystack(tokens, 0.3, "Mara's locker is number 4418.");
    let at = doc.len() * 4 / 5;
    let at = doc[..at].rfind('\n').unwrap_or(at);
    doc.insert_str(at, "\nThe combination for locker 4418 is 9902.");
    let t = Instant::now();
    let prompt = format!("{doc}\n\nIn the text above, what is the combination for Mara's locker? Answer with just the number.");
    let (ok, detail) = match m.ask(&prompt, 30).await {
        Ok(a) => (a.contains("9902"), format!("Answered {}", clip(&a, 40))),
        Err(e) => (false, e),
    };
    cases.push(Case {
        name: format!("{}K tokens, two facts to connect", tokens / 1000),
        ok,
        detail,
        seconds: t.elapsed().as_secs_f64(),
    });
    category("long", "Long documents", cases)
}

// ---------------------------------------------------------------- the run

/// Evaluates what's serving now. Takes a few minutes; one at a time. Agent
/// reliability (long Code sessions, about 15 minutes more) only when asked.
pub async fn run(state: &AppState, agent: bool) -> Result<Eval, String> {
    let ep = state
        .endpoint()
        .ok_or("Nothing is serving. Start a chat model, then evaluate it.")?;
    let context = served_context(state).await;
    let id = new_id();
    {
        let mut cur = mine(state).current.lock().expect("evals");
        if cur.as_ref().is_some_and(|e| e.status == "running") {
            return Err("An evaluation is already running.".into());
        }
        *cur = Some(Eval {
            id: id.clone(),
            model: ep.model.clone(),
            started: now(),
            status: "running".into(),
            context,
            activity: Some("Measuring speed".into()),
            agent,
            ..Default::default()
        });
    }
    checkpoint(state);
    drive(state, &ep.base_url, &ep.model, context, agent).await
}

/// Run (or carry on) the current evaluation to the end, and save it.
async fn drive(
    state: &AppState,
    base_url: &str,
    model: &str,
    context: Option<u64>,
    agent: bool,
) -> Result<Eval, String> {
    mine(state).stop.store(false, Ordering::SeqCst);
    let m = Arc::new(Model {
        http: match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(600))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                if let Some(mut cur) = mine(state).current.lock().expect("evals").take() {
                    cur.status = "failed".into();
                    cur.finished = Some(now());
                    cur.error = Some(e.to_string());
                    save(state, &cur);
                }
                return Err(e.to_string());
            }
        },
        url: crate::assistant::chat_completions_url(base_url),
        model: model.to_string(),
    });
    let result = steps(state, &m, base_url, context, agent).await;
    let mut e = mine(state)
        .current
        .lock()
        .expect("evals")
        .take()
        .unwrap_or_default();
    e.finished = Some(now());
    e.activity = None;
    match result {
        Ok(()) => {
            e.status = "done".into();
            e.overall = overall(&e.categories);
        }
        Err(why) => {
            e.status = if mine(state).stop.load(Ordering::SeqCst) {
                "stopped".into()
            } else {
                "failed".into()
            };
            e.error = Some(why);
        }
    }
    save(state, &e);
    Ok(e)
}

/// The mean of the categories, leaving out agent reliability so check-ups from
/// before it existed compare evenly with newer ones.
fn overall(categories: &[Category]) -> Option<f64> {
    let scored: Vec<&Category> = categories.iter().filter(|c| c.key != "agent").collect();
    (!scored.is_empty())
        .then(|| scored.iter().map(|c| c.score()).sum::<f64>() / scored.len() as f64)
}

async fn steps(
    state: &AppState,
    m: &Arc<Model>,
    base_url: &str,
    context: Option<u64>,
    agent: bool,
) -> Result<(), String> {
    let stop = || {
        if mine(state).stop.load(Ordering::SeqCst) {
            Err("Stopped.".to_string())
        } else {
            Ok(())
        }
    };
    // What a resumed evaluation already has.
    let (measured, done): (bool, Vec<String>) = mine(state)
        .current
        .lock()
        .expect("evals")
        .as_ref()
        .map(|e| {
            (
                e.speed.single_tps.is_some() || e.speed.ttft_s.is_some(),
                e.categories.iter().map(|c| c.key.clone()).collect(),
            )
        })
        .unwrap_or_default();
    if !measured {
        match crate::bench::run(state).await {
            Ok(b) => progress(state, |e| {
                e.speed = Speed {
                    ttft_s: b.ttft_s,
                    single_tps: b.single_tps,
                    batch_tps: b.batch_tps,
                    wh_per_million_tokens: b.wh_per_million_tokens,
                }
            }),
            Err(why) if why.contains("already") => {}
            Err(why) => return Err(why),
        }
        checkpoint(state);
    }
    stop()?;
    // Fail fast when the model doesn't answer at all.
    m.ask("Say OK.", 5).await?;
    for (key, label) in CATEGORIES {
        if done.iter().any(|d| d == key) {
            continue;
        }
        stop()?;
        progress(state, |e| e.activity = Some(label.into()));
        let c = match key {
            "tools" => tool_use(m).await,
            "code" => code(m).await,
            "reasoning" => reasoning(m).await,
            "instructions" => instructions(m).await,
            _ => long_context(m, context).await,
        };
        progress(state, |e| e.categories.push(c));
        // Each finished category survives a restart.
        checkpoint(state);
    }
    if !agent || done.iter().any(|d| d == "agent") {
        return Ok(());
    }
    stop()?;
    progress(state, |e| {
        e.activity = Some("Testing agent reliability".into())
    });
    let server = crate::reliability::Server {
        http: m.http.clone(),
        base_url: base_url.to_string(),
        model: m.model.clone(),
        context,
    };
    let say = |what: String| {
        progress(state, |e| {
            e.activity = Some(format!("Testing agent reliability: {what}"))
        })
    };
    let (report, trials) = crate::reliability::run(
        &server,
        &crate::reliability::Options::default(),
        &say,
        &|| mine(state).stop.load(Ordering::SeqCst),
    )
    .await?;
    if let Some(id) = mine(state)
        .current
        .lock()
        .expect("evals")
        .as_ref()
        .map(|e| e.id.clone())
    {
        save_trials(&dir(state), &id, &trials);
    }
    progress(state, |e| {
        e.categories.push(agent_category(&report));
        e.reliability = Some(report);
    });
    Ok(())
}

/// Agent reliability as a category, one line per size: trials with a valid
/// call within one retry (what [`crate::reliability::RULE`] judges), with the
/// first replies as returned beside them.
pub fn agent_category(r: &crate::reliability::Report) -> Category {
    let cases = r
        .rows
        .iter()
        .map(|row| {
            let mut wrong = Vec::new();
            for (n, what) in [
                (row.leaked, "leaked as text"),
                (row.invented, "invented a tool"),
                (row.invalid_args, "bad arguments"),
                (row.no_call, "no call"),
                (row.errors, "request failed"),
            ] {
                if n > 0 {
                    wrong.push(format!("{n} {what}"));
                }
            }
            Case {
                name: format!("{}K-token session", row.target / 1024),
                ok: row.trials >= crate::reliability::MIN_TRIALS
                    && row.recovered_rate >= crate::reliability::THRESHOLD,
                detail: format!(
                    "{}/{} valid within a retry (95% CI {:.0}–{:.0}%); first replies {}/{} valid{}",
                    row.within_1_retry,
                    row.trials,
                    100.0 * row.recovered_ci_low,
                    100.0 * row.recovered_ci_high,
                    row.valid,
                    row.trials,
                    if wrong.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", wrong.join(", "))
                    }
                ),
                seconds: row.median_s.unwrap_or(0.0),
            }
        })
        .collect::<Vec<_>>();
    Category {
        key: "agent".into(),
        label: "Agent reliability".into(),
        passed: r.rows.iter().map(|x| x.within_1_retry).sum(),
        total: r.rows.iter().map(|x| x.trials).sum(),
        cases,
    }
}

// ---------------------------------------------------------------- compaction

/// Where Code sessions compact when nothing has been measured for the model.
pub const DEFAULT_COMPACT_WINDOW: u64 = crate::coding::COMPACT_WINDOW;
/// Never below this: Claude Code's own prompt and a few files have to fit, or
/// it compacts again straight after compacting.
pub const MIN_COMPACT_WINDOW: u64 = 32_768;

/// The window measured for `model`: its latest agent reliability run's
/// recommendation (see [`crate::reliability::RULE`]), if one was made.
pub fn measured_window(evals: &[Eval], model: &str) -> Option<u64> {
    evals
        .iter()
        .filter(|e| e.model == model && e.status == "done")
        .filter_map(|e| e.reliability.as_ref().map(|r| (e.started, r)))
        .max_by_key(|(started, _)| *started)
        .and_then(|(_, r)| r.recommended_window)
        .map(|w| w.max(MIN_COMPACT_WINDOW))
}

/// Where a Code session on `model` should compact: as measured, else the default.
pub fn compact_window(state: &AppState, model: Option<&str>) -> u64 {
    model
        .and_then(|m| measured_window(&all(state), m))
        .unwrap_or(DEFAULT_COMPACT_WINDOW)
}

async fn served_context(state: &AppState) -> Option<u64> {
    crate::served::local(state, Duration::from_secs(5))
        .await?
        .context
}

/// One line about an evaluation, for a routine's report or a notification.
pub fn headline(e: &Eval) -> String {
    let parts: Vec<String> = e
        .categories
        .iter()
        .map(|c| format!("{} {}/{}", c.label.to_lowercase(), c.passed, c.total))
        .collect();
    let speed = e
        .speed
        .single_tps
        .map(|t| format!(", {t:.0} tok/s"))
        .unwrap_or_default();
    format!("{}: {}{speed}", short(&e.model), parts.join(", "))
}

fn short(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_string()
}

// ---------------------------------------------------------------- recommendations

#[derive(Debug, Clone, Serialize)]
pub struct Pick {
    pub use_for: &'static str,
    pub model: String,
    pub why: String,
}

/// Which model to use for what, from the latest finished evaluation of each.
pub fn recommend(evals: &[Eval]) -> Vec<Pick> {
    let mut latest: HashMap<&str, &Eval> = HashMap::new();
    // A run of agent reliability alone (`examples/agent_reliability.rs`) has nothing else to compare.
    for e in evals
        .iter()
        .filter(|e| e.status == "done" && e.categories.iter().any(|c| c.key != "agent"))
    {
        latest
            .entry(e.model.as_str())
            .and_modify(|x| {
                if e.started > x.started {
                    *x = e
                }
            })
            .or_insert(e);
    }
    let models: Vec<&Eval> = latest.into_values().collect();
    if models.is_empty() {
        return vec![];
    }
    let fastest = models
        .iter()
        .filter_map(|e| e.speed.single_tps)
        .fold(0.0_f64, f64::max)
        .max(1e-9);
    let s = |e: &Eval, k: &str| {
        e.categories
            .iter()
            .find(|c| c.key == k)
            .map_or(0.0, Category::score)
    };
    let speed = |e: &Eval| e.speed.single_tps.unwrap_or(0.0) / fastest;
    let frac = |e: &Eval, k: &str| {
        e.categories
            .iter()
            .find(|c| c.key == k)
            .map_or("–".into(), |c| format!("{}/{}", c.passed, c.total))
    };
    let tps = |e: &Eval| {
        e.speed
            .single_tps
            .map_or(String::new(), |t| format!(", {t:.0} tok/s"))
    };
    let best = |score: &dyn Fn(&Eval) -> f64, need_ctx: u64| -> Option<&Eval> {
        models
            .iter()
            .copied()
            .filter(|e| e.context.unwrap_or(u64::MAX) >= need_ctx)
            .max_by(|a, b| score(a).total_cmp(&score(b)))
    };
    let mut out = Vec::new();
    if let Some(e) = best(
        &|e| {
            0.35 * s(e, "tools")
                + 0.25 * s(e, "reasoning")
                + 0.2 * s(e, "instructions")
                + 0.2 * speed(e)
        },
        0,
    ) {
        out.push(Pick {
            use_for: "Chat and Tori",
            model: e.model.clone(),
            why: format!(
                "tool use {}, reasoning {}{}",
                frac(e, "tools"),
                frac(e, "reasoning"),
                tps(e)
            ),
        });
    }
    if let Some(e) = best(
        &|e| 0.45 * s(e, "code") + 0.35 * s(e, "tools") + 0.2 * speed(e),
        32_768,
    ) {
        out.push(Pick {
            use_for: "Code",
            model: e.model.clone(),
            why: format!(
                "code {}, tool use {}{}",
                frac(e, "code"),
                frac(e, "tools"),
                tps(e)
            ),
        });
    }
    if let Some(e) = best(
        &|e| 0.35 * s(e, "long") + 0.35 * s(e, "instructions") + 0.3 * s(e, "reasoning"),
        16_384,
    ) {
        out.push(Pick {
            use_for: "Research",
            model: e.model.clone(),
            why: format!(
                "long documents {}, instructions {}{}",
                frac(e, "long"),
                frac(e, "instructions"),
                e.context
                    .map_or(String::new(), |c| format!(", {}K context", c / 1024))
            ),
        });
    }
    if models.len() > 1 {
        if let Some(e) = models
            .iter()
            .filter(|e| e.speed.single_tps.is_some())
            .max_by(|a, b| {
                a.speed
                    .single_tps
                    .unwrap_or(0.0)
                    .total_cmp(&b.speed.single_tps.unwrap_or(0.0))
            })
        {
            out.push(Pick {
                use_for: "Speed",
                model: e.model.clone(),
                why: format!(
                    "{:.0} tok/s for one person, overall {:.0}%",
                    e.speed.single_tps.unwrap_or(0.0),
                    100.0 * e.overall.unwrap_or(0.0)
                ),
            });
        }
    }
    out
}

// ---------------------------------------------------------------- routes

/// `GET /evals`: past evaluations (newest first), the one running, and picks.
pub(crate) async fn get_evals(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let running = mine(&state).current.lock().expect("evals").clone();
    // The one running is saved as it goes; it's shown as `running`, not twice.
    let evals: Vec<Eval> = all(&state)
        .into_iter()
        .filter(|e| running.as_ref().is_none_or(|r| r.id != e.id))
        .collect();
    let picks = recommend(&evals);
    let serving = state.endpoint().map(|e| e.model);
    Json(
        json!({ "evals": evals, "running": running, "recommendations": picks, "serving": serving }),
    )
}

/// `POST /evals`: evaluate what's serving (in the background).
/// `POST /evals` (`{"agent": true}` adds agent reliability).
pub(crate) async fn post_eval(
    State(state): State<AppState>,
    _auth: BearerAuth,
    body: Option<Json<Value>>,
) -> Reply {
    let agent = body.is_some_and(|Json(b)| b["agent"] == true);
    if state.endpoint().is_none() {
        return Err(err(
            StatusCode::CONFLICT,
            "Nothing is serving. Start a chat model, then evaluate it.",
        ));
    }
    if mine(&state)
        .current
        .lock()
        .expect("evals")
        .as_ref()
        .is_some_and(|e| e.status == "running")
    {
        return Err(err(
            StatusCode::CONFLICT,
            "An evaluation is already running.",
        ));
    }
    let st = state.clone();
    tokio::spawn(async move {
        if let Err(e) = run(&st, agent).await {
            eprintln!("tt-station-agentd: evaluation: {e}");
        }
    });
    // Let it register, so the reply carries it.
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(Json(
        json!({ "running": mine(&state).current.lock().expect("evals").clone() }),
    ))
}

/// `POST /evals/stop`
pub(crate) async fn post_stop(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    mine(&state).stop.store(true, Ordering::SeqCst);
    Json(json!({ "stopping": true }))
}

/// `GET /evals/{id}`
pub(crate) async fn get_eval(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "Bad id."));
    }
    let e: Eval = crate::fsio::read_json_or_backup(&dir(&state).join(format!("{id}.json")))
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such evaluation."))?;
    Ok(Json(json!({ "eval": e })))
}

/// `DELETE /evals/{id}`
pub(crate) async fn delete_eval(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if !valid_id(&id) {
        return Err(err(StatusCode::BAD_REQUEST, "Bad id."));
    }
    std::fs::remove_file(dir(&state).join(format!("{id}.json")))
        .map_err(|_| err(StatusCode::NOT_FOUND, "No such evaluation."))?;
    Ok(Json(json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_answers_and_instructions() {
        assert!(answer_matches("The ball costs 5 cents.\nANSWER: 5", &["5"]));
        assert!(answer_matches("ANSWER: $5", &["5"]));
        assert!(!answer_matches("ANSWER: 10", &["5"]));
        assert!(answer_matches("Answer: **Friday**", &["friday"]));
        assert!(answer_matches("ANSWER: 391.", &["391"]));
        let check = INSTRUCTIONS[0].2;
        assert!(check("```json\n[\"red\", \"yellow\", \"blue\"]\n```").is_ok());
        assert!(check("Sure! [\"red\", \"yellow\", \"blue\"]").is_err());
        let bullets = INSTRUCTIONS[2].2;
        assert!(bullets("- a\n- b\n- c").is_ok());
        assert!(bullets("Here you go:\n- a\n- b\n- c").is_err());
    }

    #[test]
    fn finds_the_python_and_hides_the_needle() {
        assert_eq!(
            python_block("Here:\n```python\ndef f(): pass\n```\nDone").trim(),
            "def f(): pass"
        );
        assert_eq!(python_block("def f(): pass"), "def f(): pass");
        let h = haystack(2000, 0.5, "NEEDLE");
        assert!(h.len() >= 8000 && h.contains("NEEDLE"));
        let at = h.find("NEEDLE").unwrap() as f64 / h.len() as f64;
        assert!((0.4..0.6).contains(&at));
    }

    #[test]
    fn recommends_by_what_each_use_needs() {
        let cat = |key: &str, passed| Category {
            key: key.into(),
            label: key.into(),
            passed,
            total: 10,
            cases: vec![],
        };
        let mk = |model: &str, tools, code, tps, ctx| Eval {
            id: model.into(),
            model: model.into(),
            started: 1,
            status: "done".into(),
            context: Some(ctx),
            speed: Speed {
                single_tps: Some(tps),
                ..Default::default()
            },
            categories: vec![
                cat("tools", tools),
                cat("code", code),
                cat("reasoning", 8),
                cat("instructions", 8),
                cat("long", 10),
            ],
            overall: Some(0.8),
            ..Default::default()
        };
        let picks = recommend(&[
            mk("chatty", 10, 3, 80.0, 32_768),
            mk("coder", 8, 10, 40.0, 65_536),
            mk("tiny", 2, 2, 200.0, 8192),
        ]);
        let pick = |u: &str| {
            picks
                .iter()
                .find(|p| p.use_for == u)
                .map(|p| p.model.as_str())
        };
        assert_eq!(pick("Code"), Some("coder"));
        assert_eq!(pick("Speed"), Some("tiny"));
        assert_ne!(
            pick("Research"),
            Some("tiny"),
            "8K context is too small for research"
        );
    }

    #[test]
    fn compaction_uses_the_latest_measured_window_for_the_model() {
        let mk = |model: &str, started, window: Option<Option<u64>>| Eval {
            id: format!("{model}-{started}"),
            model: model.into(),
            started,
            status: "done".into(),
            reliability: window.map(|w| crate::reliability::Report {
                recommended_window: w,
                ..Default::default()
            }),
            ..Default::default()
        };
        let evals = vec![
            mk("openai/gpt-oss-20b", 1, Some(Some(98_304))),
            mk("openai/gpt-oss-20b", 2, Some(Some(65_536))),
            // A later check-up without the agent tests doesn't hide the measurement.
            mk("openai/gpt-oss-20b", 3, None),
            mk("Qwen/Qwen3-32B", 1, Some(None)),
            mk("tiny", 1, Some(Some(8_192))),
        ];
        assert_eq!(
            measured_window(&evals, "openai/gpt-oss-20b"),
            Some(65_536),
            "the latest run wins"
        );
        assert_eq!(
            measured_window(&evals, "Qwen/Qwen3-32B"),
            None,
            "nothing held up: no measured window"
        );
        assert_eq!(measured_window(&evals, "never-tested"), None);
        assert_eq!(
            measured_window(&evals, "tiny"),
            Some(MIN_COMPACT_WINDOW),
            "never below the floor"
        );
        let mut stopped = mk("openai/gpt-oss-20b", 9, Some(Some(16_384)));
        stopped.status = "stopped".into();
        assert_eq!(
            measured_window(
                &[evals.clone(), vec![stopped]].concat(),
                "openai/gpt-oss-20b"
            ),
            Some(65_536),
            "an unfinished run doesn't count"
        );
        assert_eq!(
            measured_window(&evals, "Qwen/Qwen3-32B").unwrap_or(DEFAULT_COMPACT_WINDOW),
            65_536,
            "the fallback is 64K"
        );
    }

    #[test]
    fn evaluations_are_saved_whole_and_temp_or_broken_files_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("evals");
        let e = Eval {
            id: "1-abc".into(),
            model: "m".into(),
            started: 5,
            status: "done".into(),
            ..Default::default()
        };
        save_in(&d, &e);
        save_trials(&d, "1-abc", &[]);
        std::fs::write(d.join(".2-x.json.tmp-00ff"), b"{").unwrap();
        std::fs::write(d.join("3-bad.json"), b"{\"id\":").unwrap();
        let all = all_in(&d);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "1-abc");
        assert!(d.join("raw/1-abc.json").is_file());
        assert!(
            !d.join("3-bad.json").exists(),
            "moved aside, not left to be overwritten"
        );
    }

    #[test]
    fn a_restart_resumes_the_same_model_and_interrupts_otherwise() {
        let e = Eval {
            id: "1-a".into(),
            model: "Qwen3-32B".into(),
            status: "running".into(),
            categories: vec![category("tools", "Tool use", vec![])],
            ..Default::default()
        };
        assert_eq!(after_restart(&e, Some("Qwen3-32B")), AfterRestart::Resume);
        assert_eq!(
            after_restart(&e, Some("Llama-3.3-70B")),
            AfterRestart::Interrupt
        );
        assert_eq!(after_restart(&e, None), AfterRestart::Interrupt);
        let mut cut = e.clone();
        interrupt(&mut cut);
        assert_eq!(cut.status, "interrupted");
        assert_eq!(cut.categories.len(), 1, "finished categories are kept");
        assert!(cut.error.as_deref().unwrap().contains("1 of 5"));
        assert!(recommend(&[cut]).is_empty(), "never used for picks");
    }

    #[tokio::test]
    async fn an_evaluation_cut_short_with_nothing_serving_is_marked_interrupted() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(tmp.path().to_path_buf());
        let e = Eval {
            id: "7-cut".into(),
            model: "Qwen3-32B".into(),
            started: 1,
            status: "running".into(),
            categories: vec![
                category("tools", "Tool use", vec![]),
                category("code", "Code", vec![]),
            ],
            ..Default::default()
        };
        save(&state, &e);
        resume_within(state.clone(), Duration::ZERO).await;
        let back = all(&state);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].status, "interrupted");
        assert_eq!(back[0].categories.len(), 2);
        assert!(back[0].finished.is_some());
    }
}
