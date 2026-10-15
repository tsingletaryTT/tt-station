// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! `POST /v1/messages` end to end: a model's tool calls are checked against the
//! request's tools, mended where that's safe, and counted in `/metrics/now`.

use std::sync::Arc;

use axum::{extract::Json as AxJson, http::header, response::IntoResponse, routing::post, Router};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

struct Idle;

impl ServingBackend for Idle {
    fn start_forcing(&self, _: &str, _: bool) -> anyhow::Result<Endpoint> {
        anyhow::bail!("unused")
    }
    fn stop(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
    fn list_models(&self) -> anyhow::Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// A vLLM stand-in whose model calls `read` (wrong case) with `offset` as a
/// string, then `Search`, a tool that doesn't exist.
async fn stub_vllm() -> String {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|AxJson(body): AxJson<Value>| async move {
            if body["stream"] == true {
                let chunks = [
                    json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "c1", "function": { "name": "read", "arguments": "{\"file_path\": \"a.rs\", " } }] } }] }),
                    json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"offset\": \"40\"}" } }] } }] }),
                    json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 1, "id": "c2", "function": { "name": "Search", "arguments": "{\"q\": \"x\"}" } }] } }] }),
                    json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
                ];
                let mut sse: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
                sse.push_str("data: [DONE]\n\n");
                ([(header::CONTENT_TYPE, "text/event-stream")], sse).into_response()
            } else {
                AxJson(json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "content": null, "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "read", "arguments": "{\"file_path\": \"a.rs\", \"offset\": \"40\"}" } },
                    { "id": "c2", "type": "function", "function": { "name": "Search", "arguments": "{\"q\": \"x\"}" } },
                ] } }], "usage": { "prompt_tokens": 10, "completion_tokens": 5 } }))
                .into_response()
            }
        }),
    );
    format!("{}/v1", serve(router).await)
}

/// The box, serving the stand-in, and an app key for it.
async fn harness() -> (reqwest::Client, String, String, String) {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    state.adopt_serving(Endpoint {
        base_url: stub_vllm().await,
        model: "openai/gpt-oss-20b".into(),
        requires_key: false,
    });
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    let init: Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pair_id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&pair_id).unwrap();
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let minted: Value = client
        .post(format!("{base}/keys"))
        .bearer_auth(done["token"].as_str().unwrap())
        .json(&json!({ "name": "Claude Code" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        client,
        base,
        minted["key"].as_str().unwrap().to_string(),
        done["token"].as_str().unwrap().to_string(),
    )
}

fn request(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-4", "max_tokens": 512, "stream": stream,
        "messages": [{ "role": "user", "content": "Read a.rs from line 40" }],
        "tools": [
            { "name": "Read", "description": "Read a file", "input_schema": { "type": "object", "additionalProperties": false, "required": ["file_path"],
                "properties": { "file_path": { "type": "string" }, "offset": { "type": "number" }, "limit": { "type": "number" } } } },
            { "name": "Grep", "description": "Search files", "input_schema": { "type": "object", "required": ["pattern"],
                "properties": { "pattern": { "type": "string" } } } },
        ],
    })
}

#[tokio::test]
async fn tool_calls_are_checked_mended_and_counted() {
    let (client, base, key, token) = harness().await;

    let m: Value = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth(&key)
        .json(&request(false))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(m["content"][0]["name"], "Read");
    assert_eq!(
        m["content"][0]["input"],
        json!({ "file_path": "a.rs", "offset": 40 })
    );
    // Not a tool the client has: passed on as written, for the client to refuse.
    assert_eq!(m["content"][1]["name"], "Search");
    assert_eq!(m["stop_reason"], "tool_use");

    let sse = client
        .post(format!("{base}/v1/messages"))
        .bearer_auth(&key)
        .json(&request(true))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(sse.contains("\"name\":\"Read\""), "{sse}");
    assert!(
        sse.contains(r#""partial_json":"{\"file_path\":\"a.rs\",\"offset\":40}""#),
        "{sse}"
    );
    assert!(sse.contains("\"name\":\"Search\""));

    let now: Value = client
        .get(format!("{base}/metrics/now"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let health = now["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["model"] == "openai/gpt-oss-20b")
        .cloned()
        .unwrap();
    assert_eq!(health["calls"], 4);
    assert_eq!(health["valid"], 0);
    assert_eq!(health["repaired"], 2);
    assert_eq!(health["invalid"], 2);
    assert_eq!(health["leaked_as_text"], 0);
}
