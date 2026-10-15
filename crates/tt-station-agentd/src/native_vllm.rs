// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Native vLLM: Tenstorrent's vLLM fork with its TT plugin (`vllm-tt-plugin`),
//! run straight from tt-metal's Python environment, no container.
//!
//! What it serves are vLLM *bundles*, tt-kernel's format: a folder with a
//! `vllm_metadata.json` (the HF architecture, the adapter class, a launch
//! command per machine) that the plugin registers through `EXTRA_MODELS_DIR`.
//! They come from two places:
//!
//! - `tt-kernel pull <namespace/model>` (`~/.cache/tt-kernel/bundles`), and
//! - bring-ups whose vLLM stage passed: TT Station writes a bundle for the port
//!   (`<config>/vllm-bundles`), so it can be tried before it's packaged.
//!
//! The server runs as the same user unit as Orchard's packages
//! ([`crate::orchard::HARVEST_UNIT`]) on the serving port, and the agent adopts
//! it as the served model once it answers.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

type Reply<T = Json<Value>> = Result<T, (StatusCode, Json<Value>)>;

/// The unit that installs the vLLM fork and plugin into tt-metal's environment.
const SETUP_UNIT: &str = "tt-station-vllm-setup";

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// tt-metal's checkout: where the model code lives and whose Python env serves.
pub fn tt_metal() -> PathBuf {
    std::env::var_os("TT_METAL_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("tt-metal"))
}

fn python() -> PathBuf {
    tt_metal().join("python_env/bin/python")
}

/// TT Station's own bundles (bring-up ports), registered via `EXTRA_MODELS_DIR`.
pub fn station_bundles(state: &AppState) -> PathBuf {
    state.config_dir().join("vllm-bundles")
}

fn tt_kernel_bundles() -> PathBuf {
    std::env::var_os("TT_KERNEL_BUNDLES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".cache/tt-kernel/bundles"))
}

fn tt_kernel_repo() -> PathBuf {
    home().join("tt-kernel-package-manager")
}

/// Is the vLLM fork and the TT plugin importable from tt-metal's Python?
/// (Detection only: `find_spec`, nothing is imported.) Cached for a minute.
static STACK: Mutex<Option<(Instant, (bool, bool))>> = Mutex::new(None);

fn stack_ready() -> (bool, bool) {
    if let Some((at, v)) = *STACK.lock().expect("vllm cache") {
        if at.elapsed() < Duration::from_secs(60) {
            return v;
        }
    }
    let out = Command::new(python())
        .args(["-c", "import importlib.util as u; print(bool(u.find_spec('vllm')), bool(u.find_spec('vllm_tt_plugin')))"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut it = text.split_whitespace().map(|w| w == "True");
    let v = (it.next().unwrap_or(false), it.next().unwrap_or(false));
    *STACK.lock().expect("vllm cache") = Some((Instant::now(), v));
    v
}

/// Look again next time (the stack is being set up).
fn forget_stack() {
    *STACK.lock().expect("vllm cache") = None;
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The launch for this box (Blackhole), else the bundle's default.
fn launch(meta: &Value) -> Option<&Value> {
    let l = &meta["launch"];
    ["blackhole", "blackhole-4card", "p300x2", "default"]
        .iter()
        .map(|k| &l[*k])
        .find(|v| v["command"].is_array())
}

/// Where a `module.path:Class` main class's code is, if it's on this box: in
/// the bundle folder, or in tt-metal's checkout.
fn code_present(bundle: &Path, main_class: &str) -> bool {
    let module = main_class.split(':').next().unwrap_or("");
    let rel = format!("{}.py", module.replace('.', "/"));
    bundle.join(&rel).is_file() || tt_metal().join(&rel).is_file()
}

/// Whether a Hugging Face repo's weights are in the local cache.
/// All of a model's weight files on the box: every shard its
/// `model.safetensors.index.json` lists, complete (not mid-download).
/// (have, need) counts the shards; need is 0 when there's no index to go by.
pub(crate) fn weights_complete(repo: &str) -> (bool, usize, usize) {
    let hub = std::env::var_os("HF_HOME")
        .map(|h| PathBuf::from(h).join("hub"))
        .unwrap_or_else(|| home().join(".cache/huggingface/hub"));
    let snaps = hub
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    let mut best = (false, 0, 0);
    for snap in std::fs::read_dir(snaps).into_iter().flatten().flatten() {
        let dir = snap.path();
        let index: Option<serde_json::Value> =
            std::fs::read(dir.join("model.safetensors.index.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
        let need: std::collections::BTreeSet<String> = index
            .as_ref()
            .and_then(|i| i["weight_map"].as_object())
            .map(|m| {
                m.values()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if need.is_empty() {
            // A single-file model: its safetensors, resolved (a link to a finished file).
            let one = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .any(|f| {
                    f.file_name().to_string_lossy().ends_with(".safetensors")
                        && std::fs::metadata(f.path()).is_ok()
                });
            if one {
                return (true, 1, 1);
            }
            continue;
        }
        let have = need
            .iter()
            .filter(|f| std::fs::metadata(dir.join(f)).is_ok_and(|m| m.len() > 0))
            .count();
        if have > best.1 || best.2 == 0 {
            best = (have == need.len(), have, need.len());
        }
        if have == need.len() {
            return best;
        }
    }
    best
}

pub(crate) fn weights_present(repo: &str) -> bool {
    let hub = std::env::var_os("HF_HOME")
        .map(|h| PathBuf::from(h).join("hub"))
        .unwrap_or_else(|| home().join(".cache/huggingface/hub"));
    let snaps = hub
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots");
    std::fs::read_dir(snaps)
        .into_iter()
        .flatten()
        .flatten()
        .any(|s| {
            std::fs::read_dir(s.path())
                .into_iter()
                .flatten()
                .flatten()
                .any(|f| f.file_name().to_string_lossy().ends_with(".safetensors"))
        })
}

/// Model code a bundle's own Python imports from tt-metal's autoports
/// (`models.autoports.<name>`) that this box's tt-metal doesn't have.
fn missing_autoports(dir: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().and_then(|x| x.to_str()) != Some("py") {
            continue;
        }
        let text = std::fs::read_to_string(e.path()).unwrap_or_default();
        for (i, _) in text.match_indices("models.autoports.") {
            let name: String = text[i + "models.autoports.".len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let module = format!("models.autoports.{name}");
            if !name.is_empty()
                && !tt_metal().join("models/autoports").join(&name).is_dir()
                && !missing.contains(&module)
            {
                missing.push(module);
            }
        }
    }
    missing
}

/// Where a bundle's README says its code lives: the `branch` it names, if any.
fn code_branch(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("README.md")).ok()?;
    text.lines()
        .find(|l| l.contains("branch") || l.contains("not upstream"))
        .and_then(|l| {
            let mut parts = l.split('`').skip(1).step_by(2);
            parts.find(|p| p.contains('/') && !p.contains(' ') && !p.contains('.'))
        })
        .map(str::to_string)
        .or_else(|| {
            // The branch can sit on the line before "not upstream".
            let lines: Vec<&str> = text.lines().collect();
            lines
                .windows(2)
                .find(|w| w[1].contains("not upstream") || w[1].contains("branch"))
                .and_then(|w| {
                    w.iter()
                        .flat_map(|l| l.split('`').skip(1).step_by(2))
                        .find(|p| p.contains('/') && !p.contains(' ') && !p.contains('.'))
                        .map(str::to_string)
                })
        })
}

/// One bundle as the app shows it.
fn describe_bundle(dir: &Path, id: String, source: &str) -> Option<Value> {
    let meta: Value =
        serde_json::from_slice(&std::fs::read(dir.join("vllm_metadata.json")).ok()?).ok()?;
    let main_class = meta["main_class"].as_str().unwrap_or("");
    let weights = meta["hf_weights"].as_str().map(str::to_string);
    let l = launch(&meta);
    let served = l
        .and_then(|l| l["command"].as_array())
        .and_then(|c| {
            c.iter()
                .position(|a| a == "--model")
                .and_then(|i| c.get(i + 1))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let missing = missing_autoports(dir);
    let code = code_present(dir, main_class) && missing.is_empty();
    let why_not = if l.is_none() {
        Some("It has no launch command for this box.".to_string())
    } else if !code {
        Some(match code_branch(dir) {
            Some(branch) => format!(
                "Its model code isn't published yet: it's on the author's tt-metal branch `{branch}`, which isn't public. Look for a community version of this model instead."
            ),
            None => format!(
                "It needs model code your tt-metal doesn't have ({}), and doesn't say where to get it.",
                missing.first().cloned().unwrap_or_else(|| main_class.split(':').next().unwrap_or("?").to_string())
            ),
        })
    } else {
        None
    };
    Some(json!({
        "id": id, "source": source, "dir": dir.display().to_string(),
        "arch": meta["arch"], "description": meta["description"].as_str().map(|d| d.chars().take(400).collect::<String>()),
        "model": served.or(weights.clone()), "weights": weights,
        "weights_on_box": weights.as_deref().is_some_and(weights_present),
        "code_present": code, "ready": why_not.is_none(), "why_not": why_not,
    }))
}

/// Every bundle on the box: tt-kernel's, then TT Station's own.
pub fn bundles(state: &AppState) -> Vec<Value> {
    let mut out = Vec::new();
    let db: Value = std::fs::read(home().join(".cache/tt-kernel/installed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let mut listed = std::collections::HashSet::new();
    if let Some(entries) = db.as_object() {
        for (id, e) in entries.iter().filter(|(_, e)| e["backend"] == "vllm") {
            if let Some(dir) = e["bundle_path"].as_str().map(PathBuf::from) {
                listed.insert(dir.clone());
                out.extend(describe_bundle(&dir, id.clone(), "tt-kernel"));
            }
        }
    }
    for (base, source) in [
        (tt_kernel_bundles(), "tt-kernel"),
        (station_bundles(state), "bring-up"),
    ] {
        for e in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            let dir = e.path();
            if dir.is_dir() && !listed.contains(&dir) {
                let id = e.file_name().to_string_lossy().replacen("__", "/", 1);
                out.extend(describe_bundle(&dir, id, source));
            }
        }
    }
    out
}

fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// `GET /vllm/native`: is the stack ready, is it being set up, what can it serve.
pub(crate) async fn get_native(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let st = state.clone();
    Json(
        tokio::task::spawn_blocking(move || {
            let (vllm, plugin) = stack_ready();
            let setting_up = unit_active(SETUP_UNIT);
            if setting_up {
                forget_stack();
            }
            let log = st.config_dir().join("vllm-native/setup.log");
            let setup_exit = std::fs::read_to_string(st.config_dir().join("vllm-native/setup.exit")).ok().map(|t| t.trim().to_string());
            let serving = st.lifecycle();
            json!({
                "ready": vllm && plugin, "vllm": vllm, "plugin": plugin,
                "python": python().display().to_string(), "tt_metal": tt_metal().display().to_string(),
                "can_set_up": tt_kernel_repo().join("scripts/install.sh").is_file(),
                // tt-metal's environment is in use: setting up would change it under a run.
                "env_busy": st.bringup_busy() || st.orchard_busy(),
                "setting_up": setting_up,
                "setup_failed": !setting_up && setup_exit.as_deref().is_some_and(|e| e != "0"),
                "setup_log": if setting_up || setup_exit.is_some() { tail(&log, 12) } else { Vec::new() },
                "bundles": bundles(&st),
                // Still ours only while the agent serves the model native vLLM started.
                "serving_native": NATIVE_SERVING.lock().expect("native").clone()
                    .filter(|(_, model)| !matches!(serving.phase, Phase::Idle | Phase::Failed) && serving.model.as_deref() == Some(model.as_str()))
                    .map(|(label, _)| label),
                "tt_kernel_hint": "tt-kernel pull <namespace/model>",
            })
        })
        .await
        .unwrap_or_else(|_| json!({ "ready": false, "bundles": [] })),
    )
}

/// `POST /vllm/native/setup`: install the vLLM fork and the TT plugin into
/// tt-metal's environment with tt-kernel's own installer (minutes; it changes
/// that environment's packages, so not while a bring-up or Orchard uses it).
pub(crate) async fn post_setup(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    if state.bringup_busy() || state.orchard_busy() {
        return Err(err(
            StatusCode::CONFLICT,
            "A bring-up is running. This setup can happen once it ends.",
        ));
    }
    let script = tt_kernel_repo().join("scripts/install.sh");
    if !script.is_file() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("tt-kernel's installer isn't here ({}).", script.display()),
        ));
    }
    if unit_active(SETUP_UNIT) {
        return Ok(Json(json!({ "setting_up": true })));
    }
    let dir = state.config_dir().join("vllm-native");
    std::fs::create_dir_all(&dir)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let log = dir.join("setup.log");
    let exit = dir.join("setup.exit");
    let _ = std::fs::remove_file(&exit);
    let _ = std::fs::write(&log, "");
    let q = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let cmd = format!(
        "bash {} --venv {} --vllm-dir {}; echo $? > {}",
        q(&script),
        q(&tt_metal().join("python_env")),
        q(&home().join("dispatch/vllm")),
        q(&exit)
    );
    let path = format!(
        "{}:{}",
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={SETUP_UNIT}"))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"))
        .arg(format!("--setenv=TT_METAL_HOME={}", tt_metal().display()))
        .args(["sh", "-c", &cmd])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    crate::audit::record(&state, "host_run", "native vLLM setup", "ok");
    forget_stack();
    Ok(Json(json!({ "setting_up": true })))
}

/// What native vLLM last started: (bundle id, the model it serves).
static NATIVE_SERVING: Mutex<Option<(String, String)>> = Mutex::new(None);

/// A launch: the command line, and the environment it runs with.
type Launch = (Vec<String>, Vec<(String, String)>);

/// The bundle's launch command, made to run here: tt-metal's Python, the
/// serving port, everything else as the bundle says.
pub fn serve_argv(meta: &Value, port: u16) -> Option<Launch> {
    let l = launch(meta)?;
    let mut argv: Vec<String> = l["command"]
        .as_array()?
        .iter()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    if argv
        .first()
        .is_some_and(|a| a == "python" || a == "python3")
    {
        argv[0] = python().display().to_string();
    }
    match argv.iter().position(|a| a == "--port") {
        Some(i) if i + 1 < argv.len() => argv[i + 1] = port.to_string(),
        _ => argv.extend(["--port".into(), port.to_string()]),
    }
    // Cached prompt tokens in each reply's usage (see `community::extra_vllm_args`;
    // the tt-metal vLLM this runs, merged from upstream 0.17, has the flag).
    let vllm = argv
        .iter()
        .any(|a| a == "vllm.entrypoints.openai.api_server")
        || argv
            .windows(2)
            .any(|w| w[0].ends_with("vllm") && w[1] == "serve");
    if vllm && !argv.iter().any(|a| a == "--enable-prompt-tokens-details") {
        argv.push("--enable-prompt-tokens-details".into());
    }
    let env = l["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string()),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Some((argv, env))
}

#[derive(Debug, Deserialize, Default)]
pub struct ServeBody {
    /// The bundle's folder, as `GET /vllm/native` lists it.
    pub dir: String,
    #[serde(default)]
    pub stop_serving: bool,
}

/// `POST /vllm/native/serve`: serve a bundle on the chips with native vLLM.
pub(crate) async fn post_serve(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<ServeBody>,
) -> Reply {
    let dir = PathBuf::from(&body.dir);
    // Only bundles the box lists, not any folder a caller names.
    let known = {
        let st = state.clone();
        tokio::task::spawn_blocking(move || bundles(&st))
            .await
            .unwrap_or_default()
    };
    let Some(bundle) = known
        .into_iter()
        .find(|b| b["dir"].as_str() == Some(body.dir.as_str()))
    else {
        return Err(err(StatusCode::NOT_FOUND, "No such bundle on this box."));
    };
    if let Some(why) = bundle["why_not"].as_str() {
        return Err(err(StatusCode::CONFLICT, why.to_string()));
    }
    serve_dir(
        &state,
        &dir,
        bundle["id"].as_str().unwrap_or("the model"),
        body.stop_serving,
    )
    .await
}

/// Serve the bundle in `dir` (its id shown as `label`): free and reset the
/// chips, start vLLM as the serving unit, adopt it once it answers.
pub async fn serve_dir(state: &AppState, dir: &Path, label: &str, stop_serving: bool) -> Reply {
    let (vllm, plugin) = tokio::task::spawn_blocking(stack_ready)
        .await
        .unwrap_or((false, false));
    if !(vllm && plugin) {
        return Err(err(
            StatusCode::CONFLICT,
            "Running models from their code needs a one-time setup first. Start any \"your bring-up\" or \"community\" model in Models to set it up.",
        ));
    }
    let meta: Value = std::fs::read(dir.join("vllm_metadata.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            err(
                StatusCode::CONFLICT,
                "The bundle has no readable vllm_metadata.json.",
            )
        })?;
    let port = state.serving_port();
    let (argv, launch_env) = serve_argv(&meta, port).ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "The bundle has no launch command for this box.",
        )
    })?;
    if let Some(holder) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, holder));
    }
    let life = state.lifecycle();
    if !matches!(life.phase, Phase::Idle | Phase::Failed) || crate::coserve::is_split(state) {
        if !stop_serving {
            return Err((
                StatusCode::CONFLICT,
                Json(
                    json!({ "error": "Serving it replaces what's serving now. Stop it first?", "needs_stop": true }),
                ),
            ));
        }
        crate::coserve::leave_split(state).await;
        state.stop_serving().await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't stop the model: {e:#}"),
            )
        })?;
    }
    let reset = state.clone();
    tokio::task::spawn_blocking(move || {
        reset.run_power_command(crate::power::PowerAction::ResetChips, false)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        err(
            StatusCode::CONFLICT,
            format!("couldn't reset the chips: {e:#}"),
        )
    })?;

    let logs = state.config_dir().join("vllm-native");
    let _ = std::fs::create_dir_all(&logs);
    let log = logs.join("serve.log");
    let repo = tt_metal();
    let venv = repo.join("python_env");
    let path = format!(
        "{}:{}:{}",
        venv.join("bin").display(),
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new("systemd-run");
    cmd.args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={}", crate::orchard::HARVEST_UNIT))
        .arg(format!("--property=WorkingDirectory={}", repo.display()))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"))
        .arg(format!("--setenv=VIRTUAL_ENV={}", venv.display()))
        .arg(format!("--setenv=TT_METAL_HOME={}", repo.display()))
        .arg(format!("--setenv=PYTHONPATH={}", repo.display()))
        .arg(format!(
            "--setenv=EXTRA_MODELS_DIR={}",
            dir.parent().unwrap_or(dir).display()
        ))
        .arg("--setenv=VLLM_USE_V1=1")
        .arg("--setenv=PYTHONUNBUFFERED=1");
    if let Some(token) = crate::bringup::hf_token() {
        // The value from systemd-run's environment, not its command line.
        cmd.arg("--setenv=HF_TOKEN").env("HF_TOKEN", token);
    }
    for (k, v) in &launch_env {
        cmd.arg(format!("--setenv={k}={v}"));
    }
    let out = cmd
        .args(&argv)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "couldn't start it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let model = meta["hf_weights"].as_str().unwrap_or(label).to_string();
    *NATIVE_SERVING.lock().expect("native") = Some((label.to_string(), model.clone()));
    crate::orchard::adopt_when_serving(state, port, model);
    Ok(Json(
        json!({ "serving": label, "log": log.display().to_string() }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_runs_here_on_the_serving_port() {
        let meta = json!({"launch": {
            "default": {"command": ["python3", "-m", "x"], "env": {}},
            "blackhole": {"command": ["python", "-m", "vllm.entrypoints.openai.api_server", "--model", "g/m", "--port", "8000"],
                          "env": {"MESH_DEVICE": "P150x4"}},
        }});
        let (argv, env) = serve_argv(&meta, 8123).unwrap();
        assert!(argv[0].ends_with("python_env/bin/python"));
        assert_eq!(
            &argv[argv.len() - 3..],
            ["--port", "8123", "--enable-prompt-tokens-details"]
        );
        assert!(argv.contains(&"g/m".to_string()));
        assert_eq!(env, vec![("MESH_DEVICE".to_string(), "P150x4".to_string())]);
        // No --port in the bundle: added.
        let bare = json!({"launch": {"default": {"command": ["python", "-m", "y"]}}});
        assert_eq!(
            &serve_argv(&bare, 9).unwrap().0[3..],
            ["--port", "9"],
            "not vLLM: no vLLM flags"
        );
        let serve = json!({"launch": {"default": {"command": ["vllm", "serve", "g/m", "--enable-prompt-tokens-details"]}}});
        assert_eq!(
            serve_argv(&serve, 9)
                .unwrap()
                .0
                .iter()
                .filter(|a| *a == "--enable-prompt-tokens-details")
                .count(),
            1,
            "not twice"
        );
        assert!(serve_argv(&json!({}), 9).is_none());
    }
}
