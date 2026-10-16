// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Idle power-down: when nobody has used the served model for a while, stop
//! it (four Blackhole chips serving draw a couple of hundred watts doing
//! nothing), and start it again when something asks for it.
//!
//! "Used" is read from the model server itself (vLLM's `/metrics`: requests
//! running or waiting, tokens generated), so every client counts -- the app,
//! the phone, `/v1` apps, Code, routines -- not just the ones going through
//! a particular route. Off unless turned on.
//!
//! Waking: Chat, Tori, `/v1` and Code start the model that was put to sleep
//! when they find nothing serving, and answer "waking up" (`503` with
//! `Retry-After` for API clients) instead of failing. Models take minutes to
//! load, so the request isn't held open.
//!
//! - `GET /idle`, `PUT /idle {enabled, after_minutes, wake_on_request}`, `POST /idle/wake`,
//!   `POST /idle/sleep` (put the model to sleep now, as idle sleep would; it wakes the same ways)

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub enabled: bool,
    pub after_minutes: u32,
    pub wake_on_request: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            after_minutes: 60,
            wake_on_request: true,
        }
    }
}

/// The model put to sleep, to start again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Asleep {
    /// A community package id, or a model name for the official catalog.
    pub target: String,
    pub community: bool,
    /// As a person would say it.
    pub label: String,
    pub at: u64,
}

#[derive(Debug, Default)]
struct Watch {
    /// Tokens generated so far, as last read.
    tokens: Option<f64>,
    last_active: u64,
    /// Waking now (so two requests don't start it twice).
    waking: bool,
    /// The serve being watched (`AppState::serve_key`): what `tokens` and
    /// `last_active` are about.
    serve: Option<String>,
}

impl Watch {
    /// Watch `serve` from now on: a different serve than last time (a model
    /// started since) starts fresh, as just active.
    fn follow(&mut self, serve: &str, now: u64) {
        if self.serve.as_deref() != Some(serve) {
            self.serve = Some(serve.to_string());
            self.tokens = None;
            self.last_active = now;
        }
    }
}

/// What `crate::idle` keeps for the life of one agent (`crate::state::PerAgent`).
pub(crate) struct PerAgent {
    /// Set up on first use, so "last active" starts when the watch does.
    watch: LazyLock<Mutex<Watch>, fn() -> Mutex<Watch>>,
}

impl Default for PerAgent {
    fn default() -> Self {
        PerAgent {
            watch: LazyLock::new(|| {
                Mutex::new(Watch {
                    last_active: now(),
                    ..Default::default()
                })
            }),
        }
    }
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().idle
}

fn settings_file(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("idle.json")
}

fn asleep_file(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("idle-asleep.json")
}

pub fn settings(state: &AppState) -> Settings {
    std::fs::read(settings_file(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn asleep(state: &AppState) -> Option<Asleep> {
    std::fs::read(asleep_file(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn set_asleep(state: &AppState, a: Option<&Asleep>) {
    match a {
        Some(a) => {
            let _ = std::fs::write(
                asleep_file(state),
                serde_json::to_vec_pretty(a).unwrap_or_default(),
            );
        }
        None => {
            let _ = std::fs::remove_file(asleep_file(state));
        }
    }
}

/// Reads vLLM's counters: (requests running or waiting, tokens generated so far).
pub fn parse_metrics(text: &str) -> (f64, f64) {
    let mut busy = 0.0;
    let mut tokens = 0.0;
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        let Some((name, value)) = line.rsplit_once(' ') else {
            continue;
        };
        let v: f64 = value.trim().parse().unwrap_or(0.0);
        let metric = name.split('{').next().unwrap_or("");
        match metric {
            "vllm:num_requests_running" | "vllm:num_requests_waiting" => busy += v,
            "vllm:generation_tokens_total" | "vllm:prompt_tokens_total" => tokens += v,
            _ => {}
        }
    }
    (busy, tokens)
}

/// Whether the served model did anything since the last look. `None` when it
/// can't be read (not vLLM, or not up): then it's left alone.
async fn active(state: &AppState) -> Option<bool> {
    let ep = state.endpoint()?;
    let base = ep.base_url.trim_end_matches('/').trim_end_matches("/v1");
    let text = crate::localhttp::local()
        .get(format!("{base}/metrics"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    if !text.contains("vllm:") {
        return None;
    }
    let (busy, tokens) = parse_metrics(&text);
    let mut w = mine(state).watch.lock().expect("idle");
    let moved = w.tokens.is_some_and(|t| (tokens - t).abs() > 0.5);
    w.tokens = Some(tokens);
    Some(busy > 0.0 || moved)
}

/// Things on the box that are using the model even between requests.
fn working(state: &AppState) -> bool {
    let code = crate::workspace::any_busy();
    let routine = crate::routines::is_running(state);
    let _ = state;
    code || routine
}

/// What's serving, as something [`wake`] can start again.
async fn current_target(state: &AppState) -> Option<Asleep> {
    let life = state.lifecycle();
    if let Some(id) = crate::community::serving_package(state, &life) {
        return Some(Asleep {
            label: id.rsplit('/').next().unwrap_or(&id).to_string(),
            target: id,
            community: true,
            at: now(),
        });
    }
    let model = life.model.clone().or(life.served_id.clone())?;
    Some(Asleep {
        label: model.rsplit('/').next().unwrap_or(&model).to_string(),
        target: model,
        community: false,
        at: now(),
    })
}

/// Watches forever; started once, at agent start.
pub async fn watch_forever(state: AppState) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let s = settings(&state);
        let life = state.lifecycle();
        if life.phase != Phase::Serving {
            mine(&state).watch.lock().expect("idle").last_active = now();
            mine(&state).watch.lock().expect("idle").tokens = None;
            continue;
        }
        // What this pass decides about; checked again before stopping it.
        let serve = state.serve_key();
        mine(&state)
            .watch
            .lock()
            .expect("idle")
            .follow(&serve, now());
        match active(&state).await {
            Some(true) => mine(&state).watch.lock().expect("idle").last_active = now(),
            Some(false) if working(&state) => {
                mine(&state).watch.lock().expect("idle").last_active = now()
            }
            Some(false) => {}
            None => continue,
        }
        if !s.enabled {
            continue;
        }
        let idle_for = now().saturating_sub(mine(&state).watch.lock().expect("idle").last_active);
        if idle_for < s.after_minutes.max(5) as u64 * 60 {
            continue;
        }
        // A start, stop or swap during the metrics read: this pass's
        // reading was about another serve. Leave the new one alone.
        if state.serve_key() != serve {
            continue;
        }
        let Some(target) = current_target(&state).await else {
            continue;
        };
        if state.serve_key() != serve {
            continue;
        }
        eprintln!(
            "tt-station-agentd: idle for {} min: stopping {}",
            idle_for / 60,
            target.target
        );
        set_asleep(&state, Some(&target));
        if let Err(e) = state.stop_serving().await {
            eprintln!("tt-station-agentd: idle stop failed: {e:#}");
            set_asleep(&state, None);
            continue;
        }
        state.notifier().emit(crate::notify::Draft {
            kind: "power",
            title: format!("{} stopped to save power", target.label),
            body: format!(
                "Nobody used it for {} minutes.{}",
                idle_for / 60,
                if s.wake_on_request {
                    " It starts again when something asks for it."
                } else {
                    ""
                }
            ),
        });
    }
}

/// Starts the model put to sleep. A line for the person, with how long, or
/// `None` when there's nothing to wake (or waking is off).
pub async fn wake_for_request(state: &AppState) -> Option<String> {
    let s = settings(state);
    let a = asleep(state)?;
    if !s.wake_on_request {
        return None;
    }
    let life = state.lifecycle();
    let eta = |state: &AppState| {
        let life = state.lifecycle();
        let total = life
            .model
            .as_deref()
            .or(Some(a.target.as_str()))
            .and_then(|m| state.load_times().estimate(m));
        total
            .map(|t| t.saturating_sub(now().saturating_sub(life.since)))
            .map(|left| format!(" in about {} min", (left / 60).max(1)))
            .unwrap_or_else(|| " in a few minutes".into())
    };
    match life.phase {
        Phase::Starting => {
            return Some(format!(
                "{} is waking up; it'll be ready{}.",
                a.label,
                eta(state)
            ))
        }
        Phase::Serving => return None,
        _ => {}
    }
    {
        let mut w = mine(state).watch.lock().expect("idle");
        if w.waking {
            return Some(format!(
                "{} is waking up; it'll be ready{}.",
                a.label,
                eta(state)
            ));
        }
        w.waking = true;
    }
    let started = wake(state, &a).await;
    mine(state).watch.lock().expect("idle").waking = false;
    match started {
        Ok(()) => Some(format!(
            "{} was asleep to save power. It's waking up now and will be ready{}.",
            a.label,
            eta(state)
        )),
        Err(e) => Some(format!(
            "{} was asleep to save power, and it couldn't be started again: {e}",
            a.label
        )),
    }
}

async fn wake(state: &AppState, a: &Asleep) -> Result<(), String> {
    eprintln!("tt-station-agentd: waking {}", a.target);
    let (ok, msg) = crate::routines::start_model(state, &a.target).await;
    if ok {
        set_asleep(state, None);
        mine(state).watch.lock().expect("idle").last_active = now();
        Ok(())
    } else {
        Err(msg)
    }
}

// ---------------------------------------------------------------- routes

/// `GET /idle`
pub(crate) async fn get_idle(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let w = mine(&state).watch.lock().expect("idle");
    let serving = state.lifecycle().phase == Phase::Serving;
    Json(json!({
        "settings": settings(&state),
        "asleep": asleep(&state),
        "idle_for_s": if serving { Some(now().saturating_sub(w.last_active)) } else { None },
        "waking": w.waking,
    }))
}

/// `PUT /idle {enabled, after_minutes, wake_on_request}`
pub(crate) async fn put_idle(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(mut s): Json<Settings>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    s.after_minutes = s.after_minutes.clamp(5, 24 * 60);
    std::fs::write(
        settings_file(&state),
        serde_json::to_vec_pretty(&s).unwrap_or_default(),
    )
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    if !s.enabled {
        // Turning it off forgets nothing: a model put to sleep can still be woken by hand.
        mine(&state).watch.lock().expect("idle").last_active = now();
    }
    Ok(Json(json!({ "settings": s })))
}

/// `POST /idle/wake`: start the model put to sleep, now.
pub(crate) async fn post_wake(
    State(state): State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let a = asleep(&state).ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "Nothing is asleep." })),
        )
    })?;
    wake(&state, &a)
        .await
        .map_err(|e| (StatusCode::CONFLICT, Json(json!({ "error": e }))))?;
    Ok(Json(json!({ "waking": a.label })))
}

/// `POST /idle/sleep`: put the served model to sleep now, the same as idle
/// sleep does after a quiet spell: stop it and remember it, so `POST
/// /idle/wake` (and, with `wake_on_request`, the next Chat, Tori, `/v1` or
/// Code request) starts it again. The machine stays on and reachable, unlike
/// suspend. `409` when nothing is serving.
pub(crate) async fn post_sleep(
    State(state): State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let nothing = || {
        (
            StatusCode::CONFLICT,
            Json(json!({ "error": "Nothing is serving, so there's nothing to put to sleep." })),
        )
    };
    if state.lifecycle().phase != Phase::Serving {
        return Err(nothing());
    }
    let target = current_target(&state).await.ok_or_else(nothing)?;
    set_asleep(&state, Some(&target));
    if let Err(e) = state.stop_serving().await {
        set_asleep(&state, None);
        crate::audit::record(&state, "model_sleep", &target.target, "failed");
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": format!("{} couldn't be stopped: {e:#}", target.label) })),
        ));
    }
    crate::audit::record(&state, "model_sleep", &target.target, "ok");
    let s = settings(&state);
    state.notifier().record(crate::notify::Draft {
        kind: "power",
        title: format!("{} is asleep", target.label),
        body: if s.wake_on_request {
            "It starts again when something asks for it, or when you wake it.".to_string()
        } else {
            "Wake it when you need it.".to_string()
        },
    });
    Ok(Json(json!({
        "asleep": target.label,
        "wake_on_request": s.wake_on_request,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_serve_is_watched_from_scratch() {
        let mut w = Watch::default();
        w.follow("a", 100);
        w.tokens = Some(5.0);
        w.follow("a", 900);
        assert_eq!(
            (w.last_active, w.tokens),
            (100, Some(5.0)),
            "same serve: kept"
        );
        // A model started since the last look isn't idle for the old one's time.
        w.follow("b", 1000);
        assert_eq!((w.last_active, w.tokens), (1000, None));
    }

    #[test]
    fn reads_vllm_counters() {
        let text = "# HELP x\nvllm:num_requests_running{engine=\"0\",model_name=\"m\"} 2.0\nvllm:num_requests_waiting{engine=\"0\"} 1.0\n\
                    vllm:num_requests_waiting_by_reason{reason=\"capacity\"} 5.0\nvllm:prompt_tokens_total{engine=\"0\"} 8.865026e+06\n\
                    vllm:generation_tokens_total{engine=\"0\"} 54311.0\n";
        let (busy, tokens) = parse_metrics(text);
        assert_eq!(
            busy, 3.0,
            "waiting_by_reason is a breakdown, not more requests"
        );
        assert_eq!(tokens, 8_865_026.0 + 54_311.0);
    }
}
