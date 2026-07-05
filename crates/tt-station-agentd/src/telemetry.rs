//! Telemetry snapshotting for the `GET /telemetry` WebSocket stream.
//!
//! This is the *publisher* half of the "remote QuietBox" feature (see
//! `tt-toplike-remote-quietbox/docs/REMOTE_QUIETBOX_DESIGN.md`): the box runs
//! `tt-smi -s` on an interval and pushes each snapshot to connected clients.
//!
//! The load-bearing contract: **the telemetry payload is the `tt-smi -s` JSON,
//! unchanged in meaning** -- the macOS client (tt-toplike's `JSONBackend`)
//! parses this exact shape, so the agent MUST NOT reshape the telemetry. Two
//! functions split the job: [`snapshot`] returns `tt-smi -s`'s stdout
//! byte-for-byte verbatim, and [`enrich_frame`] *additively* folds in the
//! optional `tt_toplike` key (processes; see `procscan`). `enrich_frame`
//! re-serializes through `serde_json::Value`, so the output is
//! semantically-identical rather than byte-identical -- top-level keys may be
//! reordered (alphabetized) and whitespace normalized. That's harmless: any
//! JSON parser is order-insensitive, and tt-smi's telemetry values are quoted
//! strings, so no numeric precision is lost. Existing consumers ignore the
//! unknown `tt_toplike` key and keep parsing the telemetry unchanged.
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

/// Insert the optional `tt_toplike` object into a `tt-smi -s` JSON frame and
/// re-serialize. Returns `frame` **unchanged** when `toplike` is `None`, when
/// `frame` isn't a JSON object, or on any serialize error — so the telemetry
/// contract is preserved and a process-scan hiccup can never corrupt the frame.
pub fn enrich_frame(frame: &str, toplike: Option<&crate::procscan::TtToplike>) -> String {
    let Some(toplike) = toplike else {
        return frame.to_string();
    };
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(frame)
    else {
        return frame.to_string();
    };
    let Ok(value) = serde_json::to_value(toplike) else {
        return frame.to_string();
    };
    map.insert("tt_toplike".to_string(), value);
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| frame.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procscan::{ProcInfo, TtToplike, TT_TOPLIKE_SCHEMA};

    fn sample_toplike() -> TtToplike {
        TtToplike {
            schema: TT_TOPLIKE_SCHEMA,
            processes: vec![ProcInfo {
                pid: 7,
                name: "python3".into(),
                cmd: "run.py".into(),
                uses_tt: true,
                cpu_pct: 3.5,
                mem_bytes: 100,
            }],
        }
    }

    #[test]
    fn enrich_inserts_key_and_keeps_tt_smi_valid() {
        let frame = r#"{"device_info":[{"board_info":{"board_type":"p150a"}}]}"#;
        let out = enrich_frame(frame, Some(&sample_toplike()));
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("device_info").is_some()); // telemetry intact
        assert_eq!(v["tt_toplike"]["schema"], 1);
        assert_eq!(v["tt_toplike"]["processes"][0]["pid"], 7);
    }

    #[test]
    fn enrich_none_returns_frame_verbatim() {
        let frame = r#"{"device_info":[]}"#;
        assert_eq!(enrich_frame(frame, None), frame);
    }

    #[test]
    fn enrich_non_object_frame_returned_verbatim() {
        // not a JSON object → can't insert a key → return unchanged (graceful)
        let frame = "[1,2,3]";
        assert_eq!(enrich_frame(frame, Some(&sample_toplike())), frame);
        let garbage = "not json at all";
        assert_eq!(enrich_frame(garbage, Some(&sample_toplike())), garbage);
    }

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
