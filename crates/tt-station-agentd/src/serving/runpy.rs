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
//! ## Defer to `run.py`, don't second-guess it
//!
//! `run.py` was verified (on real hardware) to auto-resolve everything but
//! the model itself:
//!   - `--model` is REQUIRED and validated against `model_spec.json`.
//!   - `--tt-device` is OPTIONAL -- "Defaults to the largest supported
//!     device available on the host," i.e. `run.py` detects the box's
//!     hardware and picks the mesh itself.
//!   - `--engine`/`--impl` are OPTIONAL -- default to the model's own entry
//!     in `model_spec.json`.
//!   - `--override-docker-image` is OPTIONAL -- without it, `run.py` picks
//!     the correct image from the model config itself (the flag name says
//!     it all: it's an override, not a requirement).
//!
//! So `RunPyConfig`'s device/image/impl/engine/device-id fields are all
//! `Option<String>`, and `start` only appends the corresponding `run.py`
//! flag when a caller has explicitly set one -- the DEFAULT invocation
//! carries none of them, letting `run.py` do exactly the auto-resolution it
//! was built to do. Hardcoding a guessed device string or image tag here
//! would just be a worse, staler copy of logic `run.py` already gets right
//! from `model_spec.json` plus real hardware detection.
//!
//! Like `DockerBackend`, process execution and the health probe are routed
//! through `CommandRunner` (defined in `serving::docker`, reused here rather
//! than duplicated) so tests can inject a fake instead of shelling out to a
//! real `python3`/`docker` binary or making real HTTP requests.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};

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
///
/// The device/image/impl/engine/device-id fields are all `Option<String>`
/// -- see the module doc's "Defer to `run.py`, don't second-guess it"
/// section. `None` means "let `run.py` decide," which is the DEFAULT for
/// every one of them; a caller sets `Some(..)` only to deliberately
/// override `run.py`'s own resolution.
#[derive(Clone, Debug)]
pub struct RunPyConfig {
    /// Local checkout of `tt-inference-server` -- `run.py` is invoked as a
    /// relative path from inside this directory (`python3 run.py ...`), so
    /// this is passed to `CommandRunner::run_in_dir_with_env` as the working
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
    /// When `true`, `run.py` is invoked with `--no-auth` -- same PoC
    /// default and same `Endpoint.requires_key = !no_auth` relationship as
    /// `DockerConfig::no_auth`.
    pub no_auth: bool,
    /// `MODEL_SOURCE` environment variable `run.py` expects, e.g.
    /// `"huggingface"`. NOT part of the argv -- see `start`'s doc comment
    /// for how this actually reaches the child process.
    pub model_source: String,
    /// `--host-hf-cache`: host path bind-mounted for the Hugging Face
    /// weights cache, e.g. `$HOME/.cache/huggingface`. `Some` in normal use
    /// (there's no hardware-dependent "right" default `run.py` could guess
    /// here the way it can for the device mesh), but kept `Option` so a
    /// caller that genuinely wants to omit it (e.g. a test) can.
    pub host_hf_cache: Option<String>,
    /// `--tt-device` value, e.g. `p300x2` (this box), `p300` (single card),
    /// `p150x4` (the OTHER Blackhole QuietBox variant). `None` (the
    /// default) lets `run.py` auto-detect "the largest supported device
    /// available on the host" itself -- see the module doc. `Some(..)`
    /// OVERRIDES that auto-detection.
    pub tt_device: Option<String>,
    /// `--override-docker-image`: the image `run.py` runs the resolved
    /// model in. `None` (the default) lets `run.py` pick the correct image
    /// from the model's own `model_spec.json` entry -- the flag is an
    /// override, not a requirement. `Some(..)` forces a specific image.
    pub image: Option<String>,
    /// `--impl`, e.g. `tt-transformers`. `None` (the default) lets `run.py`
    /// fall back to the model spec's own implementation choice.
    pub impl_name: Option<String>,
    /// `--engine`, e.g. `vllm`. `None` (the default) lets `run.py` fall
    /// back to the model spec's own engine choice.
    pub engine: Option<String>,
    /// `--device-id`, e.g. `Some("0,1")` to pin to specific chips. Omitted
    /// from the argv entirely when `None` -- most runs let `run.py` pick
    /// the device mesh itself.
    pub device_id: Option<String>,
    /// Path to `model_spec.json`, read by `list_models` to enumerate what
    /// this box can serve (see `ServingBackend::list_models`). `None` (the
    /// default) resolves to `<repo_dir>/model_spec.json` at call time --
    /// see `list_models`'s doc comment.
    pub model_spec_path: Option<String>,
}

impl Default for RunPyConfig {
    /// Defaults chosen to make hermetic tests concise (override only the
    /// field a given test cares about via struct-update syntax); NOT
    /// necessarily what a real deployment should run unexamined -- `main.rs`
    /// builds a `RunPyConfig` from explicit CLI flags rather than relying on
    /// this impl.
    ///
    /// Every device/image/impl/engine/device-id field defaults to `None` --
    /// deliberately, since that's also what a REAL default deployment wants
    /// (see the module doc): letting `run.py` auto-resolve them all rather
    /// than this codebase guessing on its behalf.
    fn default() -> Self {
        RunPyConfig {
            repo_dir: "tt-inference-server".to_string(),
            host: "127.0.0.1".to_string(),
            service_port: 8000,
            no_auth: true,
            model_source: "huggingface".to_string(),
            // NOTE: a literal `~` is test-fixture shorthand only -- it's
            // passed straight through as an argv value with no shell in
            // between to expand it, so this would be a broken path if it
            // ever reached a real `run.py` invocation. The REAL runtime
            // default is `main.rs::default_host_hf_cache`, which resolves
            // `$HOME` itself via `std::env::var("HOME")` before this struct
            // is ever built from CLI flags.
            host_hf_cache: Some("~/.cache/huggingface".to_string()),
            tt_device: None,
            image: None,
            impl_name: None,
            engine: None,
            device_id: None,
            model_spec_path: None,
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

    /// Resolve where `model_spec.json` lives: `config.model_spec_path` if
    /// explicitly set, else `<repo_dir>/model_spec.json` -- the file
    /// `run.py` itself validates `--model`/`--tt-device` against (see the
    /// module doc), so this is the same ground truth `list_models` reads to
    /// answer "what can this box serve" without a caller ever having to
    /// duplicate that catalog by hand.
    fn model_spec_path(&self) -> String {
        self.config
            .model_spec_path
            .clone()
            .unwrap_or_else(|| format!("{}/model_spec.json", self.config.repo_dir))
    }
}

impl ServingBackend for RunPyBackend {
    fn start(&self, model: &str) -> Result<Endpoint> {
        // Built as owned `String`s (several pieces are computed at
        // runtime) then borrowed as `&str` for `CommandRunner::run_in_dir`,
        // which takes `&[&str]` -- argv-style, no shell involved, so
        // callers never need to worry about quoting.
        //
        // This is the MINIMAL invocation: `--model` (required), `--workflow
        // server --docker-server` (how this codebase always launches
        // serving), and `--service-port`. Everything else below is an
        // OPTIONAL override appended only when the corresponding
        // `RunPyConfig` field is `Some`/enabled -- see the module doc's
        // "Defer to `run.py`, don't second-guess it" section for why the
        // default omits `--tt-device`/`--override-docker-image`/`--impl`/
        // `--engine` entirely rather than guessing values for them.
        let mut args: Vec<String> = vec![
            "python3".to_string(),
            "run.py".to_string(),
            "--model".to_string(),
            model.to_string(),
            "--workflow".to_string(),
            "server".to_string(),
            "--docker-server".to_string(),
            "--service-port".to_string(),
            self.config.service_port.to_string(),
        ];

        if self.config.no_auth {
            args.push("--no-auth".to_string());
        }
        if let Some(cache) = &self.config.host_hf_cache {
            args.push("--host-hf-cache".to_string());
            args.push(cache.clone());
        }
        if let Some(device) = &self.config.tt_device {
            args.push("--tt-device".to_string());
            args.push(device.clone());
        }
        if let Some(image) = &self.config.image {
            args.push("--override-docker-image".to_string());
            args.push(image.clone());
        }
        if let Some(impl_name) = &self.config.impl_name {
            args.push("--impl".to_string());
            args.push(impl_name.clone());
        }
        if let Some(engine) = &self.config.engine {
            args.push("--engine".to_string());
            args.push(engine.clone());
        }
        if let Some(ids) = self.config.device_id.as_ref().filter(|ids| !ids.is_empty()) {
            args.push("--device-id".to_string());
            args.push(ids.clone());
        }

        // `run.py` reads `MODEL_SOURCE` from its environment, not from an
        // argv flag (see docs/reference/tt-inference-server-docker.md's
        // `MODEL_SOURCE=huggingface python3 run.py ...` invocation). This is
        // passed via `CommandRunner::run_in_dir_with_env`, which sets it on
        // the CHILD `Command` only -- NOT via `std::env::set_var` on this
        // process. `start` is reachable from `POST /run` through
        // `tokio::task::spawn_blocking` on a multithreaded runtime with no
        // mutex serializing concurrent calls, so mutating the process-wide
        // environment here would race with any other in-flight `start` call
        // (and `std::env::set_var` is `unsafe` as of Rust 2024 for exactly
        // this reason). `RealCommandRunner::run_in_dir_with_env` builds its
        // `std::process::Command` with `.envs(...)` before spawning, so the
        // child sees `MODEL_SOURCE` without any shared mutable state.
        // `CommandRunner` fakes (e.g. `tests/support/mod.rs`'s `FakeRunner`)
        // never spawn a real process at all, so the env is a no-op from
        // their point of view -- exactly why tests only need to assert on
        // the argv, not the environment.
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run_in_dir_with_env(
            &self.config.repo_dir,
            &arg_refs,
            &[("MODEL_SOURCE", self.config.model_source.as_str())],
        )?;

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

    /// Read `model_spec.json` (see `model_spec_path`) and enumerate every
    /// model it lists, with the device meshes each one supports -- so a
    /// client (`GET /models`, `tt models`) never has to guess or hardcode
    /// which models this box can actually run.
    ///
    /// `model_spec.json`'s shape (verified on real hardware):
    /// ```json
    /// { "release_version": "0.12.0",
    ///   "model_specs": { "<model-id>": { "<DEVICE_MESH>": {...}, ... }, ... } }
    /// ```
    /// The model id is the top-level key under `model_specs`; the supported
    /// device meshes are that entry's own keys (e.g. `GALAXY`, `T3K`,
    /// `P300X2`). Parsed via `serde_json::Value` rather than a strict typed
    /// struct so an entry shaped unexpectedly (or a non-object value) is
    /// just skipped rather than failing the whole enumeration -- this is a
    /// read-only "what's available" listing, not validation of the spec
    /// file itself (that's `run.py`'s job).
    fn list_models(&self) -> Result<ModelsResponse> {
        let path = self.model_spec_path();
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading model spec at {path}"))?;
        let value: serde_json::Value = serde_json::from_str(&content)
            .with_context(|| format!("parsing model spec at {path} as JSON"))?;

        let release_version = value
            .get("release_version")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mut models: Vec<libttstation::model::ModelInfo> = value
            .get("model_specs")
            .and_then(|v| v.as_object())
            .into_iter()
            .flatten()
            .filter_map(|(name, devices_val)| {
                let devices_obj = devices_val.as_object()?;
                let mut devices: Vec<String> = devices_obj.keys().cloned().collect();
                devices.sort();
                Some(libttstation::model::ModelInfo {
                    name: name.clone(),
                    devices,
                })
            })
            .collect();
        models.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(ModelsResponse {
            release_version,
            models,
        })
    }
}
