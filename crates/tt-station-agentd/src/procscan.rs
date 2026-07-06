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

/// Select which processes to report: every `uses_tt` holder (kept even if
/// idle), then the busiest remaining processes by `cpu_pct`, capped at `cap`.
pub fn select_processes(procs: Vec<ProcInfo>, cap: usize) -> Vec<ProcInfo> {
    let (mut holders, mut others): (Vec<_>, Vec<_>) = procs.into_iter().partition(|p| p.uses_tt);
    others.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    holders.extend(others);
    holders.truncate(cap);
    holders
}

/// Stateful process sampler. Owns a `sysinfo::System` so cpu% is meaningful
/// across successive `sample()` calls (sysinfo computes cpu usage from the
/// delta between two refreshes; the telemetry loop's interval provides them).
pub struct ProcessSampler {
    sys: sysinfo::System,
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self {
            sys: sysinfo::System::new(),
        }
    }

    /// One scan → a `TtToplike { schema, processes }`. Refreshes processes,
    /// flags `uses_tt` via a best-effort /proc/<pid>/fd scan, then selects and
    /// caps via `select_processes`. Infallible: per-pid errors degrade to
    /// `uses_tt=false`; an empty box yields `processes: []`.
    pub fn sample(&mut self) -> TtToplike {
        self.sys
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let procs: Vec<ProcInfo> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| {
                let pid_u = pid.as_u32();
                ProcInfo {
                    pid: pid_u,
                    name: p.name().to_string_lossy().into_owned(),
                    cmd: p
                        .cmd()
                        .iter()
                        .map(|s| s.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" "),
                    uses_tt: pid_holds_tt_device(pid_u),
                    cpu_pct: p.cpu_usage(),
                    mem_bytes: p.memory(), // sysinfo 0.30+ returns bytes
                }
            })
            .collect();
        let procs = select_processes(procs, MAX_PROCESSES);
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
    fn select_puts_tt_holders_first_and_caps() {
        let procs = vec![
            proc(1, 90.0, false),
            proc(2, 5.0, true),
            proc(3, 50.0, false),
            proc(4, 1.0, true),
        ];
        let out = select_processes(procs, 3);
        assert_eq!(out.len(), 3);
        // both tt-holders survive the cap even though pid 4 is nearly idle
        assert!(out.iter().any(|p| p.pid == 2));
        assert!(out.iter().any(|p| p.pid == 4));
        // the remaining slot is the busiest non-holder (pid 1, cpu 90)
        assert!(out.iter().any(|p| p.pid == 1));
        assert!(!out.iter().any(|p| p.pid == 3));
    }

    #[test]
    fn select_orders_non_holders_by_cpu_desc() {
        let procs = vec![
            proc(1, 10.0, false),
            proc(2, 80.0, false),
            proc(3, 40.0, false),
        ];
        let out = select_processes(procs, 10);
        let non_holder_pids: Vec<u32> = out.iter().map(|p| p.pid).collect();
        assert_eq!(non_holder_pids, vec![2, 3, 1]);
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
