// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The agent's own health and version, for supervisors and clients.
//!
//! `GET /healthz` is for whatever watches the process (a load balancer, a
//! monitoring check, `curl` in a deploy script): unauthenticated and minimal --
//! `200 ok` when the process answers and its config dir is readable, `503`
//! otherwise, and nothing else (no model, no paths, no versions) since anyone
//! who can reach the port can ask.
//!
//! `/status` carries the versions a client compares against:
//!
//! * `agent_version` -- this build (the crate version, as `/updates` reports it);
//! * `api_version` -- the HTTP API generation this agent speaks;
//! * `min_client` -- the oldest client `api_version` this agent still serves.
//!   A client built for an `api_version` below it should tell the person to
//!   update the app rather than fail on routes that changed shape. (Client
//!   side -- desktop and iOS reading it -- is follow-up work.)

use axum::http::StatusCode;

use crate::routes::AppState;

/// This build of the agent.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The HTTP API generation. Bump it when a route changes shape incompatibly
/// (additive fields don't count), and raise [`MIN_CLIENT`] if older clients
/// can no longer work.
pub const API_VERSION: u32 = 1;

/// The oldest client API generation this agent still serves.
pub const MIN_CLIENT: u32 = 1;

/// `GET /healthz` (unauthed): `ok`, or `503` when the config dir can't be read.
pub async fn healthz(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (StatusCode, &'static str) {
    let dir = state.config_dir();
    let readable = tokio::task::spawn_blocking(move || std::fs::read_dir(dir).is_ok())
        .await
        .unwrap_or(false);
    if readable {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    }
}
