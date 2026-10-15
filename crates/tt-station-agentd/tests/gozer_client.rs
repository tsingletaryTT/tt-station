// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The `gozer` client's verbs and verdicts (`src/gozer.rs`) that
//! `tests/gozer.rs` doesn't reach: the exact `acquire` argv and its outcomes,
//! `release`'s exit-code contract, the contention message (our own lease
//! worded as ours), `ForeignLeases::resolve` with and without `--force`, the
//! `LeaseGuard` drop/disarm contract, and `Contention` surviving an `anyhow`
//! chain.
//!
//! Every call goes through `support::FakeRunner`: no real gozer is ever run.

mod support;

use support::FakeRunner;
use tt_station_agentd::gozer::{
    self, Capability, Contention, ForeignLeases, Grant, LeaseGuard, Outcome,
};

fn cap() -> Capability {
    Capability {
        path: "/opt/gozer/bin/gozer".to_string(),
        version: "gozer 0.2.0".to_string(),
    }
}

fn grant(id: &str) -> Grant {
    Grant {
        lease_id: id.to_string(),
        chips: vec!["0000:01:00.0".into()],
        dev_indices: vec![0],
        units: vec![],
        expanded: false,
        env: Default::default(),
    }
}

/// The ids of every `gozer release` the runner saw.
fn released(runner: &FakeRunner) -> Vec<String> {
    runner
        .commands()
        .iter()
        .filter(|c| c.get(1).map(String::as_str) == Some("release"))
        .filter_map(|c| c.get(2).cloned())
        .collect()
}

// ---------------------------------------------------------------- acquire

#[test]
fn acquire_never_queues_and_ties_the_lease_to_this_process() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "acquire",
        0,
        r#"{"lease_id":"L1","chips":["0000:01:00.0","0000:02:00.0"],"dev_indices":[0,1],
            "units":["board-a"],"expanded":true,"env":{"TT_VISIBLE_DEVICES":"0000:01:00.0,0000:02:00.0"}}"#,
        "",
    );
    let out = gozer::acquire(
        &runner,
        &cap(),
        "board-a",
        "tt-station:8080:Qwen/Qwen3-32B",
        "serving",
    );
    let pid = std::process::id().to_string();
    assert_eq!(
        runner.commands(),
        [[
            "/opt/gozer/bin/gozer",
            "acquire",
            "--chips",
            "board-a",
            "--owner-pid",
            pid.as_str(),
            "--who",
            "tt-station:8080:Qwen/Qwen3-32B",
            "--reason",
            "serving",
            "--no-queue",
            "--json",
        ]]
    );
    let Outcome::Granted(g) = out else {
        panic!("{out:?}")
    };
    assert_eq!(g.lease_id, "L1");
    assert_eq!(g.units, ["board-a"]);
    assert!(g.expanded);
    assert_eq!(
        g.env.get("TT_VISIBLE_DEVICES").map(String::as_str),
        Some("0000:01:00.0,0000:02:00.0")
    );
    assert_eq!(g.reset_target().unwrap(), "0000:01:00.0,0000:02:00.0");
}

#[test]
fn acquire_degrades_to_an_outcome_never_a_panic_or_an_error() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "acquire",
        12,
        r#"{"granted":false,"holder":"claude:x","since":"10:00"}"#,
        "",
    );
    assert_eq!(
        gozer::acquire(&runner, &cap(), "all", "w", "r"),
        Outcome::Unavailable {
            holder: Some("claude:x".into()),
            since: Some("10:00".into())
        }
    );

    let runner = FakeRunner::new(0);
    runner.set_run_capturing("acquire", 16, "", "gate mutex stuck");
    assert_eq!(
        gozer::acquire(&runner, &cap(), "all", "w", "r"),
        Outcome::Failed("gozer exited 16: gate mutex stuck".into())
    );

    // A grant missing a required field isn't a grant.
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("acquire", 0, r#"{"lease_id":"L","chips":[]}"#, "");
    assert!(matches!(
        gozer::acquire(&runner, &cap(), "all", "w", "r"),
        Outcome::Failed(m) if m.contains("didn't parse as a grant")
    ));

    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("acquire", "No such file or directory");
    assert!(matches!(
        gozer::acquire(&runner, &cap(), "all", "w", "r"),
        Outcome::Failed(m) if m.starts_with("could not run '/opt/gozer/bin/gozer acquire'")
    ));
}

// ---------------------------------------------------------------- release

#[test]
fn release_treats_an_already_gone_lease_as_done_and_refusals_as_errors() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("release", 0, r#"{"released":true}"#, "");
    gozer::release(&runner, &cap(), "L1").unwrap();
    assert_eq!(
        runner.commands(),
        [["/opt/gozer/bin/gozer", "release", "L1", "--json"]]
    );

    let runner = FakeRunner::new(0);
    runner.set_run_capturing("release", 13, "", "no such lease");
    gozer::release(&runner, &cap(), "L1").unwrap();

    // Refused: gozer's own message first, then stderr, then stdout.
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "release",
        15,
        r#"{"released":false,"message":"lease L1 is owned by pid 99"}"#,
        "ignored",
    );
    let e = gozer::release(&runner, &cap(), "L1")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("exit 15") && e.contains("lease L1 is owned by pid 99"),
        "{e}"
    );
    assert!(e.contains("NOT released or reset"), "{e}");
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("release", 15, "not json", "refused: busy");
    let e = gozer::release(&runner, &cap(), "L1")
        .unwrap_err()
        .to_string();
    assert!(e.contains("refused: busy"), "{e}");

    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("release", "spawn failed");
    let e = format!("{:#}", gozer::release(&runner, &cap(), "L1").unwrap_err());
    assert!(
        e.contains("could not run") && e.contains("spawn failed"),
        "{e}"
    );
}

// ---------------------------------------------------------------- who has the chips

#[test]
fn our_own_lease_is_named_as_ours_and_others_by_their_holder() {
    assert_eq!(gozer::who_service_port("tt-station:8080:m"), Some(8080));
    assert_eq!(gozer::who_service_port("tt-station:8080:"), None);
    assert_eq!(gozer::who_service_port("tt-station:http:m"), None);
    assert_eq!(gozer::who_service_port("claude:8080:m"), None);
    // A model id with colons in it stays whole.
    assert_eq!(
        gozer::own_lease_clause("tt-station:8080:ghcr.io/org/model:0.14", 8080).unwrap(),
        "this box is already serving ghcr.io/org/model:0.14 on port 8080 -- stop it first (`tt-station stop`)"
    );
    assert_eq!(gozer::own_lease_clause("tt-station:8081:m", 8080), None);
    assert_eq!(gozer::own_lease_clause("claude:optimize", 8080), None);

    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "status",
        0,
        r#"{"grain":"board","chips":[
            {"board":"B1","state":"CLAIMED","who":"tt-station:8080:Qwen/Qwen3-32B","bdf":"0000:01:00.0"},
            {"board":"B1","state":"CLAIMED","who":"tt-station:8080:Qwen/Qwen3-32B","bdf":"0000:02:00.0"},
            {"board":"B2","state":"HELD","who":"claude:ttm-optimize","bdf":"0000:03:00.0"},
            {"board":"B3","state":"BUSY-UNTRACKED","who":null,"bdf":"0000:04:00.0"},
            {"board":"B4","state":"FREE","who":null,"bdf":"0000:05:00.0"},
            {"state":"STALE","who":"someone"}]}"#,
        "",
    );
    assert_eq!(
        gozer::contention_detail(&runner, &cap(), 8080).unwrap(),
        "this box is already serving Qwen/Qwen3-32B on port 8080 -- stop it first (`tt-station stop`); \
         board B2 is held by claude:ttm-optimize; \
         board B3 is in use by an untracked process (no lease); \
         board (unknown board) is held by someone"
    );
    // Another agent's port is a stranger.
    assert!(gozer::contention_detail(&runner, &cap(), 9000)
        .unwrap()
        .starts_with("board B1 is held by tt-station:8080:Qwen/Qwen3-32B;"));
}

#[test]
fn contention_detail_is_best_effort() {
    for (code, stdout) in [(14, "{}"), (0, "not json"), (0, r#"{"grain":"board"}"#)] {
        let runner = FakeRunner::new(0);
        runner.set_run_capturing("status", code, stdout, "");
        assert_eq!(
            gozer::contention_detail(&runner, &cap(), 8080),
            None,
            "{code} {stdout}"
        );
    }
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "status",
        0,
        r#"{"chips":[{"board":"B","state":"FREE"}]}"#,
        "",
    );
    assert_eq!(gozer::contention_detail(&runner, &cap(), 8080), None);
    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("status", "gone");
    assert_eq!(gozer::contention_detail(&runner, &cap(), 8080), None);
}

// ---------------------------------------------------------------- whole-box reset guard

#[test]
fn force_overrides_a_refusal_and_always_leaves_a_note() {
    assert_eq!(ForeignLeases::None.resolve(false), Ok(None));
    assert_eq!(ForeignLeases::None.resolve(true), Ok(None));

    let held = ForeignLeases::Held {
        holders: "board B2 is held by claude:x (HELD)".into(),
        chips: vec!["0000:03:00.0".into(), "0000:04:00.0".into()],
    };
    let refused = held.resolve(false).unwrap_err();
    assert!(
        refused.contains("board B2 is held by claude:x (HELD)"),
        "{refused}"
    );
    assert!(refused.contains("--force"), "{refused}");
    let note = held.resolve(true).unwrap().unwrap();
    assert!(
        note.contains("0000:03:00.0,0000:04:00.0") && note.contains("claude:x"),
        "{note}"
    );
    let no_chips = ForeignLeases::Held {
        holders: "h".into(),
        chips: vec![],
    };
    assert!(no_chips
        .forced_override_note()
        .unwrap()
        .contains("(none reported)"));

    let unknown = ForeignLeases::Undetermined.resolve(false).unwrap_err();
    assert!(unknown.contains("could not be read") && unknown.contains("--force"));
    assert!(
        !unknown.starts_with("another tenant holds chips --"),
        "never reads like Held"
    );
    assert!(ForeignLeases::Undetermined
        .resolve(true)
        .unwrap()
        .unwrap()
        .contains("UNKNOWN"));
}

#[test]
fn foreign_leases_names_the_state_and_every_foreign_chip() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "status",
        0,
        r#"{"grain":"board","chips":[
            {"board":"B1","bdf":"0000:01:00.0","dev_index":0,"state":"CLAIMED","who":"tt-station:8080:m"},
            {"board":"B2","bdf":"0000:02:00.0","dev_index":1,"state":"STALE","who":"claude:gone"},
            {"board":"B2","bdf":"0000:03:00.0","dev_index":2,"state":"STALE","who":"claude:gone"}]}"#,
        "",
    );
    assert_eq!(
        gozer::foreign_leases(&runner, &cap(), 8080),
        ForeignLeases::Held {
            holders: "board B2 is held by claude:gone (STALE)".into(),
            chips: vec!["0000:02:00.0".into(), "0000:03:00.0".into()],
        }
    );
    let reason = gozer::foreign_leases(&runner, &cap(), 8080)
        .refusal_reason()
        .unwrap();
    assert!(reason.contains("gozer reconcile"), "{reason}");
}

// ---------------------------------------------------------------- the lease guard

#[test]
fn a_dropped_guard_releases_its_lease_and_a_disarmed_one_does_not() {
    let runner = FakeRunner::new(0);
    {
        let g = LeaseGuard::new(&runner, cap(), grant("L-drop"));
        assert_eq!(g.grant().lease_id, "L-drop");
    }
    assert_eq!(released(&runner), ["L-drop"]);

    let runner = FakeRunner::new(0);
    let id = LeaseGuard::new(&runner, cap(), grant("L-keep")).into_lease_id();
    assert_eq!(id, "L-keep");
    assert!(released(&runner).is_empty());
}

#[test]
fn a_guard_that_cannot_release_does_not_panic() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("release", 15, "", "refused");
    drop(LeaseGuard::new(&runner, cap(), grant("L-stuck")));
    let runner = FakeRunner::new(0);
    runner.fail_run_capturing("release", "spawn failed");
    drop(LeaseGuard::new(&runner, cap(), grant("L-gone")));
    // Released on unwind too.
    let runner = FakeRunner::new(0);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = LeaseGuard::new(&runner, cap(), grant("L-unwind"));
        panic!("the serve failed");
    }));
    assert!(result.is_err());
    assert_eq!(released(&runner), ["L-unwind"]);
}

#[test]
fn contention_is_found_anywhere_in_an_error_chain() {
    let e = anyhow::Error::new(Contention::new("board B2 is held by claude:x"))
        .context("runpy backend: cannot serve 'm'")
        .context("POST /run");
    assert!(e.chain().any(|c| c.is::<Contention>()));
    assert_eq!(
        e.chain()
            .find_map(|c| c.downcast_ref::<Contention>())
            .unwrap()
            .to_string(),
        "board B2 is held by claude:x"
    );
    assert!(!anyhow::anyhow!("plain failure")
        .chain()
        .any(|c| c.is::<Contention>()));
}

#[test]
fn probe_uses_the_configured_path_and_its_version() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("--version", 0, "gozer 0.2.0\n", "");
    assert_eq!(
        gozer::probe(&runner, Some("/srv/gozer")),
        Some(Capability {
            path: "/srv/gozer".into(),
            version: "gozer 0.2.0".into()
        })
    );
    assert_eq!(runner.commands(), [["/srv/gozer", "--version"]]);
    let runner = FakeRunner::new(0);
    runner.set_run_capturing("--version", 0, "  \n", "");
    assert_eq!(gozer::probe(&runner, Some("/srv/gozer")), None);
}
