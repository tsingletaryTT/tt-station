//! Integration test for `GET /telemetry` -- the WebSocket telemetry stream
//! (publisher half of the "remote QuietBox" feature).
//!
//! It starts the real axum `Router` (via `app()`) on an ephemeral port, with
//! `AppState` pointed at a tiny stub `tt-smi` script (via
//! `with_telemetry_config`) that echoes canned `tt-smi -s` JSON. A real
//! WebSocket client then connects and asserts it receives a text frame that is
//! the **verbatim** stub stdout -- proving the contract: a frame is exactly
//! `tt-smi -s`'s stdout, unreshaped.
//!
//! Bounded by design: a short interval, read exactly one frame, then drop the
//! connection so the server-side loop exits on the client disconnect.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

/// Canned `tt-smi -s` snapshot the stub script emits. The shape is
/// representative (a `tt-smi`-like object), but the test only cares that the
/// frame is byte-for-byte this string -- i.e. the agent passed stdout through
/// without reshaping it. No trailing newline: `RealCommandRunner` trims
/// captured stdout, so this is what a client sees.
const CANNED_TT_SMI_JSON: &str = r#"{"device_info":[{"board_info":{"board_type":"p150a"},"telemetry":{"asic_temperature":"61.4","aiclk":"1350"}}]}"#;

/// Write an executable stub `tt-smi` to a unique temp path. Invoked as
/// `<script> -s`, it prints the canned JSON to stdout. Kept alive (and
/// removed) by the returned guard.
struct StubTtSmi(std::path::PathBuf);

impl StubTtSmi {
    fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tt-station-stub-tt-smi-{}-{}.sh",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // `printf '%s'` emits the JSON with no trailing newline.
        let script = format!("#!/bin/sh\nprintf '%s' '{CANNED_TT_SMI_JSON}'\n");
        std::fs::write(&path, script).expect("write stub tt-smi script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod +x stub tt-smi");
        }
        StubTtSmi(path)
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for StubTtSmi {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn telemetry_stream_pushes_verbatim_tt_smi_snapshot() {
    let stub = StubTtSmi::new();

    // `/telemetry` never touches the serving backend -- `DstackBackend`'s
    // no-op stub is enough. Point the stream at the stub tt-smi with a short
    // interval so the first frame arrives promptly.
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
    .with_telemetry_config(stub.path(), 50);
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Connect a real WebSocket client and read exactly one frame.
    let url = format!("ws://{addr}/telemetry");
    let (mut ws, _resp) = connect_async(&url).await.expect("WebSocket upgrade failed");

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for a telemetry frame")
        .expect("stream closed before any frame")
        .expect("frame was an error");

    let text = msg.into_text().expect("telemetry frame was not text");
    // The frame is the verbatim stdout of `tt-smi -s` -- zero reshaping.
    assert_eq!(text.as_str(), CANNED_TT_SMI_JSON);

    // Dropping `ws` closes the connection, so the server loop exits.
    drop(ws);
}
