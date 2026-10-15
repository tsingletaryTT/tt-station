// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Push notifications (`/notify`, `/events`) against a stub APNs, and the power
//! readiness report.

use std::sync::{Arc, Mutex};

use axum::{extract::Path, http::HeaderMap, http::StatusCode, routing::post, Router};
use base64::Engine as _;
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::apns::{Apns, ApnsConfig};
use tt_station_agentd::notify::{Draft, Notifier};
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

/// (device token, authorization, apns-topic, payload)
type Pushes = Arc<Mutex<Vec<(String, String, String, Value)>>>;

const LIVE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DEAD: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    base
}

/// A stub of Apple's push endpoint: `DEAD` is an uninstalled app (410).
async fn stub_apns(pushes: Pushes) -> String {
    let router = Router::new().route(
        "/3/device/{token}",
        post(
            move |Path(token): Path<String>, headers: HeaderMap, body: axum::Json<Value>| {
                let pushes = pushes.clone();
                async move {
                    let h = |k: &str| {
                        headers
                            .get(k)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string()
                    };
                    pushes.lock().unwrap().push((
                        token.clone(),
                        h("authorization"),
                        h("apns-topic"),
                        body.0,
                    ));
                    if token == DEAD {
                        (
                            StatusCode::GONE,
                            axum::Json(json!({ "reason": "Unregistered" })),
                        )
                    } else {
                        (StatusCode::OK, axum::Json(json!({})))
                    }
                }
            },
        ),
    );
    serve(router).await
}

fn test_key_pem() -> String {
    use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(
        &ECDSA_P256_SHA256_FIXED_SIGNING,
        &ring::rand::SystemRandom::new(),
    )
    .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref());
    format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
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
async fn alerts_are_pushed_to_registered_phones() {
    let pushes: Pushes = Arc::default();
    let apns_base = stub_apns(pushes.clone()).await;
    let apns = Apns::new(
        ApnsConfig {
            team_id: "TEAM123456".into(),
            key_id: "KEY1234567".into(),
            key_file: String::new(),
            topic: "com.tenstorrent.ttstation.mobile".into(),
            endpoint: Some(apns_base),
        },
        &test_key_pem(),
    )
    .unwrap();
    let notifier = Notifier::in_memory().with_apns(apns);
    notifier.set_box_name("qb2-lab");
    let state =
        AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle)).with_notifier(notifier);
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;

    for path in ["/notify", "/events", "/power/info"] {
        let anon = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(anon.status(), 401, "{path}");
    }
    let bad = client
        .post(format!("{base}/notify/devices"))
        .bearer_auth(&token)
        .json(&json!({ "token": "nope", "sandbox": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    for device in [LIVE, DEAD] {
        let ok: Value = client
            .post(format!("{base}/notify/devices"))
            .bearer_auth(&token)
            .json(&json!({ "token": device, "sandbox": true, "name": "iPhone" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ok["push_available"], true);
    }

    let test: Value = client
        .post(format!("{base}/notify/test"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test["sent"], 1, "the live phone; the dead one is dropped");
    {
        let pushes = pushes.lock().unwrap();
        let live = pushes.iter().find(|p| p.0 == LIVE).unwrap();
        assert!(live.1.starts_with("bearer ey") || live.1.starts_with("Bearer ey"));
        assert_eq!(live.2, "com.tenstorrent.ttstation.mobile");
        assert_eq!(live.3["aps"]["alert"]["title"], "Hello from qb2-lab");
        assert_eq!(live.3["aps"]["alert"]["subtitle"], "qb2-lab");
        assert_eq!(live.3["kind"], "test");
    }
    let settings: Value = client
        .get(format!("{base}/notify"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["push"]["available"], true);
    assert_eq!(settings["push"]["devices"].as_array().unwrap().len(), 1);

    // Watcher events reach the feed and the phone.
    state.notifier().emit(Draft {
        kind: "download_done",
        title: "FLUX.1-dev downloaded".to_string(),
        body: "Ready to start in TT Station.".to_string(),
    });
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
    assert_eq!(titles, ["Hello from qb2-lab", "FLUX.1-dev downloaded"]);
    for _ in 0..50 {
        if pushes
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.3["kind"] == "download_done")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(pushes
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.0 == LIVE && p.3["aps"]["alert"]["title"] == "FLUX.1-dev downloaded"));

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

/// A pairing code lets whoever types it control the box, and a push's text
/// passes through Apple: the push only says someone is asking; the code is in
/// the box's own feed (`/events`, authenticated) for TT Station to show.
#[tokio::test]
async fn a_pairing_push_never_carries_the_code() {
    let pushes: Pushes = Arc::default();
    let apns_base = stub_apns(pushes.clone()).await;
    let apns = Apns::new(
        ApnsConfig {
            team_id: "TEAM123456".into(),
            key_id: "KEY1234567".into(),
            key_file: String::new(),
            topic: "com.tenstorrent.ttstation.mobile".into(),
            endpoint: Some(apns_base),
        },
        &test_key_pem(),
    )
    .unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle))
        .with_notifier(Notifier::in_memory().with_apns(apns));
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let ok = client
        .post(format!("{base}/notify/devices"))
        .bearer_auth(&token)
        .json(&json!({ "token": LIVE, "sandbox": true, "name": "iPhone" }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    // Another computer asks to pair.
    let init: Value = client
        .post(format!("{base}/pair/init"))
        .json(&json!({ "device": "Sam's MacBook" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = state.last_code(init["pair_id"].as_str().unwrap()).unwrap();
    let spaced = format!("{} {}", &code[..3], &code[3..]);
    let mut pushed = None;
    for _ in 0..100 {
        pushed = pushes.lock().unwrap().iter().find(|p| p.0 == LIVE && p.3["kind"] == "pairing").map(|p| p.3.clone());
        if pushed.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let pushed = pushed.expect("the phone is told someone is asking");
    let text = pushed.to_string();
    assert!(!text.contains(&code) && !text.contains(&spaced), "no code through Apple: {text}");
    assert_eq!(pushed["aps"]["alert"]["title"], "A computer is asking to pair");
    assert!(pushed["aps"]["alert"]["body"].as_str().unwrap().contains("Open TT Station to see the code"));
    assert!(pushed.get("data").is_none());

    // TT Station reads the code from the box itself.
    let events: Value = client
        .get(format!("{base}/events?since=0"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let last = events["events"].as_array().unwrap().iter().rev().find(|e| e["kind"] == "pairing").unwrap().clone();
    assert!(last["title"].as_str().unwrap().contains(&spaced), "{last}");
    assert!(last["title"].as_str().unwrap().contains("Sam's MacBook"));
}

#[tokio::test]
async fn without_a_push_key_the_box_says_so() {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    let base = serve(app(state.clone())).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let settings: Value = client
        .get(format!("{base}/notify"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(settings["push"]["available"], false);
    let test = client
        .post(format!("{base}/notify/test"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(test.status(), 409);
}
