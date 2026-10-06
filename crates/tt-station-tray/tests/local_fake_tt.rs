//! `LocalStation` against a fake `tt` shell script. No real `tt`, no hardware, no Docker.
//!
//! The script appends every invocation's argv (one line each) to a log file, so a test can
//! assert both what was run and that nothing was run at all.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use libttstation::run_options::RunOptions;
use tt_station_tray::local::LocalStation;
use tt_station_tray::station::Station;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Write a fake `tt` into `dir`. `version` is what `--version` prints. Returns (script, log).
fn fake_tt(dir: &Path, version: &str) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let script = dir.join("tt");
    let body = format!(
        r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  "--version") echo "{version}" ;;
  "--json model list") cat "{list}" ;;
  "--json model ps") cat "{ps}" ;;
  "--json serve "*" --dry-run") cat "{dry}" ;;
  "--json serve "*) echo '{{}}' ;;
  "--json model stop "*) echo '{{}}' ;;
  *) echo "fake tt: unexpected: $*" >&2; exit 64 ;;
esac
"#,
        log = log.display(),
        version = version,
        list = fixture("model_list.json").display(),
        ps = fixture("model_ps.json").display(),
        dry = fixture("dry_run_catalog.json").display(),
    );
    std::fs::write(&script, body).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, log)
}

fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

fn station(script: &Path) -> Station {
    Station::Local(LocalStation::new(script))
}

#[tokio::test]
async fn local_models_parse_tt_json() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let rows = station(&script).models().await.unwrap();
    assert_eq!(rows.len(), 6);

    let by = |n: &str| {
        rows.iter()
            .find(|r| r.name == n)
            .unwrap_or_else(|| panic!("no {n}"))
    };
    let catalog = by("gemma-4-31B-it");
    assert_eq!(catalog.kind.as_deref(), Some("llm"));
    assert_eq!(catalog.source, "tt-inference-server");
    assert!(!catalog.cached);
    assert!(
        catalog.fits,
        "lists p300x2 and the fixture's device is p300x2"
    );

    let cached_bundle = by("changh95/qwen3.8-27b-p150x2");
    assert_eq!(cached_bundle.source, "HuggingFace");
    assert!(cached_bundle.cached);
    assert!(cached_bundle.kind.is_none());
    assert!(!cached_bundle.fits, "lists only p150x2");

    let community = by("Adartras/qwen3.6-a3b-blackhole");
    assert!(!community.cached);
    assert!(community.fits, "p150 entry fits a p300x2 machine");

    let local = by("changh95/qwen3.8-27b-dflash2-p300x2");
    assert_eq!(local.source, "local");
    assert!(local.cached && local.fits);
}

#[tokio::test]
async fn local_models_teach_state_the_mesh() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let st = station(&script);
    assert_eq!(st.state().await.mesh, None);
    st.models().await.unwrap();
    assert_eq!(st.state().await.mesh.as_deref(), Some("p300x2"));
}

#[tokio::test]
async fn local_refuses_stale_tt() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt-cli 0.0.3");
    let st = station(&script);

    let s = st.state().await;
    assert!(!s.reachable);
    assert!(s.serving.is_none());

    let err = st.models().await.unwrap_err();
    assert!(err.to_string().contains("tt <semver>"), "got {err}");

    let err = st
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tt <semver>"), "got {err}");
    // Only version probes ever ran.
    assert!(
        calls(&log).iter().all(|c| c == "--version"),
        "ran: {:?}",
        calls(&log)
    );
}

#[tokio::test]
async fn local_missing_binary_is_unreachable_not_a_panic() {
    let st = station(Path::new("/nonexistent/dir/tt"));
    assert!(!st.state().await.reachable);
    assert!(st.models().await.is_err());
}

#[tokio::test]
async fn local_state_reports_what_tt_model_ps_lists() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let s = station(&script).state().await;
    assert!(s.reachable && s.paired);
    let serving = s.serving.expect("ps fixture has a running server");
    assert_eq!(serving.model, "meta-llama/Llama-3.1-8B-Instruct");
    assert_eq!(serving.base_url, "http://127.0.0.1:20002/v1");
}

#[tokio::test]
async fn local_run_builds_serve_argv_from_options() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    let st = station(&script);

    let options = RunOptions {
        port: Some(20002),
        ..RunOptions::default()
    };
    let now = st
        .run("Llama-3.1-8B-Instruct", false, &options)
        .await
        .unwrap();
    assert_eq!(now.base_url, "http://127.0.0.1:20002/v1");

    let ran = calls(&log);
    assert!(
        ran.contains(&"--json serve Llama-3.1-8B-Instruct --port 20002".to_string()),
        "ran: {ran:?}"
    );
    // The dry run came first, with no port.
    let dry = ran
        .iter()
        .position(|c| c.ends_with("--dry-run"))
        .expect("dry run");
    let serve = ran.iter().position(|c| c.contains("--port 20002")).unwrap();
    assert!(dry < serve);
}

#[tokio::test]
async fn local_run_default_port_when_options_have_none() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    station(&script)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    assert!(calls(&log).contains(&"--json serve Llama-3.1-8B-Instruct --port 20000".to_string()));
}

#[tokio::test]
async fn local_run_rejected_option_never_calls_tt() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    let st = station(&script);

    // Port below 1024 fails RunOptions::validate.
    let bad = RunOptions {
        port: Some(80),
        ..RunOptions::default()
    };
    let err = st
        .run("Llama-3.1-8B-Instruct", false, &bad)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("port"), "got {err}");
    // A model name that could smuggle a flag.
    let err = st
        .run("--port=1", false, &RunOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("model"), "got {err}");

    assert!(
        !log.exists(),
        "tt must not run for a rejected request; ran: {:?}",
        calls(&log)
    );
}

#[tokio::test]
async fn local_run_refuses_options_a_catalog_model_cannot_take() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    // The fake dry run says "catalog", and catalog models refuse --profile.
    let opts = RunOptions {
        profile: Some("fast".into()),
        ..RunOptions::default()
    };
    let err = station(&script)
        .run("Llama-3.1-8B-Instruct", false, &opts)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("profile"), "got {err}");
    assert!(!calls(&log)
        .iter()
        .any(|c| c.starts_with("--json serve") && !c.ends_with("--dry-run")));
}

#[tokio::test]
async fn local_stop_stops_each_running_server_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    station(&script).stop().await.unwrap();
    assert!(calls(&log).contains(&"--json model stop Llama-3.1-8B-Instruct".to_string()));
}

#[tokio::test]
async fn local_schema_is_the_shared_one() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    let schema = station(&script).schema().await.unwrap();
    assert!(schema.options.iter().any(|o| o.name == "port"));
    assert!(!log.exists());
}
