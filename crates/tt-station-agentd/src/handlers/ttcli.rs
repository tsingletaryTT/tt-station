// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Models from the official Tenstorrent CLI (`tt`): `GET /tt/models`,
//! `/tt/pulls`, and `POST /tt/pull` / `/tt/pull/cancel`.

use crate::auth::BearerAuth;
use crate::error::{backend_error, tt_error, ErrorResponse};
use crate::state::AppState;
use axum::{http::StatusCode, Json};
use serde::Deserialize;

// ---------------------------------------------------------------------
// The tt CLI as model manager: `/tt/models`, `/tt/pulls`, `/tt/pull`
// ---------------------------------------------------------------------

/// `POST /tt/pull` / `POST /tt/pull/cancel` body.
#[derive(Deserialize)]
pub(crate) struct PullRequest {
    /// tt catalog name (`Qwen3-32B`) or HF repo id (`Qwen/Qwen3-32B`).
    model: String,
}

fn tt_unavailable() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse {
            error: "The official Tenstorrent CLI (tt) isn't installed on this box, so models \
                    can't be listed or downloaded from here."
                .to_string(),
        }),
    )
}

/// `GET /tt/models` (paired, like `/models`): this mesh's catalog from
/// `tt model list` -- support status, tool-call parser, whether the weights are
/// downloaded and their size -- plus this agent's pulls and the free disk where
/// weights land.
pub(crate) async fn tt_models(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    let (catalog, pulls, free) = tokio::task::spawn_blocking(move || {
        (
            tt.catalog(device.as_deref(), false),
            tt.pulls(),
            crate::ttcli::disk_free_bytes(tt.hf_cache()),
        )
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("catalog task panicked: {e}"),
        )
    })?;
    let catalog = catalog.map_err(backend_error)?;
    Ok(Json(serde_json::json!({
        "device": catalog.device,
        "disk_free_bytes": free,
        "models": catalog.models,
        "pulls": pulls,
    })))
}

/// `GET /tt/pulls` (paired): just the pulls and free disk -- cheap enough to
/// poll every couple of seconds while a download runs.
pub(crate) async fn tt_pulls(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let (pulls, free) = tokio::task::spawn_blocking(move || {
        (tt.pulls(), crate::ttcli::disk_free_bytes(tt.hf_cache()))
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pulls task panicked: {e}"),
        )
    })?;
    Ok(Json(
        serde_json::json!({ "pulls": pulls, "disk_free_bytes": free }),
    ))
}

/// `POST /tt/pull` (authed): start downloading a catalog model with
/// `tt model pull`, in the background. `202 {"status":"started"}`, or `200
/// {"status":"already_downloaded"}`. Refuses a model this mesh can't run
/// (`400`), one not in the catalog (`404`), a pull already running or too
/// little free disk for tt's stated requirement (`409`).
pub(crate) async fn tt_pull(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PullRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let catalog = tt.catalog(device.as_deref(), true).map_err(backend_error)?;
        let mesh = catalog
            .device
            .clone()
            .unwrap_or_else(|| "this box".to_string());
        let model = catalog.find(&req.model).cloned().ok_or_else(|| {
            tt_error(
                StatusCode::NOT_FOUND,
                format!(
                    "There's no model called {} in this box's catalog.",
                    req.model
                ),
            )
        })?;
        if model.weights == crate::ttcli::Weights::Container {
            // The serving container fetches these itself; pulling to the host
            // cache would download gigabytes nothing reads.
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "status": "not_needed", "model": model.name })),
            ));
        }
        if model.weights == crate::ttcli::Weights::Complete && !tt.is_pulling(&model.name) {
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "status": "already_downloaded", "model": model.name })),
            ));
        }
        if !model.supported {
            return Err(tt_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "{} doesn't run on {mesh}, so there's no point downloading it here.",
                    model.name
                ),
            ));
        }
        if let (Some(free), Some(need_gb)) = (
            crate::ttcli::disk_free_bytes(tt.hf_cache()),
            model.min_disk_gb,
        ) {
            if (free as f64) < need_gb * 1e9 {
                return Err(tt_error(
                    StatusCode::CONFLICT,
                    format!(
                        "{} needs about {need_gb:.0} GB of free disk; the box has {:.0} GB.",
                        model.name,
                        free as f64 / 1e9
                    ),
                ));
            }
        }
        match tt.start_pull(&model) {
            Ok(()) => Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": "started", "model": model.name })),
            )),
            Err(crate::ttcli::PullRefusal::AlreadyRunning) => Err(tt_error(
                StatusCode::CONFLICT,
                format!("{} is already downloading.", model.name),
            )),
            Err(crate::ttcli::PullRefusal::Spawn(e)) => {
                Err(tt_error(StatusCode::INTERNAL_SERVER_ERROR, e))
            }
        }
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pull task panicked: {e}"),
        )
    })?
}

/// `POST /tt/pull/cancel` (authed): kill a running pull. `404` if none.
pub(crate) async fn tt_pull_cancel(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PullRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    tokio::task::spawn_blocking(move || {
        // Accept the HF repo id too: resolve it to the tt name the job is keyed by.
        let name = tt
            .catalog(device.as_deref(), false)
            .ok()
            .and_then(|c| c.find(&req.model).map(|m| m.name.clone()))
            .unwrap_or(req.model);
        if tt.cancel_pull(&name) {
            Ok(Json(
                serde_json::json!({ "status": "cancelled", "model": name }),
            ))
        } else {
            Err(tt_error(
                StatusCode::NOT_FOUND,
                format!("{name} isn't downloading."),
            ))
        }
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cancel task panicked: {e}"),
        )
    })?
}
