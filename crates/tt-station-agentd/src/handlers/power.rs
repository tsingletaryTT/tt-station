//! Chip resets and machine power: `POST /reset`, `POST /power` and
//! `GET /power/info`.

use axum::{http::StatusCode, Json};
use serde::Deserialize;

use crate::auth::{BearerAuth, OwnerAuth};
use crate::authkeys;
use crate::error::{backend_error, contention_aware_error, ErrorResponse};
use crate::state::AppState;

/// `POST /reset` (bearer-guarded): return the box to a fresh-install state
/// for a demo. In order:
///
///   1. Ask the backend to reset (`ServingBackend::reset`) -- stop any
///      serving container and, on `RunPyBackend`, reset the board too. Run
///      via `spawn_blocking` since it shells out (`docker`, `tt-smi`), same
///      rule `/run` and `/stop` follow for the backend's sync methods.
///   2. Sign out EVERY paired device (in-memory sessions + persisted store).
///   3. Best-effort revoke every `ttstation:<label>`-tagged SSH key the pair
///      flow ever installed (`authkeys::revoke_all_ttstation`) -- a reset
///      should also demo losing the keyless-SSH access pairing granted, not
///      just the bearer token. Non-fatal: an unwritable/missing SSH file
///      must never fail the route.
///   4. Flip `status` back to `Idle`, drop the stored `Endpoint`, and
///      re-advertise `Idle` (all via `set_idle`).
///
/// Clearing the tokens invalidates the caller's OWN bearer token -- that's
/// expected for a reset, and harmless here: auth was already checked at
/// entry (the `BearerAuth` extractor), so this handler still runs to
/// completion and returns `200 {}`.
///
/// The body is OPTIONAL: `{"force": true}` is `tt-station reset --force`, which
/// overrides the foreign-lease refusal (see `RunPyBackend::reset_forcing`).
/// Every existing client posts `/reset` with no body and no content-type at
/// all, which `Option<Json<..>>` reads as `None` -> unforced, so the polite
/// default is what an older `tt-station` still gets. (A request that DOES declare
/// `application/json` and then carries an unparseable body is a `400` rather
/// than a silent unforced reset -- guessing which one a garbled `{"force":
/// ...}` meant is exactly the wrong thing to do on a destructive route, and
/// failing there resets nothing.)
pub(crate) async fn reset(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    body: Option<Json<ResetRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();
    let force = body.map(|Json(req)| req.force).unwrap_or(false);

    // Backend reset shells out (docker/tt-smi) -- never on the async runtime.
    // `reset`/`reset_forced` rather than one call with a bool, for the same
    // reason `/run` splits: the unforced path is byte-for-byte the call it
    // always was.
    tokio::task::spawn_blocking(move || {
        if force {
            backend.reset_forced()
        } else {
            backend.reset()
        }
    })
    .await
    .map_err(|join_err| backend_error(anyhow::anyhow!("reset task panicked: {join_err}")))?
    // A reset REFUSED because another tenant holds chips is a 409, and
    // (note the `?`) it aborts the route before the token clearing
    // below: a refusal must leave the box, and the pairing, untouched.
    .map_err(contention_aware_error)?;

    // Sign out every paired device (the caller's own too -- expected).
    crate::audit::record(&state, "reset", if force { "forced" } else { "" }, "ok");
    state.clear_tokens();

    // Best-effort: also revoke every keyless-SSH key the pair flow ever
    // installed (see `authkeys::revoke_all_ttstation`), so a reset actually
    // demos losing SSH access, not just losing the bearer token. Never fail
    // the route over this -- a reset must still succeed even if the SSH
    // file is missing/unwritable (e.g. permissions, or the feature was
    // never used on this box).
    if let Err(err) = authkeys::revoke_all_ttstation(state.ssh_path()) {
        eprintln!("reset: failed to revoke ttstation SSH keys (non-fatal): {err}");
    }

    // Back to idle: status Idle, endpoint cleared, Idle re-advertised.
    state.set_idle();

    Ok(Json(serde_json::json!({})))
}

/// The success status for a power action: reset-chips completes synchronously
/// (200), while machine ops only *initiate* teardown before the box goes down
/// (202 Accepted).
pub(crate) fn power_success_status(action: crate::power::PowerAction) -> StatusCode {
    if action.is_machine_op() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

/// JSON body accepted by `POST /power`.
#[derive(Deserialize)]
pub(crate) struct PowerRequest {
    action: String,
    /// `tt-station power reset-chips --force`: run the whole-box board reset even
    /// though another tenant holds chips (or gozer cannot be asked). Ignored
    /// for suspend/reboot/shutdown, which have no lease check to override.
    /// `#[serde(default)]` keeps every pre-`--force` client working.
    #[serde(default)]
    force: bool,
}

/// JSON body OPTIONALLY accepted by `POST /reset` -- see the `reset` handler.
#[derive(Deserialize)]
pub(crate) struct ResetRequest {
    /// `tt-station reset --force`: run the whole-box `tt-smi -r` even though another
    /// tenant holds chips (or gozer cannot be asked).
    #[serde(default)]
    force: bool,
}

/// `POST /power { "action": "reset-chips" | "suspend" | "reboot" | "shutdown",
/// "force": <bool> }` (bearer-guarded, same `BearerAuth` gate as
/// `/run`/`/stop`/`/reset`): run the configured command for `action` via
/// `AppState::run_power_command`. `force` (default `false`) overrides
/// `reset-chips`'s foreign-lease refusal and is ignored for the machine ops.
///
/// Status codes:
///   - `400` if `action` doesn't parse (`PowerAction::parse`) -- caller error,
///     checked before anything runs.
///   - `200 {}` on a successful `reset-chips` -- it completes synchronously
///     (board reset), so the caller can trust the response body.
///   - `202 { "action", "accepted": true }` on a successful machine op
///     (suspend/reboot/shutdown) -- the command only *initiates* teardown;
///     the box may go down before a `200` could ever be observed, so this
///     reports "accepted", not "done".
///   - `403` if the command fails with a permission/polkit-shaped error
///     (no rule installed for the run-user to invoke `systemctl`/`tt-smi`
///     without a password) -- the operator's box to fix, not this agent's,
///     so the message points at `docs/reference/power-controls.md`.
///   - `500` for any other command failure (e.g. the binary itself is
///     missing) -- `backend_error`, same as every other route's fallback.
pub(crate) async fn power(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    Json(req): Json<PowerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let action = crate::power::PowerAction::parse(&req.action).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("unknown power action: {}", req.action),
            }),
        )
    })?;

    if let Some(holder) = state.chips_holder() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: holder.to_string(),
            }),
        ));
    }
    if action.is_machine_op() {
        let (what, body) = match action {
            crate::power::PowerAction::Suspend => (
                "going to sleep",
                "Wake it from TT Station when you need it.",
            ),
            crate::power::PowerAction::Reboot => {
                ("restarting", "You'll hear when it's back online.")
            }
            _ => ("shutting down", "Turn it back on to use it again."),
        };
        state
            .notifier()
            .emit_and_wait(crate::notify::Draft {
                kind: "power",
                title: format!("{} is {what}", state.name()),
                body: body.to_string(),
            })
            .await;
    }
    let s = state.clone();
    let force = req.force;
    let ran = tokio::task::spawn_blocking(move || s.run_power_command(action, force))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("power task panicked: {join_err}")))?;
    crate::audit::record(
        &state,
        "power",
        &req.action,
        if ran.is_ok() { "ok" } else { "failed" },
    );
    ran.map_err(|e| {
            // A permission failure (no polkit rule) is the operator's to fix --
            // distinguish it from a generic 500 with a pointer to the doc.
            let msg = e.to_string();
            if msg.contains("Interactive authentication required")
                || msg.contains("Access denied")
                || msg.contains("not authorized")
            {
                (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse {
                        error: format!(
                            "{msg} — the box is not permitted to {}. Install the polkit rule (see docs/reference/power-controls.md).",
                            req.action
                        ),
                    }),
                )
            } else {
                // `reset-chips` refused because another tenant holds chips
                // is a 409, not a 500 -- see `contention_aware_error`.
                contention_aware_error(e)
            }
        })?;

    let body = if action.is_machine_op() {
        serde_json::json!({ "action": req.action, "accepted": true })
    } else {
        serde_json::json!({})
    };
    Ok((power_success_status(action), Json(body)))
}

/// `GET /power/info` (authed): what the phone can do with the machine's power
/// and the one-time setup for the rest. See `crate::power::readiness_blocking`.
pub(crate) async fn power_info(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<crate::power::PowerReadiness> {
    let config_dir = state.config_dir();
    let user = std::env::var("USER").unwrap_or_else(|_| "ttuser".to_string());
    Json(
        tokio::task::spawn_blocking(move || crate::power::readiness_blocking(&user, &config_dir))
            .await
            .unwrap_or_else(|_| crate::power::readiness_blocking("ttuser", &std::env::temp_dir())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_success_status_is_202_for_machine_ops_200_for_reset_chips() {
        use crate::power::PowerAction;
        assert_eq!(
            power_success_status(PowerAction::ResetChips),
            StatusCode::OK
        );
        assert_eq!(
            power_success_status(PowerAction::Suspend),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Reboot),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Shutdown),
            StatusCode::ACCEPTED
        );
    }
}
