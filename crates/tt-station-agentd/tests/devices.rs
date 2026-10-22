// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Paired devices over HTTP (`GET /devices`, `DELETE /devices/{id}`,
//! `POST /reset`), end to end through the real router: names and platforms
//! from the pairing request, the caller marked as this device, one device
//! signed out without the others, signing yourself out, and the plain 401
//! an expired or removed device gets.

use std::sync::Arc;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::NullBackend;

async fn spawn_with(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Pair with `body` merged into the `/pair/complete` request; returns the
/// whole response (`token`, `device_id`).
async fn pair_with(client: &reqwest::Client, state: &AppState, base: &str, body: Value) -> Value {
    let init: Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pair_id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&pair_id).unwrap();
    let mut req = json!({ "pair_id": pair_id, "code": code });
    for (k, v) in body.as_object().unwrap() {
        req[k] = v.clone();
    }
    let resp = client
        .post(format!("{base}/pair/complete"))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

fn token(r: &Value) -> String {
    r["token"].as_str().unwrap().to_string()
}

async fn devices(client: &reqwest::Client, base: &str, token: &str) -> reqwest::Response {
    client
        .get(format!("{base}/devices"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

fn new_state() -> AppState {
    AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(NullBackend))
}

#[tokio::test]
async fn devices_are_listed_with_names_and_without_secrets() {
    let state = new_state();
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();

    let mac = pair_with(
        &client,
        &state,
        &base,
        json!({ "name": "Jane's MacBook", "platform": "macOS" }),
    )
    .await;
    let old_desktop = pair_with(&client, &state, &base, json!({ "label": "qb2 desktop" })).await;
    let anonymous = pair_with(&client, &state, &base, json!({})).await;

    let resp = devices(&client, &base, &token(&mac)).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    for r in [&mac, &old_desktop, &anonymous] {
        assert!(
            !text.contains(&token(r)),
            "a token leaked into GET /devices"
        );
        assert!(
            !text.contains(&tt_station_agentd::sessions::token_digest(&token(r))),
            "a digest leaked"
        );
    }
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["idle_expiry_days"], 90);
    let list = body["devices"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    let by_id = |r: &Value| {
        list.iter()
            .find(|d| d["id"] == r["device_id"])
            .unwrap()
            .clone()
    };

    let me = by_id(&mac);
    assert_eq!(me["name"], "Jane's MacBook");
    assert_eq!(me["platform"], "macOS");
    assert_eq!(me["this_device"], true);
    assert!(me["paired"].as_u64().is_some());
    assert!(me["expires"].as_u64().unwrap() > me["last_used"].as_u64().unwrap());

    // An older desktop's `label` stands in for a name.
    assert_eq!(by_id(&old_desktop)["name"], "qb2 desktop");
    assert_eq!(by_id(&old_desktop)["this_device"], false);
    // No name at all: an honest default.
    assert!(by_id(&anonymous)["name"]
        .as_str()
        .unwrap()
        .starts_with("Device paired "));
    assert!(by_id(&anonymous)["platform"].is_null());
}

#[tokio::test]
async fn removing_a_device_signs_out_only_that_device() {
    let state = new_state();
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();
    // The laptop pairs first, so it's the owner; removing another device is an owner's.
    let laptop = pair_with(
        &client,
        &state,
        &base,
        json!({ "name": "Work laptop", "platform": "Linux" }),
    )
    .await;
    let phone = pair_with(
        &client,
        &state,
        &base,
        json!({ "name": "Jane's iPhone", "platform": "iOS" }),
    )
    .await;

    let id = phone["device_id"].as_str().unwrap();
    let resp = client
        .delete(format!("{base}/devices/{id}"))
        .bearer_auth(token(&laptop))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["removed"], id);
    assert_eq!(body["this_device"], false);

    // The removed phone is turned away at once, with a plain way out.
    let refused = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(token(&phone))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 401);
    assert_eq!(refused.headers()["www-authenticate"], "Bearer");
    let err: Value = refused.json().await.unwrap();
    assert!(
        err["error"].as_str().unwrap().contains("Pair it again"),
        "{err}"
    );

    // The laptop still works, and the phone is gone from the list.
    let list: Value = devices(&client, &base, &token(&laptop))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list["devices"].as_array().unwrap().len(), 1);

    // Removing it again is a 404, not a silent success.
    let again = client
        .delete(format!("{base}/devices/{id}"))
        .bearer_auth(token(&laptop))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 404);

    // The removal is in the box's event feed, with no token in it.
    let events: Value = client
        .get(format!("{base}/events?since=0"))
        .bearer_auth(token(&laptop))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let feed = events.to_string();
    assert!(feed.contains("Device removed: Jane's iPhone"), "{feed}");
    assert!(
        feed.contains("New device paired: Work laptop (Linux)"),
        "{feed}"
    );
    assert!(!feed.contains(&token(&phone)) && !feed.contains(&token(&laptop)));
}

#[tokio::test]
async fn a_device_can_sign_itself_out() {
    let state = new_state();
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();
    let me = pair_with(&client, &state, &base, json!({ "name": "This one" })).await;
    let id = me["device_id"].as_str().unwrap();

    let resp = client
        .delete(format!("{base}/devices/{id}"))
        .bearer_auth(token(&me))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["this_device"], true);

    assert_eq!(devices(&client, &base, &token(&me)).await.status(), 401);
}

#[tokio::test]
async fn device_routes_need_a_paired_device() {
    let state = new_state();
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();
    let resp = client.get(format!("{base}/devices")).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    let resp = client
        .delete(format!("{base}/devices/abc"))
        .bearer_auth("never-issued")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn reset_signs_out_every_device() {
    let state = new_state();
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();
    let a = pair_with(&client, &state, &base, json!({ "name": "A" })).await;
    let b = pair_with(&client, &state, &base, json!({ "name": "B" })).await;
    let resp = client
        .post(format!("{base}/reset"))
        .bearer_auth(token(&a))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    for r in [&a, &b] {
        assert_eq!(devices(&client, &base, &token(r)).await.status(), 401);
    }
}

#[tokio::test]
async fn an_expired_device_is_told_so_and_asked_to_pair_again() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let digest = tt_station_agentd::sessions::token_digest;
    let session = |id: &str, token: &str, last_used: u64| tt_station_agentd::sessions::Session {
        id: id.into(),
        token_sha256: digest(token),
        name: id.into(),
        platform: None,
        created: Some(now - 200 * 86_400),
        last_used,
        owner: true,
    };

    // An agent that stayed up while a device went 91 days unused.
    let state = new_state();
    state
        .sessions()
        .insert_for_test(session("stale", "stale-token", now - 91 * 86_400));
    state
        .sessions()
        .insert_for_test(session("fresh", "fresh-token", now - 89 * 86_400));
    let base = spawn_with(state.clone()).await;
    let client = reqwest::Client::new();

    let resp = devices(&client, &base, "stale-token").await;
    assert_eq!(resp.status(), 401);
    let err: Value = resp.json().await.unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap()
            .contains("expired after 90 days"),
        "{err}"
    );

    // 89 days idle still works, and that use restarts its clock.
    let list: Value = devices(&client, &base, "fresh-token")
        .await
        .json()
        .await
        .unwrap();
    let list = list["devices"].as_array().unwrap();
    assert_eq!(list.len(), 1, "the expired device isn't listed");
    assert!(list[0]["last_used"].as_u64().unwrap() >= now);

    // A store holding an expired device drops it when the agent starts.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agentd-tokens.json");
    let file = json!({ "version": 2, "sessions": [
        session("stale", "stale-token", now - 91 * 86_400),
        session("fresh", "fresh-token", now),
    ] });
    std::fs::write(&path, file.to_string()).unwrap();
    let restarted =
        AppState::new_persisting("qb2-lab".into(), "4xBH".into(), Arc::new(NullBackend), path);
    assert_eq!(restarted.sessions().len(), 1);
    assert!(!restarted.sessions().is_live("stale-token"));
    assert!(restarted.sessions().is_live("fresh-token"));
}
