// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The activity log: who did what to the box, kept on disk.
//!
//! The box's events (`crate::notify`) are for telling people things and
//! forget after a couple of hundred. This is the record an owner looks at
//! afterwards: one JSON line per action that changes who can reach the box or
//! what it does --
//!
//! ```json
//! {"ts": 1760000000, "device_id": "9f…", "device_name": "Jane's iPhone",
//!  "action": "power", "target": "reboot", "result": "ok"}
//! ```
//!
//! -- for pairing and removing devices, owner changes, resets, SSH keys, app
//! keys, power, security settings, updates, answered approvals and starting
//! or stopping models, plus every owner-only request a member was refused.
//!
//! The file (`audit.jsonl` in the agent's config directory) is mode 0600 and
//! only ever appended to. At [`MAX_BYTES`] it moves aside to `audit.jsonl.1`
//! (the older ones shift up) and [`KEEP`] files in all are kept, so the log
//! stays bounded at about 50 MB. `GET /audit` (owners) reads it newest first.
//!
//! Refusals are throttled per device: in a minute, at most
//! [`DENIALS_PER_TARGET`] of the same request and [`DENIALS_PER_MINUTE`] in all
//! are written one by one; past that they're counted, and one summary line
//! (`"N more refused requests (last: …)"`) is written once the minute is over.
//! A member can't flush the log by being refused over and over.
//!
//! Never write a token, key or code here: entries name devices, keys and
//! people by their ids and names only.
//!
//! Who acted comes from the request: the auth layer (`routes::require_auth`)
//! runs each request's handler inside [`ACTOR`], so a handler records an
//! action with one line, `audit::record(&state, "power", &target, "ok")`.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Size at which the current file is rotated.
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Files kept, the current one included.
pub const KEEP: usize = 5;
/// What the in-memory log (tests, no config directory) holds at most.
const MEMORY_CAP: usize = 1000;
/// Longest target kept; a target is a name or an id, not a document.
const MAX_TARGET: usize = 200;
/// Refusals written one by one per device per minute; the rest are summed up.
pub const DENIALS_PER_MINUTE: u32 = 30;
/// Refusals of the same request written one by one per device per minute.
pub const DENIALS_PER_TARGET: u32 = 3;

/// One device's refusals in the current minute.
#[derive(Debug)]
struct Denials {
    minute: u64,
    count: u32,
    /// By request, this minute.
    per_target: HashMap<String, u32>,
    suppressed: u32,
    device_name: String,
    last_target: String,
}

/// Who made the request being handled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Actor {
    /// The paired device; `None` for the box itself (its own user, over local HTTP).
    pub device_id: Option<String>,
    pub device_name: String,
}

impl Actor {
    /// The box's own user, from a program on the box.
    pub fn the_box() -> Self {
        Actor {
            device_id: None,
            device_name: "This box".to_string(),
        }
    }
}

tokio::task_local! {
    /// The caller of the request being handled (set by `routes::require_auth`).
    pub static ACTOR: Actor;
}

/// One line of the log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Unix seconds.
    pub ts: u64,
    pub device_id: Option<String>,
    pub device_name: String,
    pub action: String,
    #[serde(default)]
    pub target: String,
    /// `ok`, `failed`, `denied`, or a short word saying how it ended.
    pub result: String,
}

pub struct AuditLog {
    path: Option<PathBuf>,
    /// Serializes appends and rotation; in memory, holds the entries.
    memory: Mutex<Vec<Entry>>,
    /// Refusals this minute, by device (`None`: the box itself).
    denials: Mutex<HashMap<Option<String>, Denials>>,
}

impl std::fmt::Debug for AuditLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditLog")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl AuditLog {
    /// Kept in memory only (tests, or no config directory).
    pub fn in_memory() -> Self {
        AuditLog {
            path: None,
            memory: Mutex::new(Vec::new()),
            denials: Mutex::default(),
        }
    }

    /// Appended to `path` (created 0600 on the first entry).
    pub fn open(path: impl Into<PathBuf>) -> Self {
        AuditLog {
            path: Some(path.into()),
            memory: Mutex::new(Vec::new()),
            denials: Mutex::default(),
        }
    }

    /// Add `entry`. A failed write is logged, never returned: the action it
    /// records has already happened.
    /// Refusals past [`DENIALS_PER_MINUTE`] are counted instead, and summed
    /// up in one line after their minute.
    pub fn append(&self, entry: Entry) {
        for e in self.throttle(entry) {
            self.write(e);
        }
    }

    /// What to write for `entry`: summaries of minutes that are over, then
    /// the entry itself unless it's one refusal too many.
    fn throttle(&self, entry: Entry) -> Vec<Entry> {
        let minute = entry.ts / 60;
        let mut out = Vec::new();
        let mut denials = self.denials.lock().expect("audit mutex poisoned");
        for (device_id, d) in denials.iter_mut() {
            if d.minute < minute && d.suppressed > 0 {
                out.push(Entry {
                    ts: d.minute * 60 + 59,
                    device_id: device_id.clone(),
                    device_name: d.device_name.clone(),
                    action: "denied".into(),
                    target: format!(
                        "{} more refused requests (last: {})",
                        d.suppressed, d.last_target
                    ),
                    result: "denied".into(),
                });
                d.suppressed = 0;
            }
        }
        denials.retain(|_, d| d.minute >= minute);
        if entry.result == "denied" {
            let d = denials
                .entry(entry.device_id.clone())
                .or_insert_with(|| Denials {
                    minute,
                    count: 0,
                    per_target: HashMap::new(),
                    suppressed: 0,
                    device_name: String::new(),
                    last_target: String::new(),
                });
            let same = d.per_target.entry(entry.target.clone()).or_insert(0);
            *same += 1;
            if d.count >= DENIALS_PER_MINUTE || *same > DENIALS_PER_TARGET {
                d.suppressed += 1;
                d.device_name = entry.device_name;
                d.last_target = entry.target;
                return out;
            }
            // Written: counts toward the minute's cap.
            d.count += 1;
        }
        out.push(entry);
        out
    }

    fn write(&self, entry: Entry) {
        let mut memory = self.memory.lock().expect("audit mutex poisoned");
        let Some(path) = &self.path else {
            memory.push(entry);
            if memory.len() > MEMORY_CAP {
                memory.remove(0);
            }
            return;
        };
        if let Err(e) = append_line(path, &entry, MAX_BYTES) {
            eprintln!(
                "tt-station-agentd: couldn't write the activity log at {}: {e}",
                path.display()
            );
        }
    }

    /// Entries newest first, skipping `offset`, at most `limit`; and whether
    /// there are older ones past this page.
    pub fn page(&self, offset: usize, limit: usize) -> (Vec<Entry>, bool) {
        let memory = self.memory.lock().expect("audit mutex poisoned");
        let Some(path) = &self.path else {
            let all: Vec<Entry> = memory.iter().rev().cloned().collect();
            let more = all.len() > offset + limit;
            return (all.into_iter().skip(offset).take(limit).collect(), more);
        };
        // Newest file first; within a file, newest line first. A file is at
        // most `MAX_BYTES`, so reading one whole is fine for an owner's page.
        let mut seen = 0usize;
        let mut out = Vec::new();
        for i in 0..KEEP {
            let lines = read_entries(&rotated(path, i));
            for e in lines.into_iter().rev() {
                if seen >= offset {
                    if out.len() == limit {
                        return (out, true);
                    }
                    out.push(e);
                }
                seen += 1;
            }
        }
        (out, false)
    }
}

/// `path` for the current file (`n == 0`), `path.n` for older ones.
fn rotated(path: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n}"));
    path.with_file_name(name)
}

fn read_entries(path: &Path) -> Vec<Entry> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Append one line, rotating first when it would take the file past `max`.
fn append_line(path: &Path, entry: &Entry, max: u64) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() && !dir.exists() {
            std::fs::create_dir_all(dir)?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size + line.len() as u64 > max {
        for n in (1..KEEP).rev() {
            let from = rotated(path, n - 1);
            if from.exists() {
                std::fs::rename(&from, rotated(path, n))?;
            }
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Record an action by whoever made the current request (see [`ACTOR`]).
pub fn record(state: &crate::routes::AppState, action: &str, target: &str, result: &str) {
    let actor = ACTOR
        .try_with(Clone::clone)
        .unwrap_or_else(|_| Actor::the_box());
    record_as(state, &actor, action, target, result);
}

/// Record an action by `actor` (when it isn't the request's caller, as in
/// pairing, where the actor is the device that just paired).
pub fn record_as(
    state: &crate::routes::AppState,
    actor: &Actor,
    action: &str,
    target: &str,
    result: &str,
) {
    let target: String = target
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TARGET)
        .collect();
    state.audit().append(Entry {
        ts: crate::sessions::unix_now(),
        device_id: actor.device_id.clone(),
        device_name: actor.device_name.clone(),
        action: action.to_string(),
        target,
        result: result.to_string(),
    });
}

#[derive(Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub offset: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /audit?offset=&limit=` (owners): the activity log, newest first.
pub(crate) async fn get_audit(
    axum::extract::State(state): axum::extract::State<crate::routes::AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Query(q): axum::extract::Query<PageQuery>,
) -> axum::Json<serde_json::Value> {
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let log = state.audit();
    let (entries, more) = tokio::task::spawn_blocking(move || log.page(offset, limit))
        .await
        .unwrap_or_default();
    axum::Json(serde_json::json!({ "entries": entries, "more": more, "offset": offset }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u64) -> Entry {
        Entry {
            ts: n,
            device_id: Some(format!("dev{n}")),
            device_name: "Jane's iPhone".into(),
            action: "power".into(),
            target: "reboot".into(),
            result: "ok".into(),
        }
    }

    #[test]
    fn appends_lines_privately_and_pages_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path);
        for n in 0..5 {
            log.append(entry(n));
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 5);
        let (page, more) = log.page(0, 2);
        assert_eq!(page.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![4, 3]);
        assert!(more);
        let (page, more) = log.page(4, 2);
        assert_eq!(page.iter().map(|e| e.ts).collect::<Vec<_>>(), vec![0]);
        assert!(!more);
    }

    #[test]
    fn rotates_by_size_and_keeps_a_bounded_number_of_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        // Same-length lines (two-digit numbers), room for two a file: 20
        // entries make 10 files' worth.
        let one = serde_json::to_string(&entry(10)).unwrap().len() as u64 + 1;
        for n in 10..30 {
            append_line(&path, &entry(n), one * 2).unwrap();
        }
        for i in 0..KEEP {
            assert!(rotated(&path, i).exists(), "file {i}");
        }
        assert!(!rotated(&path, KEEP).exists(), "only {KEEP} files are kept");
        let log = AuditLog::open(&path);
        let (page, _) = log.page(0, 100);
        // The newest 10 survive, newest first, across the files in order.
        assert_eq!(
            page.iter().map(|e| e.ts).collect::<Vec<_>>(),
            (20..30).rev().collect::<Vec<_>>()
        );
    }

    #[test]
    fn repeated_refusals_are_summed_up_not_written_one_by_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path);
        let denied = |ts: u64, dev: &str, target: String| Entry {
            ts,
            device_id: Some(dev.into()),
            device_name: "Member".into(),
            action: "denied".into(),
            target,
            result: "denied".into(),
        };
        let lines = || std::fs::read_to_string(&path).unwrap().lines().count();
        let t0 = 6_000_000;
        // The same request over and over: a few lines.
        for _ in 0..500 {
            log.append(denied(t0, "m", "POST /run".into()));
        }
        assert_eq!(lines(), DENIALS_PER_TARGET as usize);
        // Different requests: up to the per-minute cap.
        for n in 0..500 {
            log.append(denied(t0, "m", format!("POST /x{n}")));
        }
        let mine = DENIALS_PER_MINUTE as usize;
        assert_eq!(lines(), mine);
        // Another device isn't held back by the first one's refusals.
        log.append(denied(t0, "other", "POST /run".into()));
        assert_eq!(lines(), mine + 1);
        // Owner actions still go straight in.
        log.append(entry(t0));
        assert_eq!(lines(), mine + 2);
        // The next minute: one summary line, then the new entry.
        log.append(denied(t0 + 60, "m", "POST /y".into()));
        let (page, _) = log.page(0, 2);
        assert_eq!(page[0].target, "POST /y");
        assert_eq!(
            page[1].target,
            format!("{} more refused requests (last: POST /x499)", 1000 - mine)
        );
        assert_eq!(page[1].device_id.as_deref(), Some("m"));
        assert_eq!(lines(), mine + 4);
    }

    #[test]
    fn the_in_memory_log_is_bounded() {
        let log = AuditLog::in_memory();
        for n in 0..(MEMORY_CAP as u64 + 10) {
            log.append(entry(n));
        }
        let (page, more) = log.page(0, 1);
        assert_eq!(page[0].ts, MEMORY_CAP as u64 + 9);
        assert!(more);
        assert_eq!(log.memory.lock().unwrap().len(), MEMORY_CAP);
    }
}
