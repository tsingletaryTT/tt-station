//! Contract test proving `procscan::ProcessSampler` and `telemetry::enrich_frame`
//! compose correctly at the routes layer -- i.e. the same wiring
//! `telemetry_stream` (in `routes.rs`) does on every tick: sample the box's
//! processes, then fold them into a `tt-smi -s` frame.
//!
//! This is deliberately a narrower, faster check than the full WebSocket
//! integration test in `tests/telemetry.rs` (which exercises the real
//! `axum::serve` loop end-to-end with a stub `tt-smi`): it calls the two
//! public building blocks directly with a canned frame and a real sampler
//! scan, and asserts the merged output still parses as one JSON object
//! carrying both the original telemetry (`device_info`) and the new
//! `tt_toplike` key (`schema: 1`, `processes: [...]`).

use tt_station_agentd::procscan::ProcessSampler;
use tt_station_agentd::telemetry::enrich_frame;

/// A canned `tt-smi -s` snapshot, representative of real stdout -- the exact
/// shape doesn't matter to `enrich_frame` (it merges in a sibling key without
/// touching this), just that it's a JSON object.
const CANNED_TT_SMI_JSON: &str = r#"{"device_info":[{"board_info":{"board_type":"p150a"},"telemetry":{"asic_temperature":"61.4"}}]}"#;

#[test]
fn enriched_frame_carries_both_telemetry_and_tt_toplike() {
    // Same construction `telemetry_stream` does: one sampler, one `sample()`
    // call per tick.
    let mut sampler = ProcessSampler::new();
    let toplike = sampler.sample();

    let out = enrich_frame(CANNED_TT_SMI_JSON, Some(&toplike));

    let value: serde_json::Value =
        serde_json::from_str(&out).expect("enriched frame must still be valid JSON");
    let obj = value
        .as_object()
        .expect("enriched frame must be a JSON object");

    // Original telemetry is intact.
    assert!(
        obj.get("device_info").is_some(),
        "enriched frame lost the original tt-smi `device_info` key: {out}"
    );

    // The new key is present with the expected shape.
    let tt_toplike = obj
        .get("tt_toplike")
        .expect("enriched frame is missing `tt_toplike`");
    assert_eq!(tt_toplike["schema"], 1, "tt_toplike.schema should be 1");
    assert!(
        tt_toplike["processes"].is_array(),
        "tt_toplike.processes should be an array, got: {tt_toplike:?}"
    );
}
