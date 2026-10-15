// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Skill packs: the agent skills trending on GitHub (SKILL.md folders) that
//! make the coding and assistant agents on the box better at their jobs. The
//! apps show how to add each to each agent; the ones Hermes Agent installs
//! itself (`hermes plugins install <owner/repo> --enable`) are one click here.
//!
//! - `GET /skills`: whether Hermes is on the box, and which packs it has.
//! - `POST /skills/{id}/hermes`: install a pack into Hermes.

use std::process::{Command, Stdio};
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth};

/// (id, the `owner/repo` Hermes installs as a plugin)
const HERMES_PLUGINS: &[(&str, &str)] = &[("superpowers", "obra/superpowers")];

fn hermes() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    [
        home.join(".local/bin/hermes"),
        home.join(".hermes/hermes-agent/venv/bin/hermes"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn installed_plugins(bin: &std::path::Path) -> String {
    Command::new(bin)
        .args(["plugins", "list", "--plain", "--no-bundled"])
        .stdin(Stdio::null())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase())
        .unwrap_or_default()
}

/// `GET /skills`.
pub(crate) async fn get_skills(State(_state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let v = tokio::task::spawn_blocking(|| {
        let Some(bin) = hermes() else {
            return json!({ "hermes": false, "in_hermes": [] });
        };
        let list = installed_plugins(&bin);
        let have: Vec<&str> = HERMES_PLUGINS
            .iter()
            .filter(|(id, _)| list.contains(id))
            .map(|(id, _)| *id)
            .collect();
        json!({ "hermes": true, "in_hermes": have })
    })
    .await
    .unwrap_or_else(|_| json!({ "hermes": false, "in_hermes": [] }));
    Json(v)
}

/// `POST /skills/{id}/hermes`.
pub(crate) async fn post_hermes(
    State(_state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let err = |s: StatusCode, m: String| (s, Json(json!({ "error": m })));
    let (_, repo) = HERMES_PLUGINS
        .iter()
        .find(|(i, _)| *i == id)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                "Hermes can't install that pack itself; use the commands shown.".into(),
            )
        })?;
    let bin = hermes().ok_or_else(|| {
        err(
            StatusCode::CONFLICT,
            "Hermes Agent isn't installed on this box. Set it up in Connect → Agents first.".into(),
        )
    })?;
    let repo = repo.to_string();
    let out = tokio::time::timeout(
        Duration::from_secs(300),
        tokio::task::spawn_blocking(move || {
            Command::new(bin)
                .args(["plugins", "install", &repo, "--enable"])
                .stdin(Stdio::null())
                .output()
        }),
    )
    .await
    .map_err(|_| {
        err(
            StatusCode::GATEWAY_TIMEOUT,
            "Installing took too long.".into(),
        )
    })?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        let last = said
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .unwrap_or("hermes refused")
            .to_string();
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("Hermes couldn't install it: {last}"),
        ));
    }
    Ok(Json(
        json!({ "installed": id, "note": "Restart Hermes (close and open it) to load the new skills." }),
    ))
}
