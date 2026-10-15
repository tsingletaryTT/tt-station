// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Paired devices: `GET /devices`, `DELETE /devices/{id}`, and changing a
//! device's owner flag or person.

use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::auth::{OwnerAuth, ThisDevice, OWNER_ONLY};
use crate::error::{tt_error, ErrorResponse};
use crate::state::AppState;

/// One paired device, as `GET /devices` shows it: never its token or digest.
#[derive(Serialize)]
struct DeviceView {
    id: String,
    name: String,
    platform: Option<String>,
    /// Unix seconds; `null` for a device paired before the box kept track.
    paired: Option<u64>,
    last_used: u64,
    /// Unix seconds: when it stops working unless it's used before then.
    expires: u64,
    /// The device making this request.
    this_device: bool,
    /// Manages the box; `false` is a member (`crate::sessions`).
    owner: bool,
    /// Who the device belongs to (`crate::people`), if anyone in particular.
    person: Option<String>,
}

impl DeviceView {
    fn of(state: &AppState, s: crate::sessions::Session, me: &str) -> Self {
        DeviceView {
            this_device: s.id == me,
            expires: s.expires(),
            person: state
                .people()
                .person_of_digest(&s.token_sha256)
                .map(|p| p.id),
            owner: s.owner,
            id: s.id,
            name: s.name,
            platform: s.platform,
            paired: s.created,
            last_used: s.last_used,
        }
    }
}

/// `GET /devices` (bearer-guarded): every paired device, most recently used
/// first, marking the caller's own, for an owner; just its own device for a
/// member. `owner` says which the caller is.
pub(crate) async fn get_devices(
    axum::extract::State(state): axum::extract::State<AppState>,
    ThisDevice(me): ThisDevice,
) -> Json<serde_json::Value> {
    let owner = state.sessions().get(&me).is_some_and(|s| s.owner);
    let devices: Vec<DeviceView> = state
        .sessions()
        .list()
        .into_iter()
        .filter(|s| owner || s.id == me)
        .map(|s| DeviceView::of(&state, s, &me))
        .collect();
    Json(serde_json::json!({
        "devices": devices,
        "owner": owner,
        "idle_expiry_days": crate::sessions::IDLE_EXPIRY.as_secs() / 86_400,
    }))
}

#[derive(Deserialize)]
pub(crate) struct OwnerChange {
    owner: bool,
}

/// `POST /devices/{id}/owner {"owner": bool}` (owners): make a device an
/// owner, or a member again. The last owner stays one.
pub(crate) async fn post_device_owner(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<OwnerChange>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let s = state
        .sessions()
        .set_owner(&id, req.owner)
        .map_err(|e| tt_error(StatusCode::CONFLICT, &e))?;
    crate::audit::record(
        &state,
        if req.owner { "promote" } else { "demote" },
        &format!("{} ({})", s.name, s.id),
        "ok",
    );
    state.notifier().record(crate::notify::Draft {
        kind: "devices",
        title: format!(
            "{} is {} of this box now",
            s.name,
            if s.owner { "an owner" } else { "a member" }
        ),
        body: if s.owner {
            "It can manage devices, keys, power and updates.".to_string()
        } else {
            "It can use the box, but not manage devices, keys, power or updates.".to_string()
        },
    });
    Ok(Json(serde_json::json!({ "id": s.id, "owner": s.owner })))
}

#[derive(Deserialize)]
pub(crate) struct PersonChange {
    person: Option<String>,
}

/// `PUT /devices/{id}/person {"person": id | null}` (owners): whose device
/// it is (`crate::people`). A member can only claim a person nobody's
/// device has yet; an owner settles everything else here.
pub(crate) async fn put_device_person(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<PersonChange>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let Some(s) = state.sessions().get(&id) else {
        return Err(tt_error(
            StatusCode::NOT_FOUND,
            "No such device. It may have been removed.",
        ));
    };
    state
        .people()
        .assign_digest(&s.token_sha256, req.person.as_deref())
        .map_err(|e| tt_error(StatusCode::BAD_REQUEST, &e))?;
    crate::audit::record(
        &state,
        "device_person",
        &format!(
            "{} ({}) → {}",
            s.name,
            s.id,
            req.person.as_deref().unwrap_or("no one")
        ),
        "ok",
    );
    Ok(Json(
        serde_json::json!({ "id": s.id, "person": req.person }),
    ))
}

/// `DELETE /devices/{id}` (bearer-guarded): sign one device out. Its token
/// stops working on the next request, and its open terminals close. A
/// device may remove itself -- that's signing out -- and still gets this
/// answer, since it was authenticated on the way in.
pub(crate) async fn delete_device(
    axum::extract::State(state): axum::extract::State<AppState>,
    ThisDevice(me): ThisDevice,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Signing yourself out is anyone's; removing another device is an owner's.
    if id != me && !state.sessions().get(&me).is_some_and(|s| s.owner) {
        crate::audit::record(&state, "denied", &format!("DELETE /devices/{id}"), "denied");
        return Err(tt_error(StatusCode::FORBIDDEN, OWNER_ONLY));
    }
    let Some(removed) = state.sessions().revoke(&id) else {
        return Err(tt_error(
            StatusCode::NOT_FOUND,
            "No such device. It may have been removed already.",
        ));
    };
    state.people().forget_phone(&removed.token_sha256);
    let this_device = removed.id == me;
    crate::audit::record(
        &state,
        "revoke",
        &format!("{} ({})", removed.name, removed.id),
        "ok",
    );
    state.notifier().record(crate::notify::Draft {
        kind: "devices",
        title: if this_device {
            format!("{} signed out", removed.name)
        } else {
            format!("Device removed: {}", removed.name)
        },
        body: "It can no longer use this box until it pairs again.".to_string(),
    });
    Ok(Json(
        serde_json::json!({ "removed": removed.id, "this_device": this_device }),
    ))
}
