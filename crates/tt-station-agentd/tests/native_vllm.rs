// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Native vLLM (`src/native_vllm.rs`): turning a bundle's launch into the
//! command run here, and the routes up to the point where anything would
//! touch the chips -- which bundles are listed (and why one isn't ready),
//! that only listed bundles are served, and the refusals before a setup.
//!
//! `HOME`, `TT_METAL_HOME`, `TT_KERNEL_BUNDLES_DIR` and `HF_HOME` point at a
//! scratch folder for this whole test binary: there's no tt-metal Python
//! there, so the vLLM stack reads as not set up and nothing is served.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};
use tt_station_agentd::native_vllm::{self, serve_argv};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

fn scratch() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-native-vllm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["home", "tt-metal", "kernel-bundles", "hf/hub"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::env::set_var("HOME", dir.join("home"));
        std::env::set_var("TT_METAL_HOME", dir.join("tt-metal"));
        std::env::set_var("TT_KERNEL_BUNDLES_DIR", dir.join("kernel-bundles"));
        std::env::set_var("HF_HOME", dir.join("hf"));
        dir
    })
}

fn argv(meta: Value, port: u16) -> Vec<String> {
    scratch();
    serve_argv(&meta, port).unwrap().0
}

#[test]
fn the_launch_for_this_box_is_preferred_over_the_default() {
    let meta = json!({ "launch": {
        "default": { "command": ["python", "-m", "default"] },
        "p300x2": { "command": ["python", "-m", "p300"] },
        "wormhole": { "command": ["python", "-m", "wormhole"] },
    }});
    assert_eq!(argv(meta, 1)[2], "p300");
    let meta = json!({ "launch": {
        "blackhole-4card": { "command": ["python", "-m", "bh4"] },
        "blackhole": { "command": ["python", "-m", "bh"] },
    }});
    assert_eq!(argv(meta, 1)[2], "bh");
    // One without a command list doesn't count.
    let meta = json!({ "launch": {
        "blackhole": { "command": "python -m broken" },
        "default": { "command": ["python", "-m", "default"] },
    }});
    assert_eq!(argv(meta, 1)[2], "default");
    // Only another machine's launch: nothing to run here.
    assert!(serve_argv(
        &json!({ "launch": { "wormhole": { "command": ["x"] } } }),
        1
    )
    .is_none());
    assert!(serve_argv(&json!({ "launch": { "default": { "command": [] } } }), 1).is_some());
}

#[test]
fn it_runs_with_tt_metals_python_on_the_serving_port() {
    scratch();
    let tt_metal = native_vllm::tt_metal();
    let a = argv(
        json!({ "launch": { "default": { "command": ["python3", "-m", "x", "--port", "8000", "--other", "8000"] } } }),
        8123,
    );
    assert_eq!(
        a,
        [
            tt_metal.join("python_env/bin/python").display().to_string(),
            "-m".into(),
            "x".into(),
            "--port".into(),
            "8123".into(),
            "--other".into(),
            "8000".into(),
        ]
    );
    // Another interpreter is left as the bundle says.
    let a = argv(
        json!({ "launch": { "default": { "command": ["/opt/py/bin/python", "-m", "x"] } } }),
        9,
    );
    assert_eq!(a[0], "/opt/py/bin/python");
}

#[test]
fn a_port_flag_with_no_value_gets_the_serving_port() {
    let a = argv(
        json!({ "launch": { "default": { "command": ["python", "-m", "x", "--port"] } } }),
        8123,
    );
    assert_eq!(&a[3..], ["--port", "8123"], "{a:?}");
}

#[test]
fn launch_environment_values_are_passed_as_text() {
    scratch();
    let (_, env) = serve_argv(
        &json!({ "launch": { "default": { "command": ["python", "-m", "x"],
            "env": { "MESH_DEVICE": "P150x4", "TT_NUM_CARDS": 4, "TRACE": true } } } }),
        1,
    )
    .unwrap();
    let mut env = env;
    env.sort();
    assert_eq!(
        env,
        [
            ("MESH_DEVICE".to_string(), "P150x4".to_string()),
            ("TRACE".to_string(), "true".to_string()),
            ("TT_NUM_CARDS".to_string(), "4".to_string()),
        ]
    );
}

#[test]
fn vllm_servers_get_prompt_token_details_and_stats_others_do_not() {
    let a = argv(
        json!({ "launch": { "default": { "command": ["python", "-m", "vllm.entrypoints.openai.api_server", "--disable-log-stats"] } } }),
        1,
    );
    assert!(a.contains(&"--enable-prompt-tokens-details".to_string()));
    assert!(!a.contains(&"--disable-log-stats".to_string()));
    let a = argv(
        json!({ "launch": { "default": { "command": ["/venv/bin/vllm", "serve", "m"] } } }),
        1,
    );
    assert!(
        a.contains(&"--enable-prompt-tokens-details".to_string()),
        "{a:?}"
    );
    let a = argv(
        json!({ "launch": { "default": { "command": ["python", "-m", "my_server", "--disable-log-stats"] } } }),
        1,
    );
    assert!(!a.contains(&"--enable-prompt-tokens-details".to_string()));
    assert!(a.contains(&"--disable-log-stats".to_string()));
}

// ---------------------------------------------------------------- routes

/// A bring-up bundle in the box's own bundle folder.
fn bundle(config: &Path, id: &str, meta: Value, files: &[(&str, &str)]) -> PathBuf {
    let dir = native_vllm::station_bundles(
        &AppState::new("x".into(), "x".into(), Arc::new(DstackBackend))
            .with_config_dir(config.to_path_buf()),
    )
    .join(id.replace('/', "__"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("vllm_metadata.json"), meta.to_string()).unwrap();
    for (rel, text) in files {
        std::fs::write(dir.join(rel), text).unwrap();
    }
    dir
}

struct Box_ {
    base: String,
    state: AppState,
    owner: String,
    config: PathBuf,
    c: reqwest::Client,
}

impl Box_ {
    async fn new(tag: &str) -> Self {
        let config = scratch().join(format!("cfg-{tag}"));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(&config).unwrap();
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

fn launch() -> Value {
    json!({ "blackhole": { "command": ["python", "-m", "vllm.entrypoints.openai.api_server", "--model", "acme/Tiny-1B"] } })
}

#[tokio::test]
async fn bundles_are_listed_with_why_one_isnt_ready() {
    let b = Box_::new("list").await;
    let ready = bundle(
        &b.config,
        "acme/tiny",
        json!({ "arch": "TinyForCausalLM", "main_class": "tiny_model:TinyForCausalLM",
            "hf_weights": "acme/Tiny-1B", "launch": launch() }),
        &[("tiny_model.py", "class TinyForCausalLM: ...\n")],
    );
    bundle(
        &b.config,
        "acme/no-launch",
        json!({ "main_class": "m:M", "launch": { "wormhole": { "command": ["x"] } } }),
        &[("m.py", "")],
    );
    bundle(
        &b.config,
        "acme/branch",
        json!({ "main_class": "models.autoports.secret.tt:Model", "launch": launch() }),
        &[(
            "README.md",
            "# Secret\n\nThe model code is on the `someone/secret-port` branch, not upstream.\n",
        )],
    );
    bundle(
        &b.config,
        "acme/autoport",
        json!({ "main_class": "gen:Model", "launch": launch() }),
        &[("gen.py", "from models.autoports.mystery_net import Thing\n")],
    );
    // Weights for the ready one are on the box.
    let snap = scratch().join("hf/hub/models--acme--Tiny-1B/snapshots/abc");
    std::fs::create_dir_all(&snap).unwrap();
    std::fs::write(snap.join("model.safetensors"), "w").unwrap();

    let (status, v) = b.send(&b.owner, "GET", "/vllm/native", json!({})).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["ready"], false, "no tt-metal Python here");
    assert_eq!(v["can_set_up"], false);
    let by_id = |id: &str| {
        v["bundles"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id}: {v}"))
    };
    let tiny = by_id("acme/tiny");
    assert_eq!(tiny["source"], "bring-up");
    assert_eq!(tiny["dir"], ready.display().to_string());
    assert_eq!(tiny["model"], "acme/Tiny-1B");
    assert_eq!(tiny["weights_on_box"], true);
    assert_eq!(tiny["ready"], true, "{tiny}");
    assert!(tiny["why_not"].is_null());
    assert_eq!(
        by_id("acme/no-launch")["why_not"],
        "It has no launch command for this box."
    );
    let branch = by_id("acme/branch")["why_not"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(branch.contains("`someone/secret-port`"), "{branch}");
    let autoport = by_id("acme/autoport")["why_not"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        autoport.contains("models.autoports.mystery_net"),
        "{autoport}"
    );
}

#[tokio::test]
async fn only_a_listed_ready_bundle_is_served_and_only_once_set_up() {
    let b = Box_::new("serve").await;
    let ready = bundle(
        &b.config,
        "acme/ok",
        json!({ "main_class": "ok:M", "launch": launch() }),
        &[("ok.py", "")],
    );
    let broken = bundle(
        &b.config,
        "acme/nolaunch",
        json!({ "main_class": "ok:M" }),
        &[("ok.py", "")],
    );
    // A folder that isn't one of the box's bundles, even if it looks like one.
    let elsewhere = scratch().join("home/elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        elsewhere.join("vllm_metadata.json"),
        json!({ "main_class": "ok:M", "launch": launch() }).to_string(),
    )
    .unwrap();
    std::fs::write(elsewhere.join("ok.py"), "").unwrap();

    let serve = |dir: &Path| json!({ "dir": dir.display().to_string() });
    let (status, r) = b
        .send(&b.owner, "POST", "/vllm/native/serve", serve(&elsewhere))
        .await;
    assert_eq!(
        (status, &r["error"]),
        (404, &json!("No such bundle on this box."))
    );
    let (status, r) = b
        .send(&b.owner, "POST", "/vllm/native/serve", serve(&broken))
        .await;
    assert_eq!(
        (status, &r["error"]),
        (409, &json!("It has no launch command for this box."))
    );
    let (status, r) = b
        .send(&b.owner, "POST", "/vllm/native/serve", serve(&ready))
        .await;
    assert_eq!(status, 409, "{r}");
    assert!(
        r["error"].as_str().unwrap().contains("one-time setup"),
        "{r}"
    );
    assert!(!b.config.join("vllm-native/serve.log").exists());
    let r =
        b.c.post(format!("{}/vllm/native/serve", b.base))
            .json(&serve(&ready))
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 401);
}

#[tokio::test]
async fn setup_is_for_owners_and_waits_for_bring_ups_and_its_installer() {
    let b = Box_::new("setup").await;
    let member = support::pair(&b.base, &b.state).await;
    assert_eq!(
        b.send(&member, "POST", "/vllm/native/setup", json!({}))
            .await
            .0,
        403
    );
    b.state.set_bringup_busy(true);
    let (status, r) = b
        .send(&b.owner, "POST", "/vllm/native/setup", json!({}))
        .await;
    assert_eq!(status, 409);
    assert!(r["error"]
        .as_str()
        .unwrap()
        .starts_with("A bring-up is running."));
    b.state.set_bringup_busy(false);
    let (status, r) = b
        .send(&b.owner, "POST", "/vllm/native/setup", json!({}))
        .await;
    assert_eq!(status, 409);
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .starts_with("tt-kernel's installer isn't here"),
        "{r}"
    );
    assert!(!b.config.join("vllm-native").exists());
}
