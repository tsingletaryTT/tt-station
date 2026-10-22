// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The OpenAI-compatible `/v1` gateway: app keys, model rewriting, streaming
//! pass-through, and OpenAI image requests on top of the media server -- with a
//! chat model or an image model serving.

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

/// A vLLM stand-in: echoes the model it was asked for; streams two chunks.
async fn stub_vllm() -> String {
    let router = Router::new().route(
        "/v1/chat/completions",
        post(|AxJson(body): AxJson<Value>| async move {
            if body["stream"] == true {
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
                )
                    .into_response()
            } else {
                AxJson(json!({ "model": body["model"], "choices": [{ "message": { "content": "Hello" } }], "usage": { "completion_tokens": 64 } }))
                    .into_response()
            }
        }),
    );
    format!("{}/v1", serve(router).await)
}

async fn stub_media() -> String {
    let router = Router::new().route(
        "/v1/images/generations",
        post(|AxJson(body): AxJson<Value>| async move {
            let n = body["number_of_images"].as_u64().unwrap_or(1) as usize;
            AxJson(json!({ "images": vec!["aGk="; n], "steps": body["num_inference_steps"] }))
        }),
    );
    format!("{}/v1", serve(router).await)
}

struct Harness {
    client: reqwest::Client,
    base: String,
    token: String,
}

async fn chat() -> Endpoint {
    Endpoint {
        base_url: stub_vllm().await,
        model: "meta-llama/Llama-3.1-8B-Instruct".into(),
        requires_key: false,
    }
}

async fn image() -> Endpoint {
    Endpoint {
        base_url: stub_media().await,
        model: "tt-flux.1-schnell".into(),
        requires_key: false,
    }
}

async fn harness(serving: Endpoint) -> Harness {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle))
        .with_device_mesh(Some("p300x2".into()));
    state.adopt_serving(serving);
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
    Harness {
        client,
        base,
        token: done["token"].as_str().unwrap().to_string(),
    }
}

#[tokio::test]
async fn app_keys_unlock_an_openai_api_that_follows_the_model() {
    let h = harness(chat().await).await;

    // No key, a bad key: OpenAI-shaped 401s.
    let anon = h
        .client
        .get(format!("{}/v1/models", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let body: Value = anon.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("App keys"));

    // Only a paired device can mint keys.
    let denied = h
        .client
        .post(format!("{}/keys", h.base))
        .json(&json!({ "name": "Open WebUI" }))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let minted: Value = h
        .client
        .post(format!("{}/keys", h.base))
        .bearer_auth(&h.token)
        .json(&json!({ "name": "Open WebUI" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let key = minted["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("ttq-"));

    // The model is listed, by kind.
    let models: Value = h
        .client
        .get(format!("{}/v1/models", h.base))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kinds: Vec<(&str, &str)> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["id"].as_str().unwrap(), m["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(kinds, vec![("meta-llama/Llama-3.1-8B-Instruct", "chat")]);

    // Any model name is rewritten to the one serving.
    let chat: Value = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "gpt-4o", "messages": [{ "role": "user", "content": "hi" }] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(chat["model"], "meta-llama/Llama-3.1-8B-Instruct");

    // Streaming passes straight through.
    let stream = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "x", "stream": true, "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        stream.headers()["content-type"].to_str().unwrap(),
        "text/event-stream"
    );
    let text = stream.text().await.unwrap();
    assert!(text.contains("\"Hel\"") && text.contains("\"lo\"") && text.contains("[DONE]"));

    // Usage is counted; revoking locks the key out.
    let connect: Value = h
        .client
        .get(format!("{}/connect", h.base))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Plain HTTP is offered only on the box itself; the network gets HTTPS.
    let urls: Vec<&str> = connect["urls"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["url"].as_str())
        .collect();
    assert!(urls.iter().any(|u| u.starts_with("http://127.0.0.1:")));
    assert!(!urls.iter().any(|u| u.starts_with("http://qb2-lab.local:")));
    assert_eq!(connect["keys"][0]["requests"], 3);
    let id = minted["info"]["id"].as_str().unwrap();
    let revoked = h
        .client
        .delete(format!("{}/keys/{id}", h.base))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 204);
    let after = h
        .client
        .get(format!("{}/v1/models", h.base))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401);
}

/// With an image model serving, OpenAI image requests become media-server
/// ones, and back; chat isn't offered.
#[tokio::test]
async fn openai_image_requests_reach_the_image_model() {
    let h = harness(image().await).await;
    let key = h
        .client
        .post(format!("{}/keys", h.base))
        .bearer_auth(&h.token)
        .json(&json!({ "name": "Open WebUI" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["key"]
        .as_str()
        .unwrap()
        .to_string();
    let models: Value = h
        .client
        .get(format!("{}/v1/models", h.base))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"][0]["id"], "tt-flux.1-schnell");
    assert_eq!(models["data"][0]["kind"], "image");
    assert_eq!(models["data"].as_array().unwrap().len(), 1);
    let images: Value = h
        .client
        .post(format!("{}/v1/images/generations", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "dall-e-3", "prompt": "a fox", "n": 2, "response_format": "b64_json" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(images["data"].as_array().unwrap().len(), 2);
    assert_eq!(images["data"][0]["b64_json"], "aGk=");
    let chat = h
        .client
        .post(format!("{}/v1/chat/completions", h.base))
        .bearer_auth(&key)
        .json(&json!({ "model": "x", "messages": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(chat.status(), 503);
}

/// The benchmark measures the chat model (single + batch), or the image model.
#[tokio::test]
async fn bench_measures_chat_or_images() {
    let h = harness(chat().await).await;
    let anon = h
        .client
        .post(format!("{}/bench", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(anon.status(), 401);
    let r: Value = h
        .client
        .post(format!("{}/bench", h.base))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["model"], "meta-llama/Llama-3.1-8B-Instruct", "{r}");
    assert_eq!(r["single_tokens"], 2);
    assert!(r["ttft_s"].as_f64().unwrap() >= 0.0);
    assert!(r["batch_tps"].as_f64().unwrap() > 0.0, "8 x 64 tokens");
    assert_eq!(r["batch_size"], 8);
    assert_eq!(r["image_model"], Value::Null);

    let h = harness(image().await).await;
    let r: Value = h
        .client
        .post(format!("{}/bench", h.base))
        .bearer_auth(&h.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["image_model"], "tt-flux.1-schnell", "{r}");
    assert!(r["image_s"].as_f64().is_some());
}
