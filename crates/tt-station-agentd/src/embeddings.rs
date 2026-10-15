// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Embeddings and reranking on the box's CPU, beside the model on the chips.
//!
//! Search over documents (RAG), research apps, agent memory and knowledge
//! graphs need two small models the chips don't serve yet: an embedding model
//! (text to vectors) and a reranker (which passages answer a question best).
//! Each runs in Hugging Face's text-embeddings-inference (TEI) CPU container,
//! and the box's own API serves them under the same app keys as chat:
//!
//! - `POST /v1/embeddings` (OpenAI's format) → the embeddings service.
//! - `POST /v1/rerank` (the Cohere/Jina format most RAG tools speak) → the reranker.
//!
//! - `GET /embeddings`: both services: on or off, model, ready.
//! - `PUT /embeddings`: `{service: "embed" | "rerank", model}` turns one on (or
//!   switches model); `model: null` turns it off.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth};

const IMAGE: &str = "ghcr.io/huggingface/text-embeddings-inference:cpu-1.9";
/// One cache of downloaded models, shared by both services.
const VOLUME: &str = "tt-embeddings-models";
/// Leave most of the CPU to the box (and to serving the chips).
const CPUS: &str = "8";

pub struct Service {
    pub id: &'static str,
    pub name: &'static str,
    container: &'static str,
    pub port: u16,
    /// (model, what it's good for, vector size; 0 for a reranker)
    pub models: &'static [(&'static str, &'static str, u32)],
}

pub const EMBED: Service = Service {
    id: "embed",
    name: "Embeddings",
    container: "tt-embeddings",
    port: 3040,
    models: &[
        ("BAAI/bge-m3", "Multilingual, best quality: 1024-number vectors, documents up to 8K tokens", 1024),
        ("BAAI/bge-small-en-v1.5", "English only, fastest and smallest: 384-number vectors", 384),
    ],
};

pub const RERANK: Service = Service {
    id: "rerank",
    name: "Reranking",
    container: "tt-rerank",
    port: 3041,
    models: &[
        ("BAAI/bge-reranker-v2-m3", "Multilingual, best quality", 0),
        ("BAAI/bge-reranker-base", "English and Chinese, faster", 0),
    ],
};

fn service(id: &str) -> Option<&'static Service> {
    [&EMBED, &RERANK].into_iter().find(|s| s.id == id)
}

fn settings_path(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("embeddings.json")
}

/// The model each service is set to serve (absent: off).
fn settings(state: &AppState) -> HashMap<String, String> {
    std::fs::read(settings_path(state)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save(state: &AppState, s: &HashMap<String, String>) {
    let _ = std::fs::write(settings_path(state), serde_json::to_vec_pretty(s).unwrap_or_default());
}

/// Setting a service up: its step, or why it failed.
static STEPS: LazyLock<Mutex<HashMap<&'static str, (String, Option<String>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn docker_state(container: &str) -> String {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", container])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "missing".into())
}

/// How long a container's state is reused: `GET /embeddings` is polled while
/// its page is open, and turning a service on or off drops it at once.
const STATE_TTL: Duration = Duration::from_secs(5);

static STATES: LazyLock<Mutex<HashMap<&'static str, (Instant, String)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// [`docker_state`] for the routes: off the async runtime, and reused for [`STATE_TTL`].
async fn docker_state_recent(container: &'static str) -> String {
    if let Some((at, state)) = STATES.lock().expect("embeddings").get(container) {
        if at.elapsed() < STATE_TTL {
            return state.clone();
        }
    }
    let state = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", container])
        .stdin(Stdio::null())
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "missing".into());
    STATES.lock().expect("embeddings").insert(container, (Instant::now(), state.clone()));
    state
}

async fn healthy(port: u16) -> bool {
    crate::localhttp::local()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

/// The model a service serves when it's up and answering.
pub async fn ready(state: &AppState, svc: &Service) -> Option<String> {
    let model = settings(state).get(svc.id).cloned()?;
    healthy(svc.port).await.then_some(model)
}

/// The vector size of the embedding model in use (for apps that must be told it).
pub fn embed_dims(state: &AppState) -> u32 {
    let model = settings(state).get(EMBED.id).cloned().unwrap_or_else(|| EMBED.models[0].0.to_string());
    EMBED.models.iter().find(|(m, _, _)| *m == model).map_or(1024, |(_, _, d)| *d)
}

async fn describe(state: &AppState, svc: &Service) -> Value {
    let model = settings(state).get(svc.id).cloned();
    let step = STEPS.lock().expect("embeddings").get(svc.id).cloned();
    let up = model.is_some() && healthy(svc.port).await;
    let status = match (&model, &step, up) {
        (None, _, _) => "off",
        (_, Some((_, Some(_))), _) => "failed",
        (_, _, true) => "ready",
        (_, Some(_), _) => "starting",
        // Only now does docker have something to say: running but not answering yet.
        _ if docker_state_recent(svc.container).await == "running" => "starting",
        _ => "stopped",
    };
    json!({
        "id": svc.id, "name": svc.name, "model": model, "status": status,
        "step": step.as_ref().map(|(s, _)| s.clone()).filter(|_| status == "starting"),
        "error": step.and_then(|(_, e)| e),
        "models": svc.models.iter().map(|(m, about, dims)| json!({ "id": m, "about": about, "dims": dims })).collect::<Vec<_>>(),
        "endpoint": if svc.id == "embed" { "/v1/embeddings" } else { "/v1/rerank" },
    })
}

/// Turn a service on with `model` (replacing what it served) or off, in the background.
pub fn set(state: &AppState, svc: &'static Service, model: Option<String>) {
    let mut s = settings(state);
    match &model {
        Some(m) => s.insert(svc.id.to_string(), m.clone()),
        None => s.remove(svc.id),
    };
    save(state, &s);
    STEPS.lock().expect("embeddings").remove(svc.id);
    STATES.lock().expect("embeddings").remove(svc.container);
    let _ = Command::new("docker").args(["rm", "-f", svc.container]).stdin(Stdio::null()).output();
    let Some(model) = model else { return };
    STEPS.lock().expect("embeddings").insert(svc.id, ("Downloading the server".into(), None));
    std::thread::spawn(move || {
        let step = |s: &str| {
            STEPS.lock().expect("embeddings").insert(svc.id, (s.to_string(), None));
        };
        let fail = |e: String| {
            STEPS.lock().expect("embeddings").insert(svc.id, ("Failed".into(), Some(e)));
        };
        let pulled = Command::new("docker").args(["pull", IMAGE]).stdin(Stdio::null()).output();
        if !pulled.as_ref().is_ok_and(|o| o.status.success()) {
            return fail(pulled.map(|o| String::from_utf8_lossy(&o.stderr).trim().lines().last().unwrap_or("docker pull failed").to_string()).unwrap_or_else(|e| e.to_string()));
        }
        step("Downloading the model and starting");
        let _ = Command::new("docker").args(["network", "create", "tt-apps"]).stdin(Stdio::null()).output();
        let out = Command::new("docker")
            .args([
                "run", "-d", "--name", svc.container, "--restart", "unless-stopped", "--network", "tt-apps",
                "--label", "tt-station.service=embeddings", "--cpus", CPUS,
                "-p", &format!("127.0.0.1:{}:80", svc.port),
                "-v", &format!("{VOLUME}:/data"),
                IMAGE, "--model-id", &model,
            ])
            .stdin(Stdio::null())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                // It answers once the model is downloaded and loaded; describe() sees that.
                STEPS.lock().expect("embeddings").remove(svc.id);
            }
            Ok(o) => fail(String::from_utf8_lossy(&o.stderr).trim().lines().last().unwrap_or("docker run failed").to_string()),
            Err(e) => fail(e.to_string()),
        }
    });
}

/// Turn embeddings on with the default model if they're off (an app that needs them is being set up).
pub fn ensure_embed(state: &AppState) {
    if settings(state).get(EMBED.id).is_none() {
        set(state, &EMBED, Some(EMBED.models[0].0.to_string()));
    } else if docker_state(EMBED.container) != "running" {
        let _ = Command::new("docker").args(["start", EMBED.container]).stdin(Stdio::null()).output();
    }
}

// ---------------------------------------------------------------- routes

/// `GET /embeddings`.
pub(crate) async fn get_embeddings(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let (embed, rerank) = tokio::join!(describe(&state, &EMBED), describe(&state, &RERANK));
    Json(json!({ "services": [embed, rerank] }))
}

#[derive(Debug, Deserialize)]
pub struct SetBody {
    pub service: String,
    #[serde(default)]
    pub model: Option<String>,
}

/// `PUT /embeddings`.
pub(crate) async fn put_embeddings(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<SetBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let svc = service(&body.service).ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({ "error": "No such service." }))))?;
    if let Some(m) = &body.model {
        if !svc.models.iter().any(|(id, _, _)| id == m) {
            return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": "Pick one of the listed models." }))));
        }
    }
    let st = state.clone();
    let model = body.model.clone();
    tokio::task::spawn_blocking(move || set(&st, svc, model)).await.ok();
    Ok(Json(describe(&state, svc).await))
}

fn openai_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": { "message": msg, "type": "invalid_request_error" } }))).into_response()
}

/// `POST /v1/embeddings` for the gateway: the embeddings service, in OpenAI's format.
pub async fn v1_embeddings(state: &AppState, mut body: Value) -> Option<Response> {
    let model = ready(state, &EMBED).await?;
    if let Some(o) = body.as_object_mut() {
        o.insert("model".into(), json!(model));
        // TEI answers floats only.
        o.remove("encoding_format");
    }
    let r = crate::localhttp::local()
        .post(format!("http://127.0.0.1:{}/v1/embeddings", EMBED.port))
        .json(&body)
        .timeout(Duration::from_secs(300))
        .send()
        .await;
    Some(match r {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let v: Value = r.json().await.unwrap_or(Value::Null);
            (status, Json(v)).into_response()
        }
        Err(e) => openai_error(StatusCode::BAD_GATEWAY, &format!("The embeddings service didn't answer: {e}")),
    })
}

/// A Cohere/Jina rerank request in TEI's terms: `{query, documents, top_n}` → `{query, texts}`.
pub fn tei_rerank_request(body: &Value) -> Option<(Value, Vec<String>)> {
    let query = body["query"].as_str()?.to_string();
    let docs: Vec<String> = body["documents"]
        .as_array()?
        .iter()
        .map(|d| d.as_str().map(str::to_string).or_else(|| d["text"].as_str().map(str::to_string)).unwrap_or_default())
        .collect();
    Some((json!({ "query": query, "texts": docs, "truncate": true }), docs))
}

/// TEI's `[{index, score}]` as Cohere/Jina's `{results: [{index, relevance_score, document}]}`, best first.
pub fn rerank_response(tei: &Value, docs: &[String], top_n: Option<usize>, model: &str, with_docs: bool) -> Value {
    let mut results: Vec<Value> = tei
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let i = r["index"].as_u64()? as usize;
            let mut out = json!({ "index": i, "relevance_score": r["score"] });
            if with_docs {
                out["document"] = json!({ "text": docs.get(i).cloned().unwrap_or_default() });
            }
            Some(out)
        })
        .collect();
    results.sort_by(|a, b| b["relevance_score"].as_f64().partial_cmp(&a["relevance_score"].as_f64()).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(n) = top_n {
        results.truncate(n);
    }
    json!({ "model": model, "results": results })
}

/// `POST /v1/rerank` for the gateway.
pub async fn v1_rerank(state: &AppState, body: Value) -> Response {
    let Some(model) = ready(state, &RERANK).await else {
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, "Reranking isn't on. Turn it on in TT Station → Models → Embeddings and reranking.");
    };
    let Some((request, docs)) = tei_rerank_request(&body) else {
        return openai_error(StatusCode::BAD_REQUEST, "Send a query and a list of documents.");
    };
    if docs.is_empty() {
        return Json(json!({ "model": model, "results": [] })).into_response();
    }
    let r = crate::localhttp::local()
        .post(format!("http://127.0.0.1:{}/rerank", RERANK.port))
        .json(&request)
        .timeout(Duration::from_secs(300))
        .send()
        .await;
    match r {
        Ok(r) if r.status().is_success() => {
            let tei: Value = r.json().await.unwrap_or(Value::Null);
            let top_n = body["top_n"].as_u64().map(|n| n as usize);
            let with_docs = body["return_documents"].as_bool().unwrap_or(false);
            Json(rerank_response(&tei, &docs, top_n, &model, with_docs)).into_response()
        }
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let v: Value = r.json().await.unwrap_or(Value::Null);
            openai_error(status, v["error"].as_str().unwrap_or("The reranker refused it."))
        }
        Err(e) => openai_error(StatusCode::BAD_GATEWAY, &format!("The reranker didn't answer: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rerank_speaks_cohere() {
        let body = json!({ "query": "q", "documents": ["a", { "text": "b" }, "c"], "top_n": 2, "return_documents": true });
        let (req, docs) = tei_rerank_request(&body).unwrap();
        assert_eq!(req["texts"], json!(["a", "b", "c"]));
        let tei = json!([{ "index": 0, "score": 0.1 }, { "index": 2, "score": 0.9 }, { "index": 1, "score": 0.5 }]);
        let out = rerank_response(&tei, &docs, Some(2), "m", true);
        assert_eq!(out["results"][0]["index"], 2);
        assert_eq!(out["results"][1]["document"]["text"], "b");
        assert_eq!(out["results"].as_array().unwrap().len(), 2);
    }
}
