// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The model check-up (`/evals`) end to end, against a stub OpenAI-compatible
//! model server on 127.0.0.1 that answers instantly and deterministically:
//!
//! - a run grades every category from what the model answered, keeps agent
//!   reliability out unless it was asked for, and recommends from the result;
//! - agent reliability (`{"agent": true}`) measures each prompt size that fits
//!   the served context, saves every trial, and the window it recommends is
//!   where Code compacts;
//! - each finished step is checkpointed: a run cut short picks up at the next
//!   category on restart (same model), or is marked `interrupted` (another one);
//! - stop, one run at a time, ids that can't leave `evals/`, who may call what.
//!
//! The check-up's state (the run in progress, the stop flag) is process-wide,
//! so every test here that starts or resumes a run holds [`RUN_LOCK`].
//!
//! The Code category runs the model's answer in `docker run ... tt-code-sandbox`.
//! A fake `docker` (see [`fakes`]) stands in: it records its arguments and
//! "passes" a program only if the stub model's answer marked it to.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{extract::Json as AxJson, http::header, response::IntoResponse, routing, Router};
use libttstation::model::Endpoint;
use serde_json::{json, Value};
use tt_station_agentd::evals::{self, Category, Eval, Speed};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

const MODEL: &str = "acme/stub-chat-8b";

/// Runs and resumes share the check-up's process-wide state: one at a time.
static RUN_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ---------------------------------------------------------------- the fake sandbox

/// A directory of fake programs, put first on this test binary's PATH once,
/// before any test reaches code that spawns one. Every test calls this first;
/// `PATH` is process-wide, and this binary is its own process, so nothing else
/// sees it. Under the target dir (not /tmp) so a leftover is harmless.
fn fakes() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir =
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("evals-fakes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("docker.log");
        // The sandbox run: passes only a program the stub model marked.
        let docker = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nprog=$(cat)\ncase \"$prog\" in *STUB_PASS*) echo ok; exit 0;; esac\necho AssertionError >&2\nexit 1\n",
            log.display()
        );
        let path = dir.join("docker");
        std::fs::write(&path, docker).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.display()));
        dir
    })
}

// ---------------------------------------------------------------- the stub model

/// What the stub was asked, by kind.
#[derive(Default, Debug, Clone)]
struct Seen {
    bench: usize,
    say_ok: usize,
    tools: usize,
    code: usize,
    reasoning: usize,
    instructions: usize,
    long: usize,
    /// Agent reliability: the one-token size counts, and the graded turns.
    sizing: usize,
    agent: usize,
}

struct Stub {
    port: u16,
    seen: Arc<Mutex<Seen>>,
    /// While false, reasoning questions wait (to catch a run mid-way).
    open: tokio::sync::watch::Sender<bool>,
}

impl Stub {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }
    fn seen(&self) -> Seen {
        self.seen.lock().unwrap().clone()
    }
    fn release(&self) {
        let _ = self.open.send(true);
    }
}

fn last_user_text(body: &Value) -> String {
    let msgs = body["messages"].as_array().cloned().unwrap_or_default();
    let Some(m) = msgs.iter().rev().find(|m| m["role"] == "user") else {
        return String::new();
    };
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn message(content: &str) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": content } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 128 } })
}

fn tool_call(name: &str, args: Value) -> Value {
    json!({ "choices": [{ "message": { "role": "assistant", "content": "", "tool_calls": [
        { "id": "call_1", "type": "function", "function": { "name": name, "arguments": args.to_string() } }
    ] } }] })
}

fn sse(events: &[Value]) -> axum::response::Response {
    let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

/// A stand-in for vLLM serving [`MODEL`] with `context` tokens. Its answers are
/// fixed so each category's grade is known in advance (see the comments on
/// each), and agent reliability's turns get a valid tool call up to
/// `agent_ok_below` prompt tokens and plain text above.
async fn stub(context: u64, agent_ok_below: u64, hold_reasoning: bool) -> Stub {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (open, rx) = tokio::sync::watch::channel(!hold_reasoning);
    let s = seen.clone();
    let router = Router::new()
        .route(
            "/v1/models",
            routing::get(move || async move {
                AxJson(json!({ "data": [{ "id": MODEL, "max_model_len": context }] }))
            }),
        )
        .route(
            "/v1/chat/completions",
            routing::post(move |AxJson(body): AxJson<Value>| {
                let seen = s.clone();
                let mut rx = rx.clone();
                async move {
                    let stream = body["stream"] == true;
                    let tools = body["tools"].is_array();
                    // Roughly what a server would count: the request's size / 4.
                    let tokens = (body.to_string().len() / 4) as u64;
                    let text = last_user_text(&body);
                    if tools && stream {
                        seen.lock().unwrap().agent += 1;
                        let usage = json!({ "choices": [], "usage": { "prompt_tokens": tokens, "completion_tokens": 12 } });
                        return if tokens < agent_ok_below {
                            sse(&[
                                json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "Read", "arguments": "" } }] } }] }),
                                json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "{\"file_path\": \"src/lib.rs\"}" } }] } }] }),
                                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
                                usage,
                            ])
                        } else {
                            // Deep in a long session it stops calling tools.
                            sse(&[
                                json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "I would read the file now." } }] }),
                                json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
                                usage,
                            ])
                        };
                    }
                    if stream {
                        // The speed benchmark's one streamed answer.
                        seen.lock().unwrap().bench += 1;
                        return sse(&[
                            json!({ "choices": [{ "delta": { "content": "Hel" } }] }),
                            json!({ "choices": [{ "delta": { "content": "lo" } }] }),
                            json!({ "choices": [], "usage": { "completion_tokens": 2 } }),
                        ]);
                    }
                    if tools && body["max_tokens"] == 1 {
                        seen.lock().unwrap().sizing += 1;
                        return AxJson(json!({ "choices": [{ "message": { "content": "x" } }], "usage": { "prompt_tokens": tokens, "completion_tokens": 1 } })).into_response();
                    }
                    if tools {
                        seen.lock().unwrap().tools += 1;
                        // Right for the weather in Paris and a 25 minute timer, wrong
                        // for an hour and a half, and no call for anything else:
                        // 6 of 11 (the four that want no call, or "a coat", pass).
                        let reply = if text.contains("Paris") {
                            tool_call("get_weather", json!({ "city": "Paris", "unit": "celsius" }))
                        } else if text.contains("25 minutes") {
                            tool_call("set_timer", json!({ "minutes": 25 }))
                        } else if text.contains("an hour and a half") {
                            tool_call("set_timer", json!({ "minutes": 60 }))
                        } else {
                            message("Take a warm coat.")
                        };
                        return AxJson(reply).into_response();
                    }
                    let reply = if text.contains("transformer neural network") {
                        seen.lock().unwrap().bench += 1;
                        "A long explanation."
                    } else if text == "Say OK." {
                        seen.lock().unwrap().say_ok += 1;
                        "OK"
                    } else if text.contains("ANSWER: <answer>") {
                        seen.lock().unwrap().reasoning += 1;
                        let _ = rx.wait_for(|open| *open).await;
                        // Right only for 17 × 23: 1 of 18.
                        "Let me think.\nANSWER: 391"
                    } else if text.contains("```python block") {
                        seen.lock().unwrap().code += 1;
                        // Only the palindrome "passes" the fake sandbox: 1 of 10.
                        if text.contains("is_palindrome") {
                            "```python\n# STUB_PASS\ndef is_palindrome(s): return True\n```"
                        } else {
                            "```python\ndef nothing(): pass\n```"
                        }
                    } else if text.contains("blue storage room") || text.contains("Mara's locker") {
                        seen.lock().unwrap().long += 1;
                        // The code hidden 10% in: 1 of 4.
                        "4007"
                    } else {
                        seen.lock().unwrap().instructions += 1;
                        // Right for "is 91 prime?" and "fewer than 15 words": 2 of 9.
                        "no"
                    };
                    AxJson(message(reply)).into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Stub { port, seen, open }
}

/// A port nothing listens on, so nothing here can reach a real model server.
fn dead_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

// ---------------------------------------------------------------- the box

struct Station {
    state: AppState,
    base: String,
    owner: String,
    member: String,
    client: reqwest::Client,
    config: tempfile::TempDir,
}

/// A box serving from `stub` (or nothing), its config in a temp dir, with an
/// owner and a member paired.
async fn station(stub: Option<&Stub>) -> Station {
    fakes();
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_serving_config(
            "127.0.0.1".into(),
            stub.map(|s| s.port).unwrap_or_else(dead_port),
        )
        .with_config_dir(config.path().to_path_buf());
    if let Some(s) = stub {
        state.adopt_serving(Endpoint {
            base_url: s.base_url(),
            model: MODEL.into(),
            requires_key: false,
        });
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        state,
        base,
        owner,
        member,
        client: reqwest::Client::new(),
        config,
    }
}

impl Station {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut r = self.client.request(method, format!("{}{path}", self.base));
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.call(reqwest::Method::GET, path, Some(&self.owner), None)
            .await
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, Some(&self.owner), Some(body))
            .await
    }

    fn evals_dir(&self) -> PathBuf {
        self.config.path().join("evals")
    }

    /// Waits for the run in progress to finish; answers `GET /evals` then.
    async fn finished(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let (status, v) = self.get("/evals").await;
            assert_eq!(status, 200);
            if v["running"].is_null() {
                return v;
            }
            assert!(
                Instant::now() < deadline,
                "the check-up never finished: {v}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until `f` holds for the run in progress.
    async fn running_until(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (_, v) = self.get("/evals").await;
            if f(&v["running"]) {
                return v;
            }
            assert!(Instant::now() < deadline, "never saw {what}: {v}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn keys(e: &Value) -> Vec<String> {
    e["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key"].as_str().unwrap().to_string())
        .collect()
}

fn category<'a>(e: &'a Value, key: &str) -> &'a Value {
    e["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == key)
        .unwrap_or_else(|| panic!("no {key} category in {e}"))
}

fn score(e: &Value, key: &str) -> (u64, u64) {
    let c = category(e, key);
    (c["passed"].as_u64().unwrap(), c["total"].as_u64().unwrap())
}

fn case_ok(e: &Value, key: &str, name: &str) -> bool {
    category(e, key)["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no case {name} in {key}"))["ok"]
        .as_bool()
        .unwrap()
}

fn sandbox_runs() -> Vec<String> {
    std::fs::read_to_string(fakes().join("docker.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------- a whole check-up

#[tokio::test]
async fn a_check_up_grades_every_category_from_the_models_answers() {
    let _one = RUN_LOCK.lock().await;
    let model = stub(12_288, u64::MAX, false).await;
    let s = station(Some(&model)).await;

    let (status, v) = s.post("/evals", json!({})).await;
    assert_eq!(status, 200, "{v}");
    // Against an instant stub the run may already be over when this answers.
    let running = &v["running"];
    if !running.is_null() {
        assert_eq!(running["model"], MODEL);
        assert_eq!(running["status"], "running");
    }

    let v = s.finished().await;
    let e = &v["evals"][0];
    let id = e["id"].as_str().unwrap().to_string();
    assert_eq!(e["model"], MODEL);
    assert_eq!(e["status"], "done", "{e}");
    assert_eq!(e["agent"], false, "agent reliability is opt-in");
    assert_eq!(e["context"], 12_288, "read off the server's /v1/models");
    assert_eq!(
        keys(e),
        ["tools", "code", "reasoning", "instructions", "long"],
        "every category, in order, and no agent reliability unless asked"
    );
    assert!(e["reliability"].is_null());

    // Graded from what the stub answered, not from what was asked.
    assert_eq!(score(e, "tools"), (6, 11));
    assert!(case_ok(e, "tools", "Weather, with unit"));
    assert!(case_ok(e, "tools", "No tool for small talk"));
    assert!(case_ok(e, "tools", "Uses a tool's result"));
    assert!(
        !case_ok(e, "tools", "Converts units"),
        "60 minutes isn't an hour and a half"
    );
    assert!(!case_ok(e, "tools", "Email"));
    assert_eq!(score(e, "code"), (1, 10));
    assert!(case_ok(e, "code", "Palindrome"));
    assert_eq!(score(e, "reasoning"), (1, 18));
    assert!(case_ok(e, "reasoning", "Multiplication"));
    assert_eq!(score(e, "instructions"), (2, 9));
    assert!(case_ok(e, "instructions", "Yes or no"));
    assert_eq!(score(e, "long"), (1, 4));
    assert!(
        case_ok(e, "long", "6K tokens, fact 10% in"),
        "half the 12K context"
    );
    let want = (6.0 / 11.0 + 1.0 / 10.0 + 1.0 / 18.0 + 2.0 / 9.0 + 1.0 / 4.0) / 5.0;
    assert!(
        (e["overall"].as_f64().unwrap() - want).abs() < 1e-9,
        "{}",
        e["overall"]
    );
    assert!(
        e["speed"]["ttft_s"].is_number(),
        "the speed benchmark ran first"
    );
    assert!(e["speed"]["batch_tps"].as_f64().unwrap() > 0.0);
    assert!(e["finished"].is_u64());

    // The model's code ran in the locked-down sandbox, never on the box.
    let runs: Vec<String> = sandbox_runs();
    let ours: Vec<&String> = runs
        .iter()
        .filter(|r| r.contains("tt-code-sandbox"))
        .collect();
    assert!(ours.len() >= 10, "{runs:?}");
    for r in ours {
        for flag in [
            "--network none",
            "--read-only",
            "--rm",
            "--memory 256m",
            "--pids-limit 64",
        ] {
            assert!(r.contains(flag), "sandbox run without {flag}: {r}");
        }
    }

    let seen = model.seen();
    assert_eq!(
        (seen.agent, seen.sizing),
        (0, 0),
        "no agent reliability: {seen:?}"
    );
    assert_eq!(seen.tools, 11);
    assert_eq!(seen.reasoning, 18);

    // Picks: only what its 12K context allows (Code wants 32K, Research 16K).
    let picks: Vec<&str> = v["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["use_for"].as_str().unwrap())
        .collect();
    assert_eq!(picks, ["Chat and Tori"]);
    assert_eq!(v["recommendations"][0]["model"], MODEL);
    assert_eq!(v["serving"], MODEL);

    // Saved where GET /evals/{id} and a restart find it.
    let (status, one) = s.get(&format!("/evals/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(one["eval"]["overall"], e["overall"]);
    assert!(s.evals_dir().join(format!("{id}.json")).is_file());
    assert!(
        !s.evals_dir().join("raw").exists(),
        "no agent trials to keep"
    );
}

#[tokio::test]
async fn agent_reliability_measures_each_size_that_fits_and_sets_where_code_compacts() {
    let _one = RUN_LOCK.lock().await;
    // 40K of context: 8K, 16K and 32K sessions fit (with 4K left to answer in).
    // The stub calls tools cleanly below ~24K tokens and stops calling them above.
    let model = stub(40_960, 24_000, false).await;
    let s = station(Some(&model)).await;

    let (status, v) = s.post("/evals", json!({ "agent": true })).await;
    assert_eq!(status, 200, "{v}");
    let v = s.finished().await;
    let e = &v["evals"][0];
    let id = e["id"].as_str().unwrap().to_string();
    assert_eq!(e["agent"], true);
    assert_eq!(e["status"], "done", "{e}");
    assert_eq!(
        keys(e),
        [
            "tools",
            "code",
            "reasoning",
            "instructions",
            "long",
            "agent"
        ]
    );

    let r = &e["reliability"];
    let targets: Vec<u64> = r["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["target"].as_u64().unwrap())
        .collect();
    assert_eq!(targets, [8_192, 16_384, 32_768], "only sizes that fit 40K");
    for row in &r["rows"].as_array().unwrap()[..2] {
        assert_eq!(row["trials"], 20, "{row}");
        assert_eq!(row["valid"], 20, "{row}");
    }
    let deep = &r["rows"][2];
    assert_eq!(
        (deep["trials"].as_u64(), deep["no_call"].as_u64()),
        (Some(20), Some(20)),
        "{deep}"
    );
    assert_eq!(
        r["recommended_window"], 16_384,
        "the largest size that held, with every smaller one"
    );

    // The agent category: one case per size, the trials as its score; it
    // doesn't count toward `overall`, so check-ups with and without it compare.
    let agent = category(e, "agent");
    assert_eq!(
        (agent["passed"].as_u64(), agent["total"].as_u64()),
        (Some(40), Some(60))
    );
    let oks: Vec<bool> = agent["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["ok"].as_bool().unwrap())
        .collect();
    assert_eq!(oks, [true, true, false]);
    let want = (6.0 / 11.0 + 1.0 / 10.0 + 1.0 / 18.0 + 2.0 / 9.0 + 1.0 / 4.0) / 5.0;
    assert!((e["overall"].as_f64().unwrap() - want).abs() < 1e-9);

    // Every graded request is kept beside it.
    let raw: Vec<Value> = serde_json::from_slice(
        &std::fs::read(s.evals_dir().join("raw").join(format!("{id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(raw.len(), 60);
    assert_eq!(
        model.seen().agent,
        60,
        "no retries: a turn with no call just ends"
    );

    // Code on this model compacts at the measured window -- floored at 32K so
    // Claude Code's own prompt fits.
    assert_eq!(
        evals::compact_window(&s.state, Some(MODEL)),
        evals::MIN_COMPACT_WINDOW
    );
    assert_eq!(
        tt_station_agentd::coding::code_window(&s.state, Some(40_960)),
        (40_960, 32_768)
    );
    // Another model isn't affected.
    assert_eq!(
        evals::compact_window(&s.state, Some("other/model")),
        evals::DEFAULT_COMPACT_WINDOW
    );
}

// ---------------------------------------------------------------- stop, checkpoints, resume

#[tokio::test]
async fn a_run_is_checkpointed_as_it_goes_stops_when_asked_and_resumes_where_it_left_off() {
    let _one = RUN_LOCK.lock().await;
    // Reasoning questions wait until released: the run is caught after tool use and code.
    let model = stub(12_288, u64::MAX, true).await;
    let s = station(Some(&model)).await;

    let (status, v) = s.post("/evals", json!({})).await;
    assert_eq!(status, 200, "{v}");
    let id = v["running"]["id"].as_str().unwrap().to_string();
    let v = s
        .running_until("reasoning under way", |r| {
            r["activity"] == "Testing reasoning"
        })
        .await;
    assert!(
        v["evals"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["id"] != id.as_str()),
        "the run in progress is shown as running, not twice: {v}"
    );

    // One at a time.
    let (status, busy) = s.post("/evals", json!({})).await;
    assert_eq!(status, 409);
    assert!(busy["error"].as_str().unwrap().contains("already running"));

    // Meanwhile what it finished is on disk: what a restart would pick up.
    let file = s.evals_dir().join(format!("{id}.json"));
    let checkpoint: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(checkpoint["status"], "running");
    assert_eq!(keys(&checkpoint), ["tools", "code"]);
    assert!(checkpoint["speed"]["ttft_s"].is_number());

    // Stop: the category under way finishes, nothing after it starts.
    let (status, v) = s.post("/evals/stop", json!({})).await;
    assert_eq!((status, &v["stopping"]), (200, &json!(true)));
    model.release();
    let v = s.finished().await;
    let e = &v["evals"][0];
    assert_eq!(e["status"], "stopped", "{e}");
    assert_eq!(
        keys(e),
        ["tools", "code", "reasoning"],
        "kept what it finished"
    );
    assert!(
        e["overall"].is_null(),
        "an unfinished run has no overall score"
    );
    assert_eq!(model.seen().instructions, 0);
    assert!(
        evals::recommend(&evals::all(&s.state)).is_empty(),
        "never used for picks"
    );

    // The agent restarts with the checkpoint on disk, the same model serving:
    // it carries on at reasoning instead of starting over.
    let after = stub(12_288, u64::MAX, false).await;
    let s2 = station(Some(&after)).await;
    std::fs::create_dir_all(s2.evals_dir()).unwrap();
    std::fs::write(
        s2.evals_dir().join(format!("{id}.json")),
        checkpoint.to_string(),
    )
    .unwrap();
    evals::resume_within(s2.state.clone(), Duration::from_secs(5)).await;

    let (status, back) = s2.get(&format!("/evals/{id}")).await;
    assert_eq!(status, 200);
    let e = &back["eval"];
    assert_eq!(e["status"], "done", "{e}");
    assert_eq!(e["resumed"], 1);
    assert_eq!(
        keys(e),
        ["tools", "code", "reasoning", "instructions", "long"]
    );
    assert_eq!(
        e["started"], checkpoint["started"],
        "the same run, not a new one"
    );
    assert_eq!(
        e["speed"], checkpoint["speed"],
        "the speed it measured is kept"
    );
    let seen = after.seen();
    assert_eq!(
        (seen.bench, seen.tools, seen.code),
        (0, 0, 0),
        "nothing it finished is asked again: {seen:?}"
    );
    assert_eq!((seen.reasoning, seen.instructions, seen.long), (18, 9, 4));
    assert!(e["overall"].is_number());
    assert_eq!(
        evals::all(&s2.state).len(),
        1,
        "resumed in place, not saved twice"
    );
}

fn saved(id: &str, model: &str, started: u64, done: &[&str]) -> Eval {
    Eval {
        id: id.into(),
        model: model.into(),
        started,
        status: "running".into(),
        context: Some(12_288),
        speed: Speed {
            ttft_s: Some(0.1),
            single_tps: Some(42.0),
            ..Default::default()
        },
        // Perfect scores the stub could never earn, to see they're kept as they were.
        categories: done
            .iter()
            .map(|k| Category {
                key: (*k).into(),
                label: (*k).into(),
                passed: 3,
                total: 3,
                cases: vec![],
            })
            .collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn after_a_restart_only_the_newest_run_of_the_serving_model_resumes() {
    let _one = RUN_LOCK.lock().await;
    let model = stub(12_288, u64::MAX, false).await;
    let s = station(Some(&model)).await;
    let d = s.evals_dir();
    evals::save_in(&d, &saved("100-other", "other/model", 100, &["tools"]));
    evals::save_in(&d, &saved("200-older", MODEL, 200, &["tools", "code"]));
    evals::save_in(
        &d,
        &saved(
            "300-newer",
            MODEL,
            300,
            &["tools", "code", "reasoning", "instructions"],
        ),
    );

    evals::resume_within(s.state.clone(), Duration::from_secs(5)).await;

    let get = |id: &str| -> Value {
        serde_json::from_slice(&std::fs::read(d.join(format!("{id}.json"))).unwrap()).unwrap()
    };
    let other = get("100-other");
    assert_eq!(
        other["status"], "interrupted",
        "another model is serving: scores mustn't mix"
    );
    assert_eq!(keys(&other), ["tools"]);
    assert!(
        other["error"].as_str().unwrap().contains("1 of 5"),
        "{other}"
    );
    let older = get("200-older");
    assert_eq!(
        older["status"], "interrupted",
        "one resumes; the older one doesn't"
    );
    assert_eq!(keys(&older), ["tools", "code"]);

    let newer = get("300-newer");
    assert_eq!(newer["status"], "done", "{newer}");
    assert_eq!(newer["resumed"], 1);
    assert_eq!(
        keys(&newer),
        ["tools", "code", "reasoning", "instructions", "long"]
    );
    assert_eq!(
        score(&newer, "instructions"),
        (3, 3),
        "finished categories aren't redone"
    );
    assert_eq!(score(&newer, "long"), (1, 4));
    let seen = model.seen();
    assert_eq!(
        (
            seen.bench,
            seen.tools,
            seen.code,
            seen.reasoning,
            seen.instructions,
            seen.long
        ),
        (0, 0, 0, 0, 0, 4)
    );
    // Interrupted runs don't count for picks; the resumed one does.
    let picks = evals::recommend(&evals::all(&s.state));
    assert!(picks.iter().all(|p| p.model == MODEL), "{picks:?}");
}

#[tokio::test]
async fn a_resumed_run_still_does_the_agent_reliability_it_was_asked_for() {
    let _one = RUN_LOCK.lock().await;
    let model = stub(12_288, u64::MAX, false).await;
    let s = station(Some(&model)).await;
    let mut e = saved(
        "400-agent",
        MODEL,
        400,
        &["tools", "code", "reasoning", "instructions", "long"],
    );
    e.agent = true;
    evals::save_in(&s.evals_dir(), &e);

    evals::resume_within(s.state.clone(), Duration::from_secs(5)).await;

    let (_, v) = s.get("/evals/400-agent").await;
    let e = &v["eval"];
    assert_eq!(e["status"], "done", "{e}");
    assert_eq!(
        keys(e),
        [
            "tools",
            "code",
            "reasoning",
            "instructions",
            "long",
            "agent"
        ]
    );
    assert_eq!(
        e["reliability"]["rows"].as_array().unwrap().len(),
        1,
        "12K fits only the 8K size"
    );
    assert_eq!(e["reliability"]["recommended_window"], 8_192);
    let seen = model.seen();
    assert_eq!(seen.agent, 20);
    assert_eq!(
        seen.tools + seen.reasoning + seen.long + seen.bench,
        0,
        "{seen:?}"
    );
    assert!(s.evals_dir().join("raw/400-agent.json").is_file());
}

#[tokio::test]
async fn with_nothing_serving_a_check_up_is_refused_and_a_saved_run_is_interrupted() {
    let _one = RUN_LOCK.lock().await;
    let s = station(None).await;
    let (status, v) = s.post("/evals", json!({ "agent": true })).await;
    assert_eq!(status, 409);
    assert!(
        v["error"].as_str().unwrap().contains("Nothing is serving"),
        "{v}"
    );
    assert!(s.get("/evals").await.1["running"].is_null());

    evals::save_in(
        &s.evals_dir(),
        &saved("500-cut", MODEL, 500, &["tools", "code"]),
    );
    evals::resume_within(s.state.clone(), Duration::ZERO).await;
    let (_, v) = s.get("/evals").await;
    let e = &v["evals"][0];
    assert_eq!(e["status"], "interrupted");
    assert_eq!(keys(e), ["tools", "code"], "what it finished is kept");
    assert!(e["overall"].is_null());
    assert!(v["recommendations"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------- ids, owners and members

#[tokio::test]
async fn ids_that_could_leave_the_evals_folder_are_refused() {
    let s = station(None).await;
    let d = s.evals_dir();
    evals::save_in(&d, &saved("1-keep", MODEL, 1, &["tools"]));
    // A file one level up that a traversal would reach.
    std::fs::write(
        s.config.path().join("secret.json"),
        serde_json::to_string(&saved("secret", MODEL, 1, &[])).unwrap(),
    )
    .unwrap();

    for bad in [
        "..%2Fsecret",
        "..%2F..%2Fsecret",
        "a.b",
        "1-keep%00",
        &"a".repeat(64),
    ] {
        for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
            let (status, v) = s
                .call(
                    method.clone(),
                    &format!("/evals/{bad}"),
                    Some(&s.owner),
                    None,
                )
                .await;
            assert!(
                status == 400 || status == 404,
                "{method} {bad}: {status} {v}"
            );
            assert!(v["eval"].is_null(), "{method} {bad} read something: {v}");
        }
    }
    let (status, v) = s.get("/evals/..%2Fsecret").await;
    assert_eq!((status, v["error"].as_str()), (400, Some("Bad id.")));
    assert!(
        s.config.path().join("secret.json").is_file(),
        "never deleted"
    );

    let (status, _) = s.get("/evals/9-missing").await;
    assert_eq!(status, 404);
    let (status, _) = s
        .call(
            reqwest::Method::DELETE,
            "/evals/9-missing",
            Some(&s.owner),
            None,
        )
        .await;
    assert_eq!(status, 404);

    let (status, v) = s
        .call(
            reqwest::Method::DELETE,
            "/evals/1-keep",
            Some(&s.owner),
            None,
        )
        .await;
    assert_eq!((status, &v["deleted"]), (200, &json!(true)));
    assert!(!d.join("1-keep.json").exists());
    assert_eq!(s.get("/evals/1-keep").await.0, 404);
    assert!(s.get("/evals").await.1["evals"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn every_check_up_route_needs_a_paired_device_and_members_can_read_results() {
    let s = station(None).await;
    evals::save_in(&s.evals_dir(), &saved("1-seen", MODEL, 1, &["tools"]));
    for (m, path) in [
        (reqwest::Method::GET, "/evals"),
        (reqwest::Method::POST, "/evals"),
        (reqwest::Method::POST, "/evals/stop"),
        (reqwest::Method::GET, "/evals/1-seen"),
        (reqwest::Method::DELETE, "/evals/1-seen"),
    ] {
        let (status, _) = s.call(m.clone(), path, None, None).await;
        assert_eq!(status, 401, "{m} {path} without a token");
        let (status, _) = s.call(m.clone(), path, Some("not-a-token"), None).await;
        assert_eq!(status, 401, "{m} {path} with a made-up token");
    }
    assert!(s.evals_dir().join("1-seen.json").is_file());
    let (status, v) = s
        .call(reqwest::Method::GET, "/evals", Some(&s.member), None)
        .await;
    assert_eq!(status, 200);
    assert_eq!(v["evals"][0]["id"], "1-seen");
    let (status, v) = s
        .call(reqwest::Method::GET, "/evals/1-seen", Some(&s.member), None)
        .await;
    assert_eq!((status, &v["eval"]["id"]), (200, &json!("1-seen")));
}

// ---------------------------------------------------------------- where Code compacts

fn measured(id: &str, model: &str, started: u64, status: &str, window: Option<u64>) -> Eval {
    Eval {
        id: id.into(),
        model: model.into(),
        started,
        status: status.into(),
        reliability: Some(tt_station_agentd::reliability::Report {
            recommended_window: window,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test]
async fn code_compacts_where_the_latest_finished_measurement_of_the_served_model_says() {
    let s = station(None).await;
    let d = s.evals_dir();
    assert_eq!(
        evals::compact_window(&s.state, Some(MODEL)),
        evals::DEFAULT_COMPACT_WINDOW,
        "nothing measured"
    );
    assert_eq!(
        evals::compact_window(&s.state, None),
        evals::DEFAULT_COMPACT_WINDOW,
        "nothing serving"
    );

    evals::save_in(&d, &measured("1-a", MODEL, 1, "done", Some(32_768)));
    evals::save_in(&d, &measured("2-a", MODEL, 2, "done", Some(98_304)));
    // Newer, but not finished, or for another model: none of these count.
    evals::save_in(&d, &measured("3-a", MODEL, 3, "stopped", Some(16_384)));
    evals::save_in(&d, &measured("4-a", MODEL, 4, "interrupted", Some(8_192)));
    evals::save_in(&d, &measured("5-a", MODEL, 5, "running", Some(8_192)));
    evals::save_in(
        &d,
        &measured("6-a", "other/model", 6, "done", Some(122_880)),
    );
    // A later check-up without agent reliability doesn't hide the measurement.
    let mut plain = measured("7-a", MODEL, 7, "done", None);
    plain.reliability = None;
    evals::save_in(&d, &plain);
    // A broken file is moved aside, not fatal.
    std::fs::write(d.join("8-a.json"), b"{\"id\": ").unwrap();

    assert_eq!(evals::compact_window(&s.state, Some(MODEL)), 98_304);
    assert_eq!(
        evals::compact_window(&s.state, Some("other/model")),
        122_880
    );
    // How Code picks it up: compaction at the measured window, never past the
    // window it runs with (the served context, capped at 128K).
    s.state.adopt_serving(Endpoint {
        base_url: format!("http://127.0.0.1:{}/v1", dead_port()),
        model: MODEL.into(),
        requires_key: false,
    });
    assert_eq!(
        tt_station_agentd::coding::code_window(&s.state, Some(131_072)),
        (131_072, 98_304)
    );
    assert_eq!(
        tt_station_agentd::coding::code_window(&s.state, Some(65_536)),
        (65_536, 65_536)
    );
    assert_eq!(
        tt_station_agentd::coding::code_window(&s.state, Some(262_144)),
        (131_072, 98_304)
    );

    // The newest finished run held at no size at all: no measurement, the default.
    evals::save_in(&d, &measured("9-a", MODEL, 9, "done", None));
    assert_eq!(
        evals::compact_window(&s.state, Some(MODEL)),
        evals::DEFAULT_COMPACT_WINDOW
    );
    // ...and a tiny measured window is floored so Claude Code's own prompt fits.
    evals::save_in(&d, &measured("10-a", MODEL, 10, "done", Some(8_192)));
    assert_eq!(
        evals::compact_window(&s.state, Some(MODEL)),
        evals::MIN_COMPACT_WINDOW
    );

    // Survives a restart: a new AppState on the same config dir reads the same.
    let again = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(s.config.path().to_path_buf());
    assert_eq!(evals::compact_window(&again, Some("other/model")), 122_880);
}
