// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The served chat model's liveness (`crate::liveness`): a model whose server
//! is up but whose completions hang shows as `degraded` in `/status` after a few
//! missed one-token checks, and `ok` again once it answers. A stub OpenAI server
//! plays the model; a flag makes its completions hang.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use libttstation::model::Endpoint;
use serde_json::{json, Value};
use tt_station_agentd::liveness::{check_once, Config};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

#[derive(Clone, Default)]
struct Stub {
    hang: Arc<AtomicBool>,
    completions: Arc<AtomicUsize>,
}

/// A stub vLLM: `/v1/models` always answers (the server is up); chat
/// completions answer one token, or hang while `hang` is set.
async fn stub_model(stub: Stub) -> u16 {
    let router = Router::new()
        .route(
            "/v1/models",
            get(|| async { Json(json!({ "data": [{ "id": "Qwen/Qwen3-32B" }] })) }),
        )
        .route(
            "/v1/chat/completions",
            post(|State(s): State<Stub>| async move {
                s.completions.fetch_add(1, Ordering::SeqCst);
                if s.hang.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Json(json!({ "choices": [{ "message": { "content": "p" } }] }))
            }),
        )
        .with_state(stub);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    port
}

const FAST: Config = Config {
    every: Duration::from_secs(45),
    deadline: Duration::from_millis(200),
    threshold: 3,
};

async fn harness() -> (AppState, Stub, String) {
    let stub = Stub::default();
    let port = stub_model(stub.clone()).await;
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_serving_config("127.0.0.1".into(), port);
    // Up on the serving port (as after an agent restart under a loaded model).
    state.adopt_serving(Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "Qwen/Qwen3-32B".into(),
        requires_key: false,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (state, stub, base)
}

async fn status(base: &str) -> Value {
    reqwest::get(format!("{base}/status"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_hung_model_turns_degraded_and_recovers_when_it_answers() {
    let (state, stub, base) = harness().await;
    assert_eq!(status(&base).await["model_health"]["state"], "unknown");

    check_once(&state, FAST).await;
    let s = status(&base).await;
    assert_eq!(s["model_health"]["state"], "ok", "{s}");
    assert_eq!(s["status"], "serving:Qwen/Qwen3-32B");

    // The server stays up (`/v1/models` answers) but completions hang.
    stub.hang.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        check_once(&state, FAST).await;
    }
    assert_eq!(
        status(&base).await["model_health"]["state"],
        "ok",
        "two misses aren't enough"
    );
    check_once(&state, FAST).await;
    let s = status(&base).await;
    let health = &s["model_health"];
    assert_eq!(health["state"], "degraded", "{s}");
    assert!(
        health["reason"]
            .as_str()
            .unwrap()
            .contains("no answer within"),
        "{health}"
    );
    assert_eq!(health["restart"], "/doctor/fix/restart_model");
    // Still the served model: older clients keep parsing `status`.
    assert_eq!(s["status"], "serving:Qwen/Qwen3-32B");
    // `/serve` needs a paired device.
    let token = support::pair(&base, &state).await;
    assert_eq!(
        reqwest::Client::new()
            .get(format!("{base}/serve"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["health"],
        "degraded"
    );

    stub.hang.store(false, Ordering::SeqCst);
    check_once(&state, FAST).await;
    let s = status(&base).await;
    assert_eq!(s["model_health"]["state"], "ok", "{s}");
    assert_eq!(s["model_health"]["reason"], Value::Null);
}

/// While the model is generating tokens for real requests, the check doesn't
/// add one of its own.
#[tokio::test]
async fn a_busy_model_is_not_probed() {
    let (state, stub, base) = harness().await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    state.history().record(
        tt_station_agentd::history::Point {
            t: now,
            gen_tps: Some(41.0),
            ..Default::default()
        },
        1,
        "today",
    );
    check_once(&state, FAST).await;
    assert_eq!(stub.completions.load(Ordering::SeqCst), 0);
    assert_eq!(status(&base).await["model_health"]["state"], "ok");
}

#[tokio::test]
async fn nothing_serving_is_not_probed() {
    let stub = Stub::default();
    let _port = stub_model(stub.clone()).await;
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend));
    check_once(&state, FAST).await;
    assert_eq!(stub.completions.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.liveness().health().state,
        tt_station_agentd::liveness::State::Unknown
    );
}
