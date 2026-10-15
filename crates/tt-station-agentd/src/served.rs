// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What a model server says it serves, from its OpenAI-style `/v1/models`
//! listing: one probe and one reading of it, shared by everything that asks
//! (Hermes, research, evals, Code's speed, community packages, discovery and
//! the reconciled status).

use std::time::Duration;

use serde_json::Value;

use crate::state::AppState;

/// A server's answer to `GET /v1/models`.
#[derive(Debug, Clone)]
pub struct Listing {
    /// Whether it answered with a 2xx status.
    pub success: bool,
    /// The listing as it came back.
    pub body: Value,
}

/// The model a listing names first, where it was asked, and its context window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServedModel {
    /// The server's root, e.g. `http://127.0.0.1:8000` (its API is under `/v1`).
    pub endpoint: String,
    /// `data[0].id`, if the listing has one.
    pub model: Option<String>,
    /// `data[0].max_model_len`, if the server reports it (vLLM does).
    pub context: Option<u64>,
}

/// The root of this box's own serving port.
pub fn local_root(state: &AppState) -> String {
    format!("http://127.0.0.1:{}", state.serving_port())
}

/// `GET {root}/v1/models` on the shared local client, within `timeout`.
/// `None` when the server can't be reached or doesn't answer with JSON;
/// any status otherwise (callers that need a 2xx check [`Listing::success`]).
pub async fn list(root: &str, timeout: Duration) -> Option<Listing> {
    let response = crate::localhttp::local()
        .get(format!("{root}/v1/models"))
        .timeout(timeout)
        .send()
        .await
        .ok()?;
    let success = response.status().is_success();
    let body = response.json::<Value>().await.ok()?;
    Some(Listing { success, body })
}

/// The first model in a `/v1/models` listing asked at `endpoint`.
pub fn first(endpoint: &str, body: &Value) -> ServedModel {
    let first = &body["data"][0];
    ServedModel {
        endpoint: endpoint.to_string(),
        model: first["id"].as_str().map(str::to_string),
        context: first["max_model_len"].as_u64(),
    }
}

/// What this box's serving port says it serves, within `timeout`: `None`
/// when it can't be reached or doesn't answer with JSON.
pub async fn local(state: &AppState, timeout: Duration) -> Option<ServedModel> {
    let root = local_root(state);
    let listing = list(&root, timeout).await?;
    Some(first(&root, &listing.body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_model_and_its_window_are_read_off_the_listing() {
        let body = json!({ "data": [
            { "id": "Qwen/Qwen3-32B", "max_model_len": 40960 },
            { "id": "other" },
        ] });
        assert_eq!(
            first("http://127.0.0.1:8000", &body),
            ServedModel {
                endpoint: "http://127.0.0.1:8000".into(),
                model: Some("Qwen/Qwen3-32B".into()),
                context: Some(40960),
            }
        );
    }

    #[test]
    fn an_empty_or_odd_listing_names_nothing() {
        for body in [
            json!({ "data": [] }),
            json!({}),
            json!({ "data": {} }),
            json!({ "data": [{ "id": 7 }] }),
            json!({ "object": "error", "message": "loading" }),
        ] {
            let m = first("x", &body);
            assert_eq!((m.model, m.context), (None, None), "{body}");
        }
    }
}
