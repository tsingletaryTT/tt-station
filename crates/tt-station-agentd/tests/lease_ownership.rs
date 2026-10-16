// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Chip leases owned by the WORKLOAD, not by this agent: the hand-off after a
//! serve comes up, the lease reconciler, and what an agent restart does to a
//! lease. Everything runs against the stateful `FakeGozerBox` and canned
//! `docker` answers -- never a real gozer, docker or chip.

mod support;

use support::{FakeGozerBox, DEAD_AGENT_PID};
use tt_station_agentd::gozer::{self, Capability, Handoff, OwnLease, ReconcileAction};
use tt_station_agentd::serving::docker::{DockerBackend, DockerConfig};
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

const PORT: u16 = 8080;
/// The serving container's main pid, as `docker inspect` reports it.
const CONTAINER_PID: u32 = 4242;
const WHO: &str = "tt-station:8080:model-a";

const TT_SMI_TWO_P300C: &str = r#"{
    "device_info": [
        {"board_info": {"board_type": "p300c"}},
        {"board_info": {"board_type": "p300c"}}
    ]
}"#;

fn capability() -> Capability {
    Capability {
        path: "gozer".to_string(),
        version: "gozer 0.3.3".to_string(),
    }
}

/// A one-board box whose serving port is published by container `c1`, main
/// pid `CONTAINER_PID`. The specific `docker ps --filter` answer is canned
/// before the general one (first match wins).
fn serving_box() -> FakeGozerBox {
    let gozer = FakeGozerBox::with_boards(1, 0);
    let runner = gozer.runner();
    runner.set_run_output("tt-smi -s", TT_SMI_TWO_P300C);
    runner.set_run_output(&format!("docker ps --filter publish={PORT} -q"), "c1\n");
    runner.set_run_output("docker inspect", &format!("{CONTAINER_PID}\n"));
    gozer
}

fn runpy(gozer: &FakeGozerBox) -> RunPyBackend {
    RunPyBackend::new(
        RunPyConfig {
            host: "127.0.0.1".to_string(),
            service_port: PORT,
            ..Default::default()
        },
        Box::new(gozer.clone()),
    )
    .with_gozer(Some(capability()))
}

fn gozer_calls(gozer: &FakeGozerBox, verb: &str) -> Vec<Vec<String>> {
    gozer
        .commands()
        .into_iter()
        .filter(|c| c.first().map(String::as_str) == Some("gozer"))
        .filter(|c| c.get(1).map(String::as_str) == Some(verb))
        .collect()
}

// ---------------------------------------------------------------------------
// The hand-off after a serve comes up
// ---------------------------------------------------------------------------

/// Without `gozer transfer` (gozer 0.3.x), the lease is re-taken for the
/// container: released WITHOUT a reset (the container is on those chips),
/// then acquired again for exactly the same chips with `--owner-pid` = the
/// container's main pid. The backend tracks the new id.
#[test]
fn runpy_hands_its_lease_to_the_serving_container() {
    let gozer = serving_box();
    let backend = runpy(&gozer);

    backend.start("model-a").expect("start");

    let held = gozer.held_lease_ids();
    assert_eq!(held.len(), 1, "one lease, re-taken: {:?}", gozer.commands());
    let lease = &held[0];
    assert_eq!(gozer.owner_of(lease), Some(CONTAINER_PID));
    assert_eq!(backend.held_lease().as_deref(), Some(lease.as_str()));
    assert_eq!(
        gozer.releases(),
        vec![("lease-1".to_string(), false)],
        "the agent-owned lease is let go WITHOUT a reset -- the container is \
         driving those chips"
    );
    let reacquire = gozer_calls(&gozer, "acquire").pop().expect("a re-acquire");
    // The lease was the whole (one-board) box, so the re-take asks for all
    // of it -- never a count gozer could satisfy with other chips.
    assert!(
        reacquire
            .windows(2)
            .any(|w| w[0] == "--chips" && w[1] == "all"),
        "the re-take must ask for exactly the same chips: {reacquire:?}"
    );
    assert!(
        reacquire
            .windows(2)
            .any(|w| w[0] == "--owner-pid" && w[1] == CONTAINER_PID.to_string()),
        "owned by the container: {reacquire:?}"
    );

    // Stopping releases the NEW id, with a reset, after the container stops.
    backend.stop("model-a").expect("stop");
    assert!(gozer.held_lease_ids().is_empty());
    assert_eq!(gozer.releases().last(), Some(&(lease.clone(), true)));
}

/// With `gozer transfer`, the hand-off is one atomic call: same id, no
/// release, no window where the chips are unlocked.
#[test]
fn transfer_is_used_when_gozer_has_it() {
    let gozer = serving_box().with_transfer();
    let backend = runpy(&gozer);

    backend.start("model-a").expect("start");

    assert_eq!(gozer.held_lease_ids(), vec!["lease-1".to_string()]);
    assert_eq!(gozer.owner_of("lease-1"), Some(CONTAINER_PID));
    assert!(gozer.releases().is_empty(), "{:?}", gozer.commands());
    assert_eq!(gozer_calls(&gozer, "acquire").len(), 1);
}

/// gozer `--json` reports a refused transfer on stdout with stderr empty: the
/// log says gozer's own message, not nothing.
#[test]
fn a_refused_transfer_says_gozer_s_message() {
    let runner = support::FakeRunner::new(0);
    runner.set_run_capturing(
        "gozer transfer",
        1,
        r#"{"transferred":false,"message":"lease lease-9 belongs to another user"}"#,
        "",
    );
    let got = gozer::hand_off(
        &runner,
        &capability(),
        "lease-9",
        &["0000:01:00.0".to_string()],
        WHO,
        "test",
        CONTAINER_PID,
    );
    assert_eq!(
        got,
        Handoff::Kept("gozer transfer exited 1: lease lease-9 belongs to another user".to_string())
    );
}

/// While anyone is queued, the release/re-take is not attempted: the queued
/// tenant is exactly who would win the gap. The lease stays this agent's,
/// for the reconciler to hand over later.
#[test]
fn no_handoff_while_a_tenant_is_queued() {
    let gozer = serving_box();
    gozer.set_queued(1);
    let backend = runpy(&gozer);

    backend.start("model-a").expect("start");

    assert_eq!(gozer.held_lease_ids(), vec!["lease-1".to_string()]);
    assert_eq!(gozer.owner_of("lease-1"), Some(std::process::id()));
    assert!(gozer.releases().is_empty());

    // The queue drains; the reconciler finishes the job.
    gozer.set_queued(0);
    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, std::process::id())
        .expect("reconcile ran");
    assert_eq!(report.rebound.len(), 1, "{report:?}");
    let (old, new) = &report.rebound[0];
    assert!(backend.swap_held_lease(old, Some(new)));
    assert_eq!(gozer.owner_of(new), Some(CONTAINER_PID));
}

/// The docker backend knows its container by name and hands off the same way.
#[test]
fn docker_backend_hands_its_lease_to_its_container() {
    let gozer = serving_box().with_transfer();
    let backend = DockerBackend::new(
        DockerConfig {
            image: "some/image:tag".to_string(),
            host: "127.0.0.1".to_string(),
            host_port: PORT,
            ..Default::default()
        },
        Box::new(gozer.clone()),
    )
    .with_gozer(Some(capability()));

    backend.start("model-a").expect("start");

    assert_eq!(gozer.owner_of("lease-1"), Some(CONTAINER_PID));
    assert!(gozer
        .commands()
        .iter()
        .any(|c| c.first().map(String::as_str) == Some("docker")
            && c.get(1).map(String::as_str) == Some("inspect")
            && c.last().is_some_and(|n| n.contains("model-a"))));
}

// ---------------------------------------------------------------------------
// The reconciler
// ---------------------------------------------------------------------------

fn lease(id: &str, owner: Option<u32>, alive: bool) -> OwnLease {
    OwnLease {
        lease_id: id.to_string(),
        who: WHO.to_string(),
        chips: vec!["0000:01:00.0".to_string(), "0000:02:00.0".to_string()],
        owner_pid: owner,
        owner_alive: alive,
    }
}

#[test]
fn plan_covers_both_unowned_cases_and_leaves_live_owners_alone() {
    let agent = 100;
    // Owner dead, workload alive: re-attach to the workload.
    let plan = gozer::plan_reconcile(&[lease("a", Some(7), false)], Some(4242), agent);
    assert!(matches!(
        &plan[..],
        [ReconcileAction::Rebind { to_pid: 4242, .. }]
    ));
    // Owner dead, nothing serving: release.
    let plan = gozer::plan_reconcile(&[lease("a", Some(7), false)], None, agent);
    assert!(matches!(&plan[..], [ReconcileAction::Release { .. }]));
    // Still this agent's while the container runs: hand it over.
    let plan = gozer::plan_reconcile(&[lease("a", Some(agent), true)], Some(4242), agent);
    assert!(matches!(&plan[..], [ReconcileAction::Rebind { .. }]));
    // Already the workload's: nothing.
    assert!(gozer::plan_reconcile(&[lease("a", Some(4242), true)], Some(4242), agent).is_empty());
    // A live owner that is neither (a holder unit's server): nothing, even
    // with no container on the port.
    assert!(gozer::plan_reconcile(&[lease("a", Some(55), true)], None, agent).is_empty());
    // This agent's own lease with no container yet (a start in flight): nothing.
    assert!(gozer::plan_reconcile(&[lease("a", Some(agent), true)], None, agent).is_empty());
}

/// "workload alive, lease owner dead" -- the shape every agent restart left
/// before this change -- is re-attached to the container.
#[test]
fn reconciler_reattaches_a_dead_owners_lease_to_the_live_container() {
    let gozer = serving_box();
    let stale = gozer.preexisting_lease(WHO);
    assert_eq!(gozer.owner_of(&stale), Some(DEAD_AGENT_PID));

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, std::process::id())
        .expect("reconcile ran");

    assert_eq!(report.rebound.len(), 1, "{report:?}");
    let new = &report.rebound[0].1;
    assert_eq!(gozer.owner_of(new), Some(CONTAINER_PID));
    assert!(
        gozer.releases().iter().all(|(_, reset)| !reset),
        "nothing may be reset under the live container: {:?}",
        gozer.releases()
    );
}

/// "lease with no live owner" and nothing serving: released, WITH a reset.
#[test]
fn reconciler_releases_a_lease_with_no_owner_and_no_workload() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    gozer
        .runner()
        .set_run_output(&format!("docker ps --filter publish={PORT} -q"), "");
    let stale = gozer.preexisting_lease(WHO);

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, std::process::id())
        .expect("reconcile ran");

    assert_eq!(report.released, vec![stale.clone()]);
    assert_eq!(gozer.releases(), vec![(stale, true)]);
    assert!(gozer.held_lease_ids().is_empty());
}

/// A start or stop that begins during a pass stops the pass before its next
/// action: nothing is released or handed over under it.
#[test]
fn a_reconcile_pass_stops_when_a_start_or_stop_begins() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    gozer
        .runner()
        .set_run_output(&format!("docker ps --filter publish={PORT} -q"), "");
    let stale = gozer.preexisting_lease(WHO);

    let report =
        gozer::reconcile_own_leases_while(&gozer, &capability(), PORT, std::process::id(), &|| {
            false
        })
        .expect("reconcile read the box");

    assert!(report.released.is_empty(), "{report:?}");
    assert!(gozer.releases().is_empty());
    assert_eq!(gozer.held_lease_ids(), vec![stale]);
}

/// While a start or stop is counted, a reconcile pass is skipped; a start or
/// stop waits out a pass already going.
#[tokio::test]
async fn starts_and_stops_and_reconcile_passes_never_overlap() {
    let ops = std::sync::Arc::new(gozer::ServeOps::default());
    let pass = ops.pass().await;
    let begun = {
        let ops = ops.clone();
        tokio::spawn(async move {
            let _op = ops.begin().await;
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!begun.is_finished(), "a start waits for the pass");
    assert!(!ops.quiet(), "but is counted at once, so the pass stops");
    drop(pass);
    begun.await.unwrap();
    assert!(ops.quiet());
}

/// Release on workload exit: the container that owns the lease exits out of
/// band (a crash, a manual `docker stop`). Its pid is gone and nothing
/// publishes the port any more, so the next pass releases the lease -- WITH a
/// reset, which gozer's own reap of a dead owner would not do -- and the
/// backend forgets it.
#[test]
fn a_lease_is_released_once_its_container_exits() {
    let gozer = serving_box().with_transfer();
    let backend = runpy(&gozer);
    backend.start("model-a").expect("start");
    assert_eq!(gozer.owner_of("lease-1"), Some(CONTAINER_PID));

    gozer.kill(CONTAINER_PID);
    gozer
        .runner()
        .replace_run_output(&format!("docker ps --filter publish={PORT} -q"), "");

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, std::process::id())
        .expect("reconcile ran");

    assert_eq!(report.released, vec!["lease-1".to_string()]);
    assert_eq!(gozer.releases(), vec![("lease-1".to_string(), true)]);
    assert!(backend.swap_held_lease("lease-1", None));
    assert_eq!(backend.held_lease(), None);
}

/// The exact exit path, without canning order getting in the way: a lease
/// owned by a container pid that died, with nothing on the port.
#[test]
fn a_dead_containers_lease_is_released_with_a_reset() {
    let gozer = FakeGozerBox::with_boards(1, 0).with_transfer();
    gozer
        .runner()
        .set_run_output(&format!("docker ps --filter publish={PORT} -q"), "");
    let id = gozer.preexisting_lease(WHO);
    // Hand it to a container, then let the container die.
    gozer.kill(DEAD_AGENT_PID);
    assert!(matches!(
        gozer::hand_off(&gozer, &capability(), &id, &[], WHO, "t", CONTAINER_PID),
        Handoff::Moved { .. }
    ));
    gozer.kill(CONTAINER_PID);

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, std::process::id())
        .expect("reconcile ran");

    assert_eq!(report.released, vec![id.clone()]);
    assert_eq!(gozer.releases(), vec![(id, true)]);
}

/// Nothing is done when docker cannot be read: "could not tell" must never
/// act like "nothing is serving" when the action resets chips.
#[test]
fn reconciler_does_nothing_when_docker_is_unreadable() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    gozer
        .runner()
        .fail_run("docker ps", "Cannot connect to the Docker daemon");
    gozer.preexisting_lease(WHO);

    assert!(gozer::reconcile_own_leases(&gozer, &capability(), PORT, 1).is_none());
    assert!(gozer.releases().is_empty());
    assert_eq!(gozer.held_lease_ids().len(), 1);
}

/// A neighbour's lease -- another port, another tenant -- is never touched.
#[test]
fn reconciler_never_touches_another_tenants_lease() {
    let gozer = FakeGozerBox::with_boards(2, 0);
    gozer
        .runner()
        .set_run_output(&format!("docker ps --filter publish={PORT} -q"), "");
    gozer.preexisting_lease("tt-station:9090:other");
    gozer.preexisting_lease("claude:ttm-optimize");

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, 1).expect("ran");

    assert_eq!(report, Default::default());
    assert!(gozer.releases().is_empty());
}

// ---------------------------------------------------------------------------
// Agent restarts
// ---------------------------------------------------------------------------

/// The point of the whole change: once the container owns the lease, an agent
/// restart leaves it CLAIMED (owner alive), the new process adopts it, the
/// reconciler has nothing to do, and the next stop releases it with a reset.
#[test]
fn an_agent_restart_keeps_a_container_owned_lease() {
    let gozer = serving_box().with_transfer();
    runpy(&gozer).start("model-a").expect("start");
    // The agent dies: everything it owned would be STALE. The container's
    // lease is not the agent's any more.
    gozer.kill(std::process::id());
    gozer.runner().set_run_output(
        "docker ps --format",
        "c1\timg\t127.0.0.1:8080->8080/tcp\ttt-inference-server-x",
    );

    // The restarted process: sweep, adopt, reconcile.
    let sweep = gozer::startup_sweep(&gozer, &capability(), PORT);
    assert!(sweep.released.is_empty(), "{sweep:?}");
    assert_eq!(sweep.adoptable.len(), 1, "{sweep:?}");
    let backend = runpy(&gozer);
    assert!(backend.adopt_lease(&sweep.adoptable[0].lease_id, &sweep.adoptable[0].who));
    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, 1).expect("ran");
    assert_eq!(report, Default::default(), "nothing to fix: {report:?}");
    assert_eq!(gozer.owner_of("lease-1"), Some(CONTAINER_PID));

    backend.stop("model-a").expect("stop");
    assert_eq!(gozer.releases(), vec![("lease-1".to_string(), true)]);
}

/// A lease from BEFORE this change (owned by the dead previous agent) whose
/// container still runs: kept by the sweep, then re-attached by the
/// reconciler, and the adopted id in the backend follows it.
#[test]
fn an_agent_restart_reattaches_an_old_agent_owned_lease() {
    let gozer = serving_box();
    let old = gozer.preexisting_lease(WHO);
    gozer.runner().set_run_output(
        "docker ps --format",
        "c1\timg\t127.0.0.1:8080->8080/tcp\ttt-inference-server-x",
    );

    let sweep = gozer::startup_sweep(&gozer, &capability(), PORT);
    assert_eq!(sweep.kept, vec![old.clone()]);
    let backend = runpy(&gozer);
    assert!(backend.adopt_lease(&old, WHO));

    let report = gozer::reconcile_own_leases(&gozer, &capability(), PORT, 1).expect("ran");
    let (was, new) = report.rebound.first().cloned().expect("re-attached");
    assert_eq!(was, old);
    assert!(backend.swap_held_lease(&was, Some(&new)));
    assert_eq!(gozer.owner_of(&new), Some(CONTAINER_PID));
    assert_eq!(backend.held_lease(), Some(new));
}

/// On a box with more than the lease (a neighbour may hold the other board),
/// the re-take pins the lease's own board with `--exact`.
#[test]
fn a_one_board_lease_on_a_bigger_box_is_re_taken_exactly() {
    let gozer = FakeGozerBox::with_boards(2, 0);
    gozer
        .runner()
        .set_run_output(&format!("docker ps --filter publish={PORT} -q"), "c1");
    gozer.runner().set_run_output("docker inspect", "4242");
    let id = gozer.preexisting_lease(WHO);

    let outcome = gozer::hand_off(
        &gozer,
        &capability(),
        &id,
        &["0000:01:00.0".to_string(), "0000:02:00.0".to_string()],
        WHO,
        "t",
        CONTAINER_PID,
    );

    let Handoff::Moved { lease_id } = outcome else {
        panic!("expected a hand-off: {outcome:?}");
    };
    assert_eq!(gozer.owner_of(&lease_id), Some(CONTAINER_PID));
    let reacquire = gozer_calls(&gozer, "acquire").pop().expect("a re-acquire");
    assert!(
        reacquire
            .windows(2)
            .any(|w| w[0] == "--exact" && w[1] == "0000:01:00.0"),
        "{reacquire:?}"
    );
}

// ---------------------------------------------------------------------------
// Holder units: the gate
// ---------------------------------------------------------------------------

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lease-ownership-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The gate execs the real command IN PLACE on `go` -- same pid, so a lease
/// taken on the unit's main pid meanwhile is the server's -- and exits 75
/// without running it otherwise.
#[test]
fn the_gate_runs_the_command_only_on_go_and_keeps_its_pid() {
    let dir = scratch("gate");
    for (go, expect) in [(true, Some(0)), (false, Some(75))] {
        let fifo = dir.join(format!("{go}.gate"));
        gozer::make_gate(&fifo).expect("mkfifo");
        let argv = gozer::gated_argv(
            &fifo,
            &["sh".to_string(), "-c".to_string(), "echo $$".to_string()],
        );
        let child = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the gate");
        let pid = child.id();
        gozer::signal_gate(&fifo, go).expect("signal the gate");
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), expect);
        if go {
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                pid.to_string(),
                "the command must run as the gate's own pid"
            );
        } else {
            assert!(
                out.stdout.is_empty(),
                "nothing may run behind a closed gate"
            );
        }
    }
}

#[test]
fn lease_files_swap_and_drop_by_id() {
    let file = gozer::holder_lease_file(&scratch("file"));
    file.add("a");
    file.add("b");
    assert!(file.swap("a", Some("c")));
    assert!(!file.swap("zz", None));
    assert_eq!(file.ids(), vec!["c".to_string(), "b".to_string()]);
    assert!(file.swap("b", None));
    assert_eq!(file.take(), vec!["c".to_string()]);
    assert!(file.ids().is_empty());
}

/// Changes from several threads at once all land, and the file is private.
#[test]
fn lease_file_changes_at_once_all_land() {
    let dir = scratch("file-race");
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                let file = gozer::holder_lease_file(&dir);
                for n in 0..25 {
                    file.add(&format!("lease-{t}-{n}"));
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let file = gozer::holder_lease_file(&dir);
    assert_eq!(file.ids().len(), 8 * 25, "no id was lost");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.join("holder-leases.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}
