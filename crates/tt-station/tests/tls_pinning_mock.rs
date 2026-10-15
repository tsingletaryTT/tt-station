// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! End to end: the real `tt-station` binary reaching a (mock) box from
//! "another computer" -- over HTTPS with the box's certificate pinned. Covers
//! pairing (with and without a pairing link / `--fp`), commands and
//! `logs --follow` over the pinned connection, a changed certificate being
//! refused, and an old token-only pairing being pinned on first use.
//!
//! The box is addressed as `127.0.0.2`: loopback on Linux, but not one of the
//! addresses the CLI treats as "this computer" (`127.0.0.1`/`localhost`), so
//! it takes the HTTPS path a remote box does.
//!
//! `#[ignore]`d like `e2e_mock.rs` (it builds and spawns `mock-box`); run:
//!
//!   cargo test -p tt-station --test tls_pinning_mock -- --ignored

use assert_cmd::Command as AssertCommand;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand};
use std::time::{Duration, Instant};

struct MockBox(Child);

impl Drop for MockBox {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_mock_box(ctrl: u16, tls: u16, tls_dir: &Path) -> MockBox {
    let built = escargot::CargoBuild::new()
        .bin("mock-box")
        .package("mock-box")
        .run()
        .expect("failed to build mock-box");
    let mock = MockBox(
        StdCommand::new(built.path())
            .args(["serve", "--ctrl-port", &ctrl.to_string()])
            .args(["--tls-port", &tls.to_string()])
            .arg("--tls-dir")
            .arg(tls_dir)
            .spawn()
            .expect("failed to spawn mock-box serve"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.2", tls)).is_ok() {
            return mock;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("mock-box never listened on {tls}");
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "tt-tls-e2e-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tt(config: &Path, args: &[&str]) -> assert_cmd::assert::Assert {
    AssertCommand::cargo_bin("tt-station")
        .unwrap()
        .env("TT_CONFIG_DIR", config)
        .args(args)
        .assert()
}

fn stdout_json(a: &assert_cmd::assert::Assert) -> serde_json::Value {
    serde_json::from_slice(&a.get_output().stdout).expect("JSON on stdout")
}

fn stderr(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stderr).into_owned()
}

#[test]
#[ignore]
fn https_pairing_pinning_and_migration_against_mock_box() {
    let (ctrl, tls) = (18911u16, 18912u16);
    let host = format!("127.0.0.2:{ctrl}");
    let cert_a = TempDir::new("cert-a");
    let mock = spawn_mock_box(ctrl, tls, &cert_a.0);

    // --- Pair: finds HTTPS on the port above, shows and pins the certificate.
    let config = TempDir::new("config");
    let paired = tt(&config.0, &["--json", "pair", &host, "--code", "000000"]).success();
    let out = stdout_json(&paired);
    assert_eq!(out["tls"]["port"], tls);
    let sha = out["tls"]["sha256"].as_str().unwrap().to_string();
    assert_eq!(sha.len(), 64);
    assert!(
        stderr(&paired).contains(&sha),
        "fingerprint shown before pairing"
    );
    let stored = std::fs::read_to_string(config.0.join("secrets.json")).unwrap();
    assert!(stored.contains(&sha), "pin stored with the token: {stored}");

    // --- Commands run over the pinned connection, WebSocket included.
    tt(&config.0, &["--json", "run", "llama3", "--host", &host]).success();
    let endpoint = stdout_json(&tt(&config.0, &["--json", "endpoint", "--host", &host]).success());
    assert_eq!(endpoint["model"], "llama3");
    let follow = tt(&config.0, &["logs", "--follow", "--host", &host]).success();
    assert!(String::from_utf8_lossy(&follow.get_output().stdout).contains("log line 2"));
    let found = stdout_json(
        &tt(
            &config.0,
            &["--json", "discover", "--host", &host, "--no-mdns"],
        )
        .success(),
    );
    assert_eq!(found[0]["name"], "mock-box");

    // --- `--fp` and pairing links are checked before anything is sent.
    let wrong = "ab".repeat(32);
    let other = TempDir::new("config-fp");
    let refused = tt(&other.0, &["pair", &host, "--code", "1", "--fp", &wrong]).failure();
    assert!(
        stderr(&refused).contains("certificate mismatch"),
        "{}",
        stderr(&refused)
    );
    assert!(!other.0.join("secrets.json").exists());
    tt(
        &other.0,
        &["--json", "pair", &host, "--code", "1", "--fp", &sha[..16]],
    )
    .success();

    let link = |fp: &str| {
        format!(
            "ttstation://pair?name=mock&host=127.0.0.2&port={ctrl}&pair=p&code=1&tls={tls}&fp={fp}"
        )
    };
    let by_link = TempDir::new("config-link");
    let refused = tt(&by_link.0, &["pair", "--link", &link(&wrong)]).failure();
    assert!(stderr(&refused).contains("certificate mismatch"));
    let ok = stdout_json(&tt(&by_link.0, &["--json", "pair", "--link", &link(&sha)]).success());
    assert_eq!(ok["host"], host);

    // --- Migration: a token-only pairing from before HTTPS is pinned on first use.
    let old = TempDir::new("config-old");
    std::fs::write(
        old.0.join("secrets.json"),
        serde_json::json!({ &host: "mock-token" }).to_string(),
    )
    .unwrap();
    let migrated = tt(&old.0, &["--json", "endpoint", "--host", &host]).success();
    assert!(
        stderr(&migrated).contains("trust on first use"),
        "{}",
        stderr(&migrated)
    );
    let stored = std::fs::read_to_string(old.0.join("secrets.json")).unwrap();
    assert!(
        stored.contains(&sha) && stored.contains("mock-token"),
        "{stored}"
    );
    // Only once.
    let again = tt(&old.0, &["--json", "endpoint", "--host", &host]).success();
    assert!(!stderr(&again).contains("trust on first use"));

    // --- The box comes back with a different certificate: refused, plainly.
    drop(mock);
    let cert_b = TempDir::new("cert-b");
    let _mock = spawn_mock_box(ctrl, tls, &cert_b.0);
    for cmd in [
        vec!["endpoint", "--host", &host],
        vec!["status", "--host", &host],
        vec!["logs", "--follow", "--host", &host],
    ] {
        let changed = tt(&config.0, &cmd).failure();
        let err = stderr(&changed);
        assert!(err.contains("certificate changed"), "{cmd:?}: {err}");
        assert!(err.contains(&sha), "names the pinned certificate: {err}");
    }
    // The pin is kept: re-pairing is the person's call, not the CLI's.
    let stored = std::fs::read_to_string(config.0.join("secrets.json")).unwrap();
    assert!(stored.contains(&sha));
}
