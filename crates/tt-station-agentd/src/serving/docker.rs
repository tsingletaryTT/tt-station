//! Docker serving backend -- the implementation that proves the end-to-end
//! story for this PoC: `docker run` a `tt-inference-server` image, poll it
//! until it's actually answering requests, and report back the `Endpoint`
//! clients should talk to.
//!
//! Process execution and the health probe are both routed through
//! `CommandRunner` so tests can inject a fake instead of shelling out to a
//! real `docker` binary and making real HTTP requests -- see `FakeRunner`
//! in `tests/serving.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use libttstation::model::{Endpoint, ServingStatus};

use super::ServingBackend;

/// How many times `start` polls the health endpoint before giving up.
/// Combined with `DEFAULT_HEALTH_POLL_INTERVAL`, the default bounds a
/// `start` call to roughly 20s of waiting for `tt-inference-server` to come
/// up -- generous enough for a container that's already pulled its image,
/// but still a hard bound so a wedged container can't hang the caller
/// forever. `DockerBackend::with_health_poll` overrides both for tests.
const DEFAULT_HEALTH_POLL_ATTEMPTS: u32 = 40;

/// Delay between health-poll attempts. See `DEFAULT_HEALTH_POLL_ATTEMPTS`.
const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Wraps the two ways `DockerBackend` reaches outside this process: running
/// a command and capturing its stdout, and probing a URL for liveness.
///
/// Two methods rather than one because they're genuinely different kinds of
/// "reach outside this process" -- `run` shells out to `docker` (argv-style,
/// no shell, so callers never need to worry about quoting), while
/// `health_ok` makes an HTTP GET. Folding both into a single command-style
/// `run` (e.g. shelling out to `curl` for health checks too) would work,
/// but it forces every fake to parse or construct command-line args just to
/// answer a yes/no health question; keeping `health_ok` separate lets a
/// fake return a plain `bool`, which is exactly what `tests/serving.rs`'s
/// `FakeRunner` does.
pub trait CommandRunner: Send + Sync {
    /// Run a command and return its stdout as a `String` on success.
    /// `args[0]` is conventionally the docker subcommand (`"run"`,
    /// `"stop"`, ...) -- the real implementation always invokes the
    /// `docker` binary itself, so callers pass only the subcommand and its
    /// arguments, not the program name.
    fn run(&self, args: &[&str]) -> Result<String>;

    /// Probe `GET {url}` and report whether it responded with a success
    /// status. Used to poll a freshly-started container until its serving
    /// process is actually accepting requests, not just until the
    /// container exists (a container can be "running" for a while before
    /// the model is loaded and the HTTP server inside it is ready).
    fn health_ok(&self, url: &str) -> bool;
}

/// Real `CommandRunner`: shells out to the `docker` binary on `$PATH` for
/// `run`, and makes a blocking HTTP GET for `health_ok`.
///
/// The blocking `reqwest` client is deliberate: `ServingBackend` is a sync
/// trait (see `mod.rs`), and `DockerBackend::start` is meant to be called
/// from a sync context (or via `spawn_blocking` from an async one, once
/// Task 10 wires this into the agent's control routes) -- so there's no
/// async runtime available here to await a non-blocking request against.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        let output = std::process::Command::new("docker")
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn docker {}", args.join(" ")))?;

        if !output.status.success() {
            return Err(anyhow::anyhow!(
                "docker {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn health_ok(&self, url: &str) -> bool {
        reqwest::blocking::get(url)
            .map(|resp| resp.status().is_success())
            .unwrap_or(false)
    }
}

/// Docker-backed `ServingBackend`: runs `tt-inference-server` in a
/// container on `host_port` and polls `GET /v1/models` until it's healthy.
pub struct DockerBackend {
    /// Container image to run, e.g. `"tenstorrent/tt-inference-server:latest"`.
    image: String,
    /// Host the serving container is reachable on -- baked into the
    /// returned `Endpoint`'s `base_url` rather than always assuming
    /// `localhost`, since the agent and the client calling it aren't
    /// necessarily the same machine.
    host: String,
    /// Host port the container's serving port is mapped to.
    host_port: u16,
    runner: Box<dyn CommandRunner>,
    /// Tracks the last-known serving status in-process. Chosen over
    /// deriving status from a `docker ps` call on every `status()` because
    /// it's trivially testable (no runner call needed to assert on it) and
    /// this backend is the sole owner of the containers it starts/stops --
    /// nothing else in this PoC starts a `tt-inference-*` container out of
    /// band, so there's no other source of truth to fall out of sync with.
    /// A future revision that must tolerate containers coming and going
    /// behind the agent's back should switch this to a `docker ps` query.
    status: Arc<Mutex<ServingStatus>>,
    health_poll_attempts: u32,
    health_poll_interval: Duration,
}

impl DockerBackend {
    /// Build a `DockerBackend` with production health-poll defaults
    /// (`DEFAULT_HEALTH_POLL_ATTEMPTS` / `DEFAULT_HEALTH_POLL_INTERVAL`).
    /// Starts `Idle` -- constructing a backend never implies anything is
    /// already serving.
    pub fn new(
        image: String,
        host: String,
        host_port: u16,
        runner: Box<dyn CommandRunner>,
    ) -> Self {
        DockerBackend {
            image,
            host,
            host_port,
            runner,
            status: Arc::new(Mutex::new(ServingStatus::Idle)),
            health_poll_attempts: DEFAULT_HEALTH_POLL_ATTEMPTS,
            health_poll_interval: DEFAULT_HEALTH_POLL_INTERVAL,
        }
    }

    /// Override the health-poll bound used by `start`. Exposed so tests can
    /// shrink the ~20s production timeout down to milliseconds without
    /// touching the production defaults or sleeping for real in a unit
    /// test. Gated the same way `AppState`'s test-only accessors are (see
    /// `routes.rs`): compiled in for this crate's own unit tests, or for
    /// downstream integration tests via the `test-hooks` feature (already
    /// turned on for `cargo test` by this crate's Cargo.toml).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn with_health_poll(mut self, attempts: u32, interval: Duration) -> Self {
        self.health_poll_attempts = attempts;
        self.health_poll_interval = interval;
        self
    }

    /// Name of the container this backend runs a given model in. Shared
    /// between `start` and `stop` so they always agree on which container
    /// they're talking about.
    fn container_name(&self, model: &str) -> String {
        format!("tt-inference-{model}")
    }
}

impl ServingBackend for DockerBackend {
    fn start(&self, model: &str) -> Result<Endpoint> {
        let container_name = self.container_name(model);
        // Host and container listen on the same port -- simplest mapping
        // that still makes the port genuinely load-bearing in the command
        // (a stray typo swapping host/container ports would be a real bug,
        // not just a style nit).
        let port_mapping = format!("{}:{}", self.host_port, self.host_port);
        let port_str = self.host_port.to_string();

        self.runner.run(&[
            "run",
            "-d",
            "--rm",
            "--name",
            &container_name,
            "-p",
            &port_mapping,
            "-e",
            &format!("MODEL={model}"),
            &self.image,
            "--model",
            model,
            "--port",
            &port_str,
        ])?;

        let health_url = format!("http://{}:{}/v1/models", self.host, self.host_port);
        let mut healthy = false;
        for _ in 0..self.health_poll_attempts {
            if self.runner.health_ok(&health_url) {
                healthy = true;
                break;
            }
            std::thread::sleep(self.health_poll_interval);
        }

        if !healthy {
            return Err(anyhow::anyhow!(
                "docker backend: model '{model}' did not become healthy at {health_url} \
                 within {} attempts",
                self.health_poll_attempts
            ));
        }

        *self.status.lock().expect("status mutex poisoned") =
            ServingStatus::Serving(model.to_string());

        Ok(Endpoint {
            base_url: format!("http://{}:{}/v1", self.host, self.host_port),
            model: model.to_string(),
            requires_key: false,
        })
    }

    fn stop(&self, model: &str) -> Result<()> {
        let container_name = self.container_name(model);
        self.runner.run(&["stop", &container_name])?;
        *self.status.lock().expect("status mutex poisoned") = ServingStatus::Idle;
        Ok(())
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(self.status.lock().expect("status mutex poisoned").clone())
    }
}
