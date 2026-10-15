// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! People end to end: two phones on one box, each says who it is, and each
//! sees its own memories plus the household's -- never the other person's.

use std::sync::Arc;

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::ServingBackend;

struct Idle;

impl ServingBackend for Idle {
    fn start_forcing(&self, _: &str, _: bool) -> anyhow::Result<Endpoint> {
        anyhow::bail!("unused")
    }
    fn stop(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
    fn list_models(&self) -> anyhow::Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

async fn pair(client: &reqwest::Client, base: &str, state: &AppState) -> String {
    let init: Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = init["pair_id"].as_str().unwrap().to_string();
    let code = state.last_code(&id).unwrap();
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    done["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn each_phone_is_someone_and_keeps_its_memories() {
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(Idle));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let ana_phone = pair(&client, &base, &state).await;
    let ben_phone = pair(&client, &base, &state).await;

    let send = |method: reqwest::Method, path: &str, token: &str, body: Option<Value>| {
        let mut r = client
            .request(method, format!("{base}{path}"))
            .bearer_auth(token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        r.send()
    };
    let json = |r: reqwest::Response| async move { r.json::<Value>().await.unwrap() };

    let unauth = client.get(format!("{base}/people")).send().await.unwrap();
    assert_eq!(unauth.status(), 401);

    let ana = json(
        send(
            reqwest::Method::POST,
            "/people",
            &ana_phone,
            Some(json!({ "name": "Ana", "me": true })),
        )
        .await
        .unwrap(),
    )
    .await;
    let ana_id = ana["me"].as_str().unwrap().to_string();
    let listed = json(
        send(
            reqwest::Method::POST,
            "/people",
            &ben_phone,
            Some(json!({ "name": "Ben" })),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(listed["people"].as_array().unwrap().len(), 2);
    assert!(
        listed["me"].is_null(),
        "adding someone doesn't make you them"
    );
    let ben_id = listed["people"][1]["id"].as_str().unwrap().to_string();
    let me = json(
        send(
            reqwest::Method::PUT,
            "/people/me",
            &ben_phone,
            Some(json!({ "person": ben_id })),
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(me["me"], ben_id.as_str());

    state
        .memories()
        .add("The wifi is called Orchard", 1, None)
        .unwrap();
    state
        .memories()
        .add("Ana is vegetarian", 2, Some(&ana_id))
        .unwrap();
    state
        .memories()
        .add("Ben runs on Sundays", 3, Some(&ben_id))
        .unwrap();
    let texts = |v: Value| -> Vec<String> {
        v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["text"].as_str().unwrap().to_string())
            .collect()
    };
    let ana_sees = texts(
        json(
            send(reqwest::Method::GET, "/memories", &ana_phone, None)
                .await
                .unwrap(),
        )
        .await,
    );
    assert_eq!(
        ana_sees,
        ["The wifi is called Orchard", "Ana is vegetarian"]
    );
    let ben_sees = texts(
        json(
            send(reqwest::Method::GET, "/memories", &ben_phone, None)
                .await
                .unwrap(),
        )
        .await,
    );
    assert_eq!(
        ben_sees,
        ["The wifi is called Orchard", "Ben runs on Sundays"]
    );

    // Removing Ben takes his memories and frees his phone.
    let after = json(
        send(
            reqwest::Method::DELETE,
            &format!("/people/{ben_id}"),
            &ana_phone,
            None,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(after["people"].as_array().unwrap().len(), 1);
    assert_eq!(state.memories().list().len(), 2);
    let ben_now = json(
        send(reqwest::Method::GET, "/people", &ben_phone, None)
            .await
            .unwrap(),
    )
    .await;
    assert!(ben_now["me"].is_null());
}
