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

use crate::model::{Endpoint, ServingStatus};
use crate::pairing::join;
use serde::{Deserialize, Serialize};

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

    /// `GET /status`: the agent's name, chip inventory, and current serving
    /// status. Only the `status` field (`idle` / `serving:<model>`) is
    /// returned here -- name/chips matter for discovery (Task 4), not for a
    /// client that's already paired with a specific box.
    pub async fn status(&self) -> anyhow::Result<ServingStatus> {
        #[derive(Deserialize)]
        struct StatusResponse {
            status: String,
        }

        let resp = self.get(&join(&self.base, "status")).await?;
        let body: StatusResponse = resp.json().await?;
        ServingStatus::from_txt(&body.status)
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

    /// Shared `GET` helper built on [`Self::send`]: attach the bearer
    /// header, issue the request, and turn any non-2xx status into a clear
    /// `anyhow::Error` that names the URL.
    async fn get(&self, url: &str) -> anyhow::Result<reqwest::Response> {
        self.send(reqwest::Client::new().get(url), url).await
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
