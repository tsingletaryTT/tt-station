// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! A Code session's record and its project, over HTTP: reading the session's
//! events after an offset (bounds, bad offsets, a file changed under the
//! reader), and the Changes routes (`/changes`, `/commit`, `/discard`): what
//! they show, what they commit, what they delete and what they never touch,
//! and git refused in a sandboxed project until its git directory is out of
//! the sandbox's reach.
//!
//! Nothing here starts Claude Code: no model is serving and no message is
//! sent. What the agent did is written into the session's event log the way
//! it records it (`tool` / `tool_result` events). Git runs for real, in
//! throwaway repositories in the home folder.

use std::path::{Path, PathBuf};
use std::process::Command;
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

    async fn start(&self, token: &str, body: Value) -> String {
        let (status, said) = self.call(M::POST, "/code/sessions", token, body).await;
        assert_eq!(status, 200, "{said}");
        said["session"]["id"].as_str().unwrap().to_string()
    }

    fn session_dir(&self, id: &str) -> PathBuf {
        self.config.path().join("code").join(id)
    }

    /// The seq of each event after `after`, as `GET /code/sessions/{id}` gives them.
    async fn seqs_after(&self, id: &str, after: &str) -> (u16, Value) {
        self.call(
            M::GET,
            &format!("/code/sessions/{id}?after={after}"),
            &self.owner,
            json!({}),
        )
        .await
    }

    /// Appends what the agent did, as the session records it.
    fn record(&self, id: &str, events: &[Value]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.session_dir(id).join("events.jsonl"))
            .unwrap();
        for e in events {
            writeln!(f, "{e}").unwrap();
        }
    }
}

fn seqs(body: &Value) -> Vec<u64> {
    body["events"]
        .as_array()
        .unwrap_or_else(|| panic!("{body}"))
        .iter()
        .map(|e| e["seq"].as_u64().unwrap())
        .collect()
}

/// A folder in the home folder (where Code takes projects from), removed afterwards.
fn project() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME");
    tempfile::Builder::new()
        .prefix("tt-station-ws-test-")
        .tempdir_in(home)
        .expect("a folder in the home folder")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@localhost",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A repository with `a.txt` committed.
fn repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(dir, &["commit", "-q", "-m", "first commit"]);
}

fn real(p: &Path) -> String {
    std::fs::canonicalize(p).unwrap().display().to_string()
}

/// The agent wrote `file` (a path as it names them) and the write went through
/// (or didn't, with `failed`).
fn wrote(tool_id: &str, file: &str, failed: bool) -> [Value; 2] {
    [
        json!({ "kind": "tool", "id": tool_id, "name": "Write", "input": { "file_path": file } }),
        json!({ "kind": "tool_result", "id": tool_id, "output": "", "is_error": failed }),
    ]
}

#[tokio::test]
async fn events_are_read_after_an_offset_within_bounds() {
    let b = harness().await;
    let p = project();
    repo(p.path());
    let id = b
        .start(&b.owner, json!({ "folder": p.path(), "sandbox": false }))
        .await;
    // Each discard leaves one event: 1 is the start, then 2, 3, 4.
    for _ in 0..3 {
        let (status, said) = b
            .call(
                M::POST,
                &format!("/code/sessions/{id}/discard"),
                &b.owner,
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{said}");
    }

    for (after, want) in [
        ("0", vec![1, 2, 3, 4]),
        ("2", vec![3, 4]),
        ("4", vec![]),
        ("5", vec![]),
        ("999999", vec![]),
        ("18446744073709551615", vec![]),
    ] {
        let (status, body) = b.seqs_after(&id, after).await;
        assert_eq!(status, 200, "after={after}: {body}");
        assert_eq!(seqs(&body), want, "after={after}");
    }
    // No offset is from the start.
    let (_, body) = b
        .call(M::GET, &format!("/code/sessions/{id}"), &b.owner, json!({}))
        .await;
    assert_eq!(seqs(&body), vec![1, 2, 3, 4]);
    assert_eq!(body["events"][0]["kind"], "start");
    assert_eq!(body["pending"], json!([]));
    // An offset that isn't a count is refused, not read as 0.
    for bad in ["-1", "abc", "1.5", "18446744073709551616", ""] {
        let (status, body) = b.seqs_after(&id, bad).await;
        assert_eq!(status, 400, "after={bad}: {body}");
    }

    // Carrying on from where a read ended sees only what came since, and an
    // earlier offset still sees everything after it.
    let (_, _) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/discard"),
            &b.owner,
            json!({}),
        )
        .await;
    let (_, body) = b.seqs_after(&id, "4").await;
    assert_eq!(seqs(&body), vec![5]);
    let (_, body) = b.seqs_after(&id, "2").await;
    assert_eq!(seqs(&body), vec![3, 4, 5]);
    let (_, body) = b.seqs_after(&id, "5").await;
    assert_eq!(seqs(&body), Vec::<u64>::new());
}

#[tokio::test]
async fn a_record_changed_under_the_reader_is_read_afresh() {
    let b = harness().await;
    let p = project();
    let id = b.start(&b.owner, json!({ "folder": p.path() })).await;
    let file = b.session_dir(&id).join("events.jsonl");
    // Ten events, read to the end so the reader remembers where it stopped.
    let mut lines: Vec<String> = (1..=10)
        .map(|n| json!({ "seq": n, "kind": "notice", "text": format!("n{n}") }).to_string())
        .collect();
    std::fs::write(&file, lines.join("\n") + "\n").unwrap();
    let (_, body) = b.seqs_after(&id, "0").await;
    assert_eq!(seqs(&body), (1..=10).collect::<Vec<_>>());
    let (_, body) = b.seqs_after(&id, "10").await;
    assert_eq!(seqs(&body), Vec::<u64>::new());

    // Shorter now (rewritten): nothing past 10, and no error.
    std::fs::write(&file, lines[..3].join("\n") + "\n").unwrap();
    let (status, body) = b.seqs_after(&id, "10").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(seqs(&body), Vec::<u64>::new());
    let (_, body) = b.seqs_after(&id, "1").await;
    assert_eq!(seqs(&body), vec![2, 3]);

    // Rewritten with different lines where the remembered place was: the
    // reader notices and reads from the top rather than trusting the offset.
    lines[2] = json!({ "seq": 3, "kind": "notice", "text": "a longer line than it was before" })
        .to_string();
    lines.truncate(5);
    std::fs::write(&file, lines.join("\n") + "\n").unwrap();
    // (The reader stopped after line 3 before; that place is now mid-line.)
    let (_, body) = b.seqs_after(&id, "3").await;
    assert_eq!(seqs(&body), vec![4, 5]);
    assert_eq!(body["events"][0]["text"], "n4");
    let (_, body) = b.seqs_after(&id, "2").await;
    assert_eq!(seqs(&body), vec![3, 4, 5]);
    assert_eq!(
        body["events"][0]["text"],
        "a longer line than it was before"
    );

    // A line being written (no newline yet) waits for the next read; a line
    // that isn't JSON is skipped.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    write!(f, "not json\n{{\"seq\": 7, \"kind\": \"notice\"").unwrap();
    drop(f);
    let (status, body) = b.seqs_after(&id, "4").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(seqs(&body), vec![5]);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    writeln!(f, "}}").unwrap();
    drop(f);
    let (_, body) = b.seqs_after(&id, "5").await;
    assert_eq!(seqs(&body), vec![7]);
}

#[tokio::test]
async fn changes_show_the_projects_diff_but_never_what_a_link_points_at() {
    let b = harness().await;
    let p = project();
    repo(p.path());
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("id_ed25519");
    std::fs::write(&secret, "SECRET-KEY-MATERIAL\n").unwrap();
    std::fs::write(p.path().join("a.txt"), "one\ntwo\n").unwrap();
    std::fs::write(p.path().join("new.txt"), "hello there\n").unwrap();
    // What a sandbox could leave: a link to a file it can't see itself.
    std::os::unix::fs::symlink(&secret, p.path().join("leak")).unwrap();
    let id = b
        .start(&b.owner, json!({ "folder": p.path(), "sandbox": false }))
        .await;

    let (status, ch) = b
        .call(
            M::GET,
            &format!("/code/sessions/{id}/changes"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{ch}");
    assert_eq!(ch["repo"], true);
    let files: Vec<&str> = ch["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(files.contains(&" M a.txt"), "{files:?}");
    assert!(files.contains(&"?? new.txt"), "{files:?}");
    assert!(files.contains(&"?? leak"), "{files:?}");
    let diff = ch["diff"].as_str().unwrap();
    assert!(diff.contains("+two"), "{diff}");
    assert!(diff.contains("+hello there"), "{diff}");
    assert!(!diff.contains("SECRET"), "a link's target was read: {diff}");
    assert!(ch["log"].as_str().unwrap().contains("first commit"));
    // The count in the session matches.
    let (_, got) = b
        .call(M::GET, &format!("/code/sessions/{id}"), &b.owner, json!({}))
        .await;
    assert_eq!(got["session"]["changes"], 3, "{got}");
}

#[tokio::test]
async fn a_folder_that_isnt_a_repository_shows_what_the_agent_edited() {
    let b = harness().await;
    let p = project();
    std::fs::write(p.path().join("made.txt"), "from the agent\n").unwrap();
    std::fs::write(p.path().join("failed.txt"), "never written\n").unwrap();
    std::fs::write(p.path().join("mine.txt"), "the person's own\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("x.txt"), "SECRET\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), p.path().join("out")).unwrap();
    let id = b
        .start(&b.owner, json!({ "folder": p.path(), "sandbox": false }))
        .await;
    let root = real(p.path());
    let mut events = vec![];
    events.extend(wrote("t1", &format!("{root}/made.txt"), false));
    events.extend(wrote("t2", &format!("{root}/failed.txt"), true));
    // Through a link out of the project: listed, but its contents aren't read.
    events.extend(wrote("t3", &format!("{root}/out/x.txt"), false));
    // Not in the project at all: ignored.
    events.extend(wrote("t4", &format!("{root}/../elsewhere.txt"), false));
    events.extend(wrote("t5", "/etc/passwd", false));
    b.record(&id, &events);

    let (status, ch) = b
        .call(
            M::GET,
            &format!("/code/sessions/{id}/changes"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{ch}");
    assert_eq!(ch["repo"], false);
    assert_eq!(ch["edited"], true);
    assert_eq!(ch["files"], json!([" M made.txt", " M out/x.txt"]), "{ch}");
    let diff = ch["diff"].as_str().unwrap();
    assert!(diff.contains("+from the agent"), "{diff}");
    assert!(!diff.contains("SECRET"), "{diff}");
    assert!(!diff.contains("never written"), "{diff}");
    assert!(!diff.contains("the person's own"), "{diff}");
    assert!(!diff.contains("root:"), "{diff}");
}

#[tokio::test]
async fn commit_takes_this_folder_only_and_needs_something_to_commit() {
    let b = harness().await;
    let p = project();
    repo(p.path());
    for d in ["app", "other"] {
        std::fs::create_dir_all(p.path().join(d)).unwrap();
        std::fs::write(p.path().join(d).join("f.txt"), "v1\n").unwrap();
    }
    git(p.path(), &["add", "-A"]);
    git(p.path(), &["commit", "-q", "-m", "two folders"]);
    std::fs::write(p.path().join("app/f.txt"), "v2\n").unwrap();
    std::fs::write(p.path().join("other/f.txt"), "v2\n").unwrap();
    let id = b
        .start(
            &b.owner,
            json!({ "folder": p.path().join("app"), "sandbox": false }),
        )
        .await;
    let commit = |message: &str| {
        let path = format!("/code/sessions/{id}/commit");
        let body = json!({ "message": message });
        let b = &b;
        async move { b.call(M::POST, &path, &b.owner, body).await }
    };

    // No message: the default one.
    let (status, said) = commit("   ").await;
    assert_eq!(status, 200, "{said}");
    assert!(
        said["commit"]
            .as_str()
            .unwrap()
            .ends_with("Changes made in TT Station on the QuietBox"),
        "{said}"
    );
    // Only the session's folder went in; the rest of the repository is as it was.
    let status = git(p.path(), &["status", "--porcelain"]);
    assert_eq!(status.trim(), "M other/f.txt", "{status}");
    let author = git(p.path(), &["log", "-1", "--format=%an <%ae>"]);
    assert_eq!(author.trim(), "TT Station <tt-station@localhost>");
    let (_, got) = b
        .call(M::GET, &format!("/code/sessions/{id}"), &b.owner, json!({}))
        .await;
    let last = got["events"].as_array().unwrap().last().unwrap().clone();
    assert!(
        last["text"].as_str().unwrap().starts_with("Committed: "),
        "{last}"
    );

    // Nothing left here to commit.
    let (status, said) = commit("again").await;
    assert_eq!(status, 409, "{said}");
    assert_eq!(
        git(p.path(), &["rev-list", "--count", "HEAD"]).trim(),
        "3",
        "no extra commit"
    );
    // A message is required in the body.
    let (status, _) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/commit"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 422);
}

#[tokio::test]
async fn discard_restores_tracked_files_and_deletes_only_what_the_agent_made() {
    let b = harness().await;
    let p = project();
    repo(p.path());
    let keep_dir = tempfile::tempdir().unwrap();
    let keep = keep_dir.path().join("important.txt");
    std::fs::write(&keep, "keep me\n").unwrap();
    std::fs::write(p.path().join("a.txt"), "changed\n").unwrap();
    std::fs::write(p.path().join("new.txt"), "agent\n").unwrap();
    std::fs::write(p.path().join("tried.txt"), "agent, but the write failed\n").unwrap();
    std::fs::write(p.path().join("mine.txt"), "the person's own\n").unwrap();
    std::os::unix::fs::symlink(&keep, p.path().join("escape")).unwrap();
    let id = b
        .start(&b.owner, json!({ "folder": p.path(), "sandbox": false }))
        .await;
    let root = real(p.path());
    let mut events = vec![];
    events.extend(wrote("t1", &format!("{root}/new.txt"), false));
    events.extend(wrote("t2", &format!("{root}/tried.txt"), true));
    events.extend(wrote("t3", &format!("{root}/escape"), false));
    b.record(&id, &events);

    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/discard"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["removed"], 2, "{said}");
    assert_eq!(
        std::fs::read_to_string(p.path().join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!p.path().join("new.txt").exists());
    // The link went, not what it pointed at.
    assert!(std::fs::symlink_metadata(p.path().join("escape")).is_err());
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "keep me\n");
    // Files the agent didn't make stay.
    assert!(p.path().join("mine.txt").is_file());
    assert!(p.path().join("tried.txt").is_file());

    // Again: nothing more to do.
    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/discard"),
            &b.owner,
            json!({}),
        )
        .await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(said["removed"], 0);
    assert!(p.path().join("mine.txt").is_file());
}

#[tokio::test]
async fn git_in_a_sandboxed_project_waits_until_its_git_dir_is_out_of_reach() {
    let b = harness().await;
    let p = project();
    repo(p.path());
    std::fs::write(p.path().join("a.txt"), "changed\n").unwrap();
    let id = b.start(&b.member, json!({ "folder": p.path() })).await;
    let commit = || {
        let path = format!("/code/sessions/{id}/commit");
        let b = &b;
        async move {
            b.call(M::POST, &path, &b.member, json!({ "message": "m" }))
                .await
        }
    };

    // Never launched, so its .git wasn't mounted read-only: the sandbox could
    // have written its settings, and git isn't run there.
    let (status, said) = commit().await;
    assert_eq!(status, 409, "{said}");
    assert!(
        said["error"].as_str().unwrap().contains("git not run here"),
        "{said}"
    );
    let (status, said) = b
        .call(
            M::POST,
            &format!("/code/sessions/{id}/discard"),
            &b.member,
            json!({}),
        )
        .await;
    assert_eq!(status, 409, "{said}");
    assert_eq!(
        std::fs::read_to_string(p.path().join("a.txt")).unwrap(),
        "changed\n",
        "nothing was put back"
    );
    let (_, ch) = b
        .call(
            M::GET,
            &format!("/code/sessions/{id}/changes"),
            &b.member,
            json!({}),
        )
        .await;
    assert_eq!(ch["repo"], false, "{ch}");

    // As a launch leaves it: the git directory went in read-only.
    std::fs::write(
        b.session_dir(&id).join("git-ro.json"),
        json!([real(&p.path().join(".git"))]).to_string(),
    )
    .unwrap();
    let (status, said) = commit().await;
    assert_eq!(status, 200, "{said}");
    assert_eq!(git(p.path(), &["status", "--porcelain"]).trim(), "");

    // A filter driver in its config (planted before it was read-only): refused.
    git(p.path(), &["config", "filter.x.clean", "false"]);
    std::fs::write(p.path().join("a.txt"), "again\n").unwrap();
    let (status, said) = commit().await;
    assert_eq!(status, 409, "{said}");
    assert!(
        said["error"].as_str().unwrap().contains("name a program"),
        "{said}"
    );
}
