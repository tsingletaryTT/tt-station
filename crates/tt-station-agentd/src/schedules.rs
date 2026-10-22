// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Scheduled modes, now routines: switching the box's mode at set times is a
//! routine with a `mode` (`crate::routines`), and Routines is the one place
//! for it. What's left here is the old store, kept only so schedules still
//! arrive -- from `schedules.json` on a box that had them, an export, or an
//! older phone app -- and are turned into routines straight away
//! ([`crate::routines::absorb_schedules`]). The routes below keep answering in
//! the shape they always did, from those routines.
//!
//! - `GET /schedules`, `POST /schedules {mode, time: "HH:MM", days: [0..6]}` (0 is Sunday),
//!   `PATCH /schedules/{id} {enabled?, mode?, time?, days?}`, `DELETE /schedules/{id}`.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub id: String,
    /// A mode id (`crate::modes`): chat, images, transcribe.
    pub mode: String,
    /// Local time, "HH:MM".
    pub time: String,
    /// Days of the week it runs, 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    pub enabled: bool,
    /// The local day (YYYY-MM-DD) it last fired, so it fires once.
    #[serde(default)]
    pub last_run: Option<String>,
}

#[derive(Debug)]
pub struct Schedules {
    path: Option<PathBuf>,
    items: Mutex<Vec<Schedule>>,
}

pub const MAX_SCHEDULES: usize = 20;

/// "07:30" → (7, 30).
pub fn parse_time(t: &str) -> Option<(u32, u32)> {
    let (h, m) = t.trim().split_once(':')?;
    let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// Local (weekday, hour, minute, YYYY-MM-DD) now.
pub fn local_now() -> (u8, u32, u32, String) {
    // SAFETY: localtime_r writes only into the `tm` we own.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        (
            tm.tm_wday as u8,
            tm.tm_hour as u32,
            tm.tm_min as u32,
            format!(
                "{:04}-{:02}-{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday
            ),
        )
    }
}

/// Which schedules are due at this local moment and haven't run today.
pub fn due(items: &[Schedule], weekday: u8, hour: u32, minute: u32, today: &str) -> Vec<Schedule> {
    items
        .iter()
        .filter(|s| s.enabled && s.days.contains(&weekday))
        .filter(|s| s.last_run.as_deref() != Some(today))
        .filter(|s| {
            // Due from its minute until 10 minutes after (an agent restart
            // or a slow tick doesn't lose it).
            parse_time(&s.time).is_some_and(|(h, m)| {
                let at = h * 60 + m;
                let now = hour * 60 + minute;
                now >= at && now < at + 10
            })
        })
        .cloned()
        .collect()
}

impl Schedules {
    pub fn in_memory() -> Self {
        Schedules {
            path: None,
            items: Mutex::new(vec![]),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let items = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Schedules {
            path: Some(path),
            items: Mutex::new(items),
        }
    }

    pub fn list(&self) -> Vec<Schedule> {
        self.items.lock().expect("schedules mutex").clone()
    }

    fn save(&self, items: &[Schedule]) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(bytes) = serde_json::to_vec_pretty(items) {
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
    }

    pub(crate) fn check(mode: &str, time: &str, days: &[u8]) -> Result<(), String> {
        if crate::modes::mode_title(mode).is_none() {
            return Err(format!("No mode called {mode}."));
        }
        parse_time(time).ok_or("Give a time like 07:30.")?;
        if days.is_empty() || days.iter().any(|d| *d > 6) {
            return Err("Pick at least one day.".into());
        }
        Ok(())
    }

    /// Add or update schedules from an export (`crate::export`), by id:
    /// returns (added, updated). One for a mode this box doesn't have, or past
    /// [`MAX_SCHEDULES`], is left out.
    pub fn merge_imported(&self, incoming: Vec<Schedule>) -> (usize, usize) {
        let mut items = self.items.lock().expect("schedules mutex");
        let (mut added, mut updated) = (0, 0);
        for s in incoming {
            if !crate::export::safe_id(&s.id) || Self::check(&s.mode, &s.time, &s.days).is_err() {
                continue;
            }
            let len = items.len();
            match items.iter_mut().find(|x| x.id == s.id) {
                Some(cur) if *cur != s => {
                    *cur = s;
                    updated += 1;
                }
                Some(_) => {}
                None if len < MAX_SCHEDULES => {
                    items.push(s);
                    added += 1;
                }
                None => {}
            }
        }
        if added + updated > 0 {
            items.sort_by(|a, b| a.time.cmp(&b.time));
            self.save(&items);
        }
        (added, updated)
    }

    pub fn add(&self, mode: &str, time: &str, days: Vec<u8>) -> Result<Schedule, String> {
        Self::check(mode, time, &days)?;
        let mut items = self.items.lock().expect("schedules mutex");
        if items.len() >= MAX_SCHEDULES {
            return Err(format!("Up to {MAX_SCHEDULES} schedules."));
        }
        let (h, m) = parse_time(time).unwrap_or_default();
        let mut days = days;
        days.sort_unstable();
        days.dedup();
        let s = Schedule {
            id: crate::pairing::issue_pair_id()[..10].to_string(),
            mode: mode.to_string(),
            time: format!("{h:02}:{m:02}"),
            days,
            enabled: true,
            last_run: None,
        };
        items.push(s.clone());
        items.sort_by(|a, b| a.time.cmp(&b.time));
        self.save(&items);
        Ok(s)
    }

    pub fn update(&self, id: &str, patch: &Patch) -> Result<Schedule, String> {
        let mut items = self.items.lock().expect("schedules mutex");
        let s = items
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("No such schedule.")?;
        let mode = patch.mode.clone().unwrap_or_else(|| s.mode.clone());
        let time = patch.time.clone().unwrap_or_else(|| s.time.clone());
        let mut days = patch.days.clone().unwrap_or_else(|| s.days.clone());
        Self::check(&mode, &time, &days)?;
        days.sort_unstable();
        days.dedup();
        let (h, m) = parse_time(&time).unwrap_or_default();
        s.mode = mode;
        s.time = format!("{h:02}:{m:02}");
        s.days = days;
        if let Some(enabled) = patch.enabled {
            s.enabled = enabled;
        }
        let s = s.clone();
        items.sort_by(|a, b| a.time.cmp(&b.time));
        self.save(&items);
        Ok(s)
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut items = self.items.lock().expect("schedules mutex");
        let before = items.len();
        items.retain(|s| s.id != id);
        let removed = items.len() != before;
        if removed {
            self.save(&items);
        }
        removed
    }

    /// Drop `handed` (schedules `crate::routines` has already saved as
    /// routines) from the store. What they were is kept first beside the file,
    /// as `schedules.json.migrated-<unix-seconds>` (never overwriting an
    /// earlier one); a schedule added or changed meanwhile stays. Fails, and
    /// keeps everything, when the copy or the store can't be written.
    pub fn hand_over(&self, handed: &[Schedule]) -> std::io::Result<()> {
        let mut items = self.items.lock().expect("schedules mutex");
        let left: Vec<Schedule> = items
            .iter()
            .filter(|s| !handed.contains(s))
            .cloned()
            .collect();
        if left.len() == items.len() {
            return Ok(());
        }
        if let Some(path) = &self.path {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut kept = path.with_extension(format!("json.migrated-{now}"));
            let mut n = 1;
            while kept.exists() {
                kept = path.with_extension(format!("json.migrated-{now}-{n}"));
                n += 1;
            }
            crate::fsio::write_json(&kept, handed, crate::fsio::PRIVATE)?;
            crate::fsio::write_json(path, &left, crate::fsio::PRIVATE)?;
        }
        *items = left;
        Ok(())
    }

    #[cfg(test)]
    fn mark_run(&self, id: &str, today: &str) {
        let mut items = self.items.lock().expect("schedules mutex");
        if let Some(s) = items.iter_mut().find(|s| s.id == id) {
            s.last_run = Some(today.to_string());
        }
        self.save(&items);
    }
}

/// What a schedule found when it came due.
pub fn blocker(state: &AppState) -> Option<String> {
    let busy_job = state.jobs().is_some_and(|j| {
        j.list().iter().any(|j| {
            matches!(
                j.status,
                crate::jobs::JobStatus::Running | crate::jobs::JobStatus::Switching
            )
        })
    });
    busy_job.then(|| "A transcription job is running.".to_string())
}

/// Turns schedules into routines as they arrive (at start, and every 30 s
/// after, for an import or an older phone app). Routines run them.
pub async fn run_forever(state: AppState) {
    loop {
        crate::routines::absorb_schedules(&state);
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

type Reply = Result<Json<Value>, ApiError>;

pub(crate) async fn get_schedules(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    Json(json!({ "schedules": crate::routines::as_schedules(&state) }))
}

#[derive(Deserialize)]
pub struct NewSchedule {
    pub mode: String,
    pub time: String,
    pub days: Vec<u8>,
}

pub(crate) async fn post_schedule(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<NewSchedule>,
) -> Reply {
    // The cap counts the schedules already turned into routines, too.
    Schedules::check(&req.mode, &req.time, &req.days)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    if crate::routines::as_schedules(&state).len() >= MAX_SCHEDULES {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("Up to {MAX_SCHEDULES} schedules."),
        ));
    }
    state
        .schedules()
        .add(&req.mode, &req.time, req.days)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(
        json!({ "schedules": crate::routines::as_schedules(&state) }),
    ))
}

#[derive(Debug, Default, Deserialize)]
pub struct Patch {
    pub mode: Option<String>,
    pub time: Option<String>,
    pub days: Option<Vec<u8>>,
    pub enabled: Option<bool>,
}

pub(crate) async fn patch_schedule(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(patch): Json<Patch>,
) -> Reply {
    crate::routines::patch_from_schedule(&state, &id, &patch)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(
        json!({ "schedules": crate::routines::as_schedules(&state) }),
    ))
}

pub(crate) async fn delete_schedule(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    if !crate::routines::remove_mode_routine(&state, &id) {
        return Err(err(StatusCode::NOT_FOUND, "No such schedule."));
    }
    Ok(Json(
        json!({ "schedules": crate::routines::as_schedules(&state) }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_due_windows() {
        assert_eq!(parse_time("07:30"), Some((7, 30)));
        assert_eq!(parse_time("7:05"), Some((7, 5)));
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("noon"), None);
        let s = Schedules::in_memory();
        let a = s.add("images", "8:00", vec![1, 2, 3, 4, 5, 5]).unwrap();
        assert_eq!(a.time, "08:00");
        assert_eq!(a.days, [1, 2, 3, 4, 5]);
        assert!(s.add("nonsense", "08:00", vec![1]).is_err());
        assert!(s.add("chat", "08:00", vec![]).is_err());
        let list = s.list();
        assert_eq!(due(&list, 1, 8, 0, "2026-10-05").len(), 1);
        assert_eq!(
            due(&list, 1, 8, 9, "2026-10-05").len(),
            1,
            "late by a few minutes still runs"
        );
        assert!(due(&list, 1, 8, 10, "2026-10-05").is_empty());
        assert!(
            due(&list, 0, 8, 0, "2026-10-04").is_empty(),
            "not on Sundays"
        );
        s.mark_run(&a.id, "2026-10-05");
        assert!(
            due(&s.list(), 1, 8, 1, "2026-10-05").is_empty(),
            "once a day"
        );
        s.update(
            &a.id,
            &Patch {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(due(&s.list(), 2, 8, 0, "2026-10-06").is_empty());
    }
}
