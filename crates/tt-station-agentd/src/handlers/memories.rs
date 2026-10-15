// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The assistant's memories and creations: `GET`/`DELETE /memories`,
//! `DELETE /memories/{id}` and `GET /creations/{id}`.

use crate::auth::{BearerAuth, CallerAuth};
use crate::error::{tt_error, ErrorResponse};
use crate::state::AppState;
use axum::{http::StatusCode, Json};

/// `GET /memories` (authed): what the assistant remembers, oldest first.
pub(crate) async fn list_memories(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
) -> Json<serde_json::Value> {
    let person = who.person.map(|p| p.id);
    Json(serde_json::json!({ "memories": state.memories().list_for(person.as_deref()) }))
}

/// `DELETE /memories` (authed): a member forgets its person's own memories;
/// an owner forgets everything it sees, the household's shared ones too.
pub(crate) async fn clear_memories(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let person = who.person.map(|p| p.id);
    if caller.device.as_ref().is_some_and(|d| d.owner) {
        state.memories().clear(person.as_deref());
    } else if let Some(person) = person {
        state.memories().clear_own(&person);
    } else {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "This device isn't anyone's, so its memories are the household's: only an owner can clear those. Pick who you are in Settings → People first.",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /memories/{id}` (authed).
pub(crate) async fn delete_memory(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> StatusCode {
    let gone = state
        .memories()
        .forget(&id, who.person.map(|p| p.id).as_deref());
    if gone.iter().any(|m| m.id == id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `GET /creations/{id}` (authed): a JPEG the assistant made in chat.
pub(crate) async fn get_creation(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.creations().get(&id) {
        Some(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                (
                    axum::http::header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response(),
        None => tt_error(StatusCode::NOT_FOUND, "No such image.").into_response(),
    }
}
