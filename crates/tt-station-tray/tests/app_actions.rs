//! `app::dispatch` (what each tray menu action does) against the real `mock-box` binary and a
//! fake `tt` script. No hardware, no real network beyond loopback to mock-box, no docker, no
//! terminal, no clipboard tool: the Connect actions get a recording [`Runner`], and Wake gets a
//! recording [`WolSender`].

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libttstation::run_options::RunOptions;
use tt_station_tray::app::{
    dispatch, maybe_show_notice, menu_for, ActionOutcome, AppContext, WolSender,
};
use tt_station_tray::launchers::{RunOutput, Runner};
use tt_station_tray::local::LocalStation;
use tt_station_tray::menu_model::{MenuAction, MenuItem};
use tt_station_tray::registry::Registry;
use tt_station_tray::remote::RemoteStation;
use tt_station_tray::sni_host::{notice_marker_path, HostStatus};
use tt_station_tray::station::{Station, StationId};

mod common;
use common::{free_port, spawn_mock_box, MockBox};

// ---------------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------------

/// Runs nothing; records every argv. `resolve` answers from a fixed list of tool names.
#[derive(Default)]
struct Recorder {
    tools: Vec<String>,
    ran: Mutex<Vec<Vec<String>>>,
    spawned: Mutex<Vec<Vec<String>>>,
    inputs: Mutex<Vec<(Vec<String>, String)>>,
}

impl Recorder {
    fn with(tools: &[&str]) -> Arc<Recorder> {
        Arc::new(Recorder {
            tools: tools.iter().map(|t| t.to_string()).collect(),
            ..Recorder::default()
        })
    }
    fn ran(&self) -> Vec<Vec<String>> {
        self.ran.lock().unwrap().clone()
    }
    fn spawned(&self) -> Vec<Vec<String>> {
        self.spawned.lock().unwrap().clone()
    }
}

impl Runner for Recorder {
    fn resolve(&self, tool: &str) -> Option<PathBuf> {
        self.tools
            .iter()
            .any(|t| t == tool)
            .then(|| PathBuf::from(format!("/fake/bin/{tool}")))
    }
    fn output(&self, argv: &[String]) -> std::io::Result<RunOutput> {
        self.ran.lock().unwrap().push(argv.to_vec());
        // `docker inspect` says "no such container"; everything else succeeds.
        Ok(RunOutput {
            success: argv.get(1).map(String::as_str) != Some("inspect"),
            stdout: String::new(),
        })
    }
    fn run_with_input(&self, argv: &[String], input: &str) -> std::io::Result<bool> {
        self.inputs
            .lock()
            .unwrap()
            .push((argv.to_vec(), input.to_string()));
        Ok(true)
    }
    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()> {
        self.spawned.lock().unwrap().push(argv.to_vec());
        Ok(())
    }
    fn wait_http_ok(&self, _url: &str, _timeout: Duration) -> bool {
        true
    }
}

/// Records each packet instead of sending it.
#[derive(Default)]
struct RecordingWol {
    sent: Mutex<Vec<Vec<u8>>>,
}

impl WolSender for RecordingWol {
    fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(packet.to_vec());
        Ok(())
    }
}

fn remote(m: &MockBox) -> Station {
    Station::Remote(RemoteStation::new(
        m.base.clone(),
        format!("box-{}", m.port),
        Some("tok".into()),
    ))
}

fn box_id(m: &MockBox) -> StationId {
    StationId(m.hostport.clone())
}

struct Ctx {
    ctx: AppContext,
    runner: Arc<Recorder>,
    wol: Arc<RecordingWol>,
    dir: tempfile::TempDir,
}

fn ctx_with(stations: Vec<Station>, dir: tempfile::TempDir, runner: Arc<Recorder>) -> Ctx {
    let wol = Arc::new(RecordingWol::default());
    let ctx = AppContext::new(
        &dir.path().join("tray-stations.json"),
        stations,
        runner.clone(),
        wol.clone(),
        dir.path(),
    );
    Ctx {
        ctx,
        runner,
        wol,
        dir,
    }
}

fn ctx(stations: Vec<Station>) -> Ctx {
    ctx_with(stations, tempfile::tempdir().unwrap(), Recorder::with(&[]))
}

fn registry(c: &Ctx) -> Registry {
    Registry::load(&c.dir.path().join("tray-stations.json"))
}

/// mock-box's test-only echo of the last run's options.
async fn last_run_options(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/status"))
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["last_run_options"]
        .clone()
}

fn port_opts(port: u16) -> RunOptions {
    RunOptions {
        port: Some(port),
        ..RunOptions::default()
    }
}

/// A fake `tt` in `dir` whose `model ps` lists one running server, `shown`, with a URL. Every
/// call is appended to the returned log. `serve_fails` makes `serve` exit 3 with a message.
fn fake_tt(dir: &Path, serve_fails: bool) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let ps = dir.join("ps.json");
    std::fs::write(
        &ps,
        r#"{"served":[{"name":"shown-model","backend":"tt","port":20000,"base_url":"http://127.0.0.1:20000/v1","status":"running","served_id":"shown-model"}],"probed":true}"#,
    )
    .unwrap();
    let dry = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dry_run_catalog.json");
    let serve_line = if serve_fails {
        r#"echo "boom: no chips available" >&2; exit 3"#
    } else {
        "echo '{}'"
    };
    let script = dir.join("tt");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  "--version") echo "tt 1.0.1" ;;
  "--json model ps") cat "{ps}" ;;
  "--json serve "*" --dry-run") cat "{dry}" ;;
  "--json serve "*) {serve_line} ;;
  "--json model stop "*) echo '{{}}' ;;
  *) echo "fake tt: unexpected: $*" >&2; exit 64 ;;
esac
"#,
            log = log.display(),
            ps = ps.display(),
            dry = dry.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Every action in a menu, submenus flattened.
fn all_actions(items: &[MenuItem]) -> Vec<MenuAction> {
    let mut v = Vec::new();
    for i in items {
        match i {
            MenuItem::Action { action, .. } => v.push(action.clone()),
            MenuItem::Submenu { items, .. } => v.extend(all_actions(items)),
            _ => {}
        }
    }
    v
}

fn message(o: &ActionOutcome) -> &str {
    match o {
        ActionOutcome::Message(m) => m,
        other => panic!("expected a Message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stop_acts_only_on_the_named_box() {
    let a = spawn_mock_box(&[]);
    let b = spawn_mock_box(&[]);
    for m in [&a, &b] {
        remote(m)
            .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .await
            .unwrap();
    }
    let c = ctx(vec![remote(&a), remote(&b)]);

    let out = dispatch(MenuAction::Stop(box_id(&a)), &c.ctx).await;
    assert_eq!(out, ActionOutcome::Done);
    assert!(remote(&a).state().await.serving.is_none(), "A was stopped");
    assert!(
        remote(&b).state().await.serving.is_some(),
        "B must still be serving: Stop names one station"
    );
}

#[tokio::test]
async fn stop_on_this_machine_stops_only_its_shown_server_and_not_a_box() {
    let tmp = tempfile::tempdir().unwrap();
    let (tt, log) = fake_tt(tmp.path(), false);
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let c = ctx(vec![Station::Local(LocalStation::new(&tt)), remote(&m)]);

    let out = dispatch(MenuAction::Stop(StationId("local".into())), &c.ctx).await;
    assert_eq!(out, ActionOutcome::Done);
    let stops: Vec<String> = calls(&log)
        .into_iter()
        .filter(|l| l.contains("model stop"))
        .collect();
    assert_eq!(stops, vec!["--json model stop shown-model"]);
    assert!(
        remote(&m).state().await.serving.is_some(),
        "the box is untouched"
    );

    // And the other way round: stopping the box runs no `tt model stop`.
    std::fs::write(&log, "").unwrap();
    let out = dispatch(MenuAction::Stop(box_id(&m)), &c.ctx).await;
    assert_eq!(out, ActionOutcome::Done);
    assert!(remote(&m).state().await.serving.is_none());
    assert!(
        !calls(&log).iter().any(|l| l.contains("model stop")),
        "{:?}",
        calls(&log)
    );
}

// ---------------------------------------------------------------------------------------------
// Restart and Run last
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restart_repeats_the_remembered_options_for_that_model() {
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let c = ctx(vec![remote(&m)]);
    let mut reg = registry(&c);
    reg.remember_run(&box_id(&m), "Llama-3.1-8B-Instruct", &port_opts(20001));
    // A different model's options must not be used.
    reg.remember_run(&box_id(&m), "Other-Model", &port_opts(20009));
    reg.save().unwrap();

    let out = dispatch(MenuAction::Restart(box_id(&m)), &c.ctx).await;
    assert_eq!(out, ActionOutcome::Done);
    // Wiring check: what the server received on the restart's /run.
    assert_eq!(last_run_options(&m.base).await["port"], 20001);
    let serving = remote(&m).state().await.serving.expect("serving again");
    assert_eq!(serving.model, "Llama-3.1-8B-Instruct");
}

#[tokio::test]
async fn restart_with_nothing_remembered_uses_defaults_and_says_so() {
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &port_opts(20005))
        .await
        .unwrap();
    let c = ctx(vec![remote(&m)]);
    let out = dispatch(MenuAction::Restart(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("default options"), "{out:?}");
    assert!(last_run_options(&m.base).await.is_null());
}

#[tokio::test]
async fn restart_of_an_idle_station_is_a_message() {
    let m = spawn_mock_box(&[]);
    let c = ctx(vec![remote(&m)]);
    let out = dispatch(MenuAction::Restart(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("not serving"), "{out:?}");
}

#[tokio::test]
async fn run_last_with_no_remembered_run_is_a_message() {
    let m = spawn_mock_box(&[]);
    let c = ctx(vec![remote(&m)]);
    let out = dispatch(MenuAction::RunLast(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("No model has been run"), "{out:?}");
    assert!(
        remote(&m).state().await.serving.is_none(),
        "nothing started"
    );
}

#[tokio::test]
async fn run_last_runs_the_most_recent_model_with_its_options() {
    let m = spawn_mock_box(&[]);
    let c = ctx(vec![remote(&m)]);
    let mut reg = registry(&c);
    reg.remember_run(&box_id(&m), "First-Model", &port_opts(20002));
    reg.remember_run(&box_id(&m), "Second-Model", &port_opts(20003));
    reg.save().unwrap();

    let out = dispatch(MenuAction::RunLast(box_id(&m)), &c.ctx).await;
    assert_eq!(out, ActionOutcome::Done);
    assert_eq!(
        remote(&m).state().await.serving.unwrap().model,
        "Second-Model"
    );
    assert_eq!(last_run_options(&m.base).await["port"], 20003);
}

#[tokio::test]
async fn a_refused_run_returns_the_holder_text_verbatim() {
    let holder = "chips held by claude:vllm until 5pm";
    let m = spawn_mock_box(&["--run-conflict", holder]);
    let c = ctx(vec![remote(&m)]);
    let mut reg = registry(&c);
    reg.remember_run(&box_id(&m), "m", &RunOptions::default());
    reg.save().unwrap();
    let out = dispatch(MenuAction::RunLast(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains(holder), "{out:?}");
}

#[tokio::test]
async fn a_failing_local_serve_returns_its_stderr_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let (tt, _log) = fake_tt(tmp.path(), true);
    let c = ctx(vec![Station::Local(LocalStation::new(&tt))]);
    let local = StationId("local".into());
    let mut reg = registry(&c);
    reg.remember_run(&local, "Llama-3.1-8B-Instruct", &RunOptions::default());
    reg.save().unwrap();
    let out = dispatch(MenuAction::RunLast(local), &c.ctx).await;
    assert!(
        message(&out).contains("boom: no chips available"),
        "{out:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Wake, window, unknown station
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn wake_sends_the_magic_packet_for_the_registry_mac() {
    let port = free_port(); // nothing listens: the box is "off"
    let c = ctx(vec![Station::Remote(RemoteStation::new(
        format!("http://127.0.0.1:{port}"),
        "off-box",
        Some("tok".into()),
    ))]);
    let mut reg = registry(&c);
    let id = reg.add_manual("127.0.0.1", port).unwrap();
    reg.set_mac(&id, "aa:bb:cc:dd:ee:ff");
    reg.save().unwrap();

    let out = dispatch(MenuAction::Wake(id), &c.ctx).await;
    assert!(message(&out).contains("wake-up packet"), "{out:?}");
    let sent = c.wol.sent.lock().unwrap().clone();
    let want = libttstation::wol::magic_packet([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
    assert_eq!(sent, vec![want.to_vec()]);
}

#[tokio::test]
async fn wake_without_a_known_mac_sends_nothing() {
    let port = free_port();
    let c = ctx(vec![Station::Remote(RemoteStation::new(
        format!("http://127.0.0.1:{port}"),
        "off-box",
        None,
    ))]);
    let out = dispatch(
        MenuAction::Wake(StationId(format!("127.0.0.1:{port}"))),
        &c.ctx,
    )
    .await;
    assert!(message(&out).contains("not known"), "{out:?}");
    assert!(c.wol.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn open_window_returns_open_window_unless_started_without_one() {
    let c = ctx(vec![]);
    assert_eq!(
        dispatch(MenuAction::OpenWindow, &c.ctx).await,
        ActionOutcome::OpenWindow
    );
    assert_eq!(
        dispatch(MenuAction::Quit, &c.ctx).await,
        ActionOutcome::Quit
    );
    let dir = tempfile::tempdir().unwrap();
    let no_window = AppContext::new(
        &dir.path().join("r.json"),
        vec![],
        Recorder::with(&[]),
        Arc::new(RecordingWol::default()),
        dir.path(),
    )
    .with_no_window(true);
    let out = dispatch(MenuAction::OpenWindow, &no_window).await;
    assert!(message(&out).contains("--no-window"), "{out:?}");
}

#[tokio::test]
async fn an_action_for_an_unlisted_station_is_a_message() {
    let c = ctx(vec![]);
    let out = dispatch(MenuAction::Stop(StationId("gone:8765".into())), &c.ctx).await;
    assert!(message(&out).contains("no longer listed"), "{out:?}");
}

// ---------------------------------------------------------------------------------------------
// Connect actions (recording runner)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn open_webui_runs_docker_wired_to_the_served_endpoint() {
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let c = ctx_with(
        vec![remote(&m)],
        tempfile::tempdir().unwrap(),
        Recorder::with(&["docker", "xdg-open"]),
    );
    let out = dispatch(MenuAction::OpenWebUi(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("http://localhost:3000"), "{out:?}");
    let run = c
        .runner
        .ran()
        .into_iter()
        .find(|a| a.get(1).map(String::as_str) == Some("run"))
        .expect("docker run");
    // mock-box serves on 127.0.0.1:<port>, which the container reaches as host.docker.internal.
    let want = format!(
        "OPENAI_API_BASE_URL=http://host.docker.internal:{}/v1",
        m.port
    );
    assert!(run.contains(&want), "{run:?}");
}

#[tokio::test]
async fn open_webui_without_docker_is_a_message_and_starts_nothing() {
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let c = ctx(vec![remote(&m)]);
    let out = dispatch(MenuAction::OpenWebUi(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("docker is not installed"), "{out:?}");
    assert!(c.runner.ran().is_empty());
}

#[tokio::test]
async fn copy_endpoint_puts_the_v1_url_on_the_clipboard() {
    let m = spawn_mock_box(&[]);
    remote(&m)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let c = ctx_with(
        vec![remote(&m)],
        tempfile::tempdir().unwrap(),
        Recorder::with(&["xclip"]),
    );
    dispatch(MenuAction::CopyEndpoint(box_id(&m)), &c.ctx).await;
    let inputs = c.runner.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].1, format!("http://127.0.0.1:{}/v1", m.port));
}

#[tokio::test]
async fn opencode_on_an_idle_station_is_a_message() {
    let m = spawn_mock_box(&[]);
    let c = ctx_with(
        vec![remote(&m)],
        tempfile::tempdir().unwrap(),
        Recorder::with(&["opencode", "xterm"]),
    );
    let out = dispatch(MenuAction::OpenOpenCode(box_id(&m)), &c.ctx).await;
    assert!(message(&out).contains("not serving"), "{out:?}");
    assert!(c.runner.spawned().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Poll, menu and the tray-host notice
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn refresh_fills_an_offline_boxs_mac_from_the_registry_so_the_menu_offers_wake() {
    let port = free_port();
    let c = ctx(vec![Station::Remote(RemoteStation::new(
        format!("http://127.0.0.1:{port}"),
        "off-box",
        Some("tok".into()),
    ))]);
    let mut reg = registry(&c);
    let id = reg.add_manual("127.0.0.1", port).unwrap();
    reg.set_mac(&id, "aa:bb:cc:dd:ee:ff");
    reg.save().unwrap();

    let states = c.ctx.refresh().await;
    assert_eq!(states[0].mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
    assert_eq!(c.ctx.states(), states);
    let actions = all_actions(&menu_for(&states, &registry(&c)));
    assert!(actions.contains(&MenuAction::Wake(id)), "{actions:?}");
}

#[tokio::test]
async fn menu_offers_run_last_from_the_registrys_most_recent_model() {
    let m = spawn_mock_box(&[]);
    let c = ctx(vec![remote(&m)]);
    let mut reg = registry(&c);
    reg.remember_run(&box_id(&m), "Recent-Model", &RunOptions::default());
    reg.save().unwrap();
    let states = c.ctx.refresh().await;
    let text = format!("{:?}", menu_for(&states, &registry(&c)));
    assert!(text.contains("Run last model: Recent-Model"), "{text}");
}

#[test]
fn the_no_tray_notice_is_shown_once_and_then_remembered() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("cfg");
    let r = Recorder::with(&["notify-send"]);
    let ubuntu = "ID=ubuntu\n";

    assert!(!maybe_show_notice(
        r.as_ref(),
        HostStatus::Present,
        ubuntu,
        "GNOME",
        &cfg
    ));
    assert!(r.spawned().is_empty(), "no notice when a host is present");

    assert!(maybe_show_notice(
        r.as_ref(),
        HostStatus::Missing,
        ubuntu,
        "GNOME",
        &cfg
    ));
    assert_eq!(r.spawned().len(), 1);
    assert!(r.spawned()[0]
        .iter()
        .any(|w| w.contains("gnome-extensions enable")));
    assert!(notice_marker_path(&cfg).exists(), "the marker is recorded");

    assert!(!maybe_show_notice(
        r.as_ref(),
        HostStatus::Missing,
        ubuntu,
        "GNOME",
        &cfg
    ));
    assert_eq!(r.spawned().len(), 1, "shown once only");
}

#[test]
fn a_notice_marker_that_cannot_be_written_is_logged_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the config directory should be: create_dir_all fails.
    let blocker = dir.path().join("not-a-dir");
    std::fs::write(&blocker, "x").unwrap();
    let r = Recorder::with(&["notify-send"]);
    assert!(maybe_show_notice(
        r.as_ref(),
        HostStatus::Missing,
        "ID=ubuntu\n",
        "GNOME",
        &blocker
    ));
    assert_eq!(r.spawned().len(), 1);
}
