// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Who may start, stop and switch the model the box serves for everyone.
//!
//! An owner sets it in the household's settings (`model_control` in
//! `PUT /data/settings`, see `crate::export::DataSettings`):
//!
//! - `anyone` (the default, so boxes that had no such setting keep working
//!   as they did): every paired device may.
//! - `owners`: only owners (and the box's own user) may; a member gets `403`
//!   with [`OWNERS_ONLY`], and the refusal is in the activity log.
//!
//! It applies everywhere serving changes on a person's say-so: the routes
//! (`POST /run`, `/stop`, `/serve`, `/modes/{id}`, `/coserve`,
//! `/community/serve`, `/vllm/native/serve`, `/jobs/{id}/run`, the Doctor's
//! `restart_model` fix, and check-ups: `POST /evals`, `/evals/stop`; see
//! [`require`]), the assistant's `run_model`, `stop_model` and `switch_mode`
//! (refused before a confirm card is offered, and again on confirm), and
//! routines' `start_model`, `stop_model`, `evaluate_model` and mode switches
//! when they run on a member's behalf (a run started by a member, or a
//! routine a member last wrote) or a member approves one.
//!
//! The box's own background work isn't a person's say-so and isn't refused:
//! restarting the last model after a power cut, the idle sleep and wake, and
//! the transcription queue starting Whisper when nothing is serving (moving
//! another model aside for it, `POST /jobs/{id}/run`, is gated).

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::auth::Caller;
use crate::error::{err, ApiError};
use crate::state::AppState;

/// What a member hears when only owners may start or stop models.
pub const OWNERS_ONLY: &str = "Only owners can start or stop models on this box.";

/// The setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelControl {
    #[default]
    Anyone,
    Owners,
}

impl ModelControl {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelControl::Anyone => "anyone",
            ModelControl::Owners => "owners",
        }
    }
}

/// The box's setting now.
pub fn setting(state: &AppState) -> ModelControl {
    crate::export::settings(state).model_control
}

/// Whether `caller` may start, stop or switch models.
pub fn allows(state: &AppState, caller: &Caller) -> bool {
    setting(state) == ModelControl::Anyone || caller.owner()
}

/// Whether work done on behalf of the paired device `device` may (`None`:
/// the box itself). A device that's since been removed may not.
pub fn allows_device(state: &AppState, device: Option<&str>) -> bool {
    if setting(state) == ModelControl::Anyone {
        return true;
    }
    match device {
        None => true,
        Some(id) => state.sessions().get(id).is_some_and(|s| s.owner),
    }
}

/// Whether the request being handled may (outside a request: the box's own
/// work, which may).
pub fn allows_now(state: &AppState) -> bool {
    match crate::auth::current_caller() {
        Some(c) => allows(state, &c),
        None => true,
    }
}

/// [`allows_now`], as the `403` a route answers (and logs) when it doesn't.
pub fn check_now(state: &AppState, what: &str) -> Result<(), ApiError> {
    if allows_now(state) {
        return Ok(());
    }
    crate::audit::record(state, "denied", what, "denied");
    Err(err(StatusCode::FORBIDDEN, OWNERS_ONLY))
}

/// Route layer for the routes that start, stop or switch serving (see the
/// router): runs inside `require_auth`, so the caller is known.
pub(crate) async fn require(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let what = format!("{} {}", req.method(), req.uri().path());
    match check_now(&state, &what) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, owner: bool) -> Caller {
        Caller {
            local: false,
            device: Some(crate::sessions::Session {
                id: id.into(),
                token_sha256: String::new(),
                name: id.into(),
                platform: None,
                created: None,
                last_used: 0,
                owner,
            }),
        }
    }

    fn state_with(control: ModelControl) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let s = crate::export::DataSettings {
            model_control: control,
            ..Default::default()
        };
        crate::fsio::write_json(
            &dir.path().join(crate::export::SETTINGS_FILE),
            &s,
            crate::fsio::PRIVATE,
        )
        .unwrap();
        let state = AppState::new(
            "qb2".into(),
            "4xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(dir.path().to_path_buf());
        (dir, state)
    }

    #[test]
    fn the_setting_reads_and_defaults_to_anyone() {
        assert_eq!(ModelControl::default(), ModelControl::Anyone);
        let old: crate::export::DataSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(old.model_control, ModelControl::Anyone, "existing boxes");
        let v: ModelControl = serde_json::from_str("\"owners\"").unwrap();
        assert_eq!(v, ModelControl::Owners);
        assert!(serde_json::from_str::<ModelControl>("\"members\"").is_err());
    }

    #[tokio::test]
    async fn owners_mode_refuses_members_only() {
        let (_d, anyone) = state_with(ModelControl::Anyone);
        assert!(allows(&anyone, &device("phone", false)));
        let (_d, owners) = state_with(ModelControl::Owners);
        assert!(!allows(&owners, &device("phone", false)));
        assert!(allows(&owners, &device("laptop", true)));
        let local = Caller {
            local: true,
            device: None,
        };
        assert!(allows(&owners, &local), "the box's own user");
        assert!(allows_now(&owners), "the box's own background work");
        let member = crate::auth::as_caller(device("phone", false), async {
            check_now(&owners, "POST /stop")
        })
        .await;
        assert_eq!(member.unwrap_err().status, StatusCode::FORBIDDEN);
        let (denied, _) = owners.audit().page(0, 10);
        assert_eq!(denied[0].action, "denied");
        assert_eq!(denied[0].target, "POST /stop");
        // On someone's behalf: the box itself may; an unknown device may not.
        assert!(allows_device(&owners, None));
        assert!(!allows_device(&owners, Some("gone")));
        assert!(allows_device(&anyone, Some("gone")));
    }
}
