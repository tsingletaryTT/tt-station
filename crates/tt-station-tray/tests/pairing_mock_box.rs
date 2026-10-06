//! Pairing and discovery against the real `mock-box` binary, plus a cross-check with the real
//! `tt-station` CLI binary that tokens are stored the same way by both.

mod common;
use common::spawn_mock_box;

use std::process::Command;
use std::time::Duration;

use libttstation::agent_client::AgentClient;
use libttstation::secrets::{FileStore, SecretStore};
use tt_station_tray::discover::discover_manual_only;
use tt_station_tray::pairing_flow::{open_store, pair_station, token_for};
use tt_station_tray::registry::KnownStation;
use tt_station_tray::station::StationId;

fn known(host: &str, port: u16) -> KnownStation {
    KnownStation {
        name: host.to_string(),
        host: host.to_string(),
        port,
        mac: None,
    }
}

#[tokio::test]
async fn pairing_flow_pairs_against_mock_box_and_stores_token() {
    let m = spawn_mock_box(&["--pair-code", "424242"]);
    let dir = tempfile::tempdir().unwrap();
    let store = FileStore::new(dir.path().join("secrets.json"));

    pair_station(&m.base, || Some("424242".to_string()), &store)
        .await
        .expect("pairing with the right code");

    let id = StationId(m.hostport.clone());
    let token = token_for(&store, &id).unwrap().expect("a token is stored");
    assert_eq!(token, "mock-token");
    // The token works: a client built from it can run a model.
    let ep = AgentClient::new(m.base.clone(), token)
        .run("Llama-3.1-8B-Instruct", false)
        .await
        .expect("run with the stored token");
    assert!(ep.base_url.ends_with("/v1"), "got {}", ep.base_url);
}

#[tokio::test]
async fn pairing_flow_wrong_code_returns_err_and_stores_nothing() {
    let m = spawn_mock_box(&["--pair-code", "424242"]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.json");
    let store = FileStore::new(path.clone());

    let err = pair_station(&m.base, || Some("000000".to_string()), &store)
        .await
        .expect_err("a wrong code must fail");
    assert!(!format!("{err:#}").is_empty());
    assert!(token_for(&store, &StationId(m.hostport.clone()))
        .unwrap()
        .is_none());
    assert!(!path.exists(), "nothing was written at all");
}

#[tokio::test]
async fn pairing_flow_abort_never_calls_pair_complete_and_stores_nothing() {
    let m = spawn_mock_box(&["--pair-code", "424242"]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.json");
    let store = FileStore::new(path.clone());

    let err = pair_station(&m.base, || None, &store)
        .await
        .expect_err("an aborted pairing must fail");
    assert!(format!("{err:#}").contains("cancelled"), "{err:#}");
    assert!(token_for(&store, &StationId(m.hostport.clone()))
        .unwrap()
        .is_none());
    assert!(!path.exists(), "nothing was written at all");

    // The box never saw a pair-complete request, so the attempt did not count against it.
    let status: serde_json::Value = reqwest::get(format!("{}/status", m.base))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["pair_complete_calls"], 0, "status: {status}");
}

/// One test owns `TT_CONFIG_DIR` for this process; the others pass an explicit store.
#[tokio::test]
async fn token_stored_by_the_cli_key_is_found_by_the_tray_and_the_reverse() {
    let m = spawn_mock_box(&["--pair-code", "424242"]);
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("TT_CONFIG_DIR", dir.path());

    // CLI -> tray: pair with the real `tt-station` binary, read through the tray's lookup.
    let cli = escargot::CargoBuild::new()
        .bin("tt-station")
        .package("tt-station")
        .run()
        .expect("build tt-station");
    let status = Command::new(cli.path())
        .env("TT_CONFIG_DIR", dir.path())
        .args(["--json", "pair", &m.hostport, "--code", "424242"])
        .status()
        .unwrap();
    assert!(status.success(), "CLI pair failed");
    let id = StationId(m.hostport.clone());
    let store = open_store();
    assert_eq!(
        token_for(store.as_ref(), &id).unwrap().as_deref(),
        Some("mock-token"),
        "a station paired with the CLI is already paired in the tray"
    );

    // tray -> CLI: forget it, pair through the tray, and let the CLI use the token.
    store.delete(&id.0).unwrap();
    assert!(token_for(store.as_ref(), &id).unwrap().is_none());
    pair_station(&m.base, || Some("424242".to_string()), store.as_ref())
        .await
        .unwrap();
    let out = Command::new(cli.path())
        .env("TT_CONFIG_DIR", dir.path())
        .args(["--json", "run", "llama3", "--host", &m.hostport])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CLI run with a tray-written token failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // And the file really is where the CLI keeps it.
    let direct = FileStore::new(dir.path().join("secrets.json"));
    assert_eq!(
        direct.get(&m.hostport).unwrap().as_deref(),
        Some("mock-token")
    );
}

#[tokio::test]
async fn discover_all_includes_manual_stations_that_answer_status() {
    let m = spawn_mock_box(&["--name", "tower"]);
    let found = discover_manual_only(&[known("127.0.0.1", m.port)], Duration::from_secs(2)).await;
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].name, "tower");
    assert_eq!(found[0].ctrl_port, m.port);
    assert_eq!(found[0].device_mesh.as_deref(), Some("p300x2"));
}

#[tokio::test]
async fn discover_all_skips_manual_stations_that_do_not() {
    let live = spawn_mock_box(&[]);
    let dead_port = common::free_port();
    let found = discover_manual_only(
        &[known("127.0.0.1", dead_port), known("127.0.0.1", live.port)],
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(found.len(), 1, "got {found:?}");
    assert_eq!(found[0].ctrl_port, live.port);
}
