// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Read-only views of the box: `GET /status`, `/config`, `/models`, `/serving`,
//! `/endpoint` and `/leases`.

use axum::{http::StatusCode, Json};
use libttstation::model::{
    ConfigSummary, Endpoint, LeaseList, LeasingSummary, ModelsResponse, ServingList,
};
use serde::Serialize;

use crate::auth::{BearerAuth, LocalOrPaired};
use crate::error::{backend_error, ErrorResponse};
use crate::serving::discovery::discover_serving_cached;
use crate::serving::docker::RealCommandRunner;
use crate::state::AppState;

/// JSON body returned by `GET /status`.
#[derive(Serialize)]
pub(crate) struct StatusResponse {
    name: String,
    chips: String,
    /// TXT string form (`idle` / `serving:<model>`) -- the same
    /// representation used on the wire for mDNS, so agent and CLI never
    /// have to reconcile two different status encodings.
    status: String,
    /// This box's detected device-mesh label (`"p300x2"`, `"n300x4"`, ...),
    /// or `null` when detection failed/didn't run -- see
    /// `AppState::with_device_mesh`. Lets a client (Task 3's
    /// `tt-station --json status`) rank models by hardware fit without its own
    /// `tt-smi` access.
    device_mesh: Option<String>,
    /// This box's detected primary NIC MAC (`"aa:bb:cc:dd:ee:ff"`), or `null`
    /// when detection failed/didn't run -- see `AppState::with_mac`. Lets a
    /// client (the Mac app) send a Wake-on-LAN magic packet to this box when
    /// it's off.
    mac: Option<String>,
    /// Whether/how this box can lease chips out via `gozer` -- see the
    /// design doc's `GET /status` row. `available: false` (gozer never
    /// probed successfully at startup) means every other field is `null`:
    /// never a guessed version or a fabricated board count for hardware
    /// this agent never actually asked gozer about.
    leasing: LeasingSummary,
    /// Whether the served chat model is answering (`crate::liveness`):
    /// `state` is `unknown`, `ok` or `degraded`; a degraded model carries a
    /// `reason` and the `restart` action (`POST /doctor/fix/restart_model`).
    /// `status` stays `serving:<id>` either way -- older clients parse it.
    model_health: crate::liveness::Health,
    /// This build, the API generation it speaks, and the oldest client API
    /// generation it still serves (see `crate::health`).
    agent_version: &'static str,
    api_version: u32,
    min_client: u32,
}

pub(crate) async fn get_status(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<StatusResponse> {
    // Reconcile against docker reality so a model stopped out of band (manual
    // `docker stop`, crash) isn't reported as still serving. Self-heals the
    // stored state as a side effect -- see `AppState::reconciled_status`.
    let status = state.reconciled_status().await;
    let leasing = leasing_info(&state).await;
    Json(StatusResponse {
        name: state.name().to_string(),
        chips: state.chips().to_string(),
        status: status.to_txt(),
        device_mesh: state.device_mesh().map(str::to_string),
        mac: state.mac().map(str::to_string),
        leasing,
        model_health: state.liveness().health(),
        agent_version: crate::health::AGENT_VERSION,
        api_version: crate::health::API_VERSION,
        min_client: crate::health::MIN_CLIENT,
    })
}

/// Build `GET /status`'s `leasing` field: `available`/`version` come
/// straight from the startup-probed `Capability` (no shell-out); `boards`/
/// `max_concurrent` come from `AppState::gozer_snapshot`'s cached parse of
/// `gozer status --json` (the SAME cache `GET /leases` fills -- see
/// `GOZER_LEASE_CACHE_TTL`'s doc comment), so calling `/status` right after
/// (or right before) `/leases` costs no extra shell-out either way.
///
/// The type is `libttstation::model::LeasingSummary`, the SAME struct
/// `StatusInfo::leasing` decodes into -- one wire shape defined once, rather
/// than a private serializer here and a private deserializer on the client
/// that can drift apart field by field.
async fn leasing_info(state: &AppState) -> LeasingSummary {
    let Some(capability) = state.gozer() else {
        return LeasingSummary {
            available: false,
            version: None,
            boards: None,
            max_concurrent: None,
        };
    };
    let version = capability.version.clone();
    let snapshot = state.gozer_snapshot().await;
    LeasingSummary {
        available: true,
        version: Some(version),
        boards: Some(snapshot.boards),
        max_concurrent: Some(snapshot.max_concurrent),
    }
}

/// `GET /leases` (bearer-guarded, per `get_endpoint`'s `_auth: BearerAuth`
/// pattern): every chip's lease state as gozer reports it, or an explicit
/// `available: false` marker -- never an error -- when gozer isn't
/// installed on this box. See [`LeaseList`]'s doc comment for why this is a
/// struct with an explicit flag rather than just an (ambiguous) empty list.
///
/// The actual `gozer status --json` shell-out happens inside
/// `AppState::gozer_snapshot`, cached with a short TTL (`GOZER_LEASE_CACHE_TTL`)
/// -- `gozer status` is cheap (sysfs + `/proc`, never a device -- see the
/// design doc), but an unthrottled endpoint still invites a Mac polling
/// every second, exactly the contention `tt_smi_cache` exists to prevent for
/// `/telemetry`.
pub(crate) async fn get_leases(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<LeaseList> {
    let available = state.gozer().is_some();
    let leases = if available {
        state.gozer_snapshot().await.leases
    } else {
        Vec::new()
    };
    Json(LeaseList { available, leases })
}

/// `GET /config` (the box itself or a paired device): the agent's redacted
/// serving-config summary -- active/available profiles plus the resolved
/// backend/serving-host/port/image/repo/device -- so the GTK panel, the `tt-station
/// config` CLI, and the Mac app can render "what am I actually about to
/// serve with" (on the box, without pairing). `ConfigSummary` carries no secrets by
/// construction (no `hf_token` field exists on it), so there's nothing for
/// this handler to redact -- it just serves the stored summary verbatim.
pub(crate) async fn get_config(
    axum::extract::State(state): axum::extract::State<AppState>,
    _caller: LocalOrPaired,
) -> Json<ConfigSummary> {
    Json(state.config_summary())
}

/// `GET /models` (the box itself or a paired device): enumerate the models
/// this box's backend can serve (see `ServingBackend::list_models`), so a
/// client never has to guess or hardcode a model id before calling `/run`.
/// It was unauthed once, like `/status`; every route but the few in
/// `PUBLIC_ROUTES` now takes pairing (`require_auth`).
///
/// `ServingBackend::list_models` is sync (like `start`/`stop`), so it's run
/// via `spawn_blocking` rather than called directly from this async
/// handler -- same rule the module doc in `serving/mod.rs` states for every
/// `ServingBackend` method, even though `RunPyBackend`'s implementation
/// (a single small file read) is fast in practice.
pub(crate) async fn get_models(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ModelsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();

    let result = tokio::task::spawn_blocking(move || backend.list_models())
        .await
        .map_err(|join_err| {
            backend_error(anyhow::anyhow!("list_models task panicked: {join_err}"))
        })?;

    result.map(Json).map_err(backend_error)
}

/// `GET /endpoint` (bearer-guarded): the `Endpoint` of whatever's currently
/// serving, or `409 Conflict` if the box is idle. `409` rather than `404`
/// because the route itself exists and is reachable -- what's missing is a
/// *resource* (a live endpoint), which is exactly what `409` communicates:
/// the request is well-formed but conflicts with the box's current state.
pub(crate) async fn get_endpoint(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Endpoint>, StatusCode> {
    // Reconcile first: if the agent's serve died out of band, this clears the
    // stored endpoint (via set_idle), so we return 409 (idle) instead of
    // handing back a dead base_url.
    let _ = state.reconciled_status().await;
    state.endpoint().map(Json).ok_or(StatusCode::CONFLICT)
}

/// `GET /serving` (the box itself or a paired device, like `GET /models`): list
/// EVERY live `tt-inference-server` `/v1` endpoint on the box, whoever
/// launched it (the agent's own `/run`, tt-studio's FastAPI, or a manual
/// `run.py`). Read-only discovery, behind the same default-deny layer as
/// `/models` (`require_auth`).
///
/// Additive to (not a replacement for) `/status`: `/status` still reports the
/// agent's own last serving intent, while `/serving` reflects docker reality.
/// A discovered endpoint is tagged `source: "agent"` when it's on the agent's
/// own configured serving port and the agent's in-memory status is serving
/// that model, else `"external"`.
///
/// The whole scan -- `docker ps` plus a `/v1/models` probe per candidate --
/// is blocking I/O, so it runs on `tokio::task::spawn_blocking` with a
/// `RealCommandRunner`, holding no mutex across it (the status snapshot is
/// taken up front). A `docker`-missing / no-containers / probe-failure box
/// yields `{"serving":[]}` -- never an error, never a panic. See
/// `serving::discovery::discover_serving`.
pub(crate) async fn get_serving(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<ServingList> {
    let serving_host = state.serving_host().to_string();
    let serving_port = state.serving_port();
    let status = state.status();
    let key = state.serve_key();

    let serving = tokio::task::spawn_blocking(move || {
        let runner = RealCommandRunner;
        discover_serving_cached(&runner, &key, &serving_host, serving_port, &status)
    })
    .await
    // A join failure (the blocking task panicked) is reported as "nothing
    // serving" rather than a 500 -- `/serving` is best-effort discovery and
    // must never fail a clean box.
    .unwrap_or_default();

    Json(ServingList { serving })
}
