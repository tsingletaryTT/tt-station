// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The `{ "error": "<message>" }` body the agent's routes answer with when
//! a request fails, and the helpers that build it.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

use crate::state::StartInFlight;

/// JSON body returned when a control route fails after auth passes (backend
/// `start`/`stop` error, or a `spawn_blocking` join failure). Kept as a
/// simple `{ "error": "<message>" }` shape -- there's exactly one consumer
/// (Task 11's `AgentClient`) and it doesn't need anything richer than a
/// human-readable reason to log/surface.
#[derive(Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
}

/// A request that failed: its status, and the message the
/// `{ "error": "<message>" }` body carries. Routes return it as their `Err`;
/// the body is the same bytes as an [`ErrorResponse`] or a
/// `json!({ "error": .. })`, which the modules used to build by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub error: String,
}

impl ApiError {
    pub fn new(status: StatusCode, error: impl Into<String>) -> Self {
        ApiError {
            status,
            error: error.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ErrorResponse { error: self.error })).into_response()
    }
}

/// For a route that still answers `(StatusCode, Json<Value>)` (its errors can
/// carry more than a message) and passes on an `ApiError` from one that doesn't.
impl From<ApiError> for (StatusCode, Json<serde_json::Value>) {
    fn from(e: ApiError) -> Self {
        (e.status, Json(serde_json::json!({ "error": e.error })))
    }
}

/// [`ApiError::new`], by the name the route modules' own helpers had.
pub(crate) fn err(status: StatusCode, error: impl Into<String>) -> ApiError {
    ApiError::new(status, error)
}

pub(crate) fn backend_error(err: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: err.to_string(),
        }),
    )
}

/// `backend_error`, except that a failure caused by ANOTHER TENANT HOLDING
/// CHIPS answers `409 Conflict` instead of `500`.
///
/// Nothing went wrong in those cases: the box's current state conflicts with
/// what was asked (`POST /run` on a fully-leased box, `POST /reset` or
/// `POST /power reset-chips` while a neighbour holds a lease), which is
/// exactly what 409 means -- and it is the same code `GET /endpoint` already
/// uses for "well-formed request, wrong box state". A client can retry a 409
/// after the holder finishes; a 500 tells it to go looking for a bug.
///
/// Detected by walking the error chain rather than matching on message text,
/// so a `.context(..)` added anywhere between the backend and here cannot
/// silently turn a 409 back into a 500. The message is passed through
/// verbatim -- it already names the board and the holder, and (deliberately)
/// no duration, since gozer exposes none. Used by the three routes that can
/// contend; every other route keeps `backend_error`.
pub(crate) fn contention_aware_error(err: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    if err
        .chain()
        .any(|cause| cause.is::<crate::gozer::Contention>() || cause.is::<StartInFlight>())
    {
        return (
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: err.to_string(),
            }),
        );
    }
    backend_error(err)
}

/// Build the small `{ "error": "<message>" }` response for a `400 Bad
/// Request` -- a validation failure the CALLER can fix (a bad/private key,
/// a malformed label, a revoke request naming neither `label` nor
/// `public_key`), as opposed to `backend_error`'s `500`, which is for
/// failures on THIS box's side (a backend/I-O error after auth and
/// validation both passed).
pub(crate) fn bad_request(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse { error: message }),
    )
}

pub(crate) fn tt_error(
    status: StatusCode,
    error: impl Into<String>,
) -> (StatusCode, Json<ErrorResponse>) {
    (
        status,
        Json(ErrorResponse {
            error: error.into(),
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn bytes(response: Response) -> (StatusCode, String, Vec<u8>) {
        let status = response.status();
        let content_type = response.headers()["content-type"]
            .to_str()
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        (status, content_type, body)
    }

    /// The same status, content type and body bytes as both shapes it replaces.
    #[tokio::test]
    async fn an_api_error_is_the_body_the_modules_built_by_hand() {
        for message in [
            "No such run.",
            "",
            "quotes \" and \\ backslashes",
            "lines\nand\ttabs",
            "unicode — “curly” 😀",
        ] {
            let status = StatusCode::CONFLICT;
            let ours = bytes(err(status, message).into_response()).await;
            let value = bytes((status, Json(json!({ "error": message }))).into_response()).await;
            let strukt = bytes(
                (
                    status,
                    Json(ErrorResponse {
                        error: message.to_string(),
                    }),
                )
                    .into_response(),
            )
            .await;
            assert_eq!(ours, value, "{message:?}");
            assert_eq!(ours, strukt, "{message:?}");
        }
    }
}
