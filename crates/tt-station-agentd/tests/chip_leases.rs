// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Chip resets, which used to run without a gozer lease, now hold a
//! whole-box lease while they touch the chips -- see
//! `docs/reference/chip-leasing.md`.
//!
//! `POST /power reset-chips` reaches gozer through the real command runner, so
//! it runs against a STUB `gozer` script that keeps its state in a temp dir
//! (never the real gozer, never a chip). `POST /reset` goes through the
//! backend's injected runner, so it uses `FakeGozerBox`.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use support::{FakeGozerBox, FakeRunner};
use tt_station_agentd::gozer::{self, Capability, OwnLease, ReconcileAction};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

/// The tenant a "held" box is held by.
const NEIGHBOUR: &str = "claude:ttm-optimize";

fn capability(path: &str) -> Capability {
    Capability {
        path: path.to_string(),
        version: "gozer 0.1.0".to_string(),
    }
}

fn executable(path: &Path, script: &str) {
    std::fs::write(path, script).expect("write script");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod +x");
}

/// A one-board stand-in for `gozer`, stateful through files in `dir`:
/// `held` (who holds the box, written by `acquire`, removed by `release`),
/// `foreign` (a neighbour holds it), `calls` (one line per invocation).
fn stub_gozer(dir: &Path) -> PathBuf {
    let path = dir.join("gozer");
    let d = dir.display();
    executable(
        &path,
        &format!(
            r#"#!/bin/sh
D="{d}"
echo "$*" >> "$D/calls"
verb="$1"; shift
case "$verb" in
  status)
    if [ -f "$D/foreign" ]; then state=CLAIMED; who="\"{NEIGHBOUR}\""
    elif [ -f "$D/held" ]; then state=CLAIMED; who="\"$(cat "$D/held")\""
    else state=FREE; who=null; fi
    echo "{{\"grain\":\"board\",\"queue\":[],\"chips\":[
      {{\"dev_index\":0,\"bdf\":\"0000:01:00.0\",\"board\":\"board-0\",\"state\":\"$state\",\"who\":$who}},
      {{\"dev_index\":1,\"bdf\":\"0000:02:00.0\",\"board\":\"board-0\",\"state\":\"$state\",\"who\":$who}}]}}"
    ;;
  acquire)
    if [ -f "$D/foreign" ] || [ -f "$D/held" ]; then
      echo '{{"granted":false,"queued":false}}'; exit 12
    fi
    who=""
    while [ $# -gt 0 ]; do [ "$1" = "--who" ] && who="$2"; shift; done
    echo "$who" > "$D/held"
    echo '{{"granted":true,"lease_id":"lease-1","chips":["0000:01:00.0","0000:02:00.0"],"dev_indices":[0,1],"units":["board-0"]}}'
    ;;
  release)
    rm -f "$D/held"
    echo '{{"released":true,"message":"released"}}'
    ;;
esac
exit 0
"#
        ),
    );
    path
}

/// A reset command that writes whether the box was leased while it ran.
fn reset_marker(dir: &Path) -> PathBuf {
    let path = dir.join("reset");
    let d = dir.display();
    executable(
        &path,
        &format!(
            "#!/bin/sh\nif [ -f \"{d}/held\" ]; then echo \"$(cat {d}/held)\"; else echo unleased; fi > \"{d}/reset-ran\"\n"
        ),
    );
    path
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn state(dir: &Path, gozer: bool) -> AppState {
    let backend: Arc<dyn ServingBackend> = Arc::new(RunPyBackend::new(
        RunPyConfig::default(),
        Box::new(FakeRunner::new(0)),
    ));
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let state = if gozer {
        state.with_gozer(Some(capability(&stub_gozer(dir).to_string_lossy())))
    } else {
        state
    };
    state.with_power_config(
        vec![reset_marker(dir).to_string_lossy().into_owned()],
        vec!["/bin/true".to_string()],
        vec!["/bin/true".to_string()],
        vec!["/bin/true".to_string()],
    )
}

async fn serve(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn post(base: &str, token: &str, path: &str, body: serde_json::Value) -> (u16, String) {
    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("request");
    let status = resp.status().as_u16();
    (status, resp.text().await.unwrap_or_default())
}

fn reset_ran(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join("reset-ran"))
        .ok()
        .map(|s| s.trim().to_string())
}

// ---------------------------------------------------------------------------
// POST /power reset-chips (and every caller of run_power_command)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reset_chips_takes_a_lease_resets_and_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), true);
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    let (status, body) = post(
        &base,
        &token,
        "/power",
        serde_json::json!({ "action": "reset-chips" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let who = reset_ran(dir.path()).expect("the reset ran");
    assert!(
        who.starts_with("tt-station:") && who.ends_with(":reset-chips"),
        "the reset ran under its own whole-box lease: {who}"
    );
    let calls = calls(dir.path());
    let acquire = calls.iter().position(|c| c.starts_with("acquire")).unwrap();
    assert!(calls[acquire].contains("--chips all"), "{calls:?}");
    let release = calls
        .iter()
        .position(|c| c.starts_with("release"))
        .expect("a release");
    assert!(acquire < release);
    assert_eq!(
        calls[release], "release lease-1 --no-reset --json",
        "no second reset: the command just did it"
    );
    assert!(!dir.path().join("held").exists());
}

#[tokio::test]
async fn reset_chips_is_refused_when_someone_holds_the_chips() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foreign"), "").unwrap();
    let state = state(dir.path(), true);
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    let (status, body) = post(
        &base,
        &token,
        "/power",
        serde_json::json!({ "action": "reset-chips" }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains(NEIGHBOUR), "{body}");
    assert_eq!(reset_ran(dir.path()), None);
    assert!(!calls(dir.path()).iter().any(|c| c.starts_with("acquire")));
}

/// The box's own model holds the chips (its serve lease): a reset would pull
/// them out from under it, so it's refused -- forced or not -- until the
/// owner stops it.
#[tokio::test]
async fn reset_chips_is_refused_over_the_box_s_own_serve_lease() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), true);
    // The default serving port (8000), as a serve's lease names it.
    std::fs::write(
        dir.path().join("held"),
        gozer::own_who(8000, "llama3") + "\n",
    )
    .unwrap();
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    for force in [false, true] {
        let (status, body) = post(
            &base,
            &token,
            "/power",
            serde_json::json!({ "action": "reset-chips", "force": force }),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("stop it first"), "{body}");
    }
    assert_eq!(reset_ran(dir.path()), None);
    assert!(!calls(dir.path()).iter().any(|c| c.starts_with("acquire")));
}

#[tokio::test]
async fn a_forced_reset_over_a_holder_is_audited() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foreign"), "").unwrap();
    let state = state(dir.path(), true);
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    let (status, body) = post(
        &base,
        &token,
        "/power",
        serde_json::json!({ "action": "reset-chips", "force": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        reset_ran(dir.path()).as_deref(),
        Some("unleased"),
        "no lease can be taken over the holder's"
    );
    assert!(!calls(dir.path()).iter().any(|c| c.starts_with("acquire")));
    let (entries, _) = state.audit().page(0, 50);
    let entry = entries
        .iter()
        .find(|e| e.action == "power")
        .expect("an audit entry");
    assert_eq!(entry.result, "forced");
    assert!(entry.target.contains(NEIGHBOUR), "{entry:?}");
}

#[tokio::test]
async fn reset_chips_without_gozer_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let state = state(dir.path(), false);
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    let (status, body) = post(
        &base,
        &token,
        "/power",
        serde_json::json!({ "action": "reset-chips" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(reset_ran(dir.path()).as_deref(), Some("unleased"));
    assert!(calls(dir.path()).is_empty());
}

// ---------------------------------------------------------------------------
// POST /reset (the runpy backend's whole-box reset)
// ---------------------------------------------------------------------------

fn runpy(gozer: &FakeGozerBox) -> RunPyBackend {
    RunPyBackend::new(RunPyConfig::default(), Box::new(gozer.clone()))
        .with_gozer(Some(capability("gozer")))
}

fn position(commands: &[Vec<String>], pred: impl Fn(&[String]) -> bool) -> Option<usize> {
    commands.iter().position(|c| pred(c))
}

#[test]
fn box_reset_runs_tt_smi_under_a_whole_box_lease() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    let backend = runpy(&gozer);

    backend.reset().expect("reset");

    let commands = gozer.commands();
    let acquire = position(&commands, |c| {
        c.first().map(String::as_str) == Some("gozer")
            && c.get(1).map(String::as_str) == Some("acquire")
    })
    .expect("an acquire");
    assert!(
        commands[acquire]
            .windows(2)
            .any(|w| w[0] == "--who" && w[1].ends_with(":reset-chips")),
        "{:?}",
        commands[acquire]
    );
    let reset = position(&commands, |c| c == ["tt-smi", "-r"]).expect("tt-smi -r");
    assert!(acquire < reset, "{commands:?}");
    assert_eq!(
        gozer.releases(),
        vec![("lease-1".to_string(), false)],
        "released after the reset, without a second one"
    );
    assert!(gozer.held_lease_ids().is_empty());
}

#[test]
fn box_reset_is_refused_while_a_neighbour_holds_chips() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    // Its owner is a dead pid in the fake (STALE): still someone else's
    // lease, and still refused -- `gozer reconcile` is the remedy.
    let neighbour = gozer.preexisting_lease(NEIGHBOUR);
    let backend = runpy(&gozer);
    let error = backend.reset().expect_err("refused");
    assert!(error.to_string().contains(NEIGHBOUR), "{error:#}");
    assert!(!gozer
        .commands()
        .iter()
        .any(|c| c.first().map(String::as_str) == Some("tt-smi")));
    assert_eq!(gozer.held_lease_ids(), vec![neighbour]);
}

#[tokio::test]
async fn a_forced_box_reset_over_a_holder_is_audited() {
    let gozer = FakeGozerBox::with_boards(1, 0);
    gozer.preexisting_lease(NEIGHBOUR);
    let backend: Arc<dyn ServingBackend> = Arc::new(runpy(&gozer));
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let base = serve(state.clone()).await;
    let token = support::pair(&base, &state).await;

    let (status, body) = post(
        &base,
        &token,
        "/reset",
        serde_json::json!({ "force": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(gozer
        .commands()
        .iter()
        .any(|c| c == &["tt-smi".to_string(), "-r".to_string()]));
    let (entries, _) = state.audit().page(0, 50);
    let entry = entries
        .iter()
        .find(|e| e.action == "reset")
        .expect("an audit entry");
    assert_eq!(entry.result, "forced");
    assert!(entry.target.contains(NEIGHBOUR), "{entry:?}");
}

#[test]
fn box_reset_without_gozer_is_unchanged() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(RunPyConfig::default(), Box::new(runner.clone()));
    backend.reset().expect("reset");
    let commands = runner.commands();
    assert!(commands.iter().any(|c| c == &["tt-smi", "-r"]));
    assert!(!commands
        .iter()
        .any(|c| c.first().map(String::as_str) == Some("gozer")));
}

// ---------------------------------------------------------------------------
// The reconciler leaves utility leases alone
// ---------------------------------------------------------------------------

#[test]
fn utility_leases_are_never_handed_to_a_serving_container() {
    let agent = 100;
    let lease = |who: &str, alive: bool| OwnLease {
        lease_id: "l".into(),
        who: who.into(),
        chips: vec!["0000:01:00.0".into()],
        owner_pid: Some(agent),
        owner_alive: alive,
    };
    let who = "tt-station:8080:reset-chips";
    assert!(gozer::is_utility_who(who));
    // The agent's own live lease, a container on the port: left alone.
    assert!(gozer::plan_reconcile(&[lease(who, true)], Some(4242), agent).is_empty());
    // Owner dead, a container on the port: left for gozer's reap.
    assert!(gozer::plan_reconcile(&[lease(who, false)], Some(4242), agent).is_empty());
    // Owner dead, nothing serving: released (reset).
    assert!(matches!(
        &gozer::plan_reconcile(&[lease(who, false)], None, agent)[..],
        [ReconcileAction::Release { .. }]
    ));
    assert!(!gozer::is_utility_who("tt-station:8080:model-a"));
}
