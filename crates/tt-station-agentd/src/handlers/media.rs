// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Image generation, proxied to the served media model: `POST /generate/image`.

use std::time::Duration;

use crate::auth::BearerAuth;
use crate::error::{tt_error, ErrorResponse};
use crate::state::AppState;
use axum::{http::StatusCode, Json};

// ---------------------------------------------------------------------
// Image generation, proxied to the served media server
// ---------------------------------------------------------------------

/// The served image server's root URL (the endpoint without its `/v1`), after
/// checking the served model makes images.
async fn image_base(state: &AppState) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let _ = state.reconciled_status().await;
    let endpoint = state.endpoint().ok_or_else(|| {
        tt_error(
            StatusCode::CONFLICT,
            "Nothing is serving. Start an image model first.",
        )
    })?;
    let probe = state.clone();
    let served = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    if served != "image" {
        return Err(tt_error(
            StatusCode::CONFLICT,
            format!(
                "The box is serving {} ({served}), not image generation. Start an image model first.",
                endpoint.model
            ),
        ));
    }
    Ok(endpoint
        .base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string())
}

/// Forward one request to the media server and hand its status + body back.
async fn media_proxy(
    method: reqwest::Method,
    url: String,
    body: Option<serde_json::Value>,
    timeout: Duration,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    };
    // tt-media-server 0.10 images check `Authorization: Bearer $API_KEY` even
    // when launched with NO_AUTH (only newer servers honour it), and the box
    // never sets API_KEY, so the server's default applies. Newer servers with
    // NO_AUTH ignore the header.
    let api_key =
        std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
    let mut req = client.request(method, &url).bearer_auth(api_key);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return tt_error(
                StatusCode::GATEWAY_TIMEOUT,
                "The model took too long to answer.",
            )
            .into_response()
        }
        Err(e) => {
            return tt_error(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't reach the model server: {e}"),
            )
            .into_response()
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return tt_error(
                StatusCode::BAD_GATEWAY,
                format!("Reading the model's answer failed: {e}"),
            )
            .into_response()
        }
    };
    if !status.is_success() && content_type.starts_with("application/json") {
        // FastAPI's `{"detail": ...}` -> this API's `{"error": ...}`.
        let detail = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("detail").map(|d| {
                    d.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| d.to_string())
                })
            });
        if let Some(detail) = detail {
            return tt_error(status, detail).into_response();
        }
    }
    (
        status,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        bytes,
    )
        .into_response()
}

/// `POST /generate/image` (authed): tt-media-server's `/v1/images/generations`
/// body (`prompt`, `negative_prompt`, `num_inference_steps`, `seed`,
/// `number_of_images`, `guidance_scale`, `image_return_format`) passed through;
/// answers `{images: [base64...], generation_time}`.
pub(crate) async fn generate_image(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(mut body): Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Media servers (0.10) require `guidance_scale` (>= 1); FLUX schnell is
    // guidance-distilled, so the usual 3.5 is a safe default for clients that
    // don't set it.
    if let Some(obj) = body.as_object_mut() {
        obj.entry("guidance_scale")
            .or_insert(serde_json::json!(3.5));
    }
    match image_base(&state).await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::POST,
                format!("{base}/v1/images/generations"),
                Some(body),
                Duration::from_secs(600),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}
