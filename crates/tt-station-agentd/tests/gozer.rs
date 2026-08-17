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
use tt_station_agentd::gozer::{self, Capability, ForeignLeases, Grant, LeaseSnapshot, Outcome};
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
            // Absent from this payload -- `#[serde(default)]`, see `Grant::env`.
            env: Default::default(),
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
        env: Default::default(),
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

// ---------------------------------------------------------------------
// The startup sweep (Task 5). On startup agentd releases every
// `tt-station:`-prefixed lease whose SERVICE PORT has nothing serving on
// it -- the one reconciliation rule in the design doc's "One startup
// sweep" section. One rule, one direction: the reverse case (a container
// with no lease) is covered by `--owner-pid` + `BUSY-UNTRACKED`.
//
// Every test here drives `gozer::startup_sweep` through a fake
// `CommandRunner`, and asserts on the RECORDED ARGV (the side channel),
// never on the return value alone: a sweep that returned the right report
// while shelling out `gozer release` on somebody else's lease would be the
// worst possible pass.
// ---------------------------------------------------------------------

/// `gozer status --json` with one board held by a tt-station lease for
/// service port 8080, and one board free.
const STATUS_JSON_TT_STATION_HELD: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"tt-station:8080:meta-llama/Llama-3.3-70B-Instruct",
     "pid":4242,"reason":"serving via tt-station-agentd","pids_holding":[],"overstayed":false},
    {"dev_index":1,"bdf":"0000:02:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"tt-station:8080:meta-llama/Llama-3.3-70B-Instruct",
     "pid":4242,"reason":"serving via tt-station-agentd","pids_holding":[],"overstayed":false},
    {"dev_index":2,"bdf":"0000:03:00.0","board":"0100014311601048","card":"p300",
     "state":"FREE","who":null,"pid":null,"reason":null,"pids_holding":[],"overstayed":false}],
    "queue":[]}"#;

/// `gozer history --json` showing that same lease still open: a `granted`
/// record with no later `released`/`reaped` for the same id. This is the
/// ONLY place a lease id can be read from (gozer's `status --json` carries
/// `who` but no `lease_id`) -- see `gozer::startup_sweep`'s doc comment.
const HISTORY_JSON_OPEN_TT_STATION_LEASE: &str = r#"{"history":[
    {"ts":"2026-08-16T10:00:00Z","event":"granted","lease_id":"ab12ef",
     "who":"tt-station:8080:meta-llama/Llama-3.3-70B-Instruct","chips":2},
    {"ts":"2026-08-16T10:05:00Z","event":"granted","lease_id":"cc99aa",
     "who":"claude:ttm-optimize","chips":2}]}"#;

/// `docker ps` output (the `discover_serving` format: id/image/ports/name)
/// with a container publishing host port 8080.
const DOCKER_PS_SERVING_8080: &str = "c1\tghcr.io/tenstorrent/tt-inference-server:rel\t0.0.0.0:8080->8000/tcp\ttt-agent-llama";

/// The serving port this agent is configured for in the sweep tests -- the
/// same one `STATUS_JSON_TT_STATION_HELD`'s `who` names, so a KEPT lease
/// there is one of OURS and lands in `SweepReport::adoptable`.
const SWEEP_SERVING_PORT: u16 = 8080;

fn sweep_capability() -> Capability {
    Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    }
}

/// A `FakeRunner` canned for the sweep: a `docker ps` answer, gozer
/// `status`, gozer `history`, and a successful `release`.
fn sweep_runner(docker_ps: &str) -> FakeRunner {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", docker_ps);
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_TT_STATION_HELD, "");
    runner.set_run_capturing("gozer history", 0, HISTORY_JSON_OPEN_TT_STATION_LEASE, "");
    runner.set_run_capturing(
        "gozer release",
        0,
        r#"{"released":true,"message":"released ab12ef"}"#,
        "",
    );
    runner
}

/// Every `gozer release <id>` invocation recorded by `runner`, as the id
/// it targeted -- the side channel that proves what the sweep actually did
/// to the box, rather than what it reported having done.
fn released_lease_ids(runner: &FakeRunner) -> Vec<String> {
    runner
        .commands()
        .iter()
        .filter(|cmd| {
            cmd.first().map(String::as_str) == Some("gozer")
                && cmd.get(1).map(String::as_str) == Some("release")
        })
        .filter_map(|cmd| cmd.get(2).cloned())
        .collect()
}

/// THE SWEEP, RELEASING HALF. A `tt-station:` lease naming service port
/// 8080, and NOTHING published on 8080 -- the agent that took it is gone.
/// The lease must be released (which is what resets its chips and hands
/// the board back), named by the id resolved from `gozer history`.
#[test]
fn startup_sweep_releases_a_tt_station_lease_whose_port_has_nothing_serving() {
    // A running container, but on a DIFFERENT port -- proves the sweep
    // matches on the lease's own port and not merely on "docker has
    // something running".
    let runner = sweep_runner(
        "c9\tghcr.io/tenstorrent/tt-inference-server:rel\t0.0.0.0:9001->8000/tcp\tsomeone-else",
    );

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert_eq!(
        released_lease_ids(&runner),
        vec!["ab12ef".to_string()],
        "exactly the stale tt-station lease must be released: {:?}",
        runner.commands()
    );
    assert_eq!(report.released, vec!["ab12ef".to_string()]);
    assert!(report.kept.is_empty(), "nothing to keep: {report:?}");
}

/// THE SWEEP, LEAVING THE OTHER HALF ALONE. The same lease, but something
/// IS published on 8080 -- a live session that survived an agentd restart.
/// Releasing it would reset the chips under a running model, so the sweep
/// must issue no `gozer release` at all.
#[test]
fn startup_sweep_leaves_a_tt_station_lease_whose_port_is_serving() {
    let runner = sweep_runner(DOCKER_PS_SERVING_8080);

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "a lease whose port is still serving must NEVER be released -- that \
         resets a live model's chips: {:?}",
        runner.commands()
    );
    assert_eq!(report.kept, vec!["ab12ef".to_string()]);
    // IMPORTANT 4: a kept lease on THIS agent's own serving port belongs to a
    // previous process of this same agent, and the freshly-built backend knows
    // nothing about it. It must come back as ADOPTABLE -- with its `who`, which
    // is how the docker backend reconstructs the container name it has to stop
    // before releasing -- or nothing will ever release it, and gozer's reap
    // does NOT reset chips.
    assert_eq!(
        report.adoptable,
        vec![gozer::AdoptableLease {
            lease_id: "ab12ef".to_string(),
            who: "tt-station:8080:meta-llama/Llama-3.3-70B-Instruct".to_string(),
        }],
        "a kept lease on our own port must be offered for adoption: {report:?}"
    );
}

/// The other half of adoption's boundary: a KEPT `tt-station:` lease naming a
/// DIFFERENT service port is not ours to adopt. It belongs to a second agentd
/// on this box (or to a differently-configured one), and recording it would
/// make our `/stop` release -- and reset -- a board somebody else is serving
/// on. Kept, never adopted.
#[test]
fn startup_sweep_does_not_offer_another_agents_kept_lease_for_adoption() {
    let runner = sweep_runner(DOCKER_PS_SERVING_8080);

    // This agent serves on 9090; the live lease names 8080.
    let report = gozer::startup_sweep(&runner, &sweep_capability(), 9090);

    assert_eq!(
        report.kept,
        vec!["ab12ef".to_string()],
        "still kept -- something is serving on its port: {report:?}"
    );
    assert!(
        report.adoptable.is_empty(),
        "a lease on another agent's serving port must never be adopted: {report:?}"
    );
    assert!(
        released_lease_ids(&runner).is_empty(),
        "and it must certainly not be released: {:?}",
        runner.commands()
    );
}

/// A lease held by someone who is NOT tt-station is never a sweep
/// candidate, whatever docker is or isn't running. `claude:ttm-optimize`
/// holds a board with no published port anywhere -- exactly the shape that
/// would be swept if the `tt-station:` filter were dropped.
#[test]
fn startup_sweep_never_touches_a_foreign_lease() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "");
    runner.set_run_capturing(
        "gozer status",
        0,
        r#"{"grain":"board","chips":[
            {"dev_index":0,"bdf":"0000:01:00.0","board":"b1","state":"CLAIMED",
             "who":"claude:ttm-optimize","reason":"bringup"}],"queue":[]}"#,
        "",
    );
    runner.set_run_capturing(
        "gozer history",
        0,
        r#"{"history":[{"ts":"t","event":"granted","lease_id":"cc99aa",
             "who":"claude:ttm-optimize"}]}"#,
        "",
    );

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "a foreign lease must never be released by the sweep: {:?}",
        runner.commands()
    );
    assert!(report.released.is_empty() && report.kept.is_empty());
}

/// A lease already CLOSED in history (`granted` then `released`) must not
/// be released again: gozer reuses lease ids, so a stale id can name
/// somebody else's live lease by the time the sweep runs. Here status
/// still shows the `who` as holding -- so the ONLY thing stopping a
/// re-release is the history bookkeeping.
#[test]
fn startup_sweep_ignores_a_lease_id_that_history_shows_already_released() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "");
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_TT_STATION_HELD, "");
    runner.set_run_capturing(
        "gozer history",
        0,
        r#"{"history":[
            {"ts":"t1","event":"granted","lease_id":"ab12ef",
             "who":"tt-station:8080:meta-llama/Llama-3.3-70B-Instruct"},
            {"ts":"t2","event":"released","lease_id":"ab12ef"}]}"#,
        "",
    );

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "a lease id history says is already gone must not be released again -- \
         gozer reuses ids: {:?}",
        runner.commands()
    );
    assert_eq!(
        report.unresolved.len(),
        1,
        "the holder is still reported, but with no id to act on: {report:?}"
    );
}

/// An UNREADABLE `docker ps` means the sweep cannot know whether anything
/// is serving, so it must do NOTHING -- never release on a guess. The
/// failure direction is always "leave the lease alone".
#[test]
fn startup_sweep_does_nothing_when_docker_is_unreadable() {
    let runner = sweep_runner("");
    runner.fail_run("docker ps", "docker: command not found");

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "no release may happen while docker's state is unknown: {:?}",
        runner.commands()
    );
    assert!(report.released.is_empty());
    assert!(
        !runner
            .commands()
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("gozer")),
        "the sweep must not even ask gozer anything once docker is unreadable: {:?}",
        runner.commands()
    );
}

/// An unreadable `gozer status` (non-zero exit -- e.g. a stuck mutex) is
/// the same story: no releases, and no `history` call either.
///
/// NOTE the runner is built from scratch rather than by overriding
/// `sweep_runner`'s canned `status`: `FakeRunner` matches canned responses
/// in INSERTION order, first match wins, so a later registration for the
/// same matcher is silently ignored -- and this test would then pass while
/// exercising the healthy-status path.
#[test]
fn startup_sweep_does_nothing_when_gozer_status_is_unreadable() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "");
    runner.set_run_capturing("gozer status", 16, "", "mutex stuck");
    runner.set_run_capturing("gozer history", 0, HISTORY_JSON_OPEN_TT_STATION_LEASE, "");
    runner.set_run_capturing("gozer release", 0, r#"{"released":true}"#, "");

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(released_lease_ids(&runner).is_empty());
    assert!(report.released.is_empty());
    assert!(
        !runner.commands().iter().any(|cmd| {
            cmd.first().map(String::as_str) == Some("gozer")
                && cmd.get(1).map(String::as_str) == Some("history")
        }),
        "no point reading history when the live lease state is unknown: {:?}",
        runner.commands()
    );
}

/// A `who` that doesn't carry a parseable service port can't be swept --
/// there is no port to check -- so it is reported, never guessed at.
#[test]
fn startup_sweep_reports_but_never_releases_a_who_with_no_parseable_port() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "");
    runner.set_run_capturing(
        "gozer status",
        0,
        r#"{"grain":"board","chips":[
            {"dev_index":0,"bdf":"0000:01:00.0","board":"b1","state":"CLAIMED",
             "who":"tt-station:not-a-port:llama3","reason":null}],"queue":[]}"#,
        "",
    );
    runner.set_run_capturing(
        "gozer history",
        0,
        r#"{"history":[{"ts":"t","event":"granted","lease_id":"ab12ef",
             "who":"tt-station:not-a-port:llama3"}]}"#,
        "",
    );

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "an unparseable who must never be swept: {:?}",
        runner.commands()
    );
    assert_eq!(report.unresolved.len(), 1, "{report:?}");
}

/// The `who` format is a shared contract with both serving backends
/// (`tt-station:<service_port>:<model>`), and the model half may itself
/// contain colons (`org/model:tag`) -- the port is the SECOND field, not
/// "everything after the last colon".
#[test]
fn service_port_is_parsed_from_the_second_who_field() {
    assert_eq!(gozer::who_service_port("tt-station:8080:llama3"), Some(8080));
    assert_eq!(
        gozer::who_service_port("tt-station:8003:ghcr.io/org/model:0.14"),
        Some(8003)
    );
    // Not one of ours.
    assert_eq!(gozer::who_service_port("claude:ttm-optimize"), None);
    // Ours, but malformed.
    assert_eq!(gozer::who_service_port("tt-station:llama3"), None);
    assert_eq!(gozer::who_service_port("tt-station::llama3"), None);
    assert_eq!(gozer::who_service_port("tt-station:99999999:m"), None);
}

// ---------------------------------------------------------------------
// `foreign_leases` -- the guard both whole-box `tt-smi -r` paths
// (`POST /reset` and `POST /power reset-chips`) refuse on. Ownership is
// decided by the `who` prefix `tt-station:<our service port>:`.
// ---------------------------------------------------------------------

/// A lease naming OUR OWN service port is ours, whatever model it names --
/// it must not block our own reset. A lease naming a DIFFERENT port, or
/// anyone else entirely, is foreign and must be named. `FREE` chips are
/// never holders, and two chips of one board yield ONE clause.
#[test]
fn foreign_leases_names_everyone_but_our_own_service_port() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "gozer status",
        0,
        r#"{"grain":"board","chips":[
            {"dev_index":0,"bdf":"0000:01:00.0","board":"board-a","state":"CLAIMED",
             "who":"tt-station:8080:llama3","reason":"serving"},
            {"dev_index":1,"bdf":"0000:02:00.0","board":"board-a","state":"CLAIMED",
             "who":"tt-station:8080:llama3","reason":"serving"},
            {"dev_index":2,"bdf":"0000:03:00.0","board":"board-b","state":"CLAIMED",
             "who":"claude:ttm-optimize","reason":"bringup"},
            {"dev_index":3,"bdf":"0000:04:00.0","board":"board-b","state":"CLAIMED",
             "who":"claude:ttm-optimize","reason":"bringup"},
            {"dev_index":4,"bdf":"0000:05:00.0","board":"board-c","state":"FREE",
             "who":null,"reason":null}],"queue":[]}"#,
        "",
    );

    let ForeignLeases::Held(holders) = gozer::foreign_leases(&runner, &sweep_capability(), 8080)
    else {
        panic!("a foreign lease is held");
    };
    assert_eq!(
        holders,
        "board board-b is held by claude:ttm-optimize (CLAIMED)",
        "our own lease and the FREE board must not appear, one board must \
         yield one clause however many of its chips are held, and the STATE \
         must be carried verbatim -- `refusal_reason` needs the operator to \
         tell a live holder from a STALE/HELD-FOREIGN one, whose remedy is \
         `gozer reconcile` rather than stopping a session that is already gone"
    );

    // Same payload, a DIFFERENT service port: now the `tt-station:8080:`
    // lease is somebody else's too.
    let ForeignLeases::Held(holders) = gozer::foreign_leases(&runner, &sweep_capability(), 9999)
    else {
        panic!("both leases are foreign to port 9999");
    };
    assert!(holders.contains("tt-station:8080:llama3"), "{holders}");
    assert!(holders.contains("claude:ttm-optimize"), "{holders}");
}

/// A `BUSY-UNTRACKED` chip is NOT a lease -- no `who`, nobody to name -- so
/// it must not read as a foreign holder. Untracked work wedging the box is
/// one of the main reasons an operator resets it.
#[test]
fn foreign_leases_ignores_busy_untracked_chips() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "gozer status",
        0,
        r#"{"grain":"board","chips":[
            {"dev_index":0,"bdf":"0000:01:00.0","board":"board-a",
             "state":"BUSY-UNTRACKED","who":null,"reason":null}],"queue":[]}"#,
        "",
    );

    assert_eq!(
        gozer::foreign_leases(&runner, &sweep_capability(), 8080),
        ForeignLeases::None
    );
}

/// FAILS CLOSED: an unreadable `gozer status` is `Undetermined`, which is a
/// THIRD outcome and not `None`. "Nobody else is on this box" and "I cannot
/// tell whether anybody else is on this box" must never collapse into the
/// same value -- the caller refuses on the second, and can only do that if
/// the two are distinguishable here.
#[test]
fn foreign_leases_is_undetermined_when_status_is_unreadable() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("gozer status", 16, "", "mutex stuck");
    assert_eq!(
        gozer::foreign_leases(&runner, &sweep_capability(), 8080),
        ForeignLeases::Undetermined
    );
}

/// A payload with NO `chips` array at all must be `Undetermined`, not
/// `None`.
///
/// This is the schema-shaped path back to fail-open: while `chips` carried
/// `#[serde(default)]`, `{"grain":"board"}` (or any future gozer payload
/// that renames or nests that array) parsed cleanly to an empty `Vec`,
/// `snapshot_leases` answered `Some(empty)`, the guard read "nobody is on
/// this box", and both whole-box `tt-smi -r` paths ran. That is exactly the
/// collapse `Undetermined` exists to prevent -- "I could not understand the
/// answer" becoming "nobody is here" -- arriving through the schema instead
/// of the exit code.
///
/// Forward-compatibility for UNKNOWN fields stays (serde ignores them); it
/// just does not extend to the one field the guard's whole decision rests on.
#[test]
fn foreign_leases_is_undetermined_when_the_payload_has_no_chips_array() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("gozer status", 0, r#"{"grain":"board"}"#, "");
    assert_eq!(
        gozer::foreign_leases(&runner, &sweep_capability(), 8080),
        ForeignLeases::Undetermined,
        "a payload whose chips array could not be read is UNKNOWN, never empty"
    );
}

/// The same payload through `snapshot_leases` is `None` -- the single place
/// the decision is made, so `GET /leases` (which absorbs `None` via
/// `unwrap_or_default`) and the guard cannot disagree about it.
#[test]
fn snapshot_leases_returns_none_when_the_payload_has_no_chips_array() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("status", 0, r#"{"grain":"board"}"#, "");
    let capability = Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    };
    assert_eq!(gozer::snapshot_leases(&runner, &capability), None);
}

/// An EMPTY chips array, on the other hand, is a real answer -- gozer said
/// it looked and found nothing -- and must stay `None`/`Clear`. Without
/// this, "fail closed on a missing array" could be over-applied into
/// refusing on a genuinely idle box.
#[test]
fn foreign_leases_is_none_when_chips_is_present_and_empty() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("gozer status", 0, r#"{"grain":"board","chips":[]}"#, "");
    assert_eq!(
        gozer::foreign_leases(&runner, &sweep_capability(), 8080),
        ForeignLeases::None
    );
}

/// The refusal REASON differs by outcome, and says which situation the
/// operator is in: someone holds chips (stop them), or gozer could not be
/// read (fix gozer / reset by hand). A guard that refused with the same
/// words for both would leave an operator unable to tell a contended box
/// from a broken one.
#[test]
fn foreign_leases_refusal_reason_distinguishes_held_from_undetermined() {
    let held = ForeignLeases::Held("board b1 is held by claude:x (STALE)".to_string())
        .refusal_reason()
        .expect("Held must refuse");
    assert!(held.contains("claude:x"), "{held}");
    // "Stop that session" is the whole answer only for a LIVE holder. A STALE
    // or HELD-FOREIGN lease has no tenant left to ask, and `gozer status`
    // reports no lease id to pass to `gozer release` -- so without
    // `gozer reconcile` in the remedy, the guard blocks the reset exactly when
    // the box is wedged and nobody is there to unblock it.
    assert!(
        held.contains("STALE") && held.contains("HELD-FOREIGN"),
        "the reason must name the states whose tenant is already gone: {held}"
    );
    assert!(
        held.contains("gozer reconcile"),
        "and it must point at the one command that clears such a lease \
         without a lease id: {held}"
    );

    let unknown = ForeignLeases::Undetermined
        .refusal_reason()
        .expect("Undetermined must refuse");
    assert!(
        unknown.contains("gozer status") && unknown.contains("could not be read"),
        "the reason must say gozer could not be read, not that a lease exists: {unknown}"
    );
    assert!(
        !unknown.contains("is held by"),
        "an undetermined guard must not imply anyone holds anything: {unknown}"
    );
    // The capability is probed ONCE at startup, so a gozer uninstalled
    // afterwards leaves a live agentd refusing every reset with a message
    // about fixing gozer -- correct when it is broken, a dead end when it
    // was removed on purpose. The remedy for that case (restart agentd) has
    // to be in the message, or the refusal reads as permanent.
    assert!(
        unknown.contains("restart"),
        "the advice must cover a gozer that was UNINSTALLED, not just a broken \
         one -- restarting agentd re-probes and turns leasing off: {unknown}"
    );

    assert_eq!(
        ForeignLeases::None.refusal_reason(),
        None,
        "nothing foreign held -> nothing to refuse"
    );
}

/// gozer logs a `reaped` record more than once for the same lease (seen in
/// the real `history.jsonl` on this box: two identical `reaped` lines per
/// lease). The bookkeeping must be idempotent about that -- and a `reaped`
/// for a lease never seen `granted` in the window must not resurrect it.
#[test]
fn startup_sweep_tolerates_duplicate_reaped_records() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "");
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_TT_STATION_HELD, "");
    runner.set_run_capturing(
        "gozer history",
        0,
        r#"{"history":[
            {"ts":"t0","event":"reaped","lease_id":"deadbe","who":"claude:x"},
            {"ts":"t1","event":"granted","lease_id":"ab12ef",
             "who":"tt-station:8080:meta-llama/Llama-3.3-70B-Instruct"},
            {"ts":"t2","event":"reaped","lease_id":"ab12ef","who":"tt-station:8080:x"},
            {"ts":"t3","event":"reaped","lease_id":"ab12ef","who":"tt-station:8080:x"}]}"#,
        "",
    );

    let report = gozer::startup_sweep(&runner, &sweep_capability(), SWEEP_SERVING_PORT);

    assert!(
        released_lease_ids(&runner).is_empty(),
        "a reaped lease stays closed however many times it is logged: {:?}",
        runner.commands()
    );
    assert!(report.released.is_empty());
}
