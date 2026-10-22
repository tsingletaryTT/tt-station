// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! After a power cut: start the model that was serving again (opt-in).
//!
//! With "Start the last model when the box boots" on
//! ([`crate::export::DataSettings::restart_last_model`], off by default), the
//! agent keeps a note of what is serving (`last-serving.json`: the model, and
//! whether it was still serving at the last look, every 30 s). When the agent
//! starts, it reads that note *before* updating it and, once a model already
//! running has had time to be adopted, starts the model again through the
//! ordinary serve path (`routes::begin_serve`, which claims the start) when
//! [`decide`] says so:
//!
//! - the setting is on, and the note says a model was serving when the box
//!   went down (one the person stopped, or that stopped by itself, isn't
//!   brought back; nor is one serving when an owner shut the box down or
//!   rebooted it on purpose, which [`note_owner_stop`] notes);
//! - the box itself started a short while ago ([`BOOT_WINDOW_S`] of uptime):
//!   this is a boot, not just the agent restarting (an update, a crash), after
//!   which a model that was serving is still serving and is adopted instead;
//! - nothing is serving or starting, and no job is waiting to switch the box
//!   to what it needs.
//!
//! The model is started by the id it was started by (not its display name).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::routes::{AppState, Phase};

/// The note of what's serving, in the config directory.
pub const RECORD_FILE: &str = "last-serving.json";
/// Agent starts within this long of the box's own start count as a boot.
pub const BOOT_WINDOW_S: u64 = 15 * 60;
/// How long to give an already-running model to be adopted before deciding.
pub const SETTLE: Duration = Duration::from_secs(45);
/// How often the note is brought up to date.
pub const SAMPLE: Duration = Duration::from_secs(30);

/// What was serving at the last look.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LastServing {
    /// The model last seen serving (kept after it stops, for the record).
    #[serde(default)]
    pub model: Option<String>,
    /// Whether it was still serving at the last look.
    #[serde(default)]
    pub serving: bool,
    /// Unix seconds of that look.
    #[serde(default)]
    pub at: u64,
}

/// What the box looks like when the agent has just started.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Facts {
    /// The owner's setting.
    pub enabled: bool,
    /// Seconds since the box started (`/proc/uptime`), when known.
    pub uptime_s: Option<u64>,
    /// Nothing serving, starting or stopping.
    pub idle: bool,
    /// A queued job will switch the box to what it needs.
    pub jobs_pending: bool,
}

/// What to start again, if anything (see the module docs).
/// The model, by the id it was started by.
pub fn decide(last: &LastServing, facts: Facts) -> Option<String> {
    let booted = facts.uptime_s.is_some_and(|u| u <= BOOT_WINDOW_S);
    let free = facts.idle && !facts.jobs_pending;
    if !(facts.enabled && last.serving && booted && free) {
        return None;
    }
    last.model.clone().filter(|m| !m.trim().is_empty())
}

/// The note brought up to date from what the agent sees now. A start in
/// progress changes nothing (it either comes up or fails, and that's noted).
pub fn observe(last: &LastServing, phase: Phase, model: Option<String>, now: u64) -> LastServing {
    match phase {
        Phase::Serving => LastServing {
            model: model.or_else(|| last.model.clone()),
            serving: true,
            at: now,
        },
        Phase::Starting => last.clone(),
        Phase::Idle | Phase::Stopping | Phase::Failed => LastServing {
            serving: false,
            at: now,
            ..last.clone()
        },
    }
}

/// An owner is shutting the box down or rebooting it on purpose: note that
/// nothing is serving, so the next boot doesn't start it again. The model
/// stays in the note, for the record.
pub fn note_owner_stop(config_dir: &std::path::Path) {
    let path = config_dir.join(RECORD_FILE);
    let Some(mut last) = crate::fsio::read_json_or_backup::<LastServing>(&path) else {
        return;
    };
    if last.serving {
        last.serving = false;
        last.at = now();
        crate::fsio::save_json(&path, &last, crate::fsio::PRIVATE);
    }
}

/// Seconds since the box started.
pub fn uptime() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    parse_uptime(&text)
}

pub fn parse_uptime(text: &str) -> Option<u64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// At agent start: maybe start the last model again, then keep the note.
pub async fn run_forever(state: AppState) {
    let path = state.config_dir().join(RECORD_FILE);
    let mut last: LastServing = crate::fsio::read_json_or_backup(&path).unwrap_or_default();
    let uptime_s = uptime();
    tokio::time::sleep(SETTLE).await;
    let _ = state.reconciled_status().await;
    let life = state.lifecycle();
    let facts = Facts {
        enabled: crate::export::settings(&state).restart_last_model,
        uptime_s,
        idle: matches!(life.phase, Phase::Idle | Phase::Failed),
        jobs_pending: state.jobs().is_some_and(|j| j.has_pending()),
    };
    if let Some(id) = decide(&last, facts) {
        let shown = crate::notify::display_model(&id);
        crate::log::info(format!(
            "the box restarted while {shown} was serving; starting it again"
        ));
        // By the id it was started by: the display name isn't one.
        let started = crate::routes::begin_serve(&state, &id, false).await.is_ok();
        crate::audit::record_as(
            &state,
            &crate::audit::Actor::the_box(),
            "model_switch",
            &format!("{shown} (after a restart)"),
            if started { "starting" } else { "failed" },
        );
    }
    loop {
        let life = state.lifecycle();
        let next = observe(&last, life.phase, life.model.or(life.served_id), now());
        if next.model != last.model || next.serving != last.serving {
            crate::fsio::save_json(&path, &next, crate::fsio::PRIVATE);
        }
        last = next;
        tokio::time::sleep(SAMPLE).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            enabled: true,
            uptime_s: Some(120),
            idle: true,
            jobs_pending: false,
        }
    }

    fn was_serving() -> LastServing {
        LastServing {
            model: Some("meta-llama/Llama-3.3-70B-Instruct".into()),
            serving: true,
            at: 1,
        }
    }

    #[test]
    fn a_boot_with_the_setting_on_brings_the_model_back_by_its_id() {
        assert_eq!(
            decide(&was_serving(), facts()),
            Some("meta-llama/Llama-3.3-70B-Instruct".into()),
            "the id it was started by, not a display name"
        );
    }

    #[test]
    fn a_deliberate_shutdown_or_reboot_isnt_undone_at_the_next_boot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RECORD_FILE);
        crate::fsio::write_json(&path, &was_serving(), crate::fsio::PRIVATE).unwrap();
        note_owner_stop(dir.path());
        let last: LastServing = crate::fsio::read_json_or_backup(&path).unwrap();
        assert!(!last.serving);
        assert_eq!(last.model, was_serving().model, "kept for the record");
        assert_eq!(decide(&last, facts()), None);
        // Nothing noted yet: nothing to change.
        let empty = tempfile::tempdir().unwrap();
        note_owner_stop(empty.path());
        assert!(!empty.path().join(RECORD_FILE).exists());
    }

    #[test]
    fn nothing_starts_unless_every_condition_holds() {
        let last = was_serving();
        let cases = [
            (
                "setting off",
                Facts {
                    enabled: false,
                    ..facts()
                },
            ),
            (
                "an agent restart, not a boot",
                Facts {
                    uptime_s: Some(BOOT_WINDOW_S + 1),
                    ..facts()
                },
            ),
            (
                "uptime unknown",
                Facts {
                    uptime_s: None,
                    ..facts()
                },
            ),
            (
                "already serving (adopted)",
                Facts {
                    idle: false,
                    ..facts()
                },
            ),
            (
                "a job will switch the box",
                Facts {
                    jobs_pending: true,
                    ..facts()
                },
            ),
        ];
        for (why, f) in cases {
            assert_eq!(decide(&last, f), None, "{why}");
        }
        let stopped = LastServing {
            serving: false,
            ..was_serving()
        };
        assert_eq!(decide(&stopped, facts()), None, "it was stopped on purpose");
        assert_eq!(
            decide(&LastServing::default(), facts()),
            None,
            "never served"
        );
    }

    #[test]
    fn the_note_follows_what_is_serving() {
        let mut last = LastServing::default();
        last = observe(&last, Phase::Starting, Some("m".into()), 1);
        assert_eq!(last, LastServing::default(), "a start isn't serving yet");
        last = observe(&last, Phase::Serving, Some("m".into()), 2);
        assert_eq!((last.model.as_deref(), last.serving), (Some("m"), true));
        last = observe(&last, Phase::Serving, Some("m2".into()), 3);
        last = observe(&last, Phase::Idle, None, 4);
        assert_eq!(
            (last.model.as_deref(), last.serving),
            (Some("m2"), false),
            "stopped: remembered, but not brought back"
        );
    }

    #[test]
    fn uptime_parses() {
        assert_eq!(parse_uptime("350735.47 1400000.00\n"), Some(350735));
        assert_eq!(parse_uptime(""), None);
    }
}
