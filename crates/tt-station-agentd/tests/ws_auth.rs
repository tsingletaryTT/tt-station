// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! WebSockets that take a token (`/coding/term`, `/toplike/term`) read it
//! from a header -- `Authorization`, or a `tt-token.<token>` subprotocol for
//! a browser -- never from the URL.

use std::sync::Arc;

use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Error as WsError;
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

async fn harness() -> (String, String) {
    let state = AppState::new("qb2".into(), "4xBH".into(), Arc::new(DstackBackend));
    let router = app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
    let init: Value = client.post(format!("{base}/pair/init")).send().await.unwrap().json().await.unwrap();
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
    (format!("ws://{addr}"), done["token"].as_str().unwrap().to_string())
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
async fn a_terminal_websocket_takes_its_token_from_a_header_not_the_url() {
    // tt-toplike's view runs a stand-in that exits at once (this file's only
    // test, so nothing else sees the variable).
    std::env::set_var("TT_TOPLIKE_BIN", "/bin/true");
    let (ws, token) = harness().await;
    let term = format!("{ws}/coding/term?session=nope");
    // In the URL: not accepted any more.
    assert_eq!(status(format!("{term}&token={token}"), None).await, 401);
    assert_eq!(status(term.clone(), None).await, 401);
    assert_eq!(status(term.clone(), Some(("sec-websocket-protocol", "tt-station, tt-token.wrong".into()))).await, 401);
    // In a header, it gets past auth (to "no such session").
    assert_eq!(status(term.clone(), Some(("authorization", format!("Bearer {token}")))).await, 404);
    assert_eq!(status(term, Some(("sec-websocket-protocol", format!("tt-station, tt-token.{token}")))).await, 404);
    // Same for tt-toplike's view, which opens -- the handshake answering with
    // the `tt-station` subprotocol a browser insists on.
    let top = format!("{ws}/toplike/term?mode=starfield");
    assert_eq!(status(format!("{top}&token={token}"), None).await, 401);
    let mut req = top.into_client_request().unwrap();
    req.headers_mut().insert("sec-websocket-protocol", format!("tt-station, tt-token.{token}").parse().unwrap());
    let (_, resp) = tokio_tungstenite::connect_async(req).await.expect("upgraded");
    assert_eq!(resp.headers().get("sec-websocket-protocol").unwrap(), "tt-station");
}
