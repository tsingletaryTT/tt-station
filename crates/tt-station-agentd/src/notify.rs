// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Telling the owner when something they're waiting for happens -- a download
//! finished, a model is loaded and ready, a start failed, the box came online,
//! the chips are running hot.
//!
//! A watcher compares the box's state every few seconds ([`diff`]) and turns
//! changes into [`Event`]s. Each is kept for clients, which fetch them with
//! `GET /events?since=` and show them themselves (the desktop app's bell, the
//! phone's local notifications).
//!
//! Each event has an [`Audience`]: the household (the box's own news -- a
//! model, a download, heat, power), the owners (pairing, devices, approvals,
//! and work nobody in particular asked for), or one person (their research
//! report). It decides who sees the event in `GET /events`
//! ([`Notifier::since_for`]): a member sees the household's events and their
//! own, never someone else's.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, CallerAuth, Phase};

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
}

/// Who an event is for: who sees it in `GET /events`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Audience {
    /// Everyone who uses the box.
    #[default]
    Household,
    /// Owners only.
    Owners,
    /// One person, by id (`crate::people`).
    Person(String),
}

/// Someone looking at events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Viewer {
    pub owner: bool,
    /// Their person's id, if the device is someone's.
    pub person: Option<String>,
}

impl Audience {
    /// The audience for an event of `kind` that names no one in particular.
    /// What changes who can reach the box, what asks for an OK, and work
    /// whose person isn't known go to owners; the box's own state, to
    /// everyone. Kinds this doesn't know
    /// go to owners: a new kind of news is private until someone says it isn't.
    pub fn for_kind(kind: &str, data: Option<&Value>) -> Audience {
        if data.is_some_and(|d| d.get("approval").is_some()) {
            return Audience::Owners;
        }
        match kind {
            "model_ready" | "model_failed" | "model" | "download_done" | "download_failed"
            | "chips_hot" | "box_online" | "power" | "schedule" => Audience::Household,
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
        })
    }
}

/// What the watcher compares.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// Main slot: phase, model, error, kind.
    pub main: Option<(Phase, Option<String>, Option<String>, String)>,
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

pub struct Notifier {
    events: Mutex<VecDeque<Event>>,
    seq: Mutex<u64>,
    directory: Mutex<Option<Directory>>,
}

impl std::fmt::Debug for Notifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Notifier")
            .field("latest_seq", &self.latest_seq())
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
    /// Sequence numbers from zero (tests).
    pub fn in_memory() -> Self {
        Notifier {
            events: Mutex::new(VecDeque::new()),
            seq: Mutex::new(0),
            directory: Mutex::new(None),
        }
    }

    /// The agent's notifier. Sequence numbers start at the clock so a
    /// restarted agent's events sort after the previous run's for a client
    /// asking `since`.
    pub fn new() -> Self {
        Notifier {
            seq: Mutex::new(unix_now() * 1000),
            ..Notifier::in_memory()
        }
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

    /// Record an event for whoever its kind is for ([`Audience::for_kind`]).
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
        self.emit_to(draft, data, audience)
    }

    /// [`Self::emit_with`], for `audience`.
    pub fn emit_to(
        self: &std::sync::Arc<Self>,
        draft: Draft,
        data: Option<serde_json::Value>,
        audience: Audience,
    ) -> Event {
        self.record_to(draft, data, audience)
    }

    /// Record an event for whoever its kind is for.
    pub fn record(&self, draft: Draft) -> Event {
        let audience = Audience::for_kind(draft.kind, None);
        self.record_to(draft, None, audience)
    }

    /// Record an event for `audience`.
    pub fn record_to(
        &self,
        draft: Draft,
        data: Option<serde_json::Value>,
        audience: Audience,
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

    /// The events after `seq` that `viewer` may see.
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
}

impl Default for Notifier {
    fn default() -> Self {
        Notifier::new()
    }
}

/// The current snapshot (blocking: reads the tt catalog's pull jobs and the
/// served model's kind).
fn snapshot_blocking(state: &AppState) -> Snapshot {
    let life = state.lifecycle();
    let kind = state.served_kind_blocking().to_string();
    let main = Some((life.phase, life.model.or(life.served_id), life.error, kind));
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

/// `GET /notify` (authed): the newest event's sequence number, to start
/// polling `GET /events?since=` from.
pub(crate) async fn get_notify(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    Json(json!({ "latest_event": state.notifier().latest_seq() }))
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
        },
        // The box's own user: an owner (see `routes::Caller::owner`), nobody in particular.
        None => Viewer {
            owner: caller.owner(),
            person: None,
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

    /// A box with an owner (Olive, two devices), a member (Max), a member who
    /// is nobody in particular, and a signed-out phone.
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

    fn viewer(h: &Household, token: &str) -> Viewer {
        h.n.viewer(&crate::sessions::token_digest(token)).unwrap()
    }

    #[test]
    fn kinds_have_the_audience_they_concern() {
        for kind in ["model_ready", "download_failed", "chips_hot", "power"] {
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

    #[test]
    fn events_show_the_household_and_your_own_not_other_peoples() {
        let h = household();
        let n = &h.n;
        n.record(draft("model_ready"));
        n.record(draft("pairing"));
        n.record_to(draft("research"), None, Audience::Person(h.max.clone()));
        n.record_to(draft("research"), None, Audience::Person(h.olive.clone()));
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
            ["model_ready", "max's"],
            "a member: the household's, and their own"
        );
        assert_eq!(
            seen(&viewer(&h, GUEST_PHONE)),
            ["model_ready"],
            "nobody in particular: the household's"
        );
        assert_eq!(
            seen(&viewer(&h, OLIVE_MAC)),
            ["model_ready", "pairing", "olive's"],
            "an owner: also the owners', but not another person's"
        );
        let the_box = Viewer {
            owner: true,
            person: None,
        };
        assert_eq!(seen(&the_box), ["model_ready", "pairing"]);
        assert!(
            n.viewer(&crate::sessions::token_digest(GONE_PHONE))
                .is_none(),
            "a signed-out device is nobody"
        );
    }
}
