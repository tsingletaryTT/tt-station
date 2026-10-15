// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Hermes Agent (Nous Research) on the box: install it with Nous's own
//! installer, point it at the box's models, and open its desktop app.
//!
//! Unlike Connect's other apps it isn't a container: Hermes installs into
//! `~/.hermes` and ships a native desktop app (`hermes desktop`). TT Station
//! doesn't configure it behind the person's back: it guides them through
//! Hermes's own first-run setup ("I have an API key" → "Local / custom
//! endpoint"), handing them the box's gateway (`/v1`) and an app key, and
//! watches for Hermes to start using the box.
//!
//! - `GET /hermes`: installed? installing? configured? what model.
//! - `POST /hermes/install`: run Nous's installer in the background (a pinned
//!   version, checked against its sha256 before it runs).
//! - `POST /hermes/key`: the box's address and an app key, to paste into Hermes.
//! - `POST /hermes/open`: open the desktop app; its own setup takes it from there.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, Phase};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// Nous's installer, pinned (2026-10-15): `scripts/install.sh` at this commit of
/// NousResearch/hermes-agent -- byte for byte what
/// hermes-agent.nousresearch.com/install.sh served that day -- and the same
/// commit for the checkout it makes (`--commit`). The download is checked
/// against [`INSTALLER_SHA256`] before anything runs; a mismatch is refused.
/// To bump: pick a commit on `main`, fetch its `scripts/install.sh`, read it,
/// and put the commit and the file's `sha256sum` here (see
/// docs/reference/pinned-dependencies.md). The installer still fetches its own
/// tools (uv, Python, Node packages) by version from their usual places.
const INSTALLER_COMMIT: &str = "046094df28b712f62a5e3c728a9e59da05dcbf1e";
const INSTALLER_SHA256: &str = "f4cdb0e20f1aca2c3871a76b962fd4db61510f549cf973759c753ac13349beb5";

fn installer_url() -> String {
    format!("https://raw.githubusercontent.com/NousResearch/hermes-agent/{INSTALLER_COMMIT}/scripts/install.sh")
}

/// The installer is the one this code pins, or why not.
fn verify_installer(body: &[u8], want_sha256: &str) -> Result<(), String> {
    let got: String = ring::digest::digest(&ring::digest::SHA256, body)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got.eq_ignore_ascii_case(want_sha256) {
        Ok(())
    } else {
        Err(format!(
            "The Hermes installer that downloaded isn't the one TT Station knows (sha256 {got}, expected {want_sha256}), so it wasn't run. \
             Update TT Station, or install Hermes yourself from hermes-agent.nousresearch.com."
        ))
    }
}

/// Download the pinned installer, check it, and keep it where only this
/// account can read or change it until it runs.
async fn fetch_installer(dir: &Path) -> Result<PathBuf, (StatusCode, Json<Value>)> {
    let body = reqwest::Client::new()
        .get(installer_url())
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't download the Hermes installer: {e}"),
            )
        })?
        .bytes()
        .await
        .map_err(|e| {
            err(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't download the Hermes installer: {e}"),
            )
        })?;
    verify_installer(&body, INSTALLER_SHA256).map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let path = dir.join("install.sh");
    let _ = std::fs::remove_file(&path);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(&body)
    };
    write().map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(path)
}
const INSTALL_UNIT: &str = "tt-station-hermes-install";
const DESKTOP_UNIT: &str = "tt-station-hermes-desktop";
/// Hermes refuses models with less working memory than this.
const MIN_CONTEXT: u64 = 64_000;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

fn hermes_home() -> PathBuf {
    home().join(".hermes")
}

/// The `hermes` command, where the installer publishes it.
fn hermes_bin() -> Option<PathBuf> {
    [
        home().join(".local/bin/hermes"),
        hermes_home().join("bin/hermes"),
        hermes_home().join("hermes-agent/venv/bin/hermes"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn unit_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn work_dir(state: &AppState) -> PathBuf {
    state.config_dir().join("hermes")
}

fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    // The installer redraws progress with bare CRs: keep the newest of each line.
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.rsplit('\r').next().unwrap_or(l).to_string())
        .map(|l| strip_ansi(&l))
        .filter(|l| !l.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The model the box serves now and its context length, from the server itself.
async fn serving(state: &AppState) -> Option<(String, Option<u64>)> {
    let life = state.lifecycle();
    if !matches!(life.phase, Phase::Serving) {
        return None;
    }
    let url = format!("http://127.0.0.1:{}/v1/models", state.serving_port());
    let models: Value = reqwest::Client::new()
        .get(url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let first = &models["data"][0];
    let id = first["id"]
        .as_str()
        .map(str::to_string)
        .or(life.served_id.clone())?;
    Some((id, first["max_model_len"].as_u64()))
}

/// The box's own names, for telling whether Hermes already points here.
fn box_hosts() -> Vec<String> {
    let name = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut hosts = vec!["127.0.0.1".to_string(), "localhost".into(), "[::1]".into()];
    if !name.is_empty() {
        hosts.extend([name.clone(), format!("{name}.local")]);
    }
    let ips = Command::new("hostname")
        .arg("-I")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    hosts.extend(ips.split_whitespace().map(str::to_string));
    hosts
}

/// Does this base URL reach the box's own gateway?
fn is_box(base: &str, port: u16) -> bool {
    let rest = base
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let hostport = rest.split('/').next().unwrap_or("");
    let (host, p) = hostport.rsplit_once(':').unwrap_or((hostport, ""));
    p == port.to_string() && box_hosts().iter().any(|h| h.eq_ignore_ascii_case(host))
}

/// Hermes's own settings: (provider, model, base URL), from `model:` in its config.
fn settings() -> Option<(String, String, String)> {
    let text = std::fs::read_to_string(hermes_home().join("config.yaml")).ok()?;
    let mut in_model = false;
    let (mut provider, mut model, mut base) = (None, None, None);
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue; // comments (even at the margin) don't end a section
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            in_model = line.trim_end() == "model:";
            continue;
        }
        if in_model {
            let l = line.trim();
            let value = |v: &str| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string();
            if let Some(v) = l.strip_prefix("default:") {
                model = Some(value(v));
            } else if let Some(v) = l.strip_prefix("base_url:") {
                base = Some(value(v));
            } else if let Some(v) = l.strip_prefix("provider:") {
                provider = Some(value(v));
            }
        }
    }
    Some((
        provider.unwrap_or_else(|| "auto".into()),
        model.unwrap_or_default(),
        base.unwrap_or_default(),
    ))
}

/// Is Hermes set to use this box's models (any of its addresses)?
fn uses_box(state: &AppState) -> Option<bool> {
    // Any provider (`custom`, or one the person named, like `tt-qb2`) whose
    // address is this box's gateway.
    let (_, _, base) = settings()?;
    Some(is_box(&base, state.ctrl_port()))
}

/// `GET /hermes`.
pub(crate) async fn get_hermes(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let serving = serving(&state).await;
    let st = state.clone();
    Json(
        tokio::task::spawn_blocking(move || {
            let dir = work_dir(&st);
            let installing = unit_active(INSTALL_UNIT);
            let exit = std::fs::read_to_string(dir.join("install.exit")).ok().map(|t| t.trim().to_string());
            let installed = hermes_bin().is_some() && !installing;
            let (model, context) = serving.clone().map(|(m, c)| (Some(m), c)).unwrap_or((None, None));
            json!({
                "installed": installed,
                "installing": installing,
                "install_failed": !installing && !installed && exit.as_deref().is_some_and(|e| e != "0"),
                "log": if installing || exit.as_deref().is_some_and(|e| e != "0") { tail(&dir.join("install.log"), 8) } else { Vec::new() },
                // Where Hermes gets its model: this box, or something the person chose.
                "uses_box": uses_box(&st),
                "provider": settings().map(|(p, _, _)| p),
                "configured_model": settings().map(|(_, m, _)| m).filter(|m| !m.is_empty()),
                "running": desktop_running(),
                // Hermes has used the box (its key has had requests).
                "key_requests": key_requests(&st),
                // The desktop app is built on its first open (minutes).
                "desktop_built": desktop_built(),
                "serving_model": model,
                "serving_context": context,
                // Hermes needs at least 64K of context; say so before it refuses.
                "context_too_small": context.is_some_and(|c| c < MIN_CONTEXT),
                "has_session": crate::desktop::has_session(),
            })
        })
        .await
        .unwrap_or_else(|_| json!({ "installed": false })),
    )
}

/// `POST /hermes/install`: Nous's installer, unattended (its terminal wizard is
/// skipped; the desktop app's own setup replaces it), with the desktop app.
/// Into `~/.hermes`; no sudo. The installer is a pinned version, checked
/// before it runs (see [`INSTALLER_COMMIT`]).
///
/// Owners only: it runs a downloaded program as the box's account.
pub(crate) async fn post_install(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    crate::audit::record(&state, "hermes_install", "hermes", "started");
    if unit_active(INSTALL_UNIT) {
        return Ok(Json(json!({ "installing": true })));
    }
    let dir = work_dir(&state);
    std::fs::create_dir_all(&dir)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let (log, exit) = (dir.join("install.log"), dir.join("install.exit"));
    let _ = std::fs::write(&log, "");
    let _ = std::fs::remove_file(&exit);
    let installer = fetch_installer(&dir).await?;
    let script = format!(
        "bash '{}' --commit {INSTALLER_COMMIT} --non-interactive --include-desktop; echo $? > '{}'",
        installer.display(),
        exit.display()
    );
    let path = format!(
        "{}:{}",
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={INSTALL_UNIT}"))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"))
        .arg(format!("--setenv=HOME={}", home().display()))
        .args(["bash", "-c", &script])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(Json(json!({ "installing": true })))
}

/// Hermes's own TT Station app key, as `{id, secret}` (minted once, kept here
/// so the guide can show it again while the person pastes it into Hermes).
fn stored_key(state: &AppState) -> Option<(String, String)> {
    let v: Value =
        serde_json::from_slice(&std::fs::read(work_dir(state).join("app-key.json")).ok()?).ok()?;
    let (id, secret) = (
        v["id"].as_str()?.to_string(),
        v["secret"].as_str()?.to_string(),
    );
    // Revoked in Connect since: forget it.
    state
        .app_keys()
        .list()
        .iter()
        .any(|k| k.id == id)
        .then_some((id, secret))
}

/// Requests the box has had with Hermes's key: more than none means it's connected.
fn key_requests(state: &AppState) -> Option<u64> {
    let (id, _) = stored_key(state)?;
    state
        .app_keys()
        .list()
        .iter()
        .find(|k| k.id == id)
        .map(|k| k.requests)
}

/// `POST /hermes/key`: what to paste into Hermes's "Local / custom endpoint":
/// the box's address and an app key for Hermes (the same one each time).
pub(crate) async fn post_key(State(state): State<AppState>, _auth: BearerAuth) -> Reply {
    let base = format!("http://127.0.0.1:{}/v1", state.ctrl_port());
    if let Some((_, secret)) = stored_key(&state) {
        return Ok(Json(json!({ "base_url": base, "api_key": secret })));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (summary, secret) = state
        .app_keys()
        .create("Hermes Agent", now)
        .map_err(|e| err(StatusCode::CONFLICT, e))?;
    let dir = work_dir(&state);
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("app-key.json");
    std::fs::write(
        &file,
        json!({ "id": summary.id, "secret": secret }).to_string(),
    )
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
    Ok(Json(json!({ "base_url": base, "api_key": secret })))
}

/// Has `hermes desktop` built its app yet (it builds on first launch)?
fn desktop_built() -> bool {
    let dir = hermes_home().join("hermes-agent/apps/desktop");
    ["release", "out", "dist-electron"]
        .iter()
        .any(|d| dir.join(d).is_dir())
}

/// Is Hermes Desktop open, however it was started (TT Station, the app menu)?
fn desktop_running() -> bool {
    let app = hermes_home().join("hermes-agent/apps/desktop");
    let app = app.to_string_lossy();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .chars()
                .all(|c| c.is_ascii_digit())
                && std::fs::read(e.path().join("cmdline")).is_ok_and(|c| {
                    let first = c.split(|b| *b == 0).next().unwrap_or(&[]);
                    let first = String::from_utf8_lossy(first);
                    first.starts_with(app.as_ref()) && first.ends_with("/Hermes")
                })
        })
        || unit_active(DESKTOP_UNIT)
}

/// `POST /hermes/open`: open Hermes Desktop on the box's screen. Its own
/// first-run setup takes it from there (TT Station's guide shows what to enter).
pub(crate) async fn post_open(State(state): State<AppState>, _auth: BearerAuth) -> Reply {
    let bin = hermes_bin()
        .ok_or_else(|| err(StatusCode::CONFLICT, "Hermes Agent isn't installed yet."))?;
    if !crate::desktop::has_session() {
        return Err(err(
            StatusCode::CONFLICT,
            "Nobody is signed in to the box's desktop, so there's nowhere to open Hermes.",
        ));
    }
    if desktop_running() {
        // Launching it again, the way its icon does, hands over to the running
        // copy, which restores and focuses its window (Electron's second-instance).
        let id = "com.nousresearch.hermes.desktop";
        let raised = tokio::task::spawn_blocking(move || {
            crate::scope::command("gtk-launch", &[id])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .await
        .unwrap_or(false);
        return Ok(Json(
            json!({ "opened": true, "already_open": true, "raised": raised }),
        ));
    }
    let log = work_dir(&state).join("desktop.log");
    let _ = std::fs::create_dir_all(work_dir(&state));
    let path = format!(
        "{}:{}",
        home().join(".local/bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={DESKTOP_UNIT}"))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=PATH={path}"))
        .arg(&bin)
        .arg("desktop")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "couldn't open it: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(Json(
        json!({ "opened": true, "building": !desktop_built() }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_its_own_addresses() {
        assert!(is_box("http://127.0.0.1:8765/v1", 8765));
        assert!(is_box("http://localhost:8765/v1/", 8765));
        assert!(!is_box("http://127.0.0.1:8000/v1", 8765));
        assert!(!is_box("https://openrouter.ai/api/v1", 8765));
    }

    #[test]
    fn only_the_pinned_installer_runs() {
        let body = b"#!/usr/bin/env bash\necho hermes\n";
        let sha = "c1f5b5b6a8b0e6b8b6f0e8b9c6d7c9a1f3b8e2a7d0c4b5e6f7a8b9c0d1e2f3a4";
        let real: String = ring::digest::digest(&ring::digest::SHA256, body)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(verify_installer(body, &real).is_ok());
        assert!(verify_installer(body, &real.to_uppercase()).is_ok());
        let refused = verify_installer(body, sha).unwrap_err();
        assert!(
            refused.contains("wasn't run") && refused.contains(&real),
            "{refused}"
        );
        assert!(
            verify_installer(b"#!/usr/bin/env bash\necho hermes!\n", &real).is_err(),
            "one byte changed"
        );
        // The pin itself: a full sha256, and the same commit for the file and the checkout.
        assert_eq!(INSTALLER_SHA256.len(), 64);
        assert!(INSTALLER_SHA256.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(INSTALLER_COMMIT.len(), 40);
        assert!(installer_url().contains(INSTALLER_COMMIT));
    }

    #[test]
    fn installer_output_is_readable() {
        assert_eq!(strip_ansi("\u{1b}[32m✓\u{1b}[0m uv ready"), "✓ uv ready");
    }
}
