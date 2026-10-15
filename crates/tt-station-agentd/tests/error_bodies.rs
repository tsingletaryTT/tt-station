// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Error responses keep their exact bytes. Clients match on the
//! `{"error":"..."}` body, so a refactor of how routes build their errors
//! (one shared `ApiError` instead of a per-module `err()`) must not change a
//! byte of it. A sample across the shapes the routes used to build by hand:
//! `ErrorResponse` structs and `json!({ "error": .. })` values.

use std::sync::Arc;

use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

async fn spawn() -> (String, String) {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend));
    let token = "error-bodies-owner".to_string();
    state
        .sessions()
        .pair(&token, Some("Test"), Some("test"), true);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state);
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, token)
}

#[tokio::test]
async fn error_bodies_are_byte_for_byte_what_they_were() {
    let (base, token) = spawn().await;
    let client = reqwest::Client::new();
    let cases: &[(&str, &str, u16, &str)] = &[
        // crate::error (ErrorResponse)
        (
            "GET",
            "/tt/models",
            409,
            r#"{"error":"The official Tenstorrent CLI (tt) isn't installed on this box, so models can't be listed or downloaded from here."}"#,
        ),
        // schedules, library, routines (json!({ "error": .. }))
        (
            "DELETE",
            "/schedules/nope",
            404,
            r#"{"error":"No such schedule."}"#,
        ),
        (
            "DELETE",
            "/library/sources/nope",
            404,
            r#"{"error":"No such folder."}"#,
        ),
        (
            "POST",
            "/routines/nope/run",
            404,
            r#"{"error":"No such routine."}"#,
        ),
        (
            "POST",
            "/routines/runs/nope/stop",
            409,
            r#"{"error":"It isn't running."}"#,
        ),
        (
            "DELETE",
            "/people/nope",
            404,
            r#"{"error":"No such person."}"#,
        ),
        // export (ErrorResponse via its own err())
        (
            "POST",
            "/import",
            400,
            r#"{"error":"The upload was empty."}"#,
        ),
    ];
    for (method, path, status, body) in cases {
        let response = client
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        let got_status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_string());
        let got = response.text().await.unwrap();
        assert_eq!(
            (got_status, got.as_str()),
            (*status, *body),
            "{method} {path}"
        );
        assert_eq!(
            content_type.as_deref(),
            Some("application/json"),
            "{method} {path}"
        );
    }
}
