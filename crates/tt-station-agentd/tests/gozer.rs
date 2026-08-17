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

use support::FakeRunner;
use tt_station_agentd::gozer::{self, Capability, Grant, Outcome};
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
