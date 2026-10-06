//! A paired QuietBox, reached through its `tt-station-agentd` HTTP API.
//!
//! Reads (`/status`, `/models`, `/serving`, `/logs`, `/run/schema`) are unauthenticated. Writes
//! (`/run`, `/stop`, `/endpoint`) need the bearer token from pairing; a `RemoteStation` built
//! without one reports `paired == false` and refuses writes with a plain error.

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use libttstation::agent_client::{self, AgentClient};
use libttstation::model::ServingStatus;
use libttstation::run_options::{RunOptions, RunSchema};

use crate::station::{fits, ModelRow, ServingNow, StationId, StationKind, StationState};

/// One box, identified by its control-plane base URL.
pub struct RemoteStation {
    base: String,
    name: String,
    /// Pairing token, `None` until the box is paired.
    token: Option<String>,
    /// Text of the most recent refused run (HTTP 409). Set by [`RemoteStation::run`], cleared by
    /// the next successful run, and reported by every [`RemoteStation::state`] in between.
    contention: Mutex<Option<String>>,
}

impl RemoteStation {
    /// `base` is the agent's URL (`http://host:port`), `name` the box's display name, `token`
    /// the pairing token if there is one.
    pub fn new(base: impl Into<String>, name: impl Into<String>, token: Option<String>) -> Self {
        Self {
            base: base.into(),
            name: name.into(),
            token,
            contention: Mutex::new(None),
        }
    }

    /// `"<host>:<port>"`, taken from the base URL.
    pub fn id(&self) -> StationId {
        let trimmed = self
            .base
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        StationId(trimmed.to_string())
    }

    pub fn kind(&self) -> StationKind {
        StationKind::Remote {
            base: self.base.clone(),
        }
    }

    fn contention_now(&self) -> Option<String> {
        self.contention
            .lock()
            .expect("contention mutex poisoned")
            .clone()
    }

    fn client(&self) -> Result<AgentClient> {
        let token = self
            .token
            .as_deref()
            .ok_or_else(|| anyhow!("{} is not paired yet: pair with it first", self.name))?;
        Ok(AgentClient::new(self.base.clone(), token))
    }

    /// Snapshot of the box. A box that does not answer `/status` is `reachable == false` with
    /// no other fields known; the error is not surfaced.
    pub async fn state(&self) -> StationState {
        let mut state = StationState {
            id: self.id(),
            name: self.name.clone(),
            reachable: false,
            mesh: None,
            serving: None,
            paired: self.token.is_some(),
            contention: self.contention_now(),
        };
        let Ok(status) = agent_client::get_status(&self.base).await else {
            return state;
        };
        state.reachable = true;
        state.mesh = status.device_mesh;
        if let ServingStatus::Serving(model) = status.status {
            state.serving = Some(self.serving_now(&model).await);
        }
        state
    }

    /// Work out the `/v1` URL for a model the box says it is serving. Prefers the agent-started
    /// entry in `GET /serving` (unauthenticated). Falls back to `GET /endpoint` when paired. If
    /// neither answers, the URL is empty: the model name is still worth showing.
    async fn serving_now(&self, model: &str) -> ServingNow {
        if let Ok(list) = agent_client::list_serving(&self.base).await {
            if let Some(e) = list.serving.iter().find(|e| e.source == "agent") {
                return ServingNow {
                    model: e.model.clone(),
                    base_url: e.base_url.clone(),
                };
            }
        }
        if let Ok(client) = self.client() {
            if let Ok(ep) = client.endpoint().await {
                return ServingNow {
                    model: ep.model,
                    base_url: ep.base_url,
                };
            }
        }
        ServingNow {
            model: model.to_string(),
            base_url: String::new(),
        }
    }

    /// The box's servable models. `source` is `box` and `kind` is unknown, because the agent's
    /// `/models` reports neither.
    pub async fn models(&self) -> Result<Vec<ModelRow>> {
        let mesh = agent_client::get_status(&self.base).await?.device_mesh;
        let resp = agent_client::list_models(&self.base).await?;
        Ok(resp
            .models
            .into_iter()
            .map(|m| ModelRow {
                fits: fits(&m.devices, mesh.as_deref()),
                name: m.name,
                kind: None,
                source: "box".to_string(),
                hardware: m.devices,
                cached: m.downloaded,
            })
            .collect())
    }

    /// `POST /run` with `options` on the wire. A refusal (HTTP 409, another tenant holds the
    /// chips) returns `Err` with the agent's text, which names the holder, and is also kept for
    /// [`RemoteStation::state`] until a run succeeds.
    pub async fn run(&self, model: &str, force: bool, options: &RunOptions) -> Result<ServingNow> {
        let client = self.client()?;
        match client.run_with_options(model, force, options).await {
            Ok(ep) => {
                *self.contention.lock().expect("contention mutex poisoned") = None;
                Ok(ServingNow {
                    model: ep.model,
                    base_url: ep.base_url,
                })
            }
            Err(e) => {
                if agent_client::is_refusal(&e) {
                    *self.contention.lock().expect("contention mutex poisoned") =
                        Some(e.to_string());
                }
                Err(e)
            }
        }
    }

    pub async fn stop(&self) -> Result<()> {
        self.client()?.stop().await
    }

    /// The box's serving-container log, last `tail` lines.
    pub async fn logs(&self, tail: usize) -> Result<Vec<String>> {
        Ok(agent_client::get_logs(&self.base, "container", tail)
            .await?
            .lines)
    }

    /// `GET /run/schema`. An older agent answers 404, which comes back as an error.
    pub async fn schema(&self) -> Result<RunSchema> {
        agent_client::get_run_schema(&self.base).await
    }
}
