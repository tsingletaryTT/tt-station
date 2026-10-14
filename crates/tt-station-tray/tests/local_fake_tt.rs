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

/// Knobs for the fake `tt`.
struct Opts {
    /// File `model ps` prints.
    ps: PathBuf,
    /// `serve` (not `--dry-run`) exits 3 with a message on stderr.
    serve_fails: bool,
    /// `model ps` exits 5.
    ps_fails: bool,
    /// File `serve <model> --dry-run` prints.
    dry: PathBuf,
    /// `serve ... --dry-run` hangs (sleeps 30 s) instead of printing.
    dry_hangs: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            ps: fixture("model_ps.json"),
            serve_fails: false,
            ps_fails: false,
            dry: fixture("dry_run_catalog.json"),
            dry_hangs: false,
        }
    }
}

fn fake_tt(dir: &Path, version: &str) -> (PathBuf, PathBuf) {
    fake_tt_with(dir, version, Opts::default())
}

/// A `ps` file in `dir` with these `served` rows (each a JSON object literal).
fn ps_file(dir: &Path, rows: &[&str]) -> PathBuf {
    let p = dir.join("ps.json");
    std::fs::write(
        &p,
        format!(r#"{{"served":[{}],"probed":true}}"#, rows.join(",")),
    )
    .unwrap();
    p
}

fn row(name: &str, port: u16, backend: &str, profile: Option<&str>) -> String {
    let profile = profile.map_or("null".to_string(), |p| format!("\"{p}\""));
    format!(
        r#"{{"name":"{name}","backend":"{backend}","container":"c-{port}","port":{port},"base_url":"http://127.0.0.1:{port}/v1","status":"running","health":"healthy","profile":{profile},"served_id":"{name}"}}"#
    )
}

/// Write a fake `tt` into `dir`. `version` is what `--version` prints. Returns (script, log).
fn fake_tt_with(dir: &Path, version: &str, o: Opts) -> (PathBuf, PathBuf) {
    let serve_line = if o.serve_fails {
        r#"echo "boom: no chips available" >&2; exit 3"#
    } else {
        "echo '{}'"
    };
    let ps_line = if o.ps_fails {
        r#"echo "ps broke" >&2; exit 5"#.to_string()
    } else {
        format!(r#"cat "{}""#, o.ps.display())
    };
    let log = dir.join("argv.log");
    let script = dir.join("tt");
    let body = format!(
        r#"#!/bin/sh
echo "$*" >> "{log}"
case "$*" in
  "--version") echo "{version}" ;;
  "--json model list") cat "{list}" ;;
  "--json model ps") {ps_line} ;;
  "model logs "*) printf 'line one\nline two\n' ;;
  "--json serve "*" --dry-run") {dry_line} ;;
  "--json serve "*) {serve_line} ;;
  "--json model stop "*) echo '{{}}' ;;
  *) echo "fake tt: unexpected: $*" >&2; exit 64 ;;
esac
"#,
        log = log.display(),
        version = version,
        list = fixture("model_list.json").display(),
        ps_line = ps_line,
        serve_line = serve_line,
        dry_line = if o.dry_hangs {
            "exec sleep 30".to_string()
        } else {
            format!(r#"cat "{}""#, o.dry.display())
        },
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
async fn local_stop_stops_the_shown_server_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    station(&script).stop().await.unwrap();
    assert!(calls(&log).contains(&"--json model stop Llama-3.1-8B-Instruct".to_string()));
}

#[tokio::test]
async fn local_stop_with_two_rows_stops_only_the_shown_one() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(
        dir.path(),
        &[
            &row("ours", 20000, "inference-server", None),
            &row("theirs", 20001, "inference-server", None),
        ],
    );
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    let st = station(&script);
    // The row the UI shows is the one state() reports.
    assert_eq!(st.state().await.serving.unwrap().model, "ours");
    st.stop().await.unwrap();
    let stops: Vec<String> = calls(&log)
        .into_iter()
        .filter(|c| c.contains("model stop"))
        .collect();
    assert_eq!(
        stops,
        vec!["--json model stop ours".to_string()],
        "the other server must be untouched"
    );
}

#[tokio::test]
async fn local_stop_passes_the_profile_when_the_row_has_one() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(
        dir.path(),
        &[&row("org/bundle", 20000, "model-manager", Some("fast"))],
    );
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    station(&script).stop().await.unwrap();
    assert!(calls(&log).contains(&"--json model stop org/bundle --profile fast".to_string()));
}

#[tokio::test]
async fn local_stop_never_stops_a_studio_row() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(dir.path(), &[&row("studio-model", 20000, "studio", None)]);
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    let err = station(&script).stop().await.unwrap_err();
    assert!(err.to_string().contains("TT-Studio"), "got {err}");
    assert!(!calls(&log).iter().any(|c| c.contains("model stop")));
}

#[tokio::test]
async fn local_stop_refuses_an_unsafe_name_from_ps() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(
        dir.path(),
        &[&row("--all", 20000, "inference-server", None)],
    );
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    let err = station(&script).stop().await.unwrap_err();
    assert!(err.to_string().contains("model"), "got {err}");
    assert!(!calls(&log).iter().any(|c| c.contains("model stop")));
}

#[tokio::test]
async fn local_stop_with_several_unidentifiable_rows_errors_and_stops_none() {
    let dir = tempfile::tempdir().unwrap();
    // No port and no URL: neither row can be the one the UI shows.
    let bare =
        |n: &str| format!(r#"{{"name":"{n}","backend":"inference-server","status":"running"}}"#);
    let ps = ps_file(dir.path(), &[&bare("a"), &bare("b")]);
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    let err = station(&script).stop().await.unwrap_err();
    assert!(err.to_string().contains("none is the one"), "got {err}");
    assert!(!calls(&log).iter().any(|c| c.contains("model stop")));
}

#[tokio::test]
async fn local_logs_run_tt_model_logs_with_exact_argv() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    station(&script).logs(50).await.unwrap();
    assert!(
        calls(&log).contains(&"model logs Llama-3.1-8B-Instruct --tail 50".to_string()),
        "ran: {:?}",
        calls(&log)
    );
}

#[tokio::test]
async fn local_logs_with_profile_and_returns_the_scripts_lines() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(
        dir.path(),
        &[&row("org/bundle", 20000, "model-manager", Some("fast"))],
    );
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    let lines = station(&script).logs(2).await.unwrap();
    assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
    assert!(calls(&log).contains(&"model logs org/bundle --tail 2 --profile fast".to_string()));
}

#[tokio::test]
async fn local_logs_when_nothing_runs_is_empty_and_calls_no_logs() {
    let dir = tempfile::tempdir().unwrap();
    let ps = ps_file(dir.path(), &[]);
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps,
            ..Opts::default()
        },
    );
    assert!(station(&script).logs(10).await.unwrap().is_empty());
    assert!(!calls(&log).iter().any(|c| c.starts_with("model logs")));
}

#[tokio::test]
async fn local_run_surfaces_a_failing_tt_serve() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            serve_fails: true,
            ..Opts::default()
        },
    );
    let err = station(&script)
        .run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("boom: no chips available") && text.contains("exit"),
        "got {text}"
    );
}

#[tokio::test]
async fn local_state_survives_a_failing_ps() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            ps_fails: true,
            ..Opts::default()
        },
    );
    let s = station(&script).state().await;
    assert!(s.reachable, "tt itself works");
    assert!(s.serving.is_none());
}

#[tokio::test]
async fn local_schema_is_the_shared_one() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    let schema = station(&script).schema().await.unwrap();
    assert!(schema.options.iter().any(|o| o.name == "port"));
    assert!(!log.exists());
}

/// M4: `tt serve --workflow benchmarks|evals` runs in the foreground until it finishes, which
/// would block the tray. `LocalStation::run` refuses them before `tt` runs at all.
#[tokio::test]
async fn local_run_refuses_non_server_workflows_before_tt_runs() {
    use libttstation::run_options::Workflow;
    for w in [Workflow::Benchmarks, Workflow::Evals] {
        let dir = tempfile::tempdir().unwrap();
        let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
        let opts = RunOptions {
            workflow: Some(w),
            ..RunOptions::default()
        };
        let err = station(&script)
            .run("Llama-3.1-8B-Instruct", false, &opts)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("workflow") && msg.contains(w.as_str()),
            "{msg}"
        );
        assert!(!log.exists(), "tt ran: {:?}", calls(&log));
    }
}

// --- Reference performance (crate::perf) through LocalStation ---------------------------------

/// A fake tt-inference-server checkout `<root>/tt-inference-server-v<ver>/src` holding the
/// short-name fixture in the 0.22 layout and a `VERSION` file. Returns the `src` path.
fn fake_checkout(root: &Path, ver: &str) -> PathBuf {
    let src = root.join(format!("tt-inference-server-v{ver}/src"));
    let dir = src.join("reference_config/benchmarking/benchmark_targets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        fixture("perf_reference_short.json"),
        dir.join("model_performance_reference.json"),
    )
    .unwrap();
    std::fs::write(src.join("VERSION"), format!("{ver}\n")).unwrap();
    src
}

/// A dry-run plan whose argv runs `<checkout>/run.py`, like the official CLI prints.
fn dry_plan(dir: &Path, checkout: &Path) -> PathBuf {
    let p = dir.join("dry.json");
    let plan = serde_json::json!({
        "model": "gemma-4-31B-it",
        "argv": [checkout.join("../venv/bin/python"), checkout.join("run.py"), "--model", "gemma-4-31B-it"],
    });
    std::fs::write(&p, plan.to_string()).unwrap();
    p
}

#[tokio::test]
async fn perf_reference_comes_from_the_checkout_the_dry_run_names_and_is_cached() {
    let dir = tempfile::tempdir().unwrap();
    let named = fake_checkout(&dir.path().join("elsewhere"), "0.21.0");
    // A newer checkout in the tools directory must lose to the one the dry run names.
    let tools = dir.path().join("tools");
    fake_checkout(&tools, "0.99.0");
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            dry: dry_plan(dir.path(), &named),
            ..Opts::default()
        },
    );
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(&tools));
    // Listing the models teaches the station its mesh (`p300x2` in the fixture), which the dry
    // run then passes as `--device` so tt does not auto-detect hardware.
    s.models().await.unwrap();
    let table = s
        .perf_reference(Some("gemma-4-31B-it"))
        .await
        .expect("table");
    assert_eq!(table.version.as_deref(), Some("0.21.0"));
    assert!(table.models.contains_key("gemma-4-31b-it"));
    assert!(
        calls(&log).contains(&"--json serve gemma-4-31B-it --device p300x2 --dry-run".to_string()),
        "{:?}",
        calls(&log)
    );

    let before = calls(&log).len();
    let again = s
        .perf_reference(Some("gemma-4-31B-it"))
        .await
        .expect("cached");
    assert!(
        std::sync::Arc::ptr_eq(&table, &again),
        "second call reuses the parsed table"
    );
    assert_eq!(
        calls(&log).len(),
        before,
        "a cached table starts no process"
    );
}

#[tokio::test]
async fn perf_reference_falls_back_to_the_newest_installed_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    fake_checkout(&tools, "0.9.0");
    fake_checkout(&tools, "0.18.0");
    // The default dry-run fixture's argv has a relative `run.py`, which names no checkout.
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(&tools));
    let table = s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .expect("table");
    assert_eq!(table.version.as_deref(), Some("0.18.0"));
    // No sample model at all also falls back.
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(&tools));
    assert!(s.perf_reference(None).await.is_some());
}

#[tokio::test]
async fn perf_reference_is_none_when_no_file_exists_anywhere() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(dir.path().join("empty")));
    assert!(s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .is_none());
}

#[tokio::test]
async fn a_box_has_no_perf_reference() {
    // The agent's /models carries no reference numbers; nothing is requested or invented.
    let s = Station::Remote(tt_station_tray::remote::RemoteStation::new(
        "http://127.0.0.1:9".to_string(),
        "nowhere",
        None,
    ));
    assert!(s.perf_reference(Some("anything")).await.is_none());
}

#[tokio::test]
async fn local_models_carry_hf_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (script, _) = fake_tt(dir.path(), "tt 1.0.1");
    let rows = station(&script).models().await.unwrap();
    let g = rows.iter().find(|r| r.name == "gemma-4-31B-it").unwrap();
    assert_eq!(g.hf_repo.as_deref(), Some("google/gemma-4-31B-it"));
}

#[tokio::test]
async fn perf_reference_without_a_known_mesh_sends_no_device() {
    let dir = tempfile::tempdir().unwrap();
    let named = fake_checkout(&dir.path().join("elsewhere"), "0.21.0");
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            dry: dry_plan(dir.path(), &named),
            ..Opts::default()
        },
    );
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(dir.path().join("none")));
    assert!(s.perf_reference(Some("gemma-4-31B-it")).await.is_some());
    assert!(calls(&log).contains(&"--json serve gemma-4-31B-it --dry-run".to_string()));
}

#[tokio::test]
async fn a_hanging_dry_run_falls_back_within_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let tools = dir.path().join("tools");
    fake_checkout(&tools, "0.18.0");
    let (script, log) = fake_tt_with(
        dir.path(),
        "tt 1.0.1",
        Opts {
            dry_hangs: true,
            ..Opts::default()
        },
    );
    let s = Station::Local(
        LocalStation::new(&script)
            .with_tools_dir(&tools)
            .with_perf_dry_run_timeout(std::time::Duration::from_millis(300)),
    );
    let started = std::time::Instant::now();
    let table = s
        .perf_reference(Some("gemma-4-31B-it"))
        .await
        .expect("fallback table");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}; the fake dry run sleeps 30 s",
        started.elapsed()
    );
    assert_eq!(table.version.as_deref(), Some("0.18.0"));
    assert!(
        calls(&log).iter().any(|c| c.ends_with("--dry-run")),
        "the dry run was tried"
    );
}

#[tokio::test]
async fn a_miss_is_remembered_so_the_next_load_starts_no_process() {
    let dir = tempfile::tempdir().unwrap();
    let (script, log) = fake_tt(dir.path(), "tt 1.0.1");
    let s = Station::Local(LocalStation::new(&script).with_tools_dir(dir.path().join("empty")));
    assert!(s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .is_none());
    let before = calls(&log).len();
    assert!(before > 0, "the first lookup ran the dry run");
    assert!(s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .is_none());
    assert_eq!(
        calls(&log).len(),
        before,
        "a remembered miss starts no process"
    );

    // Once the remembered miss expires the lookup runs again.
    let dir2 = tempfile::tempdir().unwrap();
    let (script2, log2) = fake_tt(dir2.path(), "tt 1.0.1");
    let s = Station::Local(
        LocalStation::new(&script2)
            .with_tools_dir(dir2.path().join("empty"))
            .with_perf_miss_ttl(std::time::Duration::ZERO),
    );
    assert!(s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .is_none());
    let first = calls(&log2).len();
    assert!(s
        .perf_reference(Some("Llama-3.1-8B-Instruct"))
        .await
        .is_none());
    assert!(
        calls(&log2).len() > first,
        "an expired miss is looked up again"
    );
}
