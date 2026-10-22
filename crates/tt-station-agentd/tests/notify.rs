// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Alerts (`/notify`, `/events`): what the box tells its owners and household,
//! and the power readiness report.

use std::sync::Arc;

use axum::Router;
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::notify::Draft;
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

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

async fn pair(client: &reqwest::Client, state: &AppState, base: &str) -> String {
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
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    done["token"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn alerts_reach_the_feed() {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    for path in ["/notify", "/events", "/power/info"] {
        let anon = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(anon.status(), 401, "{path}");
    }
    for gone in ["/notify/devices", "/notify/test"] {
        let r = client
            .post(format!("{base}{gone}"))
            .bearer_auth(&token)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 404, "{gone}: no push notifications any more");
    }

    // Watcher events reach the feed.
    let emitted = state.notifier().emit(Draft {
        kind: "download_done",
        title: "FLUX.1-dev downloaded".to_string(),
        body: "Ready to start in TT Station.".to_string(),
    });
    let settings: Value = client
        .get(format!("{base}/notify"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["latest_event"], emitted.seq);
    assert!(settings.get("push").is_none());
    let events: Value = client
        .get(format!("{base}/events?since=0"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Pairing this harness put its code, and then the new device, in the feed
    // first; that's the owner's copy, not what this test is about.
    let titles: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] != "pairing" && e["kind"] != "devices")
        .map(|e| e["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["FLUX.1-dev downloaded"]);

    let power: Value = client
        .get(format!("{base}/power/info"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(power["can_power_off"].is_string());
}

/// A pairing code lets whoever types it control the box: it's in the box's
/// own feed (`/events`, authenticated) for TT Station to show, and only
/// owners see it.
#[tokio::test]
async fn events_are_routed_to_the_people_they_concern() {
    use tt_station_agentd::notify::Audience;
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    // The first device is an owner; the next, a member who says they're Max.
    let owner = pair(&client, &state, &base).await;
    let member = pair(&client, &state, &base).await;
    let people: Value = client
        .post(format!("{base}/people"))
        .bearer_auth(&member)
        .json(&json!({ "name": "Max", "me": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let max = people["me"].as_str().unwrap().to_string();
    let n = state.notifier();
    let draft = |kind: &'static str, title: &str| Draft {
        kind,
        title: title.to_string(),
        body: String::new(),
    };
    n.emit(draft("model_ready", "household"));
    n.emit_with(
        draft("coding", "owners"),
        Some(json!({ "approval": { "id": "a" } })),
    );
    n.emit_to(
        draft("research", "max's"),
        None,
        Audience::Person(max.clone()),
    );
    n.emit_to(
        draft("research", "nobody's"),
        None,
        Audience::person_or_owners(None),
    );
    let titles = |token: &str| {
        let client = client.clone();
        let url = format!("{base}/events?since=0");
        let token = token.to_string();
        async move {
            let v: Value = client
                .get(url)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            v["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["title"].as_str().unwrap().to_string())
                .filter(|t| ["household", "owners", "max's", "nobody's"].contains(&t.as_str()))
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(titles(&member).await, ["household", "max's"]);
    assert_eq!(titles(&owner).await, ["household", "owners", "nobody's"]);
    // Nor does a member see who's pairing, or the code.
    let kinds: Value = client
        .get(format!("{base}/events?since=0"))
        .bearer_auth(&member)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(kinds["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] != "pairing" && e["kind"] != "devices"));
}
