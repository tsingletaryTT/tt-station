// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Routines: jobs the box does on its own, written as a goal.
//!
//! A routine is a goal in a person's words ("every weekday at 8:30, check the
//! chips and the logs and tell me if anything needs attention"), when to run
//! it (by hand, daily, weekdays, every few hours), and which actions it may
//! take without asking. A run hands the goal to the served model with the
//! box's tools; the model works out the steps, the runner carries them out
//! and records each one, and the run ends with a short report.
//!
//! Looking things up (status, models, logs, the web, a question to the
//! served model) never needs approval. Changing the box (start or stop a
//! model, download one, reset the chips) asks the person first -- a
//! notification, answered in TT Station -- unless the routine was given
//! that action ahead of time, for runs nobody watches (overnight).
//!
//! Long operations are waited out by the runner, not the model: a download
//! or a model start can take an hour, and the model is told when it's done.
//! While models are switched the runner waits for one to serve again before
//! asking the model anything.
//!
//! Once text from the web (web_search, read_webpage) has come into a run it
//! may be what's steering the model, so -- as with Tori -- from then on every
//! acting step (the actions, even ones given ahead of time; `remember`;
//! `research`), and any read of an address the run didn't already know (see
//! `crate::urltrust`), is queued for the person instead of run: a
//! notification asks, the run carries on without it and its report says
//! what's waiting. Answered later, an allowed step runs then (`GET
//! /approvals`, `POST /approvals/answer` with `routine:<run>:<n>`), within
//! [`QUEUE_TTL`].
//!
//! One run at a time: runs share the chips.
//!
//! A routine can also just switch the box's mode at set times (`mode`): no
//! model thinks, the runner switches, and -- as schedules always did -- it
//! never interrupts a Lab run, an Orchard bring-up or a transcription job
//! (it skips that time and says so), and a switch more than ten minutes late
//! (the box was off) waits for the next time. These used to be Settings →
//! Schedules (`crate::schedules`); schedules found there are turned into
//! routines when routines load ([`absorb_schedules`]), keeping their ids,
//! days, times and on/off.
//!
//! - `GET /routines`, `POST /routines`, `PUT /routines/{id}`, `DELETE /routines/{id}`
//! - `POST /routines/{id}/run`: run it now.
//! - `GET /routines/{id}/runs`, `GET /routines/runs/{run}`
//! - `POST /routines/runs/{run}/approve` `{allow}`, `POST /routines/runs/{run}/stop`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth, Phase};

type Reply = Result<Json<Value>, ApiError>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Tool rounds in one run before it must report.
const MAX_ROUNDS: usize = 30;
/// Runs kept on disk.
const KEEP_RUNS: usize = 200;
/// How long a step queued for the person can still be allowed.
pub const QUEUE_TTL: u64 = 24 * 3600;

// ---------------------------------------------------------------- data

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    /// `manual` | `daily` | `weekdays` | `days` | `hourly`
    #[serde(default = "manual")]
    pub kind: String,
    /// `HH:MM`, local time (daily, weekdays, days).
    #[serde(default)]
    pub time: Option<String>,
    /// Hours between runs (hourly).
    #[serde(default)]
    pub every_hours: Option<u32>,
    /// Days of the week (`days`), 0 = Sunday … 6 = Saturday.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<Vec<u8>>,
}

fn manual() -> String {
    "manual".into()
}

/// Run by hand: what a routine (or request) with no `schedule` gets.
impl Default for Schedule {
    fn default() -> Self {
        Schedule {
            kind: manual(),
            time: None,
            every_hours: None,
            days: None,
        }
    }
}

/// What a routine may do without asking: the action tools it was given.
pub const ACTIONS: [&str; 4] = ["start_model", "stop_model", "download_model", "reset_chips"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Routine {
    pub id: String,
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub schedule: Schedule,
    /// Actions it may take without asking (a subset of [`ACTIONS`]).
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub created: u64,
    #[serde(default)]
    pub next_run: Option<u64>,
    #[serde(default)]
    pub last_run: Option<String>,
    /// Just switch the box to this mode (`crate::modes`) when it runs: no
    /// model plans anything (see the module doc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The paired device that last wrote it (created or edited it): its
    /// scheduled runs act on that person's behalf (`crate::model_control`).
    /// `None` for the box itself and routines from before this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub ts: u64,
    /// `tool` (a look-up), `action` (changes the box), `wait`, `note`
    pub kind: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: Value,
    pub result: String,
    pub ok: bool,
    /// How long it took, seconds.
    #[serde(default)]
    pub seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub tool: String,
    pub args: Value,
    pub summary: String,
    pub since: u64,
}

/// A step queued for the person after web text came into the run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Queued {
    pub n: usize,
    pub tool: String,
    pub args: Value,
    pub summary: String,
    pub since: u64,
    /// `waiting` | `allowed` | `declined` | `expired`
    pub status: String,
    /// What it did, once allowed.
    #[serde(default)]
    pub result: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub routine: String,
    pub name: String,
    /// `manual` or `schedule`
    pub trigger: String,
    pub started: u64,
    #[serde(default)]
    pub finished: Option<u64>,
    /// `running` | `waiting` (for the person) | `done` | `failed` | `stopped`
    pub status: String,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub report: Option<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
    /// What it's doing right now ("Waiting for the download", "Thinking").
    #[serde(default)]
    pub activity: Option<String>,
    /// Steps queued for the person (see the module doc).
    #[serde(default)]
    pub queued: Vec<Queued>,
    /// The paired device it runs on behalf of: whoever ran it by hand, or for
    /// a scheduled run whoever last wrote the routine; `None` for the box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

// ---------------------------------------------------------------- storage

/// What routines keep for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// The routines, loaded from disk on first use.
    routines: Mutex<Option<Vec<Routine>>>,
    /// Runs in progress, kept here and written through to disk.
    live: Mutex<HashMap<String, Run>>,
    /// Runs asked to stop.
    stop: Mutex<std::collections::HashSet<String>>,
    /// Runs waiting on the person's answer, and where to send it.
    decisions: Mutex<HashMap<String, tokio::sync::oneshot::Sender<bool>>>,
    /// The run going on now (one at a time).
    current: Mutex<Option<String>>,
    /// One answer at a time, so a step is never run twice.
    answering: Mutex<()>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().routines
}

fn dir(state: &AppState) -> PathBuf {
    state.config_dir().join("routines")
}

fn with_routines<T>(state: &AppState, f: impl FnOnce(&mut Vec<Routine>) -> T) -> T {
    let mut held = mine(state).routines.lock().expect("routines");
    let list = held.get_or_insert_with(|| {
        crate::fsio::read_json_or_backup(&dir(state).join("routines.json")).unwrap_or_default()
    });
    let before = serde_json::to_vec(&*list).unwrap_or_default();
    // Schedules (the old Settings → Schedules, an older phone app, an import)
    // become routines here.
    let incoming = state.schedules().take_all();
    if !incoming.is_empty() {
        absorb_into(list, incoming, now());
    }
    let out = f(list);
    if serde_json::to_vec(&*list).unwrap_or_default() != before {
        crate::fsio::save_json(
            &dir(state).join("routines.json"),
            &*list,
            crate::fsio::PRIVATE,
        );
    }
    out
}

/// Add or update routines from an export (`crate::export`), by id: returns
/// (added, updated). What each may do without asking is kept to [`ACTIONS`],
/// and its next run is worked out again here.
pub(crate) fn merge_imported(state: &AppState, incoming: Vec<Routine>) -> (usize, usize) {
    with_routines(state, |list| {
        let (mut added, mut updated) = (0, 0);
        for mut r in incoming {
            if !crate::export::safe_id(&r.id) {
                continue;
            }
            r.allowed.retain(|a| ACTIONS.contains(&a.as_str()));
            r.next_run = None;
            match list.iter_mut().find(|x| x.id == r.id) {
                Some(cur) => {
                    r.next_run = cur.next_run;
                    if serde_json::to_value(&*cur).ok() != serde_json::to_value(&r).ok() {
                        *cur = r;
                        updated += 1;
                    }
                }
                None => {
                    list.push(r);
                    added += 1;
                }
            }
        }
        (added, updated)
    })
}

/// The routines as they are now (for an export's preview).
pub(crate) fn list(state: &AppState) -> Vec<Routine> {
    with_routines(state, |l| l.clone())
}

/// A mode switch, in a person's words: "Switch to Images".
fn switch_name(mode: &str) -> String {
    format!(
        "Switch to {}",
        crate::modes::mode_title(mode).unwrap_or(mode)
    )
}

/// A schedule (`crate::schedules`) as the routine it becomes: same id, mode,
/// days, time and on/off. A schedule that already ran today has its next
/// run worked out from now, so it doesn't run twice in a day.
pub(crate) fn from_schedule(s: &crate::schedules::Schedule, at: u64) -> Routine {
    let mut days = s.days.clone();
    days.sort_unstable();
    days.dedup();
    let schedule = Schedule {
        kind: "days".into(),
        time: Some(s.time.clone()),
        every_hours: None,
        days: Some(days),
    };
    let title = crate::modes::mode_title(&s.mode).unwrap_or(&s.mode);
    Routine {
        id: s.id.clone(),
        name: switch_name(&s.mode),
        goal: format!("Switch the box to {title}."),
        next_run: if s.enabled {
            next_after(&schedule, at)
        } else {
            None
        },
        schedule,
        allowed: vec!["start_model".into()],
        enabled: s.enabled,
        created: at,
        last_run: None,
        mode: Some(s.mode.clone()),
        by: None,
    }
}

/// Adds schedules to `list` as routines; one whose id is already a routine
/// updates its mode, time, days and on/off.
fn absorb_into(list: &mut Vec<Routine>, incoming: Vec<crate::schedules::Schedule>, at: u64) {
    for s in incoming {
        let r = from_schedule(&s, at);
        match list.iter_mut().find(|x| x.id == r.id) {
            Some(cur) => {
                cur.name = r.name;
                cur.goal = r.goal;
                cur.mode = r.mode;
                cur.schedule = r.schedule;
                cur.enabled = r.enabled;
                cur.next_run = r.next_run;
            }
            None => list.push(r),
        }
    }
}

/// Turns any schedules waiting in `crate::schedules` into routines now.
pub(crate) fn absorb_schedules(state: &AppState) {
    with_routines(state, |_| ());
}

/// Mode-switching routines in the shape `GET /schedules` always had, for
/// phone apps that still ask for schedules.
pub(crate) fn as_schedules(state: &AppState) -> Vec<crate::schedules::Schedule> {
    let mut list = with_routines(state, |l| schedules_of(l));
    // Kept in time order, as `/schedules` always answered.
    list.sort_by(|a, b| a.time.cmp(&b.time));
    list
}

fn schedules_of(list: &[Routine]) -> Vec<crate::schedules::Schedule> {
    list.iter()
        .filter_map(|r| {
            let mode = r.mode.clone()?;
            let days = match r.schedule.kind.as_str() {
                "days" => r.schedule.days.clone().unwrap_or_default(),
                "weekdays" => vec![1, 2, 3, 4, 5],
                "daily" => (0..7).collect(),
                _ => return None,
            };
            Some(crate::schedules::Schedule {
                id: r.id.clone(),
                mode,
                time: r.schedule.time.clone().unwrap_or_default(),
                days,
                enabled: r.enabled,
                last_run: None,
            })
        })
        .collect()
}

/// `PATCH /schedules/{id}` for older phone apps: changes the routine it became.
pub(crate) fn patch_from_schedule(
    state: &AppState,
    id: &str,
    patch: &crate::schedules::Patch,
) -> Result<(), String> {
    let current = as_schedules(state)
        .into_iter()
        .find(|s| s.id == id)
        .ok_or("No such schedule.")?;
    let mut next = current.clone();
    if let Some(m) = &patch.mode {
        next.mode = m.clone();
    }
    if let Some(t) = &patch.time {
        next.time = t.clone();
    }
    if let Some(d) = &patch.days {
        next.days = d.clone();
    }
    if let Some(e) = patch.enabled {
        next.enabled = e;
    }
    crate::schedules::Schedules::check(&next.mode, &next.time, &next.days)?;
    let (h, m) = crate::schedules::parse_time(&next.time).unwrap_or_default();
    next.time = format!("{h:02}:{m:02}");
    with_routines(state, |l| absorb_into(l, vec![next], now()));
    Ok(())
}

/// `DELETE /schedules/{id}` for older phone apps.
pub(crate) fn remove_mode_routine(state: &AppState, id: &str) -> bool {
    with_routines(state, |l| {
        let before = l.len();
        l.retain(|r| !(r.id == id && r.mode.is_some()));
        l.len() != before
    })
}

/// `runs/<id>.json`, for a plain id only: run ids come back from disk (and
/// from imports), and a path built from anything else could point anywhere.
fn run_path(state: &AppState, id: &str) -> Option<PathBuf> {
    crate::export::safe_id(id).then(|| dir(state).join("runs").join(format!("{id}.json")))
}

fn save_run(state: &AppState, run: &Run) {
    if let Some(p) = run_path(state, &run.id) {
        crate::fsio::save_json(&p, run, crate::fsio::PRIVATE);
    }
    mine(state)
        .live
        .lock()
        .expect("routines")
        .insert(run.id.clone(), run.clone());
}

fn update_run(state: &AppState, id: &str, f: impl FnOnce(&mut Run)) -> Option<Run> {
    let mut run = mine(state)
        .live
        .lock()
        .expect("routines")
        .get(id)
        .cloned()
        .or_else(|| load_run(state, id))?;
    f(&mut run);
    save_run(state, &run);
    Some(run)
}

fn load_run(state: &AppState, id: &str) -> Option<Run> {
    if let Some(r) = mine(state).live.lock().expect("routines").get(id) {
        return Some(r.clone());
    }
    crate::fsio::read_json_or_backup(&run_path(state, id)?)
}

/// All runs on disk, newest first.
fn all_runs(state: &AppState) -> Vec<Run> {
    let mut runs: Vec<Run> = std::fs::read_dir(dir(state).join("runs"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .filter_map(|p| {
            let r = crate::fsio::read_json_or_backup::<Run>(&p)?;
            // Only a run whose id names its own file: that's the file a
            // prune or a delete removes.
            (run_path(state, &r.id).as_deref() == Some(p.as_path())).then_some(r)
        })
        .map(|r| {
            mine(state)
                .live
                .lock()
                .expect("routines")
                .get(&r.id)
                .cloned()
                .unwrap_or(r)
        })
        .collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started));
    runs
}

/// Oldest runs beyond what's kept.
fn prune_runs(state: &AppState) {
    for old in all_runs(state).iter().skip(KEEP_RUNS) {
        if let Some(p) = run_path(state, &old.id) {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

// ---------------------------------------------------------------- schedules

/// Local time pieces for a unix time.
fn local(t: u64) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let secs = t as libc::time_t;
    // SAFETY: localtime_r only writes into the provided struct.
    unsafe {
        libc::localtime_r(&secs, &mut tm);
    }
    tm
}

/// The unix time of a local date and time (normalizing overflow, e.g. day 32).
fn local_to_unix(mut tm: libc::tm) -> u64 {
    tm.tm_isdst = -1;
    // SAFETY: mktime reads and normalizes the struct we own.
    let t = unsafe { libc::mktime(&mut tm) };
    t.max(0) as u64
}

fn parse_hhmm(s: &str) -> Option<(i32, i32)> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m)).then_some((h, m))
}

/// When a schedule next runs after `after` (unix seconds), or None for manual.
pub fn next_after(s: &Schedule, after: u64) -> Option<u64> {
    match s.kind.as_str() {
        "hourly" => Some(after + u64::from(s.every_hours.unwrap_or(1).clamp(1, 168)) * 3600),
        "daily" | "weekdays" | "days" => {
            let (h, m) = parse_hhmm(s.time.as_deref().unwrap_or("08:00"))?;
            let base = local(after);
            for day in 0..8 {
                let mut tm = base;
                tm.tm_mday += day;
                tm.tm_hour = h;
                tm.tm_min = m;
                tm.tm_sec = 0;
                let t = local_to_unix(tm);
                if t <= after {
                    continue;
                }
                let wday = local(t).tm_wday;
                if s.kind == "weekdays" && (wday == 0 || wday == 6) {
                    continue;
                }
                if s.kind == "days"
                    && !s
                        .days
                        .as_deref()
                        .unwrap_or_default()
                        .contains(&(wday as u8))
                {
                    continue;
                }
                return Some(t);
            }
            None
        }
        _ => None,
    }
}

/// "Every weekday at 08:30", for lists and notifications.
pub fn describe(s: &Schedule) -> String {
    match s.kind.as_str() {
        "daily" => format!("Every day at {}", s.time.as_deref().unwrap_or("08:00")),
        "weekdays" => format!("Weekdays at {}", s.time.as_deref().unwrap_or("08:00")),
        "days" => {
            const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            let mut days = s.days.clone().unwrap_or_default();
            days.sort_unstable();
            days.dedup();
            let at = s.time.as_deref().unwrap_or("08:00");
            match days.as_slice() {
                [0, 1, 2, 3, 4, 5, 6] => format!("Every day at {at}"),
                [1, 2, 3, 4, 5] => format!("Weekdays at {at}"),
                [0, 6] => format!("Weekends at {at}"),
                d => {
                    let names: Vec<&str> = d
                        .iter()
                        .filter_map(|x| NAMES.get(*x as usize).copied())
                        .collect();
                    format!("{} at {at}", names.join(", "))
                }
            }
        }
        "hourly" => match s.every_hours.unwrap_or(1) {
            1 => "Every hour".into(),
            n => format!("Every {n} hours"),
        },
        _ => "When you run it".into(),
    }
}

// ---------------------------------------------------------------- the tools a run may use

/// Look-ups from the assistant that make sense unattended. (Not desktop
/// control: nobody is at the screen when a routine runs at 3 a.m.)
const BORROWED: [&str; 9] = [
    "box_status",
    "list_models",
    "recent_logs",
    "web_search",
    "read_webpage",
    "research",
    "research_check",
    "search_documents",
    "remember",
];

fn tools() -> Value {
    let mut out: Vec<Value> = crate::assistant::tool_definitions()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| {
            t["function"]["name"]
                .as_str()
                .is_some_and(|n| BORROWED.contains(&n))
        })
        .collect();
    let def = |name: &str, desc: &str, params: Value| json!({ "type": "function", "function": { "name": name, "description": desc, "parameters": params } });
    let none = json!({ "type": "object", "properties": {} });
    out.extend([
        def("community_models", "The community model packages on this box (with whether their weights are all here and whether they fit), downloads in progress, and which one is serving.", none.clone()),
        def("ask_model", "Send one prompt to the model serving right now and get its reply, with its speed (tokens per second) and time to first token. Use it to test a model.",
            json!({ "type": "object", "properties": { "prompt": { "type": "string" }, "max_tokens": { "type": "integer" } }, "required": ["prompt"] })),
        def("wait_until_ready", "Wait for the model that's starting to finish loading (or fail). Call it after start_model.",
            json!({ "type": "object", "properties": { "minutes": { "type": "integer", "description": "Give up after this long (default 45)." } } })),
        def("wait_for_download", "Wait for a community model's download to finish (or fail). Call it after download_model.",
            json!({ "type": "object", "properties": { "id": { "type": "string" }, "minutes": { "type": "integer", "description": "Give up after this long (default 240)." } }, "required": ["id"] })),
        def("start_model", "ACTION: start a model on the chips, stopping whatever serves now. `model` is a community package id (namespace/name, from community_models) or a model name from list_models. Then call wait_until_ready.",
            json!({ "type": "object", "properties": { "model": { "type": "string" } }, "required": ["model"] })),
        def("evaluate_model", "Run the full check-up on the model serving now (tool use, code, reasoning, following instructions, long documents, speed; about 5-10 minutes). Results are saved on the Models page, where models are compared.", none.clone()),
        def("stop_model", "ACTION: stop the model that's serving.", none.clone()),
        def("download_model", "ACTION: download a community model package with its weights (can be hundreds of GB). Then call wait_for_download.",
            json!({ "type": "object", "properties": { "id": { "type": "string", "description": "namespace/name" } }, "required": ["id"] })),
        def("reset_chips", "ACTION: reset the Tenstorrent chips (stops whatever is serving).", none),
    ]);
    json!(out)
}

fn is_action(tool: &str) -> bool {
    ACTIONS.contains(&tool)
}

/// Borrowed tools that act rather than look (see the module doc).
const ACTING: [&str; 2] = ["remember", "research"];

/// Whether a run that's read the web (`turn`) must queue `tool` for the person.
fn must_queue(tool: &str, args: &Value, turn: &crate::urltrust::Turn) -> bool {
    (turn.tainted() && (is_action(tool) || ACTING.contains(&tool)))
        || crate::assistant::fetches_unknown_address(tool, args, turn)
}

/// What the model is told about a step it asked for that waits for the person.
const QUEUED_NOTE: &str = "Not done: text from the web came into this run and may be what asked for this, so it's waiting for the person's OK. Carry on without it, and say in your report that it's waiting.";

/// What an action will do, in a person's words.
fn action_summary(tool: &str, args: &Value) -> String {
    match tool {
        "start_model" => format!("Start {}", args["model"].as_str().unwrap_or("a model")),
        "stop_model" => "Stop the model that's serving".into(),
        "download_model" => format!("Download {}", args["id"].as_str().unwrap_or("a model")),
        "reset_chips" => "Reset the chips".into(),
        "remember" | "research" => crate::assistant::pending_summary(tool, args, ""),
        "read_webpage" => format!("Read {}", args["url"].as_str().unwrap_or("a web page")),
        other => other.to_string(),
    }
}

/// Tools that start, stop or hold the served model (`crate::model_control`).
const MODEL_TOOLS: [&str; 3] = ["start_model", "stop_model", "evaluate_model"];

/// Why a run on behalf of the paired device `by` may not do `tool` without
/// someone who may saying yes (`None`: it may). Starting, stopping or
/// evaluating models follows `crate::model_control`; resetting the chips is
/// an owner's, as on `POST /power`.
fn refused(state: &AppState, by: Option<&str>, tool: &str) -> Option<String> {
    if MODEL_TOOLS.contains(&tool) && !crate::model_control::allows_device(state, by) {
        return Some(format!(
            "{} This routine runs for a member, so it can't.",
            crate::model_control::OWNERS_ONLY
        ));
    }
    let owner = by.is_none_or(|id| state.sessions().get(id).is_some_and(|s| s.owner));
    (tool == "reset_chips" && !owner).then(|| {
        "Only an owner of this box can reset the chips. This routine runs for a member, so it can't."
            .to_string()
    })
}

/// Whether the caller answering may say yes to `tool` (`403` when not):
/// the same rules as [`refused`], for the person approving.
fn may_approve(state: &AppState, tool: &str) -> Result<(), ApiError> {
    if MODEL_TOOLS.contains(&tool) {
        crate::model_control::check_now(state, &format!("approve routine {tool}"))?;
    }
    if tool == "reset_chips" && crate::auth::current_caller().is_some_and(|c| !c.owner()) {
        crate::audit::record(state, "denied", "approve routine reset_chips", "denied");
        return Err(err(StatusCode::FORBIDDEN, crate::routes::OWNER_ONLY));
    }
    Ok(())
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// Runs one look-up or action; (ok, what it found).
async fn run_tool(state: &AppState, run_id: &str, name: &str, args: &Value) -> (bool, String) {
    match name {
        "community_models" => {
            let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
            let installed: Vec<Value> = c["installed"].as_array().into_iter().flatten().map(|p| json!({
                "id": p["id"], "fits": p["fits"], "fit": p["fit"], "weights_on_box": p["weights_on_box"], "can_use_tools": p["tools"],
            })).collect();
            let pulls: Vec<Value> = c["pulls"].as_array().into_iter().flatten().map(|d| json!({
                "id": d["id"], "running": d["running"], "paused": d["paused"], "failed": d["failed"], "progress": d["progress"]["percent"],
            })).collect();
            (true, json!({ "installed": installed, "downloads": pulls, "serving": c["serving"], "starting": c["starting"]["id"] }).to_string())
        }
        "ask_model" => ask_model(state, args).await,
        "evaluate_model" => {
            set_activity(state, run_id, "Evaluating the model");
            match crate::evals::run(state, false).await {
                Ok(e) if e.status == "done" => (true, json!({ "summary": crate::evals::headline(&e), "overall_percent": e.overall.map(|o| (o * 100.0).round()), "speed": e.speed, "context": e.context }).to_string()),
                Ok(e) => (false, format!("The evaluation didn't finish: {}", e.error.unwrap_or_default())),
                Err(e) => (false, e),
            }
        }
        "wait_until_ready" => {
            let minutes = args["minutes"].as_u64().unwrap_or(45).clamp(1, 180);
            set_activity(state, run_id, "Waiting for the model to load");
            wait_until_ready(state, run_id, minutes).await
        }
        "wait_for_download" => {
            let id = args["id"].as_str().unwrap_or("").to_string();
            let minutes = args["minutes"].as_u64().unwrap_or(240).clamp(1, 24 * 60);
            set_activity(
                state,
                run_id,
                &format!("Waiting for {} to download", short(&id)),
            );
            wait_for_download(state, run_id, &id, minutes).await
        }
        "start_model" => start_model(state, args["model"].as_str().unwrap_or("")).await,
        "stop_model" => match state.stop_serving().await {
            Ok(()) => (true, "Stopped the model that was serving.".into()),
            Err(e) => (false, format!("Couldn't stop it: {e:#}")),
        },
        "download_model" => {
            let id = args["id"].as_str().unwrap_or("").to_string();
            let body = crate::community::IdBody {
                id: id.clone(),
                stop_serving: false,
                anyway: false,
                profile: None,
            };
            match crate::community::post_pull(State(state.clone()), BearerAuth, Json(body)).await {
                Ok(_) => (
                    true,
                    format!("Downloading {id}. Call wait_for_download next."),
                ),
                Err((_, Json(v))) => (
                    false,
                    v["error"]
                        .as_str()
                        .unwrap_or("couldn't start the download")
                        .to_string(),
                ),
            }
        }
        "reset_chips" => {
            let call = crate::assistant::ToolCall {
                id: String::new(),
                name: "reset_chips".into(),
                args: Ok(json!({})),
            };
            confirm_through_assistant(state, &call).await
        }
        other if BORROWED.contains(&other) => {
            let call = crate::assistant::ToolCall {
                id: String::new(),
                name: other.into(),
                args: Ok(args.clone()),
            };
            // As whom the run is for (a research report is theirs), else the box.
            let device = load_run(state, run_id)
                .and_then(|r| r.by)
                .and_then(|id| state.sessions().get(&id));
            let (result, _) = match device {
                Some(d) => {
                    let caller = crate::auth::Caller {
                        local: false,
                        device: Some(d),
                    };
                    crate::auth::as_caller(caller, crate::assistant::run_read_only(state, &call))
                        .await
                }
                None => crate::assistant::run_read_only(state, &call).await,
            };
            let ok = result.get("error").is_none();
            (ok, result.to_string())
        }
        other => (false, format!("There's no tool called {other}.")),
    }
}

fn short(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

/// Starts a community package by id, or a catalog model by name.
pub(crate) async fn start_model(state: &AppState, model: &str) -> (bool, String) {
    let model = model.trim();
    if model.is_empty() {
        return (false, "Say which model to start.".into());
    }
    let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
    let package = c["installed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p["id"].as_str())
        .find(|id| id.eq_ignore_ascii_case(model) || short(id).eq_ignore_ascii_case(model))
        .map(str::to_string);
    if let Some(id) = package {
        let body = crate::community::IdBody {
            id: id.clone(),
            stop_serving: true,
            anyway: false,
            profile: None,
        };
        return match crate::community::post_serve(State(state.clone()), BearerAuth, Json(body))
            .await
        {
            Ok(_) => (true, format!("Starting {id}. Call wait_until_ready next.")),
            Err((_, Json(v))) => (
                false,
                v["error"]
                    .as_str()
                    .unwrap_or("it wouldn't start")
                    .to_string(),
            ),
        };
    }
    let call = crate::assistant::ToolCall {
        id: String::new(),
        name: "run_model".into(),
        args: Ok(json!({ "model": model })),
    };
    confirm_through_assistant(state, &call).await
}

/// An assistant action, proposed and approved at once (the routine's own
/// approval already happened).
async fn confirm_through_assistant(
    state: &AppState,
    call: &crate::assistant::ToolCall,
) -> (bool, String) {
    let current = state.current_model().unwrap_or_default();
    match crate::assistant::propose(state, call, &current).await {
        Ok((pending, _)) => {
            let Some(action_id) = pending["action_id"].as_str().map(str::to_string) else {
                return (false, "It couldn't be set up.".into());
            };
            match crate::assistant::confirm(
                state,
                crate::assistant::ConfirmRequest {
                    action_id,
                    approve: true,
                },
            )
            .await
            {
                Ok(r) => (true, r.summary),
                Err(e) => (false, format!("{e:?}")),
            }
        }
        Err(reason) => (false, reason),
    }
}

async fn ask_model(state: &AppState, args: &Value) -> (bool, String) {
    let Some(ep) = state.endpoint() else {
        return (false, "No model is serving.".into());
    };
    let prompt = args["prompt"].as_str().unwrap_or("").to_string();
    if prompt.trim().is_empty() {
        return (false, "Give it a prompt.".into());
    }
    let body = json!({
        "model": ep.model, "messages": [{ "role": "user", "content": prompt }],
        "max_tokens": args["max_tokens"].as_u64().unwrap_or(400).clamp(16, 4096),
        "stream": true, "stream_options": { "include_usage": true },
        "chat_template_kwargs": { "enable_thinking": false },
    });
    let started = Instant::now();
    let resp = crate::localhttp::local()
        .post(crate::assistant::chat_completions_url(&ep.base_url))
        .json(&body)
        .timeout(Duration::from_secs(600))
        .send()
        .await;
    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            return (
                false,
                format!(
                    "The model answered {}: {}",
                    r.status(),
                    cut(&r.text().await.unwrap_or_default(), 300)
                ),
            )
        }
        Err(e) => return (false, format!("Couldn't reach the model: {e}")),
    };
    use futures_util::StreamExt;
    let mut stream = resp.bytes_stream();
    let (mut text, mut first, mut tokens, mut buf) =
        (String::new(), None::<f64>, 0u64, Vec::<u8>::new());
    while let Some(Ok(chunk)) = stream.next().await {
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(t) = v["choices"][0]["delta"]["content"].as_str() {
                if first.is_none() && !t.is_empty() {
                    first = Some(started.elapsed().as_secs_f64());
                }
                text.push_str(t);
            }
            if let Some(n) = v["usage"]["completion_tokens"].as_u64() {
                tokens = n;
            }
        }
    }
    let total = started.elapsed().as_secs_f64();
    let ttft = first.unwrap_or(total);
    let speed = if tokens > 1 && total > ttft {
        (tokens - 1) as f64 / (total - ttft)
    } else {
        0.0
    };
    let reply = crate::assistant::strip_think(&text);
    (true, json!({
        "reply": cut(&reply, 3000), "model": ep.model, "tokens": tokens,
        "tokens_per_second": (speed * 10.0).round() / 10.0, "seconds_to_first_token": (ttft * 10.0).round() / 10.0,
    }).to_string())
}

async fn wait_until_ready(state: &AppState, run_id: &str, minutes: u64) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    // A start that's just been asked for may not show as starting for a moment.
    tokio::time::sleep(Duration::from_secs(5)).await;
    loop {
        if stopping(state, run_id) {
            return (false, "Stopped.".into());
        }
        let life = state.lifecycle();
        match life.phase {
            Phase::Serving => {
                let model = life.served_id.or(life.model).unwrap_or_default();
                return (true, format!("{model} is serving and ready."));
            }
            Phase::Failed => {
                return (
                    false,
                    format!("It didn't start: {}", life.error.unwrap_or_default()),
                )
            }
            Phase::Idle => {
                return (
                    false,
                    "Nothing is starting (and nothing is serving).".into(),
                )
            }
            _ => {}
        }
        if let Some(stuck) = crate::community::stalled_s(state) {
            return (
                false,
                format!(
                    "The start looks stuck: no progress for {} minutes.",
                    stuck / 60
                ),
            );
        }
        if Instant::now() > deadline {
            return (
                false,
                format!("It still wasn't ready after {minutes} minutes."),
            );
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn wait_for_download(
    state: &AppState,
    run_id: &str,
    id: &str,
    minutes: u64,
) -> (bool, String) {
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    loop {
        if stopping(state, run_id) {
            return (false, "Stopped.".into());
        }
        let Json(c) = crate::community::get_community(State(state.clone()), BearerAuth).await;
        let pull = c["pulls"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|d| d["id"] == id)
            .cloned();
        match pull {
            Some(d) if d["running"] == true => {
                let pct = d["progress"]["percent"]
                    .as_f64()
                    .map(|p| format!(" ({p:.0}%)"))
                    .unwrap_or_default();
                set_activity(state, run_id, &format!("Downloading {}{pct}", short(id)));
            }
            Some(d) if d["failed"] == true => {
                return (
                    false,
                    format!("The download failed: {}", d["line"].as_str().unwrap_or("")),
                )
            }
            Some(d) if d["paused"] == true => return (false, "The download is paused.".into()),
            _ => {
                let here = c["installed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|p| p["id"] == id);
                return if here {
                    (true, format!("{id} is downloaded."))
                } else {
                    (
                        false,
                        format!("{id} isn't downloading, and it isn't on the box."),
                    )
                };
            }
        }
        if Instant::now() > deadline {
            return (
                false,
                format!("It was still downloading after {minutes} minutes."),
            );
        }
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

// ---------------------------------------------------------------- running

fn stopping(state: &AppState, run_id: &str) -> bool {
    mine(state).stop.lock().expect("routines").contains(run_id)
}

fn set_activity(state: &AppState, run_id: &str, what: &str) {
    update_run(state, run_id, |r| r.activity = Some(what.to_string()));
}

fn system_prompt(r: &Routine) -> String {
    let t = local(now());
    let days = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    format!(
        "You carry out a routine on a Tenstorrent QuietBox (four Blackhole chips) for TT Station, with the tools given. \
It's {} {:04}-{:02}-{:02}, {:02}:{:02}. Nobody may be watching: work it out yourself, one tool call at a time.\n\
- Find out before you act: check what's serving and what's on the box (box_status, community_models, list_models).\n\
- Tools marked ACTION change the box; the person may have to approve them. If one is declined, adapt or stop, and say so in your report.\n\
- After start_model, call wait_until_ready; after download_model, call wait_for_download. These can take a long time; that's fine.\n\
- To test a model thoroughly, use evaluate_model (it's graded automatically and saved for comparison); for a quick look, ask_model.\n\
- If the routine changes which model is serving, put the one that served before back at the end unless the goal says otherwise.\n\
- Never invent results. If something fails, say what and why.\n\
- Finish with your report for the person, in Markdown: a one-line headline first, then what you did, what you found (with numbers), and anything that needs their attention.\n\n\
The routine, \"{}\":\n{}",
        days[t.tm_wday.clamp(0, 6) as usize], t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, r.name, r.goal
    )
}

/// Starts a run of `r` in the background, on behalf of the paired device
/// `by` (`None`: the box); refuses if one is going.
fn start(state: &AppState, r: &Routine, trigger: &str, by: Option<String>) -> Result<Run, String> {
    let mut current = mine(state).current.lock().expect("routines");
    if let Some(id) = current.as_ref() {
        let name = load_run(state, id).map(|r| r.name).unwrap_or_default();
        return Err(format!(
            "Another routine is running ({name}). One runs at a time, since they share the chips."
        ));
    }
    let run = Run {
        id: format!("{}-{}", now(), &crate::pairing::issue_pair_id()[..6]),
        routine: r.id.clone(),
        name: r.name.clone(),
        trigger: trigger.into(),
        started: now(),
        finished: None,
        status: "running".into(),
        steps: vec![],
        report: None,
        pending: None,
        activity: Some("Starting".into()),
        queued: vec![],
        by,
    };
    save_run(state, &run);
    *current = Some(run.id.clone());
    drop(current);
    with_routines(state, |list| {
        if let Some(x) = list.iter_mut().find(|x| x.id == r.id) {
            x.last_run = Some(run.id.clone());
        }
    });
    let (st, rt, id) = (state.clone(), r.clone(), run.id.clone());
    tokio::spawn(async move {
        if let Some(mode) = rt.mode.clone() {
            let by = load_run(&st, &id).and_then(|r| r.by);
            finish_switch(&st, &rt, &id, &mode, by.as_deref()).await;
            return;
        }
        let outcome = execute(&st, &rt, &id).await;
        let (status, report) = match outcome {
            Ok(report) => ("done", report),
            Err(e) if stopping(&st, &id) => ("stopped", format!("Stopped. {e}")),
            Err(e) => ("failed", e),
        };
        let waiting = load_run(&st, &id).map(|r| r.queued).unwrap_or_default();
        let report = report + &waiting_note(&waiting);
        let run = update_run(&st, &id, |r| {
            r.status = status.into();
            r.finished = Some(now());
            r.report = Some(report.clone());
            r.pending = None;
            r.activity = None;
        });
        mine(&st).stop.lock().expect("routines").remove(&id);
        *mine(&st).current.lock().expect("routines") = None;
        prune_runs(&st);
        if let Some(run) = run {
            let headline = headline(&report);
            st.notifier().emit(crate::notify::Draft {
                kind: "routine",
                title: match status {
                    "done" => format!("{} finished", run.name),
                    "stopped" => format!("{} stopped", run.name),
                    _ => format!("{} didn't finish", run.name),
                },
                body: match waiting.len() {
                    0 => cut(&headline, 200),
                    n => format!(
                        "{} · {n} step{} wait{} for your OK",
                        cut(&headline, 160),
                        if n == 1 { "" } else { "s" },
                        if n == 1 { "s" } else { "" }
                    ),
                },
            });
        }
    });
    Ok(run)
}

/// How a mode switch went, as (status, report): `done`, `skipped` (the chips
/// were busy with something it mustn't interrupt) or `failed`.
pub(crate) fn switch_outcome(
    title: &str,
    blocker: Option<String>,
    started: Option<Result<bool, String>>,
    time: &str,
) -> (&'static str, String) {
    if let Some(why) = blocker {
        return (
            "skipped",
            format!("Skipped switching to {title}. {why} It tries again next time."),
        );
    }
    match started {
        Some(Ok(true)) => (
            "done",
            format!("Switching to {title} ({time}). You'll get a notification when it's ready."),
        ),
        Some(Ok(false)) => ("done", format!("{title} was already on.")),
        Some(Err(e)) => ("failed", format!("Couldn't switch to {title}. {e}")),
        None => ("failed", format!("Couldn't switch to {title}.")),
    }
}

/// A mode-switching routine's run: no model thinks; the runner switches,
/// unless the chips are busy with something a switch mustn't interrupt.
async fn finish_switch(state: &AppState, r: &Routine, run_id: &str, mode: &str, by: Option<&str>) {
    let title = crate::modes::mode_title(mode).unwrap_or(mode).to_string();
    let time = r.schedule.time.clone().unwrap_or_default();
    let began = Instant::now();
    let blocker = crate::schedules::blocker(state);
    let started = if let Some(why) = refused(state, by, "start_model") {
        Some(Err(why))
    } else if blocker.is_none() {
        set_activity(state, run_id, &format!("Switching to {title}"));
        Some(crate::modes::start(state, mode).await)
    } else {
        None
    };
    let (status, report) = switch_outcome(&title, blocker, started, &time);
    update_run(state, run_id, |run| {
        run.steps.push(Step {
            ts: now(),
            kind: if status == "skipped" {
                "note"
            } else {
                "action"
            }
            .into(),
            tool: (status != "skipped").then(|| "switch_mode".to_string()),
            args: json!({ "mode": mode }),
            result: report.clone(),
            ok: status != "failed",
            seconds: began.elapsed().as_secs(),
        });
        run.status = status.into();
        run.finished = Some(now());
        run.report = Some(report.clone());
        run.activity = None;
    });
    mine(state).stop.lock().expect("routines").remove(run_id);
    *mine(state).current.lock().expect("routines") = None;
    prune_runs(state);
    let draft = crate::notify::Draft {
        kind: "routine",
        title: match status {
            "done" => format!("Switching to {title}"),
            "skipped" => format!("Skipped switching to {title}"),
            _ => format!("Couldn't switch to {title}"),
        },
        body: report,
    };
    // Starting is followed by a "ready" notification of its own.
    if status == "done" {
        state.notifier().record(draft);
    } else {
        state.notifier().emit(draft);
    }
}

/// The run itself: the model plans, the runner acts. The report, or why it failed.
async fn execute(state: &AppState, r: &Routine, run_id: &str) -> Result<String, String> {
    let mut messages = vec![
        json!({ "role": "system", "content": system_prompt(r) }),
        json!({ "role": "user", "content": "Carry out the routine now." }),
    ];
    // The goal is the person's own words: addresses in it are theirs.
    let mut turn = crate::urltrust::Turn::default();
    turn.person_said(&r.goal);
    let tools = tools();
    let client = crate::localhttp::local();
    // Whom it acts for: what they may not do, it doesn't do unasked.
    let by = load_run(state, run_id).and_then(|r| r.by);
    for round in 0..=MAX_ROUNDS {
        if stopping(state, run_id) {
            return Err("Stopped by you.".into());
        }
        // A model to think with (one may be starting after a switch).
        set_activity(state, run_id, "Thinking");
        let ep = match state.endpoint() {
            Some(ep) => ep,
            None => {
                set_activity(state, run_id, "Waiting for a model to serve");
                let (ok, why) = wait_until_ready(state, run_id, 30).await;
                if !ok {
                    return Err(format!("No model to think with: {why}"));
                }
                state.endpoint().ok_or("No model is serving.")?
            }
        };
        let last = round == MAX_ROUNDS;
        let mut body = json!({
            "model": ep.model, "messages": messages, "max_tokens": 2048, "temperature": 0.2,
            "chat_template_kwargs": { "enable_thinking": false },
        });
        if !last {
            body["tools"] = tools.clone();
            body["tool_choice"] = json!("auto");
        }
        let resp = client
            .post(crate::assistant::chat_completions_url(&ep.base_url))
            .json(&body)
            .timeout(Duration::from_secs(600))
            .send()
            .await;
        let v: Value = match resp {
            Ok(x) if x.status().is_success() => x.json().await.map_err(|e| e.to_string())?,
            Ok(x) => {
                return Err(format!(
                    "The model refused ({}): {}",
                    x.status(),
                    cut(&x.text().await.unwrap_or_default(), 300)
                ))
            }
            Err(e) => return Err(format!("Couldn't reach the model: {e}")),
        };
        let message = v["choices"][0]["message"].clone();
        let content = crate::assistant::strip_think(message["content"].as_str().unwrap_or(""))
            .trim()
            .to_string();
        let calls = if last {
            vec![]
        } else {
            crate::assistant::parse_tool_calls(&message)
        };
        if calls.is_empty() {
            if content.is_empty() {
                return Err("The model gave no report.".into());
            }
            return Ok(content);
        }
        if !content.is_empty() {
            update_run(state, run_id, |run| {
                run.steps.push(Step {
                    ts: now(),
                    kind: "note".into(),
                    tool: None,
                    args: Value::Null,
                    result: cut(&content, 2000),
                    ok: true,
                    seconds: 0,
                })
            });
        }
        messages.push(
            json!({ "role": "assistant", "content": content, "tool_calls": message["tool_calls"] }),
        );
        for call in calls {
            let args = call.args.clone().unwrap_or_else(|_| json!({}));
            let began = Instant::now();
            let queue = call.args.is_ok() && must_queue(&call.name, &args, &turn);
            let (ok, result) = if call.args.is_err() {
                (
                    false,
                    "Those arguments weren't valid JSON; call it again.".to_string(),
                )
            } else if queue {
                queue_for_person(state, r, run_id, &call.name, &args);
                (false, QUEUED_NOTE.to_string())
            } else if is_action(&call.name) && !r.allowed.contains(&call.name) {
                match ask_person(state, r, run_id, &call.name, &args).await {
                    Some(true) => run_tool(state, run_id, &call.name, &args).await,
                    Some(false) => (
                        false,
                        "The person declined. Don't try this again in this run.".into(),
                    ),
                    None => return Err("Stopped while waiting for your OK.".into()),
                }
            } else if let Some(why) = refused(state, by.as_deref(), &call.name) {
                (false, why)
            } else {
                run_tool(state, run_id, &call.name, &args).await
            };
            if crate::assistant::WEB_TOOLS.contains(&call.name.as_str()) && !queue {
                turn.web_result(&result);
            }
            let kind = if queue {
                "queued"
            } else if is_action(&call.name) {
                "action"
            } else if call.name.starts_with("wait_") {
                "wait"
            } else {
                "tool"
            };
            update_run(state, run_id, |run| {
                run.steps.push(Step {
                    ts: now(),
                    kind: kind.into(),
                    tool: Some(call.name.clone()),
                    args: args.clone(),
                    result: cut(&result, 2000),
                    ok,
                    seconds: began.elapsed().as_secs(),
                });
            });
            messages.push(json!({ "role": "tool", "tool_call_id": call.id, "name": call.name, "content": cut(&result, 6000) }));
        }
    }
    Err("It ran out of steps without a report.".into())
}

/// Whether a routine is running now.
pub(crate) fn is_running(state: &AppState) -> bool {
    mine(state).current.lock().expect("routines").is_some()
}

/// What routines are waiting on the person for (`GET /approvals`): the
/// running one's question, and steps queued after a web read.
pub(crate) fn pending_approvals(state: &AppState) -> Vec<Value> {
    let mut out = Vec::new();
    let current = mine(state).current.lock().expect("routines").clone();
    if let Some(run) = current.as_deref().and_then(|id| load_run(state, id)) {
        if let Some(p) = run.pending {
            out.push(json!({ "id": format!("routine:{}", run.id), "kind": "routine", "run": run.id, "where": run.name, "summary": p.summary, "tool": p.tool, "input": p.args, "since": p.since, "can_always": false }));
        }
    }
    let t = now();
    for run in all_runs(state)
        .into_iter()
        .filter(|r| r.started + QUEUE_TTL > t)
    {
        for q in run
            .queued
            .iter()
            .filter(|q| q.status == "waiting" && q.since + QUEUE_TTL > t)
        {
            out.push(queued_approval(&run.id, &run.name, q));
        }
    }
    out
}

fn queued_approval(run: &str, name: &str, q: &Queued) -> Value {
    json!({ "id": format!("routine:{run}:{}", q.n), "kind": "routine", "run": run, "where": name, "summary": q.summary,
        "tool": q.tool, "input": q.args, "since": q.since, "can_always": false, "queued": true })
}

/// Queues `tool` for the person instead of running it, and asks them.
fn queue_for_person(state: &AppState, r: &Routine, run_id: &str, tool: &str, args: &Value) {
    let summary = action_summary(tool, args);
    let mut queued = None;
    update_run(state, run_id, |run| {
        let q = Queued {
            n: run.queued.len(),
            tool: tool.into(),
            args: args.clone(),
            summary: summary.clone(),
            since: now(),
            status: "waiting".into(),
            result: None,
        };
        queued = Some(q.clone());
        run.queued.push(q);
    });
    let Some(q) = queued else { return };
    state.notifier().emit_with(
        crate::notify::Draft {
            kind: "routine",
            title: format!("{} needs your OK", r.name),
            body: format!("{summary}? It read the web first, so it's waiting for you."),
        },
        Some(json!({ "approval": queued_approval(run_id, &r.name, &q) })),
    );
}

/// The end of a report: what's waiting for the person.
fn waiting_note(queued: &[Queued]) -> String {
    let waiting: Vec<&Queued> = queued.iter().filter(|q| q.status == "waiting").collect();
    if waiting.is_empty() {
        return String::new();
    }
    let mut note = "\n\n**Waiting for your OK.** This run read the web, so these weren't done without you (allow them in TT Station within a day):".to_string();
    for q in waiting {
        note.push_str(&format!("\n- {}", q.summary));
    }
    note
}

/// The person's answer to a queued step: declined, or run now (in the background).
pub(crate) fn answer_queued(
    state: &AppState,
    run_id: &str,
    n: usize,
    allow: bool,
) -> Result<String, ApiError> {
    let _one = mine(state).answering.lock().expect("routines");
    if !valid_id(run_id) {
        return Err(err(StatusCode::NOT_FOUND, "No such run."));
    }
    let run = load_run(state, run_id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such run."))?;
    let q = run
        .queued
        .get(n)
        .cloned()
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "Nothing like that is waiting."))?;
    if q.status != "waiting" {
        return Err(err(
            StatusCode::CONFLICT,
            "It isn't waiting for an answer any more.",
        ));
    }
    if allow {
        may_approve(state, &q.tool)?;
    }
    let status = if now() >= q.since + QUEUE_TTL {
        "expired"
    } else if allow {
        "allowed"
    } else {
        "declined"
    };
    update_run(state, run_id, |r| r.queued[n].status = status.into());
    match status {
        "expired" => {
            return Err(err(
                StatusCode::CONFLICT,
                "It waited too long, so it won't run now. Run the routine again.",
            ))
        }
        "declined" => return Ok("declined".into()),
        _ => {}
    }
    let (st, id) = (state.clone(), run_id.to_string());
    tokio::spawn(async move {
        let began = Instant::now();
        let (ok, result) = run_tool(&st, &id, &q.tool, &q.args).await;
        update_run(&st, &id, |r| {
            r.queued[n].result = Some(cut(&result, 2000));
            r.steps.push(Step {
                ts: now(),
                kind: if is_action(&q.tool) { "action" } else { "tool" }.into(),
                tool: Some(q.tool.clone()),
                args: q.args.clone(),
                result: cut(&format!("Allowed by you: {result}"), 2000),
                ok,
                seconds: began.elapsed().as_secs(),
            });
        });
    });
    Ok("allowed".into())
}

/// Asks the person to OK an action: Some(answer), or None if the run was stopped.
async fn ask_person(
    state: &AppState,
    r: &Routine,
    run_id: &str,
    tool: &str,
    args: &Value,
) -> Option<bool> {
    let summary = action_summary(tool, args);
    let (tx, rx) = tokio::sync::oneshot::channel();
    mine(state)
        .decisions
        .lock()
        .expect("routines")
        .insert(run_id.to_string(), tx);
    update_run(state, run_id, |run| {
        run.status = "waiting".into();
        run.pending = Some(Pending {
            tool: tool.into(),
            args: args.clone(),
            summary: summary.clone(),
            since: now(),
        });
        run.activity = Some("Waiting for your OK".into());
    });
    state.notifier().emit_with(
        crate::notify::Draft { kind: "routine", title: format!("{} needs your OK", r.name), body: format!("{summary}?") },
        Some(json!({ "approval": { "id": format!("routine:{run_id}"), "kind": "routine", "run": run_id, "summary": summary, "can_always": false } })),
    );
    let answer = rx.await.ok();
    update_run(state, run_id, |run| {
        run.status = "running".into();
        run.pending = None;
        run.activity = None;
    });
    answer.filter(|_| !stopping(state, run_id))
}

/// How late a mode switch may still happen (seconds).
const SWITCH_GRACE: u64 = 10 * 60;

fn too_late(due: u64, now: u64) -> bool {
    now >= due + SWITCH_GRACE
}

/// Runs scheduled routines when they're due. Started once, at agent start.
pub async fn schedule_forever(state: AppState) {
    // Runs that were going when the agent stopped can't carry on.
    for mut run in all_runs(&state)
        .into_iter()
        .filter(|r| r.finished.is_none())
    {
        run.status = "failed".into();
        run.finished = Some(now());
        run.pending = None;
        run.activity = None;
        run.report = Some("TT Station restarted while this was running, so it stopped.".into());
        save_run(&state, &run);
        mine(&state).live.lock().expect("routines").remove(&run.id);
    }
    loop {
        let due: Vec<Routine> = with_routines(&state, |list| {
            let t = now();
            for r in list.iter_mut() {
                if r.next_run.is_none() && r.enabled {
                    r.next_run = next_after(&r.schedule, t);
                }
            }
            // A mode switch more than ten minutes late (the box was off)
            // waits for its next time, as schedules always did.
            for r in list.iter_mut() {
                if r.mode.is_some() && r.next_run.is_some_and(|n| too_late(n, t)) {
                    r.next_run = next_after(&r.schedule, t);
                }
            }
            list.iter()
                .filter(|r| r.enabled && r.next_run.is_some_and(|n| n <= t))
                .cloned()
                .collect()
        });
        for r in due {
            let started = start(&state, &r, "schedule", r.by.clone());
            with_routines(&state, |list| {
                if let Some(x) = list.iter_mut().find(|x| x.id == r.id) {
                    x.next_run = next_after(&x.schedule, now());
                }
            });
            if let Err(e) = started {
                state.notifier().emit(crate::notify::Draft {
                    kind: "routine",
                    title: format!("{} didn't run", r.name),
                    body: e,
                });
            }
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// A report's headline: its "Headline:" line, else its first heading, else
/// its first line -- not a preamble like "All checks complete. Here's the report."
pub fn headline(report: &str) -> String {
    let clean = |l: &str| {
        l.trim()
            .trim_start_matches('#')
            .trim()
            .trim_matches('*')
            .trim()
            .to_string()
    };
    let lines: Vec<&str> = report
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(l) = lines
        .iter()
        .find(|l| l.to_lowercase().contains("headline:"))
    {
        let after = l.split_once(':').map(|x| x.1).unwrap_or(l);
        return clean(after);
    }
    if let Some(l) = lines.iter().find(|l| l.starts_with('#')) {
        return clean(l);
    }
    lines.first().map(|l| clean(l)).unwrap_or_default()
}

// ---------------------------------------------------------------- routes

fn view(state: &AppState, r: &Routine) -> Value {
    let last = r.last_run.as_deref().and_then(|id| load_run(state, id));
    json!({
        "id": r.id, "name": r.name, "goal": r.goal, "schedule": r.schedule, "when": describe(&r.schedule),
        "allowed": r.allowed, "enabled": r.enabled, "created": r.created, "next_run": if r.enabled { r.next_run } else { None },
        "mode": r.mode,
        "last": last.map(|l| json!({ "id": l.id, "status": l.status, "started": l.started, "finished": l.finished,
            "headline": l.report.as_deref().map(headline) })),
    })
}

/// `GET /routines`
pub(crate) async fn get_routines(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let list = with_routines(&state, |l| l.clone());
    let current = mine(&state).current.lock().expect("routines").clone();
    Json(json!({
        "routines": list.iter().map(|r| view(&state, r)).collect::<Vec<_>>(),
        "running": current.and_then(|id| load_run(&state, &id)),
        "actions": ACTIONS,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RoutineBody {
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub schedule: Schedule,
    #[serde(default)]
    pub allowed: Vec<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Just switch to this mode (see the module doc).
    #[serde(default)]
    pub mode: Option<String>,
}

/// A body with its blanks filled in: a mode switch needs no goal of its own.
fn normalized(mut b: RoutineBody) -> RoutineBody {
    b.mode = b.mode.filter(|m| !m.trim().is_empty());
    if let Some(m) = &b.mode {
        if b.name.trim().is_empty() {
            b.name = switch_name(m);
        }
        if b.goal.trim().is_empty() {
            let title = crate::modes::mode_title(m).unwrap_or(m);
            b.goal = format!("Switch the box to {title}.");
        }
        b.allowed = vec!["start_model".into()];
    }
    if let Some(days) = b.schedule.days.as_mut() {
        days.sort_unstable();
        days.dedup();
    }
    b
}

fn check(b: &RoutineBody) -> Result<(), ApiError> {
    if b.name.trim().is_empty() || b.goal.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Give it a name and say what it should do.",
        ));
    }
    if !["manual", "daily", "weekdays", "days", "hourly"].contains(&b.schedule.kind.as_str()) {
        return Err(err(StatusCode::BAD_REQUEST, "Pick when it runs."));
    }
    if matches!(b.schedule.kind.as_str(), "daily" | "weekdays" | "days")
        && parse_hhmm(b.schedule.time.as_deref().unwrap_or("")).is_none()
    {
        return Err(err(StatusCode::BAD_REQUEST, "Give a time like 08:30."));
    }
    if b.schedule.kind == "days"
        && b.schedule
            .days
            .as_deref()
            .is_none_or(|d| d.is_empty() || d.iter().any(|x| *x > 6))
    {
        return Err(err(StatusCode::BAD_REQUEST, "Pick at least one day."));
    }
    if let Some(m) = &b.mode {
        if crate::modes::mode_title(m).is_none() {
            return Err(err(StatusCode::BAD_REQUEST, format!("No mode called {m}.")));
        }
    }
    if let Some(bad) = b.allowed.iter().find(|a| !is_action(a)) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("{bad} isn't an action a routine can take."),
        ));
    }
    Ok(())
}

/// `POST /routines`
pub(crate) async fn post_routine(
    State(state): State<AppState>,
    _auth: BearerAuth,
    crate::routes::CallerAuth(caller): crate::routes::CallerAuth,
    Json(b): Json<RoutineBody>,
) -> Reply {
    let b = normalized(b);
    check(&b)?;
    let r = Routine {
        id: crate::pairing::issue_pair_id()[..10].to_string(),
        name: b.name.trim().chars().take(80).collect(),
        goal: b.goal.trim().chars().take(4000).collect(),
        next_run: next_after(&b.schedule, now()),
        schedule: b.schedule,
        allowed: b.allowed,
        enabled: b.enabled.unwrap_or(true),
        created: now(),
        last_run: None,
        mode: b.mode,
        by: caller.device_id(),
    };
    with_routines(&state, |l| l.push(r.clone()));
    Ok(Json(json!({ "routine": view(&state, &r) })))
}

/// `PUT /routines/{id}`
pub(crate) async fn put_routine(
    State(state): State<AppState>,
    _auth: BearerAuth,
    crate::routes::CallerAuth(caller): crate::routes::CallerAuth,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<RoutineBody>,
) -> Reply {
    let b = normalized(b);
    check(&b)?;
    let updated = with_routines(&state, |l| {
        let r = l.iter_mut().find(|r| r.id == id)?;
        r.name = b.name.trim().chars().take(80).collect();
        r.goal = b.goal.trim().chars().take(4000).collect();
        if r.schedule != b.schedule || b.enabled.is_some_and(|e| e && !r.enabled) {
            r.next_run = next_after(&b.schedule, now());
        }
        r.schedule = b.schedule;
        r.allowed = b.allowed;
        r.mode = b.mode;
        r.by = caller.device_id();
        if let Some(e) = b.enabled {
            r.enabled = e;
        }
        Some(r.clone())
    })
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such routine."))?;
    Ok(Json(json!({ "routine": view(&state, &updated) })))
}

/// `DELETE /routines/{id}`: the routine and its runs.
pub(crate) async fn delete_routine(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if mine(&state)
        .current
        .lock()
        .expect("routines")
        .as_ref()
        .and_then(|r| load_run(&state, r))
        .is_some_and(|r| r.routine == id)
    {
        return Err(err(StatusCode::CONFLICT, "It's running. Stop it first."));
    }
    let gone = with_routines(&state, |l| {
        let before = l.len();
        l.retain(|r| r.id != id);
        l.len() != before
    });
    if !gone {
        return Err(err(StatusCode::NOT_FOUND, "No such routine."));
    }
    for run in all_runs(&state).into_iter().filter(|r| r.routine == id) {
        if let Some(p) = run_path(&state, &run.id) {
            let _ = std::fs::remove_file(p);
        }
        mine(&state).live.lock().expect("routines").remove(&run.id);
    }
    Ok(Json(json!({ "deleted": true })))
}

/// `POST /routines/{id}/run`
pub(crate) async fn post_run(
    State(state): State<AppState>,
    _auth: BearerAuth,
    crate::routes::CallerAuth(caller): crate::routes::CallerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let r = with_routines(&state, |l| l.iter().find(|r| r.id == id).cloned())
        .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such routine."))?;
    if r.mode.is_none()
        && state.endpoint().is_none()
        && !matches!(state.lifecycle().phase, Phase::Starting)
    {
        return Err(err(
            StatusCode::CONFLICT,
            "No model is serving, and a routine needs one to think with. Start one in Models.",
        ));
    }
    // By hand: on behalf of whoever ran it (the box's own user acts as the box).
    let by = caller.device_id();
    let run = start(&state, &r, "manual", by).map_err(|e| err(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "run": run })))
}

/// `GET /routines/{id}/runs`: newest first, without their steps.
pub(crate) async fn get_runs(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Json<Value> {
    let runs: Vec<Value> = all_runs(&state)
        .into_iter()
        .filter(|r| r.routine == id)
        .take(30)
        .map(|r| json!({ "id": r.id, "status": r.status, "trigger": r.trigger, "started": r.started, "finished": r.finished, "steps": r.steps.len(),
            "headline": r.report.as_deref().map(headline) }))
        .collect();
    Json(json!({ "runs": runs }))
}

/// `GET /routines/runs/{run}`
pub(crate) async fn get_run(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(run): UrlPath<String>,
) -> Reply {
    if !valid_id(&run) {
        return Err(err(StatusCode::NOT_FOUND, "No such run."));
    }
    let r = load_run(&state, &run).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such run."))?;
    Ok(Json(json!({ "run": r })))
}

#[derive(Debug, Deserialize)]
pub struct Decision {
    pub allow: bool,
}

/// `POST /routines/runs/{run}/approve` `{allow}`
pub(crate) async fn post_approve(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(run): UrlPath<String>,
    Json(d): Json<Decision>,
) -> Reply {
    if d.allow {
        let pending = load_run(&state, &run).and_then(|r| r.pending);
        if let Some(p) = pending {
            may_approve(&state, &p.tool)?;
        }
    }
    let tx = mine(&state)
        .decisions
        .lock()
        .expect("routines")
        .remove(&run)
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                "It isn't waiting for an answer any more.",
            )
        })?;
    let _ = tx.send(d.allow);
    let _ = state;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /routines/runs/{run}/stop`
pub(crate) async fn post_stop(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(run): UrlPath<String>,
) -> Reply {
    if !valid_id(&run) || load_run(&state, &run).is_none_or(|r| r.finished.is_some()) {
        return Err(err(StatusCode::CONFLICT, "It isn't running."));
    }
    mine(&state)
        .stop
        .lock()
        .expect("routines")
        .insert(run.clone());
    // Waiting for an answer: the stop is the answer.
    if let Some(tx) = mine(&state)
        .decisions
        .lock()
        .expect("routines")
        .remove(&run)
    {
        let _ = tx.send(false);
    }
    set_activity(&state, &run, "Stopping after the current step");
    Ok(Json(json!({ "stopping": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedules_find_their_next_time() {
        let at = |s: &Schedule, t: u64| next_after(s, t);
        let manual = Schedule {
            kind: "manual".into(),
            ..Default::default()
        };
        assert_eq!(at(&manual, 1000), None);
        let hourly = Schedule {
            kind: "hourly".into(),
            every_hours: Some(6),
            ..Default::default()
        };
        assert_eq!(at(&hourly, 1000), Some(1000 + 6 * 3600));
        let daily = Schedule {
            kind: "daily".into(),
            time: Some("08:30".into()),
            ..Default::default()
        };
        let now = super::now();
        let next = at(&daily, now).unwrap();
        assert!(next > now && next <= now + 25 * 3600);
        let tm = local(next);
        assert_eq!((tm.tm_hour, tm.tm_min), (8, 30));
        let weekdays = Schedule {
            kind: "weekdays".into(),
            time: Some("23:59".into()),
            ..Default::default()
        };
        let mut t = now;
        for _ in 0..10 {
            t = at(&weekdays, t).unwrap();
            assert!(!matches!(local(t).tm_wday, 0 | 6), "never on a weekend");
        }
        assert_eq!(
            at(
                &Schedule {
                    kind: "daily".into(),
                    time: Some("25:00".into()),
                    ..Default::default()
                },
                now
            ),
            None
        );
    }

    #[test]
    fn a_run_for_a_member_does_not_switch_models_or_reset_chips_unasked() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(tmp.path().to_path_buf());
        let session = |id: &str, owner| crate::sessions::Session {
            id: id.into(),
            token_sha256: format!("sha-{id}"),
            name: id.into(),
            platform: None,
            created: None,
            last_used: crate::sessions::unix_now(),
            owner,
        };
        state.sessions().insert_for_test(session("owner", true));
        state.sessions().insert_for_test(session("member", false));
        // Anyone may switch models (the default); resetting is an owner's.
        assert_eq!(refused(&state, Some("member"), "stop_model"), None);
        assert!(refused(&state, Some("member"), "reset_chips").is_some());
        assert_eq!(refused(&state, Some("owner"), "reset_chips"), None);
        assert_eq!(refused(&state, None, "reset_chips"), None, "the box's own");
        let owners = crate::export::DataSettings {
            model_control: crate::model_control::ModelControl::Owners,
            ..Default::default()
        };
        crate::fsio::write_json(
            &tmp.path().join(crate::export::SETTINGS_FILE),
            &owners,
            crate::fsio::PRIVATE,
        )
        .unwrap();
        for tool in ["start_model", "stop_model", "evaluate_model"] {
            let why = refused(&state, Some("member"), tool).unwrap();
            assert!(why.starts_with(crate::model_control::OWNERS_ONLY), "{why}");
            assert_eq!(refused(&state, Some("owner"), tool), None);
            assert_eq!(refused(&state, None, tool), None);
            assert!(refused(&state, Some("removed"), tool).is_some());
        }
        assert_eq!(refused(&state, Some("member"), "download_model"), None);
        assert_eq!(refused(&state, Some("member"), "box_status"), None);
    }

    #[test]
    fn finds_the_headline_past_a_preamble() {
        let report = "All checks complete. Here's the report.\n\n## Morning health check — all clear\n\n**Headline: Box is healthy — Qwen3.8-27B is serving.**\n\nDetails…";
        assert_eq!(headline(report), "Box is healthy — Qwen3.8-27B is serving.");
        assert_eq!(headline("Preamble.\n# Big news\nmore"), "Big news");
        assert_eq!(headline("Just one line."), "Just one line.");
        assert_eq!(headline(""), "");
    }

    #[test]
    fn says_when_a_routine_runs() {
        assert_eq!(
            describe(&Schedule {
                kind: "weekdays".into(),
                time: Some("08:30".into()),
                ..Default::default()
            }),
            "Weekdays at 08:30"
        );
        assert_eq!(
            describe(&Schedule {
                kind: "hourly".into(),
                every_hours: Some(1),
                ..Default::default()
            }),
            "Every hour"
        );
        assert_eq!(
            describe(&Schedule {
                kind: "manual".into(),
                ..Default::default()
            }),
            "When you run it"
        );
    }

    #[test]
    fn offers_lookups_and_actions_but_not_desktop_control() {
        let names: Vec<String> = tools()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str().map(str::to_string))
            .collect();
        for want in [
            "box_status",
            "community_models",
            "ask_model",
            "wait_until_ready",
            "start_model",
            "download_model",
        ] {
            assert!(names.iter().any(|n| n == want), "missing {want}");
        }
        for never in ["control_computer", "open_app", "start_recording"] {
            assert!(
                !names.iter().any(|n| n == never),
                "{never} has no place in an unattended run"
            );
        }
        assert!(is_action("start_model") && !is_action("ask_model"));
    }

    #[test]
    fn after_web_text_acting_steps_are_queued() {
        let mut turn = crate::urltrust::Turn::default();
        turn.person_said(
            "Every morning, read https://status.example/today and remember anything odd.",
        );
        let none = json!({});
        // Before the web: looking, remembering and actions go as before.
        for t in ["box_status", "remember", "start_model", "read_webpage"] {
            assert!(
                !must_queue(t, &json!({ "url": "https://anything.example/?q=1" }), &turn),
                "{t}"
            );
        }
        turn.web_result(r#"{"url":"https://status.example/today","text":"All good. See https://status.example/history"}"#);
        // Every action -- given ahead of time or not -- remembering and research wait.
        for t in ACTIONS.iter().chain(ACTING.iter()) {
            assert!(must_queue(t, &none, &turn), "{t}");
        }
        for t in [
            "box_status",
            "list_models",
            "ask_model",
            "community_models",
            "wait_until_ready",
        ] {
            assert!(!must_queue(t, &none, &turn), "{t}");
        }
        // Reading: known addresses go; new ones (or new queries) wait.
        assert!(!must_queue(
            "read_webpage",
            &json!({ "url": "https://status.example/history" }),
            &turn
        ));
        assert!(!must_queue(
            "read_webpage",
            &json!({ "url": "https://status.example/today" }),
            &turn
        ));
        assert!(must_queue(
            "read_webpage",
            &json!({ "url": "https://status.example/today?box=qb2&model=Qwen3" }),
            &turn
        ));
        assert!(must_queue(
            "read_webpage",
            &json!({ "url": "https://collector.example/" }),
            &turn
        ));
    }

    #[test]
    fn a_report_says_what_waits() {
        let q = |n, status: &str| Queued {
            n,
            tool: "remember".into(),
            args: json!({ "fact": "x" }),
            summary: format!("Remember: x{n}"),
            since: 0,
            status: status.into(),
            result: None,
        };
        assert_eq!(waiting_note(&[]), "");
        assert_eq!(waiting_note(&[q(0, "declined")]), "");
        let note = waiting_note(&[q(0, "waiting"), q(1, "allowed"), q(2, "waiting")]);
        assert!(
            note.contains("Waiting for your OK")
                && note.contains("Remember: x0")
                && note.contains("Remember: x2")
                && !note.contains("x1"),
            "{note}"
        );
        // Older runs on disk have no queue.
        let old: Run = serde_json::from_value(json!({ "id": "1-a", "routine": "r", "name": "n", "trigger": "manual", "started": 1, "status": "done" })).unwrap();
        assert!(old.queued.is_empty());
    }

    #[test]
    fn runs_are_written_whole_and_a_broken_one_is_set_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(tmp.path().to_path_buf());
        let run: Run = serde_json::from_value(json!({ "id": "9-atomic", "routine": "r", "name": "n", "trigger": "manual", "started": 7, "status": "done" })).unwrap();
        save_run(&state, &run);
        let runs = tmp.path().join("routines/runs");
        // A crash mid-write leaves a temp file and a half-written run.
        std::fs::write(runs.join(".9-x.json.tmp-0123"), b"{").unwrap();
        std::fs::write(runs.join("9-broken.json"), b"{\"id\": ").unwrap();
        let all = all_runs(&state);
        assert_eq!(all.len(), 1, "only the whole run is listed");
        assert_eq!(all[0].id, "9-atomic");
        let names: Vec<String> = std::fs::read_dir(&runs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("9-broken.json.corrupt-")),
            "{names:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(runs.join("9-atomic.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_run_whose_id_isnt_its_file_name_is_ignored_and_never_deletes_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(tmp.path().to_path_buf());
        let runs = tmp.path().join("routines/runs");
        std::fs::create_dir_all(&runs).unwrap();
        let victim = tmp.path().join("memories.json");
        std::fs::write(&victim, b"[]").unwrap();
        std::fs::write(
            runs.join("0-evil.json"),
            br#"{"id":"../../memories","routine":"r","name":"n","trigger":"manual","started":0,"finished":1,"status":"done"}"#,
        )
        .unwrap();
        for n in 1..=(KEEP_RUNS as u64 + 1) {
            let run: Run = serde_json::from_value(json!({ "id": format!("{n}-ok"), "routine": "r", "name": "n", "trigger": "manual", "started": n, "status": "done" })).unwrap();
            save_run(&state, &run);
        }
        let all = all_runs(&state);
        assert!(all.iter().all(|r| r.id.ends_with("-ok")));
        prune_runs(&state);
        assert!(victim.exists(), "a pruned run never deletes outside runs/");
        assert!(!runs.join("1-ok.json").exists(), "the oldest real run went");
        assert!(run_path(&state, "../x").is_none());
    }

    fn sched(
        id: &str,
        mode: &str,
        time: &str,
        days: Vec<u8>,
        enabled: bool,
    ) -> crate::schedules::Schedule {
        crate::schedules::Schedule {
            id: id.into(),
            mode: mode.into(),
            time: time.into(),
            days,
            enabled,
            last_run: Some("2026-10-14".into()),
        }
    }

    #[test]
    fn schedules_become_mode_routines_keeping_what_they_did() {
        let at = super::now();
        let mut list = vec![];
        absorb_into(
            &mut list,
            vec![
                sched("abc123", "images", "07:30", vec![5, 1, 3, 3], true),
                sched("def456", "chat", "19:00", vec![0, 6], false),
            ],
            at,
        );
        assert_eq!(list.len(), 2);
        let r = &list[0];
        assert_eq!(r.id, "abc123", "keeps its id");
        assert_eq!(r.mode.as_deref(), Some("images"));
        assert_eq!(r.name, "Switch to Images");
        assert_eq!(r.schedule.kind, "days");
        assert_eq!(r.schedule.time.as_deref(), Some("07:30"));
        assert_eq!(r.schedule.days.as_deref(), Some(&[1u8, 3, 5][..]));
        assert!(r.enabled);
        let next = r.next_run.expect("an enabled one has a next run");
        assert!(next > at, "never runs again for a time already past");
        let tm = local(next);
        assert_eq!((tm.tm_hour, tm.tm_min), (7, 30));
        assert!([1, 3, 5].contains(&tm.tm_wday), "only on its days");
        assert!(
            !list[1].enabled && list[1].next_run.is_none(),
            "off stays off"
        );
        assert_eq!(describe(&list[1].schedule), "Weekends at 19:00");
        assert_eq!(describe(&r.schedule), "Mon, Wed, Fri at 07:30");

        // The same schedule again (an older phone app editing it) updates, never duplicates.
        absorb_into(
            &mut list,
            vec![sched("abc123", "video", "08:00", vec![2], true)],
            at,
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].mode.as_deref(), Some("video"));
        assert_eq!(list[0].schedule.days.as_deref(), Some(&[2u8][..]));

        // And older phone apps still see them as schedules.
        let back = schedules_of(&list);
        assert_eq!(back.len(), 2);
        assert_eq!(
            (
                back[0].id.as_str(),
                back[0].mode.as_str(),
                back[0].time.as_str()
            ),
            ("abc123", "video", "08:00")
        );
        assert_eq!(back[1].days, vec![0, 6]);
        // A routine that thinks isn't a schedule.
        let mut thinking = list[0].clone();
        thinking.id = "zzz".into();
        thinking.mode = None;
        assert_eq!(schedules_of(&[thinking]).len(), 0);
    }

    #[test]
    fn the_store_hands_its_schedules_over_once_and_keeps_a_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schedules.json");
        let store = crate::schedules::Schedules::open(&path);
        store.add("images", "07:30", vec![1, 2]).unwrap();
        let reopened = crate::schedules::Schedules::open(&path);
        let taken = reopened.take_all();
        assert_eq!(taken.len(), 1);
        assert!(reopened.list().is_empty());
        assert!(reopened.take_all().is_empty(), "only once");
        assert!(
            crate::schedules::Schedules::open(&path).list().is_empty(),
            "the file is emptied"
        );
        let kept: Vec<crate::schedules::Schedule> = serde_json::from_slice(
            &std::fs::read(tmp.path().join("schedules.json.migrated")).unwrap(),
        )
        .unwrap();
        assert_eq!(kept, taken);
    }

    #[test]
    fn a_mode_switch_skips_rather_than_interrupt_and_never_runs_late() {
        let (status, report) = switch_outcome(
            "Images",
            Some("A transcription job is running.".into()),
            None,
            "07:30",
        );
        assert_eq!(status, "skipped");
        assert!(
            report.contains("A transcription job is running.") && report.contains("next time"),
            "{report}"
        );
        assert_eq!(
            switch_outcome("Images", None, Some(Ok(true)), "07:30").0,
            "done"
        );
        assert_eq!(
            switch_outcome("Images", None, Some(Ok(false)), "07:30").1,
            "Images was already on."
        );
        let (status, report) = switch_outcome(
            "Images",
            None,
            Some(Err("Needs a download.".into())),
            "07:30",
        );
        assert_eq!(status, "failed");
        assert!(report.contains("Needs a download."));
        // Due from its minute until ten minutes after, as schedules were.
        assert!(!too_late(1000, 1000 + 9 * 60));
        assert!(too_late(1000, 1000 + 10 * 60));
    }

    #[test]
    fn a_mode_routine_fills_in_its_own_name_and_goal_and_checks_its_days() {
        let body = |kind: &str, days: Option<Vec<u8>>, mode: Option<&str>| RoutineBody {
            name: String::new(),
            goal: String::new(),
            schedule: Schedule {
                kind: kind.into(),
                time: Some("06:00".into()),
                every_hours: None,
                days,
            },
            allowed: vec![],
            enabled: None,
            mode: mode.map(str::to_string),
        };
        let b = normalized(body("days", Some(vec![3, 1, 1]), Some("chat")));
        assert_eq!(b.name, "Switch to Chat");
        assert!(!b.goal.is_empty());
        assert_eq!(b.allowed, vec!["start_model".to_string()]);
        assert_eq!(b.schedule.days.as_deref(), Some(&[1u8, 3][..]));
        assert!(check(&b).is_ok());
        assert!(
            check(&normalized(body("days", Some(vec![]), Some("chat")))).is_err(),
            "no days"
        );
        assert!(
            check(&normalized(body("days", Some(vec![7]), Some("chat")))).is_err(),
            "no such day"
        );
        assert!(
            check(&normalized(body("daily", None, Some("nonsense")))).is_err(),
            "no such mode"
        );
        assert!(
            check(&normalized(body("daily", None, None))).is_err(),
            "a thinking routine needs a goal"
        );
        // Weekdays-only "days" never land on a weekend.
        let s = Schedule {
            kind: "days".into(),
            time: Some("23:59".into()),
            every_hours: None,
            days: Some(vec![2]),
        };
        let mut t = super::now();
        for _ in 0..4 {
            t = next_after(&s, t).unwrap();
            assert_eq!(local(t).tm_wday, 2);
        }
    }
}
