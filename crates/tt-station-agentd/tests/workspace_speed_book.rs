// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The box's book of what Code measured per model (`code-speed.json` in the
//! config folder) outlives the agent: a book left by an earlier run is what
//! the model picker shows, and a new step is written back to it.
//!
//! The book is loaded once per process, from the config folder of the first
//! state a Code route sees, so this is a test binary of its own with a single
//! test. The catalog comes from a stub `tt`; nothing is serving.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tt_station_agentd::codespeed::{record, Step};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::NullBackend;
use tt_station_agentd::ttcli::TtCli;

mod support;

#[tokio::test]
async fn what_an_earlier_run_measured_is_shown_and_new_steps_are_kept() {
    support::home_without_models();
    let config = tempfile::tempdir().unwrap();
    // As an earlier run left it.
    let book = config.path().join("code-speed.json");
    std::fs::write(
        &book,
        json!({ "org/stub-remembered": { "decode_tps": [12.0, 14.0, 13.0], "ttft_ms": [900],
            "steps": 3, "cached_seen": false, "prefix_cache": false, "at": 1_700_000_000u64 } })
        .to_string(),
    )
    .unwrap();
    let tt = config.path().join("tt");
    std::fs::create_dir_all(tt.join("hf")).unwrap();
    let catalog = json!({ "device": "p300x2", "models": [
        { "name": "Stub-Remembered", "model_type": "llm",
          "devices": { "p300x2": { "supported": true, "tool_call_parser": "hermes" } } },
        { "name": "Stub-Fresh", "model_type": "llm",
          "devices": { "p300x2": { "supported": true, "tool_call_parser": "hermes" } } },
    ] });
    std::fs::write(tt.join("catalog.json"), catalog.to_string()).unwrap();
    std::fs::write(
        tt.join("tt.sh"),
        format!(
            "[ \"$1\" = \"--json\" ] && shift\n[ \"$1 $2\" = \"model list\" ] && exec cat '{}'\nexit 2\n",
            tt.join("catalog.json").display()
        ),
    )
    .unwrap();
    let argv = vec!["sh".to_string(), tt.join("tt.sh").display().to_string()];
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(NullBackend))
        .with_config_dir(config.path().to_path_buf())
        .with_tt_cli(Arc::new(TtCli::new(argv, tt.join("hf"))));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let token = support::pair(&base, &state).await;
    let client = reqwest::Client::new();
    let models = || async {
        let list: Value = client
            .get(format!("{base}/code/models"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        list
    };

    let list = models().await;
    let by_name = |list: &Value, name: &str| -> Value {
        list["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no {name}: {list}"))
    };
    let remembered = by_name(&list, "Stub-Remembered");
    // The median of what it measured, matched across spellings. The earlier
    // run didn't record which setup it measured, and the list says so.
    assert_eq!(
        remembered["speed"],
        json!({ "decode_tps": 13.0, "ttft_s": 0.9, "steps": 3, "at": 1_700_000_000u64,
                "setup_unrecorded": true })
    );
    assert_eq!(remembered["prefix_cache"]["state"], "off");
    assert_eq!(
        remembered["prefix_cache"]["how"],
        "seen when it last served here"
    );
    assert_eq!(by_name(&list, "Stub-Fresh")["speed"], Value::Null);

    // A step on another model: written to the book beside what was there.
    let t0 = Instant::now();
    let first = t0 + Duration::from_millis(250);
    record(
        "1700000000-bookbk",
        Step::measured(
            Some("Stub-Fresh".into()),
            t0,
            Some(first),
            first + Duration::from_secs(1),
            2000,
            None,
            51,
        ),
    );
    let saved: Value = serde_json::from_slice(&std::fs::read(&book).unwrap()).unwrap();
    assert_eq!(saved["Stub-Fresh"]["decode_tps"], json!([50.0]), "{saved}");
    assert_eq!(saved["Stub-Fresh"]["ttft_ms"], json!([250]));
    assert_eq!(saved["Stub-Fresh"]["steps"], 1);
    assert_eq!(saved["Stub-Fresh"]["recorded_setup"], true);
    assert_eq!(saved["org/stub-remembered"]["steps"], 3, "{saved}");
    // No temporary file left beside it.
    assert!(!config.path().join("code-speed.json.tmp").exists());

    let list = models().await;
    assert_eq!(by_name(&list, "Stub-Fresh")["speed"]["decode_tps"], 50.0);
}
