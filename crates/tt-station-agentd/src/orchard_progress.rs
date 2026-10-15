// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What an Orchard run is doing right now, how far along it is, and whether
//! it needs the owner -- for the phone's progress bars.
//!
//! tt-orchard's status says which stage a run is in, not how far through it
//! is. This reads the rest from the run itself: the downloads' bytes on disk
//! against their size on the Hub, the ledger's own decisions (`coder
//! starting`, `agent step`, `hardware test started` with its deadline,
//! `queued for lease`), and the agent's turn log. Stage lengths come from
//! tt-orchard's first Clef run and its own budgets; they are typical times,
//! and the phone says so.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{json, Value};

use crate::orchard::STAGES;

/// About how long each stage took on tt-orchard's first Clef run (weights
/// plus sidecar, Coder-Next on one board), rounded up. Stages that run took
/// no measurement get the same order as their neighbours.
pub const TYPICAL_S: [f64; 9] = [
    900.0, 900.0, 2400.0, 5400.0, 3600.0, 1800.0, 1800.0, 2400.0, 900.0,
];
/// tt-orchard's `STAGE_BUDGET_S`: past this a stage stops.
pub const BUDGET_S: [f64; 9] = [
    7200.0, 14400.0, 14400.0, 21600.0, 28800.0, 10800.0, 14400.0, 14400.0, 3600.0,
];
/// Stages a weights-only run normally takes; the others run only on the
/// full-port path or when packaging, and count once they start.
const USUAL: [u64; 5] = [0, 1, 2, 4, 8];
/// tt-orchard's `COLD_BOOT_S`: a cold first boot on two chips.
pub const COLD_BOOT_S: f64 = 1800.0;
/// tt-orchard's `AGENT_MAX_TURNS`.
pub const AGENT_MAX_TURNS: u64 = 60;
/// Assumed download speed before one is measured.
const ASSUMED_BYTES_PER_S: f64 = 50e6;

/// A ledger timestamp (`2026-10-07T17:21:00Z`) as seconds since the epoch.
pub fn parse_ts(ts: &str) -> Option<f64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| ts.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some((days * 86400 + h * 3600 + mi * 60 + s) as f64)
}

/// What the ledger says about where a run is.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Facts {
    pub run_start: Option<f64>,
    pub open_stage: Option<u64>,
    pub stage_started: HashMap<u64, f64>,
    /// When the coder began starting, if it hasn't answered yet.
    pub coder_starting: Option<f64>,
    pub coder_ready: bool,
    /// `(started, deadline_s, chips)` of a hardware test in the open stage.
    pub hw_test: Option<(f64, f64, usize)>,
    pub queued: bool,
    pub agent_step: Option<String>,
    pub parking: bool,
}

/// Fold the ledger (one JSON object per line) into [`Facts`].
pub fn facts(entries: &[Value]) -> Facts {
    let mut f = Facts::default();
    for e in entries {
        let ts = e["ts"].as_str().and_then(parse_ts);
        let stage = e["stage"].as_u64();
        let d = &e["data"];
        match e["event"].as_str().unwrap_or("") {
            "run_start" => f.run_start = f.run_start.or(ts),
            "stage_start" => {
                f.open_stage = stage;
                if let (Some(n), Some(ts)) = (stage, ts) {
                    f.stage_started.insert(n, ts);
                }
                f.hw_test = None;
                f.queued = false;
                f.agent_step = None;
            }
            "stage_end" => {
                if stage == f.open_stage {
                    f.open_stage = None;
                }
                f.hw_test = None;
                f.queued = false;
                f.agent_step = None;
            }
            "decision" => match d["decision"].as_str().unwrap_or("") {
                "coder starting"
                | "finishing an unfinished coder boot"
                | "relaunch the coder under this supervisor's lease" => {
                    f.coder_starting = ts;
                    f.coder_ready = false;
                }
                "coder started" => {
                    f.coder_starting = None;
                    f.coder_ready = true;
                    f.parking = false;
                }
                "coder died; restarting it once" => {
                    f.coder_starting = ts;
                    f.coder_ready = false;
                }
                "stopping the coder" => f.parking = true,
                "agent step" | "wrap-up" => {
                    f.agent_step = d["phase"].as_str().map(str::to_string);
                    f.hw_test = None;
                    f.queued = false;
                }
                "queued for lease" => f.queued = true,
                "lease granted" | "lease taken" | "test lease taken" => f.queued = false,
                "hardware test started" => {
                    f.queued = false;
                    f.agent_step = None;
                    if let Some(ts) = ts {
                        f.hw_test = Some((
                            ts,
                            d["deadline_s"].as_f64().unwrap_or(COLD_BOOT_S * 1.5),
                            d["chips"].as_array().map(Vec::len).unwrap_or(0),
                        ));
                    }
                }
                "test lease released" | "hardware released" => f.hw_test = None,
                _ => {}
            },
            _ => {}
        }
    }
    f
}

/// Where each hardware test ran and how it went, from the ledger: this box,
/// or the lab box (`--lab`, whose tests carry `where: "lab <host>"`), plus the
/// configurations a lab was too small for ("not run: the lab ... has N chips").
pub fn hardware(entries: &[Value]) -> Value {
    let mut lab = Value::Null;
    let mut tests: Vec<Value> = Vec::new();
    for e in entries {
        let d = &e["data"];
        let stage = e["stage"].as_u64();
        let ts = e["ts"].as_str().and_then(parse_ts);
        match (e["event"].as_str().unwrap_or(""), d["decision"].as_str().unwrap_or("")) {
            ("run_start", _) if d["lab"].is_object() => lab = d["lab"].clone(),
            ("decision", "hardware test started") => tests.push(json!({
                "stage": stage,
                "config": d["config"].as_u64().unwrap_or(d["chips"].as_array().map(|c| c.len() as u64).unwrap_or(0)),
                "chips": d["chips"].as_array().map(Vec::len).unwrap_or(0),
                "where": d["where"].as_str().unwrap_or("this box"),
                "started": ts, "result": "running", "detail": Value::Null,
            })),
            ("evidence", _) if d["what"] == "hardware test" => {
                if let Some(why) = d["not_run"].as_str() {
                    tests.push(json!({
                        "stage": stage, "config": d["config"], "chips": 0, "where": "lab",
                        "started": ts, "result": "not run", "detail": why,
                    }));
                } else if let Some(t) = tests.iter_mut().rev().find(|t| t["stage"] == json!(stage) && t["result"] == "running") {
                    let (result, detail) = if d["timed_out"] == true {
                        ("timed out", Value::Null)
                    } else if d["returncode"] == 0 {
                        ("passed", Value::Null)
                    } else {
                        ("failed", json!(format!("exit {}", d["returncode"])))
                    };
                    t["result"] = json!(result);
                    t["detail"] = detail;
                }
            }
            _ => {}
        }
    }
    json!({ "lab": lab, "tests": tests })
}

pub fn read_ledger(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Bytes of a Hugging Face cache entry on disk, partial files included
/// (symlinks in `snapshots/` are skipped so nothing counts twice).
pub fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// The newest agent log in a stage and its turn count.
pub fn agent_turns(stage_dir: &Path, since: f64) -> Option<u64> {
    let newest = std::fs::read_dir(stage_dir.join("log"))
        .ok()?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max_by_key(|(m, _)| *m)?;
    let modified = newest
        .0
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    if modified + 1.0 < since {
        return None;
    }
    let text = std::fs::read_to_string(newest.1).ok()?;
    Some(text.lines().filter(|l| l.contains("\"turn\"")).count() as u64)
}

/// A download in flight.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub label: String,
    pub done: u64,
    pub total: Option<u64>,
    /// Bytes a second, once measured.
    pub rate: Option<f64>,
}

impl Download {
    pub fn finished(&self) -> bool {
        self.total.is_some_and(|t| self.done + t / 1000 >= t)
    }
}

fn gb(bytes: f64) -> String {
    format!("{:.1}", bytes / 1e9)
}

/// "about 3 h 10 min", "about 12 min", "under a minute".
pub fn approx(seconds: f64) -> String {
    let m = (seconds / 60.0).round() as u64;
    match m {
        0 => "under a minute".into(),
        1..=59 => format!("about {m} min"),
        _ if m.is_multiple_of(60) => format!("about {} h", m / 60),
        _ => format!("about {} h {} min", m / 60, m % 60),
    }
}

/// Everything the progress card needs.
pub struct Inputs<'a> {
    pub state: &'a str,
    pub now: f64,
    pub facts: &'a Facts,
    /// tt-orchard's `stages` list from its status.
    pub stages: &'a [Value],
    pub model_download: Option<Download>,
    pub coder_download: Option<Download>,
    pub turns: Option<u64>,
    pub blocked_reason: Option<&'a str>,
    pub pause_reason: Option<&'a str>,
}

fn download_line(d: &Download) -> (Option<f64>, String, Option<f64>) {
    let total = d.total.map(|t| t as f64);
    let fraction = total
        .filter(|t| *t > 0.0)
        .map(|t| (d.done as f64 / t).min(1.0));
    let mut detail = match total {
        Some(t) => format!("{} of {} GB", gb(d.done as f64), gb(t)),
        None => format!("{} GB so far", gb(d.done as f64)),
    };
    if let Some(r) = d.rate.filter(|r| *r > 1e5) {
        detail.push_str(&format!(" · {:.0} MB/s", r / 1e6));
    }
    let eta = match (total, d.rate) {
        (Some(t), Some(r)) if r > 1e5 => Some(((t - d.done as f64).max(0.0)) / r),
        _ => None,
    };
    (fraction, detail, eta)
}

/// The progress card for one run.
pub fn progress(i: &Inputs) -> Value {
    let f = i.facts;
    let stage_status = |n: u64| -> Option<&str> {
        i.stages
            .iter()
            .rev()
            .find(|s| s["stage"].as_u64() == Some(n))
            .and_then(|s| s["status"].as_str())
    };

    // What's happening now.
    let mut title = String::new();
    let mut detail = String::new();
    let mut fraction: Option<f64> = None;
    let mut eta: Option<f64> = None;
    let live = matches!(i.state, "preparing" | "running" | "paused");
    // The ledger's open stage, or failing that the one tt-orchard's status
    // reports as running.
    let open_stage = f.open_stage.or_else(|| {
        i.stages
            .iter()
            .rev()
            .find(|s| s["status"] == "running")
            .and_then(|s| s["stage"].as_u64())
    });
    if i.state == "preparing" {
        match &i.model_download {
            Some(d) if !d.finished() => {
                title = format!("Downloading {}", d.label);
                (fraction, detail, eta) = download_line(d);
            }
            _ => {
                title = "Getting ready".into();
                detail = "Checking the model and starting the supervisor.".into();
            }
        }
    } else if live && f.coder_starting.is_some() && !f.coder_ready {
        match &i.coder_download {
            // Only while bytes are arriving: a coder that failed before its
            // download leaves a few MB behind that never grow.
            Some(d) if !d.finished() && d.rate.is_some_and(|r| r > 1e5) => {
                title = format!("Downloading the agents' model ({})", d.label);
                (fraction, detail, eta) = download_line(d);
                detail.push_str(". Only the first run needs this.");
            }
            _ => {
                let since = i.now - f.coder_starting.unwrap_or(i.now);
                title = "Starting the agents' model".into();
                fraction = Some((since / COLD_BOOT_S).min(0.95));
                eta = Some((COLD_BOOT_S - since).max(60.0));
                detail = "It loads onto one board; the first start takes about 30 min.".into();
            }
        }
    } else if live && f.parking {
        title = "Moving the agents' model aside".into();
        detail = "A test needs its chips. It comes back when the test is done.".into();
    } else if let (true, Some(n)) = (live, open_stage) {
        let (emoji, name) = STAGES.get(n as usize).copied().unwrap_or(("•", "stage"));
        let started = f.stage_started.get(&n).copied().unwrap_or(i.now);
        let elapsed = (i.now - started).max(0.0);
        let typical = TYPICAL_S[n as usize % 9];
        let budget = BUDGET_S[n as usize % 9];
        title = format!("{emoji} Stage {n} of 8: {name}");
        if let Some((t0, deadline, chips)) = f.hw_test {
            let on = (i.now - t0).max(0.0);
            fraction = Some((on / COLD_BOOT_S).min(0.95));
            eta = Some((COLD_BOOT_S - on).max(60.0));
            detail = format!(
                "Testing on {} chip{}: boot the model, then compare its answers with the CPU reference. Gives up after {}.",
                chips.max(1),
                if chips == 1 { "" } else { "s" },
                approx(deadline).trim_start_matches("about ")
            );
        } else if f.queued {
            detail = "Waiting for chips to be free.".into();
        } else {
            fraction = Some((elapsed / typical).min(0.95));
            eta = Some((typical - elapsed).max(60.0));
            detail = match i.turns {
                Some(t) => {
                    format!("An agent is working on it: turn {t} of up to {AGENT_MAX_TURNS}.")
                }
                None => "An agent is working on it.".into(),
            };
            detail.push_str(&format!(
                " Usually {}; Orchard stops it after {}.",
                approx(typical).trim_start_matches("about "),
                approx(budget).trim_start_matches("about ")
            ));
        }
    } else if live {
        title = if f.run_start.is_some() {
            "Between stages".into()
        } else {
            "Starting the run".into()
        };
    }

    // The whole run, estimated.
    let mut plan: Vec<(f64, f64)> = vec![]; // (expected seconds, done fraction)
    let dl_total = i.model_download.as_ref().and_then(|d| d.total).unwrap_or(0) as f64;
    if dl_total > 0.0 {
        let done = i
            .model_download
            .as_ref()
            .map(|d| d.done as f64 / dl_total)
            .unwrap_or(0.0);
        let after = f.run_start.is_some() || i.state != "preparing";
        plan.push((
            dl_total / ASSUMED_BYTES_PER_S,
            if after { 1.0 } else { done.min(1.0) },
        ));
    }
    let coder = COLD_BOOT_S
        + i.coder_download
            .as_ref()
            .and_then(|d| {
                d.total
                    .map(|t| (t.saturating_sub(d.done)) as f64 / ASSUMED_BYTES_PER_S)
            })
            .unwrap_or(0.0);
    let coder_done = if f.coder_ready || f.open_stage.is_some() || !i.stages.is_empty() {
        1.0
    } else if let (Some(s), true) = (f.coder_starting, live) {
        ((i.now - s) / coder).min(0.95)
    } else {
        0.0
    };
    plan.push((coder, coder_done));
    for n in 0..9u64 {
        let status = stage_status(n);
        let started = status.is_some();
        if !USUAL.contains(&n) && !started {
            continue;
        }
        let typical = TYPICAL_S[n as usize];
        let done = match status {
            Some("pass" | "skipped") => 1.0,
            _ if open_stage == Some(n) => {
                let s = f.stage_started.get(&n).copied().unwrap_or(i.now);
                ((i.now - s) / typical).min(0.95)
            }
            _ => 0.0,
        };
        // A stage that was skipped took no time.
        let expected = if status == Some("skipped") {
            0.0
        } else {
            typical
        };
        plan.push((expected, done));
    }
    let total: f64 = plan.iter().map(|(s, _)| s).sum();
    let done: f64 = plan.iter().map(|(s, d)| s * d).sum();
    let finished = i.state == "ready-for-operator-review";
    let overall = if finished {
        1.0
    } else if total > 0.0 {
        (done / total).min(0.99)
    } else {
        0.0
    };
    let overall_eta = (live && !finished).then(|| (total - done).max(60.0));

    // What the owner should do.
    let needs_you = match i.state {
        "paused" => Some(format!(
            "{} Tap Resume to carry on, or Stop.",
            i.pause_reason.unwrap_or("It's paused.")
        )),
        "blocked" => Some(format!(
            "{} Fix that, then tap Retry: it picks up where it stopped.",
            i.blocked_reason.unwrap_or("It stopped and named why.")
        )),
        "stopped-or-crashed" => Some("It stopped on an error. Tap Retry to resume from where it was.".into()),
        "refused" => Some("tt-orchard's checks refused to start. Open the console to see why, then Retry.".into()),
        "ready-for-operator-review" => Some("Read the harvest basket below: results, risks and the model card. Publishing is up to you.".into()),
        _ => None,
    };
    let idle_hint = live.then(|| {
        "Nothing to do. You can close the app; you'll get a notification at each stage and when it's done.".to_string()
    });

    json!({
        "title": title,
        "detail": detail,
        "fraction": fraction,
        "eta_s": eta,
        "overall": overall,
        "overall_eta_s": overall_eta,
        "needs_you": needs_you,
        "hint": idle_hint,
        "typical_s": TYPICAL_S,
        "budget_s": BUDGET_S,
        "estimate_note": "Times are typical, from tt-orchard's first Clef run; agents vary.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ledger_timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(parse_ts("2026-10-07T17:21:00Z"), Some(1791393660.0));
        assert_eq!(parse_ts("nonsense"), None);
    }

    fn e(event: &str, ts: &str, stage: Option<u64>, data: Value) -> Value {
        json!({ "event": event, "ts": ts, "stage": stage, "data": data })
    }

    #[test]
    fn the_ledger_says_what_the_run_is_doing() {
        let mut l = vec![
            e("run_start", "2026-10-07T17:00:00Z", None, json!({})),
            e(
                "decision",
                "2026-10-07T17:00:05Z",
                None,
                json!({ "decision": "coder starting" }),
            ),
        ];
        let f = facts(&l);
        assert!(f.coder_starting.is_some() && !f.coder_ready);
        l.push(e(
            "decision",
            "2026-10-07T17:30:00Z",
            None,
            json!({ "decision": "coder started" }),
        ));
        l.push(e("stage_start", "2026-10-07T17:30:10Z", Some(2), json!({})));
        l.push(e(
            "decision",
            "2026-10-07T17:31:00Z",
            Some(2),
            json!({ "decision": "agent step", "phase": "prepare" }),
        ));
        let f = facts(&l);
        assert_eq!(f.open_stage, Some(2));
        assert_eq!(f.agent_step.as_deref(), Some("prepare"));
        l.push(e(
            "decision",
            "2026-10-07T17:40:00Z",
            Some(2),
            json!({ "decision": "hardware test started", "deadline_s": 2700.0, "chips": [0, 1] }),
        ));
        let f = facts(&l);
        assert_eq!(
            f.hw_test,
            Some((parse_ts("2026-10-07T17:40:00Z").unwrap(), 2700.0, 2))
        );
        l.push(e(
            "stage_end",
            "2026-10-07T18:00:00Z",
            Some(2),
            json!({ "result": "pass" }),
        ));
        let f = facts(&l);
        assert_eq!(f.open_stage, None);
        assert_eq!(f.hw_test, None);
    }

    fn inputs<'a>(state: &'a str, f: &'a Facts, stages: &'a [Value]) -> Inputs<'a> {
        Inputs {
            state,
            now: 10_000.0,
            facts: f,
            stages,
            model_download: None,
            coder_download: None,
            turns: None,
            blocked_reason: None,
            pause_reason: None,
        }
    }

    #[test]
    fn a_download_shows_bytes_speed_and_time_left() {
        let f = Facts::default();
        let mut i = inputs("preparing", &f, &[]);
        i.model_download = Some(Download {
            label: "clef".into(),
            done: 22_000_000_000,
            total: Some(55_000_000_000),
            rate: Some(110e6),
        });
        let p = progress(&i);
        assert_eq!(p["title"], "Downloading clef");
        assert_eq!(p["detail"], "22.0 of 55.0 GB · 110 MB/s");
        assert!((p["fraction"].as_f64().unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(p["eta_s"].as_f64().unwrap().round(), 300.0);
        assert!(p["needs_you"].is_null());
        assert!(p["hint"].as_str().unwrap().starts_with("Nothing to do"));
        assert!(p["overall"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn a_stage_shows_the_agent_turn_and_typical_time() {
        let mut f = Facts {
            open_stage: Some(1),
            coder_ready: true,
            ..Facts::default()
        };
        f.stage_started.insert(1, 9_700.0);
        let stages = [
            json!({ "stage": 0, "status": "pass" }),
            json!({ "stage": 1, "status": "running" }),
        ];
        let mut i = inputs("running", &f, &stages);
        i.turns = Some(12);
        let p = progress(&i);
        assert_eq!(p["title"], "🧪 Stage 1 of 8: soil test");
        assert_eq!(
            p["detail"],
            "An agent is working on it: turn 12 of up to 60. Usually 15 min; Orchard stops it after 4 h."
        );
        assert!((p["fraction"].as_f64().unwrap() - 300.0 / 900.0).abs() < 1e-9);
        // Coder, stage 0 and a third of stage 1 of (coder + stages 0, 1, 2, 4, 8).
        let total = COLD_BOOT_S + 900.0 + 900.0 + 2400.0 + 3600.0 + 900.0;
        let done = COLD_BOOT_S + 900.0 + 300.0;
        assert!((p["overall"].as_f64().unwrap() - done / total).abs() < 1e-9);
    }

    #[test]
    fn a_hardware_test_counts_toward_a_cold_boot() {
        let f = Facts {
            open_stage: Some(4),
            coder_ready: true,
            hw_test: Some((9_100.0, 2700.0, 4)),
            ..Facts::default()
        };
        let p = progress(&inputs("running", &f, &[]));
        assert_eq!(p["fraction"].as_f64().unwrap(), 0.5);
        assert!(p["detail"]
            .as_str()
            .unwrap()
            .starts_with("Testing on 4 chips"));
        assert!(p["detail"]
            .as_str()
            .unwrap()
            .ends_with("Gives up after 45 min."));
    }

    #[test]
    fn endings_say_what_to_do() {
        let f = Facts::default();
        let mut i = inputs("blocked", &f, &[]);
        i.blocked_reason = Some("stage 2 failed twice.");
        assert_eq!(
            progress(&i)["needs_you"],
            "stage 2 failed twice. Fix that, then tap Retry: it picks up where it stopped."
        );
        assert_eq!(
            progress(&inputs("ready-for-operator-review", &f, &[]))["overall"],
            1.0
        );
        assert!(progress(&inputs("ready-for-operator-review", &f, &[]))["hint"].is_null());
    }

    #[test]
    fn rough_durations() {
        assert_eq!(approx(20.0), "under a minute");
        assert_eq!(approx(900.0), "about 15 min");
        assert_eq!(approx(7200.0), "about 2 h");
        assert_eq!(approx(11_400.0), "about 3 h 10 min");
    }
}

#[cfg(test)]
mod hardware_tests {
    use super::*;

    #[test]
    fn reads_where_each_test_ran() {
        let ledger = vec![
            json!({"ts":"2026-10-08T10:00:00Z","event":"run_start","data":{"lab":{"host":"node4","chips":2}}}),
            json!({"ts":"2026-10-08T10:01:00Z","event":"decision","stage":4,"data":{"decision":"hardware test started","chips":["0","1"],"config":2,"where":"lab node4"}}),
            json!({"ts":"2026-10-08T10:30:00Z","event":"evidence","stage":4,"data":{"what":"hardware test","returncode":0,"timed_out":false}}),
            json!({"ts":"2026-10-08T10:31:00Z","event":"evidence","stage":4,"data":{"what":"hardware test","config":4,"returncode":null,"not_run":"not run: the lab node4 has 2 chips and this configuration needs 4"}}),
        ];
        let h = hardware(&ledger);
        assert_eq!(h["lab"]["host"], "node4");
        assert_eq!(h["tests"][0]["where"], "lab node4");
        assert_eq!(h["tests"][0]["result"], "passed");
        assert_eq!(h["tests"][1]["result"], "not run");
        assert_eq!(h["tests"][1]["config"], 4);
    }
}
