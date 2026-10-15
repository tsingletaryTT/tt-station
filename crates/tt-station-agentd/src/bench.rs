// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! "How fast is my box?" -- a one-tap benchmark of whatever is serving
//! (`POST /bench`): one streamed answer (time to first token, tokens/s), a batch
//! of parallel requests (aggregate tokens/s -- what the box does for several
//! users at once), one image when an image model serves, and the chips' power
//! meanwhile, so a client can show energy per million tokens.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::Json;
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse};

/// What `crate::bench` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// One benchmark at a time per box.
    running: AtomicBool,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().bench
}

const PROMPT: &str = "Explain how a transformer neural network turns a prompt into an answer, \
                      step by step, for a curious engineer.";
const SINGLE_TOKENS: u32 = 256;
const BATCH: usize = 8;
const BATCH_TOKENS: u32 = 128;

#[derive(Debug, Clone, Serialize, Default)]
pub struct BenchResult {
    pub model: Option<String>,
    pub image_model: Option<String>,
    /// Unix seconds.
    pub at: u64,
    pub ttft_s: Option<f64>,
    /// One request's decode speed.
    pub single_tps: Option<f64>,
    pub single_tokens: Option<u64>,
    /// `BATCH` requests at once: total generated tokens / wall time.
    pub batch_tps: Option<f64>,
    pub batch_size: usize,
    pub image_s: Option<f64>,
    /// Mean total chip power while it ran.
    pub watts: Option<f64>,
    /// Energy for a million generated tokens at the batch rate.
    pub wh_per_million_tokens: Option<f64>,
    pub notes: Vec<String>,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Total chip power in the latest recorded sample.
fn current_watts(state: &AppState) -> Option<f64> {
    let point = state.history().latest()?;
    let watts: f32 = point.chips.iter().filter_map(|c| c.power_w).sum();
    (watts > 0.0).then_some(watts as f64)
}

/// One streamed completion: (time to first token, decode tokens/s, tokens).
async fn single(
    client: &reqwest::Client,
    url: &str,
    model: &str,
) -> anyhow::Result<(f64, f64, u64)> {
    let started = Instant::now();
    let response = client
        .post(url)
        .json(&json!({
            "model": model,
            "messages": [{ "role": "user", "content": PROMPT }],
            "max_tokens": SINGLE_TOKENS,
            "temperature": 0,
            // Plain decoding: a thinking model's hidden reasoning would
            // otherwise delay the first visible token and skew the rate.
            "chat_template_kwargs": { "enable_thinking": false },
            "stream": true,
            "stream_options": { "include_usage": true },
        }))
        .send()
        .await?
        .error_for_status()?;
    let mut stream = response.bytes_stream();
    let mut first: Option<f64> = None;
    let mut tokens: Option<u64> = None;
    let mut deltas = 0u64;
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        while let Some(end) = buffer.find('\n') {
            let line: String = buffer.drain(..=end).collect();
            let Some(data) = line.trim().strip_prefix("data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
                continue;
            };
            let delta = &event["choices"][0]["delta"];
            let content = delta["content"]
                .as_str()
                .or(delta["reasoning_content"].as_str())
                .or(delta["reasoning"].as_str())
                .unwrap_or("");
            if !content.is_empty() {
                deltas += 1;
                first.get_or_insert_with(|| started.elapsed().as_secs_f64());
            }
            if let Some(n) = event["usage"]["completion_tokens"].as_u64() {
                tokens = Some(n);
            }
        }
    }
    let total = started.elapsed().as_secs_f64();
    let ttft = first.ok_or_else(|| anyhow::anyhow!("the model streamed no text"))?;
    let tokens = tokens.unwrap_or(deltas);
    let tps = if total > ttft && tokens > 1 {
        (tokens - 1) as f64 / (total - ttft)
    } else {
        0.0
    };
    Ok((ttft, tps, tokens))
}

/// `BATCH` requests at once: aggregate generated tokens per second.
async fn batch(client: &reqwest::Client, url: &str, model: &str) -> anyhow::Result<f64> {
    let started = Instant::now();
    let requests = (0..BATCH).map(|i| {
        let body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": format!("{PROMPT} (variant {i})") }],
            "max_tokens": BATCH_TOKENS,
            "temperature": 0.7,
            "chat_template_kwargs": { "enable_thinking": false },
        });
        async move {
            let v: Value = client
                .post(url)
                .json(&body)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::Ok(v["usage"]["completion_tokens"].as_u64().unwrap_or(0))
        }
    });
    let tokens: u64 = futures_util::future::try_join_all(requests)
        .await?
        .into_iter()
        .sum();
    Ok(tokens as f64 / started.elapsed().as_secs_f64())
}

/// Run the benchmark against what's serving.
pub async fn run(state: &AppState) -> Result<BenchResult, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let mut result = BenchResult {
        at: unix_now(),
        batch_size: BATCH,
        ..Default::default()
    };
    let mut watts = Vec::new();
    watts.extend(current_watts(state));

    let chat = crate::gateway::chat_endpoint(state).await;
    if let Some(endpoint) = &chat {
        result.model = Some(endpoint.model.clone());
        let url = format!(
            "{}/chat/completions",
            endpoint.base_url.trim_end_matches('/')
        );
        match single(&client, &url, &endpoint.model).await {
            Ok((ttft, tps, tokens)) => {
                result.ttft_s = Some(ttft);
                result.single_tps = Some(tps);
                result.single_tokens = Some(tokens);
            }
            Err(e) => result.notes.push(format!("Single request failed: {e}")),
        }
        watts.extend(current_watts(state));
        match batch(&client, &url, &endpoint.model).await {
            Ok(tps) => result.batch_tps = Some(tps),
            Err(e) => result.notes.push(format!("Batch failed: {e}")),
        }
        watts.extend(current_watts(state));
    }

    if let Some((base, model)) = crate::gateway::image_target(state).await {
        result.image_model = Some(model.clone());
        let request = crate::gateway::media_image_request(
            &json!({ "prompt": "a lighthouse on a cliff at sunset, photograph", "seed": 1 }),
            &model,
        );
        let started = Instant::now();
        let api_key =
            std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
        let ok = client
            .post(format!("{base}/v1/images/generations"))
            .bearer_auth(api_key)
            .json(&request)
            .send()
            .await
            .and_then(|r| r.error_for_status());
        match ok {
            Ok(_) => result.image_s = Some(started.elapsed().as_secs_f64()),
            Err(e) => result.notes.push(format!("Image failed: {e}")),
        }
        watts.extend(current_watts(state));
    }

    if chat.is_none() && result.image_model.is_none() {
        return Err("Nothing is serving. Start a model, then run the benchmark.".to_string());
    }
    if !watts.is_empty() {
        let mean = watts.iter().sum::<f64>() / watts.len() as f64;
        result.watts = Some(mean);
        if let Some(tps) = result.batch_tps.filter(|t| *t > 0.0) {
            // W / (tok/s) = J per token; × 1e6 / 3600 = Wh per million tokens.
            result.wh_per_million_tokens = Some(mean / tps * 1e6 / 3600.0);
        }
    } else {
        result
            .notes
            .push("No chip power readings yet, so no energy figure.".to_string());
    }
    Ok(result)
}

/// `POST /bench` (authed): run it (30 s to a few minutes) and answer the result.
/// `409` while one is running.
pub(crate) async fn post_bench(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<BenchResult>, (StatusCode, Json<ErrorResponse>)> {
    if mine(&state).running.swap(true, Ordering::SeqCst) {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "A benchmark is already running.".to_string(),
            }),
        ));
    }
    let outcome = run(&state).await;
    mine(&state).running.store(false, Ordering::SeqCst);
    outcome
        .map(Json)
        .map_err(|e| (StatusCode::CONFLICT, Json(ErrorResponse { error: e })))
}
