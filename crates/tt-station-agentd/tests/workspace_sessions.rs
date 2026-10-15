// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Code sessions over HTTP, short of starting Claude Code: which folders a
//! session may be given (inside the home folder, not hidden, not the box's own,
//! links and `..` resolved first), the policy rules, who sees and may touch
//! which session, and the per-session routes that only change what's on disk
//! (settings, rename, forget, delete), including across a restart.
//!
//! Nothing here starts Claude Code: no model is serving, so `/message` stops
//! at its `409` before the spawn, and no test sends a message that could get
//! past it. (`claude` is found at `$HOME/.local/bin/claude`, not through
//! `PATH`, so a fake on `PATH` would not stand in for it.) `GET /code` runs
//! `docker inspect`, which the hardware stubs answer.
//! `tests/code_isolation.rs` covers the member-vs-owner sandbox gate and the
//! shared-folder warning; this file doesn't repeat those.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reqwest::Method as M;
use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

struct Station {
    base: String,
    owner: String,
    member: String,
    other_member: String,
    config: PathBuf,
    client: reqwest::Client,
}

/// The box, with its config folder at `config`, and an owner and two members
/// paired (the first paired device is the owner).
async fn station_at(config: &Path) -> (Station, AppState) {
    let state = box_at(config);
    let base = serve(&state).await;
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    let other_member = support::pair(&base, &state).await;
    (
        Station {
            base,
            owner,
            member,
            other_member,
            config: config.to_path_buf(),
            client: reqwest::Client::new(),
        },
        state,
    )
}

/// The box's state on `config`, its paired devices kept there too (so a
/// restart, a new `AppState` on the same folder, still knows them).
fn box_at(config: &Path) -> AppState {
    AppState::new_persisting(
        "qb2-lab".into(),
        "4xBH".into(),
        Arc::new(DstackBackend),
        config.join("tokens.json"),
    )
    .with_config_dir(config.to_path_buf())
}

async fn serve(state: &AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

impl Station {
    async fn call(&self, method: M, path: &str, token: &str, body: Value) -> (u16, Value) {
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

    /// Starts a session and returns its id, failing the test if it was refused.
    async fn start(&self, token: &str, body: Value) -> String {
        let (status, said) = self.call(M::POST, "/code/sessions", token, body).await;
        assert_eq!(status, 200, "{said}");
        said["session"]["id"].as_str().unwrap().to_string()
    }

    fn session_file(&self, id: &str) -> Value {
        let path = self.config.join("code").join(id).join("session.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME"))
}

/// A folder in the home folder (where Code takes projects from), removed afterwards.
fn in_home(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(home())
        .expect("a folder in the home folder")
}

fn project() -> tempfile::TempDir {
    in_home("tt-station-ws-test-")
}

fn real(p: &Path) -> String {
    std::fs::canonicalize(p).unwrap().display().to_string()
}

fn error_of(body: &Value) -> &str {
    body["error"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn an_owners_folder_must_be_a_real_project_folder_inside_home() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let outside = tempfile::tempdir().unwrap();
    let hidden = in_home(".tt-station-ws-test-");
    let comma = in_home("tt-station-ws-test,comma-");
    // Links in a project: to a folder outside home, and to a hidden one in it.
    std::os::unix::fs::symlink(outside.path(), p.path().join("out")).unwrap();
    std::os::unix::fs::symlink(hidden.path(), p.path().join("keys")).unwrap();
    let pf = p.path().display().to_string();
    let home_parent = home()
        .parent()
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| "/".into());

    for (folder, why) in [
        (format!("{pf}/does-not-exist"), "doesn't exist"),
        (home().display().to_string(), "inside your home"),
        (home_parent, "inside your home"),
        (outside.path().display().to_string(), "inside your home"),
        // `..` is resolved before the rules apply: these are the home folder and its parent.
        (format!("{pf}/.."), "inside your home"),
        (format!("{pf}/../.."), "inside your home"),
        // A link is followed to where it really goes.
        (format!("{pf}/out"), "inside your home"),
        (format!("{pf}/keys"), "hidden folder"),
        (hidden.path().display().to_string(), "hidden folder"),
        // A file isn't a folder.
        ("/etc/hostname".to_string(), ""),
        (comma.path().display().to_string(), "colon, comma"),
    ] {
        let (status, said) = b
            .call(
                M::POST,
                "/code/sessions",
                &b.owner,
                json!({ "folder": folder, "sandbox": false }),
            )
            .await;
        assert_eq!(status, 400, "{folder}: {said}");
        assert!(error_of(&said).contains(why), "{folder}: {said}");
    }
    // Nothing was made for the refusals.
    let (_, code) = b.call(M::GET, "/code", &b.owner, json!({})).await;
    assert_eq!(code["sessions"], json!([]), "{code}");

    // A path that only goes through `..` to a real project is kept as its real path.
    let name = p.path().file_name().unwrap().to_string_lossy().to_string();
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": format!("{pf}/../{name}/."), "sandbox": false }),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["session"]["folder"], real(p.path()), "{said}");
    assert_eq!(said["session"]["title"], name.as_str());
}

#[tokio::test]
async fn policies_are_checked_and_dont_ask_is_for_the_sandbox_only() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let folder = p.path().display().to_string();

    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": folder, "policy": "yolo" }),
        )
        .await;
    assert_eq!(status, 400, "{said}");
    // On the box itself it always asks before commands.
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": folder, "sandbox": false, "policy": "auto" }),
        )
        .await;
    assert_eq!(status, 400, "{said}");
    assert!(error_of(&said).contains("only for the sandbox"), "{said}");

    // In the sandbox it may; and with no policy given, it asks.
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.member,
            json!({ "folder": folder, "policy": "auto" }),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["session"]["policy"], "auto");
    assert_eq!(said["session"]["sandbox"], true, "sandboxed by default");
    let (_, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": folder }),
        )
        .await;
    assert_eq!(said["session"]["policy"], "ask", "{said}");
    assert_eq!(said["session"]["sandbox"], true, "{said}");
}

#[tokio::test]
async fn a_folder_of_several_projects_asks_which_one() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    for name in ["api", "web"] {
        std::fs::create_dir_all(p.path().join(name).join(".git")).unwrap();
    }
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": p.path() }),
        )
        .await;
    assert_eq!(status, 409, "{said}");
    let projects: Vec<&str> = said["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    let root = real(p.path());
    assert_eq!(
        projects,
        vec![format!("{root}/api"), format!("{root}/web")],
        "{said}"
    );
    // One of them is fine.
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.owner,
            json!({ "folder": p.path().join("api") }),
        )
        .await;
    assert_eq!(status, 200, "{said}");
}

#[tokio::test]
async fn a_member_cant_give_the_sandbox_the_boxs_own_folders() {
    // The config folder in the home folder, as on a real box (~/.config is
    // hidden, so a visible one is what puts the "box's own" rule to the test).
    let config = in_home("tt-station-ws-cfg-");
    let (b, _state) = station_at(config.path()).await;
    let holder = in_home("tt-station-ws-test-");
    let cfg_inside = holder.path().join("cfg");
    std::fs::create_dir_all(&cfg_inside).unwrap();

    let mut refused = vec![
        // TT Station's own settings and state...
        config.path().to_path_buf(),
        // ...and a folder inside them.
        config.path().join("code"),
    ];
    // This checkout: the box runs the agent built from it.
    let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf();
    if std::fs::canonicalize(&checkout)
        .unwrap()
        .starts_with(std::fs::canonicalize(home()).unwrap())
    {
        refused.push(checkout.clone());
        // A folder holding it would hand the sandbox the checkout too.
        refused.push(checkout.parent().unwrap().to_path_buf());
        refused.push(checkout.join("crates"));
    }
    std::fs::create_dir_all(config.path().join("code")).unwrap();
    for folder in &refused {
        let (status, said) = b
            .call(
                M::POST,
                "/code/sessions",
                &b.member,
                json!({ "folder": folder }),
            )
            .await;
        assert_eq!(status, 403, "{}: {said}", folder.display());
        assert!(
            error_of(&said).contains("software the box runs"),
            "{}: {said}",
            folder.display()
        );
    }
    // The member's other refusals are 403s too (an owner's are 400s).
    for folder in [
        home().display().to_string(),
        format!("{}/missing", holder.path().display()),
        "/tmp".to_string(),
    ] {
        let (status, said) = b
            .call(
                M::POST,
                "/code/sessions",
                &b.member,
                json!({ "folder": folder }),
            )
            .await;
        assert_eq!(status, 403, "{folder}: {said}");
    }
    // An ordinary project folder is fine.
    let (status, said) = b
        .call(
            M::POST,
            "/code/sessions",
            &b.member,
            json!({ "folder": cfg_inside }),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    // Only the member's session was made, in the folder they were allowed.
    let (_, code) = b.call(M::GET, "/code", &b.owner, json!({})).await;
    assert_eq!(code["sessions"].as_array().unwrap().len(), 1, "{code}");
    assert_eq!(code["sessions"][0]["folder"], real(&cfg_inside));
}

#[tokio::test]
async fn sessions_are_listed_by_role_and_members_share_sandboxed_ones() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let q = project();
    let mine = b
        .start(&b.member, json!({ "folder": p.path(), "sandbox": true }))
        .await;
    let owners_sandboxed = b.start(&b.owner, json!({ "folder": q.path() })).await;
    let owners_outside = b
        .start(&b.owner, json!({ "folder": q.path(), "sandbox": false }))
        .await;
    let ids = |code: &Value| -> Vec<String> {
        let mut v: Vec<String> = code["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_string())
            .collect();
        v.sort();
        v
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };

    let (status, code) = b.call(M::GET, "/code", &b.owner, json!({})).await;
    assert_eq!(status, 200, "{code}");
    assert_eq!(
        ids(&code),
        sorted(vec![
            mine.clone(),
            owners_sandboxed.clone(),
            owners_outside.clone()
        ])
    );
    assert_eq!(code["outside_sandbox"], true);
    // A member sees the sandboxed ones, whoever started them: sandboxed Code is
    // the household's (`workspace::may_use`, `approvals`), never one outside.
    for member in [&b.member, &b.other_member] {
        let (_, code) = b.call(M::GET, "/code", member, json!({})).await;
        assert_eq!(
            ids(&code),
            sorted(vec![mine.clone(), owners_sandboxed.clone()])
        );
        assert_eq!(code["outside_sandbox"], false);
    }
    // And may open one; never the one outside the sandbox.
    let (status, said) = b
        .call(
            M::GET,
            &format!("/code/sessions/{mine}"),
            &b.other_member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    let (status, said) = b
        .call(
            M::GET,
            &format!("/code/sessions/{owners_outside}"),
            &b.other_member,
            json!({}),
        )
        .await;
    assert_eq!(status, 403, "{said}");

    // The member who started it is recorded (for the shared-folder warning);
    // an owner's session names nobody.
    assert!(b.session_file(&mine)["member"].is_string());
    assert!(b.session_file(&owners_sandboxed).get("member").is_none());

    // Nobody unpaired gets anything.
    let r = b
        .client
        .get(format!("{}/code", b.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 401);
    let (status, _) = b
        .call(
            M::GET,
            &format!("/code/sessions/{mine}"),
            "not-a-token",
            json!({}),
        )
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn settings_change_thinking_only_never_the_sandbox() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let id = b.start(&b.member, json!({ "folder": p.path() })).await;

    // Whatever else the body says, only `think` is read.
    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/settings"),
            &b.member,
            json!({ "think": true, "sandbox": false, "policy": "auto", "folder": "/", "member": null }),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["session"]["think"], true);
    assert_eq!(said["session"]["sandbox"], true);
    assert_eq!(said["session"]["policy"], "ask");
    let on_disk = b.session_file(&id);
    assert_eq!(on_disk["sandbox"], true);
    assert_eq!(on_disk["think"], true);
    assert_eq!(on_disk["folder"], real(p.path()));
    assert!(on_disk["member"].is_string(), "{on_disk}");
    // Still a member's to use.
    let (status, _) = b
        .call(
            M::GET,
            &format!("/code/sessions/{id}"),
            &b.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200);

    // An empty body changes nothing.
    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/settings"),
            &b.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["session"]["think"], true);

    for bad in ["1700000000-nope00", "a.b", &"x".repeat(41)] {
        let (status, said) = b
            .call(
                M::POST,
                &format!("/code/sessions/{bad}/settings"),
                &b.owner,
                json!({ "think": true }),
            )
            .await;
        assert_eq!(status, 404, "{bad}: {said}");
    }
}

#[tokio::test]
async fn a_rename_is_trimmed_bounded_and_kept() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
    let rename = |title: String| {
        let path = format!("/code/sessions/{id}/rename");
        let b = &b;
        async move {
            b.call(M::POST, &path, &b.owner, json!({ "title": title }))
                .await
        }
    };

    for empty in ["", "   ", "\n\t"] {
        let (status, said) = rename(empty.to_string()).await;
        assert_eq!(status, 400, "{empty:?}: {said}");
    }
    let (status, said) = rename("x".repeat(121)).await;
    assert_eq!(status, 400, "{said}");
    let (status, said) = rename(format!("  {}  ", "x".repeat(120))).await;
    assert_eq!(status, 200, "the spaces around it don't count: {said}");

    let (status, said) = rename("  The website  ".to_string()).await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["session"]["title"], "The website");
    assert_eq!(b.session_file(&id)["title"], "The website");
    // The folder isn't renamed with it.
    assert!(p.path().is_dir());

    // A name is up to 120 characters, as the refusal says, not 120 bytes.
    let accented = "é".repeat(100);
    let (status, said) = rename(accented.clone()).await;
    assert_eq!(status, 200, "100 characters (200 bytes): {said}");
    assert_eq!(said["session"]["title"], accented.as_str());

    let (status, _) = b
        .call(
            M::POST,
            "/code/sessions/1700000000-nope00/rename",
            &b.owner,
            json!({ "title": "x" }),
        )
        .await;
    assert_eq!(status, 404);
    // Without a title at all: the body is refused.
    let (status, _) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/rename"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn forgetting_an_always_rule_removes_only_that_rule() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
    // Rules as "Always" answers leave them (`/approve` needs a running agent).
    let path = config.path().join("code").join(&id).join("session.json");
    let mut s = b.session_file(&id);
    s["always"] = json!(["edits", "Bash:pytest", "Bash=make deploy"]);
    std::fs::write(&path, s.to_string()).unwrap();

    let forget = |rule: &str| {
        let path = format!("/code/sessions/{id}/forget");
        let body = json!({ "rule": rule });
        let b = &b;
        async move { b.call(M::POST, &path, &b.owner, body).await }
    };
    let (status, said) = forget("Bash:pytest").await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["always"], json!(["edits", "Bash=make deploy"]));
    // Exact match only: a prefix or a different spelling forgets nothing.
    for nothing in ["Bash", "Bash=make", "EDITS", ""] {
        let (status, said) = forget(nothing).await;
        assert_eq!(status, 200, "{said}");
        assert_eq!(
            said["always"],
            json!(["edits", "Bash=make deploy"]),
            "{nothing:?}"
        );
    }
    assert_eq!(
        b.session_file(&id)["always"],
        json!(["edits", "Bash=make deploy"])
    );
    let (_, got) = b
        .call(M::GET, &format!("/code/sessions/{id}"), &b.owner, json!({}))
        .await;
    assert_eq!(
        got["session"]["always"],
        json!(["edits", "Bash=make deploy"])
    );

    let (status, _) = b
        .call(
            M::POST,
            "/code/sessions/1700000000-nope00/forget",
            &b.owner,
            json!({ "rule": "edits" }),
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn deleting_a_session_forgets_it_but_keeps_the_project() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    std::fs::write(p.path().join("main.py"), "print('hi')\n").unwrap();
    let id = b.start(&b.member, json!({ "folder": p.path() })).await;
    let other = b.start(&b.owner, json!({ "folder": p.path() })).await;

    let (status, said) = b
        .call(
            M::DELETE,
            &format!("/code/sessions/{id}"),
            &b.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["deleted"], true);
    assert!(!config.path().join("code").join(&id).exists());
    assert_eq!(
        std::fs::read_to_string(p.path().join("main.py")).unwrap(),
        "print('hi')\n"
    );
    // Gone everywhere; the other session in the same folder isn't.
    for (m, path) in [
        (M::GET, format!("/code/sessions/{id}")),
        (M::DELETE, format!("/code/sessions/{id}")),
        (M::GET, format!("/code/sessions/{id}/changes")),
    ] {
        let (status, said) = b.call(m.clone(), &path, &b.owner, json!({})).await;
        assert_eq!(status, 404, "{m} {path}: {said}");
    }
    let (_, code) = b.call(M::GET, "/code", &b.owner, json!({})).await;
    assert_eq!(code["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(code["sessions"][0]["id"], other.as_str());

    // Ids that aren't ours never reach the disk: the sessions folder and the
    // holder's script beside the sessions stay put.
    std::fs::write(config.path().join("code").join("code_holder.py"), "#").unwrap();
    for bad in ["code_holder.py", "..", "a%2F..", "%2E%2E"] {
        let (status, said) = b
            .call(
                M::DELETE,
                &format!("/code/sessions/{bad}"),
                &b.owner,
                json!({}),
            )
            .await;
        assert_eq!(status, 404, "{bad}: {said}");
    }
    assert!(config.path().join("code").join("code_holder.py").is_file());
    assert!(config.path().join("code").join(&other).is_dir());
}

#[tokio::test]
async fn a_message_needs_words_and_a_serving_model() {
    let config = tempfile::tempdir().unwrap();
    let (b, _state) = station_at(config.path()).await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
    let path = format!("/code/sessions/{id}/message");

    let (status, said) = b
        .call(M::POST, &path, &b.owner, json!({ "text": "  \n " }))
        .await;
    assert_eq!(status, 400, "{said}");
    // Nothing is serving here, so it stops before starting Claude Code.
    let (status, said) = b
        .call(M::POST, &path, &b.owner, json!({ "text": "fix the tests" }))
        .await;
    assert_eq!(status, 409, "{said}");
    assert!(error_of(&said).contains("No model is serving"), "{said}");
    // Not recorded as sent: the session is idle, with only its start event.
    let (_, got) = b
        .call(M::GET, &format!("/code/sessions/{id}"), &b.owner, json!({}))
        .await;
    assert_eq!(got["session"]["busy"], false, "{got}");
    assert_eq!(got["session"]["running"], false, "{got}");
    let kinds: Vec<&str> = got["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, vec!["start"], "{got}");

    let (status, _) = b
        .call(
            M::POST,
            "/code/sessions/1700000000-nope00/message",
            &b.owner,
            json!({ "text": "hi" }),
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn sessions_and_their_changes_outlive_a_restart() {
    let config = tempfile::tempdir().unwrap();
    let p = project();
    let (id, owner_token) = {
        let (b, _state) = station_at(config.path()).await;
        let id = b
            .start(
                &b.owner,
                json!({ "folder": p.path(), "sandbox": false, "think": true }),
            )
            .await;
        let (status, _) = b
            .call(
                M::POST,
                &format!("/code/sessions/{id}/rename"),
                &b.owner,
                json!({ "title": "Kept" }),
            )
            .await;
        assert_eq!(status, 200);
        (id, b.owner.clone())
    };

    // A new agent on the same config folder; the paired owner's token still works.
    let state = box_at(config.path());
    let base = serve(&state).await;
    let client = reqwest::Client::new();
    let got: Value = client
        .get(format!("{base}/code/sessions/{id}"))
        .bearer_auth(&owner_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["session"]["title"], "Kept", "{got}");
    assert_eq!(got["session"]["sandbox"], false);
    assert_eq!(got["session"]["think"], true);
    assert_eq!(got["session"]["running"], false);
    assert_eq!(got["events"][0]["kind"], "start", "{got}");
}
