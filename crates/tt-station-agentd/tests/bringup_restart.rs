// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Bring-up across an agent restart (`bringup::watch_forever` on a config dir
//! a previous agent left): a live run's unanswered approvals come back and can
//! still be answered, a gone run's don't, and a run whose unit ended while the
//! agent was down is recorded as finished with its exit code.
//!
//! Its own test binary: the watcher keeps where approvals are saved in a
//! process-wide `OnceLock`, so only one watcher may run per process.

use std::time::Duration;

use reqwest::Method as M;
use serde_json::{json, Value};

mod bringup_support;
use bringup_support::{fakes, harness, run_record, unique_id};

fn approval(id: &str, command: &str) -> Value {
    json!({
        "id": id, "kind": "command", "why": "uses sudo", "command": command,
        "cwd": null, "reason": null, "grant_root": null, "created": 1_700_000_000u64,
        "decision": null,
    })
}

async fn eventually<F: Fn() -> bool>(what: &str, ok: F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !ok() {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_restarted_agent_picks_up_where_its_runs_are() {
    let s = harness().await;
    let live = run_record(&s, &unique_id("live"), "live-secret");
    let gone = run_record(&s, &unique_id("gone"), "gone-secret");
    let (lid, gid) = (
        live["id"].as_str().unwrap().to_string(),
        gone["id"].as_str().unwrap().to_string(),
    );
    fakes().set_active(live["unit"].as_str().unwrap(), true);
    // The gone run's unit ended long ago, a stage having failed its check.
    std::fs::write(s.bringup_dir().join("codex").join(&gid).join("exit"), "6\n").unwrap();
    s.write_runs(&[live, gone]);
    let saved = s.bringup_dir().join("approvals.json");
    std::fs::write(
        &saved,
        serde_json::to_vec(&json!([
            [lid, approval("aaaaaaaaaa", "sudo apt install x")],
            [gid, approval("bbbbbbbbbb", "sudo rm x")],
        ]))
        .unwrap(),
    )
    .unwrap();

    let watcher = tokio::spawn(tt_station_agentd::bringup::watch_forever(s.state.clone()));

    // The gone run is recorded as ended, with what its exit means.
    eventually("the gone run is marked finished", || {
        s.saved_runs()
            .iter()
            .any(|r| r["id"] == gid.as_str() && r["finished"].is_u64())
    })
    .await;
    let (status, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{gid}"),
            &s.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["state"], "failed", "{shown}");
    assert_eq!(shown["exit"], 6);
    assert_eq!(
        shown["exit_meaning"],
        "A stage failed its check, even after a fix-up attempt."
    );
    assert_eq!(
        shown["approvals"],
        json!([]),
        "a gone run's question isn't restored"
    );
    // The live one keeps the chips.
    assert!(s.state.chips_holder().is_some());

    // The live run's question is back, and still answerable end to end.
    let (_, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{lid}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(shown["state"], "waiting_for_you", "{shown}");
    assert_eq!(shown["approvals"][0]["id"], "aaaaaaaaaa");
    assert_eq!(shown["approvals"][0]["command"], "sudo apt install x");
    let (status, _) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{lid}/approvals/aaaaaaaaaa"),
            &s.owner,
            json!({ "decision": "decline" }),
        )
        .await;
    assert_eq!(status, 200);
    let (status, body) = s
        .driver(
            &format!("/bringup/codex/runs/{lid}/approvals/aaaaaaaaaa/wait"),
            Some("live-secret"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["decision"], "decline");

    // Answered, it's no longer kept for the next restart.
    let open = || -> Vec<Value> {
        std::fs::read(&saved)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    };
    assert!(open().is_empty(), "{:?}", open());

    // A new question from the live run is kept until it's answered.
    let (status, body) = s
        .driver(
            &format!("/bringup/codex/runs/{lid}/approvals"),
            Some("live-secret"),
            json!({ "kind": "command", "why": "uses sudo", "command": "sudo ls" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let kept = open();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0][0], lid.as_str());
    assert_eq!(kept[0][1]["command"], "sudo ls");
    watcher.abort();
}
