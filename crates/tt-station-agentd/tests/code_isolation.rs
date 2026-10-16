// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What runs as the box's own account is an owner's: Code outside the
//! sandbox, bring-ups, Orchard, the lab and the desktop launches. Members get
//! Code in the sandbox, in a folder that isn't the box's own; a session someone started outside the sandbox stays out of their reach.

use std::sync::Arc;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

struct Station {
    base: String,
    owner: String,
    member: String,
    config: tempfile::TempDir,
    client: reqwest::Client,
}

async fn harness() -> Station {
    let config = tempfile::tempdir().unwrap();
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(config.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let base = format!("http://{addr}");
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        base,
        owner,
        member,
        config,
        client: reqwest::Client::new(),
    }
}

impl Station {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Value,
    ) -> (u16, Value) {
        let r = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }
}

/// A project folder in the home folder (where Code takes them from), removed afterwards.
fn project() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME");
    tempfile::Builder::new()
        .prefix("tt-station-isolation-test-")
        .tempdir_in(home)
        .expect("a folder in the home folder")
}

#[tokio::test]
async fn members_get_code_in_the_sandbox_only_and_owners_choose() {
    let b = harness().await;
    let p = project();
    let folder = p.path().display().to_string();
    use reqwest::Method as M;

    let outside = json!({ "folder": folder, "sandbox": false });
    let (status, body) = b
        .call(M::POST, "/code/sessions", &b.member, outside.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().starts_with("Only an owner"),
        "{body}"
    );

    // Sandboxed, in a project of their own: fine.
    let (status, body) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.member,
            json!({ "folder": folder }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session"]["sandbox"], true);

    // The member's sandbox used that folder: an owner outside the sandbox is
    // asked first, then goes ahead once they confirm.
    let (status, body) = b
        .call(M::POST, "/code/sessions", &b.owner, outside.clone())
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["needs_confirm"], "shared_folder", "{body}");
    assert!(body["used_by"][0]["who"].is_string(), "{body}");
    assert!(body["used_by"][0]["when"].is_u64(), "{body}");
    let mut confirmed = outside;
    confirmed["confirm_shared"] = json!(true);
    let (status, body) = b.call(M::POST, "/code/sessions", &b.owner, confirmed).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session"]["sandbox"], false);

    // Each start outside the sandbox, and each refusal, is in the activity log.
    let (_, log) = b.call(M::GET, "/audit", &b.owner, json!({})).await;
    let actions: Vec<&str> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"host_run"), "{log}");
    assert!(actions.contains(&"denied"), "{log}");
    assert!(actions.contains(&"shared_folder_confirmed"), "{log}");
}

#[tokio::test]
async fn running_things_as_the_box_is_for_owners() {
    let b = harness().await;
    use reqwest::Method as M;
    let refused = [
        (M::POST, "/bringup/codex/runs", json!({ "model": "a/b" })),
        (M::POST, "/bringup/codex/runs/x/resume", json!({})),
        (M::POST, "/bringup/claude/login", json!({})),
        (M::POST, "/orchard/setup", json!({})),
        (M::POST, "/orchard/preflight", json!({ "model": "a/b" })),
        (M::POST, "/orchard/runs", json!({ "model": "a/b" })),
        (M::POST, "/orchard/runs/x/resume", json!({})),
        (M::POST, "/orchard/runs/x/serve", json!({})),
        (M::PUT, "/orchard/lab", json!({ "remove": true })),
        (M::POST, "/orchard/lab/setup", json!({})),
        (M::POST, "/lab/prepare", json!({ "model": "gpt-oss-20b" })),
        (
            M::POST,
            "/lab/run",
            json!({ "model": "gpt-oss-20b", "prompt": "hi" }),
        ),
        (M::POST, "/desktop/open", json!({ "app": "Terminal" })),
        (M::POST, "/hermes/open", json!({})),
        (M::POST, "/vllm/native/setup", json!({})),
    ];
    for (m, path, body) in refused {
        let (status, said) = b.call(m.clone(), path, &b.member, body.clone()).await;
        assert_eq!(status, 403, "member {m} {path}: {said}");
        assert!(
            said["error"].as_str().unwrap().contains("Only an owner"),
            "{said}"
        );
    }
}

#[tokio::test]
async fn a_member_cannot_pick_up_a_session_started_outside_the_sandbox() {
    let b = harness().await;
    use reqwest::Method as M;
    // A session from before roles were enforced: started outside the sandbox.
    let id = "1700000000-abcdef";
    let dir = b.config.path().join("code").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.json"),
        json!({ "id": id, "folder": "/tmp", "title": "theirs", "created": 1, "sandbox": false, "policy": "ask" }).to_string(),
    )
    .unwrap();

    for (m, path, body) in [
        (
            M::POST,
            format!("/code/sessions/{id}/message"),
            json!({ "text": "go on" }),
        ),
        (M::GET, format!("/code/sessions/{id}"), json!({})),
        (M::GET, format!("/code/sessions/{id}/changes"), json!({})),
        (
            M::POST,
            format!("/code/sessions/{id}/settings"),
            json!({ "think": true }),
        ),
        (
            M::POST,
            format!("/code/sessions/{id}/commit"),
            json!({ "message": "x" }),
        ),
        (M::DELETE, format!("/code/sessions/{id}"), json!({})),
        (
            M::POST,
            "/approvals/answer".to_string(),
            json!({ "id": format!("code:{id}:r1"), "allow": true }),
        ),
    ] {
        let (status, said) = b.call(m.clone(), &path, &b.member, body).await;
        assert_eq!(status, 403, "member {m} {path}: {said}");
    }
    let (_, code) = b.call(M::GET, "/code", &b.member, json!({})).await;
    assert_eq!(code["sessions"], json!([]), "a member doesn't see it");
    assert_eq!(code["outside_sandbox"], false);

    let (_, code) = b.call(M::GET, "/code", &b.owner, json!({})).await;
    assert_eq!(code["sessions"][0]["id"], id, "an owner does");
    // An owner may carry on with it (here only nothing is serving).
    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/message"),
            &b.owner,
            json!({ "text": "go on" }),
        )
        .await;
    assert_eq!(status, 409, "{said}");
}
