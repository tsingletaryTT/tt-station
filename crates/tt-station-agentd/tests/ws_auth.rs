// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! WebSockets that take a token (`/logs/stream`, `/telemetry`) read it from a
//! header -- `Authorization`, or a `tt-token.<token>` subprotocol for a
//! browser -- never from the URL.

use std::sync::Arc;

use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error as WsError;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::NullBackend;

async fn harness() -> (String, String) {
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(NullBackend));
    let router = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
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
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    (
        format!("ws://{addr}"),
        done["token"].as_str().unwrap().to_string(),
    )
}

/// The HTTP status the upgrade was refused with.
async fn status(url: String, header: Option<(&'static str, String)>) -> u16 {
    let mut req = url.into_client_request().unwrap();
    if let Some((k, v)) = header {
        req.headers_mut().insert(k, v.parse().unwrap());
    }
    match tokio_tungstenite::connect_async(req).await {
        Ok(_) => 101,
        Err(WsError::Http(r)) => r.status().as_u16(),
        Err(e) => panic!("{e}"),
    }
}

#[tokio::test]
async fn a_websocket_takes_its_token_from_a_header_not_the_url() {
    let (ws, token) = harness().await;
    let logs = format!("{ws}/logs/stream");
    // In the URL: not accepted any more.
    assert_eq!(status(format!("{logs}?token={token}"), None).await, 401);
    assert_eq!(status(logs.clone(), None).await, 401);
    assert_eq!(
        status(
            logs.clone(),
            Some((
                "sec-websocket-protocol",
                "tt-station, tt-token.wrong".into()
            ))
        )
        .await,
        401
    );
    // In a header, it opens.
    assert_eq!(
        status(
            logs.clone(),
            Some(("authorization", format!("Bearer {token}")))
        )
        .await,
        101
    );
    // As a subprotocol, the handshake answers with the `tt-station`
    // subprotocol a browser insists on.
    let mut req = logs.into_client_request().unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("tt-station, tt-token.{token}").parse().unwrap(),
    );
    let (_, resp) = tokio_tungstenite::connect_async(req)
        .await
        .expect("upgraded");
    assert_eq!(
        resp.headers().get("sec-websocket-protocol").unwrap(),
        "tt-station"
    );
}
