// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Is the served chat model actually answering? `/status` used to say
//! `serving` as long as the container and port existed, so a model whose
//! engine had hung looked fine until someone's chat sat there forever.
//!
//! A background watcher asks the served chat model for ONE token every
//! [`Config::every`], with a deadline. [`Config::threshold`] misses in a row
//! mark it `degraded` with a reason; the next answer clears it. It adds no load
//! when the model is busy: if the metrics recorder saw tokens being generated
//! within the last interval, the model is plainly answering and the probe is
//! skipped. Nothing is restarted automatically -- `/status` says what's wrong
//! and the apps offer "Restart model" (`POST /doctor/fix/restart_model`), so a
//! person decides.
//!
//! Docker's own healthcheck is deliberately ignored. The tt-inference-server
//! images bake in a check against `127.0.0.1:8000/health`, but the box serves on
//! other ports (gpt-oss on 8003), so the container shows `unhealthy` (a failing
//! streak in the hundreds) while it answers requests fine. The check is the
//! image's, not ours to change, and it says nothing about inference anyway: a
//! hung engine can still answer `/health`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::routes::{AppState, Phase};

/// How often to check, how long to wait, and how many misses make `degraded`.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub every: Duration,
    pub deadline: Duration,
    pub threshold: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            every: Duration::from_secs(45),
            deadline: Duration::from_secs(20),
            threshold: 3,
        }
    }
}

/// What `/status` says about the served model's health.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Nothing checked yet (or nothing to check: idle, starting, not a chat model).
    Unknown,
    Ok,
    /// Missed `threshold` checks in a row; see `reason`.
    Degraded,
}

/// `/status`'s `model_health`.
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub state: State,
    /// Why it's degraded, as a person would read it.
    pub reason: Option<String>,
    /// Unix seconds the current state began.
    pub since: Option<u64>,
    /// Unix seconds of the last check (or skip because the model was busy).
    pub checked_at: Option<u64>,
    /// Misses in a row.
    pub failures: u32,
    /// The action that fixes a degraded model (the doctor's restart).
    pub restart: Option<&'static str>,
}

#[derive(Default)]
struct Inner {
    /// The served id this record is about; a different model starts over.
    model: Option<String>,
    failures: u32,
    last_error: Option<String>,
    degraded_since: Option<u64>,
    ok_since: Option<u64>,
    checked_at: Option<u64>,
}

/// The served model's health record, held by `AppState`.
#[derive(Default)]
pub struct Liveness {
    inner: Mutex<Inner>,
}

impl Liveness {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Start over for `model` (or nothing) if it isn't the one on record.
    fn track(inner: &mut Inner, model: Option<&str>) {
        if inner.model.as_deref() != model {
            *inner = Inner {
                model: model.map(str::to_string),
                ..Inner::default()
            };
        }
    }

    /// The model answered (a probe, or tokens seen being generated).
    pub fn note_ok(&self, model: &str) {
        let mut i = self.lock();
        Self::track(&mut i, Some(model));
        let now = unix_now();
        if i.degraded_since.is_some() {
            crate::log::info(format!("liveness: {model} is answering again"));
        }
        i.failures = 0;
        i.last_error = None;
        i.degraded_since = None;
        i.ok_since.get_or_insert(now);
        i.checked_at = Some(now);
    }

    /// A probe missed. Returns true when this miss made it `degraded`.
    pub fn note_failure(&self, model: &str, why: String, threshold: u32) -> bool {
        let mut i = self.lock();
        Self::track(&mut i, Some(model));
        let now = unix_now();
        i.failures += 1;
        i.last_error = Some(why);
        i.checked_at = Some(now);
        if i.failures >= threshold.max(1) && i.degraded_since.is_none() {
            i.degraded_since = Some(now);
            i.ok_since = None;
            return true;
        }
        false
    }

    /// Nothing to check (idle, starting, not a chat model): forget the record.
    pub fn clear(&self) {
        Self::track(&mut self.lock(), None);
    }

    /// What `/status` reports.
    pub fn health(&self) -> Health {
        let i = self.lock();
        let (state, since) = match (i.degraded_since, i.ok_since) {
            (Some(t), _) => (State::Degraded, Some(t)),
            (None, Some(t)) => (State::Ok, Some(t)),
            _ => (State::Unknown, None),
        };
        Health {
            reason: (state == State::Degraded).then(|| {
                format!(
                    "The model isn't responding ({} checks in a row: {}).",
                    i.failures,
                    i.last_error.as_deref().unwrap_or("no answer")
                )
            }),
            restart: (state == State::Degraded).then_some("/doctor/fix/restart_model"),
            state,
            since,
            checked_at: i.checked_at,
            failures: i.failures,
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.lock().degraded_since.is_some()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Ask `base` (an OpenAI `/v1` root) for one token of `model` within `deadline`.
/// `Ok` for any answer that shows the server is handling requests: a 2xx, or a
/// 4xx (auth required, an endpoint this server doesn't have) -- it answered, so
/// it isn't hung. A 5xx, a timeout or a refused connection is a miss.
pub async fn probe(base: &str, model: &str, deadline: Duration) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(deadline)
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
        "stream": false,
    });
    match client.post(&url).json(&body).send().await {
        Ok(r) if r.status().is_server_error() => Err(format!("the server answered {}", r.status())),
        Ok(_) => Ok(()),
        Err(e) if e.is_timeout() => Err(format!("no answer within {} s", deadline.as_secs_f32())),
        Err(e) if e.is_connect() => Err("its port refused the connection".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// Did the metrics recorder see tokens being generated in the last `within`?
fn recently_generating(state: &AppState, within: Duration) -> bool {
    let since = unix_now().saturating_sub(within.as_secs().max(1));
    state
        .history()
        .latest()
        .is_some_and(|p| p.t >= since && p.gen_tps.is_some_and(|t| t > 0.0))
}

/// One round: check the served chat model, unless there's nothing to check or
/// it's busy answering real requests.
pub async fn check_once(state: &AppState, cfg: Config) {
    let liveness = state.liveness();
    let (Phase::Serving, Some(endpoint)) = (state.lifecycle().phase, state.endpoint()) else {
        liveness.clear();
        return;
    };
    let probe_state = state.clone();
    let kind = tokio::task::spawn_blocking(move || probe_state.served_kind_blocking())
        .await
        .unwrap_or("other");
    if kind != "chat" {
        liveness.clear();
        return;
    }
    if recently_generating(state, cfg.every) {
        liveness.note_ok(&endpoint.model);
        return;
    }
    match probe(&endpoint.base_url, &endpoint.model, cfg.deadline).await {
        Ok(()) => liveness.note_ok(&endpoint.model),
        Err(why) => {
            crate::log::warn(format!("liveness: {}: {why}", endpoint.model));
            if liveness.note_failure(&endpoint.model, why, cfg.threshold) {
                let name = crate::notify::display_model(&endpoint.model);
                crate::log::error(format!(
                    "liveness: {name} missed {} checks in a row; marking it degraded",
                    cfg.threshold
                ));
                state.notifier().emit(crate::notify::Draft {
                    kind: "model",
                    title: format!("{name} isn't responding"),
                    body: "It's loaded but hasn't answered for a few minutes. Restart it from the app (Restart model)."
                        .to_string(),
                });
            }
        }
    }
}

/// Check every `cfg.every`, forever. Spawned once by `main.rs`.
pub async fn watch_forever(state: AppState, cfg: Config) {
    let mut ticker = tokio::time::interval(cfg.every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once; give a just-adopted model a moment.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let started = Instant::now();
        check_once(&state, cfg).await;
        if started.elapsed() > cfg.every {
            crate::log::warn(format!(
                "liveness: a check took {} s, longer than the interval",
                started.elapsed().as_secs()
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misses_degrade_after_the_threshold_and_one_answer_clears_it() {
        let l = Liveness::default();
        assert_eq!(l.health().state, State::Unknown);
        l.note_ok("m");
        assert_eq!(l.health().state, State::Ok);
        assert!(!l.note_failure("m", "no answer within 20 s".into(), 3));
        assert!(!l.note_failure("m", "no answer within 20 s".into(), 3));
        assert_eq!(l.health().state, State::Ok, "two misses aren't enough");
        assert!(l.note_failure("m", "no answer within 20 s".into(), 3));
        let h = l.health();
        assert_eq!(h.state, State::Degraded);
        assert!(h.reason.unwrap().contains("no answer within 20 s"));
        assert_eq!(h.restart, Some("/doctor/fix/restart_model"));
        // Only the miss that crosses the line reports the transition.
        assert!(!l.note_failure("m", "again".into(), 3));
        l.note_ok("m");
        assert_eq!(l.health().state, State::Ok);
        assert_eq!(l.health().failures, 0);
    }

    #[test]
    fn a_different_model_starts_over() {
        let l = Liveness::default();
        for _ in 0..3 {
            l.note_failure("old", "x".into(), 3);
        }
        assert!(l.is_degraded());
        l.note_failure("new", "x".into(), 3);
        assert!(!l.is_degraded());
        l.clear();
        assert_eq!(l.health().state, State::Unknown);
    }
}
