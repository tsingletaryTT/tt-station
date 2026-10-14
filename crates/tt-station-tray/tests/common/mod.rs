//! Shared helper for the tray's mock-box integration tests: build and spawn the `mock-box`
//! binary from another workspace crate (same pattern as tt-station's e2e test).
#![allow(dead_code)] // each test binary uses a different subset

use std::net::TcpStream;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Kills the spawned mock-box when dropped, so a failed assertion never leaks a server.
pub struct MockBox {
    child: Child,
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    /// `127.0.0.1:<port>`: what a person types after `tt-station pair`.
    pub hostport: String,
    pub port: u16,
}

impl Drop for MockBox {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A port that was free a moment ago.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Build and start `mock-box serve`, waiting until it listens. `extra` are more `serve` flags.
pub fn spawn_mock_box(extra: &[&str]) -> MockBox {
    let built = escargot::CargoBuild::new()
        .bin("mock-box")
        .package("mock-box")
        .run()
        .expect("build mock-box");
    let port = free_port();
    let child = Command::new(built.path())
        .args(["serve", "--ctrl-port", &port.to_string()])
        .args(extra)
        .spawn()
        .expect("spawn mock-box");
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            Instant::now() < deadline,
            "mock-box never listened on {port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    MockBox {
        child,
        base: format!("http://127.0.0.1:{port}"),
        hostport: format!("127.0.0.1:{port}"),
        port,
    }
}
