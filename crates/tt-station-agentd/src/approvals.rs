// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Everything waiting for the person's OK, in one place -- Code sessions'
//! questions and routines' actions -- so a phone can list and answer them
//! (from the app or straight from the notification) without knowing how
//! each feature asks.
//!
//! - `GET /approvals`: `{approvals: [{id, kind, where, summary, can_always, ...}]}`
//! - `POST /approvals/answer {id, allow, always?}`
//!
//! Ids: `code:<session>:<request>`, `routine:<run>` (a running routine's
//! question) and `routine:<run>:<n>` (a step it queued after reading the web). The same objects ride
//! along on the box's events (`data.approval`), so a notification can answer.
//!
//! Any paired device may answer these: Code sessions and routines are the
//! household's, and members use both. Bring-up's approvals -- commands on
//! the box itself, with root when asked for -- are owners' (`crate::bringup`),
//! and so are the questions of a Code session outside the sandbox: a member
//! neither sees nor answers those (`crate::workspace`).

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, CallerAuth};

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// `GET /approvals`
pub(crate) async fn get_approvals(
    State(state): State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
) -> Json<Value> {
    let mut all = crate::workspace::pending_approvals(&state, caller.owner());
    all.extend(crate::routines::pending_approvals(&state));
    Json(json!({ "approvals": all }))
}

#[derive(Debug, Deserialize)]
pub struct Answer {
    pub id: String,
    pub allow: bool,
    #[serde(default)]
    pub always: bool,
}

/// `POST /approvals/answer {id, allow, always?}`
pub(crate) async fn post_answer(
    State(state): State<AppState>,
    auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    Json(a): Json<Answer>,
) -> Reply {
    crate::audit::record(
        &state,
        "approval",
        &a.id,
        if a.allow { "allowed" } else { "declined" },
    );
    let mut parts = a.id.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("code"), Some(session), Some(request_id)) => {
            let body = crate::workspace::Approve {
                request_id: request_id.to_string(),
                allow: a.allow,
                always: a.always,
            };
            crate::workspace::post_approve(
                State(state),
                auth,
                CallerAuth(caller),
                UrlPath(session.to_string()),
                Json(body),
            )
            .await
        }
        (Some("routine"), Some(run), Some(n)) => {
            let n: usize = n.parse().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "Unknown approval." })),
                )
            })?;
            let done = crate::routines::answer_queued(&state, run, n, a.allow)?;
            Ok(Json(json!({ "ok": true, "status": done })))
        }
        (Some("routine"), Some(run), None) => {
            crate::routines::post_approve(
                State(state),
                auth,
                UrlPath(run.to_string()),
                Json(crate::routines::Decision { allow: a.allow }),
            )
            .await
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Unknown approval." })),
        )),
    }
}
