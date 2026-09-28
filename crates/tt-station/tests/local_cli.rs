//! `tt-station local` end to end, with no hardware and no network: the IORegistry comes from the
//! real capture in libttstation's fixtures, and the "official tt CLI" is a fake shell script
//! selected through `TTS_OFFICIAL_TT_BIN`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use assert_cmd::Command;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../libttstation/src/fixtures/ioreg-p100a-thunderbolt.xml")
}

/// Write an executable fake `tt` into `dir`.
fn fake_tt(dir: &tempfile::TempDir, body: &str) -> PathBuf {
    let path = dir.path().join("tt");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn run_local(tt: &PathBuf) -> serde_json::Value {
    let out = Command::cargo_bin("tt-station")
        .unwrap()
        .env("TTS_OFFICIAL_TT_BIN", tt)
        .args(["--json", "local", "--ioreg-file"])
        .arg(fixture())
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn official_tt_is_asked_for_the_p100_config() {
    let dir = tempfile::tempdir().unwrap();
    // Record the argv so we can assert tt-station asked for exactly `--hw p100`.
    let argv_log = dir.path().join("argv");
    let tt = fake_tt(
        &dir,
        &format!(
            r#"if [ "$1" = "--version" ]; then echo "tt 1.0.1"; exit 0; fi
echo "$@" > {log}
echo '{{"device":"p100","models":[{{"name":"Llama-3.1-8B","model_type":"llm","devices":{{"p100":{{"status":"EXPERIMENTAL","supported":true,"max_context":65536}}}}}}]}}'"#,
            log = argv_log.display()
        ),
    );
    let report = run_local(&tt);
    assert_eq!(report["device_mesh"], "p100");
    assert_eq!(report["cards"][0]["board_type"], "p100a");
    assert_eq!(report["official_tt"]["version"], "1.0.1");
    assert_eq!(report["models"][0]["name"], "Llama-3.1-8B");
    assert_eq!(std::fs::read_to_string(argv_log).unwrap().trim(), "--json model list --hw p100");
}

#[test]
fn stale_tt_station_shadowing_tt_is_refused_not_trusted() {
    // Exactly what `~/.local/bin/tt` did on the owner's Mac before 2026-09-28: tt-station's own
    // pre-rename CLI, which has no --version and would happily "answer" a model list.
    let dir = tempfile::tempdir().unwrap();
    let tt = fake_tt(
        &dir,
        r#"echo "error: unexpected argument '$1' found" >&2
echo "Usage: tt [OPTIONS] <COMMAND>"
exit 2"#,
    );
    let report = run_local(&tt);
    assert_eq!(report["device_mesh"], "p100", "detection must not depend on tt");
    assert!(report["models"].is_null());
    let err = report["models_error"].as_str().unwrap();
    assert!(err.contains("not the official Tenstorrent CLI"), "{err}");
    assert!(err.contains("uv tool install tenstorrent"), "{err}");
}
