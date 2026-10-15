// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Orchard's lab box (tt-orchard `--lab`): every hardware test of a run goes
//! to a second box over ssh, while the supervisor and the coder model stay on
//! this one and are never parked.
//!
//! tt-orchard reads the lab from the `[lab]` table of its `bringup.toml`;
//! TT Station writes that table (its lines marked, so it only ever removes its
//! own), runs `tt-orchard lab setup`'s checks, and can run its fixes.
//!
//! - `GET /orchard/lab`: the lab, and the last setup run.
//! - `PUT /orchard/lab`: set it (`{host, root, path, python, test_python, gozer}`) or `{remove: true}`.
//! - `POST /orchard/lab/check`: `tt-orchard lab setup --check`, as JSON.
//! - `POST /orchard/lab/setup`: `tt-orchard lab setup --yes` in the background.
//!
//! Setting the lab, its checks and its setup are an owner's: they run
//! tt-orchard (and ssh) as the box's own account.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::orchard::{blocking, need, Orchard};
use crate::routes::{AppState, BearerAuth};

type Reply = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

const MARK: &str = "# set by tt-station";
const SETUP_UNIT: &str = "tt-orchard-lab-setup";

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

#[derive(Debug, Deserialize, Default)]
pub struct LabBody {
    #[serde(default)]
    pub remove: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub path: Vec<String>,
    #[serde(default)]
    pub python: Option<String>,
    #[serde(default)]
    pub test_python: Option<String>,
    #[serde(default)]
    pub gozer: Option<String>,
}

/// `[user@]host`, as tt-orchard accepts it.
fn valid_host(h: &str) -> bool {
    let (user, host) = h.split_once('@').unwrap_or(("", h));
    let part = |s: &str| {
        !s.is_empty()
            && s.len() <= 64
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (user.is_empty() || part(user)) && part(host) && !host.starts_with('-')
}

/// A value safe to put in one TOML string.
fn plain(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && !s.contains(['\n', '\r', '"', '\\'])
}

impl LabBody {
    fn check(&self) -> Result<(), String> {
        if !valid_host(self.host.trim()) {
            return Err("The lab's ssh host should look like node4 or user@node4.".into());
        }
        let root = self.root.trim();
        if !plain(root) || !Path::new(root).is_absolute() {
            return Err("The lab root must be an absolute path, the same on both boxes (like /srv/orchard).".into());
        }
        for p in &self.path {
            if !plain(p.trim()) {
                return Err(format!("{p:?} isn't a directory path."));
            }
        }
        for v in [&self.python, &self.test_python, &self.gozer]
            .into_iter()
            .flatten()
        {
            if !v.trim().is_empty() && !plain(v.trim()) {
                return Err(format!("{v:?} isn't a usable command or path."));
            }
        }
        Ok(())
    }
}

/// `text` with TT Station's `[lab]` table replaced (or removed with `None`).
/// Refuses a `[lab]` table someone wrote by hand: that one is theirs to edit.
pub fn set_lab(text: &str, lab: Option<&LabBody>) -> Result<String, String> {
    let hand_written = text.lines().any(|l| {
        l.trim() == "[lab]" || (l.trim_start().starts_with("[lab]") && !l.ends_with(MARK))
    });
    if hand_written {
        return Err(
            "bringup.toml already has a [lab] table written by hand; edit it there.".into(),
        );
    }
    let mut out: Vec<String> = text
        .lines()
        .filter(|l| !l.ends_with(MARK))
        .map(str::to_string)
        .collect();
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    if let Some(lab) = lab {
        let q = |s: &str| serde_json::to_string(s.trim()).unwrap_or_default();
        out.push(String::new());
        out.push(format!("[lab] {MARK}"));
        out.push(format!("host = {} {MARK}", q(&lab.host)));
        out.push(format!("root = {} {MARK}", q(&lab.root)));
        let path: Vec<String> = lab
            .path
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(q)
            .collect();
        if !path.is_empty() {
            out.push(format!("path = [{}] {MARK}", path.join(", ")));
        }
        for (key, v) in [
            ("python", &lab.python),
            ("test_python", &lab.test_python),
            ("gozer", &lab.gozer),
        ] {
            if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                out.push(format!("{key} = {} {MARK}", q(v)));
            }
        }
    }
    Ok(out.join("\n") + "\n")
}

fn config_path(o: &Orchard) -> Option<PathBuf> {
    o.info_blocking(true)["config"]["path"]
        .as_str()
        .map(PathBuf::from)
}

fn setup_dir(o: &Orchard) -> PathBuf {
    o.log_path("lab-setup")
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

fn unit_active(unit: &str) -> bool {
    std::process::Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", unit])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `GET /orchard/lab`.
pub(crate) async fn get_lab(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Reply {
    let o = need(&state)?;
    let v = blocking(move || {
        let info = o.info_blocking(false);
        let dir = setup_dir(&o);
        let log = crate::orchard::tail(&dir.join("lab-setup.log"), 40);
        let exit = std::fs::read_to_string(dir.join("lab-setup.exit"))
            .ok()
            .map(|t| t.trim().to_string());
        json!({
            "lab": info["config"]["lab"],
            "packaging": info["config"]["package_format"].is_string(),
            "config_path": info["config"]["path"],
            "setup": { "running": unit_active(SETUP_UNIT), "log": log, "exit": exit },
        })
    })
    .await?;
    Ok((StatusCode::OK, Json(v)))
}

/// `PUT /orchard/lab`.
pub(crate) async fn put_lab(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(body): Json<LabBody>,
) -> Reply {
    if !body.remove {
        body.check().map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    }
    let o = need(&state)?;
    blocking(move || {
        let info = o.info_blocking(true);
        if !body.remove && info["config"]["package_format"].is_string() {
            return Err(err(
                StatusCode::CONFLICT,
                "Turn packaging off first: tt-orchard can't package a run whose tests run on a lab yet.",
            ));
        }
        let path = config_path(&o).ok_or_else(|| err(StatusCode::PRECONDITION_FAILED, "tt-orchard has no bringup config yet."))?;
        let text = std::fs::read_to_string(&path).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let next = set_lab(&text, (!body.remove).then_some(&body)).map_err(|e| err(StatusCode::CONFLICT, e))?;
        std::fs::write(&path, next).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let after = o.info_blocking(true);
        if let Some(e) = after["config_error"].as_str() {
            let _ = std::fs::write(&path, text); // put it back rather than leave tt-orchard unable to start
            return Err(err(StatusCode::UNPROCESSABLE_ENTITY, format!("tt-orchard refused it: {e}")));
        }
        Ok((StatusCode::OK, Json(json!({ "lab": after["config"]["lab"] }))))
    })
    .await?
}

/// `POST /orchard/lab/check`: every check `tt-orchard lab setup --check` makes.
pub(crate) async fn post_check(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    let o = need(&state)?;
    let v = tokio::time::timeout(
        Duration::from_secs(300),
        blocking(move || o.bridge(&["lab-check"])),
    )
    .await
    .map_err(|_| {
        err(
            StatusCode::GATEWAY_TIMEOUT,
            "Checking the lab took too long (is it reachable over ssh?).",
        )
    })??
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    if let Some(e) = v["error"].as_str() {
        return Err(err(StatusCode::CONFLICT, e.to_string()));
    }
    Ok((StatusCode::OK, Json(v)))
}

/// `POST /orchard/lab/setup`: `tt-orchard lab setup --yes` (it never runs sudo
/// and never touches a lease; what it can't fix, it says how to).
pub(crate) async fn post_setup(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    if unit_active(SETUP_UNIT) {
        return Ok((StatusCode::OK, Json(json!({ "running": true }))));
    }
    let o = need(&state)?;
    blocking(move || {
        let dir = setup_dir(&o);
        let _ = std::fs::create_dir_all(&dir);
        let (log, exit) = (dir.join("lab-setup.log"), dir.join("lab-setup.exit"));
        let _ = std::fs::write(&log, "");
        let _ = std::fs::remove_file(&exit);
        let q = |s: String| format!("'{}'", s.replace('\'', "'\\''"));
        let script = format!(
            "cd {} && {} {} lab setup --yes; echo $? > {}",
            q(o.checkout.display().to_string()),
            q(o.python.display().to_string()),
            q(o.checkout.join("bin/tt-orchard").display().to_string()),
            q(exit.display().to_string())
        );
        o.spawn(SETUP_UNIT, &log, &["sh".into(), "-c".into(), script])
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        Ok((StatusCode::ACCEPTED, Json(json!({ "running": true }))))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab() -> LabBody {
        LabBody {
            host: "node4".into(),
            root: "/srv/orchard".into(),
            path: vec!["~/.local/bin".into()],
            ..Default::default()
        }
    }

    #[test]
    fn writes_and_removes_its_own_lab_table() {
        let cfg = "runs_root = \"/srv/orchard/runs\"\n\n[env]\nHF_HUB_OFFLINE = \"1\"\n";
        let on = set_lab(cfg, Some(&lab())).unwrap();
        assert!(on.contains("[lab] # set by tt-station"));
        assert!(on.contains("host = \"node4\" # set by tt-station"));
        assert!(on.contains("path = [\"~/.local/bin\"] # set by tt-station"));
        assert!(on.find("[env]").unwrap() < on.find("[lab]").unwrap());
        let off = set_lab(&on, None).unwrap();
        assert_eq!(off.trim_end(), cfg.trim_end());
    }

    #[test]
    fn leaves_a_hand_written_lab_alone() {
        assert!(set_lab("[lab]\nhost = \"x\"\n", Some(&lab())).is_err());
    }

    #[test]
    fn checks_what_it_writes() {
        assert!(lab().check().is_ok());
        assert!(LabBody {
            host: "-oProxyCommand=x".into(),
            ..lab()
        }
        .check()
        .is_err());
        assert!(LabBody {
            root: "srv/orchard".into(),
            ..lab()
        }
        .check()
        .is_err());
        assert!(LabBody {
            root: "/srv/\"x".into(),
            ..lab()
        }
        .check()
        .is_err());
        assert!(valid_host("me@node4"));
    }
}
