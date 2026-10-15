// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Store apps (`src/apps.rs`) end to end through the router, with `docker`
//! replaced by a shell stub on `PATH`:
//!
//! - an install pulls, then runs the container published on the box's
//!   loopback only, with its secrets handed over by name (never in argv);
//! - an app the owner opened to the network is published on every address,
//!   and turns its own sign-in on (`LoginWhenShared`);
//! - a stopped container made for another setting is made again, keeping its
//!   token, without pulling; one that matches is just started;
//! - what an app needs is installed first, and other apps' addresses are
//!   filled in on the apps' own network;
//! - failures (a pull that fails, a container that keeps restarting) show on
//!   the tile; who may open what to the network, and the audit of it.
//!
//! The stub keeps per-container state (`<name>.status`, `<name>.inspect`) in
//! one directory for the whole test binary, so each test uses its own apps.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

mod support;

/// A `docker` that answers from files in its own directory and logs each call.
const FAKE_DOCKER: &str = r#"#!/bin/sh
D=$(dirname "$0")
echo "$*" >> "$D/log"
case "$1" in
  inspect)
    c=$4
    [ -f "$D/$c.status" ] || { echo "Error: No such object: $c" >&2; exit 1; }
    case "$3" in
      '{{.State.Status}}') cat "$D/$c.status" ;;
      *) if [ -f "$D/$c.inspect" ]; then cat "$D/$c.inspect"; else echo '{}'; fi ;;
    esac ;;
  pull)
    f="$D/fail-pull-$(echo "$2" | tr '/:@' '___')"
    if [ -f "$f" ]; then cat "$f" >&2; exit 1; fi ;;
  run)
    shift
    name=; p=; tmp="$D/env.$$"
    : > "$tmp"
    while [ $# -gt 0 ]; do
      case "$1" in
        --name) name=$2; shift ;;
        -p) p=$2; shift ;;
        -e) case "$2" in
              *=*) echo "$2" >> "$tmp" ;;
              *) echo "$2=$(printenv "$2")" >> "$tmp" ;;
            esac
            shift ;;
      esac
      shift
    done
    case "$p" in 127.0.0.1:*) ip=127.0.0.1 ;; *) ip= ;; esac
    { printf '{"x/tcp":[{"HostIp":"%s","HostPort":"1"}]}\n' "$ip"; cat "$tmp"; } > "$D/$name.inspect"
    rm -f "$tmp"
    echo running > "$D/$name.status" ;;
  rm) rm -f "$D/$3.status" "$D/$3.inspect" ;;
  start) [ -f "$D/$2.status" ] || exit 1; echo running > "$D/$2.status" ;;
  stop) [ -f "$D/$2.status" ] && echo exited > "$D/$2.status" ;;
  logs) [ -f "$D/$4.logs" ] && cat "$D/$4.logs" ;;
esac
exit 0
"#;

/// The stub's directory, put first on `PATH` once for this test binary.
fn docker_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("tt-apps-docker-{}", std::process::id()));
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

/// Every `docker` call so far that mentions `container`.
fn calls_for(container: &str) -> Vec<String> {
    std::fs::read_to_string(docker_dir().join("log"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.split(' ').any(|w| w == container))
        .map(str::to_string)
        .collect()
}

/// A container the stub already has: its state, its port bindings' host IP
/// and its environment.
fn existing(container: &str, status: &str, host_ip: &str, env: &[&str]) {
    let dir = docker_dir();
    std::fs::write(dir.join(format!("{container}.status")), status).unwrap();
    let mut inspect = format!("{{\"x/tcp\":[{{\"HostIp\":\"{host_ip}\",\"HostPort\":\"1\"}}]}}\n");
    for e in env {
        inspect.push_str(e);
        inspect.push('\n');
    }
    std::fs::write(dir.join(format!("{container}.inspect")), inspect).unwrap();
}

fn env_of(container: &str) -> Vec<String> {
    std::fs::read_to_string(docker_dir().join(format!("{container}.inspect")))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .map(str::to_string)
        .collect()
}

fn env_value(container: &str, key: &str) -> Option<String> {
    env_of(container)
        .iter()
        .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
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
        docker_dir();
        let config = std::env::temp_dir().join(format!("tt-apps-cfg-{tag}-{}", std::process::id()));
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

    async fn apps(&self) -> Vec<Value> {
        let v: Value = self
            .c
            .get(format!("{}/apps", self.base))
            .bearer_auth(&self.owner)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["apps"].as_array().unwrap().clone()
    }

    async fn tile(&self, id: &str) -> Value {
        self.apps()
            .await
            .into_iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("no {id} tile"))
    }

    /// The tile once `done` holds for it (installs run on a thread).
    async fn tile_when(&self, id: &str, done: impl Fn(&Value) -> bool) -> Value {
        let mut t = Value::Null;
        for _ in 0..100 {
            t = self.tile(id).await;
            if done(&t) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{id} never got there: {t}");
    }

    async fn post(&self, path: &str) -> (u16, Value) {
        let r = self
            .c
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.owner)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    async fn network(&self, token: &str, id: &str, open: bool) -> (u16, Value) {
        let r = self
            .c
            .put(format!("{}/apps/{id}/network", self.base))
            .bearer_auth(token)
            .json(&json!({ "open": open }))
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
}

#[tokio::test]
async fn the_app_list_needs_a_paired_device() {
    let b = Box_::new("auth").await;
    let r = b.c.get(format!("{}/apps", b.base)).send().await.unwrap();
    assert_eq!(r.status(), 401);
    for (method, path) in [
        ("POST", "/apps/comfyui/start"),
        ("POST", "/apps/comfyui/stop"),
        ("DELETE", "/apps/comfyui"),
    ] {
        let r =
            b.c.request(method.parse().unwrap(), format!("{}{path}", b.base))
                .send()
                .await
                .unwrap();
        assert_eq!(r.status(), 401, "{method} {path}");
    }
    assert!(calls_for("tt-app-comfyui")
        .iter()
        .all(|c| c.starts_with("inspect")));
}

#[tokio::test]
async fn an_unknown_app_is_not_found() {
    let b = Box_::new("unknown").await;
    assert_eq!(b.post("/apps/nope/start").await.0, 404);
    assert_eq!(b.post("/apps/nope/stop").await.0, 404);
    assert_eq!(b.network(&b.owner, "nope", true).await.0, 404);
    let r =
        b.c.delete(format!("{}/apps/nope", b.base))
            .bearer_auth(&b.owner)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 404);
    assert!(calls_for("tt-app-nope").is_empty());
}

#[tokio::test]
async fn an_install_publishes_on_the_box_only_and_hands_secrets_over_by_name() {
    let b = Box_::new("jupyter").await;
    let before = b.tile("jupyter").await;
    assert_eq!(before["state"], "not_installed", "{before}");
    assert_eq!(before["local_only"], true);
    assert!(before["url"].is_null());

    let (status, body) = b.post("/apps/jupyter/start").await;
    assert_eq!((status, &body["state"]), (200, &json!("installing")));
    let tile = b.tile_when("jupyter", |t| t["state"] == "running").await;

    let calls = calls_for("tt-app-jupyter");
    let pull = calls
        .iter()
        .position(|c| c.starts_with("pull "))
        .unwrap_or(usize::MAX);
    let run = calls
        .iter()
        .position(|c| c.starts_with("run "))
        .unwrap_or_else(|| panic!("never ran: {calls:?}"));
    assert!(pull == usize::MAX || pull < run);
    let all = std::fs::read_to_string(docker_dir().join("log")).unwrap();
    assert!(
        all.lines()
            .any(|l| l.starts_with("pull quay.io/jupyter/minimal-notebook")),
        "{all}"
    );
    let argv = &calls[run];
    assert!(argv.contains("-p 127.0.0.1:3021:8888"), "{argv}");
    assert!(argv.contains("--label tt-station.app=jupyter"), "{argv}");
    assert!(argv.contains("--restart unless-stopped"), "{argv}");
    assert!(argv.contains("--network tt-apps"), "{argv}");
    // Secrets by name only: the values never show in a process list.
    assert!(argv.contains("-e JUPYTER_TOKEN "), "{argv}");
    assert!(argv.contains("-e OPENAI_API_KEY "), "{argv}");
    let token = env_value("tt-app-jupyter", "JUPYTER_TOKEN").unwrap();
    assert_eq!(token.len(), 16, "{token}");
    assert!(!argv.contains(&token), "{argv}");
    let key = env_value("tt-app-jupyter", "OPENAI_API_KEY").unwrap();
    assert!(!key.is_empty() && !argv.contains(&key), "{argv}");
    assert_eq!(
        env_value("tt-app-jupyter", "OPENAI_BASE_URL").as_deref(),
        Some("http://host.docker.internal:8765/v1")
    );

    // The tile shows its box-only address, with the token it was made with.
    assert_eq!(
        tile["url"],
        format!("http://127.0.0.1:3021/lab?token={token}"),
        "{tile}"
    );
    assert_eq!(tile["local_only"], true);
    assert_eq!(tile["network"]["can_share"], false);
    assert_eq!(tile["network"]["restart_to_apply"], false);
    assert!(tile["error"].is_null());
}

#[tokio::test]
async fn an_app_opened_to_the_network_is_published_on_every_address() {
    let b = Box_::new("lobechat").await;
    let (status, tile) = b.network(&b.owner, "lobechat", true).await;
    assert_eq!(status, 200, "{tile}");
    assert_eq!(tile["network"]["open"], true);
    assert_eq!(tile["network"]["can_share"], true);
    assert!(tile["network"]["who"].as_str().unwrap().contains(":3020"));
    // Not running yet: still box-only.
    assert_eq!(tile["local_only"], true);
    let saved: Vec<String> =
        serde_json::from_slice(&std::fs::read(b.config.join("apps/network.json")).unwrap())
            .unwrap();
    assert_eq!(saved, ["lobechat"]);

    assert_eq!(b.post("/apps/lobechat/start").await.0, 200);
    let tile = b.tile_when("lobechat", |t| t["state"] == "running").await;
    let run = calls_for("tt-app-lobechat")
        .into_iter()
        .find(|c| c.starts_with("run "))
        .unwrap();
    assert!(run.contains("-p 3020:3210"), "{run}");
    assert!(!run.contains("127.0.0.1:3020"), "{run}");
    assert_eq!(tile["local_only"], false, "{tile}");
    assert_eq!(tile["network"]["on_network"], true);
    assert_eq!(tile["network"]["restart_to_apply"], false);
    // Its own sign-in: the access code it was made with.
    let code = env_value("tt-app-lobechat", "ACCESS_CODE").unwrap();
    assert_eq!(tile["login"], json!({ "access_code": code }));

    // Back to the box only: the running container keeps its address until restarted.
    let (status, tile) = b.network(&b.owner, "lobechat", false).await;
    assert_eq!(status, 200);
    assert_eq!(tile["network"]["open"], false);
    assert_eq!(tile["network"]["restart_to_apply"], true, "{tile}");
    assert_eq!(tile["local_only"], true);

    let actions: Vec<(String, String)> = b
        .state
        .audit()
        .page(0, 50)
        .0
        .into_iter()
        .filter(|e| e.target == "lobechat")
        .map(|e| (e.action, e.result))
        .collect();
    assert!(actions.contains(&("app_network_open".into(), "ok".into())));
    assert!(actions.contains(&("app_network_close".into(), "ok".into())));
}

#[tokio::test]
async fn an_app_that_never_shares_is_refused_and_the_refusal_is_audited() {
    let b = Box_::new("comfy-share").await;
    let (status, body) = b.network(&b.owner, "comfyui", true).await;
    assert_eq!(status, 409, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap()
        .starts_with("ComfyUI stays on the box."));
    assert!(!b.config.join("apps/network.json").exists());
    assert_eq!(b.tile("comfyui").await["network"]["open"], false);
    let refused =
        b.state.audit().page(0, 50).0.into_iter().any(|e| {
            e.action == "app_network_open" && e.target == "comfyui" && e.result == "refused"
        });
    assert!(refused);
    // Closing it is always allowed (nothing to refuse).
    assert_eq!(b.network(&b.owner, "comfyui", false).await.0, 200);
}

#[tokio::test]
async fn n8n_is_not_opened_before_it_runs_and_has_an_owner() {
    let b = Box_::new("n8n").await;
    let (status, body) = b.network(&b.owner, "n8n", true).await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("Start n8n"),
        "{body}"
    );
    assert!(!b.config.join("apps/network.json").exists());
}

#[tokio::test]
async fn only_an_owner_opens_an_app_to_the_network() {
    let b = Box_::new("member").await;
    let member = support::pair(&b.base, &b.state).await;
    let (status, _) = b.network(&member, "scrapling", true).await;
    assert_eq!(status, 403);
    assert!(!b.config.join("apps/network.json").exists());
    // A member still sees the list.
    let r =
        b.c.get(format!("{}/apps", b.base))
            .bearer_auth(&member)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn a_stopped_container_made_for_another_setting_is_made_again_with_its_token() {
    let b = Box_::new("langflow").await;
    // Made box-only, before its owner opened it.
    existing(
        "tt-app-langflow",
        "exited",
        "127.0.0.1",
        &[
            "LANGFLOW_SUPERUSER_PASSWORD=kept-token-1234",
            "OPENAI_COMPATIBLE_API_KEY=kept-key-5678",
        ],
    );
    assert_eq!(b.network(&b.owner, "langflow", true).await.0, 200);
    let tile = b.tile("langflow").await;
    assert_eq!(tile["state"], "stopped");
    assert_eq!(tile["network"]["restart_to_apply"], true, "{tile}");

    let (status, body) = b.post("/apps/langflow/start").await;
    assert_eq!((status, &body["state"]), (200, &json!("running")), "{body}");
    let calls = calls_for("tt-app-langflow");
    let rm = calls.iter().position(|c| c == "rm -f tt-app-langflow");
    let run = calls.iter().position(|c| c.starts_with("run "));
    assert!(rm.is_some() && run.is_some() && rm < run, "{calls:?}");
    assert!(!calls.iter().any(|c| c.starts_with("start ")), "{calls:?}");
    let log = std::fs::read_to_string(docker_dir().join("log")).unwrap();
    assert!(
        !log.lines().any(|l| l.starts_with("pull langflowai/")),
        "re-made without pulling"
    );
    assert!(calls[run.unwrap()].contains("-p 3028:7860"));
    // Same token and key; sign-in turned on now that it's shared.
    assert_eq!(
        env_value("tt-app-langflow", "LANGFLOW_SUPERUSER_PASSWORD").as_deref(),
        Some("kept-token-1234")
    );
    assert_eq!(
        env_value("tt-app-langflow", "OPENAI_COMPATIBLE_API_KEY").as_deref(),
        Some("kept-key-5678")
    );
    assert_eq!(
        env_value("tt-app-langflow", "LANGFLOW_AUTO_LOGIN").as_deref(),
        Some("False")
    );
    let tile = b.tile("langflow").await;
    assert_eq!(tile["network"]["restart_to_apply"], false, "{tile}");
    assert_eq!(
        tile["login"],
        json!({ "user": "langflow", "password": "kept-token-1234" })
    );
}

#[tokio::test]
async fn a_stopped_container_that_matches_its_setting_is_just_started() {
    let b = Box_::new("flowise").await;
    existing(
        "tt-app-flowise",
        "exited",
        "127.0.0.1",
        &["FLOWISE_PASSWORD=pw-flowise-0001"],
    );
    let (status, body) = b.post("/apps/flowise/start").await;
    assert_eq!((status, &body["state"]), (200, &json!("running")), "{body}");
    let calls = calls_for("tt-app-flowise");
    assert!(
        calls.contains(&"start tt-app-flowise".to_string()),
        "{calls:?}"
    );
    assert!(!calls
        .iter()
        .any(|c| c.starts_with("rm ") || c.starts_with("run ")));
    let tile = b.tile("flowise").await;
    assert_eq!(tile["state"], "running");
    assert_eq!(
        tile["login"],
        json!({ "user": "admin", "password": "pw-flowise-0001" })
    );

    // Stopped from the tile.
    let (status, body) = b.post("/apps/flowise/stop").await;
    assert_eq!((status, &body["state"]), (200, &json!("stopped")));
    assert!(calls_for("tt-app-flowise").contains(&"stop tt-app-flowise".to_string()));
    assert_eq!(b.tile("flowise").await["state"], "stopped");
}

#[tokio::test]
async fn what_an_app_needs_is_installed_first_and_reached_on_the_apps_network() {
    let b = Box_::new("ldr").await;
    let (status, _) = b.post("/apps/localdeepresearch/start").await;
    assert_eq!(status, 200);
    b.tile_when("localdeepresearch", |t| t["state"] == "running")
        .await;
    let log = std::fs::read_to_string(docker_dir().join("log")).unwrap();
    let runs: Vec<&str> = log.lines().filter(|l| l.starts_with("run ")).collect();
    let searx = runs
        .iter()
        .position(|l| l.contains("--name tt-app-searxng "));
    let ldr = runs
        .iter()
        .position(|l| l.contains("--name tt-app-localdeepresearch "));
    assert!(searx.is_some() && searx < ldr, "{runs:?}");
    assert_eq!(b.tile("searxng").await["state"], "running");
    assert_eq!(
        env_value(
            "tt-app-localdeepresearch",
            "LDR_SEARCH_ENGINE_WEB_SEARXNG_DEFAULT_PARAMS_INSTANCE_URL"
        )
        .as_deref(),
        Some("http://tt-app-searxng:8080")
    );
    assert_eq!(
        env_value("tt-app-localdeepresearch", "LDR_LLM_MODEL").as_deref(),
        Some("quietbox"),
        "nothing serving: the gateway's catch-all name"
    );
    // SearXNG's settings file carries the same secret as its env, and is mounted.
    let secret = env_value("tt-app-searxng", "SEARXNG_SECRET").unwrap();
    let settings = std::fs::read_to_string(b.config.join("apps/searxng/settings.yml")).unwrap();
    assert!(
        settings.contains(&format!("secret_key: \"{secret}\"")),
        "{settings}"
    );
    let searx_run = runs[searx.unwrap()];
    assert!(
        searx_run.contains(&format!(
            "{}:/etc/searxng/settings.yml",
            b.config.join("apps/searxng/settings.yml").display()
        )),
        "{searx_run}"
    );
    // SearXNG shows it's in use by the research app.
    assert_eq!(
        b.tile("searxng").await["used_by"],
        json!(["Local Deep Research"])
    );
}

#[tokio::test]
async fn an_app_that_shows_its_key_keeps_it_private_on_disk() {
    let b = Box_::new("vane").await;
    assert_eq!(b.post("/apps/vane/start").await.0, 200);
    let tile = b.tile_when("vane", |t| t["state"] == "running").await;
    let key_file = b.config.join("apps/vane/key");
    let key = std::fs::read_to_string(&key_file).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&key_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        env_value("tt-app-vane", "OPENAI_API_KEY"),
        Some(key.clone())
    );
    assert_eq!(tile["connect"]["api_key"], key, "{tile}");
    assert_eq!(
        tile["connect"]["base_url"],
        "http://host.docker.internal:8765/v1"
    );
}

#[tokio::test]
async fn a_failed_pull_shows_on_the_tile_until_the_app_is_removed() {
    let b = Box_::new("crawl").await;
    let image = tt_station_agentd::apps::app("crawl4ai").unwrap().image;
    let fail = docker_dir().join(format!("fail-pull-{}", image.replace(['/', ':', '@'], "_")));
    std::fs::write(
        &fail,
        "pulling layer\nError: toomanyrequests: rate limited\n",
    )
    .unwrap();

    assert_eq!(b.post("/apps/crawl4ai/start").await.0, 200);
    let tile = b.tile_when("crawl4ai", |t| t["state"] == "failed").await;
    assert_eq!(tile["error"], "Error: toomanyrequests: rate limited");
    assert_eq!(tile["step"], "Failed");
    assert!(!calls_for("tt-app-crawl4ai")
        .iter()
        .any(|c| c.starts_with("run ")));

    let r =
        b.c.delete(format!("{}/apps/crawl4ai", b.base))
            .bearer_auth(&b.owner)
            .send()
            .await
            .unwrap();
    assert_eq!(r.status(), 200);
    assert!(calls_for("tt-app-crawl4ai").contains(&"rm -f tt-app-crawl4ai".to_string()));
    let tile = b.tile("crawl4ai").await;
    assert_eq!(tile["state"], "not_installed", "{tile}");
    assert!(tile["error"].is_null());
    std::fs::remove_file(fail).unwrap();
}

#[tokio::test]
async fn a_container_that_keeps_restarting_is_shown_failed_and_replaced_on_start() {
    let b = Box_::new("openhands").await;
    existing(
        "tt-app-openhands",
        "restarting",
        "127.0.0.1",
        &["LOCAL_BACKEND_API_KEY=old"],
    );
    std::fs::write(
        docker_dir().join("tt-app-openhands.logs"),
        "booting\nFATAL: database is locked\n\n",
    )
    .unwrap();
    let tile = b.tile("openhands").await;
    assert_eq!(tile["state"], "failed", "{tile}");
    assert_eq!(
        tile["error"],
        "It keeps stopping: FATAL: database is locked"
    );

    let (status, body) = b.post("/apps/openhands/start").await;
    assert_eq!(
        (status, &body["state"]),
        (200, &json!("installing")),
        "{body}"
    );
    b.tile_when("openhands", |t| t["state"] == "running").await;
    let calls = calls_for("tt-app-openhands");
    let rm = calls.iter().position(|c| c == "rm -f tt-app-openhands");
    let run = calls.iter().position(|c| c.starts_with("run "));
    assert!(rm.is_some() && rm < run, "{calls:?}");
    // A fresh container: a fresh token.
    assert_ne!(
        env_value("tt-app-openhands", "LOCAL_BACKEND_API_KEY").as_deref(),
        Some("old")
    );
}
