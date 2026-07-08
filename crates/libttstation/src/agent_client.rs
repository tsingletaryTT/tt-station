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

use crate::model::{
    ConfigSummary, Endpoint, LogsInfo, ModelsResponse, ServingList, ServingStatus, StatusInfo,
};
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

/// `GET /serving` (UNAUTHED, mirroring the agent's own route -- see
/// `tt-station-agentd::routes::get_serving`): every live
/// `tt-inference-server` `/v1` endpoint on the box, whoever launched it (the
/// agent, tt-studio, or a manual `run.py`), as a [`ServingList`].
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`list_models`]/[`get_status`]: `/serving` is unauthed read-only
/// discovery, so a caller that hasn't paired (no bearer token) can still see
/// what's serving. `tt serving` calls this directly instead of going through
/// `authed_client()`.
pub async fn list_serving(base: &str) -> anyhow::Result<ServingList> {
    let url = join(base, "serving");
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
/// extractor): the agent's current serving status (parsed via
/// [`ServingStatus::from_txt`]) plus (Task 3) its detected `device_mesh`
/// hint, bundled as a [`StatusInfo`].
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`list_models`] and [`crate::pairing::pair_init`]: a client that hasn't
/// paired yet (no bearer token to construct an `AgentClient` with) still
/// wants a live status dot for discovery/UI purposes, and the agent's
/// `/status` route doesn't require a token to answer that. `tt status`
/// (`crates/tt/src/main.rs::cmd_status`) calls this directly instead of
/// going through `authed_client()`, so a `tt status` on an unpaired box
/// works instead of failing with "no token stored".
///
/// `device_mesh` deserializes to `None` when the agent's JSON omits the key
/// entirely (serde's derive treats a missing `Option<T>` field as `None`
/// rather than an error) -- lets this keep working unmodified against
/// `mock-box`, whose `/status` fixture predates Task 2 and doesn't send the
/// key at all.
pub async fn get_status(base: &str) -> anyhow::Result<StatusInfo> {
    #[derive(Deserialize)]
    struct StatusResponse {
        status: String,
        device_mesh: Option<String>,
    }

    let url = join(base, "status");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

    let body: StatusResponse = resp.json().await?;
    Ok(StatusInfo {
        status: ServingStatus::from_txt(&body.status)?,
        device_mesh: body.device_mesh,
    })
}

/// `GET /config` (UNAUTHED, mirroring the agent's own route -- see
/// `tt-station-agentd::routes::get_config`, Task 5): the box's fully-resolved
/// serving config, redacted to a [`ConfigSummary`] (no `hf_token`, no
/// token-store contents -- see that type's docs).
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`list_models`]/[`get_status`]: `/config` is unauthed read-only discovery
/// (the GTK panel and `tt config` want to show "what will this box actually
/// serve with" before/without a pairing existing), so there's no bearer
/// token to hang an `AgentClient` off of yet.
pub async fn get_config(base: &str) -> anyhow::Result<ConfigSummary> {
    let url = join(base, "config");
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

    Ok(resp.json().await?)
}

/// `GET /logs?source=<source>&tail=<tail>` (UNAUTHED, mirroring the agent's
/// own route -- see `tt-station-agentd::routes::get_logs`, Task 2): the
/// trailing `tail` lines of the box's `source` log stream (`"container"` or
/// `"run"`), bundled as a [`LogsInfo`].
///
/// A FREE function rather than an `AgentClient` method, same reasoning as
/// [`list_models`]/[`get_status`]/[`get_config`]: `/logs` is unauthed
/// read-only discovery (an operator debugging a stuck serve wants to `tt
/// logs` a box it never bothered to pair with), so there's no bearer token to
/// hang an `AgentClient` off of yet. `join` doesn't know about query strings,
/// so the `?source=&tail=` suffix is appended directly onto its output.
pub async fn get_logs(base: &str, source: &str, tail: usize) -> anyhow::Result<LogsInfo> {
    let url = format!("{}?source={source}&tail={tail}", join(base, "logs"));
    let resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

    Ok(resp.json().await?)
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

/// `POST /ssh/authorize`'s response body, as decoded by
/// [`AgentClient::ssh_authorize`] -- see
/// `tt-station-agentd::routes::SshAuthorizeResponse` (Task 2) for the wire
/// shape this mirrors field-for-field.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SshAuthorizeResult {
    /// Always `true` on a `200` response (the agent errors out rather than
    /// returning `authorized: false`) -- kept as a field rather than
    /// collapsed away so the struct matches the agent's wire shape exactly.
    pub authorized: bool,
    /// The account the newly-installed key can `ssh` in as (the agent's
    /// RUN-USER, e.g. `ttuser` -- not necessarily whoever the client is
    /// paired as).
    pub ssh_user: String,
    /// Whether this exact key was already present before this call (a
    /// no-op) as opposed to freshly installed.
    pub already_present: bool,
}

/// Which existing key [`AgentClient::ssh_revoke`] should remove, mirroring
/// `tt-station-agentd::routes::SshRevokeRequest`'s `label`/`public_key`
/// pair -- but as an enum here rather than two `Option` fields, since a
/// caller always knows which one it means to send and an enum makes
/// "exactly one of these" a compile-time guarantee instead of a runtime
/// convention.
pub enum SshRevokeBy {
    /// Revoke by the `ttstation:<label>` marker `authorize` tagged the key
    /// with.
    Label(String),
    /// Revoke by the key material itself (the same public key string that
    /// was passed to `ssh_authorize`).
    PublicKey(String),
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
            // The "(409)" suffix is a stable marker consumers can match on
            // (e.g. the Mac app's `isIdleConflict`) to distinguish "authed
            // but idle" from an auth failure, without depending on the exact
            // human-readable wording around it.
            anyhow::bail!("no model is currently serving on this agent (409)");
        }

        let resp = resp
            .error_for_status()
            .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?;

        Ok(resp.json().await?)
    }

    /// `POST /ssh/authorize { "public_key": "...", "label": "..." }`
    /// (bearer-guarded, same gate as `run`/`stop`): install `public_key`
    /// into the agent's `authorized_keys` under `label`, returning the
    /// decoded [`SshAuthorizeResult`] -- see
    /// `tt-station-agentd::routes::ssh_authorize` (Task 2) for the
    /// server-side counterpart this drives.
    pub async fn ssh_authorize(
        &self,
        public_key: &str,
        label: &str,
    ) -> anyhow::Result<SshAuthorizeResult> {
        #[derive(Serialize)]
        struct SshAuthorizeRequest<'a> {
            public_key: &'a str,
            label: &'a str,
        }

        let url = join(&self.base, "ssh/authorize");
        let resp = self
            .send(
                reqwest::Client::new()
                    .post(&url)
                    .json(&SshAuthorizeRequest { public_key, label }),
                &url,
            )
            .await?;

        Ok(resp.json().await?)
    }

    /// `DELETE /ssh/authorize { "label": "..." }` or `{ "public_key": "..." }`
    /// (bearer-guarded, same gate as `run`/`stop`): remove a previously
    /// -installed key identified by `by`, either its `label` or the key
    /// material itself -- see `tt-station-agentd::routes::ssh_revoke`
    /// (Task 2). The agent's response is just `{"revoked": true}` on
    /// success -- nothing to parse out of it, same as [`AgentClient::stop`].
    pub async fn ssh_revoke(&self, by: SshRevokeBy) -> anyhow::Result<()> {
        #[derive(Serialize)]
        struct SshRevokeRequest {
            #[serde(skip_serializing_if = "Option::is_none")]
            label: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            public_key: Option<String>,
        }

        let body = match by {
            SshRevokeBy::Label(label) => SshRevokeRequest {
                label: Some(label),
                public_key: None,
            },
            SshRevokeBy::PublicKey(public_key) => SshRevokeRequest {
                label: None,
                public_key: Some(public_key),
            },
        };

        let url = join(&self.base, "ssh/authorize");
        self.send(reqwest::Client::new().delete(&url).json(&body), &url)
            .await?;
        Ok(())
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
