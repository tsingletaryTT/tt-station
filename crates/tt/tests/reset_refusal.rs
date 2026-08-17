//! `tt reset --host <h>` against a box that REFUSES the reset (HTTP 409).
//!
//! The agent refuses a whole-box reset when another tenant holds chips, or
//! when it cannot determine whether one does (see
//! `tt-station-agentd`'s `gozer::foreign_leases`). The server deliberately
//! preserves pairing on a refusal -- nothing was reset, so nothing should be
//! forgotten. This test exists because the CLI used to throw that away: it
//! treated ANY reset failure as a warning and cleared the operator's local
//! token regardless, so a correctly-refused reset left the box untouched,
//! the token destroyed, and no indication of who held the box.
//!
//! Driven through the real `tt` binary against a hand-rolled one-shot HTTP
//! server (a dozen lines of `std::net`) rather than a mock framework: the
//! only thing under test is what the binary does with a 409, and the tt
//! crate has no HTTP-mocking dev-dependency. `TT_CONFIG_DIR` points the
//! binary's secret store at a temp dir, so no real `~/.config` is touched.

use std::io::{Read, Write};
use std::net::TcpListener;

use assert_cmd::Command as AssertCommand;

/// The refusal body the agent actually sends (see
/// `tt-station-agentd`'s `contention_aware_error` + `ForeignLeases::refusal_reason`).
const REFUSAL_BODY: &str = r#"{"error":"refusing to reset this box: it would run a WHOLE-BOX `tt-smi -r` -- another tenant holds chips -- board 0100014311601055 is held by claude:ttm-optimize. Stop that session (or `gozer release` its lease) first."}"#;

/// Serve exactly one HTTP request on `listener`, answering `409` with
/// [`REFUSAL_BODY`], then return. Runs on a background thread; the test
/// makes exactly one request.
fn serve_one_409(listener: TcpListener) {
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        // Read just enough of the request that the client's write completes;
        // we don't parse it -- the only route the binary calls here is
        // `POST /reset`.
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf);
        let response = format!(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            REFUSAL_BODY.len(),
            REFUSAL_BODY
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    });
}

/// A refused `tt reset --host` must FAIL, name the holder, and leave the
/// stored token exactly where it was.
///
/// The surviving token is the assertion that matters: it is the side channel
/// proving the CLI honoured the server's "nothing happened". A test that
/// only checked the exit code would pass against a version that printed a
/// clean error and then wiped the store anyway -- which is precisely the bug
/// this covers.
#[test]
fn refused_reset_fails_loudly_and_keeps_local_pairing() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("read the bound address");
    let host = format!("127.0.0.1:{}", addr.port());
    serve_one_409(listener);

    // A config dir holding one paired box -- the same `secrets.json` shape
    // `FileStore` writes.
    let dir = tempfile::tempdir().expect("temp config dir");
    let secrets = dir.path().join("secrets.json");
    std::fs::write(
        &secrets,
        serde_json::json!({ &host: "tok-abc123" }).to_string(),
    )
    .expect("seed secrets.json");

    let assert = AssertCommand::cargo_bin("tt")
        .expect("built tt binary")
        .env("TT_CONFIG_DIR", dir.path())
        .args(["reset", "--host", &host, "--yes"])
        .assert()
        .failure();

    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("claude:ttm-optimize"),
        "the operator must be told WHO holds the box: {stderr}"
    );
    assert!(
        !stderr.contains("clearing local state anyway"),
        "a refusal is not a reason to forget the box: {stderr}"
    );

    let after = std::fs::read_to_string(&secrets).expect("secrets.json still readable");
    assert!(
        after.contains("tok-abc123"),
        "the box refused and reset NOTHING, so the pairing it preserved server-side \
         must survive locally too -- got: {after}"
    );
}
