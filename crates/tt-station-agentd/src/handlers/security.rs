// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Whether plain HTTP from the LAN is allowed: `GET`/`POST /security`.

use crate::auth::{lan_http_allowed, LocalCaller, OwnerAuth};
use crate::error::{tt_error, ErrorResponse};
use crate::state::AppState;
use axum::{http::StatusCode, Json};
use serde::Deserialize;

/// `GET /security` (the box itself): how the box is reachable.
pub(crate) async fn get_security(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "lan_http": lan_http_allowed(&state),
        "https_port": state.ctrl_port() + 1,
        "tls": state.tls(),
    }))
}

#[derive(Deserialize)]
pub(crate) struct SecurityBody {
    lan_http: bool,
}

/// `POST /security` (the box itself): allow or forbid plain HTTP for other
/// computers' apps on `/v1` (phones always use HTTPS).
pub(crate) async fn post_security(
    axum::extract::State(state): axum::extract::State<AppState>,
    caller: Option<axum::extract::Extension<LocalCaller>>,
    _owner: OwnerAuth,
    Json(body): Json<SecurityBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if caller.is_none() {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "Change this from TT Station on the box itself.",
        ));
    }
    let flag = state.config_dir().join("allow-lan-http");
    let done = if body.lan_http {
        std::fs::write(&flag, "")
    } else {
        std::fs::remove_file(&flag).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    };
    done.map_err(|e| tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::audit::record(
        &state,
        "security",
        &format!("lan_http={}", body.lan_http),
        "ok",
    );
    Ok(Json(serde_json::json!({ "lan_http": body.lan_http })))
}
