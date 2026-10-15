// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Telling the owner when something they're waiting for happens -- a download
//! finished, a model is loaded and ready, a start failed, the box came online,
//! the chips are running hot -- as ordinary iPhone notifications, app closed.
//!
//! A watcher compares the box's state every few seconds ([`diff`]) and turns
//! changes into [`Event`]s. Each is kept for clients (`GET /events?since=`) and
//! pushed through Apple's push service to the phones that registered their
//! device token (`POST /notify/devices`) -- see `crate::apns`. Pushes carry
//! generic words per kind of event and the event's id, never what happened
//! (see [`push_payload`]); the app fetches the event itself.
//!
//! Each event has an [`Audience`]: the household (the box's own news -- a
//! model, a download, heat, power -- and a recording, which concerns everyone
//! in the room), the owners (pairing, devices, approvals, and work nobody in
//! particular asked for), or one person (their research report). The same
//! rule decides both whose phones are pushed ([`Notifier::recipients`]) and
//! who sees the event in `GET /events` ([`Notifier::since_for`]): a member
//! sees the household's events and their own, never someone else's.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, CallerAuth, ErrorResponse, Phase};

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
    /// Who it's for (see the module docs).
    #[serde(skip)]
    pub audience: Audience,
    /// The device (its pairing token's digest) not to push it to: the one
    /// that caused it, which already knows.
    #[serde(skip)]
    pub quiet_for: Option<String>,
}

/// Who an event is for: whose phones are pushed, and who sees it in `GET /events`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Audience {
    /// Everyone who uses the box.
    #[default]
    Household,
    /// Owners only.
    Owners,
    /// One person, by id (`crate::people`).
    Person(String),
    /// One device, by its pairing token's digest (a test notification).
    Device(String),
}

/// Someone looking at events, or a phone that might be pushed one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    pub owner: bool,
    /// Their person's id, if the device is someone's.
    pub person: Option<String>,
    /// Their pairing token's digest; `None` for the box itself.
    pub device: Option<String>,
}

impl Audience {
    /// The audience for an event of `kind` that names no one in particular.
    /// What changes who can reach the box, what asks for an OK, and work
    /// whose person isn't known go to owners; the box's own state, and a
    /// recording (the room's business), to everyone. Kinds this doesn't know
    /// go to owners: a new kind of news is private until someone says it isn't.
    pub fn for_kind(kind: &str, data: Option<&Value>) -> Audience {
        if data.is_some_and(|d| d.get("approval").is_some()) {
            return Audience::Owners;
        }
        match kind {
            "model_ready" | "model_failed" | "model" | "download_done" | "download_failed"
            | "chips_hot" | "box_online" | "power" | "schedule" | "recording"
            | "recording_stopped" => Audience::Household,
            _ => Audience::Owners,
        }
    }

    /// One person's, or the owners' when nobody in particular asked.
    pub fn person_or_owners(person: Option<String>) -> Audience {
        person.map_or(Audience::Owners, Audience::Person)
    }

    pub fn shows_to(&self, v: &Viewer) -> bool {
        match self {
            Audience::Household => true,
            Audience::Owners => v.owner,
            Audience::Person(id) => v.person.as_deref() == Some(id.as_str()),
            Audience::Device(d) => v.device.as_deref() == Some(d.as_str()),
        }
    }
}

/// Who's making the current request (from `crate::audit::ACTOR` and
/// `crate::people::CALLER`), for events about something they started.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Requester {
    pub actor: crate::audit::Actor,
    /// Their device's pairing token digest; `None` for the box itself.
    pub device: Option<String>,
    pub person: Option<crate::people::Person>,
}

impl Requester {
    pub fn current(state: &AppState) -> Requester {
        let actor = crate::audit::ACTOR
            .try_with(Clone::clone)
            .unwrap_or_else(|_| crate::audit::Actor::the_box());
        let device = actor
            .device_id
            .as_deref()
            .and_then(|id| state.sessions().get(id))
            .map(|s| s.token_sha256);
        let person = crate::people::current().or_else(|| {
            device
                .as_deref()
                .and_then(|d| state.people().person_of_digest(d))
        });
        Requester {
            actor,
            device,
            person,
        }
    }

    /// How a person reads who it was: their name, else their device's.
    pub fn label(&self) -> String {
        match &self.person {
            Some(p) => p.name.clone(),
            None => self.actor.device_name.clone(),
        }
    }
}

/// Where the notifier looks up whose a device is.
#[derive(Clone)]
struct Directory {
    sessions: std::sync::Arc<crate::sessions::Sessions>,
    people: std::sync::Arc<crate::people::People>,
}

impl Directory {
    /// The viewer behind a live device; `None` once it's signed out or expired.
    fn viewer(&self, digest: &str) -> Option<Viewer> {
        let session = self
            .sessions
            .list()
            .into_iter()
            .find(|s| s.token_sha256 == digest)?;
        Some(Viewer {
            owner: session.owner,
            person: self.people.person_of_digest(digest).map(|p| p.id),
            device: Some(digest.to_string()),
        })
    }
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
    /// The digest of the pairing token it registered with: whose phone it is,
    /// and whether it's still paired. `None` for a registration from before
    /// pushes were routed; the app registers again when it opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
    directory: Mutex<Option<Directory>>,
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
            directory: Mutex::new(None),
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
            directory: Mutex::new(None),
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

    /// Where to look up whose each device is (`AppState` wires this up).
    pub fn set_directory(
        &self,
        sessions: std::sync::Arc<crate::sessions::Sessions>,
        people: std::sync::Arc<crate::people::People>,
    ) {
        *self.directory.lock().expect("notify mutex poisoned") =
            Some(Directory { sessions, people });
    }

    /// Who the device with this pairing token digest is, while it's paired.
    pub fn viewer(&self, digest: &str) -> Option<Viewer> {
        let dir = self
            .directory
            .lock()
            .expect("notify mutex poisoned")
            .clone();
        dir?.viewer(digest)
    }

    pub fn devices(&self) -> Vec<Device> {
        self.devices.lock().expect("notify mutex poisoned").clone()
    }

    /// The phones `event` is pushed to: those whose paired device it's for
    /// ([`Audience::shows_to`]), minus the one that caused it. A phone whose
    /// device was signed out gets nothing; one registered before pushes were
    /// routed (no device on record) gets only the household's news.
    pub fn recipients(&self, event: &Event) -> Vec<Device> {
        self.devices()
            .into_iter()
            .filter(|d| match d.session.as_deref() {
                Some(digest) => {
                    event.quiet_for.as_deref() != Some(digest)
                        && self
                            .viewer(digest)
                            .is_some_and(|v| event.audience.shows_to(&v))
                }
                None => event.audience == Audience::Household,
            })
            .collect()
    }

    /// Register (or refresh) a phone's device token, for the paired device
    /// whose token has the digest `session`.
    pub fn register(
        &self,
        token: &str,
        sandbox: bool,
        name: &str,
        session: Option<&str>,
    ) -> Result<(), String> {
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
            session: session.map(str::to_string),
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

    /// Record an event and push it (in the background) to the phones of
    /// whoever its kind is for ([`Audience::for_kind`]).
    pub fn emit(self: &std::sync::Arc<Self>, draft: Draft) -> Event {
        self.emit_with(draft, None)
    }

    /// [`Self::emit`], with what a client needs to act on it.
    pub fn emit_with(
        self: &std::sync::Arc<Self>,
        draft: Draft,
        data: Option<serde_json::Value>,
    ) -> Event {
        let audience = Audience::for_kind(draft.kind, data.as_ref());
        self.emit_to(draft, data, audience, None)
    }

    /// [`Self::emit_with`], for `audience`, not pushed to the device whose
    /// token digest is `quiet_for`.
    pub fn emit_to(
        self: &std::sync::Arc<Self>,
        draft: Draft,
        data: Option<serde_json::Value>,
        audience: Audience,
        quiet_for: Option<String>,
    ) -> Event {
        let event = self.record_to(draft, data, audience, quiet_for);
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

    /// Push `event` to its [`Self::recipients`]; returns how many got it.
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
        for device in self.recipients(event) {
            let mut sandbox = device.sandbox;
            let mut outcome = apns.send(&device.token, sandbox, &payload).await;
            if outcome == crate::apns::Delivery::WrongEnvironment {
                sandbox = !sandbox;
                outcome = apns.send(&device.token, sandbox, &payload).await;
                if outcome == crate::apns::Delivery::Sent {
                    let _ = self.register(
                        &device.token,
                        sandbox,
                        &device.name,
                        device.session.as_deref(),
                    );
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
        let audience = Audience::for_kind(draft.kind, None);
        self.record_to(draft, None, audience, None)
    }

    /// Record an event for `audience` without pushing it.
    pub fn record_to(
        &self,
        draft: Draft,
        data: Option<serde_json::Value>,
        audience: Audience,
        quiet_for: Option<String>,
    ) -> Event {
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
                audience,
                quiet_for,
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

    /// Every event after `seq`, whoever it's for (the box's own checks).
    pub fn since(&self, seq: u64) -> Vec<Event> {
        self.events
            .lock()
            .expect("notify mutex poisoned")
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }

    /// The events after `seq` that `viewer` may see: the same rule as pushes.
    pub fn since_for(&self, seq: u64, viewer: &Viewer) -> Vec<Event> {
        self.events
            .lock()
            .expect("notify mutex poisoned")
            .iter()
            .filter(|e| e.seq > seq && e.audience.shows_to(viewer))
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
        "recording" => (
            "The box started recording",
            "Everything said near it is being recorded. Open TT Station to see who started it, or to stop it.",
        ),
        "recording_stopped" => ("The box stopped recording", "Open TT Station for details."),
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
/// push notifications -- the ones for whoever the paired device it's asking
/// with is (see [`Audience`]).
pub(crate) async fn register_device(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<RegisterDevice>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let n = state.notifier();
    let session = caller.device.map(|d| d.token_sha256);
    n.register(&req.token, req.sandbox, &req.name, session.as_deref())
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

/// `POST /notify/test` (authed): push a test notification now -- to the
/// asking phone, or from the box itself to the owners' -- and answer how many
/// phones got it.
pub(crate) async fn test_notify(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let n = state.notifier();
    let audience = match caller.device {
        Some(d) => Audience::Device(d.token_sha256),
        None => Audience::Owners,
    };
    let event = n.record_to(
        Draft {
            kind: "test",
            title: format!("Hello from {}", state.name()),
            body:
                "Notifications are working: you'll hear when downloads finish and models are ready."
                    .to_string(),
        },
        None,
        audience,
        None,
    );
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

/// Who the caller of a request is, as an event's [`Audience`] sees them.
pub(crate) fn viewer_of(state: &AppState, caller: &crate::routes::Caller) -> Viewer {
    match &caller.device {
        Some(d) => Viewer {
            owner: d.owner,
            person: state
                .people()
                .person_of_digest(&d.token_sha256)
                .map(|p| p.id),
            device: Some(d.token_sha256.clone()),
        },
        // The box's own user: an owner (see `routes::Caller::owner`), nobody in particular.
        None => Viewer {
            owner: caller.owner(),
            person: None,
            device: None,
        },
    }
}

/// `GET /events?since=N` (authed): events after `N` that are for the caller,
/// for the app's own alerts.
pub(crate) async fn get_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    axum::extract::Query(q): axum::extract::Query<Since>,
) -> Json<Value> {
    let n = state.notifier();
    let viewer = viewer_of(&state, &caller);
    Json(json!({ "events": n.since_for(q.since, &viewer), "latest": n.latest_seq() }))
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
            "recording",
            "recording_stopped",
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
                    audience: Audience::Owners,
                    quiet_for: Some("digest".into()),
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
            audience: Audience::Owners,
            quiet_for: None,
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
        n.register(&token, true, "Jesse's iPhone", None).unwrap();
        n.register(&token.to_uppercase(), false, "Jesse's iPhone", Some("d1"))
            .unwrap();
        assert_eq!(n.devices().len(), 1, "same token, refreshed");
        assert!(!n.devices()[0].sandbox);
        assert_eq!(n.devices()[0].session.as_deref(), Some("d1"));
        assert!(n.register("not-a-token", true, "x", None).is_err());
        assert_eq!(Notifier::open(dir.join("notify.json")).devices().len(), 1);
        assert!(n.unregister(&token));
        assert!(!n.unregister(&token));
        let (available, problem) = n.push_status();
        assert!(!available && problem.unwrap().contains("apns.toml"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A box with an owner (Olive, two phones), a member (Max), a member who
    /// is nobody in particular, a signed-out phone, and a phone registered
    /// before pushes were routed.
    struct Household {
        n: std::sync::Arc<Notifier>,
        olive: String,
        max: String,
    }

    const OLIVE_PHONE: &str = "olive-phone";
    const OLIVE_MAC: &str = "olive-mac";
    const MAX_PHONE: &str = "max-phone";
    const GUEST_PHONE: &str = "guest-phone";
    const GONE_PHONE: &str = "gone-phone";

    fn household() -> Household {
        use crate::sessions::{token_digest, unix_now, Session, Sessions};
        let sessions = Sessions::in_memory();
        for (token, owner) in [
            (OLIVE_PHONE, true),
            (OLIVE_MAC, true),
            (MAX_PHONE, false),
            (GUEST_PHONE, false),
        ] {
            sessions.insert_for_test(Session {
                id: token.to_string(),
                token_sha256: token_digest(token),
                name: token.to_string(),
                platform: None,
                created: Some(unix_now()),
                last_used: unix_now(),
                owner,
            });
        }
        let people = crate::people::People::in_memory();
        let olive = people.add("Olive").unwrap().id;
        let max = people.add("Max").unwrap().id;
        people.assign(OLIVE_PHONE, Some(&olive)).unwrap();
        people.assign(OLIVE_MAC, Some(&olive)).unwrap();
        people.assign(MAX_PHONE, Some(&max)).unwrap();
        // Max's old phone: still Max's in the book, but signed out.
        people.assign(GONE_PHONE, Some(&max)).unwrap();
        let n = Notifier::in_memory();
        n.set_directory(std::sync::Arc::new(sessions), std::sync::Arc::new(people));
        for (i, token) in [OLIVE_PHONE, OLIVE_MAC, MAX_PHONE, GUEST_PHONE, GONE_PHONE]
            .iter()
            .enumerate()
        {
            let apns = format!("{i}").repeat(64);
            n.register(&apns, false, token, Some(&token_digest(token)))
                .unwrap();
        }
        n.register(&"f".repeat(64), false, "legacy", None).unwrap();
        Household {
            n: std::sync::Arc::new(n),
            olive,
            max,
        }
    }

    fn draft(kind: &'static str) -> Draft {
        Draft {
            kind,
            title: "t".into(),
            body: "b".into(),
        }
    }

    fn names(devices: Vec<Device>) -> Vec<String> {
        let mut v: Vec<String> = devices.into_iter().map(|d| d.name).collect();
        v.sort();
        v
    }

    fn viewer(h: &Household, token: &str) -> Viewer {
        h.n.viewer(&crate::sessions::token_digest(token)).unwrap()
    }

    #[test]
    fn kinds_have_the_audience_they_concern() {
        for kind in [
            "model_ready",
            "download_failed",
            "chips_hot",
            "power",
            "recording",
        ] {
            assert_eq!(
                Audience::for_kind(kind, None),
                Audience::Household,
                "{kind}"
            );
        }
        for kind in [
            "pairing", "devices", "research", "job_done", "coding", "routine", "doctor", "new_kind",
        ] {
            assert_eq!(Audience::for_kind(kind, None), Audience::Owners, "{kind}");
        }
        let ask = json!({ "approval": { "id": "x" } });
        assert_eq!(Audience::for_kind("model", Some(&ask)), Audience::Owners);
        assert_eq!(Audience::person_or_owners(None), Audience::Owners);
        assert_eq!(
            Audience::person_or_owners(Some("p".into())),
            Audience::Person("p".into())
        );
    }

    #[tokio::test]
    async fn pushes_go_to_the_people_they_concern() {
        let h = household();
        let n = &h.n;
        // The box's own news: every paired phone, and the old registration.
        let e = n.emit(draft("model_ready"));
        assert_eq!(
            names(n.recipients(&e)),
            [
                "guest-phone",
                "legacy",
                "max-phone",
                "olive-mac",
                "olive-phone"
            ]
        );
        // Pairing and approvals: owners only.
        let e = n.emit(draft("pairing"));
        assert_eq!(names(n.recipients(&e)), ["olive-mac", "olive-phone"]);
        let e = n.emit_with(draft("coding"), Some(json!({ "approval": {} })));
        assert_eq!(names(n.recipients(&e)), ["olive-mac", "olive-phone"]);
        // Max's report: Max's live phone only (not his signed-out one, not Olive's).
        let e = n.emit_to(
            draft("research"),
            None,
            Audience::Person(h.max.clone()),
            None,
        );
        assert_eq!(names(n.recipients(&e)), ["max-phone"]);
        // Nobody in particular asked: the owners.
        let e = n.emit_to(
            draft("research"),
            None,
            Audience::person_or_owners(None),
            None,
        );
        assert_eq!(names(n.recipients(&e)), ["olive-mac", "olive-phone"]);
        // A recording: everyone but the phone that started it.
        let e = n.emit_to(
            draft("recording"),
            None,
            Audience::Household,
            Some(crate::sessions::token_digest(MAX_PHONE)),
        );
        assert_eq!(
            names(n.recipients(&e)),
            ["guest-phone", "legacy", "olive-mac", "olive-phone"]
        );
        // A test: just the phone that asked.
        let e = n.record_to(
            draft("test"),
            None,
            Audience::Device(crate::sessions::token_digest(GUEST_PHONE)),
            None,
        );
        assert_eq!(names(n.recipients(&e)), ["guest-phone"]);
    }

    #[test]
    fn events_show_the_household_and_your_own_not_other_peoples() {
        let h = household();
        let n = &h.n;
        n.record(draft("model_ready"));
        n.record(draft("pairing"));
        n.record_to(
            draft("research"),
            None,
            Audience::Person(h.max.clone()),
            None,
        );
        n.record_to(
            draft("research"),
            None,
            Audience::Person(h.olive.clone()),
            None,
        );
        n.record_to(
            draft("recording"),
            None,
            Audience::Household,
            Some(crate::sessions::token_digest(MAX_PHONE)),
        );
        let seen = |v: &Viewer| {
            n.since_for(0, v)
                .into_iter()
                .map(|e| match e.audience {
                    Audience::Person(p) if p == h.max => "max's".to_string(),
                    Audience::Person(_) => "olive's".to_string(),
                    _ => e.kind,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            seen(&viewer(&h, MAX_PHONE)),
            ["model_ready", "max's", "recording"],
            "a member: the household's, and their own"
        );
        assert_eq!(
            seen(&viewer(&h, GUEST_PHONE)),
            ["model_ready", "recording"],
            "nobody in particular: the household's"
        );
        assert_eq!(
            seen(&viewer(&h, OLIVE_MAC)),
            ["model_ready", "pairing", "olive's", "recording"],
            "an owner: also the owners', but not another person's"
        );
        let the_box = Viewer {
            owner: true,
            person: None,
            device: None,
        };
        assert_eq!(seen(&the_box), ["model_ready", "pairing", "recording"]);
        // What a phone is pushed, it can fetch: the same rule.
        for e in n.since(0) {
            for d in n.recipients(&e) {
                if let Some(digest) = &d.session {
                    let v = n.viewer(digest).unwrap();
                    assert!(n.since_for(0, &v).contains(&e), "{} for {}", e.kind, d.name);
                }
            }
        }
        assert!(
            n.viewer(&crate::sessions::token_digest(GONE_PHONE))
                .is_none(),
            "a signed-out device is nobody"
        );
    }
}
