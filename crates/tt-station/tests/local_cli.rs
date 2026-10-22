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

/// The real capture with tt-station's dext attached to the P100A, as it will look once the dext
/// loads: an IORegistry child named `ttstation-blackhole` (same shape as the capture's Wi-Fi, which
/// Apple's own dext claims).
fn fixture_with_our_driver(dir: &tempfile::TempDir) -> PathBuf {
    let mut root = plist::Value::from_file(fixture()).unwrap();
    for entry in root.as_array_mut().unwrap() {
        let d = entry.as_dictionary_mut().unwrap();
        if d.get("IORegistryEntryName").and_then(plist::Value::as_string) == Some("pci1e52,b140") {
            let mut child = plist::Dictionary::new();
            child.insert("IORegistryEntryName".into(), "ttstation-blackhole".into());
            child.insert("IOObjectClass".into(), "IOUserService".into());
            d.insert("IORegistryEntryChildren".into(), plist::Value::Array(vec![plist::Value::Dictionary(child)]));
        }
    }
    let path = dir.path().join("ioreg-with-driver.xml");
    root.to_file_xml(&path).unwrap();
    path
}

fn run_local_with(ioreg: &PathBuf, driver_host: &PathBuf) -> serde_json::Value {
    let out = Command::cargo_bin("tt-station")
        .unwrap()
        .env("TTS_DRIVER_HOST_BIN", driver_host)
        .args(["--json", "local", "--no-models", "--ioreg-file"])
        .arg(ioreg)
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn telemetry_is_read_only_when_our_dext_holds_the_card() {
    let dir = tempfile::tempdir().unwrap();
    // A fake host app that prints what bh_probe.c's ttbh_telemetry_json prints (values from the
    // 2026-09-29 silicon run).
    let host = dir.path().join("TTStationDriver");
    std::fs::write(&host, "#!/bin/sh\n[ \"$1\" = telemetry ] || exit 9\necho '{\"abi\":2,\"boot_status\":5,\"arc_ready\":true,\"asic_temp_c\":46.176,\"power_w\":15,\"vcore_mv\":727,\"current_a\":22,\"aiclk_mhz\":800}'\n").unwrap();
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o755)).unwrap();

    let with = run_local_with(&fixture_with_our_driver(&dir), &host);
    assert_eq!(with["cards"][0]["driver"], "ttstation-blackhole");
    assert_eq!(with["telemetry"]["aiclk_mhz"], 800);
    assert_eq!(with["telemetry"]["arc_ready"], true);

    // Unclaimed card: the host app must not even be asked.
    let without = run_local_with(&fixture(), &dir.path().join("does-not-exist"));
    assert!(without["cards"][0]["driver"].is_null());
    assert!(without["telemetry"].is_null());
    assert!(without["telemetry_error"].is_null());
}

#[test]
fn a_broken_driver_host_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let report = run_local_with(&fixture_with_our_driver(&dir), &dir.path().join("missing-host"));
    assert!(report["telemetry"].is_null());
    assert!(report["telemetry_error"].as_str().unwrap().contains("telemetry"));
    assert_eq!(report["device_mesh"], "p100", "detection must survive a broken driver host");
}
