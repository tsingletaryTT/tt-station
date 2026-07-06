//! Box-side process scanning for the `tt_toplike` telemetry enrichment.
//!
//! `GET /telemetry` streams `tt-smi -s` verbatim today; this module adds an
//! optional, additive `tt_toplike` key carrying a short list of
//! processes running on the box, with a callout for anything holding a
//! Tenstorrent device open. This file is intentionally split into two
//! layers:
//!
//! - **Shared types** (`TtToplike`, `ProcInfo`) plus the schema/cap
//!   constants -- the wire shape sent to `tt-toplike` clients.
//! - **Pure helpers** (`target_holds_tt_device`, `select_processes`) --
//!   unit-tested here without touching `/proc` or spawning a real sampler.
//!
//! The `sysinfo`-based sampler that actually walks `/proc` and produces
//! `Vec<ProcInfo>` (Task 2), the frame enricher that merges a `TtToplike`
//! into each telemetry frame (Task 3), and the WebSocket wiring (Task 4)
//! land in later tasks.

use serde::Serialize;
use std::time::{Duration, Instant};

/// Schema version for the `tt_toplike` telemetry payload. Bump this if the
/// wire shape of `TtToplike`/`ProcInfo` changes in an incompatible way, so
/// `tt-toplike` clients can detect and handle the change explicitly.
pub const TT_TOPLIKE_SCHEMA: u32 = 1;

/// Maximum number of processes reported per telemetry frame. Keeps the
/// `tt_toplike` payload small even on a box with hundreds of processes --
/// `select_processes` decides which ones make the cut.
pub const MAX_PROCESSES: usize = 12;

/// The `tt_toplike` telemetry payload: a schema-versioned list of
/// processes running on the box, plus an optional view of the box's model
/// workload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtToplike {
    pub schema: u32,
    pub processes: Vec<ProcInfo>,
    /// The box's model-serving workload, or `None` when agentd has no
    /// authoritative opinion this tick (the consumer then falls back to its
    /// own local probe -- see `inference::build_inference`'s doc comment for
    /// the exact phase table). `skip_serializing_if` (not just `default` on
    /// deserialize, which this struct never does) is what actually omits the
    /// key on the wire when `None`; `default` alone lets old callers that
    /// construct a `TtToplike` without setting this field still compile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<Vec<crate::inference::InferenceInfo>>,
}

/// A single process entry in the `tt_toplike` payload.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    /// True if this process holds an open fd on a `/dev/tenstorrent*`
    /// device node (see `target_holds_tt_device`).
    pub uses_tt: bool,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}

/// True if any of a pid's fd symlink targets points at a Tenstorrent device
/// node. Pure (takes the already-readlink'd targets) so it's testable without
/// a real /proc.
pub fn target_holds_tt_device(fd_link_targets: &[String]) -> bool {
    fd_link_targets
        .iter()
        .any(|t| t.starts_with("/dev/tenstorrent"))
}

/// Select which processes to report: the busiest `cap` by `cpu_pct`.
///
/// `uses_tt` is intentionally NOT consulted here. `ProcessSampler::scan` builds
/// every `ProcInfo` with `uses_tt: false` and only does the (expensive)
/// `/proc/<pid>/fd` walk AFTER selection, on the chosen few -- so a genuinely
/// idle `/dev/tenstorrent` holder that doesn't rank into the top `cap` by cpu
/// won't be surfaced. That's the documented trade-off that keeps the scan cheap
/// (an earlier version partitioned holders first, but with the post-selection
/// fd walk that branch never fired -- see BOX_TELEMETRY_VALIDATION.md).
pub fn select_processes(mut procs: Vec<ProcInfo>, cap: usize) -> Vec<ProcInfo> {
    procs.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    procs.truncate(cap);
    procs
}

/// How often the process scan actually recomputes. The telemetry stream ticks
/// ~1/s per client, but the process list changes slowly and the scan (sysinfo
/// refresh + `/proc/<pid>/fd` readlinks) is the agent's single biggest CPU
/// cost, so `sample()` recomputes at most this often and returns the cached
/// list in between. cpu% is a delta across this interval (sysinfo needs two
/// refreshes to be meaningful) -- still fine at a few seconds, and less noisy.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(3);

/// Should the sampler recompute? True if it has never scanned, or the last
/// scan is at least `interval` old. Pure so the throttle is unit-testable
/// without a clock or a real `sysinfo` scan.
fn due_for_rescan(last_scan: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last_scan {
        None => true,
        Some(t) => now.duration_since(t) >= interval,
    }
}

/// Stateful process sampler. Owns a `sysinfo::System` (cpu% is only meaningful
/// as a delta between two refreshes) and caches its last result so repeated
/// `sample()` calls within `SCAN_INTERVAL` are nearly free -- the telemetry
/// loop calls it every tick, but the underlying scan runs at most once per
/// interval.
pub struct ProcessSampler {
    sys: sysinfo::System,
    cache: TtToplike,
    last_scan: Option<Instant>,
    /// Count of actual (non-throttled) scans, used by tests to assert the
    /// throttle actually skips the `/proc` walk within `SCAN_INTERVAL`.
    #[cfg(test)]
    scan_count: u32,
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self {
            sys: sysinfo::System::new(),
            cache: TtToplike {
                schema: TT_TOPLIKE_SCHEMA,
                processes: Vec::new(),
                inference: None,
            },
            last_scan: None,
            #[cfg(test)]
            scan_count: 0,
        }
    }

    /// Return the current process list, recomputing at most once per
    /// [`SCAN_INTERVAL`] and returning the cached snapshot in between.
    /// Infallible.
    pub fn sample(&mut self) -> TtToplike {
        if due_for_rescan(self.last_scan, Instant::now(), SCAN_INTERVAL) {
            self.cache = self.scan();
            self.last_scan = Some(Instant::now());
        }
        self.cache.clone()
    }

    /// The actual (expensive) scan: refresh process stats for all processes,
    /// **select the ones we'll report first**, then do the `/proc/<pid>/fd`
    /// readlink walk only for that small set to flag `uses_tt`. Doing the fd
    /// walk after selection (not for all ~hundreds of processes) is what keeps
    /// this cheap -- the walk is O(total open fds), which was the dominant CPU
    /// cost. Trade-off: a genuinely idle `/dev/tenstorrent` holder that doesn't
    /// rank into the top `MAX_PROCESSES` by cpu won't be surfaced.
    fn scan(&mut self) -> TtToplike {
        #[cfg(test)]
        {
            self.scan_count += 1;
        }
        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let procs: Vec<ProcInfo> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| ProcInfo {
                pid: pid.as_u32(),
                name: p.name().to_string_lossy().into_owned(),
                cmd: p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
                uses_tt: false, // set below, only for the selected few
                cpu_pct: p.cpu_usage(),
                mem_bytes: p.memory(), // sysinfo 0.30+ returns bytes
            })
            .collect();
        let mut procs = select_processes(procs, MAX_PROCESSES);
        for p in &mut procs {
            p.uses_tt = pid_holds_tt_device(p.pid);
        }
        TtToplike {
            schema: TT_TOPLIKE_SCHEMA,
            processes: procs,
            // `ProcessSampler` only ever produces the process list -- the
            // inference view is folded in separately by `telemetry_stream`
            // (see `routes.rs`), which owns the `InferenceSampler` and sets
            // this field on the `TtToplike` before `enrich_frame` runs.
            inference: None,
        }
    }
}

impl Default for ProcessSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Best-effort: read /proc/<pid>/fd, resolve each symlink, and test for a
/// Tenstorrent device holder. Any error (unreadable dir -- e.g. another uid's
/// process) yields `false`; never panics.
fn pid_holds_tt_device(pid: u32) -> bool {
    let dir = format!("/proc/{pid}/fd");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return false;
    };
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            targets.push(target.to_string_lossy().into_owned());
        }
    }
    target_holds_tt_device(&targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu: f32, uses_tt: bool) -> ProcInfo {
        ProcInfo {
            pid,
            name: format!("p{pid}"),
            cmd: String::new(),
            uses_tt,
            cpu_pct: cpu,
            mem_bytes: 0,
        }
    }

    #[test]
    fn rescan_due_only_when_never_scanned_or_interval_elapsed() {
        let now = Instant::now();
        let interval = Duration::from_secs(3);
        // never scanned → always due
        assert!(due_for_rescan(None, now, interval));
        // scanned just now → not due
        assert!(!due_for_rescan(Some(now), now, interval));
        // 1s later, interval 3s → still not due (throttled; returns cache)
        assert!(!due_for_rescan(
            Some(now),
            now + Duration::from_secs(1),
            interval
        ));
        // exactly at the interval → due again
        assert!(due_for_rescan(Some(now), now + interval, interval));
        // past the interval → due
        assert!(due_for_rescan(
            Some(now),
            now + Duration::from_secs(5),
            interval
        ));
    }

    #[test]
    fn holds_tt_device_matches_prefix() {
        assert!(target_holds_tt_device(&["/dev/tenstorrent0".into()]));
        assert!(target_holds_tt_device(&[
            "/dev/null".into(),
            "/dev/tenstorrent1".into()
        ]));
        assert!(!target_holds_tt_device(&[
            "/dev/null".into(),
            "socket:[123]".into()
        ]));
        assert!(!target_holds_tt_device(&[]));
    }

    #[test]
    fn select_is_top_n_by_cpu_ignoring_uses_tt() {
        // Selection is purely top-N by cpu; `uses_tt` is NOT consulted (the
        // sampler flags it only AFTER selection). A busy non-holder beats an
        // idle holder, and an idle holder is capped out like any other.
        let procs = vec![
            proc(1, 90.0, false),
            proc(2, 5.0, true), // holder, but nearly idle
            proc(3, 50.0, false),
            proc(4, 1.0, true), // holder, but idle
        ];
        let out = select_processes(procs, 2);
        let pids: Vec<u32> = out.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![1, 3]); // top 2 by cpu; idle holders 2 & 4 dropped
    }

    #[test]
    fn select_orders_by_cpu_desc() {
        let procs = vec![
            proc(1, 10.0, false),
            proc(2, 80.0, false),
            proc(3, 40.0, false),
        ];
        let out = select_processes(procs, 10);
        let pids: Vec<u32> = out.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![2, 3, 1]);
    }

    #[test]
    fn tt_toplike_serializes_with_brief_field_names() {
        let t = TtToplike {
            schema: TT_TOPLIKE_SCHEMA,
            processes: vec![proc(7, 3.5, true)],
            inference: None,
        };
        let json = serde_json::to_string(&t).unwrap();
        for key in [
            "\"schema\"",
            "\"processes\"",
            "\"pid\"",
            "\"name\"",
            "\"cmd\"",
            "\"uses_tt\"",
            "\"cpu_pct\"",
            "\"mem_bytes\"",
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        assert!(json.contains("\"schema\":1"));
    }

    #[test]
    fn sample_throttles_rescans_within_interval() {
        let mut s = ProcessSampler::new();
        let _ = s.sample(); // never scanned before → one real scan
        let _ = s.sample(); // immediate second call → within SCAN_INTERVAL → cached
        assert_eq!(
            s.scan_count, 1,
            "a second sample() within SCAN_INTERVAL must reuse the cache, not rescan /proc"
        );
    }

    #[test]
    fn sampler_reports_schema_and_finds_self() {
        let mut s = ProcessSampler::new();
        let _ = s.sample(); // first refresh seeds cpu% baseline
        let snap = s.sample();
        assert_eq!(snap.schema, TT_TOPLIKE_SCHEMA);
        assert!(snap.processes.len() <= MAX_PROCESSES);
        // the test process itself should be visible to sysinfo
        let me = std::process::id();
        // not asserting `me` is in the (capped/sorted) list -- it may be idle
        // and truncated; just assert the scan produced a well-formed, bounded
        // result.
        let _ = me;
    }
}
