// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Approval gating for bring-up runs: the run's driver (authenticated by the
//! run's own secret, never a device token) asks, long-polls for a decision,
//! and only an owner answers. Requests are scoped to their run: another run's
//! driver can neither raise nor read them, and stopping or deleting a run
//! cancels what it was waiting on. The HTTP calls mirror `codex_bringup.py`'s
//! `ask_person`.

use std::time::Duration;

use reqwest::Method as M;
use serde_json::{json, Value};

mod bringup_support;
use bringup_support::{fakes, harness, run_record, unique_id, Station};

/// What `ask_person` sends for a risky command.
fn ask(command: &str) -> Value {
    json!({
        "kind": "command",
        "why": "uses sudo",
        "command": command,
        "cwd": "/home/x/tt-metal",
        "reason": null,
        "grant_root": null,
        "thread_id": "t1",
        "item_id": "i1",
    })
}

/// A live run with this secret.
fn live_run(s: &Station, tag: &str, secret: &str) -> Value {
    let id = unique_id(tag);
    let run = run_record(s, &id, secret);
    fakes().set_active(run["unit"].as_str().unwrap(), true);
    run
}

async fn pending(s: &Station, id: &str) -> Vec<Value> {
    let (status, body) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    body["approvals"].as_array().unwrap().clone()
}

#[tokio::test]
async fn a_drivers_request_waits_for_an_owner_and_gets_their_answer() {
    let s = harness().await;
    let run = live_run(&s, "ask", "s3cret-ask");
    let id = run["id"].as_str().unwrap().to_string();
    s.write_runs(&[run]);

    let (status, body) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals"),
            Some("s3cret-ask"),
            ask("sudo apt install x"),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let aid = body["id"].as_str().unwrap().to_string();

    // The app shows the run waiting on a person, with what it wants to do;
    // members see it too (they can't answer it).
    let (status, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{shown}");
    assert_eq!(shown["state"], "waiting_for_you", "{shown}");
    let a = &shown["approvals"][0];
    assert_eq!(a["id"], aid.as_str());
    assert_eq!(a["command"], "sudo apt install x");
    assert_eq!(a["why"], "uses sudo");
    assert_eq!(a["cwd"], "/home/x/tt-metal");
    assert!(a["decision"].is_null(), "{a}");

    // The driver long-polls; the owner's answer reaches it as soon as it's given.
    let wait = {
        let (client, url) = (
            s.client.clone(),
            format!("{}/bringup/codex/runs/{id}/approvals/{aid}/wait", s.local),
        );
        tokio::spawn(async move {
            let r = client
                .post(url)
                .header("X-Run-Secret", "s3cret-ask")
                .json(&json!({}))
                .send()
                .await
                .unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !wait.is_finished(),
        "no decision yet, so it must still be waiting"
    );
    let (status, body) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/approvals/{aid}"),
            &s.owner,
            json!({ "decision": "decline" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["decision"], "decline");
    let (status, got) = tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the waiting driver hears the decision promptly")
        .unwrap();
    assert_eq!(status, 200);
    assert_eq!(got["decision"], "decline");

    // A later wait on the same request answers straight away.
    let (_, again) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals/{aid}/wait"),
            Some("s3cret-ask"),
            json!({}),
        )
        .await;
    assert_eq!(again["decision"], "decline");

    // Answered: no longer waiting on anyone, and the answer is in the activity log.
    let (_, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(shown["state"], "running", "{shown}");
    assert_eq!(shown["approvals"], json!([]));
    let audit = s.audit().await;
    assert!(
        audit
            .iter()
            .any(|(action, target, result)| action == "bringup_approval"
                && target == &format!("{id}: sudo apt install x")
                && result == "decline"),
        "{audit:?}"
    );
}

#[tokio::test]
async fn only_the_runs_own_secret_lets_a_driver_in() {
    let s = harness().await;
    let a = live_run(&s, "secA", "secret-of-a");
    let b = live_run(&s, "secB", "secret-of-b");
    let (ida, idb) = (
        a["id"].as_str().unwrap().to_string(),
        b["id"].as_str().unwrap().to_string(),
    );
    s.write_runs(&[a, b]);
    let asks = format!("/bringup/codex/runs/{ida}/approvals");

    for (who, secret) in [
        ("no secret", None),
        ("an empty secret", Some("")),
        ("a wrong secret", Some("secret-of-c")),
        ("another run's secret", Some("secret-of-b")),
        ("a prefix of the secret", Some("secret-of-")),
    ] {
        let (status, body) = s.driver(&asks, secret, ask("sudo rm -rf /")).await;
        assert_eq!(status, 401, "{who}: {body}");
        assert_eq!(body["error"], "not this run's driver", "{who}");
    }
    // A device token is not a driver's credential, not even an owner's.
    for token in [&s.owner, &s.member] {
        let (status, body) = s
            .driver_with_token(&asks, token, None, ask("sudo rm -rf /"))
            .await;
        assert_eq!(status, 401, "{body}");
    }
    // Unpaired, from off the box, with nothing at all: the route lets it
    // through to the handler (it's handler-authed), which refuses it.
    let (status, _) = s.call(M::POST, &asks, "", ask("sudo rm -rf /")).await;
    assert_eq!(status, 401);
    // No such run: same answer, so ids can't be probed.
    let (status, _) = s
        .driver(
            "/bringup/codex/runs/nosuchrun/approvals",
            Some("secret-of-a"),
            ask("x"),
        )
        .await;
    assert_eq!(status, 401);
    assert_eq!(
        pending(&s, &ida).await,
        Vec::<Value>::new(),
        "nothing was raised"
    );

    // The right secret works from a phone-facing connection too (with or
    // without a token alongside): the secret is what counts.
    let (status, body) = s
        .driver_with_token(&asks, &s.member, Some("secret-of-a"), ask("sudo ls"))
        .await;
    assert_eq!(status, 200, "{body}");
    let aid = body["id"].as_str().unwrap().to_string();

    // Waiting is the driver's too.
    let wait = format!("/bringup/codex/runs/{ida}/approvals/{aid}/wait");
    for secret in [None, Some("secret-of-b"), Some("nope")] {
        let (status, _) = s.driver(&wait, secret, json!({})).await;
        assert_eq!(status, 401, "{secret:?}");
    }
    let (status, _) = s.driver_with_token(&wait, &s.owner, None, json!({})).await;
    assert_eq!(status, 401);

    // Run B's driver, with its own secret, asking after A's request through
    // its own run: there's no such request there, which reads as cancelled --
    // never A's answer.
    let (status, body) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{ida}/approvals/{aid}"),
            &s.owner,
            json!({ "decision": "accept" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = s
        .driver(
            &format!("/bringup/codex/runs/{idb}/approvals/{aid}/wait"),
            Some("secret-of-b"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["decision"], "cancel");
    let (_, body) = s.driver(&wait, Some("secret-of-a"), json!({})).await;
    assert_eq!(body["decision"], "accept");
}

#[tokio::test]
async fn only_an_owner_answers_and_only_the_request_named() {
    let s = harness().await;
    let a = live_run(&s, "ownA", "own-secret-a");
    let b = live_run(&s, "ownB", "own-secret-b");
    let (ida, idb) = (
        a["id"].as_str().unwrap().to_string(),
        b["id"].as_str().unwrap().to_string(),
    );
    s.write_runs(&[a, b]);
    let (_, body) = s
        .driver(
            &format!("/bringup/codex/runs/{ida}/approvals"),
            Some("own-secret-a"),
            ask("sudo reboot"),
        )
        .await;
    let aid = body["id"].as_str().unwrap().to_string();
    let decide = format!("/bringup/codex/runs/{ida}/approvals/{aid}");
    let accept = json!({ "decision": "accept" });

    // A member is refused, and the refusal is logged.
    let (status, body) = s.call(M::POST, &decide, &s.member, accept.clone()).await;
    assert_eq!(status, 403, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("Only an owner"),
        "{body}"
    );
    let audit = s.audit().await;
    assert!(
        audit
            .iter()
            .any(|(action, target, result)| action == "denied"
                && target == &format!("POST {decide}")
                && result == "denied"),
        "{audit:?}"
    );
    // Unpaired: not even a hearing.
    let (status, _) = s.call(M::POST, &decide, "", accept.clone()).await;
    assert_eq!(status, 401);
    // The driver's secret doesn't make it an owner: it can't answer itself.
    let (status, _) = s
        .driver_with_token(&decide, &s.member, Some("own-secret-a"), accept.clone())
        .await;
    assert_eq!(status, 403);

    // Owners: the request must be named by its own run and id.
    let (status, body) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{idb}/approvals/{aid}"),
            &s.owner,
            accept.clone(),
        )
        .await;
    assert_eq!(status, 404, "another run's id: {body}");
    let (status, _) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{ida}/approvals/0000000000"),
            &s.owner,
            accept.clone(),
        )
        .await;
    assert_eq!(status, 404, "an unknown request");
    // Only the three answers Codex understands; `cancel` is the agent's own.
    for d in ["cancel", "yes", "ACCEPT", ""] {
        let (status, body) = s
            .call(M::POST, &decide, &s.owner, json!({ "decision": d }))
            .await;
        assert_eq!(status, 400, "{d}: {body}");
    }
    let still = pending(&s, &ida).await;
    assert_eq!(still.len(), 1, "nothing above answered it: {still:?}");
    assert!(still[0]["decision"].is_null());

    let (status, body) = s
        .call(
            M::POST,
            &decide,
            &s.owner,
            json!({ "decision": "acceptForSession" }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, body) = s
        .driver(&format!("{decide}/wait"), Some("own-secret-a"), json!({}))
        .await;
    assert_eq!(body["decision"], "acceptForSession");
}

#[tokio::test]
async fn stopping_a_run_cancels_what_its_driver_is_waiting_on() {
    let s = harness().await;
    let run = live_run(&s, "stop", "stop-secret");
    let id = run["id"].as_str().unwrap().to_string();
    let unit = run["unit"].as_str().unwrap().to_string();
    s.write_runs(&[run]);
    let (_, body) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals"),
            Some("stop-secret"),
            ask("git push"),
        )
        .await;
    let aid = body["id"].as_str().unwrap().to_string();
    let wait = {
        let (client, url) = (
            s.client.clone(),
            format!("{}/bringup/codex/runs/{id}/approvals/{aid}/wait", s.local),
        );
        tokio::spawn(async move {
            client
                .post(url)
                .header("X-Run-Secret", "stop-secret")
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A member can't stop it.
    let (status, _) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/stop"),
            &s.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 403);
    assert!(fakes().is_active(&unit));

    let (status, body) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/stop"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["stopped"], true);
    let got = tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the driver hears it was cancelled")
        .unwrap();
    assert_eq!(got["decision"], "cancel");
    assert!(
        fakes()
            .systemctl_log()
            .contains(&format!("--user stop {unit}")),
        "the run's unit is stopped"
    );
    assert!(!fakes().is_active(&unit));

    // Stopped, saved as such, and nothing left to answer.
    let saved = s.saved_runs();
    assert_eq!(saved[0]["stopped"], true);
    assert!(saved[0]["finished"].is_u64(), "{saved:?}");
    let (_, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(shown["state"], "stopped", "{shown}");
    assert_eq!(shown["live"], false);
    assert_eq!(shown["approvals"], json!([]));
}

#[tokio::test]
async fn deleting_a_run_cancels_its_requests() {
    let s = harness().await;
    let run = live_run(&s, "del", "del-secret");
    let id = run["id"].as_str().unwrap().to_string();
    let unit = run["unit"].as_str().unwrap().to_string();
    s.write_runs(&[run]);
    let (_, body) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals"),
            Some("del-secret"),
            ask("sudo x"),
        )
        .await;
    let aid = body["id"].as_str().unwrap().to_string();
    // The unit ended without the agent noticing yet.
    fakes().set_active(&unit, false);
    let (status, body) = s
        .call(
            M::DELETE,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    // The run's secret died with it: its driver can't come back.
    let (status, _) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals/{aid}/wait"),
            Some("del-secret"),
            json!({}),
        )
        .await;
    assert_eq!(status, 401);
    let (status, _) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/approvals/{aid}"),
            &s.owner,
            json!({ "decision": "accept" }),
        )
        .await;
    assert_eq!(status, 404, "the request went with the run");
}

#[tokio::test]
async fn what_a_driver_reports_is_bounded() {
    let s = harness().await;
    let run = live_run(&s, "big", "big-secret");
    let id = run["id"].as_str().unwrap().to_string();
    s.write_runs(&[run]);
    let long = "x".repeat(10_000);
    let (status, _) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals"),
            Some("big-secret"),
            json!({ "why": long, "command": long, "cwd": long, "reason": long, "grant_root": long, "kind": long }),
        )
        .await;
    assert_eq!(status, 200);
    let shown = pending(&s, &id).await;
    for k in ["why", "command", "cwd", "reason", "grant_root", "kind"] {
        let n = shown[0][k].as_str().unwrap().chars().count();
        assert!(
            n <= 2001,
            "{k} is clipped to 2,000 characters (and an ellipsis): {n}"
        );
    }
    // Missing fields read as a command with no reason given, not an error.
    let (status, _) = s
        .driver(
            &format!("/bringup/codex/runs/{id}/approvals"),
            Some("big-secret"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    let shown = pending(&s, &id).await;
    assert_eq!(shown[1]["kind"], "command");
    assert_eq!(shown[1]["why"], "");
    assert!(shown[1]["command"].is_null());
}
