// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Who may do what, end to end through the real router:
//!
//! - default-deny: every route the router serves answers `401` without
//!   credentials unless it's on the public allowlist (walked from the route
//!   table in `src/router.rs`, so a route added later is covered too);
//! - owners and members: the first device paired is an owner, later ones are
//!   members until promoted, what members are refused, and the last owner
//!   staying one;
//! - people: a device can't claim a person another device already is, and a
//!   member's "forget everything" forgets only its own;
//! - the model API takes device tokens only over HTTPS;
//! - the activity log records actions and refusals, newest first;
//! - streams close when their device is removed.

use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tt_station_agentd::routes::{
    app, local_http_app, AppState, HANDLER_AUTHED_ROUTES, PUBLIC_ROUTES,
};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

fn new_state() -> AppState {
    AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
}

/// The router as the network sees it (the HTTPS server's), on a real port.
async fn spawn(state: AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// The box's own plain-HTTP server; this test process is the agent's account.
async fn spawn_local(state: AppState) -> String {
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
    format!("http://{addr}")
}

/// `(method, route)` for every `.route(..)` in the body of `fn_header` in
/// `src/router.rs`, up to the auth layer that closes it (`require_auth`; a
/// route's own layer, such as `crate::model_control`'s, doesn't).
fn routes_in(fn_header: &str) -> Vec<(String, String)> {
    let src = include_str!("../src/router.rs");
    let start = src
        .find(fn_header)
        .unwrap_or_else(|| panic!("{fn_header} not found"));
    let body = &src[start..];
    let end = regex::Regex::new(r"\.route_layer\(\s*axum::middleware::from_fn_with_state\(\s*state\.clone\(\),\s*require_auth")
        .unwrap()
        .find(body)
        .expect("the router ends with its auth layer")
        .start();
    let body = &body[..end];
    let route = regex::Regex::new(r#"\.route\(\s*"([^"]+)",\s*"#).unwrap();
    let method =
        regex::Regex::new(r#"(?:^|[\s.:])(get|post|put|patch|delete)\(\s*[A-Za-z_]"#).unwrap();
    let starts: Vec<(usize, String)> = route
        .captures_iter(body)
        .map(|c| (c.get(0).unwrap().end(), c[1].to_string()))
        .collect();
    let mut out = Vec::new();
    for (i, (at, path)) in starts.iter().enumerate() {
        let until = starts.get(i + 1).map(|(n, _)| *n).unwrap_or(body.len());
        let spec = &body[*at..until];
        let mut found = false;
        for m in method.captures_iter(spec) {
            out.push((m[1].to_uppercase(), path.clone()));
            found = true;
        }
        assert!(found, "no method found for route {path}");
    }
    out
}

/// A concrete path for a route template.
fn concrete(route: &str) -> String {
    regex::Regex::new(r"\{\*?[a-z_]+\}")
        .unwrap()
        .replace_all(route, "x")
        .into_owned()
}

#[tokio::test]
async fn every_route_needs_credentials_unless_it_is_on_the_public_allowlist() {
    let routes = routes_in("pub fn app(state: AppState) -> Router {");
    assert!(
        routes.len() > 200,
        "the route table parsed to only {} routes",
        routes.len()
    );
    for (m, r) in PUBLIC_ROUTES {
        assert!(
            routes.iter().any(|(rm, rr)| rm == m && rr == r),
            "public route {m} {r} isn't served"
        );
    }
    for r in HANDLER_AUTHED_ROUTES {
        assert!(
            routes.iter().any(|(_, rr)| rr == r),
            "handler-authed route {r} isn't served"
        );
    }

    let base = spawn(new_state()).await;
    let client = reqwest::Client::new();
    let mut public = 0;
    for (method, route) in &routes {
        let url = format!("{base}{}", concrete(route));
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let resp = client
            .request(m, &url)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        if PUBLIC_ROUTES
            .iter()
            .any(|(pm, pr)| pm == method && pr == route)
        {
            public += 1;
            assert_ne!(status, 401, "{method} {route} is public but refused");
        } else {
            assert_eq!(
                status, 401,
                "{method} {route} answered {status} without credentials"
            );
        }
    }
    assert_eq!(public, PUBLIC_ROUTES.len());

    // The routes only the box itself has are not on the network at all.
    for (method, route) in routes_in("pub fn local_http_app(state: AppState) -> Router {") {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let resp = client
            .request(m, format!("{base}{route}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            404,
            "{method} {route} is reachable from the network"
        );
    }
}

#[tokio::test]
async fn the_box_itself_needs_no_pairing_on_its_own_plain_http() {
    let base = spawn_local(new_state()).await;
    // This test runs as the agent's own account, so it's the box itself.
    let r = reqwest::get(format!("{base}/security")).await.unwrap();
    assert_eq!(r.status(), 200);
    let r = reqwest::get(format!("{base}/serving")).await.unwrap();
    assert_eq!(r.status(), 200);
}

async fn call(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Value,
) -> (u16, Value) {
    let r = client
        .request(method, url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_first_device_is_an_owner_and_members_cannot_manage_the_box() {
    let state = new_state();
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    use reqwest::Method as M;

    let (_, mine) = call(
        &client,
        M::GET,
        format!("{base}/devices"),
        &owner,
        json!({}),
    )
    .await;
    assert_eq!(mine["owner"], true);
    assert_eq!(
        mine["devices"].as_array().unwrap().len(),
        2,
        "an owner sees every device"
    );
    let member_id = mine["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["this_device"] == false)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let owner_id = mine["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["this_device"] == true)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // A member sees only itself, and is told what it is.
    let (_, theirs) = call(
        &client,
        M::GET,
        format!("{base}/devices"),
        &member,
        json!({}),
    )
    .await;
    assert_eq!(theirs["owner"], false);
    let list = theirs["devices"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], member_id.as_str());
    assert_eq!(list[0]["owner"], false);

    // What only owners may do.
    let refused = [
        (M::DELETE, format!("{base}/devices/{owner_id}"), json!({})),
        (
            M::POST,
            format!("{base}/devices/{member_id}/owner"),
            json!({ "owner": true }),
        ),
        (M::POST, format!("{base}/keys"), json!({ "name": "x" })),
        (
            M::PATCH,
            format!("{base}/keys/abc"),
            json!({ "paused": true }),
        ),
        (M::DELETE, format!("{base}/keys/abc"), json!({})),
        (
            M::POST,
            format!("{base}/ssh/authorize"),
            json!({ "public_key": "ssh-ed25519 AAAA x", "label": "x" }),
        ),
        (
            M::DELETE,
            format!("{base}/ssh/authorize"),
            json!({ "label": "x" }),
        ),
        (
            M::POST,
            format!("{base}/power"),
            json!({ "action": "reboot" }),
        ),
        (M::POST, format!("{base}/power/wake-on-lan"), json!({})),
        (M::POST, format!("{base}/reset"), json!({})),
        (
            M::PUT,
            format!("{base}/apps/n8n/network"),
            json!({ "open": true }),
        ),
        (M::POST, format!("{base}/hermes/install"), json!({})),
        (M::POST, format!("{base}/updates/install"), json!({})),
        (M::POST, format!("{base}/updates/undo"), json!({})),
        (
            M::POST,
            format!("{base}/updates/channel/approve"),
            json!({ "fingerprint": "0123456789abcdef" }),
        ),
        (M::DELETE, format!("{base}/apps/n8n"), json!({})),
        (
            M::POST,
            format!("{base}/software/cli-update"),
            json!({ "dry_run": true }),
        ),
        (M::GET, format!("{base}/audit"), json!({})),
        (
            M::PUT,
            format!("{base}/devices/{member_id}/person"),
            json!({ "person": null }),
        ),
        (
            M::POST,
            format!("{base}/bringup/codex/runs/r/approvals/a"),
            json!({ "decision": "accept" }),
        ),
    ];
    for (m, url, body) in refused {
        let (status, body) = call(&client, m.clone(), url.clone(), &member, body).await;
        assert_eq!(status, 403, "{m} {url}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("Only an owner"),
            "{body}"
        );
    }

    // What members keep: their own sign-out, chat-side reads, their memories.
    let (status, _) = call(
        &client,
        M::GET,
        format!("{base}/memories"),
        &member,
        json!({}),
    )
    .await;
    assert_eq!(status, 200);
    let (status, _) = call(&client, M::GET, format!("{base}/keys"), &member, json!({})).await;
    assert_eq!(status, 200, "seeing the keys and their use is fine");

    // An owner promotes the member, which can then make keys.
    let (status, body) = call(
        &client,
        M::POST,
        format!("{base}/devices/{member_id}/owner"),
        &owner,
        json!({ "owner": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, _) = call(
        &client,
        M::POST,
        format!("{base}/keys"),
        &member,
        json!({ "name": "Open WebUI" }),
    )
    .await;
    assert_eq!(status, 201);

    // Demoting: fine while another owner remains, refused for the last one.
    let (status, _) = call(
        &client,
        M::POST,
        format!("{base}/devices/{member_id}/owner"),
        &owner,
        json!({ "owner": false }),
    )
    .await;
    assert_eq!(status, 200);
    let (status, body) = call(
        &client,
        M::POST,
        format!("{base}/devices/{owner_id}/owner"),
        &owner,
        json!({ "owner": false }),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"].as_str().unwrap().contains("only owner"));

    // A member may still sign itself out.
    let (status, _) = call(
        &client,
        M::DELETE,
        format!("{base}/devices/{member_id}"),
        &member,
        json!({}),
    )
    .await;
    assert_eq!(status, 200);

    // The activity log: newest first, with who did it, and never a token.
    let r = client
        .get(format!("{base}/audit?limit=200"))
        .bearer_auth(&owner)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let text = r.text().await.unwrap();
    assert!(
        !text.contains(&owner) && !text.contains(&member),
        "a token reached the activity log"
    );
    let log: Value = serde_json::from_str(&text).unwrap();
    let actions: Vec<(&str, &str)> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["action"].as_str().unwrap(), e["result"].as_str().unwrap()))
        .collect();
    assert_eq!(actions[0], ("revoke", "ok"));
    for wanted in [
        ("pair", "ok"),
        ("promote", "ok"),
        ("demote", "ok"),
        ("app_key_create", "ok"),
        ("denied", "denied"),
    ] {
        assert!(
            actions.contains(&wanted),
            "{wanted:?} not logged: {actions:?}"
        );
    }
    let promote = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["action"] == "promote")
        .unwrap();
    assert_eq!(promote["device_id"], owner_id.as_str());
    let (_, page) = call(
        &client,
        M::GET,
        format!("{base}/audit?limit=2&offset=1"),
        &owner,
        json!({}),
    )
    .await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 2);
    assert_eq!(page["more"], true);
    assert_eq!(page["entries"][0]["action"], log["entries"][1]["action"]);
}

#[tokio::test]
async fn pairing_with_the_local_secret_makes_an_owner_even_later() {
    let state = new_state().with_local_secret("s3cret".into());
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    let _first = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;

    let refused = client
        .post(format!("{base}/pair/offer"))
        .header("x-tt-local-secret", "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 403);
    let offer: Value = client
        .post(format!("{base}/pair/offer"))
        .header("x-tt-local-secret", "s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": offer["pair_id"], "code": offer["code"], "name": "TT Station on the box" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let desktop = done["token"].as_str().unwrap();
    let (_, d) = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/devices"),
        desktop,
        json!({}),
    )
    .await;
    assert_eq!(d["owner"], true);
    let (_, d) = call(
        &client,
        reqwest::Method::GET,
        format!("{base}/devices"),
        &member,
        json!({}),
    )
    .await;
    assert_eq!(d["owner"], false);
}

#[tokio::test]
async fn a_device_cannot_claim_someone_another_device_already_is() {
    let state = new_state();
    let base = spawn(state.clone()).await;
    let client = reqwest::Client::new();
    use reqwest::Method as M;
    let owner = support::pair(&base, &state).await;
    let ana_phone = support::pair(&base, &state).await;
    let intruder = support::pair(&base, &state).await;

    // Ana adds herself from her phone: a new person, so that's hers.
    let (status, body) = call(
        &client,
        M::POST,
        format!("{base}/people"),
        &ana_phone,
        json!({ "name": "Ana", "me": true }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ana = body["me"].as_str().unwrap().to_string();
    state
        .memories()
        .add("Ana's bank is in Lisbon", 1, Some(&ana))
        .unwrap();
    state
        .memories()
        .add("The wifi is called Orchard", 2, None)
        .unwrap();

    // Another member can't become Ana, by id or by name.
    let (status, body) = call(
        &client,
        M::PUT,
        format!("{base}/people/me"),
        &intruder,
        json!({ "person": ana }),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body["error"].as_str().unwrap().contains("ask an owner"));
    let (status, _) = call(
        &client,
        M::POST,
        format!("{base}/people"),
        &intruder,
        json!({ "name": "ana", "me": true }),
    )
    .await;
    assert_eq!(status, 403);
    let (_, mems) = call(
        &client,
        M::GET,
        format!("{base}/memories"),
        &intruder,
        json!({}),
    )
    .await;
    assert!(
        !mems.to_string().contains("Lisbon"),
        "the intruder read Ana's memories"
    );

    // Members can't rename or remove people.
    let (status, _) = call(
        &client,
        M::PATCH,
        format!("{base}/people/{ana}"),
        &intruder,
        json!({ "name": "Bob" }),
    )
    .await;
    assert_eq!(status, 403);
    let (status, _) = call(
        &client,
        M::DELETE,
        format!("{base}/people/{ana}"),
        &intruder,
        json!({}),
    )
    .await;
    assert_eq!(status, 403);

    // A member that isn't anyone can't clear the household's memories...
    let (status, _) = call(
        &client,
        M::DELETE,
        format!("{base}/memories"),
        &intruder,
        json!({}),
    )
    .await;
    assert_eq!(status, 403);
    // ...and Ana's "forget everything" forgets only hers.
    let (status, _) = call(
        &client,
        M::DELETE,
        format!("{base}/memories"),
        &ana_phone,
        json!({}),
    )
    .await;
    assert_eq!(status, 204);
    let left: Vec<String> = state
        .memories()
        .list()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(left, ["The wifi is called Orchard"]);

    // An owner can make a device Ana's, for itself or for another device.
    let (status, _) = call(
        &client,
        M::PUT,
        format!("{base}/people/me"),
        &owner,
        json!({ "person": ana }),
    )
    .await;
    assert_eq!(status, 200);
    let (_, devices) = call(
        &client,
        M::GET,
        format!("{base}/devices"),
        &owner,
        json!({}),
    )
    .await;
    let intruder_id = devices["devices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["person"].is_null())
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _) = call(
        &client,
        M::PUT,
        format!("{base}/devices/{intruder_id}/person"),
        &owner,
        json!({ "person": ana }),
    )
    .await;
    assert_eq!(status, 200);
    let (_, me) = call(
        &client,
        M::GET,
        format!("{base}/people"),
        &intruder,
        json!({}),
    )
    .await;
    assert_eq!(me["me"], ana.as_str());
}

#[tokio::test]
async fn the_model_api_takes_device_tokens_only_over_https() {
    let state = new_state();
    let network = spawn(state.clone()).await;
    let local = spawn_local(state.clone()).await;
    let token = support::pair(&network, &state).await;
    let client = reqwest::Client::new();

    // Over the HTTPS server's router a device token still works for /v1.
    let r = client
        .get(format!("{network}/v1/models"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    // On plain HTTP, even from the box itself, it doesn't: app keys only.
    let r = client
        .get(format!("{local}/v1/models"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("app keys only"),
        "{body}"
    );
    let key: Value = client
        .post(format!("{network}/keys"))
        .bearer_auth(&token)
        .json(&json!({ "name": "x" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = client
        .get(format!("{local}/v1/models"))
        .bearer_auth(key["key"].as_str().unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_log_stream_closes_when_its_device_is_removed() {
    let repo = tempfile::tempdir().unwrap();
    let dir = repo.path().join("workflow_logs/docker_server");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("vllm_test.log"), "boot1\n").unwrap();
    let state = new_state().with_log_source(repo.path());
    let base = spawn(state.clone()).await;
    let _owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;

    let mut req = format!(
        "{}/logs/stream?source=container&tail=1",
        base.replace("http", "ws")
    )
    .into_client_request()
    .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {member}").parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().into_text().unwrap(),
        "boot1"
    );

    let id = state
        .sessions()
        .list()
        .into_iter()
        .find(|s| !s.owner)
        .unwrap()
        .id;
    state.sessions().revoke(&id);
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("still open after removal");
    match closed {
        Some(Ok(tokio_tungstenite::tungstenite::Message::Close(Some(frame)))) => {
            assert_eq!(u16::from(frame.code), 1008);
        }
        other => panic!("expected a close frame, got {other:?}"),
    }
}
