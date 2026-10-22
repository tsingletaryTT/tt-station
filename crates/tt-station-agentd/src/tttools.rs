// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The rest of the official `tt` CLI, surfaced for the owner:
//!
//! - **Software** (`GET /software`): tt itself and its managed tools (tt-smi,
//!   tt-flash, tt-inference-server, tt-model, tt-installer) against the golden
//!   versions, what `tt update` would change -- including a warning when its
//!   system stack would *downgrade* the installed driver -- and leftovers of
//!   old installs worth deleting. `POST /software/cli-update` upgrades tt.
//! - **Removing models** (`DELETE /tt/models/{name}`): `tt model rm`, weights
//!   included, to free disk.
//! - **Apps for the whole household** (`/clients`): `tt launch` runs Open WebUI
//!   or AnythingLLM as a container wired to the served chat model, so anyone on
//!   the network can chat from a browser -- no app, no pairing.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth};
use crate::ttcli::TtCli;

/// Browser apps `tt launch` can run, and the host port each gets (3000 is
/// TT-Studio's).
pub const WEB_CLIENTS: [(&str, &str, u16); 2] = [
    ("openwebui", "Open WebUI", 3010),
    ("anythingllm", "AnythingLLM", 3011),
];

fn tt(state: &AppState) -> Result<Arc<TtCli>, ApiError> {
    state
        .tt_cli()
        .ok_or_else(|| err(StatusCode::CONFLICT, "This box doesn't have the tt CLI."))
}

/// The KMD version in `tt update`'s system-stack target ("... kmd 2.10.0, ...").
pub fn target_kmd(target: &str) -> Option<String> {
    target
        .split(',')
        .map(str::trim)
        .find_map(|part| part.strip_prefix("kmd "))
        .map(|v| v.trim().to_string())
}

/// Dotted-version comparison: `a < b`.
pub fn version_lt(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim_start_matches(|c: char| !c.is_ascii_digit())
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|p| p.parse().ok())
            .collect()
    };
    parse(a) < parse(b)
}

/// Assemble the software report from tt's three answers and the installed
/// driver version.
pub fn software_report(
    tools: &Value,
    check: &Value,
    plan: &Value,
    driver: Option<&str>,
    leftovers: Vec<(String, u64)>,
) -> Value {
    let tools: Vec<Value> = tools["tools"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|t| {
                    let installed = t["installed_version"].as_str().unwrap_or("");
                    let golden = t["golden_version"].as_str().unwrap_or("");
                    let short = |v: &str| {
                        if v.len() > 12 {
                            v[..8].to_string()
                        } else {
                            v.to_string()
                        }
                    };
                    json!({
                        "name": t["name"],
                        "installed": short(installed),
                        "golden": short(golden),
                        "up_to_date": !installed.is_empty() && installed == golden,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let system = plan["items"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["kind"] == "system"))
        .map(|item| {
            let target = item["target"].as_str().unwrap_or("").to_string();
            let installed_kmd = driver.map(|d| d.trim_start_matches("TT-KMD").trim().to_string());
            let kmd = target_kmd(&target);
            let downgrade = matches!((&installed_kmd, &kmd), (Some(i), Some(t)) if version_lt(t, i));
            json!({
                "target": target,
                "installed_kmd": installed_kmd,
                "target_kmd": kmd,
                "downgrade": downgrade,
                "advice": if downgrade {
                    "tt update would install an older driver than this box has; skip the system stack until tt catches up."
                } else {
                    "Run `tt update` on the box to bring the driver, firmware and system packages to Tenstorrent's tested set."
                },
            })
        });
    let tool_updates = plan["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| i["kind"] != "system" && i["action"] != "up-to-date")
                .map(|i| json!({ "name": i["name"], "current": i["current"], "target": i["target"] }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "tt": {
            "current": check["current"],
            "latest": check["latest"],
            "newer": check["newer"].as_bool().unwrap_or(false),
        },
        "tools": tools,
        "tool_updates": tool_updates,
        "system": system,
        "firmware_target": plan["firmware"],
        "leftovers": leftovers.into_iter().map(|(path, bytes)| json!({ "path": path, "bytes": bytes })).collect::<Vec<_>>(),
    })
}

fn dir_bytes(path: &std::path::Path) -> u64 {
    std::process::Command::new("du")
        .args(["-sb"])
        .arg(path)
        .output()
        .ok()
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// `GET /software` (authed).
pub(crate) async fn get_software(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, ApiError> {
    let tt = tt(&state)?;
    let driver = state
        .cached_snapshot()
        .await
        .ok()
        .and_then(|s| crate::doctor::parse_tt_smi(&s))
        .and_then(|(_, d)| d);
    let report = tokio::task::spawn_blocking(move || {
        let tools = tt.run_json(&["self", "tools"]).unwrap_or(Value::Null);
        let check = tt
            .run_json(&["self", "check-update"])
            .unwrap_or(Value::Null);
        let plan = tt.run_json(&["update", "--dry-run"]).unwrap_or(Value::Null);
        let home = std::env::var("HOME").unwrap_or_default();
        // Old installer copies `tt` itself says it doesn't use.
        let leftovers = ["tt-inference-server", "tt-studio"]
            .iter()
            .map(|name| {
                std::path::PathBuf::from(&home)
                    .join(".local/lib")
                    .join(name)
            })
            .filter(|p| p.is_dir())
            .map(|p| {
                let bytes = dir_bytes(&p);
                (p.display().to_string(), bytes)
            })
            .collect();
        software_report(&tools, &check, &plan, driver.as_deref(), leftovers)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

#[derive(Debug, Deserialize, Default)]
pub struct CliUpdateBody {
    /// Only report what the update would do.
    #[serde(default)]
    pub dry_run: bool,
}

/// Package names pip's dry run would install: `Would install a-1.0 b-2.1`.
pub fn would_install(pip_output: &str) -> Vec<String> {
    pip_output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Would install "))
        .map(|rest| rest.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn pip_name(pkg_ver: &str) -> &str {
    pkg_ver.rsplit_once('-').map_or(pkg_ver, |(n, _)| n)
}

/// `POST /software/cli-update` (authed). Where `tt` owns its environment this is
/// `tt self update -y`. In a shared virtualenv (this box's `~/.tenstorrent-venv`,
/// which also holds ttnn, vllm, tt-smi…) `tt` refuses and prints a pip command
/// instead; then it's `pip install --upgrade tenstorrent`, but only if pip's own
/// dry run says nothing besides `tenstorrent` would change. Anything more is
/// returned (`blocked`, with the command) for the person to run. `dry_run`
/// returns the plan without changing anything. `tt` runs as a fresh process
/// every time, so no restart is needed: the next call uses the new version.
pub(crate) async fn post_cli_update(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    body: Option<Json<CliUpdateBody>>,
) -> Result<Json<Value>, ApiError> {
    let dry_run = body.is_some_and(|b| b.dry_run);
    let tt = tt(&state)?;
    if !dry_run
        && !matches!(
            state.lifecycle().phase,
            crate::routes::Phase::Idle
                | crate::routes::Phase::Serving
                | crate::routes::Phase::Failed
        )
    {
        return Err(err(
            StatusCode::CONFLICT,
            "A model is starting or stopping, and that uses tt. Update once it's done.",
        ));
    }
    if !dry_run {
        crate::audit::record(&state, "update_tt_cli", "", "started");
    }
    tokio::task::spawn_blocking(move || {
        let check = tt.run_json(&["self", "update", "--check"]).map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
        let (current, latest) = (check["current"].as_str().map(str::to_string), check["latest"].as_str().map(str::to_string));
        if check["status"] != "available" {
            return Ok(Json(json!({ "mode": "none", "current": current, "latest": latest, "updated": false, "message": "tt is up to date." })));
        }
        // tt owns its environment (a `uv tool` or pipx install): it updates itself.
        let shared = check["hint"].as_str().is_some_and(|h| h.contains("pip install"));
        if !shared {
            if dry_run {
                return Ok(Json(json!({ "mode": "tt", "current": current, "latest": latest, "would_install": ["tenstorrent"], "blocked": false })));
            }
            let out = tt.run_json(&["self", "update", "-y"]).map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
            return Ok(Json(json!({ "mode": "tt", "updated": true, "current": current, "latest": latest, "result": out })));
        }
        let Some(py) = tt.venv_python() else {
            return Err(err(StatusCode::CONFLICT, "tt is in a shared environment this app can't find the Python of."));
        };
        let command = format!("{} -m pip install --upgrade tenstorrent", py.display());
        let pip = |extra: &[&str]| {
            std::process::Command::new(&py)
                .args(["-m", "pip", "install", "--upgrade", "--disable-pip-version-check"])
                .args(extra)
                .arg("tenstorrent")
                .stdin(std::process::Stdio::null())
                .output()
        };
        let plan = pip(&["--dry-run"]).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !plan.status.success() {
            let said = String::from_utf8_lossy(&plan.stderr);
            return Err(err(StatusCode::BAD_GATEWAY, format!("pip couldn't plan the update: {}", said.lines().last().unwrap_or("").trim())));
        }
        let would = would_install(&String::from_utf8_lossy(&plan.stdout));
        let others: Vec<String> = would.iter().filter(|p| pip_name(p) != "tenstorrent").cloned().collect();
        let blocked = !others.is_empty();
        let base = json!({ "mode": "pip", "current": current, "latest": latest, "would_install": would, "others": others, "blocked": blocked, "command": command });
        if dry_run || blocked {
            return Ok(Json(base));
        }
        let done = pip(&[]).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if !done.status.success() {
            let said = String::from_utf8_lossy(&done.stderr);
            return Err(err(StatusCode::BAD_GATEWAY, format!("The update didn't finish: {}", said.lines().last().unwrap_or("").trim())));
        }
        tt.invalidate_catalog();
        let now = tt.version();
        let mut out = base;
        out["updated"] = json!(true);
        out["version_now"] = json!(now);
        Ok(Json(out))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
}

#[derive(Debug, Deserialize)]
pub struct RemoveQuery {
    /// Also delete the downloaded weights (the space-saving part).
    #[serde(default)]
    pub weights: bool,
}

/// `DELETE /tt/models/{name}?weights=true` (authed): `tt model rm`. Refused for
/// the model that's serving.
pub(crate) async fn delete_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(name): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<RemoveQuery>,
) -> Result<Json<Value>, ApiError> {
    let tt = tt(&state)?;
    let short = |m: &str| m.rsplit('/').next().unwrap_or(m).to_lowercase();
    let life = state.lifecycle();
    let busy = [life.model, life.served_id, state.current_model()]
        .into_iter()
        .flatten()
        .any(|m| short(&m) == short(&name));
    if busy {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{name} is in use. Stop it first, then remove it."),
        ));
    }
    let weights = q.weights;
    let result = tokio::task::spawn_blocking(move || {
        let mut args = vec!["model", "rm", name.as_str(), "-y"];
        if weights {
            args.push("--include-weights");
        }
        let out = tt.run_json(&args);
        tt.invalidate_catalog();
        out
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({
        "removed": result["model"],
        "freed_bytes": result["total_bytes"].as_u64().unwrap_or(0)
            + if weights { result["weights_bytes"].as_u64().or(result["weights_kept_bytes"].as_u64()).unwrap_or(0) } else { 0 },
        "result": result,
    })))
}

/// The address a phone can open a web client at.
fn client_url(state: &AppState, port: u16) -> String {
    let host = crate::pairqr::lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| format!("{}.local", state.name()));
    format!("http://{host}:{port}")
}

/// `GET /clients` (authed): the browser apps tt can run, and which are running.
pub(crate) async fn get_clients(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, ApiError> {
    let tt = tt(&state)?;
    let list = tokio::task::spawn_blocking(move || tt.run_json(&["launch", "list"]))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let state_of = |tool: &str| {
        list.as_array()
            .and_then(|a| a.iter().find(|c| c["tool"] == tool).cloned())
            .unwrap_or(Value::Null)
    };
    let clients: Vec<Value> = WEB_CLIENTS
        .iter()
        .map(|(tool, label, port)| {
            let entry = state_of(tool);
            let running = entry["container_state"].as_str() == Some("running");
            json!({
                "tool": tool,
                "name": label,
                "available": entry["available"].as_bool().unwrap_or(false),
                "running": running,
                "state": entry["container_state"],
                "url": running.then(|| client_url(&state, *port)),
            })
        })
        .collect();
    Ok(Json(json!({ "clients": clients })))
}

/// `POST /clients/{tool}/start` (authed): run a browser app wired to the chat
/// model. Answers its URL.
pub(crate) async fn start_client(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(tool): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tt = tt(&state)?;
    let Some((_, label, port)) = WEB_CLIENTS.iter().find(|(t, _, _)| *t == tool).copied() else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("{tool} isn't a browser app tt can run."),
        ));
    };
    let endpoint = crate::gateway::chat_endpoint(&state).await.ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            format!("{label} needs a chat model serving. Start one first."),
        )
    })?;
    let url = endpoint.base_url.trim_end_matches('/').to_string();
    let web_port = port.to_string();
    let tool_arg = tool.clone();
    tokio::task::spawn_blocking(move || {
        tt.run_json(&[
            "launch",
            &tool_arg,
            "-y",
            "--force",
            "--url",
            &url,
            "--web-port",
            &web_port,
        ])
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(
        json!({ "tool": tool, "name": label, "url": client_url(&state, port) }),
    ))
}

/// `POST /clients/{tool}/stop` (authed): `tt launch stop` (keeps its data).
pub(crate) async fn stop_client(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(tool): axum::extract::Path<String>,
) -> Result<Json<Value>, ApiError> {
    let tt = tt(&state)?;
    if !WEB_CLIENTS.iter().any(|(t, _, _)| *t == tool) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            format!("{tool} isn't a browser app tt runs."),
        ));
    }
    tokio::task::spawn_blocking(move || tt.run_json(&["launch", "stop", &tool]))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_pips_dry_run() {
        let out =
            "Requirement already satisfied: idna\nWould install tenstorrent-1.1.0 tomlkit-0.14.0\n";
        assert_eq!(
            super::would_install(out),
            ["tenstorrent-1.1.0", "tomlkit-0.14.0"]
        );
        assert_eq!(super::pip_name("tenstorrent-1.1.0"), "tenstorrent");
        assert_eq!(
            super::pip_name("opentelemetry-sdk-1.2.3"),
            "opentelemetry-sdk"
        );
        assert!(super::would_install("Requirement already satisfied: x").is_empty());
    }

    use super::*;

    #[test]
    fn spots_a_driver_downgrade_in_the_update_plan() {
        let target =
            "hugepages 1.4.1, kmd 2.10.0, metal-version v0.75.0, sfpi 7.67.0, firmware 19.13.1";
        assert_eq!(target_kmd(target).as_deref(), Some("2.10.0"));
        assert!(version_lt("2.10.0", "2.11.0"));
        assert!(!version_lt("2.11.0", "2.11.0"));
        assert!(version_lt("v0.9.8", "v0.18.0"));

        let tools = json!({ "tools": [
            { "name": "tt-smi", "installed_version": "6.1.0", "golden_version": "6.1.0" },
            { "name": "tt-model", "installed_version": "019abcb143d369450436c2778b9b40267d1e6903", "golden_version": "aaaaaaaa1111" }
        ]});
        let check = json!({ "current": "1.0.1", "latest": "1.0.2", "newer": true });
        let plan = json!({ "items": [
            { "name": "tt-smi", "kind": "uv-tool", "current": "6.1.0", "target": "6.1.0", "action": "up-to-date" },
            { "name": "tt-flash", "kind": "uv-tool", "current": "3.9.0", "target": "3.10.0", "action": "upgrade" },
            { "name": "system-stack", "kind": "system", "target": target, "action": "converge" }
        ], "firmware": "19.13.1" });
        let r = software_report(
            &tools,
            &check,
            &plan,
            Some("TT-KMD 2.11.0"),
            vec![("/x".into(), 5)],
        );
        assert_eq!(r["tt"]["newer"], true);
        assert_eq!(r["tools"][0]["up_to_date"], true);
        assert_eq!(r["tools"][1]["up_to_date"], false);
        assert_eq!(r["tools"][1]["installed"], "019abcb1");
        assert_eq!(r["tool_updates"].as_array().unwrap().len(), 1);
        assert_eq!(r["system"]["downgrade"], true);
        assert_eq!(r["system"]["installed_kmd"], "2.11.0");
        assert_eq!(r["leftovers"][0]["bytes"], 5);
    }
}
