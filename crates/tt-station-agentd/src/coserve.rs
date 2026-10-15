// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Co-serving: two models at once, one per P300 card -- chat with images, or
//! two chat models (say, a general one and a coder).
//!
//! A P300x2 box is two P300 cards (chips 0,1 and 2,3). Instead of one model on
//! all four chips, split mode serves a chat model on the first card -- through
//! the agent's normal backend, so the assistant, `/status` and metrics follow it
//! unchanged -- and an image model on the second, through a *companion* backend
//! with its own port and lifecycle. Each slot resets only its own chips and
//! sweeps only containers on its own port, so starting or stopping one never
//! touches the other.
//!
//! Only models tt supports on a single `p300` qualify -- e.g. Llama-3.1-8B-
//! Instruct with FLUX.1-schnell. Qwen3-32B needs all four chips.
//!
//! With two chat models, requests go by name: an API client that asks for a
//! model gets that one (`/v1/models` lists both), Code sessions use the second
//! one unless they name the first, and everything else uses the first.
//!
//! Leaving split mode is automatic: any ordinary start (`/run`, `/serve`, the
//! assistant) or stop stops the companion and un-pins the main backend first.

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use libttstation::model::Endpoint;
use serde::Deserialize;

use crate::routes::{AppState, BearerAuth, ErrorResponse, Lifecycle, Phase};
use crate::serving::{Placement, ServingBackend};

/// The device config for one slot.
pub const SLOT_DEVICE: &str = "p300";
/// Chips for the chat model (the main backend) and the image model (companion).
pub const CHAT_CHIPS: &str = "0,1";
pub const MEDIA_CHIPS: &str = "2,3";

pub fn chat_placement() -> Placement {
    Placement {
        device: SLOT_DEVICE.to_string(),
        device_ids: CHAT_CHIPS.to_string(),
    }
}

pub fn media_placement() -> Placement {
    Placement {
        device: SLOT_DEVICE.to_string(),
        device_ids: MEDIA_CHIPS.to_string(),
    }
}

/// The second serving slot.
pub struct Companion {
    backend: Arc<dyn ServingBackend>,
    lifecycle: Mutex<Lifecycle>,
    endpoint: Mutex<Option<Endpoint>>,
}

impl std::fmt::Debug for Companion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Companion")
            .field("lifecycle", &self.lifecycle())
            .field("endpoint", &self.endpoint())
            .finish()
    }
}

impl Companion {
    pub fn new(backend: Arc<dyn ServingBackend>) -> Self {
        backend.set_placement(Some(media_placement()));
        Companion {
            backend,
            lifecycle: Mutex::new(Lifecycle::default()),
            endpoint: Mutex::new(None),
        }
    }

    pub fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
            .lock()
            .expect("companion mutex poisoned")
            .clone()
    }

    pub fn endpoint(&self) -> Option<Endpoint> {
        self.endpoint
            .lock()
            .expect("companion mutex poisoned")
            .clone()
    }

    fn set(
        &self,
        phase: Phase,
        model: Option<String>,
        served_id: Option<String>,
        error: Option<String>,
    ) {
        *self.lifecycle.lock().expect("companion mutex poisoned") = Lifecycle {
            phase,
            model,
            served_id,
            since: unix_now(),
            error,
            adopted: false,
        };
    }

    /// Whether this slot is doing anything (so the box is split).
    pub fn is_active(&self) -> bool {
        !matches!(self.lifecycle().phase, Phase::Idle | Phase::Failed)
    }

    /// Bring `model` up on the companion's chips. Blocks the task for the whole
    /// load; records the outcome in the companion's lifecycle.
    async fn start(&self, model: &str) -> anyhow::Result<Endpoint> {
        self.set(Phase::Starting, Some(model.to_string()), None, None);
        let backend = Arc::clone(&self.backend);
        let wanted = model.to_string();
        let result = tokio::task::spawn_blocking(move || backend.start(&wanted))
            .await
            .map_err(|e| anyhow::anyhow!("companion start panicked: {e}"))
            .and_then(|r| r);
        match &result {
            Ok(endpoint) => {
                *self.endpoint.lock().expect("companion mutex poisoned") = Some(endpoint.clone());
                self.set(
                    Phase::Serving,
                    Some(model.to_string()),
                    Some(endpoint.model.clone()),
                    None,
                );
            }
            Err(e) => {
                if self.lifecycle().phase != Phase::Stopping {
                    eprintln!("tt-station-agentd: companion start of {model} failed: {e:#}");
                    self.set(
                        Phase::Failed,
                        Some(model.to_string()),
                        None,
                        Some(format!("{e:#}")),
                    );
                }
            }
        }
        result
    }

    /// Stop whatever the companion serves (idempotent; also cancels a start).
    pub async fn stop(&self) -> anyhow::Result<()> {
        let was = self.lifecycle();
        if was.phase == Phase::Idle {
            return Ok(());
        }
        self.set(Phase::Stopping, was.model.clone(), None, None);
        let backend = Arc::clone(&self.backend);
        let model = was.model.unwrap_or_default();
        let stopped = tokio::task::spawn_blocking(move || backend.stop(&model))
            .await
            .map_err(|e| anyhow::anyhow!("companion stop panicked: {e}"))
            .and_then(|r| r);
        *self.endpoint.lock().expect("companion mutex poisoned") = None;
        match &stopped {
            Ok(()) => self.set(Phase::Idle, None, None, None),
            Err(e) => self.set(
                Phase::Failed,
                None,
                None,
                Some(format!("stop failed: {e:#}")),
            ),
        }
        stopped
    }

    /// Adopt an image server already running on the companion's port at agent
    /// startup.
    pub fn adopt(&self, endpoint: Endpoint) {
        eprintln!(
            "tt-station-agentd: adopting companion {} at {}",
            endpoint.model, endpoint.base_url
        );
        let id = endpoint.model.clone();
        *self.endpoint.lock().expect("companion mutex poisoned") = Some(endpoint);
        self.set(Phase::Serving, None, Some(id), None);
        self.lifecycle
            .lock()
            .expect("companion mutex poisoned")
            .adopted = true;
    }

    /// What the companion serves: `chat`, `image`, ...
    pub fn kind(&self) -> &'static str {
        let life = self.lifecycle();
        let id = self.endpoint().map(|e| e.model).or(life.served_id).or(life.model.clone());
        crate::routes::model_kind(None, id.as_deref())
    }

    /// The companion's chat endpoint, when it serves a chat model.
    pub fn chat(&self) -> Option<Endpoint> {
        (self.lifecycle().phase == Phase::Serving && self.kind() == "chat").then(|| self.endpoint()).flatten()
    }

    /// The companion's base URL for `kind` generation, when it serves that kind.
    pub fn media_base(&self, kind: &str) -> Option<String> {
        let life = self.lifecycle();
        if life.phase != Phase::Serving {
            return None;
        }
        let endpoint = self.endpoint()?;
        let served = crate::routes::model_kind(None, Some(&endpoint.model));
        let asked = crate::routes::model_kind(
            None,
            life.model.as_deref().or(Some(endpoint.model.as_str())),
        );
        if served != kind && asked != kind {
            return None;
        }
        Some(
            endpoint
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_string(),
        )
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Leave split mode before an ordinary whole-box start or a stop: stop the
/// companion and un-pin the main backend. A no-op when not split.
pub async fn leave_split(state: &AppState) {
    if let Some(companion) = state.companion() {
        if let Err(e) = companion.stop().await {
            eprintln!("tt-station-agentd: stopping the companion model failed: {e:#}");
        }
    }
    state.backend().set_placement(None);
}

/// Whether the box is split right now.
pub fn is_split(state: &AppState) -> bool {
    state.backend().placement().is_some() || state.companion().is_some_and(|c| c.is_active())
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

/// Why this box can't co-serve, if it can't.
fn unavailable(state: &AppState) -> Option<String> {
    if let Some(holder) = state.chips_holder() {
        return Some(holder.to_string());
    }
    if state.companion().is_none() {
        return Some("This agent isn't set up to run two models at once.".to_string());
    }
    if state.device_mesh() != Some("p300x2") {
        return Some(format!(
            "Running two models at once needs a P300x2 box (this one is {}).",
            state.device_mesh().unwrap_or("unknown")
        ));
    }
    None
}

/// One model as a co-serve choice.
#[derive(Debug, serde::Serialize)]
struct Choice {
    name: String,
    ready: bool,
    /// A known problem running this model on half the box, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

/// Known problems with models on a single P300 card, from testing on a QuietBox.
pub fn half_box_note(model: &str) -> Option<&'static str> {
    // tt-media-inference-server 0.10: FLUX on a 1x2 mesh renders fixed-position
    // noise strips (same pixels on chips 0,1 and 2,3); on the whole box (2x2)
    // the same prompts and seeds are clean. Verified 2026-10-07.
    model.to_lowercase().starts_with("flux").then_some(
        "Known issue: on half the box this model's images currently show thin noise strips. \
         For clean images, run it on the whole box.",
    )
}

/// Models tt supports on a single `p300` card, by kind, with whether their
/// weights are on the box.
fn choices_blocking(state: &AppState) -> (Vec<Choice>, Vec<Choice>) {
    let Some(tt) = state.tt_cli() else {
        return (Vec::new(), Vec::new());
    };
    let Ok(catalog) = tt.catalog(Some(SLOT_DEVICE), false) else {
        return (Vec::new(), Vec::new());
    };
    let mut chat = Vec::new();
    let mut image = Vec::new();
    for m in catalog.models.iter().filter(|m| m.supported) {
        let ready = matches!(
            m.weights,
            crate::ttcli::Weights::Complete | crate::ttcli::Weights::Container
        );
        let choice = Choice {
            name: m.name.clone(),
            ready,
            note: half_box_note(&m.name),
        };
        match m.model_type.as_str() {
            "llm" if m.tool_call_parser.is_some() => chat.push(choice),
            "image" => image.push(choice),
            _ => {}
        }
    }
    (chat, image)
}

/// `GET /coserve` (paired, like `/serve`): whether the box can split, the
/// layout, what each slot is doing, and the models that fit a slot.
pub(crate) async fn get_coserve(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    let probe = state.clone();
    let (chat, image) = tokio::task::spawn_blocking(move || choices_blocking(&probe))
        .await
        .unwrap_or_default();
    let companion = state.companion().map(|c| {
        let life = c.lifecycle();
        serde_json::json!({
            "phase": life.phase,
            "model": life.model,
            "served_id": life.served_id,
            "since": life.since,
            "error": life.error,
            "kind": c.kind(),
        })
    });
    Json(serde_json::json!({
        "available": unavailable(&state).is_none(),
        "reason": unavailable(&state),
        "split": is_split(&state),
        "layout": { "chat": CHAT_CHIPS, "image": MEDIA_CHIPS, "second": MEDIA_CHIPS },
        "chat": state.lifecycle(),
        "image": companion,
        "second": companion,
        "choices": { "chat": chat, "image": image },
    }))
}

/// `{chat, image}` or `{chat, second}`: the second card's model, an image
/// model or another chat model.
#[derive(Debug, Deserialize)]
pub struct CoserveRequest {
    pub chat: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub second: Option<String>,
}

/// `POST /coserve` (authed) `{chat, image}`: stop whatever serves, then bring
/// the chat model up on chips 0,1 and the image model on 2,3, side by side.
/// `202` at once; poll `GET /coserve` (or `/serve`, which carries the
/// companion too).
pub(crate) async fn post_coserve(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<CoserveRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    if let Some(reason) = unavailable(&state) {
        return Err(err(StatusCode::CONFLICT, reason));
    }
    crate::audit::record(&state, "model_switch", &format!("{} + {}", req.chat, req.image.as_deref().unwrap_or("nothing")), "requested");
    let companion = state.companion().expect("checked by unavailable");
    let busy = |p: Phase| matches!(p, Phase::Starting | Phase::Stopping);
    if busy(state.lifecycle().phase) || busy(companion.lifecycle().phase) {
        return Err(err(
            StatusCode::CONFLICT,
            "The box is already starting or stopping a model. Wait a moment.",
        ));
    }
    let probe = state.clone();
    let (chat, image) = tokio::task::spawn_blocking(move || choices_blocking(&probe))
        .await
        .unwrap_or_default();
    let check = |list: &[Choice],
                 wanted: &str,
                 what: &str|
     -> Result<String, (StatusCode, Json<ErrorResponse>)> {
        let wanted_short = wanted.rsplit('/').next().unwrap_or(wanted);
        let found = list
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(wanted_short))
            .ok_or_else(|| {
                let names: Vec<&str> = list.iter().map(|c| c.name.as_str()).collect();
                err(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "{wanted} can't be the {what} model on half the box. Options: {}.",
                        if names.is_empty() {
                            "none".to_string()
                        } else {
                            names.join(", ")
                        }
                    ),
                )
            })?;
        if !found.ready {
            return Err(err(
                StatusCode::CONFLICT,
                format!("{} isn't fully downloaded yet.", found.name),
            ));
        }
        Ok(found.name.clone())
    };
    let chat_model = check(&chat, &req.chat, "chat")?;
    let image_model = match (&req.image, &req.second) {
        (Some(i), None) => check(&image, i, "image")?,
        (None, Some(sec)) => {
            let both: Vec<Choice> = chat.iter().chain(image.iter()).map(|c| Choice { name: c.name.clone(), ready: c.ready, note: c.note }).collect();
            check(&both, sec, "second")?
        }
        _ => return Err(err(StatusCode::BAD_REQUEST, "Say what the second card serves: `image` or `second`.")),
    };
    if image_model.eq_ignore_ascii_case(&chat_model) {
        return Err(err(StatusCode::BAD_REQUEST, "Pick two different models (one model on both cards is the whole box: start it normally)."));
    }

    let bg = state.clone();
    let (chat_bg, image_bg) = (chat_model.clone(), image_model.clone());
    tokio::spawn(async move {
        // Clear the whole box first (this also leaves any earlier split).
        if let Err(e) = bg.stop_serving().await {
            eprintln!("tt-station-agentd: co-serve: stopping the current model failed: {e:#}");
        }
        bg.backend().set_placement(Some(chat_placement()));
        let companion = bg.companion().expect("checked above");
        // Both load at once; the backends serialize their chip resets.
        let image_started = std::time::Instant::now();
        let (chat_result, image_result) = tokio::join!(bg.start_serving_placed(&chat_bg), async {
            let r = companion.start(&image_bg).await;
            if r.is_ok() {
                bg.load_times()
                    .record(&image_bg, image_started.elapsed().as_secs());
            }
            r
        });
        if let Err(e) = chat_result {
            eprintln!("tt-station-agentd: co-serve chat start failed: {e:#}");
        }
        if let Err(e) = image_result {
            eprintln!("tt-station-agentd: co-serve image start failed: {e:#}");
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "starting",
            "chat": chat_model,
            "image": image_model,
        })),
    ))
}

/// Adopt servers already running at agent startup: an image server on the
/// companion's port means the box was split, so the main backend is pinned to
/// the chat card again.
pub fn adopt_at_startup(state: &AppState, companion_endpoint: Option<Endpoint>) {
    if let (Some(companion), Some(endpoint)) = (state.companion(), companion_endpoint) {
        companion.adopt(endpoint);
        state.backend().set_placement(Some(chat_placement()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_cover_the_two_cards_without_overlap() {
        let chat: Vec<&str> = CHAT_CHIPS.split(',').collect();
        let media: Vec<&str> = MEDIA_CHIPS.split(',').collect();
        assert!(chat.iter().all(|c| !media.contains(c)));
        assert_eq!(chat.len() + media.len(), 4);
        assert_eq!(chat_placement().device, "p300");
    }
}
