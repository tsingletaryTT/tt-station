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
    ///
    /// `args[0]` is the PROGRAM to execute (e.g. `"docker"`, `"python3"`) --
    /// this trait is deliberately generic over what gets run, not
    /// docker-specific, since `RunPyBackend` (serving/runpy.rs) needs to
    /// exec `python3 run.py ...` through the exact same seam `DockerBackend`
    /// uses for `docker run ...`. (Earlier revisions of this trait had the
    /// real implementation hardcode the `docker` binary and callers pass
    /// only the subcommand; that stopped working once a second backend
    /// needed to run a different program through the same trait, so
    /// `RealCommandRunner::run` now execs whatever `args[0]` names.)
    fn run(&self, args: &[&str]) -> Result<String>;

    /// Like `run`, but with the child process's working directory set to
    /// `dir` first.
    ///
    /// Default implementation ignores `dir` and just calls `run` -- fine
    /// for `DockerBackend` (which invokes `docker`, a `$PATH`-resolved
    /// binary with no relative-path dependencies) and for any
    /// `CommandRunner` fake that never actually shells out. `RunPyBackend`
    /// is the one caller that needs a real working-directory change: `run.py`
    /// lives inside a `tt-inference-server` checkout and is invoked as a
    /// relative path (`python3 run.py ...`), so `RealCommandRunner`
    /// overrides this to set `Command::current_dir`.
    fn run_in_dir(&self, dir: &str, args: &[&str]) -> Result<String> {
        let _ = dir;
        self.run(args)
    }

    /// Like `run_in_dir`, but with the given `(key, value)` pairs also set
    /// on the CHILD process's environment before it's spawned -- e.g.
    /// `run.py`'s `MODEL_SOURCE`, which it reads from its own environment
    /// rather than an argv flag (see `RunPyBackend::start`).
    ///
    /// Deliberately scoped to the child `Command` only, never the calling
    /// process's environment: `std::env::set_var` mutates GLOBAL, per-process
    /// state, which is unsound to touch from a multithreaded program without
    /// external synchronization (it's `unsafe` as of the Rust 2024 edition
    /// for exactly this reason). `RunPyBackend::start` is reachable from
    /// `POST /run` via `tokio::task::spawn_blocking` on a multithreaded
    /// runtime with no mutex serializing concurrent calls, so two overlapping
    /// requests setting different `MODEL_SOURCE` values would otherwise race
    /// on the process environment and could leak the wrong value into
    /// whichever child happens to fork during the window.
    ///
    /// Default implementation ignores `env` and just calls `run_in_dir` --
    /// fine for `DockerBackend` (which passes env via `docker run --env
    /// KEY=value` in its own argv, not the child-process environment) and
    /// for any `CommandRunner` fake that never actually shells out.
    /// `RealCommandRunner` overrides this to set `Command::envs` on the
    /// child before spawning it.
    fn run_in_dir_with_env(
        &self,
        dir: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String> {
        let _ = env;
        self.run_in_dir(dir, args)
    }

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
        let (program, rest) = args
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("CommandRunner::run called with empty argv"))?;
        run_and_capture(std::process::Command::new(program).args(rest), args)
    }

    fn run_in_dir(&self, dir: &str, args: &[&str]) -> Result<String> {
        let (program, rest) = args
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("CommandRunner::run_in_dir called with empty argv"))?;
        run_and_capture(
            std::process::Command::new(program)
                .args(rest)
                .current_dir(dir),
            args,
        )
    }

    fn run_in_dir_with_env(
        &self,
        dir: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String> {
        let (program, rest) = args.split_first().ok_or_else(|| {
            anyhow::anyhow!("CommandRunner::run_in_dir_with_env called with empty argv")
        })?;
        run_and_capture(
            std::process::Command::new(program)
                .args(rest)
                .current_dir(dir)
                // Set on the CHILD `Command` only -- never
                // `std::env::set_var` on the parent process. See the trait
                // method's doc comment for why that distinction matters.
                .envs(env.iter().copied()),
            args,
        )
    }

    fn health_ok(&self, url: &str) -> bool {
        reqwest::blocking::get(url)
            .map(|resp| resp.status().is_success())
            .unwrap_or(false)
    }
}

/// Shared plumbing for `RealCommandRunner::run`/`run_in_dir`: spawn `cmd`,
/// wait for it, and turn a non-zero exit into an `Err` that names the full
/// argv (`display_args`, kept separate from `cmd` since `Command` doesn't
/// expose its own argv back out) and stderr for debugging.
fn run_and_capture(cmd: &mut std::process::Command, display_args: &[&str]) -> Result<String> {
    let output = cmd
        .output()
        .with_context(|| format!("failed to spawn {}", display_args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "{} failed: {}",
            display_args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Everything `DockerBackend` needs to know to build a real
/// `tt-inference-server` `docker run` invocation. Grouped into one struct
/// (rather than a growing `DockerBackend::new` argument list) because
/// `main.rs` builds this straight from CLI flags and tests build it straight
/// from `Default::default()` plus targeted overrides -- see
/// `docs/reference/tt-inference-server-docker.md` for why each field exists
/// and where its value comes from on real hardware.
#[derive(Clone, Debug)]
pub struct DockerConfig {
    /// Container image to run. There is deliberately no sane hardcoded
    /// default here in production (see `main.rs`'s `--serving-image` doc
    /// comment) -- `tt-inference-server` publishes no `latest` tag, so any
    /// default is an example tag that must be reviewed per release. Tests
    /// that don't care about the image still need a value, which is why
    /// `Default` below sets a placeholder rather than omitting the field.
    pub image: String,
    /// Host the serving container is reachable on -- baked into the
    /// returned `Endpoint`'s `base_url` rather than always assuming
    /// `localhost`, since the agent and the client calling it aren't
    /// necessarily the same machine.
    pub host: String,
    /// Host port mapped onto the container's fixed serving port (8000 --
    /// see `CONTAINER_PORT`). This is the only port that's actually
    /// configurable; the container always listens on 8000 internally.
    pub host_port: u16,
    /// `--tt-device` value, e.g. `n300`, `p150x4`, `p300x2`. Not pinned to a
    /// single hardcoded default in the codebase beyond the CLI's
    /// `--tt-device` flag default, since the correct string depends on the
    /// physical box and isn't 100% confirmed for QuietBox 2 -- see the doc's
    /// "Uncertainties" section.
    pub tt_device: String,
    /// Hugging Face access token for gated repos (e.g. Llama). Passed
    /// through as `--env HF_TOKEN=...` only when `Some` and non-empty --
    /// most local/open models need no token at all, so the argv shouldn't
    /// carry an empty or placeholder one.
    pub hf_token: Option<String>,
    /// Name of the Docker volume mounted at
    /// `/home/container_app_user/cache_root` to persist downloaded
    /// weights/HF cache across container restarts.
    pub cache_volume: String,
    /// When `true`, the container is started with `--no-auth` (JWT bearer
    /// auth disabled) -- the PoC default, since minting a JWT client-side
    /// is out of scope here. When `false`, `--no-auth` is omitted and the
    /// returned `Endpoint.requires_key` is `true`.
    pub no_auth: bool,
    /// Host path passed to `--device`, e.g. `/dev/tenstorrent`. Configurable
    /// (rather than hardcoded) so tests and non-standard hosts can override
    /// it without touching this file.
    pub device_path: String,
    /// Host path bind-mounted onto itself inside the container via `--mount
    /// type=bind,src=...,dst=...` -- tt-metal needs 1G hugepages for DMA,
    /// provisioned on the host ahead of time by `tt-installer`.
    pub hugepages_src: String,
}

impl Default for DockerConfig {
    /// Defaults chosen to make hermetic tests concise (override only the
    /// field a given test cares about via struct-update syntax); NOT
    /// necessarily what a real deployment should run unexamined -- `main.rs`
    /// builds a `DockerConfig` from explicit CLI flags rather than relying
    /// on this impl.
    fn default() -> Self {
        DockerConfig {
            image: "tenstorrent/tt-inference-server:unset".to_string(),
            host: "127.0.0.1".to_string(),
            host_port: 8000,
            tt_device: "p150x4".to_string(),
            hf_token: None,
            cache_volume: "tt-station-cache".to_string(),
            no_auth: true,
            device_path: "/dev/tenstorrent".to_string(),
            hugepages_src: "/dev/hugepages-1G".to_string(),
        }
    }
}

/// Port `tt-inference-server`'s OpenAI-compatible HTTP server listens on
/// *inside* the container. Fixed by the image itself (overridable only via
/// `$SERVICE_PORT` inside the container, which this PoC doesn't touch) --
/// the only thing actually configurable is what host port it's published
/// to, via `DockerConfig::host_port`.
const CONTAINER_PORT: u16 = 8000;

/// Docker-backed `ServingBackend`: runs `tt-inference-server` in a
/// container on `host_port` (mapped to the container's fixed
/// `CONTAINER_PORT`) and polls `GET /health` until it's healthy.
pub struct DockerBackend {
    config: DockerConfig,
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
    pub fn new(config: DockerConfig, runner: Box<dyn CommandRunner>) -> Self {
        DockerBackend {
            config,
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
    ///
    /// The model id itself is passed through `sanitize_container_name`
    /// first -- real model ids commonly contain characters (like the `/` in
    /// `meta-llama/Llama-3.1-8B`) that Docker rejects in a `--name` value.
    /// Only the container name is sanitized; `start` still passes the
    /// *original* `model` string to `--model`, since that's what the server
    /// inside the container needs to actually load the right model.
    fn container_name(&self, model: &str) -> String {
        format!("tt-inference-{}", sanitize_container_name(model))
    }
}

/// Replace every character not valid in a Docker `--name` value
/// (`[A-Za-z0-9_.-]`) with `-`.
///
/// Docker container names must match `[a-zA-Z0-9][a-zA-Z0-9_.-]*` --
/// notably no `/`, which shows up constantly in real model ids (e.g. a
/// Hugging Face-style `org/model-name`). Without this, `docker run --name
/// tt-inference-meta-llama/Llama-3.1-8B ...` fails outright on the first
/// real (non-mock) hardware run. The leading `tt-inference-` prefix this is
/// always appended to already starts with an alphanumeric character, so
/// sanitizing only the model portion is enough to satisfy Docker's "must
/// start with an alphanumeric" rule too.
fn sanitize_container_name(model: &str) -> String {
    model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl ServingBackend for DockerBackend {
    fn start(&self, model: &str) -> Result<Endpoint> {
        let container_name = self.container_name(model);

        // The container's serving port is fixed at `CONTAINER_PORT`; only
        // the host side of the `--publish` mapping is configurable.
        let publish_mapping = format!("{}:{}", self.config.host_port, CONTAINER_PORT);
        // `--mount type=bind,src=X,dst=X`: the hugepages path is bind-mounted
        // onto the SAME path inside the container, since that's the path
        // tt-metal's DMA code expects to find inside the container too.
        let mount_spec = format!("type=bind,src={0},dst={0}", self.config.hugepages_src);
        let volume_spec = format!(
            "{}:/home/container_app_user/cache_root",
            self.config.cache_volume
        );
        // Only pass `--env HF_TOKEN=...` when a real, non-empty token is
        // configured -- most local/open models need no token, and shipping
        // an empty one would be actively misleading in a `docker inspect`.
        let hf_token_env = self
            .config
            .hf_token
            .as_ref()
            .filter(|token| !token.is_empty())
            .map(|token| format!("HF_TOKEN={token}"));

        // Built as owned `String`s (several pieces are computed at runtime,
        // e.g. `publish_mapping`) then borrowed as `&str` for
        // `CommandRunner::run`, which takes `&[&str]` -- argv-style, no
        // shell involved, so callers never need to worry about quoting.
        let mut args: Vec<String> = vec![
            "docker".to_string(),
            "run".to_string(),
            "-d".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            container_name,
            "--ipc".to_string(),
            "host".to_string(),
            "--device".to_string(),
            self.config.device_path.clone(),
            "--mount".to_string(),
            mount_spec,
            "--volume".to_string(),
            volume_spec,
        ];
        if let Some(env) = hf_token_env {
            args.push("--env".to_string());
            args.push(env);
        }
        args.push("--publish".to_string());
        args.push(publish_mapping);
        args.push(self.config.image.clone());
        // Everything from here on is passed straight through to
        // `tt-inference-server`'s own CLI, after the image name -- NOT
        // docker flags.
        args.push("--model".to_string());
        args.push(model.to_string());
        args.push("--tt-device".to_string());
        args.push(self.config.tt_device.clone());
        if self.config.no_auth {
            args.push("--no-auth".to_string());
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run(&arg_refs)?;

        // `run.py` (and this backend) poll `/health`, not `/v1/models` --
        // see docs/reference/tt-inference-server-docker.md.
        let health_url = format!(
            "http://{}:{}/health",
            self.config.host, self.config.host_port
        );
        let healthy = super::poll_until_healthy(
            self.runner.as_ref(),
            &health_url,
            self.health_poll_attempts,
            self.health_poll_interval,
        );

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
            base_url: format!("http://{}:{}/v1", self.config.host, self.config.host_port),
            model: model.to_string(),
            // Auth is required exactly when the container was NOT started
            // with `--no-auth`.
            requires_key: !self.config.no_auth,
        })
    }

    fn stop(&self, model: &str) -> Result<()> {
        let container_name = self.container_name(model);
        self.runner.run(&["docker", "stop", &container_name])?;
        *self.status.lock().expect("status mutex poisoned") = ServingStatus::Idle;
        Ok(())
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(self.status.lock().expect("status mutex poisoned").clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model id with both a `/` (org/model-style Hugging Face ids) and a
    /// `.` (version numbers) must sanitize into something Docker will
    /// accept as a container name -- `.` is already valid so it passes
    /// through unchanged, `/` is the character that would otherwise break
    /// `docker run --name`.
    #[test]
    fn sanitize_container_name_replaces_invalid_characters() {
        assert_eq!(
            sanitize_container_name("meta-llama/Llama-3.1-8B"),
            "meta-llama-Llama-3.1-8B"
        );
    }

    /// Minimal local fake `CommandRunner`: always healthy on the first
    /// probe, just records the argv it was asked to `run` so the test below
    /// can inspect it. `tests/support/mod.rs`'s richer `FakeRunner` isn't
    /// reachable from here -- integration tests under `tests/` are separate
    /// compilation units from this crate's own `src/`-internal unit tests.
    #[derive(Clone)]
    struct RecordingFakeRunner {
        commands: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl RecordingFakeRunner {
        fn new() -> Self {
            RecordingFakeRunner {
                commands: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn commands(&self) -> Vec<Vec<String>> {
            self.commands
                .lock()
                .expect("commands mutex poisoned")
                .clone()
        }
    }

    impl CommandRunner for RecordingFakeRunner {
        fn run(&self, args: &[&str]) -> Result<String> {
            self.commands
                .lock()
                .expect("commands mutex poisoned")
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(String::new())
        }

        fn health_ok(&self, _url: &str) -> bool {
            true
        }
    }

    /// The container name built from a slashed model id must be valid, but
    /// the `docker run` argv must still carry the ORIGINAL model string in
    /// `--model` -- the server inside the container needs the real model id
    /// to know what to load, not the name-safe-but-mangled version.
    ///
    /// (The exhaustive shape of the real `tt-inference-server` argv --
    /// `--device`, `--tt-device`, `--publish`, `--no-auth`, HF token
    /// handling, ... -- is covered by `tests/serving.rs`, which can share
    /// the richer `FakeRunner` in `tests/support/mod.rs`. This unit test
    /// stays focused on the one property that's specific to sanitization.)
    #[test]
    fn start_sanitizes_container_name_but_keeps_original_model_in_argv() {
        let runner = RecordingFakeRunner::new();
        let config = DockerConfig {
            image: "some/image:tag".to_string(),
            host: "127.0.0.1".to_string(),
            host_port: 8080,
            ..Default::default()
        };
        let backend = DockerBackend::new(config, Box::new(runner.clone()));

        let model = "meta-llama/Llama-3.1-8B";
        backend.start(model).expect("start should succeed");

        let commands = runner.commands();
        assert_eq!(commands.len(), 1);
        let run_cmd = &commands[0];

        let name_idx = run_cmd
            .iter()
            .position(|a| a == "--name")
            .expect("--name flag should be present");
        let container_name = &run_cmd[name_idx + 1];
        assert_eq!(container_name, "tt-inference-meta-llama-Llama-3.1-8B");
        assert!(
            !container_name.contains('/'),
            "container name must not contain '/': {container_name}"
        );

        let model_idx = run_cmd
            .iter()
            .position(|a| a == "--model")
            .expect("--model flag should be present");
        assert_eq!(
            run_cmd[model_idx + 1],
            model,
            "--model should carry the ORIGINAL, unsanitized model id"
        );
    }
}
