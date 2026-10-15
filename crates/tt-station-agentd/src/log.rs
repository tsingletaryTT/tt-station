// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! A level for the agent's log lines, without a logging framework.
//!
//! The agent logs with `eprintln!("tt-station-agentd: ...")` and systemd puts
//! stderr in the journal, every line at the same priority, so a real failure is
//! as loud as a breadcrumb. Lines written through here carry a level:
//!
//! * under systemd (`$JOURNAL_STREAM` set), as the `<N>` prefix journald turns
//!   into the entry's priority, so `journalctl --user -u tt-station-agentd
//!   -p warning` shows only what needs a look;
//! * on a terminal, as a word: `tt-station-agentd: warn: ...`.
//!
//! `debug` lines are dropped unless `TT_STATION_LOG=debug`. New and central
//! messages should go through here (`crate::log!(warn, "...")`); existing
//! `eprintln!`s keep working and read as `info`.

use std::fmt::Display;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    /// syslog priority (journald's `<N>` prefix).
    fn priority(self) -> u8 {
        match self {
            Level::Error => 3,
            Level::Warn => 4,
            Level::Info => 6,
            Level::Debug => 7,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

fn debug_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TT_STATION_LOG").is_ok_and(|v| v.eq_ignore_ascii_case("debug"))
    })
}

fn to_journal() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("JOURNAL_STREAM").is_some())
}

/// One line as it's written: `journal` picks the `<N>` prefix over the word.
pub fn format_line(level: Level, journal: bool, msg: &str) -> String {
    if journal {
        format!("<{}>tt-station-agentd: {msg}", level.priority())
    } else {
        format!("tt-station-agentd: {}: {msg}", level.word())
    }
}

/// Write `msg` at `level` to stderr.
pub fn write(level: Level, msg: impl Display) {
    if level == Level::Debug && !debug_enabled() {
        return;
    }
    eprintln!("{}", format_line(level, to_journal(), &msg.to_string()));
}

pub fn error(msg: impl Display) {
    write(Level::Error, msg)
}

pub fn warn(msg: impl Display) {
    write(Level::Warn, msg)
}

pub fn info(msg: impl Display) {
    write(Level::Info, msg)
}

pub fn debug(msg: impl Display) {
    write(Level::Debug, msg)
}

/// `crate::log!(warn, "{model} didn't answer: {e}")`.
#[macro_export]
macro_rules! log {
    (error, $($arg:tt)*) => { $crate::log::error(format_args!($($arg)*)) };
    (warn, $($arg:tt)*) => { $crate::log::warn(format_args!($($arg)*)) };
    (info, $($arg:tt)*) => { $crate::log::info(format_args!($($arg)*)) };
    (debug, $($arg:tt)*) => { $crate::log::debug(format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_lines_carry_a_priority_and_terminal_lines_a_word() {
        assert_eq!(
            format_line(Level::Warn, true, "x"),
            "<4>tt-station-agentd: x"
        );
        assert_eq!(
            format_line(Level::Error, false, "x"),
            "tt-station-agentd: error: x"
        );
        assert!(Level::Error < Level::Debug);
    }
}
