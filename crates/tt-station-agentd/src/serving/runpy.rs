//! `run.py` serving backend -- the DEFAULT `ServingBackend` and the one that
//! matches how the operator's PROVEN scripts actually launch LLMs (see
//! `docs/reference/tt-inference-server-docker.md`'s "⭐ Ground truth: launch
//! via run.py" section). Rather than a hand-rolled `docker run` (that's
//! `DockerBackend`, kept as a best-effort fallback), this shells out to
//! `tt-inference-server/run.py`, which itself resolves the model against
//! `model_spec.json`, builds the container, and wires up the device mesh,
//! hugepages, cache binds, and auth -- all the things `DockerBackend` has to
//! approximate by hand.
//!
//! Like `DockerBackend`, process execution and the health probe are routed
//! through `CommandRunner` (defined in `serving::docker`, reused here rather
//! than duplicated) so tests can inject a fake instead of shelling out to a
//! real `python3`/`docker` binary or making real HTTP requests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use libttstation::model::{Endpoint, ServingStatus};

use super::docker::CommandRunner;
use super::ServingBackend;

/// How many times `start` polls the health endpoint before giving up. See
/// `docker::DEFAULT_HEALTH_POLL_ATTEMPTS` -- kept as a separate constant
/// (rather than shared) since `run.py`'s bring-up is a strict superset of
/// `docker run`'s (it builds/resolves the container itself first), so the
/// two backends' defaults are free to diverge later even though they match
/// today. `RunPyBackend::with_health_poll` overrides both for tests.
const DEFAULT_HEALTH_POLL_ATTEMPTS: u32 = 40;

/// Delay between health-poll attempts. See `DEFAULT_HEALTH_POLL_ATTEMPTS`.
const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Everything `RunPyBackend` needs to build the real `run.py` invocation
/// documented in `docs/reference/tt-inference-server-docker.md`. Grouped
/// into one struct (rather than a growing `RunPyBackend::new` argument
/// list) for the same reason `DockerConfig` is: `main.rs` builds this
/// straight from CLI flags, and tests build it from `Default::default()`
/// plus targeted overrides.
#[derive(Clone, Debug)]
pub struct RunPyConfig {
    /// Local checkout of `tt-inference-server` -- `run.py` is invoked as a
    /// relative path from inside this directory (`python3 run.py ...`), so
    /// this is passed to `CommandRunner::run_in_dir` as the working
    /// directory rather than baked into the argv itself. Operator
    /// convention: prefer `<checkout>/vendor/tt-inference-server`, else
    /// `$HOME/code/tt-inference-server` -- see `main.rs`.
    pub repo_dir: String,
    /// Host the serving container is reachable on -- baked into the
    /// returned `Endpoint`'s `base_url`, same role as `DockerConfig::host`.
    pub host: String,
    /// `--service-port`: the host port `run.py` publishes the OpenAI
    /// server on. Also what the health poll and `stop`'s `docker ps
    /// --filter publish=<port>` both key off of.
    pub service_port: u16,
    /// `--tt-device` value, e.g. `p300x2` (this box), `p300` (single card),
    /// `p150x4` (the OTHER Blackhole QuietBox variant). See the doc's
    /// "Device string is box- AND model-specific" section.
    pub tt_device: String,
    /// `--override-docker-image`: the image `run.py` runs the resolved
    /// model in, once it's done resolving `--model`/`--tt-device` against
    /// `model_spec.json`.
    pub image: String,
    /// `--engine`, e.g. `vllm`. Defaults to `"vllm"` -- the only engine the
    /// operator's scripts and this codebase's docs cover today.
    pub engine: String,
    /// `--impl`, e.g. `tt-transformers`. Defaults to `"tt-transformers"`.
    pub impl_name: String,
    /// `--host-hf-cache`: host path bind-mounted for the Hugging Face
    /// weights cache, e.g. `$HOME/.cache/huggingface`.
    pub host_hf_cache: String,
    /// When `true`, `run.py` is invoked with `--no-auth` -- same PoC
    /// default and same `Endpoint.requires_key = !no_auth` relationship as
    /// `DockerConfig::no_auth`.
    pub no_auth: bool,
    /// `--device-id`, e.g. `Some("0,1")` to pin to specific chips. Omitted
    /// from the argv entirely when `None` or empty -- most runs let
    /// `run.py` pick the device mesh itself.
    pub device_ids: Option<String>,
    /// `MODEL_SOURCE` environment variable `run.py` expects, e.g.
    /// `"huggingface"`. NOT part of the argv -- see `start`'s doc comment
    /// for how this actually reaches the child process.
    pub model_source: String,
}

impl Default for RunPyConfig {
    /// Defaults chosen to make hermetic tests concise (override only the
    /// field a given test cares about via struct-update syntax); NOT
    /// necessarily what a real deployment should run unexamined -- `main.rs`
    /// builds a `RunPyConfig` from explicit CLI flags rather than relying on
    /// this impl.
    fn default() -> Self {
        RunPyConfig {
            repo_dir: "tt-inference-server".to_string(),
            host: "127.0.0.1".to_string(),
            service_port: 8000,
            tt_device: "p300x2".to_string(),
            image: "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:unset".to_string(),
            engine: "vllm".to_string(),
            impl_name: "tt-transformers".to_string(),
            host_hf_cache: "~/.cache/huggingface".to_string(),
            no_auth: true,
            device_ids: None,
            model_source: "huggingface".to_string(),
        }
    }
}

/// `run.py`-backed `ServingBackend`: the DEFAULT backend (see `main.rs`'s
/// `--backend` flag). Invokes `python3 run.py ...` from inside
/// `RunPyConfig::repo_dir`, polls `GET /health` until the resulting server
/// is up, and stops it later by finding whatever container `run.py` started
/// via `docker ps --filter publish=<port>` -- mirroring the operator's own
/// `start_artgen.sh --stop`, since `run.py` doesn't hand back a predictable
/// container name the way `DockerBackend`'s own `--name` does.
pub struct RunPyBackend {
    config: RunPyConfig,
    runner: Box<dyn CommandRunner>,
    /// Tracks the last-known serving status in-process. Same rationale as
    /// `DockerBackend::status` -- see that field's doc comment.
    status: Arc<Mutex<ServingStatus>>,
    health_poll_attempts: u32,
    health_poll_interval: Duration,
}

impl RunPyBackend {
    /// Build a `RunPyBackend` with production health-poll defaults. Starts
    /// `Idle` -- constructing a backend never implies anything is already
    /// serving.
    pub fn new(config: RunPyConfig, runner: Box<dyn CommandRunner>) -> Self {
        RunPyBackend {
            config,
            runner,
            status: Arc::new(Mutex::new(ServingStatus::Idle)),
            health_poll_attempts: DEFAULT_HEALTH_POLL_ATTEMPTS,
            health_poll_interval: DEFAULT_HEALTH_POLL_INTERVAL,
        }
    }

    /// Override the health-poll bound used by `start`. Same rationale (and
    /// same `#[cfg]` gating) as `DockerBackend::with_health_poll`.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn with_health_poll(mut self, attempts: u32, interval: Duration) -> Self {
        self.health_poll_attempts = attempts;
        self.health_poll_interval = interval;
        self
    }
}

impl ServingBackend for RunPyBackend {
    fn start(&self, model: &str) -> Result<Endpoint> {
        // Built as owned `String`s (several pieces are computed at
        // runtime) then borrowed as `&str` for `CommandRunner::run_in_dir`,
        // which takes `&[&str]` -- argv-style, no shell involved, so
        // callers never need to worry about quoting.
        //
        // Order matches `docs/reference/tt-inference-server-docker.md`'s
        // "⭐ Ground truth" invocation exactly, so a diff against that doc
        // is trivial to eyeball.
        let mut args: Vec<String> = vec![
            "python3".to_string(),
            "run.py".to_string(),
            "--model".to_string(),
            model.to_string(),
            "--workflow".to_string(),
            "server".to_string(),
            "--tt-device".to_string(),
            self.config.tt_device.clone(),
            "--impl".to_string(),
            self.config.impl_name.clone(),
            "--engine".to_string(),
            self.config.engine.clone(),
            "--docker-server".to_string(),
            "--override-docker-image".to_string(),
            self.config.image.clone(),
        ];
        if self.config.no_auth {
            args.push("--no-auth".to_string());
        }
        args.push("--service-port".to_string());
        args.push(self.config.service_port.to_string());
        args.push("--host-hf-cache".to_string());
        args.push(self.config.host_hf_cache.clone());
        if let Some(ids) = self
            .config
            .device_ids
            .as_ref()
            .filter(|ids| !ids.is_empty())
        {
            args.push("--device-id".to_string());
            args.push(ids.clone());
        }

        // `run.py` reads `MODEL_SOURCE` from its environment, not from an
        // argv flag (see docs/reference/tt-inference-server-docker.md's
        // `MODEL_SOURCE=huggingface python3 run.py ...` invocation).
        // `CommandRunner::run_in_dir`'s signature is argv-only -- it has no
        // env parameter, on purpose, so it stays a straightforward argv-in
        // seam for BOTH backends -- so this sets the var on the CURRENT
        // process instead. `RealCommandRunner::run_in_dir` builds its
        // `std::process::Command` without ever clearing the environment,
        // and a freshly-spawned `Command` inherits the parent's environment
        // by default, so the child sees it. `CommandRunner` fakes (e.g.
        // `tests/support/mod.rs`'s `FakeRunner`) never spawn a real process
        // at all, so this is a no-op from their point of view -- exactly
        // why the task description says the env need not be captured by
        // tests, only the argv.
        std::env::set_var("MODEL_SOURCE", &self.config.model_source);

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run_in_dir(&self.config.repo_dir, &arg_refs)?;

        // `run.py` (and `DockerBackend`) poll `/health`, not `/v1/models` --
        // see docs/reference/tt-inference-server-docker.md.
        let health_url = format!(
            "http://{}:{}/health",
            self.config.host, self.config.service_port
        );
        let healthy = super::poll_until_healthy(
            self.runner.as_ref(),
            &health_url,
            self.health_poll_attempts,
            self.health_poll_interval,
        );

        if !healthy {
            return Err(anyhow::anyhow!(
                "runpy backend: model '{model}' did not become healthy at {health_url} \
                 within {} attempts",
                self.health_poll_attempts
            ));
        }

        *self.status.lock().expect("status mutex poisoned") =
            ServingStatus::Serving(model.to_string());

        Ok(Endpoint {
            base_url: format!(
                "http://{}:{}/v1",
                self.config.host, self.config.service_port
            ),
            model: model.to_string(),
            // Auth is required exactly when `run.py` was NOT invoked with
            // `--no-auth`.
            requires_key: !self.config.no_auth,
        })
    }

    fn stop(&self, _model: &str) -> Result<()> {
        // `run.py` doesn't hand back a predictable container name the way
        // `DockerBackend`'s own `--name` flag does, so -- exactly like the
        // operator's `start_artgen.sh --stop` -- find whatever container is
        // publishing our configured port and stop it directly. Empty
        // `docker ps` output (nothing running) is treated as success: `stop`
        // is idempotent, same contract as `DockerBackend::stop`.
        let publish_filter = format!("publish={}", self.config.service_port);
        let ps_output = self
            .runner
            .run(&["docker", "ps", "--filter", &publish_filter, "-q"])?;

        for container_id in ps_output.split_whitespace() {
            self.runner.run(&["docker", "stop", container_id])?;
        }

        *self.status.lock().expect("status mutex poisoned") = ServingStatus::Idle;
        Ok(())
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(self.status.lock().expect("status mutex poisoned").clone())
    }
}
