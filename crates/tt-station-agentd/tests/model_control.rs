// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Who may start, stop and switch models (`src/model_control.rs`), through
//! the real router: the owner-set `model_control` setting (default "anyone",
//! owners only to change, logged), and every route that starts, stops or
//! switches serving refusing a member with one clear `403` when it's
//! "owners" -- while owners, the box's own user, and the routes that only
//! read stay as they were.
//!
//! The assistant's tools, the Doctor's restart, check-ups and routines are
//! covered beside their own tests (`tests/assistant.rs`, `tests/evals_*.rs`,
//! `tests/routines_api.rs`).

use std::sync::Arc;

use reqwest::Method as M;
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, local_http_app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

const REFUSED: &str = "Only owners can start or stop models on this box.";

/// Every route that starts, stops or switches serving, with a body it takes.
fn switching_routes() -> Vec<(M, &'static str, Value)> {
    vec![
        (M::POST, "/run", json!({ "model": "Qwen/Qwen3-32B" })),
        (M::POST, "/stop", json!({})),
        (M::POST, "/serve", json!({ "model": "Qwen/Qwen3-32B" })),
        (M::POST, "/modes/chat", json!({})),
        (
            M::POST,
            "/coserve",
            json!({ "chat": "Qwen/Qwen3-32B", "image": "FLUX.1-schnell" }),
        ),
        (M::POST, "/community/serve", json!({ "id": "acme/chat" })),
        (
            M::POST,
            "/vllm/native/serve",
            json!({ "model": "acme/chat" }),
        ),
        (M::POST, "/jobs/abc123/run", json!({})),
        (M::POST, "/evals", json!({})),
        (M::POST, "/evals/stop", json!({})),
        (M::PUT, "/preferences/models", json!({})),
        (M::PUT, "/idle", json!({})),
        (M::POST, "/idle/wake", json!({})),
        (M::POST, "/idle/sleep", json!({})),
        (
            M::POST,
            "/schedules",
            json!({ "mode": "chat", "time": "08:00", "days": [1] }),
        ),
        (M::PATCH, "/schedules/abc", json!({ "enabled": false })),
        (M::POST, "/doctor/fix/restart_model", json!({})),
    ]
}

/// Of those, the ones whose handlers refuse here without touching serving
/// (nothing is serving, no job queue, no such schedule, no second slot), so
/// a request that gets past the gate is safe to make in a test.
fn harmless_routes() -> Vec<(M, &'static str, Value)> {
    let harmless = [
        "/evals",
        "/evals/stop",
        "/jobs/abc123/run",
        "/schedules/abc",
        "/coserve",
        // Nothing serves in these tests: refused (409) before it stops anything.
        "/idle/sleep",
    ];
    switching_routes()
        .into_iter()
        .filter(|(_, p, _)| harmless.contains(p))
        .collect()
}

struct Station {
    base: String,
    owner: String,
    member: String,
    state: AppState,
    client: reqwest::Client,
    _config: tempfile::TempDir,
}

async fn the_box() -> Station {
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        // Never the box's real serving port.
        .with_serving_config("127.0.0.1".into(), 9)
        .with_config_dir(config.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        base,
        owner,
        member,
        state,
        client: reqwest::Client::new(),
        _config: config,
    }
}

impl Station {
    async fn call(&self, method: M, path: &str, token: &str, body: Value) -> (u16, Value) {
        let mut r = self
            .client
            .request(method.clone(), format!("{}{path}", self.base))
            .bearer_auth(token);
        if method != M::GET && method != M::DELETE {
            r = r.json(&body);
        }
        let r = r.send().await.unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn set(&self, token: &str, control: &str) -> (u16, Value) {
        self.call(
            M::PUT,
            "/data/settings",
            token,
            json!({ "model_control": control }),
        )
        .await
    }
}

#[tokio::test]
async fn the_setting_is_anyone_by_default_and_only_owners_change_it() {
    let b = the_box().await;
    let (status, v) = b
        .call(M::GET, "/data/settings", &b.member, json!(null))
        .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["settings"]["model_control"], "anyone");
    assert_eq!(v["may_control_models"], true);

    let (status, _) = b.set(&b.member, "owners").await;
    assert_eq!(status, 403, "a member can't change it");
    let (status, v) = b.set(&b.owner, "members").await;
    assert!(status == 400 || status == 422, "{status}: {v}");
    let (status, v) = b.set(&b.owner, "owners").await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["model_control"], "owners");
    // Other settings untouched by a patch that names only this one.
    assert_eq!(v["keep_audio"], false);

    let (_, v) = b
        .call(M::GET, "/data/settings", &b.member, json!(null))
        .await;
    assert_eq!(v["settings"]["model_control"], "owners");
    assert_eq!(v["may_control_models"], false);
    let (_, v) = b
        .call(M::GET, "/data/settings", &b.owner, json!(null))
        .await;
    assert_eq!(v["may_control_models"], true);

    // The change is logged, once, as a change; setting it again isn't one.
    let (status, _) = b.set(&b.owner, "owners").await;
    assert_eq!(status, 200);
    let (_, audit) = b.call(M::GET, "/audit", &b.owner, json!(null)).await;
    let changes: Vec<&Value> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "model_control")
        .collect();
    assert_eq!(changes.len(), 1, "{audit}");
    assert_eq!(changes[0]["target"], "anyone -> owners");
    assert!(changes[0]["device_id"].is_string());
}

#[tokio::test]
async fn when_only_owners_may_every_switching_route_refuses_members() {
    let b = the_box().await;
    // Anyone may (the default): no route refuses a member for this.
    for (method, path, body) in harmless_routes() {
        let (status, v) = b.call(method.clone(), path, &b.member, body).await;
        assert!(
            status != 403 || v["error"] != REFUSED,
            "{method} {path} refused by default: {v}"
        );
    }
    let (status, _) = b.set(&b.owner, "owners").await;
    assert_eq!(status, 200);
    let before = b.state.audit().page(0, 1000).0.len();
    for (method, path, body) in switching_routes() {
        let (status, v) = b.call(method.clone(), path, &b.member, body).await;
        assert_eq!(status, 403, "{method} {path}: {v}");
        assert_eq!(v["error"], REFUSED, "{method} {path}");
    }
    // Each refusal is in the activity log, with the member's device.
    let (entries, _) = b.state.audit().page(0, 1000);
    let denied: Vec<_> = entries[..entries.len() - before]
        .iter()
        .filter(|e| e.action == "denied")
        .collect();
    assert_eq!(denied.len(), switching_routes().len(), "{denied:?}");
    assert!(denied.iter().all(|e| e.device_id.is_some()));
    assert!(denied.iter().any(|e| e.target == "POST /modes/chat"));

    // Owners still may (whatever the box then says about the request itself).
    for (method, path, body) in harmless_routes() {
        let (status, v) = b.call(method.clone(), path, &b.owner, body).await;
        assert!(
            !(status == 403 && v["error"] == REFUSED),
            "{method} {path} refused an owner: {v}"
        );
    }
}

#[tokio::test]
async fn reading_and_the_routes_beside_them_stay_open_to_members() {
    let b = the_box().await;
    let (status, _) = b.set(&b.owner, "owners").await;
    assert_eq!(status, 200);
    for path in ["/serve", "/evals", "/coserve", "/schedules", "/modes"] {
        let (status, v) = b.call(M::GET, path, &b.member, json!(null)).await;
        assert_ne!(status, 403, "GET {path}: {v}");
    }
    // Removing a schedule stops a switch rather than making one.
    let (status, v) = b
        .call(M::DELETE, "/schedules/nope", &b.member, json!(null))
        .await;
    assert_eq!(status, 404, "{v}");
}

#[tokio::test]
async fn the_boxs_own_user_is_an_owner_here_too() {
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_serving_config("127.0.0.1".into(), 9)
        .with_config_dir(config.path().to_path_buf());
    std::fs::write(
        config.path().join("data-settings.json"),
        json!({ "model_control": "owners" }).to_string(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = local_http_app(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    // A route that takes the box's own user without a device token.
    let r = reqwest::Client::new()
        .post(format!("http://{addr}/jobs/abc123/run"))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    assert!(!(status == 403 && v["error"] == REFUSED), "{status}: {v}");
}

/// Putting the model to sleep by hand: nothing to stop on an idle box says so
/// plainly (`409`) and remembers nothing to wake.
#[tokio::test]
async fn sleeping_an_idle_box_is_refused_plainly() {
    let b = the_box().await;
    let (status, v) = b.call(M::POST, "/idle/sleep", &b.owner, json!({})).await;
    assert_eq!(status, 409, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("Nothing is serving"),
        "{v}"
    );
    let (status, v) = b.call(M::GET, "/idle", &b.owner, json!(null)).await;
    assert_eq!(status, 200);
    assert!(v["asleep"].is_null(), "{v}");
}
