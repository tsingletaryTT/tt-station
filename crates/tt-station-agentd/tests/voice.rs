// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The box's voice (`src/voice.rs`) through the router, with no audio and no
//! voice services: the saved settings (checked, clamped, kept private, a
//! corrupt file set aside), the requests refused before anything is spoken
//! or heard, a setup whose download fails (with `docker` replaced by a stub
//! on `PATH`), and removing it again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::voice;

mod support;

/// A `docker` with no containers whose pulls fail; it logs each call.
const FAKE_DOCKER: &str = r#"#!/bin/sh
D=$(dirname "$0")
echo "$*" >> "$D/log"
case "$1" in
  inspect) echo "Error: No such object" >&2; exit 1 ;;
  pull) echo "pulling" >&2; echo "Error response from daemon: no space left on device" >&2; exit 1 ;;
esac
exit 0
"#;

fn docker_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-voice-docker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        std::fs::write(&bin, FAKE_DOCKER).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", dir.display()));
        dir
    })
}

struct Box_ {
    base: String,
    state: AppState,
    token: String,
    config: PathBuf,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(tag: &str) -> Self {
        docker_dir();
        let config =
            std::env::temp_dir().join(format!("tt-voice-cfg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
        let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
            .with_telemetry_config("/nonexistent/tt-smi".to_string(), 1000)
            .with_config_dir(config.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app(state.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let token = support::pair(&base, &state).await;
        Box_ {
            base,
            state,
            token,
            config,
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

#[tokio::test]
async fn every_voice_route_needs_a_paired_device() {
    let b = Box_::new("auth").await;
    for (method, path) in [
        ("GET", "/voice"),
        ("PUT", "/voice"),
        ("DELETE", "/voice"),
        ("POST", "/voice/setup"),
        ("POST", "/voice/speak"),
        ("POST", "/voice/listen"),
        ("GET", "/voice/hear"),
        ("POST", "/voice/hear"),
        ("DELETE", "/voice/hear"),
        ("POST", "/voice/hear/finish"),
        ("POST", "/voice/say"),
    ] {
        let r =
            b.c.request(method.parse().unwrap(), format!("{}{path}", b.base))
                .json(&json!({ "text": "hi" }))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 401, "{method} {path}");
    }
}

#[tokio::test]
async fn settings_start_at_the_default_and_are_saved_privately() {
    let b = Box_::new("settings").await;
    assert_eq!(
        voice::settings(&b.state),
        voice::Settings {
            voice: voice::DEFAULT_VOICE.into(),
            speed: 1.0
        }
    );
    let (status, s) = b
        .send(
            "PUT",
            "/voice",
            Some(json!({ "voice": "bm_george", "speed": 1.25 })),
        )
        .await;
    assert_eq!(status, 200, "{s}");
    assert_eq!(s, json!({ "voice": "bm_george", "speed": 1.25 }));
    let path = b.config.join("voice.json");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o077,
        0,
        "voice.json is private"
    );
    assert_eq!(voice::settings(&b.state).voice, "bm_george");

    // A partial update keeps the rest.
    let (_, s) = b
        .send("PUT", "/voice", Some(json!({ "speed": 0.75 })))
        .await;
    assert_eq!(s, json!({ "voice": "bm_george", "speed": 0.75 }));
    let (_, s) = b
        .send("PUT", "/voice", Some(json!({ "voice": "af_bella+af_sky" })))
        .await;
    assert_eq!(s, json!({ "voice": "af_bella+af_sky", "speed": 0.75 }));
}

#[tokio::test]
async fn speed_is_clamped_and_a_bad_voice_is_refused_without_saving() {
    let b = Box_::new("clamp").await;
    let (_, s) = b.send("PUT", "/voice", Some(json!({ "speed": 9.0 }))).await;
    assert_eq!(s["speed"], 2.0);
    let (_, s) = b.send("PUT", "/voice", Some(json!({ "speed": 0.1 }))).await;
    assert_eq!(s["speed"], 0.5);
    let (_, s) = b.send("PUT", "/voice", Some(json!({ "speed": -3 }))).await;
    assert_eq!(s["speed"], 0.5);

    for bad in ["", "../../etc/passwd", "af heart", "a\"b", &"x".repeat(65)] {
        let (status, body) = b
            .send("PUT", "/voice", Some(json!({ "voice": bad, "speed": 1.5 })))
            .await;
        assert_eq!(status, 400, "{bad:?}: {body}");
        assert_eq!(body["error"], "That isn't a voice.");
    }
    // Neither the bad voice nor the speed that came with it was saved.
    assert_eq!(
        voice::settings(&b.state),
        voice::Settings {
            voice: voice::DEFAULT_VOICE.into(),
            speed: 0.5
        }
    );
}

#[tokio::test]
async fn a_corrupt_settings_file_falls_back_to_the_defaults() {
    let b = Box_::new("corrupt").await;
    std::fs::write(b.config.join("voice.json"), "{not json").unwrap();
    assert_eq!(voice::settings(&b.state), voice::Settings::default());
    // Set aside, so a later save isn't blocked by it.
    let (status, s) = b
        .send("PUT", "/voice", Some(json!({ "voice": "am_puck" })))
        .await;
    assert_eq!(status, 200, "{s}");
    assert_eq!(s, json!({ "voice": "am_puck", "speed": 1.0 }));
}

#[tokio::test]
async fn nothing_is_spoken_or_heard_for_an_empty_or_oversized_request() {
    let b = Box_::new("refuse").await;
    let (status, body) = b
        .send("POST", "/voice/speak", Some(json!({ "text": "   \n " })))
        .await;
    assert_eq!((status, &body["error"]), (400, &json!("Nothing to say.")));
    let (status, body) = b
        .send(
            "POST",
            "/voice/speak",
            Some(json!({ "text": "a".repeat(4001) })),
        )
        .await;
    assert_eq!(status, 413, "{body}");

    let r =
        b.c.post(format!("{}/voice/listen", b.base))
            .bearer_auth(&b.token)
            .header("content-type", "audio/webm")
            .body(Vec::<u8>::new())
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 400);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"], "The recording was empty.");

    // Saying nothing out loud is a no-op, not an error.
    let (status, body) = b
        .send("POST", "/voice/say", Some(json!({ "text": "  " })))
        .await;
    assert_eq!((status, body), (200, json!({ "said": false })));
}

#[tokio::test]
async fn the_hearing_state_is_reported_and_stopping_is_always_accepted() {
    let b = Box_::new("hear").await;
    let (status, h) = b.send("GET", "/voice/hear", None).await;
    assert_eq!(status, 200);
    assert_eq!(h["hearing"], false);
    assert_eq!(h["saying"], false);
    assert!(h["level_db"].is_number());
    assert_eq!(
        b.send("POST", "/voice/hear/finish", None).await,
        (200, json!({ "finishing": true }))
    );
    assert_eq!(
        b.send("DELETE", "/voice/hear", None).await,
        (200, json!({ "stopped": true }))
    );
}

#[tokio::test]
async fn a_setup_whose_download_fails_says_why_until_the_voice_is_removed() {
    let b = Box_::new("setup").await;
    let (status, body) = b.send("POST", "/voice/setup", None).await;
    assert_eq!((status, body), (202, json!({ "state": "installing" })));

    let mut v = Value::Null;
    for _ in 0..100 {
        v = b.send("GET", "/voice", None).await.1;
        if v["state"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(v["state"], "failed", "{v}");
    assert_eq!(
        v["error"],
        "Error response from daemon: no space left on device"
    );
    assert_eq!(v["voice"], voice::DEFAULT_VOICE);
    assert_eq!(v["listen_model"], voice::LISTEN_MODEL);
    let log = std::fs::read_to_string(docker_dir().join("log")).unwrap();
    assert!(
        log.lines()
            .any(|l| l == format!("pull {}", voice::SPEAK.image)),
        "{log}"
    );
    assert!(!log.lines().any(|l| l.starts_with("run ")), "{log}");

    // A failed setup can be retried (it isn't stuck "installing").
    let (status, _) = b.send("POST", "/voice/setup", None).await;
    assert_eq!(status, 202);
    for _ in 0..100 {
        if b.send("GET", "/voice", None).await.1["state"] == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let (status, body) = b.send("DELETE", "/voice", None).await;
    assert_eq!((status, body), (200, json!({ "state": "not_installed" })));
    let log = std::fs::read_to_string(docker_dir().join("log")).unwrap();
    for c in [voice::SPEAK.container, voice::LISTEN.container] {
        assert!(log.lines().any(|l| l == format!("rm -f {c}")), "{log}");
    }
    let v = b.send("GET", "/voice", None).await.1;
    assert_ne!(v["state"], "failed", "{v}");
    assert!(v["error"].is_null());
}

#[test]
fn voices_are_described_for_people() {
    let v = voice::describe_voice("ef_dora");
    assert_eq!(v["name"], "Dora");
    assert_eq!(v["accent"], "Spanish");
    assert_eq!(v["english"], false);
    assert_eq!(v["favourite"], false);
    let v = voice::describe_voice("am_santa_claus");
    assert_eq!(v["name"], "Santa Claus");
    assert_eq!(v["english"], true);
    // Not a Kokoro id: named as given, nothing guessed.
    let v = voice::describe_voice("custom");
    assert_eq!(v["name"], "Custom");
    assert_eq!(v["accent"], "");
    assert_eq!(v["gender"], "");
}
