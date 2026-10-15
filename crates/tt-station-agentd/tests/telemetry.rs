//! Integration test for `GET /telemetry` -- the WebSocket telemetry stream
//! (publisher half of the "remote QuietBox" feature).
//!
//! It starts the real axum `Router` (via `app()`) on an ephemeral port, with
//! `AppState` pointed at a tiny stub `tt-smi` script (via
//! `with_telemetry_config`) that echoes canned `tt-smi -s` JSON. A real
//! WebSocket client then connects and asserts it receives a text frame whose
//! original `tt-smi` telemetry is intact byte-for-content (same
//! `device_info`) -- proving the base contract: the agent never reshapes
//! `tt-smi -s`'s stdout, only additively folds in a sibling `tt_toplike` key
//! (Task 4's process-list enrichment, see `procscan`/`telemetry::enrich_frame`).
//!
//! Bounded by design: a short interval, read exactly one frame, then drop the
//! connection so the server-side loop exits on the client disconnect.
//!
//! `/telemetry` takes the box itself (its own plain HTTP, as this account) or
//! a paired device; anyone else is refused, and a member sees each process's
//! command cut to its executable's name.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tt_station_agentd::routes::{app, local_http_app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

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
    // The box's own plain HTTP: tt-toplike and the panel read it there.
    let router = local_http_app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
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

    // The original `tt-smi -s` telemetry is preserved exactly -- the agent
    // never reshapes it, only additively merges in `tt_toplike` alongside it
    // (Task 4). Compare parsed `device_info` (not raw bytes) since the
    // enriched frame is a superset of the canned JSON.
    let canned: serde_json::Value = serde_json::from_str(CANNED_TT_SMI_JSON).unwrap();
    let got: serde_json::Value =
        serde_json::from_str(&text).expect("telemetry frame was not valid JSON");
    assert_eq!(
        got["device_info"], canned["device_info"],
        "tt-smi telemetry should be passed through unreshaped"
    );

    // And the process-list enrichment rode along on this tick.
    assert_eq!(got["tt_toplike"]["schema"], 1);
    assert!(got["tt_toplike"]["processes"].is_array());

    // Dropping `ws` closes the connection, so the server loop exits.
    drop(ws);
}

/// One full telemetry frame for `token` (or none) from the network router.
async fn frame(addr: std::net::SocketAddr, token: Option<&str>) -> Result<serde_json::Value, u16> {
    let mut req = format!("ws://{addr}/telemetry")
        .into_client_request()
        .unwrap();
    if let Some(t) = token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    let (mut ws, _) = match connect_async(req).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            return Err(resp.status().as_u16())
        }
        Err(e) => panic!("{e}"),
    };
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for a telemetry frame")
        .expect("stream closed before any frame")
        .expect("frame was an error");
    Ok(serde_json::from_str(&msg.into_text().unwrap()).unwrap())
}

#[tokio::test]
async fn telemetry_needs_pairing_and_hides_command_lines_from_members() {
    let stub = StubTtSmi::new();
    let state = AppState::new(
        "qb2-lab".to_string(),
        "4xBH".to_string(),
        Arc::new(DstackBackend),
    )
    .with_telemetry_config(stub.path(), 50);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");

    // Not paired: no stream at all.
    assert_eq!(frame(addr, None).await.unwrap_err(), 401);

    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;

    // A member's process list names executables only: no arguments, no paths.
    let got = frame(addr, Some(&member)).await.unwrap();
    for p in got["tt_toplike"]["processes"].as_array().unwrap() {
        let cmd = p["cmd"].as_str().unwrap();
        assert!(
            !cmd.contains(' ') && !cmd.contains('/'),
            "a member saw a command line: {cmd}"
        );
    }
    // An owner still gets the frame (with whole command lines).
    let got = frame(addr, Some(&owner)).await.unwrap();
    assert!(got["tt_toplike"]["processes"].is_array());
}
