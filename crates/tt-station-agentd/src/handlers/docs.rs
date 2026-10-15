// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The document store behind the assistant's file search: `GET`/`POST /docs`,
//! `DELETE /docs/{id}` and `POST /docs/search`.

use crate::auth::BearerAuth;
use crate::error::{tt_error, ErrorResponse};
use crate::state::{unix_now, AppState};
use axum::{http::StatusCode, Json};
use serde::Deserialize;

// ---------------------------------------------------------------------
// Documents (`/docs`): private Q&A over the user's own files. See `crate::docs`.
// ---------------------------------------------------------------------

/// Upload body ceiling: [`crate::docs::MAX_DOC_CHARS`] of multi-byte text.
pub(crate) const DOC_BODY_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct AddDocRequest {
    name: String,
    text: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct SearchDocsRequest {
    query: String,
    #[serde(default)]
    document: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
}

fn docs_unavailable() -> (StatusCode, Json<ErrorResponse>) {
    tt_error(StatusCode::CONFLICT, "This agent has no document store.")
}

/// `GET /docs` (authed): the uploaded documents, newest first.
pub(crate) async fn list_docs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    Ok(Json(serde_json::json!({ "documents": docs.list() })))
}

/// `POST /docs` (authed) `{name, text}`: add a document (the client extracts the
/// text), replacing any with the same name. `201` with its summary.
pub(crate) async fn add_doc(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<AddDocRequest>,
) -> Result<(StatusCode, Json<crate::docs::DocSummary>), (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    if req.name.trim().is_empty() {
        return Err(tt_error(
            StatusCode::BAD_REQUEST,
            "The document needs a name.",
        ));
    }
    if req.text.trim().is_empty() {
        return Err(tt_error(
            StatusCode::BAD_REQUEST,
            "No text found in that document (a scanned PDF has none to read).",
        ));
    }
    if req.text.chars().count() > crate::docs::MAX_DOC_CHARS {
        return Err(tt_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That document is too long (2 million characters at most).",
        ));
    }
    let now = unix_now();
    tokio::task::spawn_blocking(move || docs.add(&req.name, &req.text, now))
        .await
        .map_err(|e| tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(|summary| (StatusCode::CREATED, Json(summary)))
        .map_err(|e| {
            tt_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't save: {e}"),
            )
        })
}

/// `DELETE /docs/{id}` (authed).
pub(crate) async fn delete_doc(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    if docs.delete(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(tt_error(StatusCode::NOT_FOUND, "No such document."))
    }
}

/// `POST /docs/search` (authed) `{query, document?, top_k?}`: the matching
/// passages, best first -- what the assistant's `search_documents` tool sees.
pub(crate) async fn search_docs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<SearchDocsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.docs().ok_or_else(docs_unavailable)?;
    let hits = crate::library::search(
        &state,
        &req.query,
        req.top_k.unwrap_or(5).clamp(1, 20),
        req.document.as_deref(),
    )
    .await;
    Ok(Json(serde_json::json!({ "hits": hits })))
}
