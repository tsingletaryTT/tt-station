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

/// Trim a `tt-smi -s` JSON snapshot down to just the fields the macOS
/// dashboard renders: per-device `board_info.board_type` and
/// `telemetry.{asic_temperature,power,aiclk}`. This is the mirror image of
/// [`enrich_frame`]: instead of *adding* an optional key, it *drops*
/// everything the dashboard doesn't need (smbus_telem, firmwares, limits,
/// and any other telemetry field) so the lite frame is cheap to send on a
/// slow/metered link.
///
/// The result is a structural *subset* of the same tt-smi shape the app
/// already decodes -- so it's a drop-in for the dashboard, and an old agent
/// (which only ever emits the full frame) still just works from the app's
/// point of view, since full is a superset of lite.
///
/// Values are cloned **verbatim** (string or number, whichever the source
/// used) -- never reformatted -- and a key/sub-object is omitted entirely
/// when absent from the source rather than emitted as `null`. Never emits
/// `tt_toplike`. Malformed JSON, or a `device_info` that isn't an array,
/// yields `{"device_info":[]}`. Never panics.
pub fn lite_frame(tt_smi_json: &str) -> String {
    const EMPTY: &str = r#"{"device_info":[]}"#;

    let Ok(value) = serde_json::from_str::<serde_json::Value>(tt_smi_json) else {
        return EMPTY.to_string();
    };

    let Some(devices) = value.get("device_info").and_then(|d| d.as_array()) else {
        return EMPTY.to_string();
    };

    let trimmed: Vec<serde_json::Value> = devices
        .iter()
        .map(|dev| {
            let mut out = serde_json::Map::new();

            if let Some(board_type) = dev.get("board_info").and_then(|b| b.get("board_type")) {
                let mut board_info = serde_json::Map::new();
                board_info.insert("board_type".to_string(), board_type.clone());
                out.insert(
                    "board_info".to_string(),
                    serde_json::Value::Object(board_info),
                );
            }

            if let Some(telemetry) = dev.get("telemetry").and_then(|t| t.as_object()) {
                let mut lite_telemetry = serde_json::Map::new();
                for key in ["asic_temperature", "power", "aiclk"] {
                    if let Some(v) = telemetry.get(key) {
                        lite_telemetry.insert(key.to_string(), v.clone());
                    }
                }
                out.insert(
                    "telemetry".to_string(),
                    serde_json::Value::Object(lite_telemetry),
                );
            }

            serde_json::Value::Object(out)
        })
        .collect();

    let mut root = serde_json::Map::new();
    root.insert(
        "device_info".to_string(),
        serde_json::Value::Array(trimmed),
    );
    serde_json::to_string(&serde_json::Value::Object(root)).unwrap_or_else(|_| EMPTY.to_string())
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
            inference: None,
        }
    }

    /// Same as `sample_toplike`, but with an `inference` entry set -- for
    /// (d) below: the frame must round-trip `inference[0].phase` and its
    /// `serving.generation_tps`.
    fn sample_toplike_with_inference() -> TtToplike {
        use crate::inference::{InferenceInfo, Phase, ServingInfo};
        TtToplike {
            inference: Some(vec![InferenceInfo {
                key: "meta-llama/Llama-3.1-8B-Instruct".into(),
                label: "Llama-3.1-8B-Instruct".into(),
                phase: Phase::Ready,
                progress: None,
                serving: Some(ServingInfo {
                    generation_tps: 842.0,
                    prompt_tps: 120.0,
                    requests_running: 6,
                    requests_waiting: 2,
                    kv_cache_usage: 0.42,
                    ttft_avg_s: 0.11,
                    queue_avg_s: 0.02,
                    prefill_avg_s: 0.05,
                    decode_avg_s: 0.03,
                    tpot_avg_s: 0.01,
                    completed_delta: 4,
                    errored_delta: 0,
                    prefix_hit_rate: 0.0,
                    preemptions_delta: 0,
                }),
            }]),
            ..sample_toplike()
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

    /// (d) `enrich_frame` folds a populated `inference` entry into the wire
    /// frame with the exact field names/values the brief specifies, and a
    /// `TtToplike` with `inference: None` omits the key entirely (rather
    /// than emitting `"inference":null`) -- the None-vs-empty contract.
    #[test]
    fn enrich_frame_carries_inference_when_present_and_omits_key_when_none() {
        let frame = r#"{"device_info":[{"board_info":{"board_type":"p150a"}}]}"#;

        let with_inference = enrich_frame(frame, Some(&sample_toplike_with_inference()));
        let v: serde_json::Value = serde_json::from_str(&with_inference).unwrap();
        assert_eq!(v["tt_toplike"]["inference"][0]["phase"], "ready");
        assert_eq!(
            v["tt_toplike"]["inference"][0]["key"],
            "meta-llama/Llama-3.1-8B-Instruct"
        );
        assert_eq!(
            v["tt_toplike"]["inference"][0]["label"],
            "Llama-3.1-8B-Instruct"
        );
        assert!(v["tt_toplike"]["inference"][0]["serving"]["generation_tps"].is_number());
        assert_eq!(
            v["tt_toplike"]["inference"][0]["serving"]["generation_tps"],
            842.0
        );
        assert!(v["tt_toplike"]["inference"][0]["progress"].is_null());

        // `inference: None` (the plain `sample_toplike()` helper) -> no
        // `inference` key at all on `tt_toplike`, not `"inference":null`.
        let without_inference = enrich_frame(frame, Some(&sample_toplike()));
        let v2: serde_json::Value = serde_json::from_str(&without_inference).unwrap();
        assert!(v2["tt_toplike"].get("inference").is_none());
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

    #[test]
    fn lite_frame_trims_to_dashboard_fields() {
        let full = r#"{"device_info":[{"smbus_telem":{"x":1},"board_info":{"board_type":"p300c","other":"drop"},"telemetry":{"asic_temperature":"61.4","power":"85.2","aiclk":"1000","voltage":"0.8","fan_speed":"30"},"firmwares":{"a":"b"},"limits":{"c":"d"}}]}"#;
        let out = lite_frame(full);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let dev = &v["device_info"][0];
        assert_eq!(dev["board_info"]["board_type"], "p300c");
        assert_eq!(dev["telemetry"]["asic_temperature"], "61.4");
        assert_eq!(dev["telemetry"]["power"], "85.2");
        assert_eq!(dev["telemetry"]["aiclk"], "1000");
        // Trimmed: dropped fields + keys are gone.
        assert!(dev["telemetry"].get("voltage").is_none());
        assert!(dev["telemetry"].get("fan_speed").is_none());
        assert!(dev["board_info"].get("other").is_none());
        assert!(dev.get("smbus_telem").is_none());
        assert!(dev.get("firmwares").is_none());
        assert!(dev.get("limits").is_none());
        assert!(v.get("tt_toplike").is_none());
    }

    #[test]
    fn lite_frame_preserves_numeric_values_verbatim() {
        let full = r#"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":60,"power":85,"aiclk":1000}}]}"#;
        let v: serde_json::Value = serde_json::from_str(&lite_frame(full)).unwrap();
        assert_eq!(v["device_info"][0]["telemetry"]["asic_temperature"], 60);
    }

    #[test]
    fn lite_frame_omits_missing_fields() {
        let full = r#"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#; // no telemetry
        let v: serde_json::Value = serde_json::from_str(&lite_frame(full)).unwrap();
        assert_eq!(v["device_info"][0]["board_info"]["board_type"], "p300c");
        // telemetry object present but empty (or absent) — assert no crash + no stray fields
        let dev = &v["device_info"][0];
        assert!(dev["telemetry"].get("power").is_none());
    }

    #[test]
    fn lite_frame_garbage_yields_empty_device_info() {
        let v: serde_json::Value = serde_json::from_str(&lite_frame("not json")).unwrap();
        assert_eq!(v["device_info"].as_array().unwrap().len(), 0);
        let v2: serde_json::Value =
            serde_json::from_str(&lite_frame(r#"{"device_info":"nope"}"#)).unwrap();
        assert_eq!(v2["device_info"].as_array().unwrap().len(), 0);
    }
}
