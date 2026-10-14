//! `RemoteStation` against the real `mock-box` binary (no hardware, no real agent).

use libttstation::run_options::RunOptions;
mod common;
use common::{free_port, spawn_mock_box, MockBox};

use tt_station_tray::remote::RemoteStation;
use tt_station_tray::station::Station;

fn station(m: &MockBox) -> Station {
    Station::Remote(RemoteStation::new(
        m.base.clone(),
        "mock",
        Some("tok".into()),
    ))
}

/// mock-box's test-only echo of the last run's options.
async fn last_run_options(base: &str) -> serde_json::Value {
    reqwest::get(format!("{base}/status"))
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["last_run_options"]
        .clone()
}

#[tokio::test]
async fn remote_state_maps_status_and_serving() {
    let m = spawn_mock_box(&[]);
    let st = station(&m);

    let idle = st.state().await;
    assert!(idle.reachable);
    assert_eq!(idle.mesh.as_deref(), Some("p300x2"));
    assert!(idle.serving.is_none());
    assert!(idle.paired);
    assert_eq!(idle.id.0, m.base.trim_start_matches("http://"));

    st.run("Llama-3.1-8B-Instruct", false, &RunOptions::default())
        .await
        .unwrap();
    let serving = st.state().await.serving.expect("serving after run");
    assert_eq!(serving.model, "Llama-3.1-8B-Instruct");
    assert!(
        serving.base_url.ends_with("/v1"),
        "got {}",
        serving.base_url
    );
}

#[tokio::test]
async fn remote_unreachable_is_a_value_not_an_error() {
    // Nothing listens on this port.
    let st = Station::Remote(RemoteStation::new(
        format!("http://127.0.0.1:{}", free_port()),
        "gone",
        None,
    ));
    let s = st.state().await;
    assert!(!s.reachable);
    assert!(!s.paired);
    assert!(s.serving.is_none() && s.mesh.is_none());
}

#[tokio::test]
async fn remote_run_sends_options_on_the_wire() {
    let m = spawn_mock_box(&[]);
    let options = RunOptions {
        port: Some(20001),
        ..RunOptions::default()
    };
    station(&m).run("m", false, &options).await.unwrap();
    // Wiring assertion: what the SERVER saw, not what the client built.
    assert_eq!(last_run_options(&m.base).await["port"], 20001);
}

#[tokio::test]
async fn remote_run_409_sets_contention_text_and_returns_err() {
    let m = spawn_mock_box(&["--run-conflict", "chips held by claude:vllm until 5pm"]);
    let st = station(&m);

    let err = st
        .run("m", false, &RunOptions::default())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("claude:vllm"),
        "error lost the holder: {err}"
    );

    // The contention text survives state() polls.
    for _ in 0..2 {
        let c = st.state().await.contention.expect("contention kept");
        assert!(c.contains("claude:vllm"), "got {c}");
    }
}

#[tokio::test]
async fn remote_successful_run_clears_contention() {
    // The first run is refused, the second is accepted: same station object throughout.
    let m = spawn_mock_box(&[
        "--run-conflict",
        "held by someone",
        "--run-conflict-count",
        "1",
    ]);
    let st = station(&m);
    assert!(st.run("m", false, &RunOptions::default()).await.is_err());
    assert!(st.state().await.contention.is_some());
    st.run("m", false, &RunOptions::default()).await.unwrap();
    assert!(
        st.state().await.contention.is_none(),
        "a successful run must clear it"
    );
}

#[tokio::test]
async fn remote_unpaired_run_is_refused_without_a_request() {
    let st = Station::Remote(RemoteStation::new(
        format!("http://127.0.0.1:{}", free_port()),
        "x",
        None,
    ));
    let err = st
        .run("m", false, &RunOptions::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not paired"), "got {err}");
}

#[tokio::test]
async fn remote_models_stop_logs_and_schema_pass_through() {
    let m = spawn_mock_box(&[]);
    let st = station(&m);
    let rows = st.models().await.unwrap();
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r.source == "box"));
    assert!(st
        .schema()
        .await
        .unwrap()
        .options
        .iter()
        .any(|o| o.name == "port"));
    st.logs(5).await.unwrap();
    st.run("m", false, &RunOptions::default()).await.unwrap();
    st.stop().await.unwrap();
    assert!(st.state().await.serving.is_none());
}

/// Copilot review of PR #2: Restart on a box whose backend takes no run options (mock-box
/// reports `runpy` in `/config`, as does the agent's default) refuses non-empty options BEFORE
/// it stops anything. The box keeps serving the old model.
#[tokio::test]
async fn remote_replace_refuses_options_a_runpy_box_cannot_take_and_stops_nothing() {
    let m = spawn_mock_box(&[]);
    let st = station(&m);
    st.run("Old-Model", false, &RunOptions::default())
        .await
        .unwrap();
    let opts = RunOptions {
        port: Some(20001),
        ..RunOptions::default()
    };
    let err = st
        .replace("New-Model", false, &opts)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("runpy"), "{err}");
    assert!(err.contains("still running"), "{err}");
    let now = st
        .state()
        .await
        .serving
        .expect("the old serve is untouched");
    assert_eq!(now.model, "Old-Model");

    // No options: the replacement goes ahead.
    st.replace("New-Model", false, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(st.state().await.serving.unwrap().model, "New-Model");
}
