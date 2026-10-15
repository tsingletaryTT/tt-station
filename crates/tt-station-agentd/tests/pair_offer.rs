// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! `POST /pair/offer`: only the local-secret holder gets a code, and that code
//! completes pairing like one read off the screen.

use std::sync::Arc;

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::{json, Value};
use tt_station_agentd::pairqr::SECRET_HEADER;
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

async fn serve(secret: Option<&str>) -> String {
    let mut state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    if let Some(secret) = secret {
        state = state.with_local_secret(secret.to_string());
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app(state)).await.unwrap() });
    base
}

#[tokio::test]
async fn offer_needs_the_secret_and_its_code_pairs() {
    let base = serve(Some("s3cret")).await;
    let client = reqwest::Client::new();
    for presented in [None, Some("wrong"), Some("s3cre")] {
        let mut req = client.post(format!("{base}/pair/offer"));
        if let Some(p) = presented {
            req = req.header(SECRET_HEADER, p);
        }
        assert_eq!(req.send().await.unwrap().status(), 403, "{presented:?}");
    }
    let offer: Value = client
        .post(format!("{base}/pair/offer"))
        .header(SECRET_HEADER, "s3cret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(offer["name"], "qb2-lab");
    assert_eq!(offer["code"].as_str().unwrap().len(), 6);
    let done: Value = client
        .post(format!("{base}/pair/complete"))
        .json(&json!({ "pair_id": offer["pair_id"], "code": offer["code"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(done["token"].as_str().is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
async fn offer_is_off_without_a_secret() {
    let base = serve(None).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/pair/offer"))
        .header(SECRET_HEADER, "")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
