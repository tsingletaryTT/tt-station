// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Bring-up run lifecycle over HTTP: an owner starts a run (launched as a user
//! unit with its secret kept off the command line), anyone paired can watch
//! it, an owner stops, resumes and deletes it. Members are refused (and the
//! refusal logged), inputs are validated before anything starts, and ids or
//! log folders can't reach outside TT Station's own bring-up folder. Engines
//! are fakes (see `bringup_support`): nothing real is launched.

use reqwest::Method as M;
use serde_json::{json, Value};

mod bringup_support;
use bringup_support::{fakes, harness, run_record, unique_id};

/// `POST /bringup/codex/runs` lets one start through at a time across the
/// whole process, so tests that reach it take turns.
static STARTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn denied(audit: &[(String, String, String)], what: &str) -> bool {
    audit
        .iter()
        .any(|(a, t, r)| a == "denied" && t == what && r == "denied")
}

#[tokio::test]
async fn an_owner_starts_stops_and_deletes_a_run() {
    let s = harness().await;
    let _turn = STARTS.lock().await;
    let start = json!({ "model": "Qwen/Qwen3-0.6B" });

    // A member can't start one, and the attempt is logged.
    let (status, body) = s
        .call(M::POST, "/bringup/codex/runs", &s.member, start.clone())
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(denied(&s.audit().await, "POST /bringup/codex/runs"));
    assert!(s.saved_runs().is_empty());

    let (status, body) = s
        .call(M::POST, "/bringup/codex/runs", &s.owner, start.clone())
        .await;
    assert_eq!(status, 202, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    // Saved, with a fresh secret and its own unit and log folder.
    let saved = s.saved_runs();
    assert_eq!(saved.len(), 1);
    let run = &saved[0];
    let (unit, secret) = (
        run["unit"].as_str().unwrap().to_string(),
        run["secret"].as_str().unwrap().to_string(),
    );
    assert_eq!(unit, format!("tt-codex-bringup-{id}"));
    assert_eq!(run["policy"], "smart");
    assert_eq!(run["engine"], "codex");
    assert_eq!(run["model_dir"], "models/autoports/qwen_qwen3_0_6b");
    assert_eq!(
        run["log_dir"].as_str().unwrap(),
        s.bringup_dir().join("codex").join(&id).to_str().unwrap()
    );
    assert!(
        secret.len() >= 32 && secret.chars().all(|c| c.is_ascii_hexdigit()),
        "{secret}"
    );

    // Launched under the lease wrapper with every stage, as that unit; the
    // secret reaches the driver through systemd-run's environment only, never
    // its command line (which any user on the box can read).
    let argv = fakes().launched(&unit).expect("systemd-run was called");
    assert!(argv.contains(&format!("--unit={unit}")), "{argv:?}");
    assert!(
        argv.contains(&format!("--setenv=TT_STATION_RUN={id}")),
        "{argv:?}"
    );
    assert!(
        argv.contains(&"--setenv=TT_STATION_RUN_SECRET".to_string()),
        "{argv:?}"
    );
    assert!(argv.iter().all(|a| !a.contains(&secret)), "{argv:?}");
    assert_eq!(
        fakes().launched_secret(&unit).as_deref(),
        Some(secret.as_str())
    );
    let script = argv.last().unwrap();
    assert!(script.contains("lease_run.py"), "{script}");
    assert!(script.contains("codex_bringup.py"), "{script}");
    assert!(script.contains("'HF_MODEL=Qwen/Qwen3-0.6B'"), "{script}");
    assert!(script.contains("11-tti-release.txt"), "{script}");
    for driver in [
        "codex_bringup.py",
        "claude_bringup.py",
        "permission_mcp.py",
        "lease_run.py",
        "sandbox_exec.py",
    ] {
        assert!(s.bringup_dir().join(driver).is_file(), "{driver}");
    }

    // Confined: the unit properties a user manager applies, and the stage
    // starts through the Landlock launcher, which hides TT Station's folder
    // and keys and lets it write only its own folder, the repo and caches.
    for prop in [
        "--property=NoNewPrivileges=yes",
        "--property=RestrictSUIDSGID=yes",
        "--property=RestrictNamespaces=yes",
        "--property=KeyringMode=private",
    ] {
        assert!(argv.contains(&prop.to_string()), "{prop}: {argv:?}");
    }
    assert!(
        argv.iter()
            .any(|a| a.starts_with("--property=SystemCallFilter=~@mount")),
        "{argv:?}"
    );
    // This box's user manager can't make mount namespaces (the fake reports
    // nothing), so none of the properties it would silently skip.
    assert!(
        !argv.iter().any(|a| a.contains("InaccessiblePaths")
            || a.contains("ProtectHome")
            || a.contains("PrivateUsers")),
        "{argv:?}"
    );
    let launcher = argv
        .iter()
        .position(|a| a.ends_with("sandbox_exec.py"))
        .expect("the stage starts through the sandbox launcher");
    let sh = argv.iter().position(|a| a == "sh").unwrap();
    assert!(launcher < sh, "{argv:?}");
    let pair = |flag: &str, path: &std::path::Path| {
        argv.windows(2)
            .any(|w| w[0] == flag && w[1] == path.display().to_string())
    };
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap());
    assert!(pair("--deny", s.config.path()), "{argv:?}");
    assert!(pair("--deny", &home.join(".ssh")), "{argv:?}");
    assert!(pair("--deny", &home.join(".gnupg")), "{argv:?}");
    assert!(
        pair("--rw", &s.bringup_dir().join("codex").join(&id)),
        "{argv:?}"
    );
    assert!(pair("--rw", &fakes().repo), "{argv:?}");
    assert!(
        pair("--ro-file", &s.bringup_dir().join("codex_bringup.py")),
        "{argv:?}"
    );
    assert!(
        !pair("--rw", &s.bringup_dir()),
        "never the whole bring-up folder"
    );
    assert!(!pair("--rw", &home), "never the home folder");
    assert_eq!(argv[sh + 1], "-c");
    assert!(
        s.audit().await.iter().any(|(a, t, r)| a == "host_run"
            && t == "bring-up of Qwen/Qwen3-0.6B with codex"
            && r == "ok"),
        "starting one is logged"
    );

    // Anyone paired can watch it; nobody is shown its secret.
    for token in [&s.owner, &s.member] {
        let (status, list) = s
            .call(M::GET, "/bringup/codex/runs", token, json!({}))
            .await;
        assert_eq!(status, 200, "{list}");
        assert_eq!(list["runs"][0]["id"], id.as_str());
        assert_eq!(list["runs"][0]["state"], "running");
        assert_eq!(list["runs"][0]["total"], 11);
        assert!(!list.to_string().contains(&secret), "{list}");
        let (status, one) = s
            .call(
                M::GET,
                &format!("/bringup/codex/runs/{id}"),
                token,
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{one}");
        assert!(!one.to_string().contains(&secret), "{one}");
    }
    let (status, _) = s.call(M::GET, "/bringup/codex/runs", "", json!({})).await;
    assert_eq!(status, 401);

    // One at a time.
    let (status, body) = s
        .call(M::POST, "/bringup/codex/runs", &s.owner, start.clone())
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(s.saved_runs().len(), 1);

    // A live run can't be deleted, by anyone.
    let (status, _) = s
        .call(
            M::DELETE,
            &format!("/bringup/codex/runs/{id}"),
            &s.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 403);
    let (status, body) = s
        .call(
            M::DELETE,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 409, "{body}");

    // Stopped, then deleted: its logs go, the port's code is left alone.
    let (status, _) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/stop"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    let log_dir = s.bringup_dir().join("codex").join(&id);
    std::fs::write(log_dir.join("runner.log"), "x\n").unwrap();
    let (status, body) = s
        .call(
            M::DELETE,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["deleted"], id.as_str());
    assert_eq!(body["kept"], "models/autoports/qwen_qwen3_0_6b");
    assert!(!log_dir.exists());
    assert!(s.saved_runs().is_empty());
    let (status, _) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_start_is_checked_before_anything_runs() {
    let s = harness().await;
    let _turn = STARTS.lock().await;
    let start = |body: Value| s.call(M::POST, "/bringup/codex/runs", &s.owner, body);

    for model in [
        "../../etc/passwd",
        "Qwen/../../x",
        "Qwen/..",
        "Qwen/Qwen3/extra",
        "Qwen",
        "/Qwen3",
        "Qwen/",
        "-rf/x",
        "a b/c",
        "Qwen/Qwen3;reboot",
        "Qwen/$(id)",
        "Qwen/x'y",
        "",
    ] {
        let (status, body) = start(json!({ "model": model })).await;
        assert_eq!(status, 400, "{model:?}: {body}");
    }
    let (status, body) = start(json!({ "model": "Qwen/Qwen3-0.6B", "policy": "yolo" })).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "Policy is ask, smart or auto.");
    let (status, body) = start(json!({ "model": "Qwen/Qwen3-0.6B", "engine": "sh" })).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "The engine is codex or claude.");
    // Claude Code is installed here but not signed in.
    let (status, body) = start(json!({ "model": "Qwen/Qwen3-0.6B", "engine": "claude" })).await;
    assert_eq!(status, 412, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .starts_with("Claude Code isn't ready"));
    // Orchard has the chips.
    s.state.set_orchard_busy(true);
    let (status, body) = start(json!({ "model": "Qwen/Qwen3-0.6B" })).await;
    assert_eq!(status, 409, "{body}");
    s.state.set_orchard_busy(false);

    assert!(s.saved_runs().is_empty(), "nothing was started");
    assert!(
        !s.bringup_dir().join("codex").exists(),
        "no log folder was made"
    );
}

#[tokio::test]
async fn unknown_or_traversing_run_ids_reach_nothing() {
    let s = harness().await;
    // A folder next to the runs' that a traversing id would name.
    let codex = s.bringup_dir().join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    let victim = s.config.path().join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("keep.txt"), "x").unwrap();
    let run = run_record(&s, &unique_id("real"), "sec");
    s.write_runs(&[run]);

    for id in [
        "..",
        "..%2F..%2Fvictim",
        "%2e%2e%2f%2e%2e%2fvictim",
        "nosuchrun",
        "",
    ] {
        let base = format!("/bringup/codex/runs/{id}");
        let (status, body) = s.call(M::GET, &base, &s.owner, json!({})).await;
        assert!(matches!(status, 404 | 405), "GET {id}: {status} {body}");
        let (status, body) = s.call(M::DELETE, &base, &s.owner, json!({})).await;
        assert!(matches!(status, 404 | 405), "DELETE {id}: {status} {body}");
        for action in ["stop", "resume", "tell", "package", "publish"] {
            let (status, body) = s
                .call(
                    M::POST,
                    &format!("{base}/{action}"),
                    &s.owner,
                    json!({ "text": "hi" }),
                )
                .await;
            assert_eq!(status, 404, "{action} {id}: {body}");
        }
    }
    assert!(victim.join("keep.txt").is_file());
    assert_eq!(s.saved_runs().len(), 1, "the real run is untouched");
}

#[tokio::test]
async fn actions_are_owner_only_named_and_in_order() {
    let s = harness().await;
    let run = run_record(&s, &unique_id("act"), "sec");
    let id = run["id"].as_str().unwrap().to_string();
    s.write_runs(&[run]);
    let s = &s;
    let act = |token: String, action: &str, body: Value| {
        let path = format!("/bringup/codex/runs/{id}/{action}");
        async move { s.call(M::POST, &path, &token, body).await }
    };

    // Members: refused for every action, logged.
    for action in [
        "stop",
        "resume",
        "free-chips",
        "tell",
        "package",
        "serve",
        "try",
        "publish",
        "list",
    ] {
        let (status, body) = act(s.member.clone(), action, json!({ "text": "hi" })).await;
        assert_eq!(status, 403, "{action}: {body}");
    }
    assert!(denied(
        &s.audit().await,
        &format!("POST /bringup/codex/runs/{id}/publish")
    ));

    for action in ["rm", "STOP", "delete", "stop;reboot"] {
        let (status, body) = act(s.owner.clone(), action, json!({})).await;
        assert_eq!(status, 400, "{action}: {body}");
        assert_eq!(
            body["error"],
            "Use stop, resume, package, serve, publish or list."
        );
    }
    // Packaging and trying wait for the vLLM stage; serving and publishing
    // for a package.
    for (action, why) in [
        ("package", "Package it once the vLLM stage (9) has passed."),
        ("try", "Try it once the vLLM stage (9) has passed."),
        (
            "serve",
            "Package it first: there's no package to serve yet.",
        ),
        (
            "publish",
            "Package it first: there's nothing to publish yet.",
        ),
        ("list", "Package it first: there's nothing to publish yet."),
    ] {
        let (status, body) = act(s.owner.clone(), action, json!({})).await;
        assert_eq!(status, 409, "{action}: {body}");
        assert_eq!(body["error"], why, "{action}");
    }
    // A note needs words, and not too many.
    let (status, body) = act(s.owner.clone(), "tell", json!({ "text": "   " })).await;
    assert_eq!(status, 400, "{body}");
    let (status, body) = act(s.owner.clone(), "tell", json!({ "text": "é".repeat(4001) })).await;
    assert_eq!(status, 400, "{body}");
    let log_dir = s.bringup_dir().join("codex").join(&id);
    assert!(!log_dir.join("notes.jsonl").exists());
    assert!(fakes()
        .launched(&format!("tt-codex-bringup-{id}"))
        .is_none());
}

#[tokio::test]
async fn a_resumed_run_carries_on_from_its_first_unfinished_stage() {
    let s = harness().await;
    let run = run_record(&s, &unique_id("res"), "sec");
    let id = run["id"].as_str().unwrap().to_string();
    let unit = run["unit"].as_str().unwrap().to_string();
    let log_dir = s.bringup_dir().join("codex").join(&id);
    std::fs::write(
        log_dir.join("manifest.txt"),
        "stage_1_thread_id=t1\nstage_1_terminal_status=complete\nstage_1_check=pass\n\
         stage_2_thread_id=t2\nstage_2_terminal_status=complete\n\
         stage_3_thread_id=t3\nstage_3_terminal_status=blocked\n",
    )
    .unwrap();
    std::fs::write(log_dir.join("exit"), "3").unwrap();

    // Another run on the chips: not now.
    let other = run_record(&s, &unique_id("busy"), "sec2");
    fakes().set_active(other["unit"].as_str().unwrap(), true);
    s.write_runs(&[
        {
            let mut r = run.clone();
            r["stopped"] = json!(true);
            r["finished"] = json!(1_700_000_100u64);
            r
        },
        other.clone(),
    ]);
    let resume = format!("/bringup/codex/runs/{id}/resume");
    let (status, body) = s.call(M::POST, &resume, &s.owner, json!({})).await;
    assert_eq!(status, 409, "{body}");
    assert!(fakes().launched(&unit).is_none());
    fakes().set_active(other["unit"].as_str().unwrap(), false);

    let (status, body) = s.call(M::POST, &resume, &s.owner, json!({})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resumed_at_stage"], 3);
    // Stage 3 had a thread: resumed in that conversation, not restarted.
    let argv = fakes().launched(&unit).expect("relaunched");
    let script = argv.last().unwrap();
    assert!(script.contains("'--resume-stage' '3'"), "{script}");
    assert!(!script.contains("--start-index"), "{script}");
    let saved = s.saved_runs();
    let me = saved.iter().find(|r| r["id"] == id.as_str()).unwrap();
    // A new secret for the new launch, handed to the new unit; the old one is dead.
    let secret = me["secret"].as_str().unwrap();
    assert_ne!(secret, "sec");
    assert_eq!(fakes().launched_secret(&unit).as_deref(), Some(secret));
    assert_eq!(me["stopped"], false);
    assert!(me["finished"].is_null());
    assert!(me["exit"].is_null());
    let (_, shown) = s
        .call(
            M::GET,
            &format!("/bringup/codex/runs/{id}"),
            &s.member,
            json!({}),
        )
        .await;
    assert_eq!(shown["state"], "running", "{shown}");
    assert_eq!(shown["current"], 3);
    assert_eq!(shown["done"], 2);
    // And now it's the live one, it can't be resumed twice.
    let (status, _) = s.call(M::POST, &resume, &s.owner, json!({})).await;
    assert_eq!(status, 409);
}

#[tokio::test]
async fn a_note_restarts_the_stage_it_is_for() {
    let s = harness().await;
    let run = run_record(&s, &unique_id("tell"), "sec");
    let id = run["id"].as_str().unwrap().to_string();
    let unit = run["unit"].as_str().unwrap().to_string();
    fakes().set_active(&unit, true);
    s.write_runs(&[run]);
    let log_dir = s.bringup_dir().join("codex").join(&id);
    std::fs::write(
        log_dir.join("manifest.txt"),
        "stage_1_thread_id=t1\nstage_1_terminal_status=complete\nstage_2_thread_id=t2\n",
    )
    .unwrap();
    let (status, body) = s
        .call(
            M::POST,
            &format!("/bringup/codex/runs/{id}/tell"),
            &s.owner,
            json!({ "text": "  use bfloat16 for the KV cache  " }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["resumed_at_stage"], 2);
    assert_eq!(
        std::fs::read_to_string(log_dir.join("note.txt")).unwrap(),
        "use bfloat16 for the KV cache"
    );
    let note: Value = serde_json::from_str(
        std::fs::read_to_string(log_dir.join("notes.jsonl"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(note["stage"], 2);
    assert_eq!(note["text"], "use bfloat16 for the KV cache");
    // The running stage was stopped and relaunched in its conversation, with the note.
    assert!(fakes()
        .systemctl_log()
        .contains(&format!("--user stop {unit}")));
    let script = fakes().launched(&unit).unwrap().last().unwrap().clone();
    assert!(script.contains("'--resume-stage' '2'"), "{script}");
    assert!(script.contains("--note-file"), "{script}");
}

#[tokio::test]
async fn deleting_only_ever_removes_the_runs_own_log_folder() {
    let s = harness().await;
    let codex = s.bringup_dir().join("codex");
    std::fs::create_dir_all(&codex).unwrap();
    // Folders a damaged runs file could point a run at.
    let outside = s.config.path().join("outside");
    let parent_walk = s.config.path().join("walked");
    for d in [&outside, &parent_walk] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("keep.txt"), "x").unwrap();
    }
    let mut runs = Vec::new();
    for log_dir in [
        outside.clone(),
        codex.clone(),
        codex.join("..").join("..").join("walked"),
    ] {
        let mut r = run_record(&s, &unique_id("del"), "sec");
        r["log_dir"] = json!(log_dir);
        runs.push(r);
    }
    s.write_runs(&runs);
    for r in &runs {
        let id = r["id"].as_str().unwrap();
        let (status, body) = s
            .call(
                M::DELETE,
                &format!("/bringup/codex/runs/{id}"),
                &s.owner,
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }
    assert!(s.saved_runs().is_empty());
    assert!(
        outside.join("keep.txt").is_file(),
        "outside the bring-up folder"
    );
    assert!(codex.is_dir(), "the bring-up folder itself");
    assert!(
        parent_walk.join("keep.txt").is_file(),
        "a log folder that only looks inside, through `..`"
    );
}

#[tokio::test]
async fn engines_and_the_survey_are_for_paired_devices() {
    let s = harness().await;
    let (status, _) = s.call(M::GET, "/bringup/engines", "", json!({})).await;
    assert_eq!(status, 401);
    let (status, body) = s
        .call(M::GET, "/bringup/engines", &s.member, json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    let engines = body["engines"].as_array().unwrap();
    let find = |id: &str| engines.iter().find(|e| e["id"] == id).unwrap().clone();
    let codex = find("codex");
    assert_eq!(codex["ready"], true, "{codex}");
    assert!(codex["why_not"].is_null());
    assert_eq!(codex["detail"]["stages"].as_array().unwrap().len(), 11);
    assert_eq!(codex["detail"]["stages"][0], "Functional decoder");
    assert_eq!(codex["detail"]["stages"][8], "vLLM");
    let claude = find("claude");
    assert_eq!(claude["installed"], true);
    assert_eq!(claude["signed_in"], false);
    assert_eq!(claude["ready"], false);
    assert_eq!(claude["why_not"], "Claude Code isn't signed in yet.");
    assert_eq!(find("orchard")["ready"], false);

    // The survey checks the id before asking Hugging Face anything.
    let (status, _) = s
        .call(M::POST, "/bringup/survey", "", json!({ "model": "a/b" }))
        .await;
    assert_eq!(status, 401);
    for model in [
        "../x",
        "a/b/c",
        "https://huggingface.co/../../x",
        "a/..",
        "",
    ] {
        let (status, body) = s
            .call(
                M::POST,
                "/bringup/survey",
                &s.member,
                json!({ "model": model }),
            )
            .await;
        assert_eq!(status, 400, "{model}: {body}");
        assert_eq!(body["error"], "Use a Hugging Face id like org/model-name.");
    }
}

#[tokio::test]
async fn signing_claude_in_is_an_owners_and_needs_the_desktop() {
    let s = harness().await;
    let (status, body) = s
        .call(M::POST, "/bringup/claude/login", &s.member, json!({}))
        .await;
    assert_eq!(status, 403, "{body}");
    assert!(denied(&s.audit().await, "POST /bringup/claude/login"));
    // No one at the box's desktop here: told to sign in there, nothing opened.
    let (status, body) = s
        .call(M::POST, "/bringup/claude/login", &s.owner, json!({}))
        .await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("claude auth login"));
}
