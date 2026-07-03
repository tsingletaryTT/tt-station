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
    assert_eq!(boxes.len(), 1, "expected exactly one discovered box, got {boxes:?}");
    assert_eq!(boxes[0]["host"], "127.0.0.1");
    assert_eq!(boxes[0]["ctrl_port"], port);

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
    assert!(secrets_path.exists(), "pair should have written a secrets file");
    let secrets = std::fs::read_to_string(&secrets_path).unwrap();
    assert!(secrets.contains(&host), "secrets file should be keyed by the paired host");

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
