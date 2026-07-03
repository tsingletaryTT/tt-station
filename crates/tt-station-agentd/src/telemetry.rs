//! Telemetry snapshotting for the `GET /telemetry` WebSocket stream.
//!
//! This is the *publisher* half of the "remote QuietBox" feature (see
//! `tt-toplike-remote-quietbox/docs/REMOTE_QUIETBOX_DESIGN.md`): the box runs
//! `tt-smi -s` on an interval and pushes each snapshot to connected clients.
//!
//! The single most load-bearing contract here: **a telemetry frame is the
//! verbatim stdout of `tt-smi -s`** -- a JSON telemetry snapshot. The macOS
//! client (tt-toplike's `JSONBackend`) already parses this exact shape, so the
//! agent MUST NOT reshape it. One schema, zero mapping: run `tt-smi -s`, send
//! its stdout. That's the whole job of [`snapshot`].
//!
//! The command runner is injected as a plain `Fn` so this function is
//! pure-ish and trivially unit-testable with canned `tt-smi -s` JSON, with no
//! real `tt-smi` binary or subprocess involved (see the tests below). In
//! production the runner is `serving::docker::RealCommandRunner::run` (see
//! `routes.rs`), the same argv-style, no-shell command seam every other
//! backend call already goes through.

use anyhow::Result;

/// Produce one telemetry snapshot by running `<tt_smi_bin> -s` and returning
/// its stdout **verbatim** (a JSON telemetry snapshot).
///
/// `run` is the injectable command seam: it receives the full argv
/// (`[tt_smi_bin, "-s"]`) and returns the child's captured stdout on success.
/// Keeping it a `&dyn Fn` (rather than hardcoding a `Command`) is what makes
/// this testable without a real `tt-smi` on the box -- tests pass a closure
/// returning canned JSON (or an error).
///
/// Any error from `run` (spawn failure, non-zero exit, ...) propagates as
/// `Err` -- callers decide whether to skip the tick or send an error frame;
/// this function never panics and never fabricates a snapshot.
pub fn snapshot(tt_smi_bin: &str, run: &dyn Fn(&[&str]) -> Result<String>) -> Result<String> {
    run(&[tt_smi_bin, "-s"])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A canned `tt-smi -s` snapshot stands in for the real binary's stdout.
    /// The exact JSON shape doesn't matter to `snapshot` (it passes stdout
    /// through untouched); this is just a plausible fragment so the test
    /// reads honestly.
    const CANNED_TT_SMI_JSON: &str = r#"{"device_info":[{"board_info":{"board_type":"p150a"},"telemetry":{"asic_temperature":"61.4"}}]}"#;

    #[test]
    fn snapshot_returns_runner_stdout_verbatim() {
        // A fake runner that records the argv it was handed and returns
        // canned tt-smi JSON -- no real subprocess involved.
        let seen_args = std::cell::RefCell::new(Vec::<String>::new());
        let run = |args: &[&str]| -> Result<String> {
            *seen_args.borrow_mut() = args.iter().map(|s| s.to_string()).collect();
            Ok(CANNED_TT_SMI_JSON.to_string())
        };

        let out = snapshot("tt-smi", &run).expect("snapshot should succeed");

        // The frame is the runner's stdout, byte-for-byte -- no reshaping.
        assert_eq!(out, CANNED_TT_SMI_JSON);
        // ...and it invoked exactly `<bin> -s`.
        assert_eq!(seen_args.into_inner(), vec!["tt-smi", "-s"]);
    }

    #[test]
    fn snapshot_honors_custom_binary_path() {
        let seen_args = std::cell::RefCell::new(Vec::<String>::new());
        let run = |args: &[&str]| -> Result<String> {
            *seen_args.borrow_mut() = args.iter().map(|s| s.to_string()).collect();
            Ok("{}".to_string())
        };

        let _ = snapshot("/opt/tt/bin/tt-smi", &run).expect("snapshot should succeed");

        assert_eq!(seen_args.into_inner(), vec!["/opt/tt/bin/tt-smi", "-s"]);
    }

    #[test]
    fn snapshot_propagates_runner_error_without_panicking() {
        // A runner that fails (as `tt-smi` can under serving load) must
        // surface as `Err`, never a panic -- the WS loop relies on this to
        // keep the connection alive across a transient failure.
        let run = |_args: &[&str]| -> Result<String> { Err(anyhow::anyhow!("tt-smi not found")) };

        let err = snapshot("tt-smi", &run).expect_err("snapshot should propagate the error");
        assert!(err.to_string().contains("tt-smi not found"));
    }
}
