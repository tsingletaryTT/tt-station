//! End-to-end test: discover -> pair -> run -> endpoint -> completion, all
//! driven through the real `tt` binary against a real (mock) HTTP server --
//! no actual agent or hardware anywhere in the loop. This is the Task 12 /
//! M2 CI stand-in: if this passes, the whole PoC flow works.
//!
//! `#[ignore]`d because it needs the `mock-box` binary built and a free TCP
//! port; run it explicitly:
//!
//!   cargo build --workspace
//!   cargo test -p tt --test e2e_mock -- --ignored

use assert_cmd::Command as AssertCommand;
use std::net::TcpStream;
use std::process::{Child, Command as StdCommand};
use std::time::{Duration, Instant};

/// RAII guard around the spawned `mock-box serve` child: killed on drop so
/// a failed assertion (which unwinds past the rest of the test body) never
/// leaves a server bound to the test's port.
struct MockBox(Child);

impl Drop for MockBox {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Build (if needed) and spawn `mock-box serve --ctrl-port <port>`.
///
/// Uses `escargot` rather than `assert_cmd::Command::cargo_bin` because
/// `mock-box` lives in a different workspace crate than this test binary --
/// `cargo_bin` only resolves binaries via the `CARGO_BIN_EXE_<name>` env
/// vars Cargo sets for the *current* package's own `[[bin]]` targets.
fn spawn_mock_box(port: u16) -> MockBox {
    let run_result = escargot::CargoBuild::new()
        .bin("mock-box")
        .package("mock-box")
        .run()
        .expect("failed to build mock-box for e2e test");

    let child = StdCommand::new(run_result.path())
        .args(["serve", "--ctrl-port", &port.to_string()])
        .spawn()
        .expect("failed to spawn mock-box serve");

    MockBox(child)
}

/// Poll `127.0.0.1:port` until something is listening (or panic after a
/// generous timeout) -- `mock-box` binds its socket asynchronously relative
/// to when we spawn it, so the first `tt discover` call needs to wait for
/// that instead of racing it.
fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock-box never started listening on port {port}");
}

/// A `TT_CONFIG_DIR` scratch directory, unique per test run and cleaned up
/// on drop, so this test never touches (or collides with) a real `tt`
/// config dir or a concurrently-running instance of itself.
struct TempConfigDir(std::path::PathBuf);

impl TempConfigDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tt-e2e-config-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos() // cheap uniqueness salt
        ));
        std::fs::create_dir_all(&dir).expect("create TT_CONFIG_DIR scratch dir");
        Self(dir)
    }
}

impl Drop for TempConfigDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore]
fn discover_pair_run_endpoint_completion_against_mock_box() {
    // A high, unusual port to make an accidental clash with something else
    // on the test host unlikely.
    let port: u16 = 18899;
    let host = format!("127.0.0.1:{port}");

    let _mock_box = spawn_mock_box(port);
    wait_for_port(port);

    let config_dir = TempConfigDir::new();

    // --- 1. `tt --json discover --host <mock>` lists the mock box. ---
    // `--no-mdns`: this environment may have no multicast/avahi available,
    // and we don't need real LAN discovery to prove the CLI's plumbing --
    // the manual-host path is what's under test here.
    let discover_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "discover", "--host", &host, "--no-mdns"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let boxes: serde_json::Value =
        serde_json::from_slice(&discover_stdout).expect("discover output is valid JSON");
    let boxes = boxes.as_array().expect("discover JSON is an array");
    assert_eq!(
        boxes.len(),
        1,
        "expected exactly one discovered box, got {boxes:?}"
    );
    assert_eq!(boxes[0]["host"], "127.0.0.1");
    assert_eq!(boxes[0]["ctrl_port"], port);

    // --- 1b. `tt --json models --host <mock>` lists the mock's canned
    // models -- UNAUTHED, so this works even before `tt pair`. ---
    let models_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "models", "--host", &host])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let models: serde_json::Value =
        serde_json::from_slice(&models_stdout).expect("models output is valid JSON");
    let models_array = models["models"]
        .as_array()
        .expect("models JSON has a models array");
    assert!(
        !models_array.is_empty(),
        "expected at least one model from mock-box, got {models:?}"
    );
    assert_eq!(models_array[0]["name"], "mock-model");

    // --- 2. `tt --json pair <host> --code 000000` stores a token. ---
    // The mock accepts any code (see mock-box/src/main.rs's pair_complete),
    // so "000000" is arbitrary -- what matters is that pairing succeeds and
    // a token ends up in this test's scratch TT_CONFIG_DIR.
    AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "pair", &host, "--code", "000000"])
        .assert()
        .success();

    let secrets_path = config_dir.0.join("secrets.json");
    assert!(
        secrets_path.exists(),
        "pair should have written a secrets file"
    );
    let secrets = std::fs::read_to_string(&secrets_path).unwrap();
    assert!(
        secrets.contains(&host),
        "secrets file should be keyed by the paired host"
    );

    // --- 3. `tt --json run llama3 --host <host>` returns an Endpoint JSON
    // whose base_url contains "/v1". ---
    let run_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "run", "llama3", "--host", &host])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let endpoint: serde_json::Value =
        serde_json::from_slice(&run_stdout).expect("run output is valid JSON");
    let base_url = endpoint["base_url"]
        .as_str()
        .expect("endpoint JSON has a base_url string");
    assert!(
        base_url.contains("/v1"),
        "expected base_url to contain /v1, got {base_url:?}"
    );
    assert_eq!(endpoint["model"], "llama3");

    // --- 4. POST {base_url}/chat/completions -> the canned completion. ---
    let completion: serde_json::Value = reqwest::blocking::Client::new()
        .post(format!("{base_url}/chat/completions"))
        .json(&serde_json::json!({
            "model": "llama3",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .send()
        .expect("POST to mock chat/completions")
        .json()
        .expect("chat/completions response is valid JSON");

    let content = completion["choices"][0]["message"]["content"]
        .as_str()
        .expect("completion has choices[0].message.content");
    assert_eq!(content, "hello from mock-box");

    // --- Bonus: `tt endpoint --host <host>` (non-json) prints the export
    // line, and `tt stop` + `tt status` round-trip back to idle. Not
    // required by the brief's five-step sequence, but cheap extra coverage
    // of the remaining commands using the same live mock. ---
    let endpoint_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["endpoint", "--host", &host])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let endpoint_text = String::from_utf8(endpoint_stdout).unwrap();
    assert!(endpoint_text.trim().starts_with("export OPENAI_BASE_URL="));

    AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["stop", "--host", &host])
        .assert()
        .success();

    let status_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "status", "--host", &host])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: serde_json::Value = serde_json::from_slice(&status_stdout).unwrap();
    assert_eq!(status["status"], "idle");
}

/// `tt --json config --host <mock>` round-trips a `ConfigSummary` from
/// mock-box's fake `/config` route (Task 6). UNAUTHED, like `tt
/// status`/`tt serving`/`tt models` -- no `tt pair` needed first, so this
/// spins up its own mock-box instance and drives just this one command
/// against it, using the exact same harness (`spawn_mock_box`,
/// `wait_for_port`, `TempConfigDir`) as
/// `discover_pair_run_endpoint_completion_against_mock_box` above.
#[test]
#[ignore] // hardware-free but network/process -- run with --ignored like the others
fn tt_config_json_round_trips_from_mock_box() {
    let port: u16 = 18900;
    let host = format!("127.0.0.1:{port}");

    let _mock_box = spawn_mock_box(port);
    wait_for_port(port);

    let config_dir = TempConfigDir::new();

    let config_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "config", "--host", &host])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let summary: libttstation::model::ConfigSummary =
        serde_json::from_slice(&config_stdout).expect("config output parses as ConfigSummary");

    assert_eq!(summary.active_profile.as_deref(), Some("mock"));
}

/// `tt --json catalog --host <mock> --catalog-file <fixture>` (Task 4):
/// classifies a trimmed fixture catalog (`tests/fixtures/compatibility.json`
/// -- one Supported/one Experimental/one Galaxy-only/one Not-Supported model)
/// against mock-box's canned `/status` (`device_mesh: "p300x2"`) and
/// `/models` (`mock-model`, `mock-model-large`), entirely without hardware or
/// network -- `--catalog-file` bypasses both the real CDN fetch and the
/// on-disk cache (see `tt::catalog::load_catalog`'s `file_override` path),
/// and mock-box stands in for a live agent, UNAUTHED just like `tt
/// status`/`tt models`, so this needs no prior `tt pair`.
#[test]
#[ignore] // hardware-free but network/process -- run with --ignored like the others
fn tt_catalog_json_classifies_fixture_against_mock_box() {
    let port: u16 = 18901;
    let host = format!("127.0.0.1:{port}");

    let _mock_box = spawn_mock_box(port);
    wait_for_port(port);

    let config_dir = TempConfigDir::new();

    // Absolute path built from CARGO_MANIFEST_DIR (this crate's own
    // `crates/tt/`) rather than a path relative to the test binary's cwd --
    // `cargo test` doesn't guarantee a stable cwd, but this env var is
    // always set at compile time to the crate root.
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("compatibility.json");
    assert!(
        fixture_path.exists(),
        "fixture must exist at {fixture_path:?}"
    );

    let catalog_stdout = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args([
            "--json",
            "catalog",
            "--host",
            &host,
            "--catalog-file",
            fixture_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let bc: libttstation::catalog::BoxCatalog =
        serde_json::from_slice(&catalog_stdout).expect("catalog output parses as BoxCatalog");

    // mock-box's `/status` reports `device_mesh: "p300x2"` (see
    // `mock-box/src/main.rs`'s `get_status`).
    assert_eq!(bc.box_mesh.as_deref(), Some("p300x2"));
    assert!(bc.catalog_available, "the fixture file should parse");
    assert!(!bc.catalog_stale, "a --catalog-file load is never stale");

    // "Model A" (Supported on Quietbox 2 == p300x2) lands in runs_here,
    // alongside mock-box's live "mock-model"/"mock-model-large" (no catalog
    // match, so they're appended verbatim -- see `classify`'s doc).
    assert!(
        bc.runs_here.iter().any(|e| e.id == "model-a"),
        "expected Model A in runs_here, got: {:?}",
        bc.runs_here
    );
    assert!(
        bc.runs_here.iter().any(|e| e.display_name == "mock-model"),
        "expected mock-box's live mock-model in runs_here, got: {:?}",
        bc.runs_here
    );

    // "Model B" (Experimental on Quietbox 2) lands in experimental.
    assert!(
        bc.experimental.iter().any(|e| e.id == "model-b"),
        "expected Model B in experimental, got: {:?}",
        bc.experimental
    );

    // "Model C" (Supported only on Galaxy) needs other hardware, and is
    // annotated with the mesh it needs (Galaxy -> T3K, see
    // `libttstation::catalog::hw_to_mesh`).
    let model_c = bc
        .other_hardware
        .iter()
        .find(|e| e.id == "model-c")
        .unwrap_or_else(|| {
            panic!(
                "expected Model C in other_hardware, got: {:?}",
                bc.other_hardware
            )
        });
    assert_eq!(model_c.needed_hardware, vec!["T3K".to_string()]);

    // "Model D" (Not Supported everywhere) is omitted entirely.
    assert!(
        !bc.runs_here
            .iter()
            .chain(&bc.experimental)
            .chain(&bc.other_hardware)
            .any(|e| e.id == "model-d"),
        "Model D should be omitted entirely, got: {bc:?}"
    );
}

/// Regression test for the nested-runtime panic in `tt catalog`'s primary
/// usage path -- `tt catalog --host <h>` WITHOUT `--catalog-file`.
///
/// `cmd_catalog` used to call `catalog::load_catalog` (which builds a
/// `reqwest::blocking::Client` in `fetch_remote`) from INSIDE the Tokio
/// runtime `run_async` already has blocked on -- building a blocking client
/// while an async runtime is active panics in debug builds ("Cannot drop a
/// runtime in a context where blocking is not allowed" / nested-runtime).
/// Every other e2e test above always passes `--catalog-file`, which
/// early-returns in `load_catalog` before ever reaching `fetch_remote`, so
/// none of them exercised this path. This test deliberately omits
/// `--catalog-file` AND points `TT_CONFIG_DIR` at a fresh, empty temp dir
/// (via `TempConfigDir`, same as every other test here) so
/// `catalog::cache_path()` -- which honors `TT_CONFIG_DIR` exactly like
/// `secrets.json` does, see `catalog.rs`'s doc -- finds no cache file and is
/// forced down the `fetch_remote()` branch that used to panic.
///
/// The assertion is deliberately network-outcome-agnostic: whether this test
/// runner has internet access or not, `tt catalog` must exit 0 and print
/// parseable `BoxCatalog` JSON with a `runs_here` array -- offline, the fetch
/// fails and `load_catalog` degrades to `(None, false)` (still a valid,
/// classifiable catalog per its degradation contract); online, the real
/// fetch succeeds. Either way, the process must not panic. This is exactly
/// the pre-fix panic: a debug build of `tt catalog --host <mock> --json`
/// (no `--catalog-file`) crashed instead of exiting 0.
#[test]
#[ignore] // hardware-free but network/process -- run with --ignored like the others
fn tt_catalog_without_catalog_file_does_not_panic_in_nested_runtime() {
    let port: u16 = 18902;
    let host = format!("127.0.0.1:{port}");

    let _mock_box = spawn_mock_box(port);
    wait_for_port(port);

    // Fresh, empty `TT_CONFIG_DIR` -- no `compatibility.json` cache present,
    // so `catalog::load_catalog` is forced past the fresh-cache fast path
    // and into `fetch_remote()`, the branch that used to panic.
    let config_dir = TempConfigDir::new();

    let output = AssertCommand::cargo_bin("tt")
        .unwrap()
        .env("TT_CONFIG_DIR", &config_dir.0)
        .args(["--json", "catalog", "--host", &host])
        .assert()
        .success() // must exit 0, not panic (pre-fix: panics before this)
        .get_output()
        .stdout
        .clone();

    let bc: serde_json::Value =
        serde_json::from_slice(&output).expect("catalog output is valid JSON");
    assert!(
        bc["runs_here"].is_array(),
        "expected a runs_here array in BoxCatalog JSON, got: {bc}"
    );
}
