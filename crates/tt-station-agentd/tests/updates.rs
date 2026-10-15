// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Updating TT Station (`src/updates.rs`, `src/release_keys.rs`) without the
//! network: which version counts as newer (pre-releases included), the
//! downgrade floor, package names and signatures bound to their version, the
//! update channel from its file and the service's environment, and the
//! routes' refusals that come before any download -- who may update, a
//! broken channel file, and undo with nothing (or nothing on disk) to go
//! back to.
//!
//! `HOME` is pointed at a scratch folder for this whole test binary, so the
//! update history and unpacked versions it reads are the test's own. Nothing
//! here reaches GitHub: every request that would is refused first.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use base64::Engine as _;
use ring::signature::KeyPair as _;
use serde_json::{json, Value};
use tt_station_agentd::release_keys::{self, TrustedKey};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::updates::{self, below_floor, names_version, older, valid_version};

mod support;

/// Tests that read or change the process environment take this.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn home() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-updates-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("HOME", &dir);
        std::env::remove_var("TT_STATION_UPDATE_REPO");
        std::env::remove_var(updates::ALLOW_UNSIGNED_ENV);
        dir
    })
}

fn keypair(name: &str) -> (ring::signature::Ed25519KeyPair, TrustedKey, String) {
    let pkcs8 =
        ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let b64 = base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref());
    let key = TrustedKey::new(name, &b64, "channel").unwrap();
    (kp, key, b64)
}

fn sign(kp: &ring::signature::Ed25519KeyPair, file: &str, sha: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        kp.sign(release_keys::statement(file, sha).as_bytes())
            .as_ref(),
    )
}

// ---------------------------------------------------------------- versions

#[test]
fn a_pre_release_is_older_than_its_release() {
    // Semver: 1.0.0-rc.2 < 1.0.0, however many pre-release parts it has.
    assert!(older("1.0.0-rc.1", "1.0.0"));
    assert!(older("1.0.0-rc.2", "1.0.0"));
    assert!(!older("1.0.0", "1.0.0-rc.2"));
    assert!(older("1.0.0-rc.1", "1.0.0-rc.2"));
    assert!(!older("1.0.0-rc.2", "1.0.0-rc.1"));
    assert!(older("0.12.0-rc.3", "0.12.0"));
    // But still newer than the release before it.
    assert!(older("0.11.9", "0.12.0-rc.1"));
    assert!(!older("0.12.0-rc.1", "0.11.9"));
}

#[test]
fn versions_compare_by_number_not_text() {
    assert!(older("0.9.0", "0.10.0"));
    assert!(older("0.10.9", "0.11"));
    assert!(!older("0.11", "0.11.0"));
    assert!(!older("0.11.0", "0.11"));
    assert!(older("v1.2.3", "v1.2.10"));
    assert!(!older("2.0.0", "1.99.99"));
    // Build metadata isn't a version part.
    assert!(!older("1.0.0+build.9", "1.0.0"));
    assert!(!older("1.0.0", "1.0.0+build.9"));
}

#[test]
fn the_downgrade_floor_is_the_running_major_minor() {
    assert!(below_floor("0.12.4", "0.11.99"));
    assert!(!below_floor("0.12.4", "0.12.0"));
    assert!(!below_floor("v0.12.4", "0.12.0"));
    assert!(below_floor("1.0.0", "0.12.0"));
    // A pre-release of the running line is below it.
    assert!(below_floor("1.0.2", "1.0.0-rc.1"));
    assert!(!below_floor("0.12.0-rc.1", "0.12.0"));
}

#[test]
fn only_plain_tags_are_versions() {
    for ok in ["0.12.0", "1.0.0-rc.1", "1.0.0+build.5", "10.0"] {
        assert!(valid_version(ok), "{ok}");
    }
    for bad in [
        "",
        "v0.12.0",
        "latest",
        "0.12.0/../../x",
        "0.12.0;rm -rf",
        "0.12 0",
        "0.12.0\n",
        &format!("1.{}", "0".repeat(70)),
    ] {
        assert!(!valid_version(bad), "{bad:?}");
    }
}

// ---------------------------------------------------------------- packages and signatures

#[test]
fn a_package_must_be_named_for_its_release() {
    assert!(names_version("tt-station_0.12.0_amd64_noble.deb", "0.12.0"));
    assert!(names_version(
        "tt-station_1.0.0~rc.1_amd64_noble.deb",
        "1.0.0-rc.1"
    ));
    // An older package re-published under a newer tag.
    assert!(!names_version(
        "tt-station_0.11.0_amd64_noble.deb",
        "0.12.0"
    ));
    assert!(!names_version(
        "tt-station_0.12.0.1_amd64_noble.deb",
        "0.12.0"
    ));
    assert!(!names_version("other_0.12.0_amd64_noble.deb", "0.12.0"));
    assert!(!names_version("tt-station-0.12.0.deb", "0.12.0"));
}

#[test]
fn a_signature_covers_the_file_name_and_its_hash() {
    let (kp, key, _) = keypair("release-2026");
    let keys = vec![key];
    let sha = "ab".repeat(32);
    let file = "tt-station_0.11.0_amd64_noble.deb";
    let sig = sign(&kp, file, &sha);
    assert_eq!(
        release_keys::verify(&keys, file, &sha, &sig).unwrap().name,
        "release-2026"
    );
    // Upper-case hex is the same hash.
    assert!(release_keys::verify(&keys, file, &sha.to_uppercase(), &sig).is_ok());
    // Surrounding whitespace in the .sig file is fine.
    assert!(release_keys::verify(&keys, file, &sha, &format!("\n{sig}  \n")).is_ok());
    // The same signature under a newer name, or for other bytes, isn't.
    let renamed = release_keys::verify(&keys, "tt-station_0.12.0_amd64_noble.deb", &sha, &sig);
    assert!(renamed
        .unwrap_err()
        .contains("isn't signed by a key this box trusts"));
    assert!(release_keys::verify(&keys, file, &"ac".repeat(32), &sig).is_err());
    assert_eq!(
        release_keys::statement(file, &sha.to_uppercase()),
        format!("tt-station-release-v1\nfile={file}\nsha256={sha}\n")
    );
}

#[test]
fn a_signature_that_isnt_one_or_with_no_keys_is_refused() {
    let (kp, key, _) = keypair("k");
    let sha = "cd".repeat(32);
    let file = "tt-station_0.12.0_amd64_noble.deb";
    for junk in ["", "not base64!", "AAAA", &"A".repeat(200)] {
        let e = release_keys::verify(std::slice::from_ref(&key), file, &sha, junk).unwrap_err();
        assert!(e.contains("isn't an Ed25519 signature"), "{junk:?}: {e}");
    }
    let e = release_keys::verify(&[], file, &sha, &sign(&kp, file, &sha)).unwrap_err();
    assert!(e.contains("trusts no release-signing keys"), "{e}");
}

#[test]
fn trusted_keys_are_parsed_strictly() {
    let (_, _, b64) = keypair("x");
    let keys = release_keys::parse_keys(
        &format!("# release keys\n\nmain-2026 {b64}\n  backup_1   {b64}  \n"),
        "built in",
    )
    .unwrap();
    assert_eq!(
        keys.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
        ["main-2026", "backup_1"]
    );
    assert_eq!(keys[0].fingerprint.len(), 16);
    assert_eq!(keys[0].fingerprint, keys[1].fingerprint);
    assert_eq!(keys[0].origin, "built in");
    for bad in [
        format!("{b64}"),
        format!("a b {b64}"),
        format!("bad/name {b64}"),
        "k AAAA".to_string(),
        "k not-base64".to_string(),
    ] {
        assert!(release_keys::parse_keys(&bad, "x").is_err(), "{bad}");
    }
    // The compiled-in keys parse.
    let _ = release_keys::builtin();
}

// ---------------------------------------------------------------- the channel

#[test]
fn the_channel_comes_from_its_file_then_the_service_environment() {
    let _env = ENV.blocking_lock();
    home();
    let dir = tempfile::tempdir().unwrap();
    let c = updates::channel(dir.path()).unwrap();
    assert_eq!(
        (c.repo.as_str(), c.repo_from.as_str()),
        (updates::DEFAULT_REPO, "default")
    );
    assert!(!c.unsigned_allowed);
    let builtin = c.keys.len();

    let (_, _, b64) = keypair("x");
    std::fs::write(
        dir.path().join(updates::CHANNEL_FILE),
        format!("repo = \"acme/tt-station\"\n[[keys]]\nname = \"acme-2026\"\nkey = \"{b64}\"\n"),
    )
    .unwrap();
    let c = updates::channel(dir.path()).unwrap();
    assert_eq!(
        (c.repo.as_str(), c.repo_from.as_str()),
        ("acme/tt-station", "channel file")
    );
    assert_eq!(c.keys.len(), builtin + 1);
    assert_eq!(
        (
            c.keys[builtin].name.as_str(),
            c.keys[builtin].origin.as_str()
        ),
        ("acme-2026", "channel")
    );

    std::env::set_var("TT_STATION_UPDATE_REPO", "lab/tt-station-nightly");
    let c = updates::channel(dir.path()).unwrap();
    assert_eq!(
        (c.repo.as_str(), c.repo_from.as_str()),
        ("lab/tt-station-nightly", "environment")
    );
    std::env::set_var("TT_STATION_UPDATE_REPO", "https://evil.example/x");
    assert!(updates::channel(dir.path())
        .unwrap_err()
        .contains("isn't a GitHub owner/name"));
    // Empty is unset.
    std::env::set_var("TT_STATION_UPDATE_REPO", "");
    assert_eq!(
        updates::channel(dir.path()).unwrap().repo,
        "acme/tt-station"
    );
    std::env::remove_var("TT_STATION_UPDATE_REPO");

    for (value, allowed) in [
        ("1", true),
        ("true", true),
        ("yes", true),
        ("0", false),
        ("TRUE", false),
    ] {
        std::env::set_var(updates::ALLOW_UNSIGNED_ENV, value);
        assert_eq!(
            updates::channel(dir.path()).unwrap().unsigned_allowed,
            allowed,
            "{value}"
        );
    }
    std::env::remove_var(updates::ALLOW_UNSIGNED_ENV);
}

// ---------------------------------------------------------------- the routes

struct Box_ {
    base: String,
    state: AppState,
    owner: String,
    config: PathBuf,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(tag: &str) -> Self {
        home();
        let config =
            std::env::temp_dir().join(format!("tt-updates-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        // A channel file that doesn't parse: nothing goes to GitHub.
        std::fs::write(config.join(updates::CHANNEL_FILE), "repos = \"typo/x\"\n").unwrap();
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_config_dir(config.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let owner = support::pair(&base, &state).await;
        Box_ {
            base,
            state,
            owner,
            config,
            c: reqwest::Client::new(),
        }
    }

    async fn send(&self, token: &str, method: &str, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .c
            .request(method.parse().unwrap(), format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn only_an_owner_updates_or_undoes() {
    let b = Box_::new("roles").await;
    let member = support::pair(&b.base, &b.state).await;
    for path in ["/updates/install", "/updates/undo"] {
        let (status, _) = b
            .send(&member, "POST", path, json!({ "allow_downgrade": true }))
            .await;
        assert_eq!(status, 403, "{path}");
        let r =
            b.c.post(format!("{}{path}", b.base))
                .json(&json!({}))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 401, "{path}");
    }
    assert!(!b
        .state
        .audit()
        .page(0, 50)
        .0
        .iter()
        .any(|e| e.action.starts_with("update_")));
}

#[tokio::test]
async fn a_broken_channel_file_is_reported_and_stops_an_install() {
    let _env = ENV.lock().await;
    let b = Box_::new("channel").await;
    let (status, v) = b.send(&b.owner, "GET", "/updates", json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["version"], updates::VERSION);
    assert!(v["channel"].is_null(), "{v}");
    assert!(
        v["problem"]
            .as_str()
            .unwrap()
            .contains("update channel file"),
        "{v}"
    );
    assert_eq!(v["releases"], json!([]));
    assert!(v["update"].is_null());
    assert!(v["working"].is_null());

    let (status, v) = b
        .send(&b.owner, "POST", "/updates/install", json!({}))
        .await;
    assert_eq!(status, 409, "{v}");
    assert!(v["error"].as_str().unwrap().contains("isn't valid"), "{v}");
    // Not quietly back to the default channel.
    std::fs::remove_file(b.config.join(updates::CHANNEL_FILE)).unwrap();
    std::fs::write(
        b.config.join(updates::CHANNEL_FILE),
        "repo = \"not a repo\"\n",
    )
    .unwrap();
    let (status, v) = b
        .send(
            &b.owner,
            "POST",
            "/updates/install",
            json!({ "version": "0.1.0" }),
        )
        .await;
    assert_eq!(status, 409, "{v}");
    assert!(v["error"]
        .as_str()
        .unwrap()
        .contains("isn't a GitHub owner/name"));
}

#[tokio::test]
async fn undo_needs_a_finished_switch_and_the_version_before_on_disk() {
    let _env = ENV.lock().await;
    let b = Box_::new("undo").await;
    let history = home().join(".local/share/tt-station/update-history.json");
    let _ = std::fs::remove_file(&history);
    let (status, v) = b.send(&b.owner, "POST", "/updates/undo", json!({})).await;
    assert_eq!(
        (status, &v["error"]),
        (409, &json!("There's no update to undo."))
    );

    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    let change = |status: &str| {
        json!({ "at": 1, "from": "0.10.0", "to": "0.11.0", "from_label": "0.10.0",
            "to_label": "0.11.0", "status": status })
    };
    // A switch that rolled back isn't undone.
    std::fs::write(&history, json!([change("rolled_back")]).to_string()).unwrap();
    let (status, _) = b.send(&b.owner, "POST", "/updates/undo", json!({})).await;
    assert_eq!(status, 409);
    // The version before was pruned from disk.
    std::fs::write(&history, json!([change("done")]).to_string()).unwrap();
    let (status, v) = b.send(&b.owner, "POST", "/updates/undo", json!({})).await;
    assert_eq!(status, 410, "{v}");
    assert_eq!(v["error"], "0.10.0 isn't on the box any more.");

    // GET /updates offers the undo all the same, and shows the history.
    let (_, v) = b.send(&b.owner, "GET", "/updates", json!({})).await;
    assert_eq!(v["undo"], json!({ "to": "0.10.0", "label": "0.10.0" }));
    assert_eq!(v["history"][0]["status"], "done");
    assert_eq!(v["on_disk"], json!([]));
    let _ = std::fs::remove_file(&history);
}
