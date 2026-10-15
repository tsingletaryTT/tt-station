// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Scheduled modes (`src/schedules.rs`): the routes (checked, normalized,
//! kept sorted, saved to disk and read back), the cap, when one is due, and
//! what stops one from switching (a bring-up holding the chips).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::schedules::{self, due, parse_time, Schedule, Schedules};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tt-schedules-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("schedules.json")
}

struct Box_ {
    base: String,
    state: AppState,
    token: String,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(file: &PathBuf) -> Self {
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_schedules(Schedules::open(file));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let token = support::pair(&base, &state).await;
        Box_ {
            base,
            state,
            token,
            c: reqwest::Client::new(),
        }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut r = self
            .c
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send().await.unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

fn times(list: &Value) -> Vec<String> {
    list["schedules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["time"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn schedules_are_checked_normalized_sorted_and_saved() {
    let file = scratch("crud");
    let b = Box_::new(&file).await;
    assert_eq!(
        b.send("GET", "/schedules", None).await,
        (200, json!({ "schedules": [] }))
    );
    for (body, error) in [
        (
            json!({ "mode": "karaoke", "time": "08:00", "days": [1] }),
            "No mode called karaoke.",
        ),
        (
            json!({ "mode": "chat", "time": "8am", "days": [1] }),
            "Give a time like 07:30.",
        ),
        (
            json!({ "mode": "chat", "time": "08:00", "days": [] }),
            "Pick at least one day.",
        ),
        (
            json!({ "mode": "chat", "time": "08:00", "days": [7] }),
            "Pick at least one day.",
        ),
    ] {
        let (status, r) = b.send("POST", "/schedules", Some(body.clone())).await;
        assert_eq!((status, &r["error"]), (400, &json!(error)), "{body}");
    }
    assert!(!file.exists(), "nothing saved for a refused schedule");

    let (status, list) = b
        .send(
            "POST",
            "/schedules",
            Some(json!({ "mode": "chat", "time": "18:5", "days": [5, 1, 1, 3] })),
        )
        .await;
    assert_eq!(status, 200, "{list}");
    let evening = list["schedules"][0].clone();
    assert_eq!(evening["time"], "18:05");
    assert_eq!(evening["days"], json!([1, 3, 5]));
    assert_eq!(evening["enabled"], true);
    assert!(evening["last_run"].is_null());
    let (_, list) = b
        .send(
            "POST",
            "/schedules",
            Some(json!({ "mode": "images", "time": "07:30", "days": [1, 2, 3, 4, 5] })),
        )
        .await;
    assert_eq!(times(&list), ["07:30", "18:05"], "kept in time order");

    // Saved, and read back by a fresh box.
    let saved: Vec<Schedule> = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(saved.len(), 2);
    assert_eq!(Schedules::open(&file).list(), saved);
    assert_eq!(b.state.schedules().list(), saved);
}

#[tokio::test]
async fn a_schedule_is_changed_turned_off_and_removed() {
    let file = scratch("patch");
    let b = Box_::new(&file).await;
    let (_, list) = b
        .send(
            "POST",
            "/schedules",
            Some(json!({ "mode": "chat", "time": "06:00", "days": [0, 6] })),
        )
        .await;
    let id = list["schedules"][0]["id"].as_str().unwrap().to_string();
    b.send(
        "POST",
        "/schedules",
        Some(json!({ "mode": "images", "time": "12:00", "days": [1] })),
    )
    .await;

    // Moved past the other one: still sorted.
    let (status, list) = b
        .send(
            "PATCH",
            &format!("/schedules/{id}"),
            Some(json!({ "time": "21:0", "enabled": false })),
        )
        .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(times(&list), ["12:00", "21:00"]);
    let s = &list["schedules"][1];
    assert_eq!(
        (&s["id"], &s["mode"], &s["enabled"]),
        (&json!(id), &json!("chat"), &json!(false))
    );
    assert_eq!(s["days"], json!([0, 6]), "untouched fields stay");

    // Days are tidied the same way as a new schedule's.
    let (_, list) = b
        .send(
            "PATCH",
            &format!("/schedules/{id}"),
            Some(json!({ "days": [6, 2, 2, 0] })),
        )
        .await;
    assert_eq!(list["schedules"][1]["days"], json!([0, 2, 6]));

    // A bad change leaves it as it was.
    for patch in [
        json!({ "mode": "nope" }),
        json!({ "time": "25:00" }),
        json!({ "days": [] }),
    ] {
        let (status, _) = b
            .send("PATCH", &format!("/schedules/{id}"), Some(patch.clone()))
            .await;
        assert_eq!(status, 400, "{patch}");
    }
    assert_eq!(
        b.state
            .schedules()
            .list()
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .time,
        "21:00"
    );
    let (status, r) = b
        .send(
            "PATCH",
            "/schedules/missing",
            Some(json!({ "enabled": true })),
        )
        .await;
    assert_eq!((status, &r["error"]), (400, &json!("No such schedule.")));

    let (status, list) = b.send("DELETE", &format!("/schedules/{id}"), None).await;
    assert_eq!(status, 200);
    assert_eq!(times(&list), ["12:00"]);
    assert_eq!(
        b.send("DELETE", &format!("/schedules/{id}"), None).await.0,
        404
    );
    assert_eq!(Schedules::open(&file).list().len(), 1);
}

#[tokio::test]
async fn schedules_need_a_paired_device_and_stop_at_the_cap() {
    let file = scratch("cap");
    let b = Box_::new(&file).await;
    let r =
        b.c.post(format!("{}/schedules", b.base))
            .json(&json!({ "mode": "chat", "time": "08:00", "days": [1] }))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 401);
    for i in 0..schedules::MAX_SCHEDULES {
        let (status, _) = b
            .send(
                "POST",
                "/schedules",
                Some(json!({ "mode": "chat", "time": format!("{:02}:00", i % 24), "days": [1] })),
            )
            .await;
        assert_eq!(status, 200);
    }
    let (status, r) = b
        .send(
            "POST",
            "/schedules",
            Some(json!({ "mode": "chat", "time": "23:59", "days": [1] })),
        )
        .await;
    assert_eq!((status, &r["error"]), (400, &json!("Up to 20 schedules.")));
}

#[test]
fn a_schedule_is_due_for_ten_minutes_on_its_days_once_a_day() {
    let s = |time: &str, days: &[u8], enabled: bool, last: Option<&str>| Schedule {
        id: "s".into(),
        mode: "chat".into(),
        time: time.into(),
        days: days.to_vec(),
        enabled,
        last_run: last.map(str::to_string),
    };
    let today = "2026-10-15";
    assert_eq!(
        due(&[s("07:30", &[4], true, None)], 4, 7, 29, today).len(),
        0
    );
    assert_eq!(
        due(&[s("07:30", &[4], true, None)], 4, 7, 30, today).len(),
        1
    );
    assert_eq!(
        due(&[s("07:30", &[4], true, None)], 4, 7, 39, today).len(),
        1
    );
    assert_eq!(
        due(&[s("07:30", &[4], true, None)], 4, 7, 40, today).len(),
        0
    );
    assert_eq!(
        due(&[s("07:30", &[4], true, None)], 4, 8, 0, today).len(),
        0
    );
    assert_eq!(
        due(&[s("07:30", &[3, 5], true, None)], 4, 7, 30, today).len(),
        0
    );
    assert_eq!(
        due(&[s("07:30", &[4], false, None)], 4, 7, 30, today).len(),
        0
    );
    assert_eq!(
        due(&[s("07:30", &[4], true, Some(today))], 4, 7, 31, today).len(),
        0
    );
    assert_eq!(
        due(
            &[s("07:30", &[4], true, Some("2026-10-08"))],
            4,
            7,
            31,
            today
        )
        .len(),
        1,
        "ran last week, due again today"
    );
    // A schedule with a time that doesn't parse (an old file) is never due.
    assert!(due(&[s("7.30", &[4], true, None)], 4, 7, 30, today).is_empty());
    assert_eq!(parse_time(" 07:30 "), Some((7, 30)));
    assert_eq!(parse_time("07:60"), None);
    assert_eq!(parse_time("-1:00"), None);
}

#[test]
fn imported_schedules_are_checked_and_capped() {
    let s = Schedules::in_memory();
    let mk = |id: &str, mode: &str, time: &str| Schedule {
        id: id.into(),
        mode: mode.into(),
        time: time.into(),
        days: vec![1],
        enabled: true,
        last_run: None,
    };
    let (added, updated) = s.merge_imported(vec![
        mk("good-1", "chat", "09:00"),
        mk("good-0", "images", "08:00"),
        mk("bad-mode", "karaoke", "08:00"),
        mk("bad-time", "chat", "nine"),
        mk("../escape", "chat", "08:00"),
    ]);
    assert_eq!((added, updated), (2, 0));
    assert_eq!(
        s.list().iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        ["good-0", "good-1"]
    );
    // The same again changes nothing; a changed one updates.
    assert_eq!(
        s.merge_imported(vec![mk("good-1", "chat", "09:00")]),
        (0, 0)
    );
    assert_eq!(
        s.merge_imported(vec![mk("good-1", "chat", "10:00")]),
        (0, 1)
    );
    let many: Vec<Schedule> = (0..30)
        .map(|i| mk(&format!("n{i}"), "chat", "11:00"))
        .collect();
    let (added, _) = s.merge_imported(many);
    assert_eq!(added, schedules::MAX_SCHEDULES - 2);
    assert_eq!(s.list().len(), schedules::MAX_SCHEDULES);
}

#[test]
fn a_schedule_skips_while_something_else_holds_the_chips() {
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend));
    assert_eq!(schedules::blocker(&state), None);
    state.set_orchard_busy(true);
    assert!(schedules::blocker(&state).unwrap().contains("Orchard"));
    state.set_orchard_busy(false);
    state.set_bringup_busy(true);
    assert!(schedules::blocker(&state)
        .unwrap()
        .starts_with("A model bring-up is using the chips."));
    state.set_bringup_busy(false);
    assert_eq!(schedules::blocker(&state), None);
}
