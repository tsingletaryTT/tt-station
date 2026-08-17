//! Integration tests for the `gozer` client module (Task 1 of the
//! tt-station x tt-gozer integration -- see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`).
//!
//! Lives under `tests/` (not `src/gozer.rs`'s own `#[cfg(test)]`) so it can
//! use the richer `FakeRunner` in `tests/support/mod.rs`, which isn't
//! reachable from this crate's own `src/`-internal unit tests (see that
//! file's module doc).
//!
//! Every test here injects a fake `CommandRunner` -- NEVER a real `gozer`
//! binary -- per the module-level constraint that `acquire`/`release`/`run`/
//! `reconcile` must never be invoked against a real box from tests.

mod support;

use libttstation::model::LeaseEntry;
use support::FakeRunner;
use tt_station_agentd::gozer::{self, Capability, Grant, LeaseSnapshot, Outcome};
use tt_station_agentd::serving::docker::CapturedOutput;

/// `probe` runs `<path> --version` and, on a clean exit-0 response, reports
/// `Some(Capability)` carrying the resolved path and the exact version
/// string gozer printed.
#[test]
fn probe_returns_capability_when_version_command_succeeds() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("--version", 0, "gozer 0.1.0", "");

    let cap = gozer::probe(&runner, Some("/opt/gozer/bin/gozer"));

    assert_eq!(
        cap,
        Some(Capability {
            path: "/opt/gozer/bin/gozer".to_string(),
            version: "gozer 0.1.0".to_string(),
        })
    );
}

/// A probe whose command can't even be spawned (gozer genuinely not
/// installed at the configured/hinted path) yields `None` -- absence is a
/// normal outcome, never an error that would abort agent startup.
#[test]
fn probe_returns_none_when_command_fails_to_run() {
    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("--version", "No such file or directory");

    let cap = gozer::probe(&runner, Some("/opt/gozer/bin/gozer"));

    assert_eq!(cap, None);
}

/// A probe that runs but exits non-zero (e.g. a broken install) is ALSO
/// absence, not an error -- only a clean exit 0 counts as "found".
#[test]
fn probe_returns_none_when_version_command_exits_nonzero() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("--version", 1, "", "broken pipe");

    let cap = gozer::probe(&runner, Some("/opt/gozer/bin/gozer"));

    assert_eq!(cap, None);
}

/// Exit 0 with a well-formed grant JSON payload on stdout parses to
/// `Outcome::Granted`, preserving `dev_indices` (the field the docker/runpy
/// backends will need to build `--device`/`--device-id` from in a later
/// task).
#[test]
fn granted_outcome_parses_dev_indices() {
    let captured = CapturedOutput {
        code: 0,
        stdout: r#"{
            "lease_id": "lease-123",
            "chips": ["0000:01:00.0", "0000:02:00.0"],
            "dev_indices": [0, 1],
            "units": ["board-a"],
            "expanded": false
        }"#
        .to_string(),
        stderr: String::new(),
    };

    let outcome = Outcome::from_captured(&captured);

    assert_eq!(
        outcome,
        Outcome::Granted(Grant {
            lease_id: "lease-123".to_string(),
            chips: vec!["0000:01:00.0".to_string(), "0000:02:00.0".to_string()],
            dev_indices: vec![0, 1],
            units: vec!["board-a".to_string()],
            expanded: false,
        })
    );
}

/// Exit 12 (unavailable) parses to `Outcome::Unavailable`, carrying whatever
/// holder/since-when detail the JSON payload provides.
#[test]
fn exit_12_parses_to_unavailable_with_holder() {
    let captured = CapturedOutput {
        code: 12,
        stdout: r#"{"granted": false, "holder": "claude:ttm-optimize", "since": "2026-08-16T14:02:00Z"}"#.to_string(),
        stderr: String::new(),
    };

    let outcome = Outcome::from_captured(&captured);

    assert_eq!(
        outcome,
        Outcome::Unavailable {
            holder: Some("claude:ttm-optimize".to_string()),
            since: Some("2026-08-16T14:02:00Z".to_string()),
        }
    );
}

/// Exit 10 (queued) ALSO maps to `Outcome::Unavailable` -- this version of
/// the integration deliberately does not queue (see the design doc's "When
/// chips are unavailable" section), so both codes collapse to the same
/// caller-facing outcome.
#[test]
fn exit_10_also_maps_to_unavailable() {
    let captured = CapturedOutput {
        code: 10,
        stdout: r#"{"granted": false, "queued": true, "ticket": "t-1"}"#.to_string(),
        stderr: String::new(),
    };

    let outcome = Outcome::from_captured(&captured);

    assert_eq!(
        outcome,
        Outcome::Unavailable {
            holder: None,
            since: None,
        }
    );
}

/// Malformed JSON on an exit-0 response is a `Failed`, never a panic --
/// gozer misbehaving must degrade to a reportable error, not crash agentd.
#[test]
fn malformed_json_on_exit_zero_yields_failed_not_panic() {
    let captured = CapturedOutput {
        code: 0,
        stdout: "not json at all {".to_string(),
        stderr: String::new(),
    };

    let outcome = Outcome::from_captured(&captured);

    match outcome {
        Outcome::Failed(_) => {}
        other => panic!("expected Outcome::Failed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// `Grant::reset_target` -- the argv value for a lease-scoped `tt-smi -r`.
// This is the one place a wrong answer resets hardware someone else is
// using, so it fails closed in every case it isn't certain about.
// ---------------------------------------------------------------------

/// Helper: a grant over `chips`, with the other fields irrelevant here.
fn grant_over(chips: &[&str]) -> Grant {
    Grant {
        lease_id: "lease-abc123".to_string(),
        chips: chips.iter().map(|c| c.to_string()).collect(),
        dev_indices: vec![0],
        units: vec![],
        expanded: false,
    }
}

/// The happy path: BDFs comma-joined into ONE argv value, exactly the form
/// gozer's own `reset.py` builds (`[exe, "-r", ",".join(bdfs)]`).
#[test]
fn reset_target_joins_bdfs_with_commas() {
    let grant = grant_over(&["0000:01:00.0", "0000:02:00.0"]);
    assert_eq!(
        grant.reset_target().expect("BDFs should be accepted"),
        "0000:01:00.0,0000:02:00.0"
    );
}

/// A non-BDF chip id must be REFUSED, not passed through: `tt-smi -r` would
/// read a bare integer as a UMD logical id -- a different namespace -- and
/// could reset a device nobody leased.
#[test]
fn reset_target_refuses_anything_that_is_not_a_bdf() {
    for chips in [
        vec!["0"],                 // a bare index
        vec!["0000:01:00.0", "1"], // one good, one not
        vec!["0000:01:00"],        // no function
        vec!["00:01:00.0"],        // short domain
        vec!["0000:01:00.8"],      // function out of range (0-7)
        vec!["0000:zz:00.0"],      // not hex
    ] {
        let err = grant_over(&chips)
            .reset_target()
            .expect_err(&format!("{chips:?} must be refused"));
        assert!(
            err.to_string().contains("not a PCI BDF"),
            "the error should say why: {err}"
        );
    }
}

/// An EMPTY grant must be refused too -- `tt-smi -r` with no target is a
/// whole-box reset, so degrading to it would be the worst possible reading
/// of "reset only what I leased".
#[test]
fn reset_target_refuses_an_empty_grant() {
    let err = grant_over(&[])
        .reset_target()
        .expect_err("an empty grant must be refused");
    assert!(
        err.to_string().contains("WHOLE BOX"),
        "the error should name the hazard it is avoiding: {err}"
    );
}

// ---------------------------------------------------------------------
// `snapshot_leases` -- the parse behind `GET /leases` and `/status`'s
// `leasing` field. Both routes shell out to `gozer status --json` exactly
// once and share this one parse of the payload (see the module's doc
// comment on `LeaseSnapshot`).
// ---------------------------------------------------------------------

/// A realistic two-board, four-chip `gozer status --json` payload: one
/// board fully `CLAIMED` by a remote agent, one board `FREE`. Mirrors the
/// shape already used in `tests/runpy.rs`'s `STATUS_JSON_HELD` /
/// `tests/serving.rs`'s fixtures, extended with a second board so the
/// board-count / max_concurrent math has something to count.
const STATUS_JSON_TWO_BOARDS: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false},
    {"dev_index":1,"bdf":"0000:02:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false},
    {"dev_index":2,"bdf":"0000:03:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,
     "reason":null,"pids_holding":[],"overstayed":false},
    {"dev_index":3,"bdf":"0000:04:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,
     "reason":null,"pids_holding":[],"overstayed":false}],
    "queue":[]}"#;

/// The happy path: every chip becomes one `LeaseEntry`, `chip`/`bdf`/`board`
/// carried through from gozer's `dev_index`/`bdf`/`board`, `state` preserved
/// VERBATIM (including the hyphenated `HELD-FOREIGN` form -- this must never
/// be renamed/restyled, see `LeaseEntry`'s doc comment), `who`/`reason`
/// passed through as-is, and -- the fact this whole task hinges on --
/// `since` is ALWAYS `None`, because gozer's per-chip status never carries
/// one. `boards` counts the 2 distinct board serials; with `"grain":
/// "board"`, `max_concurrent` equals that same count.
#[test]
fn snapshot_leases_maps_every_chip_and_never_invents_since() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("status", 0, STATUS_JSON_TWO_BOARDS, "");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    let snapshot = gozer::snapshot_leases(&runner, &capability)
        .expect("a clean exit-0 status payload must parse");

    assert_eq!(
        snapshot,
        LeaseSnapshot {
            leases: vec![
                LeaseEntry {
                    chip: 0,
                    bdf: "0000:01:00.0".to_string(),
                    board: "0100014311601055".to_string(),
                    state: "CLAIMED".to_string(),
                    who: Some("claude:ttm-optimize".to_string()),
                    since: None,
                    reason: Some("bringup".to_string()),
                },
                LeaseEntry {
                    chip: 1,
                    bdf: "0000:02:00.0".to_string(),
                    board: "0100014311601055".to_string(),
                    state: "CLAIMED".to_string(),
                    who: Some("claude:ttm-optimize".to_string()),
                    since: None,
                    reason: Some("bringup".to_string()),
                },
                LeaseEntry {
                    chip: 2,
                    bdf: "0000:03:00.0".to_string(),
                    board: "0100014311601048".to_string(),
                    state: "FREE".to_string(),
                    who: None,
                    since: None,
                    reason: None,
                },
                LeaseEntry {
                    chip: 3,
                    bdf: "0000:04:00.0".to_string(),
                    board: "0100014311601048".to_string(),
                    state: "FREE".to_string(),
                    who: None,
                    since: None,
                    reason: None,
                },
            ],
            boards: 2,
            max_concurrent: 2,
        }
    );
}

/// Gozer's own six-value state vocabulary must survive completely unchanged
/// -- including the hyphenated forms -- because these exact strings appear
/// verbatim in the `gozer-gatekeeper`/`gozer-keymaster` skills a human reads.
#[test]
fn snapshot_leases_preserves_every_state_string_verbatim() {
    let runner = FakeRunner::new(0);
    let payload = r#"{"grain":"board","chips":[
        {"dev_index":0,"bdf":"0000:01:00.0","board":"b1","state":"HELD","who":"a","reason":null},
        {"dev_index":1,"bdf":"0000:02:00.0","board":"b1","state":"HELD-FOREIGN","who":"b","reason":null},
        {"dev_index":2,"bdf":"0000:03:00.0","board":"b2","state":"STALE","who":"c","reason":null},
        {"dev_index":3,"bdf":"0000:04:00.0","board":"b2","state":"BUSY-UNTRACKED","who":null,"reason":null}],
        "queue":[]}"#;
    runner.set_run_capturing("status", 0, payload, "");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    let snapshot = gozer::snapshot_leases(&runner, &capability).expect("must parse");
    let states: Vec<&str> = snapshot
        .leases
        .iter()
        .map(|l| l.state.as_str())
        .collect();
    assert_eq!(states, vec!["HELD", "HELD-FOREIGN", "STALE", "BUSY-UNTRACKED"]);
}

/// When gozer's grain is `"chip"` (not this box's `"board"`), each chip is
/// its own leasable unit, so `max_concurrent` must count CHIPS, not distinct
/// boards -- `boards` still reports the (lower) board count either way,
/// since that's a hardware fact independent of the leasing grain.
#[test]
fn snapshot_leases_counts_max_concurrent_by_chip_when_grain_is_chip() {
    let runner = FakeRunner::new(0);
    let payload = r#"{"grain":"chip","chips":[
        {"dev_index":0,"bdf":"0000:01:00.0","board":"b1","state":"FREE","who":null,"reason":null},
        {"dev_index":1,"bdf":"0000:02:00.0","board":"b1","state":"FREE","who":null,"reason":null}],
        "queue":[]}"#;
    runner.set_run_capturing("status", 0, payload, "");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    let snapshot = gozer::snapshot_leases(&runner, &capability).expect("must parse");
    assert_eq!(snapshot.boards, 1, "1 distinct board serial");
    assert_eq!(snapshot.max_concurrent, 2, "2 chips, chip-grain");
}

/// A `status` call that can't even run degrades to `None`, never a panic --
/// same "gozer misbehaving is never fatal" contract as `probe`/`acquire`.
#[test]
fn snapshot_leases_returns_none_when_status_command_fails_to_run() {
    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("status", "No such file or directory");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    assert_eq!(gozer::snapshot_leases(&runner, &capability), None);
}

/// A non-zero exit from `gozer status` (its own health signal -- e.g. a
/// stuck mutex) also yields `None` rather than a stale/partial snapshot.
#[test]
fn snapshot_leases_returns_none_when_status_exits_nonzero() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("status", 16, "", "mutex stuck");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    assert_eq!(gozer::snapshot_leases(&runner, &capability), None);
}

/// Malformed JSON on an exit-0 response is also `None`, not a panic.
#[test]
fn snapshot_leases_returns_none_on_malformed_json() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("status", 0, "not json {", "");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };

    assert_eq!(gozer::snapshot_leases(&runner, &capability), None);
}
