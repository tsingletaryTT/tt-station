// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The owner's preferred models: which chat, image and speech models
//! the box's modes start ("Switch to chat" starts *your* chat model), and
//! which CPU model Tori thinks with while the chips are busy. Unset means
//! "the best this box has" (each mode's built-in order).
//!
//! - `GET /preferences/models`: the choices for each, and what's picked.
//! - `PUT /preferences/models` `{chat?, image?, transcribe?, cpu_chat?}`;
//!   `""` goes back to automatic.

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelPrefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcribe: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_chat: Option<String>,
}

impl ModelPrefs {
    /// The preferred model for a mode id.
    pub fn for_kind(&self, kind: &str) -> Option<&str> {
        match kind {
            "chat" => self.chat.as_deref(),
            "image" | "images" => self.image.as_deref(),
            "transcribe" | "speech" => self.transcribe.as_deref(),
            _ => None,
        }
    }
}

fn path(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("model-prefs.json")
}

pub fn load(state: &AppState) -> ModelPrefs {
    crate::fsio::read_json_or_backup(&path(state)).unwrap_or_default()
}

fn save(state: &AppState, prefs: &ModelPrefs) -> Result<(), String> {
    crate::fsio::write_json(&path(state), prefs, crate::fsio::PRIVATE).map_err(|e| e.to_string())
}

/// What kind of model a catalog entry is: chat, image, video, speech, voice, or other.
pub fn kind_of(model_type: &str, name: &str) -> &'static str {
    let n = name.to_lowercase();
    match model_type {
        "image" => "image",
        "video" => "video",
        "audio" => "speech",
        "text_to_speech" => "voice",
        "llm" | "vlm" => "chat",
        _ if n.contains("flux") || n.contains("diffusion") || n.contains("sdxl") => "image",
        _ if n.contains("wan2") || n.contains("mochi") => "video",
        _ if n.contains("whisper") => "speech",
        _ => "other",
    }
}

async fn ollama_models() -> Vec<String> {
    let Ok(res) = crate::localhttp::local()
        .get("http://127.0.0.1:11434/api/tags")
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return Vec::new();
    };
    let v: Value = res.json().await.unwrap_or_default();
    v["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| m["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// `GET /preferences/models` (authed).
pub(crate) async fn get_prefs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let probe = state.clone();
    let catalog = tokio::task::spawn_blocking(move || {
        probe
            .tt_cli()
            .and_then(|tt| tt.catalog(probe.device_mesh(), false).ok())
            .map(|c| c.models)
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();
    let mut choices = json!({ "chat": [], "image": [], "speech": [] });
    for m in catalog.iter().filter(|m| m.supported) {
        let kind = kind_of(&m.model_type, &m.name);
        let Some(list) = choices.get_mut(kind).and_then(Value::as_array_mut) else {
            continue;
        };
        list.push(json!({
            "name": m.name,
            "ready": matches!(m.weights, crate::ttcli::Weights::Complete | crate::ttcli::Weights::Container),
            // Tori and Chat need tool calling to act on the box.
            "tools": kind != "chat" || m.tool_call_parser.is_some(),
            "params_b": m.param_count,
        }));
    }
    let cpu = ollama_models().await;
    let prefs = load(&state);
    let cpu_default = std::env::var("TT_STATION_CPU_MODEL")
        .unwrap_or_else(|_| crate::assistant::CPU_MODEL_DEFAULT.into());
    Json(json!({
        "prefs": prefs,
        "choices": choices,
        "cpu_chat": cpu,
        "cpu_default": cpu_default,
    }))
}

#[derive(Debug, Deserialize)]
pub struct PrefsBody {
    pub chat: Option<String>,
    pub image: Option<String>,
    pub transcribe: Option<String>,
    pub cpu_chat: Option<String>,
}

/// `PUT /preferences/models` (authed): `""` clears one back to automatic.
pub(crate) async fn put_prefs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<PrefsBody>,
) -> Result<Json<ModelPrefs>, (StatusCode, Json<ErrorResponse>)> {
    let mut prefs = load(&state);
    let set = |slot: &mut Option<String>, v: Option<String>| {
        if let Some(v) = v {
            let v = v.trim().to_string();
            *slot = (!v.is_empty() && v.len() < 200).then_some(v);
        }
    };
    set(&mut prefs.chat, body.chat);
    set(&mut prefs.image, body.image);
    set(&mut prefs.transcribe, body.transcribe);
    set(&mut prefs.cpu_chat, body.cpu_chat);
    save(&state, &prefs).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse { error: e }),
        )
    })?;
    Ok(Json(prefs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind_of("llm", "Qwen3-32B"), "chat");
        assert_eq!(kind_of("image", "FLUX.1-schnell"), "image");
        assert_eq!(kind_of("", "Wan2.2-T2V-A14B-Diffusers"), "video");
        assert_eq!(kind_of("audio", "whisper-large-v3"), "speech");
        assert_eq!(kind_of("text_to_speech", "speecht5_tts"), "voice");
    }

    #[test]
    fn prefs_pick_by_kind() {
        let p = ModelPrefs {
            chat: Some("Llama-3.3-70B-Instruct".into()),
            ..Default::default()
        };
        assert_eq!(p.for_kind("chat"), Some("Llama-3.3-70B-Instruct"));
        assert_eq!(p.for_kind("images"), None);
    }
}
