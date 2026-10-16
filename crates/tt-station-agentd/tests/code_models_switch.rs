// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Code's model list offers each serve profile of an installed community
//! package that fits this box, and choosing one is the package's own start
//! (`POST /community/serve` with that profile): what the list says to call,
//! called, ends in `tt-model serve … --profile <it>`. A profile the package
//! doesn't have, or one too big for the box, is refused before anything runs.
//!
//! The whole binary runs against a scratch `HOME` (set once, before any agent
//! starts): a fake `tt-model` that logs its arguments, a package in tt-model's
//! index, and its model's files in a scratch Hugging Face cache. `systemd-run`,
//! `systemctl`, `docker` and `gozer` are logging fakes earlier on `PATH`, and
//! the chip reset is `/bin/true`: nothing touches the chips or a container.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

const PKG: &str = "tt-community/coder-p150";

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fake_box() -> &'static Path {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let home = support::home_without_models().to_path_buf();
        let log = home.join("calls.log");
        let bin = home.join("fakebin");
        for tool in ["systemd-run", "docker", "gozer", "tt-smi"] {
            write_script(
                &bin.join(tool),
                &format!("echo \"{tool} $*\" >> '{}'\nexit 0\n", log.display()),
            );
        }
        write_script(
            &home.join(".local/bin/tt-model"),
            &format!("echo \"tt-model $*\" >> '{}'\nexit 0\n", log.display()),
        );
        // No unit is ever running.
        write_script(
            &bin.join("systemctl"),
            &format!(
                "echo \"systemctl $*\" >> '{}'\n[ \"$2\" = is-active ] && exit 3\nexit 0\n",
                log.display()
            ),
        );
        // The package: three profiles, one of them for 8 chips.
        let manifest = home.join("manifests/coder.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        let profile = |name: &str, hw: &str| json!({ "name": name, "hardware": hw, "args": [] });
        std::fs::write(
            &manifest,
            json!({
                "device_count": 4,
                "weights": { "repo_id": "org/Coder-Instruct" },
                "container": {
                    "kind": "vllm-plugin",
                    "serve": { "max_model_len": 65536, "capabilities": { "tool_parser": "hermes" } },
                    "serve_profiles": [profile("x2", "p150x2"), profile("x4", "p150x4"), profile("x8", "p150x8")],
                    "default_profile": "x2",
                },
            })
            .to_string(),
        )
        .unwrap();
        let index = home.join(".cache/tt-model/installed.json");
        std::fs::create_dir_all(index.parent().unwrap()).unwrap();
        std::fs::write(
            &index,
            json!({ PKG: { "manifest": manifest, "container": true, "profile": "x2",
                           "profiles": ["x2", "x4", "x8"] } })
            .to_string(),
        )
        .unwrap();
        // Its model, whole, in the Hugging Face cache: a chat model.
        let snap = home.join(".cache/huggingface/hub/models--org--Coder-Instruct/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        std::fs::write(
            snap.join("config.json"),
            r#"{"architectures":["Qwen3ForCausalLM"]}"#,
        )
        .unwrap();
        std::fs::write(snap.join("chat_template.jinja"), "{{ messages }}").unwrap();
        std::fs::write(snap.join("model.safetensors"), "weights").unwrap();

        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
        std::env::set_var("XDG_RUNTIME_DIR", &home);
        home
    })
}

fn calls(needle: &str) -> Vec<String> {
    std::fs::read_to_string(fake_box().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn choosing_a_profile_in_code_starts_the_package_with_it() {
    fake_box();
    let config = tempfile::tempdir().unwrap();
    // A closed serving port: nothing here may reach a real model server.
    let state = AppState::new("qb2-lab".into(), "4xBH".into(), Arc::new(DstackBackend))
        .with_config_dir(config.path().to_path_buf())
        .with_serving_config("127.0.0.1".into(), 9)
        .with_power_config(
            vec!["/bin/true".into()],
            vec!["/bin/true".into()],
            vec!["/bin/true".into()],
            vec!["/bin/true".into()],
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let token = support::pair(&base, &state).await;
    let client = reqwest::Client::new();
    let call = |method: reqwest::Method, path: String, body: Value| {
        let req = client
            .request(method, format!("{base}{path}"))
            .bearer_auth(&token)
            .json(&body);
        async move {
            let r = req.send().await.unwrap();
            (r.status().as_u16(), r.json::<Value>().await.unwrap())
        }
    };

    let (status, list) = call(reqwest::Method::GET, "/code/models".into(), Value::Null).await;
    assert_eq!(status, 200, "{list}");
    let models = list["models"].as_array().unwrap();
    let profiles: Vec<&str> = models
        .iter()
        .map(|m| m["profile"].as_str().unwrap())
        .collect();
    assert_eq!(profiles, ["x2", "x4"], "x8 needs 8 chips: {list}");
    let x4 = &models[1];
    assert_eq!(x4["source"], "community");
    assert_eq!(x4["group"], "coder-instruct");
    assert_eq!(x4["profile_label"], "4 chips · x4");
    assert_eq!(x4["context"], 65_536);
    assert_eq!(x4["tools"]["verified"], true);
    assert_eq!(x4["choice"], json!({ "action": "switch" }));
    assert_eq!(
        x4["serve"],
        json!({ "route": "/community/serve",
                "body": { "id": PKG, "profile": "x4", "stop_serving": true } })
    );

    // A profile it doesn't have, or one too big for the box: refused, nothing runs.
    for (profile, why) in [
        ("x16", "has no profile called x16"),
        ("x8", "doesn't fit this box"),
    ] {
        let (status, body) = call(
            reqwest::Method::POST,
            "/community/serve".into(),
            json!({ "id": PKG, "profile": profile, "stop_serving": true }),
        )
        .await;
        assert_eq!(status, 409, "{body}");
        assert!(body["error"].as_str().unwrap().contains(why), "{body}");
    }
    assert!(calls("tt-model serve").is_empty(), "{:?}", calls(""));

    // What the list says to call, called.
    let route = x4["serve"]["route"].as_str().unwrap().to_string();
    let (status, body) = call(reqwest::Method::POST, route, x4["serve"]["body"].clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["starting"], PKG);
    let mut launched = Vec::new();
    for _ in 0..100 {
        launched = calls("tt-model serve");
        if !launched.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(launched.len(), 1, "{:?}", calls(""));
    let argv: Vec<&str> = launched[0].split_whitespace().collect();
    let at = argv
        .iter()
        .position(|a| *a == "--profile")
        .expect("a profile");
    assert_eq!(argv[at + 1], "x4", "{}", launched[0]);
    assert!(
        argv.windows(2).any(|w| w == ["serve", PKG]),
        "{}",
        launched[0]
    );
}
