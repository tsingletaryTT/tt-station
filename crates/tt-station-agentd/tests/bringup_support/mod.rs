// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Shared harness for the `tests/bringup_*.rs` integration tests: a fake
//! engine environment (Codex, Claude Code, systemd, gozer, tt-metal) so a
//! bring-up can be started, stopped, resumed and deleted without anything real
//! running, plus helpers to seed runs the way the agent persists them.
//!
//! Why fakes on PATH: `crate::bringup` finds `codex`, `claude`, `systemctl`,
//! `systemd-run` and `gozer` by name, and this box has the real ones. Every
//! fake here only records what it was asked and exits; `systemd-run` never
//! runs its command, so no driver, engine or chip reset ever starts.
//!
//! The environment (PATH, HOME, TT_METAL_HOME, CODEX_BIN, no desktop session)
//! is process-global, so it is set exactly once per test binary, through a
//! `OnceLock`, before any test talks to the agent: every test calls
//! [`harness`] (or [`fakes`]) first.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};
use tt_station_agentd::routes::{app, local_http_app, AppState};
use tt_station_agentd::serving::dstack::DstackBackend;

#[path = "../support/mod.rs"]
mod support;

/// The fake world one test binary shares.
pub struct Fakes {
    /// Fake executables, first on PATH.
    pub bin: PathBuf,
    /// What the fakes record: `active/<unit>` (a running unit),
    /// `units/<unit>.args` / `.secret` (a `systemd-run` call), `systemctl.log`.
    pub state: PathBuf,
    /// A tt-metal checkout with fast-models-fast's runner and eleven prompts.
    pub repo: PathBuf,
}

static FAKES: OnceLock<Fakes> = OnceLock::new();

fn script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Build the fakes and point this process at them (once).
pub fn fakes() -> &'static Fakes {
    FAKES.get_or_init(|| {
        // Under cargo's per-target scratch dir, one folder per test process;
        // earlier processes' folders are cleared away.
        let parent = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let exe = std::env::current_exe().unwrap();
        let name = exe
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .split('-')
            .next()
            .unwrap()
            .to_string();
        let prefix = format!("{name}-fakes-");
        for e in std::fs::read_dir(&parent).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if let Some(pid) = f.strip_prefix(&prefix) {
                if !Path::new("/proc").join(pid).exists() {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let root = parent.join(format!("{prefix}{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (bin, st, repo, home) = (
            root.join("bin"),
            root.join("state"),
            root.join("tt-metal"),
            root.join("home"),
        );
        for d in [&bin, &st.join("active"), &st.join("units"), &home] {
            std::fs::create_dir_all(d).unwrap();
        }
        let s = st.display();
        // `systemd-run --user --collect --quiet --unit=U … sh -c SCRIPT`: record
        // the argv and the secret it was handed in its environment, mark U
        // running, run nothing.
        script(
            &bin.join("systemd-run"),
            &format!(
                r#"unit=""
for a in "$@"; do case "$a" in --unit=*) unit="${{a#--unit=}}";; esac; done
printf '%s\n' "$@" > "{s}/units/$unit.args"
printf '%s' "$TT_STATION_RUN_SECRET" > "{s}/units/$unit.secret"
touch "{s}/active/$unit"
exit 0"#
            ),
        );
        // `systemctl --user is-active --quiet U` / `systemctl --user stop U`.
        script(
            &bin.join("systemctl"),
            &format!(
                r#"echo "$*" >> "{s}/systemctl.log"
case "$2" in
  is-active) test -e "{s}/active/$4";;
  stop) rm -f "{s}/active/$3"; exit 0;;
  *) exit 0;;
esac"#
            ),
        );
        // Codex: signed in; anything else it's asked to do fails.
        script(
            &bin.join("codex"),
            r#"if [ "$1 $2" = "login status" ]; then echo "Logged in using ChatGPT"; exit 0; fi
exit 1"#,
        );
        // Claude Code: installed, never signed in.
        script(
            &bin.join("claude"),
            r#"if [ "$1 $2" = "auth status" ]; then echo '{"loggedIn": false}'; exit 0; fi
exit 1"#,
        );
        // gozer: nobody holds the chips; reconcile does nothing.
        script(&bin.join("gozer"), "exit 0");
        for name in ["tt-smi", "gnome-terminal", "tt-model", "docker", "sudo"] {
            script(&bin.join(name), "exit 1");
        }

        // tt-metal with the runner, its python and eleven stage prompts.
        let prompts = repo.join(".agents/prompts/model_bringup_multigoal");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::create_dir_all(repo.join(".agents/scripts")).unwrap();
        std::fs::create_dir_all(repo.join("python_env/bin")).unwrap();
        script(&repo.join(".agents/scripts/multigoal"), "exit 1");
        script(&repo.join("python_env/bin/python"), "exit 1");
        for (i, stage) in [
            "functional-decoder",
            "optimized-decoder",
            "functional-model",
            "optimized-model",
            "multichip",
            "datatype-sweep",
            "tracing",
            "full-model",
            "vllm",
            "optimized-vllm",
            "tti-release",
        ]
        .iter()
        .enumerate()
        {
            std::fs::write(prompts.join(format!("{:02}-{stage}.txt", i + 1)), "/goal x").unwrap();
        }

        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", path);
        std::env::set_var("HOME", &home);
        std::env::set_var("TT_METAL_HOME", &repo);
        std::env::set_var("CODEX_BIN", bin.join("codex"));
        for v in ["DISPLAY", "WAYLAND_DISPLAY", "HF_TOKEN"] {
            std::env::remove_var(v);
        }
        Fakes {
            bin,
            state: st,
            repo,
        }
    })
}

impl Fakes {
    pub fn set_active(&self, unit: &str, active: bool) {
        let p = self.state.join("active").join(unit);
        if active {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }

    pub fn is_active(&self, unit: &str) -> bool {
        self.state.join("active").join(unit).exists()
    }

    /// The argv `systemd-run` was given for `unit`, if it was called.
    pub fn launched(&self, unit: &str) -> Option<Vec<String>> {
        std::fs::read_to_string(self.state.join("units").join(format!("{unit}.args")))
            .ok()
            .map(|t| t.lines().map(str::to_string).collect())
    }

    /// The `TT_STATION_RUN_SECRET` `systemd-run` saw in its environment.
    pub fn launched_secret(&self, unit: &str) -> Option<String> {
        std::fs::read_to_string(self.state.join("units").join(format!("{unit}.secret"))).ok()
    }

    pub fn systemctl_log(&self) -> String {
        std::fs::read_to_string(self.state.join("systemctl.log")).unwrap_or_default()
    }
}

/// An agent on a fresh config dir with an owner and a member paired.
pub struct Station {
    pub state: AppState,
    /// `app` (what phones reach): callers must bring a token.
    pub base: String,
    /// `local_http_app` from 127.0.0.1: the box itself.
    pub local: String,
    pub owner: String,
    pub member: String,
    pub config: tempfile::TempDir,
    pub client: reqwest::Client,
}

pub async fn harness() -> Station {
    fakes();
    let config = tempfile::tempdir().unwrap();
    let t = || vec!["true".to_string()];
    let state = AppState::new("qb2-test".into(), "4xBH".into(), Arc::new(DstackBackend))
        // A chip reset is `true`, never tt-smi.
        .with_power_config(t(), t(), t(), t())
        .with_config_dir(config.path().to_path_buf());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app(state.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let local_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local_addr = local_listener.local_addr().unwrap();
    let local_router =
        local_http_app(state.clone()).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move { axum::serve(local_listener, local_router).await.unwrap() });
    let base = format!("http://{addr}");
    let owner = support::pair(&base, &state).await;
    let member = support::pair(&base, &state).await;
    Station {
        state,
        base,
        local: format!("http://{local_addr}"),
        owner,
        member,
        config,
        client: reqwest::Client::new(),
    }
}

async fn reply(r: reqwest::Response) -> (u16, Value) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

impl Station {
    /// A call as a paired device (`token`), or unpaired (`""`).
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
        body: Value,
    ) -> (u16, Value) {
        let mut req = self
            .client
            .request(method, format!("{}{path}", self.base))
            .json(&body);
        if !token.is_empty() {
            req = req.bearer_auth(token);
        }
        reply(req.send().await.unwrap()).await
    }

    /// A call the way the run's driver makes it (`codex_bringup.post`): a JSON
    /// POST with `X-Run-Secret`, from the box itself, no device token.
    pub async fn driver(&self, path: &str, secret: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self
            .client
            .post(format!("{}{path}", self.local))
            .json(&body);
        if let Some(s) = secret {
            req = req.header("X-Run-Secret", s);
        }
        reply(req.send().await.unwrap()).await
    }

    /// The same driver call over `app` with a device token as well.
    pub async fn driver_with_token(
        &self,
        path: &str,
        token: &str,
        secret: Option<&str>,
        body: Value,
    ) -> (u16, Value) {
        let mut req = self
            .client
            .post(format!("{}{path}", self.base))
            .bearer_auth(token)
            .json(&body);
        if let Some(s) = secret {
            req = req.header("X-Run-Secret", s);
        }
        reply(req.send().await.unwrap()).await
    }

    pub fn bringup_dir(&self) -> PathBuf {
        self.config.path().join("bringup")
    }

    pub fn runs_file(&self) -> PathBuf {
        self.bringup_dir().join("codex-runs.json")
    }

    /// The runs as the agent persisted them.
    pub fn saved_runs(&self) -> Vec<Value> {
        std::fs::read(self.runs_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn write_runs(&self, runs: &[Value]) {
        std::fs::create_dir_all(self.bringup_dir()).unwrap();
        std::fs::write(self.runs_file(), serde_json::to_vec(runs).unwrap()).unwrap();
    }

    /// The activity log, newest first: (action, target, result).
    pub async fn audit(&self) -> Vec<(String, String, String)> {
        let (status, log) = self
            .call(
                reqwest::Method::GET,
                "/audit?limit=200",
                &self.owner,
                json!({}),
            )
            .await;
        assert_eq!(status, 200, "{log}");
        log["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["action"].as_str().unwrap_or("").to_string(),
                    e["target"].as_str().unwrap_or("").to_string(),
                    e["result"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }
}

/// A run as `crate::bringup::CodexRun` serializes it, with its log folder
/// created under the station's bring-up folder. Not finished, not stopped;
/// whether it is live is the fake unit's state ([`Fakes::set_active`]).
pub fn run_record(s: &Station, id: &str, secret: &str) -> Value {
    let log_dir = s.bringup_dir().join("codex").join(id);
    std::fs::create_dir_all(&log_dir).unwrap();
    json!({
        "id": id,
        "engine": "codex",
        "model": "Qwen/Qwen3-0.6B",
        "model_dir": "models/autoports/qwen_qwen3_0_6b",
        "log_dir": log_dir,
        "unit": format!("tt-codex-bringup-{id}"),
        "policy": "smart",
        "started": 1_700_000_000u64,
        "secret": secret,
        "restore": null,
        "finished": null,
        "exit": null,
        "stopped": false,
    })
}

/// A run id no other test in this process uses (approvals are kept per
/// process, keyed by run id).
pub fn unique_id(tag: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!("{tag}{:04}", N.fetch_add(1, Ordering::SeqCst))
}
