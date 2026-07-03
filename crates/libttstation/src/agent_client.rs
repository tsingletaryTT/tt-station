//! Client side of the agent control plane (Task 10's routes on
//! `tt-station-agentd`): `GET /status`, `POST /run`, `POST /stop`, and
//! `GET /endpoint`. Lives in `libttstation` for the same reason
//! [`crate::pairing`] does -- so any future caller (the `tt` CLI in Task 12,
//! a GUI, another service) can drive an agent without reimplementing the
//! HTTP calls.
//!
//! Every request carries `Authorization: Bearer <token>`. Per the brief,
//! `/status` isn't currently bearer-gated on the agent side, but sending the
//! header anyway costs nothing and keeps all four calls uniform.

use crate::model::{Endpoint, ModelsResponse, ServingStatus};
use crate::pairing::join;
use serde::{Deserialize, Serialize};

/// `GET /models` (UNAUTHED, mirroring the agent's own route -- see
/// `tt-station-agentd::routes::get_models`): enumerate the models the agent
/// at `base` can serve, so a caller (the `tt` CLI's `models` command today)
/// never has to guess or hardcode a model id before calling
/// [`AgentClient::run`].
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`crate::pairing::pair_init`]: no bearer token exists yet at this point
/// in a fresh interaction (a client may want to see what's servable BEFORE
/// pairing), and the agent's `/models` route doesn't require one anyway.
pub async fn list_models(base: &str) -> anyhow::Result<ModelsResponse> {
    let url = join(base, "models");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

    Ok(resp.json().await?)
}

/// `GET /status` (UNAUTHED, mirroring the agent's own route -- see
/// `tt-station-agentd::routes::get_status`, which has no `BearerAuth`
/// extractor): the agent's current serving status, parsed via
/// [`ServingStatus::from_txt`].
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`list_models`] and [`crate::pairing::pair_init`]: a client that hasn't
/// paired yet (no bearer token to construct an `AgentClient` with) still
/// wants a live status dot for discovery/UI purposes, and the agent's
/// `/status` route doesn't require a token to answer that. `tt status`
/// (`crates/tt/src/main.rs::cmd_status`) calls this directly instead of
/// going through `authed_client()`, so a `tt status` on an unpaired box
/// works instead of failing with "no token stored".
pub async fn get_status(base: &str) -> anyhow::Result<ServingStatus> {
    #[derive(Deserialize)]
    struct StatusResponse {
        status: String,
    }

    let url = join(base, "status");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

    let body: StatusResponse = resp.json().await?;
    ServingStatus::from_txt(&body.status)
}

/// `POST /reset` (bearer-guarded): ask the agent at `base` to return the box
/// to a fresh-install state -- stop any serving container, reset the board,
/// and clear ALL of its issued bearer tokens (see
/// `tt-station-agentd::routes::reset`). Used by `tt reset --host <h>` to reset
/// the remote box before it forgets its local copy of the token.
///
/// A FREE function rather than an [`AgentClient`] method, mirroring the
/// action-command style of [`list_models`]/[`get_status`]: the one caller
/// (`tt reset`) already has the `host`+`token` in hand and doesn't otherwise
/// build an `AgentClient`, and `reset` deliberately invalidates the very
/// token it authenticates with (a fresh box has no tokens), so there's no
/// reusable authenticated handle to hang onto afterward anyway.
///
/// The agent responds `{}` on success -- nothing to parse out of it.
pub async fn reset(base: &str, token: &str) -> anyhow::Result<()> {
    let url = join(base, "reset");
    reqwest::Client::new()
        .post(&url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;
    Ok(())
}

/// A handle to one paired agent: its control-plane base URL plus the bearer
/// token minted for it by [`crate::pairing::pair_complete`].
pub struct AgentClient {
    base: String,
    token: String,
}

impl AgentClient {
    /// Build a client for the agent at `base` (with or without a trailing
    /// slash -- see [`join`]), authenticating with `token`.
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            token: token.into(),
        }
    }

    /// `POST /run { "model": "..." }`: ask the agent to start serving
    /// `model`, returning the resulting [`Endpoint`].
    pub async fn run(&self, model: &str) -> anyhow::Result<Endpoint> {
        #[derive(Serialize)]
        struct RunRequest<'a> {
            model: &'a str,
        }

        #[derive(Deserialize)]
        struct RunResponse {
            endpoint: Endpoint,
        }

        let url = join(&self.base, "run");
        let resp = self
            .send(
                reqwest::Client::new()
                    .post(&url)
                    .json(&RunRequest { model }),
                &url,
            )
            .await?;

        let body: RunResponse = resp.json().await?;
        Ok(body.endpoint)
    }

    /// `POST /stop`: ask the agent to stop whatever's currently serving.
    /// The agent's response body is just `{}` on success -- nothing to
    /// parse out of it.
    pub async fn stop(&self) -> anyhow::Result<()> {
        let url = join(&self.base, "stop");
        self.send(reqwest::Client::new().post(&url), &url).await?;
        Ok(())
    }

    /// `GET /endpoint`: the [`Endpoint`] of whatever's currently serving.
    /// The agent returns `409 Conflict` specifically when idle (see Task
    /// 10's `get_endpoint` handler); that's mapped here to a distinct,
    /// human-readable "no model is serving" error rather than the generic
    /// "409" a bare `error_for_status` would produce, since "nothing is
    /// serving" is a meaningfully different (and expected!) outcome from an
    /// actual network/server error. Any other non-2xx still goes through
    /// the shared status-including error path.
    pub async fn endpoint(&self) -> anyhow::Result<Endpoint> {
        let url = join(&self.base, "endpoint");
        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::CONFLICT {
            anyhow::bail!("no model is currently serving on this agent");
        }

        let resp = resp
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

        Ok(resp.json().await?)
    }

    /// Shared request-sending helper for every method above: attach the
    /// bearer header to an in-progress request, send it, and turn any
    /// non-2xx status into a clear `anyhow::Error` naming `url` (kept
    /// separate from the builder since `reqwest::RequestBuilder` doesn't
    /// expose the URL it was built with).
    async fn send(
        &self,
        req: reqwest::RequestBuilder,
        url: &str,
    ) -> anyhow::Result<reqwest::Response> {
        req.bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))
    }
}
