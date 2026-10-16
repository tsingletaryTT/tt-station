// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! How fast Code runs, over HTTP: `GET /code/sessions/{id}/speed` (who may read
//! a session's steps, how many are kept, the window and compaction point from
//! the served model, its prefix cache) and `GET /code/models` (the served model
//! first, the catalog's other models each with what choosing it would do and
//! what the box measured for it).
//!
//! The model server is a stub on 127.0.0.1 (its `/v1/models` and `/metrics`),
//! adopted as the serving endpoint; the catalog comes from a stub `tt` script
//! run through `sh`. No Code session sends a message, so Claude Code never
//! starts. Steps are recorded through `codespeed::record`, which is what the
//! `/v1/messages` gateway calls when a step ends.
//!
//! The step store and the speed book are process-wide: each test uses its own
//! model names and session ids, and only one test here records steps for
//! sessions whose steps it reads back.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use axum::{Json as AxJson, Router};
use libttstation::model::Endpoint;
use reqwest::Method as M;
use serde_json::{json, Value};
use tt_station_agentd::codespeed::{record, Step};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::ttcli::TtCli;

mod support;

struct Station {
    base: String,
    owner: String,
    member: String,
    client: reqwest::Client,
    _config: tempfile::TempDir,
}

/// What the box serves and knows, for one test.
#[derive(Default)]
struct Setup {
    /// A stub model server's base URL (`…/v1`) and its model id.
    serving: Option<(String, String)>,
    /// A catalog for the stub `tt`.
    catalog: Option<Value>,
}

async fn listen(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// A vLLM stand-in serving `model` with a `context` window; its `/metrics`
/// say whether it caches prompt prefixes (`None`: it exports no counter).
async fn model_server(model: &str, context: u64, prefix_cache: Option<bool>) -> String {
    let models = json!({ "data": [{ "id": model, "max_model_len": context }] });
    let metrics = match prefix_cache {
        Some(true) => "vllm:prompt_tokens_total{model_name=\"M\"} 900.0\nvllm:prefix_cache_queries_total{model_name=\"M\"} 800.0\n",
        Some(false) => "vllm:prompt_tokens_total{model_name=\"M\"} 900.0\nvllm:prefix_cache_queries_total{model_name=\"M\"} 0.0\n",
        None => "vllm:prompt_tokens_total{model_name=\"M\"} 900.0\n",
    };
    let router = Router::new()
        .route("/v1/models", get(move || async move { AxJson(models) }))
        .route("/metrics", get(move || async move { metrics }));
    format!("{}/v1", listen(router).await)
}

/// A stub `tt` that prints `catalog` for `tt --json model list` and refuses anything else.
fn stub_tt(dir: &Path, catalog: &Value) -> Vec<String> {
    let file = dir.join("catalog.json");
    std::fs::write(&file, catalog.to_string()).unwrap();
    let script = dir.join("tt.sh");
    std::fs::write(
        &script,
        format!(
            "[ \"$1\" = \"--json\" ] && shift\ncase \"$1 $2\" in\n  \"model list\") cat '{}';;\n  *) echo \"stub tt: unexpected $*\" >&2; exit 2;;\nesac\n",
            file.display()
        ),
    )
    .unwrap();
    vec!["sh".into(), script.display().to_string()]
}

async fn harness(setup: Setup) -> Station {
    support::home_without_models();
    let config = tempfile::tempdir().unwrap();
    let mut state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(config.path().to_path_buf());
    if let Some(catalog) = &setup.catalog {
        let dir = config.path().join("tt");
        std::fs::create_dir_all(dir.join("hf")).unwrap();
        state = state.with_tt_cli(Arc::new(TtCli::new(stub_tt(&dir, catalog), dir.join("hf"))));
    }
    if let Some((base_url, model)) = setup.serving {
        state.adopt_serving(Endpoint {
            base_url,
            model,
            requires_key: false,
        });
    }
    let base = listen(app(state.clone())).await;
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        base,
        owner,
        member,
        client: reqwest::Client::new(),
        _config: config,
    }
}

impl Station {
    async fn call(&self, method: M, path: &str, token: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn start(&self, token: &str, body: Value) -> String {
        let (status, said) = self.call(M::POST, "/code/sessions", token, body).await;
        assert_eq!(status, 200, "{said}");
        said["session"]["id"].as_str().unwrap().to_string()
    }

    async fn speed(&self, id: &str, token: &str) -> (u16, Value) {
        self.call(
            M::GET,
            &format!("/code/sessions/{id}/speed"),
            token,
            json!({}),
        )
        .await
    }
}

fn project() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME");
    tempfile::Builder::new()
        .prefix("tt-station-ws-test-")
        .tempdir_in(home)
        .expect("a folder in the home folder")
}

/// A step that read `prompt` tokens and wrote `output` in one second after its
/// first token, which came `ttft_ms` after the request.
fn step(model: Option<&str>, prompt: u64, cached: Option<u64>, output: u64, ttft_ms: u64) -> Step {
    let t0 = Instant::now();
    let first = t0 + Duration::from_millis(ttft_ms);
    Step::measured(
        model.map(str::to_string),
        t0,
        Some(first),
        first + Duration::from_secs(1),
        prompt,
        cached,
        output,
    )
}

#[tokio::test]
async fn a_sessions_speed_is_for_those_who_may_use_the_session() {
    let b = harness(Setup::default()).await;
    let p = project();
    let theirs = b
        .start(&b.owner, json!({ "folder": p.path(), "sandbox": false }))
        .await;
    let shared = b.start(&b.member, json!({ "folder": p.path() })).await;

    let (status, said) = b.speed(&theirs, &b.owner).await;
    assert_eq!(status, 200, "{said}");
    let (status, said) = b.speed(&shared, &b.member).await;
    assert_eq!(status, 200, "{said}");
    // An owner's session outside the sandbox is the owner's alone: a member
    // doesn't see it, its steps included (`workspace`: "see it").
    let (status, said) = b.speed(&theirs, &b.member).await;
    assert_eq!(status, 403, "{said}");

    for missing in ["1700000000-nope00", "a.b", "..", &"x".repeat(41)] {
        let (status, said) = b.speed(missing, &b.owner).await;
        assert_eq!(status, 404, "{missing}: {said}");
    }
    let (status, _) = b.speed(&theirs, "not-a-token").await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn a_session_keeps_its_last_forty_steps_and_the_box_its_last_sessions() {
    let b = harness(Setup::default()).await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;

    // Nothing yet, and nothing serving: the default window.
    let (status, s) = b.speed(&id, &b.owner).await;
    assert_eq!(status, 200, "{s}");
    assert_eq!(s["steps"], json!([]));
    assert_eq!(s["model"], Value::Null);
    assert_eq!(s["prefix_cache"], Value::Null);
    assert_eq!(s["window"], 131_072);
    assert_eq!(s["compact_at"], 65_536);

    for i in 0..45 {
        record(&id, step(None, 1000 + i, None, 9, 300));
    }
    let (_, s) = b.speed(&id, &b.owner).await;
    let prompts: Vec<u64> = s["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|st| st["prompt_tokens"].as_u64().unwrap())
        .collect();
    // Oldest first, the five oldest dropped.
    assert_eq!(prompts, (1005..1045).collect::<Vec<u64>>());
    let last = s["steps"].as_array().unwrap().last().unwrap();
    assert_eq!(last["ttft_ms"], 300);
    assert_eq!(last["decode_tps"], 8.0);
    assert_eq!(last["output_tokens"], 9);

    // Steps of 32 other sessions since: this one, the least recently active,
    // is let go.
    for n in 0..32 {
        record(
            &format!("1700000000-evict{n:02}"),
            step(None, 1, None, 1, 10),
        );
    }
    let (_, s) = b.speed(&id, &b.owner).await;
    assert_eq!(s["steps"], json!([]), "{s}");
}

#[tokio::test]
async fn the_window_and_compaction_point_come_from_the_served_model() {
    for (model, context, window, compact_at, cache, how) in [
        // A small window: compaction can't come later than the window itself.
        (
            "stub-coder-ctx16k",
            16_384,
            16_384,
            16_384,
            Some(true),
            "on",
        ),
        // A huge one is capped.
        (
            "stub-coder-ctx1m",
            1_000_000,
            131_072,
            65_536,
            Some(false),
            "off",
        ),
        ("stub-coder-ctx100k", 100_000, 100_000, 65_536, None, ""),
    ] {
        let server = model_server(model, context, cache).await;
        let b = harness(Setup {
            serving: Some((server, format!("org/{model}"))),
            ..Setup::default()
        })
        .await;
        let p = project();
        let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
        let (status, s) = b.speed(&id, &b.owner).await;
        assert_eq!(status, 200, "{s}");
        assert_eq!(s["model"], format!("org/{model}"), "{s}");
        assert_eq!(s["window"], window, "{model}: {s}");
        assert_eq!(s["compact_at"], compact_at, "{model}: {s}");
        if cache.is_some() {
            assert_eq!(s["prefix_cache"]["state"], how, "{model}: {s}");
            assert_eq!(s["prefix_cache"]["how"], "the server's metrics", "{s}");
        } else {
            // No counter in its metrics: whatever else says, never "on" from them.
            assert_ne!(s["prefix_cache"]["how"], "the server's metrics", "{s}");
        }
    }
}

#[tokio::test]
async fn cached_tokens_in_an_answer_say_the_server_caches() {
    let model = "org/stub-coder-cached";
    // Its metrics say no, but an answer said it read cached tokens: that wins.
    let server = model_server(model, 32_768, Some(false)).await;
    let b = harness(Setup {
        serving: Some((server, model.into())),
        ..Setup::default()
    })
    .await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
    record(&id, step(Some(model), 5000, Some(4800), 20, 200));
    let (_, s) = b.speed(&id, &b.owner).await;
    assert_eq!(s["prefix_cache"]["state"], "on", "{s}");
    assert_eq!(
        s["prefix_cache"]["how"],
        "the server reported cached prompt tokens"
    );
}

fn catalog() -> Value {
    let entry =
        |name: &str, repo: Option<&str>, kind: &str, supported: bool, parser: Option<&str>| {
            json!({ "name": name, "hf_repo": repo, "model_type": kind, "cached": false,
                "param_count": 8, "devices": { "p300x2": { "supported": supported,
                "tool_call_parser": parser, "max_context": 65_536 } } })
        };
    json!({ "device": "p300x2", "models": [
        // What's serving, spelled the catalog's way.
        entry("Stub-Served", None, "llm", true, Some("hermes")),
        entry("Stub-Beta-Tools", None, "llm", true, Some("hermes")),
        entry("stub-alpha-notools", None, "vlm", true, None),
        // Left out: weights not on the box, not for this mesh, not a chat model.
        entry("Stub-Missing", Some("org/Stub-Missing"), "llm", true, Some("hermes")),
        entry("Stub-Unsupported", None, "llm", false, Some("hermes")),
        entry("stub-whisper", None, "audio", true, None),
    ] })
}

#[tokio::test]
async fn the_model_list_puts_the_served_model_first_and_says_what_choosing_does() {
    let served = "org/stub-served";
    let server = model_server(served, 40_960, Some(true)).await;
    let b = harness(Setup {
        serving: Some((server, served.into())),
        catalog: Some(catalog()),
    })
    .await;
    // What the box measured: three steps on the served model (decode 10, 20
    // and 30 tokens a second), one on another under another spelling.
    for out in [11, 21, 31] {
        record("1700000000-models", step(Some(served), 100, None, out, 500));
    }
    record(
        "1700000000-models",
        step(Some("tt-stub-beta-tools"), 100, Some(90), 41, 1500),
    );

    for token in [&b.owner, &b.member] {
        let (status, list) = b.call(M::GET, "/code/models", token, json!({})).await;
        assert_eq!(status, 200, "{list}");
        assert_eq!(list["code_model"], served);
        assert_eq!(list["catalog_error"], Value::Null, "{list}");
        let names: Vec<&str> = list["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        // Not one that can't make tool calls: Code can't work on it.
        assert_eq!(names, vec!["Stub-Served", "Stub-Beta-Tools"], "{list}");
        let [now, beta] = &list["models"].as_array().unwrap()[..] else {
            panic!("{list}");
        };
        assert_eq!(now["served_id"], served);
        assert_eq!(now["choice"], json!({ "action": "in_use" }));
        assert_eq!(now["source"], "catalog");
        assert_eq!(now["tools"]["verified"], true);
        assert_eq!(now["serve"], Value::Null, "nothing to start");
        // The server's own window, not the catalog's.
        assert_eq!(now["context"], 40_960);
        assert_eq!(now["prefix_cache"]["state"], "on");
        assert_eq!(now["speed"]["decode_tps"], 20.0, "{now}");
        assert_eq!(now["speed"]["ttft_s"], 0.5);
        assert_eq!(now["speed"]["steps"], 3);

        assert_eq!(beta["choice"], json!({ "action": "switch" }));
        assert_eq!(beta["context"], 65_536);
        // Switching is the catalog's own start, as from Models.
        assert_eq!(
            beta["serve"],
            json!({ "route": "/serve", "body": { "model": "Stub-Beta-Tools", "force": true } })
        );
        assert_eq!(beta["served_id"], Value::Null);
        assert_eq!(beta["speed"]["steps"], 1);
        assert_eq!(beta["speed"]["decode_tps"], 40.0);
        // It answered with cached tokens when it served.
        assert_eq!(beta["prefix_cache"]["state"], "on", "{beta}");
        assert_eq!(beta["prefix_cache"]["how"], "seen when it last served here");
    }
}

#[tokio::test]
async fn the_model_list_without_a_catalog_or_a_served_model() {
    // No tt: the served model only, and why there's nothing else.
    let served = "org/stub-lonely";
    let server = model_server(served, 8_192, Some(true)).await;
    let b = harness(Setup {
        serving: Some((server, served.into())),
        ..Setup::default()
    })
    .await;
    let (status, list) = b.call(M::GET, "/code/models", &b.owner, json!({})).await;
    assert_eq!(status, 200, "{list}");
    assert!(
        list["catalog_error"].as_str().unwrap().contains("tt CLI"),
        "{list}"
    );
    assert_eq!(list["models"].as_array().unwrap().len(), 1);
    // Served some way the list doesn't know: still shown, as what Code runs on.
    assert_eq!(list["models"][0]["model"], served);
    assert_eq!(list["models"][0]["name"], "stub-lonely");
    assert_eq!(list["models"][0]["source"], "running");
    assert_eq!(list["models"][0]["choice"]["action"], "in_use");
    assert_eq!(list["models"][0]["context"], 8_192);

    // Nothing serving: every ready chat model is a start.
    let b = harness(Setup {
        catalog: Some(catalog()),
        ..Setup::default()
    })
    .await;
    let (_, list) = b.call(M::GET, "/code/models", &b.owner, json!({})).await;
    assert_eq!(list["code_model"], Value::Null);
    assert_eq!(list["serve"]["phase"], "idle", "{list}");
    let choices: Vec<(&str, &str)> = list["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["name"].as_str().unwrap(),
                m["choice"]["action"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        choices,
        vec![("Stub-Beta-Tools", "switch"), ("Stub-Served", "switch")]
    );
    // Unpaired: nothing.
    let r = b
        .client
        .get(format!("{}/code/models", b.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
}
