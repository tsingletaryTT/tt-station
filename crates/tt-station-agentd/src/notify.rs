// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Telling the owner when something they're waiting for happens -- a download
//! finished, a model is loaded and ready, a start failed, the box came online,
//! the chips are running hot -- as ordinary iPhone notifications, app closed.
//!
//! A watcher compares the box's state every few seconds ([`diff`]) and turns
//! changes into [`Event`]s. Each is kept for clients (`GET /events?since=`) and
//! pushed through Apple's push service to every phone that registered its
//! device token (`POST /notify/devices`) -- see `crate::apns`. Pushes carry
//! generic words per kind of event and the event's id, never what happened
//! (see [`push_payload`]); the app fetches the event itself.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse, Phase};

const KEEP_EVENTS: usize = 200;
const WATCH_INTERVAL: Duration = Duration::from_secs(5);
/// Chips at or above this are "hot"; the alert re-arms below [`HOT_REARM_C`].
pub const HOT_C: f32 = 90.0;
pub const HOT_REARM_C: f32 = 80.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Event {
    pub seq: u64,
    /// Unix seconds.
    pub at: u64,
    /// `download_done`, `download_failed`, `model_ready`, `model_failed`,
    /// `box_online`, `chips_hot`, `test`.
    pub kind: String,
    pub title: String,
    pub body: String,
    /// What a client needs to act on it: `{"approval": {...}}` when it asks the
    /// person to allow something (see `GET /approvals`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// What the watcher compares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Main slot: phase, model, error, kind.
    pub main: Option<(Phase, Option<String>, Option<String>, String)>,
    /// Image slot of a split box.
    pub companion: Option<(Phase, Option<String>, Option<String>)>,
    /// Downloads by model: (state, error).
    pub pulls: HashMap<String, (String, Option<String>)>,
    pub hottest_c: Option<f32>,
}

/// An event before it gets a sequence number and time.
#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub kind: &'static str,
    pub title: String,
    pub body: String,
}

/// A model id as a person would say it: `Qwen/Qwen3-32B` → `Qwen3-32B`, and a
/// media server's weights path (`…/Wan2.2-T2V-A14B-Diffusers/snapshots/<hash>`,
/// `…/models--org--Name/…`) → its model name.
pub fn display_model(model: &str) -> String {
    let parts: Vec<&str> = model.trim_end_matches('/').split('/').collect();
    if let Some(i) = parts.iter().position(|p| *p == "snapshots") {
        if i > 0 {
            let name = parts[i - 1];
            return name
                .strip_prefix("models--")
                .and_then(|r| r.split("--").last())
                .unwrap_or(name)
                .to_string();
        }
    }
    parts.last().copied().unwrap_or(model).to_string()
}

fn short(model: &str) -> String {
    display_model(model)
}

fn first_line(s: &str, max: usize) -> String {
    let line = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_string()
    }
}

/// The events between two snapshots. `hot_armed` tracks the chips-hot
/// hysteresis across calls.
pub fn diff(prev: &Snapshot, next: &Snapshot, hot_armed: &mut bool) -> Vec<Draft> {
    let mut out = Vec::new();

    let lifecycle = |slot: &str,
                     was: Option<(Phase, Option<String>, Option<String>)>,
                     now: Option<(Phase, Option<String>, Option<String>)>,
                     what: &str,
                     out: &mut Vec<Draft>| {
        let (Some((was_phase, ..)), Some((now_phase, model, error))) = (was, now) else {
            return;
        };
        if was_phase != Phase::Starting {
            return;
        }
        let name = model
            .as_deref()
            .map(short)
            .unwrap_or_else(|| slot.to_string());
        match now_phase {
            Phase::Serving => out.push(Draft {
                kind: "model_ready",
                title: format!("{name} is ready"),
                body: what.to_string(),
            }),
            Phase::Failed => out.push(Draft {
                kind: "model_failed",
                title: format!("{name} couldn't start"),
                body: first_line(
                    error.as_deref().unwrap_or("Open TT Station for details."),
                    180,
                ),
            }),
            _ => {}
        }
    };

    let main_kind = next.main.as_ref().map(|m| m.3.clone()).unwrap_or_default();
    let what = match main_kind.as_str() {
        "image" => "Loaded and ready: make images in TT Station.",
        "video" => "Loaded and ready: make videos in TT Station.",
        "chat" => "Loaded and ready to chat in TT Station.",
        _ => "Loaded and ready in TT Station.",
    };
    lifecycle(
        "The model",
        prev.main.as_ref().map(|m| (m.0, m.1.clone(), m.2.clone())),
        next.main.as_ref().map(|m| (m.0, m.1.clone(), m.2.clone())),
        what,
        &mut out,
    );
    lifecycle(
        "The image model",
        prev.companion.clone(),
        next.companion.clone(),
        "Loaded beside chat: make images in TT Station.",
        &mut out,
    );

    for (model, (state, error)) in &next.pulls {
        let was = prev.pulls.get(model).map(|(s, _)| s.as_str());
        if was != Some("running") {
            continue;
        }
        match state.as_str() {
            "done" => out.push(Draft {
                kind: "download_done",
                title: format!("{} downloaded", short(model)),
                body: "Ready to start in TT Station.".to_string(),
            }),
            "failed" => out.push(Draft {
                kind: "download_failed",
                title: format!("Download of {} failed", short(model)),
                body: first_line(
                    error.as_deref().unwrap_or("Open TT Station for details."),
                    180,
                ),
            }),
            _ => {}
        }
    }

    if let Some(t) = next.hottest_c {
        if *hot_armed && t >= HOT_C {
            *hot_armed = false;
            out.push(Draft {
                kind: "chips_hot",
                title: format!("Chips are hot: {t:.0} °C"),
                body: "Check the box's airflow; it will slow down to protect itself.".to_string(),
            });
        } else if !*hot_armed && t < HOT_REARM_C {
            *hot_armed = true;
        }
    }
    out
}

/// A phone that asked for push notifications.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Device {
    /// The APNs device token, hex.
    pub token: String,
    /// `true` for development builds (Xcode), which use Apple's sandbox.
    pub sandbox: bool,
    /// The phone's name, for the owner's list.
    #[serde(default)]
    pub name: String,
    /// Unix seconds.
    pub added: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    devices: Vec<Device>,
}

pub struct Notifier {
    path: Option<PathBuf>,
    devices: Mutex<Vec<Device>>,
    events: Mutex<VecDeque<Event>>,
    seq: Mutex<u64>,
    apns: Option<std::sync::Arc<crate::apns::Apns>>,
    /// Why push isn't available, when `apns` is `None` and setup was attempted.
    apns_problem: Option<String>,
    box_name: Mutex<String>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field(
                "devices",
                &self.devices.lock().map(|d| d.len()).unwrap_or(0),
            )
            .field("apns", &self.apns)
            .finish()
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Notifier {
    pub fn in_memory() -> Self {
        Notifier {
            path: None,
            devices: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            seq: Mutex::new(0),
            apns: None,
            apns_problem: None,
            box_name: Mutex::new(String::new()),
        }
    }

    /// Devices from `path`; push from `apns.toml` in the same directory.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let saved: Saved = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let (apns, apns_problem) = match path.parent().map(crate::apns::Apns::load) {
            Some(Ok(Some(apns))) => (Some(std::sync::Arc::new(apns)), None),
            Some(Ok(None)) | None => (None, None),
            Some(Err(e)) => {
                eprintln!("tt-station-agentd: push notifications are off: {e}");
                (None, Some(e))
            }
        };
        Notifier {
            path: Some(path),
            devices: Mutex::new(saved.devices),
            events: Mutex::new(VecDeque::new()),
            // Sequence numbers start at the clock so a restarted agent's events
            // sort after the previous run's for a client asking `since`.
            seq: Mutex::new(unix_now() * 1000),
            apns,
            apns_problem,
            box_name: Mutex::new(String::new()),
        }
    }

    /// Use `apns` for pushes (tests; `open` loads it from `apns.toml`).
    pub fn with_apns(mut self, apns: crate::apns::Apns) -> Self {
        self.apns = Some(std::sync::Arc::new(apns));
        self
    }

    /// The box's name, shown as each notification's subtitle.
    pub fn set_box_name(&self, name: &str) {
        *self.box_name.lock().expect("notify mutex poisoned") = name.to_string();
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices.lock().expect("notify mutex poisoned").clone()
    }

    /// Register (or refresh) a phone's device token.
    pub fn register(&self, token: &str, sandbox: bool, name: &str) -> Result<(), String> {
        let token = token.trim().to_lowercase();
        if token.len() < 32 || !token.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("That isn't an APNs device token.".to_string());
        }
        let mut devices = self.devices.lock().expect("notify mutex poisoned");
        devices.retain(|d| d.token != token);
        devices.push(Device {
            token,
            sandbox,
            name: name.chars().take(60).collect(),
            added: unix_now(),
        });
        drop(devices);
        self.save();
        Ok(())
    }

    pub fn unregister(&self, token: &str) -> bool {
        let token = token.trim().to_lowercase();
        let mut devices = self.devices.lock().expect("notify mutex poisoned");
        let before = devices.len();
        devices.retain(|d| d.token != token);
        let removed = devices.len() != before;
        drop(devices);
        if removed {
            self.save();
        }
        removed
    }

    /// Whether this box can send push notifications, and if not, why.
    pub fn push_status(&self) -> (bool, Option<String>) {
        match (&self.apns, &self.apns_problem) {
            (Some(_), _) => (true, None),
            (None, Some(problem)) => (false, Some(problem.clone())),
            (None, None) => (
                false,
                Some("This box has no push key yet (apns.toml in its config folder).".to_string()),
            ),
        }
    }

    /// Record an event and push it to every registered phone (in the background).
    pub fn emit(self: &std::sync::Arc<Self>, draft: Draft) -> Event {
        self.emit_with(draft, None)
    }

    /// [`Self::emit`], with what a client needs to act on it.
    pub fn emit_with(
        self: &std::sync::Arc<Self>,
        draft: Draft,
        data: Option<serde_json::Value>,
    ) -> Event {
        let event = self.record_with(draft, data);
        let me = std::sync::Arc::clone(self);
        let pushed = event.clone();
        tokio::spawn(async move {
            me.push(&pushed).await;
        });
        event
    }

    /// Record an event and wait for its pushes -- for news sent just before the
    /// box goes down.
    pub async fn emit_and_wait(self: &std::sync::Arc<Self>, draft: Draft) {
        let event = self.record(draft);
        let _ = tokio::time::timeout(Duration::from_secs(8), self.push(&event)).await;
    }

    /// Push `event` to every registered phone; returns how many got it.
    /// What's sent is [`push_payload`]'s version of it.
    /// Forgets tokens Apple says are dead, and fixes a token's environment when
    /// Apple says it's the other one.
    pub async fn push(&self, event: &Event) -> usize {
        let Some(apns) = self.apns.clone() else {
            return 0;
        };
        let subtitle = self.box_name.lock().expect("notify mutex poisoned").clone();
        let payload = push_payload(event, &subtitle);
        let mut sent = 0;
        for device in self.devices() {
            let mut sandbox = device.sandbox;
            let mut outcome = apns.send(&device.token, sandbox, &payload).await;
            if outcome == crate::apns::Delivery::WrongEnvironment {
                sandbox = !sandbox;
                outcome = apns.send(&device.token, sandbox, &payload).await;
                if outcome == crate::apns::Delivery::Sent {
                    let _ = self.register(&device.token, sandbox, &device.name);
                }
            }
            match outcome {
                crate::apns::Delivery::Sent => sent += 1,
                crate::apns::Delivery::Unregistered | crate::apns::Delivery::WrongEnvironment => {
                    eprintln!(
                        "tt-station-agentd: forgetting push device {} (Apple says it's gone)",
                        device.name
                    );
                    self.unregister(&device.token);
                }
                crate::apns::Delivery::Failed(e) => {
                    eprintln!("tt-station-agentd: push to {} failed: {e}", device.name)
                }
            }
        }
        sent
    }

    /// Record an event without pushing it.
    pub fn record(&self, draft: Draft) -> Event {
        self.record_with(draft, None)
    }

    fn record_with(&self, draft: Draft, data: Option<serde_json::Value>) -> Event {
        let event = {
            let mut seq = self.seq.lock().expect("notify mutex poisoned");
            *seq += 1;
            Event {
                seq: *seq,
                at: unix_now(),
                kind: draft.kind.to_string(),
                title: draft.title,
                body: draft.body,
                data,
            }
        };
        {
            let mut events = self.events.lock().expect("notify mutex poisoned");
            events.push_back(event.clone());
            while events.len() > KEEP_EVENTS {
                events.pop_front();
            }
        }
        eprintln!(
            "tt-station-agentd: event: {} -- {}",
            event.title, event.body
        );
        event
    }

    pub fn since(&self, seq: u64) -> Vec<Event> {
        self.events
            .lock()
            .expect("notify mutex poisoned")
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }

    pub fn latest_seq(&self) -> u64 {
        *self.seq.lock().expect("notify mutex poisoned")
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let saved = Saved {
            devices: self.devices(),
        };
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&saved).map_err(std::io::Error::other)?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            eprintln!("tt-station-agentd: couldn't save notification devices: {e}");
        }
    }
}

/// What a push says about `event`: generic words for its kind, never its own
/// title or body. A push passes through Apple and lands on every registered
/// phone's lock screen, so a report's question, a transcript's title, the
/// command a session wants to run, a model's error -- and a pairing code,
/// which lets whoever types it control the box -- stay on the box. The app's
/// notification service extension fetches the event by id over its own
/// authenticated connection and shows the real text; these words are what
/// shows when it can't (the phone is away from the box, say).
pub fn push_alert(event: &Event) -> (&'static str, &'static str) {
    if event
        .data
        .as_ref()
        .is_some_and(|d| d.get("approval").is_some())
    {
        let who = match event.kind.as_str() {
            "coding" => "Code needs your OK",
            "routine" => "A routine needs your OK",
            "bringup" => "A bring-up needs your OK",
            _ => "TT Station needs your OK",
        };
        return (who, "Open TT Station to see what it's asking.");
    }
    match event.kind.as_str() {
        "pairing" => (
            "A computer is asking to pair",
            "Open TT Station to see the code. If you didn't start pairing, ignore this.",
        ),
        "research" => ("Your research has news", "Open TT Station to read it."),
        "job_done" => (
            "A meeting transcript is ready",
            "Open TT Station to read it.",
        ),
        "coding" => ("A coding session finished", "Open TT Station to review it."),
        "routine" => ("A routine has news", "Open TT Station to see how it went."),
        "bringup" => ("A model bring-up has news", "Open TT Station to see it."),
        "orchard" | "orchard_done" => ("Orchard has news", "Open TT Station to see it."),
        "model_ready" => ("A model is ready", "Loaded and ready in TT Station."),
        "model_failed" => ("A model couldn't start", "Open TT Station for details."),
        "model" => ("A model needs a look", "Open TT Station for details."),
        "download_done" => ("A download finished", "Ready to start in TT Station."),
        "download_failed" => ("A download failed", "Open TT Station for details."),
        "chips_hot" => (
            "The chips are running hot",
            "Check the box's airflow; it will slow down to protect itself.",
        ),
        "box_online" => ("Your box is online", "TT Station can reach it."),
        "power" => ("Your box's power changed", "Open TT Station for details."),
        "devices" => (
            "The devices that can use your box changed",
            "Check them in TT Station: Settings, Devices.",
        ),
        "schedule" => ("A schedule ran", "Open TT Station for details."),
        "doctor" => (
            "A box check found something",
            "Open TT Station for details.",
        ),
        "test" => (
            "Notifications are working",
            "You'll hear when downloads finish and models are ready.",
        ),
        _ => ("TT Station", "Open TT Station for details."),
    }
}

/// The whole push for `event`: [`push_alert`]'s words, the box's name as the
/// subtitle, and the event's id (`event`, its `seq`) -- nothing else.
/// `mutable-content` lets the app's notification service extension replace
/// the words with the event's own, fetched from the box.
pub fn push_payload(event: &Event, box_name: &str) -> Value {
    let (title, body) = push_alert(event);
    let approval = event
        .data
        .as_ref()
        .is_some_and(|d| d.get("approval").is_some());
    let urgent = approval || event.kind.contains("failed") || event.kind == "chips_hot";
    json!({
        "aps": {
            "alert": { "title": title, "subtitle": box_name, "body": body },
            "sound": "default",
            "interruption-level": if urgent { "time-sensitive" } else { "active" },
            "thread-id": box_name,
            "mutable-content": 1,
        },
        "event": event.seq,
    })
}

/// The current snapshot (blocking: reads the tt catalog's pull jobs and the
/// served model's kind).
fn snapshot_blocking(state: &AppState) -> Snapshot {
    let life = state.lifecycle();
    let kind = state.served_kind_blocking().to_string();
    let main = Some((life.phase, life.model.or(life.served_id), life.error, kind));
    let companion = state.companion().map(|c| {
        let l = c.lifecycle();
        (l.phase, l.model.or(l.served_id), l.error)
    });
    let pulls = state
        .tt_cli()
        .map(|tt| {
            tt.pulls()
                .into_iter()
                .map(|p| {
                    let s = serde_json::to_value(p.state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default();
                    (p.model, (s, p.error))
                })
                .collect()
        })
        .unwrap_or_default();
    let hottest_c = state.history().latest().and_then(|p| {
        p.chips
            .iter()
            .filter_map(|c| c.temp_c)
            .fold(None, |m: Option<f32>, t| Some(m.map_or(t, |m| m.max(t))))
    });
    Snapshot {
        main,
        companion,
        pulls,
        hottest_c,
    }
}

/// Watch the box forever, emitting events on changes. Announces the box coming
/// online first (useful after a power-on).
pub async fn watch_forever(state: AppState) {
    state.notifier().emit(Draft {
        kind: "box_online",
        title: format!("{} is online", state.name()),
        body: "The box is up and TT Station can reach it.".to_string(),
    });
    let mut hot_armed = true;
    let probe = state.clone();
    let mut prev = tokio::task::spawn_blocking(move || snapshot_blocking(&probe))
        .await
        .unwrap_or_default();
    loop {
        tokio::time::sleep(WATCH_INTERVAL).await;
        let probe = state.clone();
        let Ok(next) = tokio::task::spawn_blocking(move || snapshot_blocking(&probe)).await else {
            continue;
        };
        for draft in diff(&prev, &next, &mut hot_armed) {
            state.notifier().emit(draft);
        }
        prev = next;
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

/// `GET /notify` (authed): whether this box can push, and to how many phones.
pub(crate) async fn get_notify(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let n = state.notifier();
    let (available, problem) = n.push_status();
    Json(json!({
        "push": {
            "available": available,
            "problem": problem,
            "devices": n.devices().iter().map(|d| json!({ "name": d.name, "added": d.added, "token_hint": &d.token[..8.min(d.token.len())] })).collect::<Vec<_>>(),
        },
        "latest_event": n.latest_seq(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct RegisterDevice {
    pub token: String,
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub name: String,
}

/// `POST /notify/devices` (authed) `{token, sandbox, name}`: this phone wants
/// push notifications.
pub(crate) async fn register_device(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<RegisterDevice>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let n = state.notifier();
    n.register(&req.token, req.sandbox, &req.name)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error: e })))?;
    let (available, problem) = n.push_status();
    Ok(Json(
        json!({ "registered": true, "push_available": available, "problem": problem }),
    ))
}

/// `DELETE /notify/devices/{token}` (authed).
pub(crate) async fn unregister_device(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> StatusCode {
    if state.notifier().unregister(&token) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `POST /notify/test` (authed): push a test notification now; answers how
/// many phones got it.
pub(crate) async fn test_notify(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let n = state.notifier();
    let event = n.record(Draft {
        kind: "test",
        title: format!("Hello from {}", state.name()),
        body: "Notifications are working: you'll hear when downloads finish and models are ready."
            .to_string(),
    });
    let (available, problem) = n.push_status();
    if !available {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: problem.unwrap_or_default(),
            }),
        ));
    }
    let sent = n.push(&event).await;
    if sent == 0 {
        return Err((
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse {
                error: "Apple didn't accept the notification for any registered phone.".to_string(),
            }),
        ));
    }
    Ok(Json(json!({ "sent": sent })))
}

#[derive(Debug, Deserialize)]
pub struct Since {
    #[serde(default)]
    pub since: u64,
}

/// `GET /events?since=N` (authed): events after `N`, for the app's own alerts.
pub(crate) async fn get_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Query(q): axum::extract::Query<Since>,
) -> Json<Value> {
    let n = state.notifier();
    Json(json!({ "events": n.since(q.since), "latest": n.latest_seq() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(main: Phase, model: &str) -> Snapshot {
        Snapshot {
            main: Some((main, Some(model.to_string()), None, "chat".to_string())),
            ..Default::default()
        }
    }

    #[test]
    fn model_ids_read_like_names() {
        assert_eq!(display_model("Qwen/Qwen3-32B"), "Qwen3-32B");
        assert_eq!(
            display_model("/home/container_app_user/readonly_weights_mount/Wan2.2-T2V-A14B-Diffusers/snapshots/5be7df96"),
            "Wan2.2-T2V-A14B-Diffusers"
        );
        assert_eq!(
            display_model("/cache/hub/models--black-forest-labs--FLUX.1-dev/snapshots/abc/"),
            "FLUX.1-dev"
        );
        assert_eq!(display_model("tt-flux.1-schnell"), "tt-flux.1-schnell");
    }

    #[test]
    fn a_finished_start_is_announced_once() {
        let mut armed = true;
        let starting = snap(Phase::Starting, "Qwen/Qwen3-32B");
        let serving = snap(Phase::Serving, "Qwen/Qwen3-32B");
        let events = diff(&starting, &serving, &mut armed);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "model_ready");
        assert_eq!(events[0].title, "Qwen3-32B is ready");
        assert!(diff(&serving, &serving, &mut armed).is_empty());
        // An adopted serve (idle -> serving) isn't news.
        assert!(diff(&snap(Phase::Idle, "x"), &serving, &mut armed).is_empty());
    }

    #[test]
    fn failures_downloads_and_heat_are_announced() {
        let mut armed = true;
        let mut failed = snap(Phase::Failed, "FLUX.1-schnell");
        failed.main.as_mut().unwrap().2 = Some("run.py failed: Traceback...\nmore".to_string());
        let e = diff(
            &snap(Phase::Starting, "FLUX.1-schnell"),
            &failed,
            &mut armed,
        );
        assert_eq!(e[0].kind, "model_failed");
        assert_eq!(e[0].body, "run.py failed: Traceback...");

        let mut a = Snapshot::default();
        a.pulls
            .insert("FLUX.1-dev".into(), ("running".into(), None));
        let mut b = Snapshot::default();
        b.pulls.insert("FLUX.1-dev".into(), ("done".into(), None));
        let e = diff(&a, &b, &mut armed);
        assert_eq!(e[0].title, "FLUX.1-dev downloaded");
        b.pulls
            .insert("FLUX.1-dev".into(), ("failed".into(), Some("gated".into())));
        assert_eq!(diff(&a, &b, &mut armed)[0].kind, "download_failed");

        let hot = Snapshot {
            hottest_c: Some(92.0),
            ..Default::default()
        };
        assert_eq!(
            diff(&Snapshot::default(), &hot, &mut armed)[0].kind,
            "chips_hot"
        );
        assert!(
            diff(&hot, &hot, &mut armed).is_empty(),
            "once until it cools"
        );
        let cool = Snapshot {
            hottest_c: Some(70.0),
            ..Default::default()
        };
        assert!(diff(&hot, &cool, &mut armed).is_empty());
        assert_eq!(diff(&cool, &hot, &mut armed).len(), 1, "re-armed");
    }

    #[test]
    fn companion_readiness_is_announced() {
        let mut armed = true;
        let mut a = snap(Phase::Serving, "Llama");
        a.companion = Some((Phase::Starting, Some("FLUX.1-schnell".into()), None));
        let mut b = a.clone();
        b.companion = Some((Phase::Serving, Some("FLUX.1-schnell".into()), None));
        let e = diff(&a, &b, &mut armed);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].title, "FLUX.1-schnell is ready");
        assert!(e[0].body.contains("beside chat"));
    }

    #[test]
    fn a_push_carries_no_content_only_the_event_id() {
        let secret = "Q3 layoffs plan for Dana";
        let kinds = [
            "research",
            "job_done",
            "coding",
            "routine",
            "bringup",
            "orchard",
            "orchard_done",
            "model_ready",
            "model_failed",
            "model",
            "download_done",
            "download_failed",
            "chips_hot",
            "box_online",
            "power",
            "devices",
            "pairing",
            "schedule",
            "doctor",
            "test",
            "something_new",
        ];
        for (i, kind) in kinds.iter().enumerate() {
            for data in [
                None,
                Some(
                    json!({ "approval": { "id": "ap-1", "kind": "command", "summary": secret, "can_always": true } }),
                ),
            ] {
                let event = Event {
                    seq: 1_000 + i as u64,
                    at: 1,
                    kind: kind.to_string(),
                    title: format!("Report ready: {secret}"),
                    body: format!("rm -rf {secret}; 123 456"),
                    data,
                };
                let p = push_payload(&event, "qb2-lab");
                let text = p.to_string();
                assert!(
                    !text.contains("Dana")
                        && !text.contains("rm -rf")
                        && !text.contains("123 456")
                        && !text.contains("ap-1"),
                    "{kind}: {text}"
                );
                assert_eq!(p["event"], event.seq, "{kind}");
                assert_eq!(p["aps"]["mutable-content"], 1, "{kind}");
                assert_eq!(p["aps"]["alert"]["subtitle"], "qb2-lab");
                let mut keys: Vec<&String> = p.as_object().unwrap().keys().collect();
                keys.sort();
                assert_eq!(keys, ["aps", "event"], "{kind}");
                assert!(
                    p["aps"].get("category").is_none(),
                    "the extension sets the approval buttons once it has the approval"
                );
                assert!(!p["aps"]["alert"]["title"].as_str().unwrap().is_empty());
            }
        }
        let ask = Event {
            seq: 7,
            at: 1,
            kind: "coding".into(),
            title: String::new(),
            body: String::new(),
            data: Some(json!({ "approval": {} })),
        };
        assert_eq!(push_alert(&ask).0, "Code needs your OK");
        assert_eq!(
            push_payload(&ask, "")["aps"]["interruption-level"],
            "time-sensitive"
        );
        let done = Event {
            kind: "job_done".into(),
            data: None,
            ..ask.clone()
        };
        assert_eq!(push_alert(&done).0, "A meeting transcript is ready");
        assert_eq!(
            push_payload(&done, "")["aps"]["interruption-level"],
            "active"
        );
    }

    #[test]
    fn devices_register_dedupe_validate_and_persist() {
        let dir = std::env::temp_dir().join(format!("tt-notify-{}", std::process::id()));
        let n = Notifier::open(dir.join("notify.json"));
        let token = "a1".repeat(32);
        n.register(&token, true, "Jesse's iPhone").unwrap();
        n.register(&token.to_uppercase(), false, "Jesse's iPhone")
            .unwrap();
        assert_eq!(n.devices().len(), 1, "same token, refreshed");
        assert!(!n.devices()[0].sandbox);
        assert!(n.register("not-a-token", true, "x").is_err());
        assert_eq!(Notifier::open(dir.join("notify.json")).devices().len(), 1);
        assert!(n.unregister(&token));
        assert!(!n.unregister(&token));
        let (available, problem) = n.push_status();
        assert!(!available && problem.unwrap().contains("apns.toml"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
