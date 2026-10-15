// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! `GET /metrics/history` and `GET /metrics/now` over a pre-filled history.

use std::sync::Arc;

use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde_json::Value;
use tt_station_agentd::history::{ChipPoint, Point};
use tt_station_agentd::routes::{local_http_app, AppState};
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

async fn serve_with_history() -> String {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(Idle));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let history = state.history();
    for i in 0..120u64 {
        history.record(
            Point {
                t: now - (119 - i) * 5,
                chips: vec![
                    ChipPoint {
                        temp_c: Some(50.0),
                        power_w: Some(100.0),
                        aiclk_mhz: Some(1350.0)
                    };
                    4
                ],
                gen_tps: Some(36.0),
                running: Some(1),
                ..Point::default()
            },
            1,
            "today",
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    // The box's own plain HTTP (a program on the box, as this account):
    // dashboards read it there without pairing.
    tokio::spawn(async move {
        axum::serve(listener, local_http_app(state).into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
    });
    base
}

#[tokio::test]
async fn history_returns_points_for_the_range_and_totals() {
    let base = serve_with_history().await;
    let h: Value = reqwest::get(format!("{base}/metrics/history?range=5m"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(h["range"], "5m");
    let points = h["points"].as_array().unwrap();
    assert!(
        (59..=61).contains(&points.len()),
        "{} points in 5 minutes",
        points.len()
    );
    assert_eq!(points[0]["chips"].as_array().unwrap().len(), 4);
    assert_eq!(points[0]["gen_tps"], 36.0);
    // 595 s at 400 W ≈ 66 Wh; 36 tok/s for 595 s.
    let totals = &h["totals"];
    assert!(
        (totals["energy_wh_today"].as_f64().unwrap() - 66.1).abs() < 1.0,
        "{totals}"
    );
    assert!((totals["tokens_today"].as_f64().unwrap() - 21_420.0).abs() < 1.0);
    assert_eq!(totals["requests_today"], 120);

    let hour: Value = reqwest::get(format!("{base}/metrics/history?range=1h"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hour["points"].as_array().unwrap().len(), 120);

    let bad = reqwest::get(format!("{base}/metrics/history?range=7y"))
        .await
        .unwrap();
    assert_eq!(bad.status().as_u16(), 400);
}

#[tokio::test]
async fn now_returns_the_latest_point_and_totals() {
    let base = serve_with_history().await;
    let now: Value = reqwest::get(format!("{base}/metrics/now"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(now["point"]["chips"][0]["aiclk_mhz"], 1350.0);
    assert!(now["totals"]["energy_wh_all"].as_f64().unwrap() > 0.0);
}
