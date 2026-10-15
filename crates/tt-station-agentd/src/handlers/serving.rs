// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Starting and stopping the served model: `POST /run`, `/stop`, and
//! `GET`/`POST /serve`.

use axum::{http::StatusCode, Json};
use libttstation::model::Endpoint;
use serde::{Deserialize, Serialize};

use crate::auth::BearerAuth;
use crate::error::{backend_error, contention_aware_error, tt_error, ErrorResponse};
use crate::state::{AppState, Phase};

/// JSON body accepted by `POST /run`.
#[derive(Deserialize)]
pub(crate) struct RunRequest {
    model: String,
    /// `tt-station run --force`: serve even though gozer will not grant the chips,
    /// because another tenant holds them (or because gozer cannot be asked).
    /// See `ServingBackend::start_forcing`.
    ///
    /// `#[serde(default)]` so every pre-`--force` client -- which sends
    /// `{"model": ".."}` and nothing else -- keeps working and gets the
    /// polite, refusing behaviour.
    #[serde(default)]
    force: bool,
}

/// JSON body returned by `POST /run` on success.
#[derive(Serialize)]
pub(crate) struct RunResponse {
    endpoint: Endpoint,
}

/// `POST /run { "model": "...", "force": <bool> }` (bearer-guarded): ask the
/// backend to start serving `model`.
///
/// `force` (default `false`, so an older client is unaffected) is
/// `tt-station run --force`: serve even though the box's chips are held by somebody
/// else. It answers a `409` that would otherwise stand -- see
/// `ServingBackend::start_forcing` and `RunPyBackend::acquire_lease`'s `force`
/// section for what it does (serve unleased) and does not (take the holder's
/// lease away) do.
///
/// The whole body lives in [`AppState::start_serving`] (shared with the
/// assistant's confirmed `run_model` action, which runs it in the
/// background): `backend.start` on `spawn_blocking`, then `set_serving` only
/// after it returns, with no mutex held across the `.await`.
pub(crate) async fn run_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Claimed before the first await: a second `/run` (or `/serve`) meanwhile
    // gets a 409 instead of starting the box twice.
    let prev = state
        .claim_start(&req.model)
        .map_err(|e| tt_error(StatusCode::CONFLICT, e.0))?;
    if let Err(e) = check_servable(&state, &req.model).await {
        state.release_start(prev, &req.model);
        return Err(e);
    }
    // A contended box answers 409 (naming the board and the holder), not
    // 500 -- see `contention_aware_error`.
    let started = state
        .start_serving_claimed(&req.model, req.force, prev)
        .await;
    crate::audit::record(
        &state,
        "model_start",
        &req.model,
        if started.is_ok() { "ok" } else { "failed" },
    );
    let endpoint = started.map_err(contention_aware_error)?;

    Ok(Json(RunResponse { endpoint }))
}

/// `POST /stop` (bearer-guarded): ask the backend to stop serving.
///
/// The whole body lives in [`AppState::stop_serving`] (shared with the
/// assistant's confirmed `stop_model`/`reset_chips` actions) -- see its doc
/// for why `backend.stop()` is ALWAYS called, even when nothing is recorded
/// as serving (a mid-bring-up `/stop` must still trip the backend's cancel
/// flag).
pub(crate) async fn stop_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let stopped = state.stop_serving().await;
    crate::audit::record(
        &state,
        "model_stop",
        "",
        if stopped.is_ok() { "ok" } else { "failed" },
    );
    stopped.map_err(backend_error)?;
    Ok(Json(serde_json::json!({})))
}

// ---------------------------------------------------------------------
// Serving lifecycle for clients that can't hold a request open: `/serve`
// ---------------------------------------------------------------------

/// Refuse to start a model whose weights aren't all on the box when the tt
/// catalog knows it: run.py would otherwise download them *inside* the launch,
/// silently, for as long as that takes (tens of GB), past the readiness ceiling.
/// Models tt doesn't know (an agent without tt, or a bundle id) pass through.
async fn check_servable(
    state: &AppState,
    model: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let Some(tt) = state.tt_cli() else {
        return Ok(());
    };
    let device = state.device_mesh().map(str::to_string);
    let wanted = model.to_string();
    let found = tokio::task::spawn_blocking(move || {
        tt.catalog(device.as_deref(), false)
            .ok()
            .and_then(|c| c.find(&wanted).cloned())
    })
    .await
    .ok()
    .flatten();
    let Some(m) = found else { return Ok(()) };
    if !m.supported {
        return Err(tt_error(
            StatusCode::BAD_REQUEST,
            format!("{} doesn't run on this box.", m.name),
        ));
    }
    match m.weights {
        crate::ttcli::Weights::Complete | crate::ttcli::Weights::Container => Ok(()),
        crate::ttcli::Weights::Partial => Err(tt_error(
            StatusCode::CONFLICT,
            format!(
                "{} is only partly downloaded. Finish the download before starting it.",
                m.name
            ),
        )),
        crate::ttcli::Weights::Missing => Err(tt_error(
            StatusCode::CONFLICT,
            format!(
                "{} isn't downloaded yet. Download it before starting it.",
                m.name
            ),
        )),
    }
}

/// `GET /serve` (paired): the [`Lifecycle`] plus the kind of
/// experience the served model is (`chat`, `image`, `video`, `speech`, `tts`).
pub(crate) async fn get_serve(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    let life = state.lifecycle();
    let probe = state.clone();
    let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    // How long this model usually takes to load here, while it's loading.
    let life_model_eta = (life.phase == Phase::Starting)
        .then(|| {
            life.model
                .as_deref()
                .and_then(|m| state.load_times().estimate(m))
        })
        .flatten();
    Json(serde_json::json!({
        "phase": life.phase,
        "model": life.model,
        // Only while something serves: during a switch the old model's name
        // would read as if it were the one loading.
        "served_id": life.served_id.or_else(|| (life.phase == Phase::Serving).then(|| state.current_model()).flatten()),
        "since": life.since,
        "error": life.error,
        "kind": kind,
        "split": crate::coserve::is_split(&state),
        "eta_s": life_model_eta,
        // A start that has shown no progress for a while (community packages): seconds.
        "stalled_s": (life.phase == Phase::Starting).then(|| crate::community::stalled_s(&state)).flatten(),
        // `ok`, `degraded` (loaded but not answering) or `unknown`; see `/status`'s `model_health`.
        "health": state.liveness().health().state,
        "companion": state.companion().map(|c| {
            let life = c.lifecycle();
            serde_json::json!({
                "phase": life.phase,
                "model": life.model,
                "served_id": life.served_id,
                "since": life.since,
                "error": life.error,
                "eta_s": (life.phase == Phase::Starting)
                    .then(|| life.model.as_deref().and_then(|m| state.load_times().estimate(m)))
                    .flatten(),
                "kind": "image",
            })
        }),
    }))
}

/// `POST /serve` (authed) `{model}`: like `/run`, but answers `202` at once and
/// brings the model up in the background; poll `GET /serve` for the outcome.
/// `409` while another start is in flight or the weights aren't downloaded.
pub(crate) async fn post_serve(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<RunRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let begun = begin_serve(&state, &req.model, req.force).await;
    crate::audit::record(
        &state,
        "model_switch",
        &req.model,
        if begun.is_ok() { "starting" } else { "failed" },
    );
    begun?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "starting", "model": req.model })),
    ))
}

/// Start `model` on the whole box in the background, after the same checks as
/// `POST /serve` (nothing else starting; weights downloaded). Shared with
/// `crate::modes`.
pub(crate) async fn begin_serve(
    state: &AppState,
    model: &str,
    force: bool,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    if let Some(holder) = state.chips_holder() {
        return Err(tt_error(StatusCode::CONFLICT, holder));
    }
    // Claimed before the first await, so a second caller sees `starting`.
    let prev = state
        .claim_start(model)
        .map_err(|e| tt_error(StatusCode::CONFLICT, e.0))?;
    if let Err(e) = check_servable(state, model).await {
        state.release_start(prev, model);
        return Err(e);
    }
    let model = model.to_string();
    let bg = state.clone();
    tokio::spawn(async move {
        let _ = bg.start_serving_claimed(&model, force, prev).await;
    });
    Ok(())
}
