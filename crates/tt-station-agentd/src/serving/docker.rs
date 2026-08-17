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

    /// `GET {url}` and return the response body as a `String`.
    ///
    /// Used by `RunPyBackend::start` to ask the just-started server what
    /// model id it's ACTUALLY serving (`GET /v1/models`) -- `run.py`'s
    /// `--model` flag wants a short name (`Qwen3-32B`), but the served
    /// OpenAI `model` field (and `model_spec.json`'s own keys) use the full
    /// Hugging Face id (`Qwen/Qwen3-32B`); asking the server directly avoids
    /// guessing which form to report back to a caller.
    ///
    /// Default implementation returns an error -- fine for any
    /// `CommandRunner` (real or fake) that has no need to answer an HTTP GET
    /// with a body, e.g. `DockerBackend`'s `RecordingFakeRunner` unit-test
    /// double, which never calls this. `RealCommandRunner` overrides it with
    /// a real blocking GET; `tests/support/mod.rs`'s `FakeRunner` overrides
    /// it with a settable canned body.
    fn http_get(&self, url: &str) -> Result<String> {
        let _ = url;
        Err(anyhow::anyhow!("CommandRunner::http_get not implemented"))
    }

    /// Run a command and return its exit code plus stdout/stderr, WITHOUT
    /// collapsing a non-zero exit into an `Err` the way `run` does (see
    /// `run_and_capture`, which discards the code and keeps only stderr).
    ///
    /// This exists for `gozer` (`src/gozer.rs`): its contract lives IN its
    /// exit codes -- `0` granted, `10` queued, `12` unavailable, `13` no
    /// such lease, `14` topology unreadable, `15` release refused, `16`
    /// mutex stuck, `130` interrupted -- and it emits its `--json` payload
    /// on stdout even when the exit code is non-zero. A caller with only
    /// `run` can read neither: the code is gone, and a non-zero exit means
    /// the JSON on stdout is discarded along with it.
    ///
    /// Default implementation delegates to `run` and reports exit `0` with
    /// no stderr on success -- adequate for any EXISTING implementor that
    /// only ever cared about success/failure, so adding this method breaks
    /// no current `CommandRunner`. Only implementors that actually need
    /// faithful exit-code/stdout/stderr capture (`RealCommandRunner` here,
    /// plus the two test fakes -- `FakeRunner` in `tests/support/mod.rs` and
    /// `RecordingFakeRunner` below) override it explicitly. `run` itself is
    /// left completely untouched: every existing caller keeps its current
    /// behavior.
    fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
        self.run(args).map(|stdout| CapturedOutput {
            code: 0,
            stdout,
            stderr: String::new(),
        })
    }
}

/// Exit code, stdout, and stderr from a `run_capturing` call -- the
/// un-collapsed sibling of what `run`'s plain `Result<String>` throws away
/// on a non-zero exit. See `CommandRunner::run_capturing`'s doc comment for
/// why gozer specifically needs this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedOutput {
    /// Process exit code. `-1` when the process was killed by a signal
    /// rather than exiting normally (see `std::process::ExitStatus::code`),
    /// which never happens for gozer's own documented contract but is a
    /// real possibility for any subprocess in general.
    pub code: i32,
    /// Captured stdout, trimmed of trailing whitespace (matches
    /// `run_and_capture`'s existing convention for `run`).
    pub stdout: String,
    /// Captured stderr, trimmed the same way.
    pub stderr: String,
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
        probe_client()
            .and_then(|c| c.get(url).send())
            .map(|resp| resp.status().is_success())
            .unwrap_or(false)
    }

    fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
        let (program, rest) = args.split_first().ok_or_else(|| {
            anyhow::anyhow!("CommandRunner::run_capturing called with empty argv")
        })?;
        let output = std::process::Command::new(program)
            .args(rest)
            .output()
            .with_context(|| format!("failed to spawn {}", args.join(" ")))?;
        Ok(CapturedOutput {
            // `.code()` is `None` only when the process was killed by a
            // signal rather than exiting -- `-1` is a clearly-out-of-band
            // sentinel (real exit codes are 0-255) rather than silently
            // treating "killed by signal" as a successful exit 0.
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        })
    }

    fn http_get(&self, url: &str) -> Result<String> {
        let resp = probe_client()
            .with_context(|| "building probe HTTP client")?
            .get(url)
            .send()
            .with_context(|| format!("GET {url} failed to send"))?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "GET {url} returned non-success status {}",
                resp.status()
            ));
        }
        resp.text()
            .with_context(|| format!("reading response body of GET {url}"))
    }
}

/// Blocking HTTP client for probes (`health_ok`, `/v1/models`) with BOUNDED
/// timeouts. Without these, a container that accepts the TCP connection but
/// never completes the HTTP response would hang the caller indefinitely -- a
/// real risk for `GET /serving`, which probes unknown/external containers
/// (e.g. tt-studio's) that this agent didn't launch. A slow/hung probe now
/// fails fast and degrades to "skip that endpoint" rather than stalling.
fn probe_client() -> reqwest::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
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
    /// Host paths passed to `--device`, one flag per entry -- e.g.
    /// `["/dev/tenstorrent"]` (the default: the whole directory, every chip
    /// on the box) or `["/dev/tenstorrent/2", "/dev/tenstorrent/3"]` for a
    /// two-chip pin. Configurable (rather than hardcoded) so tests and
    /// non-standard hosts can override it without touching this file.
    ///
    /// A `Vec` rather than the single `String` this used to be because a
    /// gozer lease pins N specific chips and docker takes one `--device` per
    /// device node -- see `DockerBackend::start`. The CLI still exposes a
    /// single `--device-path` and `main.rs` wraps it in a one-element `Vec`,
    /// so the DEFAULT, unleased argv is unchanged: exactly one `--device`
    /// flag with exactly the same value as before.
    pub device_path: Vec<String>,
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
            // One entry: the whole `/dev/tenstorrent` directory, exactly the
            // single `--device` flag this backend has always emitted.
            device_path: vec!["/dev/tenstorrent".to_string()],
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
    /// The `gozer` binary this backend leases chips through, or `None` when
    /// gozer isn't installed on this box (see `crate::gozer`'s module doc --
    /// gozer is OPTIONAL). `None` is the default and means EVERY path below
    /// behaves exactly as it did before leasing existed: one `--device`
    /// straight from config, no container stop on a failed bring-up, and no
    /// `gozer` process ever spawned. Set via `with_gozer`, which
    /// `serving::make_backend` calls on the production path.
    gozer: Option<crate::gozer::Capability>,
    /// The lease this backend currently holds on behalf of a RUNNING serve,
    /// and the container that lease was taken for -- or `None` when nothing
    /// is leased.
    ///
    /// Set exactly once, by `start`, at the moment the serve is confirmed
    /// healthy: ownership transfers out of `start`'s `LeaseGuard` (which
    /// would otherwise release on the way out of the function) and into the
    /// backend, where it outlives the call. Cleared by `stop` after a
    /// successful `gozer release`. Every FAILING exit from `start` leaves
    /// this `None` and lets the guard's `Drop` release instead.
    ///
    /// `Arc<Mutex<..>>` for the same reason `status` is one: `start` and
    /// `stop` take `&self` and are reachable concurrently from
    /// `spawn_blocking` tasks.
    lease: Arc<Mutex<Option<HeldLease>>>,
}

/// A lease this backend holds, paired with the container it belongs to.
///
/// **The pairing is the point, and it is a safety property, not bookkeeping.**
/// `stop(model)` derives a container name from its argument, but
/// `routes.rs::stop_model` passes `state.current_model().unwrap_or_default()`
/// -- the EMPTY STRING whenever `AppState` has no recorded model, which a
/// `POST /reset` (a no-op for this backend, but it does `set_idle()`) leaves
/// it in while the container is still running. Stopping `tt-inference-` finds
/// nothing, the failure is swallowed as "nothing to stop", and the release
/// that follows resets chips a live container is still driving. Recording the
/// name at handover means `stop` can always find the container the lease
/// actually belongs to, whatever it was asked to stop.
///
/// `RunPyBackend` needs no equivalent: its `stop` ignores the model argument
/// entirely and sweeps by published port, so its stop cannot miss.
///
/// **Known limitation** (listed with the others in `crate::gozer`'s module
/// doc): `container_name` is recorded only at SUCCESSFUL handover. The
/// failing exits inside `start` stop the container through an in-scope local
/// instead, because there is no `HeldLease` yet. The two agree today --
/// both derive from the same `container_name(model)` -- but they are two
/// implementations of "stop the right container", and a change to either
/// has to keep them agreeing.
#[derive(Debug, Clone)]
struct HeldLease {
    /// gozer's lease id, the argument to `gozer release`.
    lease_id: String,
    /// The `--name` of the container this lease was acquired for.
    container_name: String,
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
            // Leasing OFF by default -- a backend built without an explicit
            // `with_gozer` behaves exactly as it did before this integration
            // existed. See the `gozer` field's doc comment.
            gozer: None,
            lease: Arc::new(Mutex::new(None)),
        }
    }

    /// Attach the `gozer` capability this agent probed for at startup (see
    /// `crate::gozer::probe`), turning leasing ON for every subsequent
    /// `start`/`stop`.
    ///
    /// `None` (the default, and what a box without gozer installed yields)
    /// leaves the backend in its pre-leasing behaviour, which is the whole
    /// point of gozer being optional: no `gozer` subprocess and one
    /// `--device` straight from config.
    ///
    /// Builder-style (`self` by value) to match `with_health_poll`, but
    /// deliberately NOT `#[cfg]`-gated the way that one is: this is
    /// production wiring, called by `serving::make_backend`, not a test hook.
    pub fn with_gozer(mut self, gozer: Option<crate::gozer::Capability>) -> Self {
        self.gozer = gozer;
        self
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

    /// Take a gozer lease for this serve, or `Ok(None)` when leasing isn't
    /// available on this box (no `gozer` capability -- see the `gozer`
    /// field). `Ok(None)` is the pre-integration behaviour and is not an
    /// error: it means the caller serves whole-box exactly as it always did.
    ///
    /// The returned `LeaseGuard` releases on drop, which is what covers
    /// `start`'s failure exits -- including any added later. Mirrors
    /// `RunPyBackend::acquire_lease`; the two differ only in what `--who`
    /// carries.
    ///
    /// **`--who` is the lease↔container binding, and its FORMAT is a shared
    /// contract**: `tt-station:<published_port>:<model>`, byte-identical to
    /// what `RunPyBackend::acquire_lease` writes.
    ///
    /// This backend could do better on its own -- it passes `--name` itself,
    /// so unlike the runpy path it knows its container's name before it
    /// acquires, and could embed that. It deliberately does NOT. The design
    /// doc's "One startup sweep" section specifies exactly one reconciliation
    /// rule: list gozer leases whose `who` begins `tt-station:`, extract the
    /// **service port**, and release any lease with nothing serving on that
    /// port. A second `who` shape would make that one rule two, and the
    /// sweep would read a container name where it expects a port -- so a
    /// docker-backend lease left behind by a crashed agent would never be
    /// reclaimed. One format, one sweep, either backend.
    fn acquire_lease(&self, model: &str) -> Result<Option<crate::gozer::LeaseGuard<'_>>> {
        let Some(capability) = &self.gozer else {
            return Ok(None);
        };

        let who = format!("tt-station:{}:{}", self.config.host_port, model);
        let reason = format!("serving {model} via tt-station-agentd (docker backend)");
        match crate::gozer::acquire(
            self.runner.as_ref(),
            capability,
            crate::gozer::DEFAULT_LEASE_CHIPS,
            &who,
            &reason,
        ) {
            crate::gozer::Outcome::Granted(grant) => {
                eprintln!(
                    "tt-station-agentd: leased {} chip(s) [{}] as lease {} for '{model}'",
                    grant.chips.len(),
                    grant.chips.join(", "),
                    grant.lease_id
                );
                Ok(Some(crate::gozer::LeaseGuard::new(
                    self.runner.as_ref(),
                    capability.clone(),
                    grant,
                )))
            }
            crate::gozer::Outcome::Unavailable { holder, .. } => {
                // `acquire`'s own unavailable payload names nobody, so ask
                // `gozer status --json` who holds the boards. Deliberately
                // NO duration: gozer's per-chip status carries `who` but no
                // `since`, and agentd must not fabricate one (see
                // `gozer::contention_detail`).
                let detail = holder
                    .or_else(|| crate::gozer::contention_detail(self.runner.as_ref(), capability))
                    .unwrap_or_else(|| "gozer reports no free chips".to_string());
                // A `Contention`, not a bare `anyhow!` -- same reasoning
                // (and same wording) as `RunPyBackend::acquire_lease`:
                // `POST /run` answers 409 for a contended box, not 500.
                Err(anyhow::Error::new(crate::gozer::Contention::new(format!(
                    "docker backend: cannot serve '{model}' -- no chips are available: {detail}"
                ))))
            }
            crate::gozer::Outcome::Failed(message) => Err(anyhow::anyhow!(
                "docker backend: cannot serve '{model}' -- gozer acquire failed: {message}"
            )),
        }
    }

    /// The `--device` values for a LEASED serve: one `/dev/tenstorrent/<n>`
    /// node per granted device index, replacing the configured path
    /// entirely.
    ///
    /// The configured `device_path` is deliberately OVERRIDDEN (and logged),
    /// for the same reason `RunPyBackend` overrides a configured
    /// `--device-id`: it was chosen without knowing what gozer would grant,
    /// and its DEFAULT value is the whole `/dev/tenstorrent` directory --
    /// i.e. every chip on the box, including the neighbour's.
    ///
    /// **Fails closed**, like `gozer::Grant::reset_target`: a grant with no
    /// device indices refuses the serve rather than falling back to the
    /// configured path. Falling back would mean a container that asked for a
    /// lease and then opened the whole box anyway -- exactly the
    /// silent cross-tenant failure this integration exists to prevent, and
    /// the worst possible reading of "run only on what I leased".
    fn leased_device_paths(&self, grant: &crate::gozer::Grant) -> Result<Vec<String>> {
        if grant.dev_indices.is_empty() {
            return Err(anyhow::anyhow!(
                "lease '{}' granted no device indices; refusing to serve, because falling back \
                 to the configured --device {:?} would hand the container chips this lease does \
                 not cover",
                grant.lease_id,
                self.config.device_path
            ));
        }
        let paths: Vec<String> = grant
            .dev_indices
            .iter()
            .map(|index| format!("/dev/tenstorrent/{index}"))
            .collect();
        eprintln!(
            "tt-station-agentd: pinning the container to the leased devices {paths:?} \
             (the configured --device {:?} does not apply to a leased serve)",
            self.config.device_path
        );
        Ok(paths)
    }

    /// Resolve `--tt-device` for a LEASED serve: the mesh label for the shape
    /// gozer actually granted, not for the whole box.
    ///
    /// `config.tt_device` is a plain `String` with a production default of
    /// `p300x2` (see `main.rs`'s `DEFAULT_DOCKER_TT_DEVICE`), while
    /// `DEFAULT_LEASE_CHIPS` asks for one board -- so passing it verbatim
    /// under a lease names a mesh WIDER than the lease on every real box.
    ///
    /// Unlike the runpy path, the cross-tenant half of that is contained
    /// here: `--device` already restricts the container's cgroup to the
    /// leased device nodes, so it physically cannot open the neighbour's
    /// board no matter what mesh it is told to expect. What it can do is ask
    /// tt-inference-server for hardware it was not given and fail to come up
    /// -- which reads as a flaky bring-up rather than a misconfiguration, and
    /// is a worse failure for costing an operator an hour of debugging.
    ///
    /// Derivation, fail-closed policy and the `(board_type, count) -> mesh`
    /// table are all `device::leased_mesh`, shared with
    /// `RunPyBackend::leased_tt_device`. The configured value is deliberately
    /// OVERRIDDEN (and logged), for the same reason the configured
    /// `device_path` is: it was chosen without knowing what gozer would
    /// grant.
    fn leased_tt_device(&self, grant: &crate::gozer::Grant) -> Result<String> {
        let snapshot = self.runner.run(&["tt-smi", "-s"]).with_context(|| {
            format!(
                "cannot derive --tt-device for lease '{}': `tt-smi -s` failed, and guessing a \
                 mesh could hand the serve a board this lease does not cover",
                grant.lease_id
            )
        })?;
        // The shared helper's message is backend-neutral; name the SELECTOR
        // being derived so a failed serve says which flag it choked on.
        let mesh = crate::device::leased_mesh(&snapshot, grant.chips.len(), &grant.lease_id)
            .with_context(|| format!("cannot derive --tt-device for lease '{}'", grant.lease_id))?;
        if mesh != self.config.tt_device {
            eprintln!(
                "tt-station-agentd: ignoring configured --tt-device {} in favour of the leased \
                 device shape {mesh}",
                self.config.tt_device
            );
        }
        Ok(mesh)
    }

    /// Best-effort stop of the container this `start` launched, called on an
    /// error path BEFORE a `LeaseGuard` drop releases the lease.
    ///
    /// It exists for one reason: `gozer release` RESETS the released chips.
    /// Releasing while a container is still driving them lands a real
    /// `tt-smi -r` on a live workload and then advertises those chips FREE,
    /// so the next tenant collides. gozer cannot refuse it -- its
    /// `still_open` check reads `/proc/<pid>/fd` unprivileged and the serving
    /// container is root-owned, the same fd blindness that makes
    /// `--owner-pid` mandatory.
    ///
    /// Unlike `RunPyBackend::stop_launched_container`, this needs NO
    /// published-port fallback: that backend has to parse a container id out
    /// of run.py's stdout and documents a missing id as normal, whereas this
    /// backend passes `--name` itself and can always name its own container.
    /// The `docker stop` may still find nothing (the container never got far
    /// enough to exist) -- which is fine and is why the failure is logged
    /// rather than propagated.
    ///
    /// Every failure is LOGGED, never silently swallowed: the caller is
    /// already returning an error, but if this stop didn't happen the
    /// release that follows is about to reset a live container's chips, and
    /// the journal is the only place that can say so.
    fn stop_launched_container(&self, container_name: &str) {
        if let Err(err) = self.runner.run(&["docker", "stop", container_name]) {
            eprintln!(
                "tt-station-agentd: could not stop container {container_name} before releasing \
                 its lease: {err:#} -- the release will reset chips that may still be in use"
            );
        }
    }

    /// Give back the lease this backend holds on behalf of a running serve,
    /// if any, and forget it. A no-op (`Ok(())`) when nothing is leased --
    /// which is both the no-gozer case and the already-released case, so
    /// `stop` stays idempotent.
    ///
    /// **`take()`, not read-then-clear.** Two concurrent `stop`s (a client
    /// retrying after a timeout, or `/stop` racing the power path's
    /// best-effort stop) would both observe the same `Some(id)` if this
    /// cloned the id out and released outside the lock. gozer's exit 13 ("no
    /// such lease") makes the loser harmless only while the id is still
    /// unique -- and gozer reuses lease ids, so the loser's release can land
    /// on somebody else's lease, resetting THEIR chips. Taking the id out
    /// under a single lock means exactly one caller can ever hold it.
    ///
    /// A REFUSED release (gozer exit 15) propagates AND puts the lease back:
    /// the chips were not released and not reset, so a later `stop` must be
    /// able to try again rather than the id being dropped on the floor. It
    /// goes back only if the slot is still empty -- a `start` that stored a
    /// fresh lease while this release was in flight owns the slot now, and
    /// overwriting it would strand the newer lease.
    fn release_lease(&self) -> Result<()> {
        let Some(capability) = &self.gozer else {
            return Ok(());
        };
        let Some(held) = self.lease.lock().expect("lease mutex poisoned").take() else {
            return Ok(());
        };
        if let Err(err) = crate::gozer::release(self.runner.as_ref(), capability, &held.lease_id) {
            let mut slot = self.lease.lock().expect("lease mutex poisoned");
            match slot.as_ref() {
                None => *slot = Some(held),
                Some(newer) => eprintln!(
                    "tt-station-agentd: lease '{}' was refused release and a newer lease '{}' \
                     has since been recorded; the refused lease is NOT retried and must be \
                     cleaned up by hand -- check `gozer status`",
                    held.lease_id, newer.lease_id
                ),
            }
            return Err(err);
        }
        Ok(())
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
    /// Start a container serving `model`, taking a gozer chip lease first
    /// when leasing is available.
    ///
    /// **The lease is released on EVERY exit but one.** `acquire_lease`
    /// hands back a `LeaseGuard` that releases on drop, which covers the
    /// `docker run` failure, the health timeout, an unusable grant, and
    /// whatever exit someone adds later; only the success path disarms the
    /// guard and hands the lease to `self.lease` for `stop` to release.
    ///
    /// **Every exit that could leave a container running stops it FIRST**,
    /// because `gozer release` resets the released chips -- see
    /// `stop_launched_container`. Those stops are gated on `lease.is_some()`
    /// so that a box without gozer keeps its exact previous behaviour (a
    /// failed bring-up leaves its container for the operator to inspect).
    fn start(&self, model: &str) -> Result<Endpoint> {
        let container_name = self.container_name(model);

        // Lease the chips BEFORE touching anything on the box -- before the
        // container is started, so it can only ever be launched against
        // chips gozer granted. On a box without gozer this yields `None` and
        // every path below is unchanged.
        let lease = self.acquire_lease(model)?;

        // With a lease, `--device` names the granted device nodes and
        // nothing else; the configured path is overridden. Fails closed
        // rather than falling back -- see `leased_device_paths`. `?` here
        // drops `lease`, handing the unusable lease straight back before a
        // single container exists.
        let device_paths = match &lease {
            Some(guard) => self.leased_device_paths(guard.grant())?,
            None => self.config.device_path.clone(),
        };

        // `--tt-device` must describe the LEASED shape too, not the whole box
        // -- see `leased_tt_device`. Also fails closed; `?` drops `lease`.
        let tt_device = match &lease {
            Some(guard) => self.leased_tt_device(guard.grant())?,
            None => self.config.tt_device.clone(),
        };

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
            container_name.clone(),
            "--ipc".to_string(),
            "host".to_string(),
        ];
        // ONE `--device` flag per device node: docker has no multi-value
        // form, so pinning N chips means N flags. Unleased, `device_paths`
        // is the single configured entry and this loop emits exactly the one
        // flag it always did.
        for device in &device_paths {
            args.push("--device".to_string());
            args.push(device.clone());
        }
        args.extend([
            "--mount".to_string(),
            mount_spec,
            "--volume".to_string(),
            volume_spec,
        ]);
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
        args.push(tt_device);
        if self.config.no_auth {
            args.push("--no-auth".to_string());
        }

        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.runner.run(&arg_refs).inspect_err(|_| {
            // `docker run -d` can create a container and still exit non-zero
            // (it started and immediately died, or the daemon failed part
            // way through), so this exit can leave one behind. It must be
            // stopped BEFORE this error unwinds, because unwinding drops the
            // lease guard and `gozer release` RESETS the released chips --
            // see `stop_launched_container`.
            //
            // Gated on actually HOLDING a lease, not merely on gozer being
            // installed: the stop exists to protect the release, so it
            // should depend on the thing it protects. Without a lease the
            // unleased behaviour stays byte-identical (no stop at all, as
            // before).
            if lease.is_some() {
                self.stop_launched_container(&container_name);
            }
        })?;

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
            // The container is alive by construction on this path -- the
            // poll simply never saw it answer -- and returning from here
            // drops the lease guard, whose `gozer release` RESETS the
            // released chips. Releasing first would land a real `tt-smi -r`
            // on BDFs a live container is driving and then advertise them
            // FREE while the orphan keeps using them, so the next tenant
            // collides. gozer cannot catch that: its `still_open` check
            // reads `/proc/<pid>/fd` unprivileged and the serving container
            // is root-owned -- the same fd blindness that makes
            // `--owner-pid` mandatory.
            //
            // Gated on actually HOLDING a lease ON PURPOSE: this stop exists
            // to protect the release, so it depends on the thing it
            // protects, and without a lease the unleased behaviour stays
            // byte-identical to what it has always been (a timed-out
            // bring-up leaves its container for the operator to inspect).
            // Whether the unleased path SHOULD stop it too is a separate
            // question, not a side effect of this change.
            if lease.is_some() {
                self.stop_launched_container(&container_name);
            }
            return Err(anyhow::anyhow!(
                "docker backend: model '{model}' did not become healthy at {health_url} \
                 within {} attempts",
                self.health_poll_attempts
            ));
        }

        *self.status.lock().expect("status mutex poisoned") =
            ServingStatus::Serving(model.to_string());

        // The serve is up, so the lease must OUTLIVE this call: disarm the
        // guard (it would otherwise release on the way out of this function,
        // resetting the chips underneath the container that just came up)
        // and hand the id to the backend, where `stop` releases it. This is
        // the ONLY exit from `start` that doesn't release.
        if let Some(guard) = lease {
            let mut held = self.lease.lock().expect("lease mutex poisoned");
            if let Some(previous) = held.as_ref() {
                // Nothing should be able to reach here holding a lease --
                // `stop` clears it and every failing `start` releases -- but
                // silently overwriting one would strand chips until the
                // agent exits, so say so.
                eprintln!(
                    "tt-station-agentd: WARNING: replacing still-held lease '{}' (container {}) \
                     -- it will not be released until this agent exits; check `gozer status`",
                    previous.lease_id, previous.container_name
                );
            }
            // The CONTAINER NAME is recorded alongside the id, and that is
            // what makes `stop` safe: it must stop the container this lease
            // belongs to before releasing, and it cannot trust its own
            // `model` argument to name it. See `HeldLease`.
            *held = Some(HeldLease {
                lease_id: guard.into_lease_id(),
                container_name: container_name.clone(),
            });
        }

        Ok(Endpoint {
            base_url: format!("http://{}:{}/v1", self.config.host, self.config.host_port),
            model: model.to_string(),
            // Auth is required exactly when the container was NOT started
            // with `--no-auth`.
            requires_key: !self.config.no_auth,
        })
    }

    /// Stop the container serving `model` and hand back any lease held on
    /// its behalf, in that order.
    ///
    /// **When a lease is held, the RECORDED container is what gets stopped
    /// and the `model` argument is ignored.** It has to be: `routes.rs`
    /// passes `state.current_model().unwrap_or_default()` -- the empty string
    /// once anything has cleared `AppState`'s model, which `POST /reset` does
    /// while leaving this backend's container running (the trait's default
    /// `reset` is a no-op for `DockerBackend`). Deriving the name from `""`
    /// would `docker stop tt-inference-`, swallow the failure as "nothing to
    /// stop", and then release -- resetting the chips of a container that is
    /// still driving them and advertising them FREE to the next tenant, which
    /// gozer's root-blind `still_open` check cannot detect. The stop before a
    /// release must be GUARANTEED to hit the container the lease belongs to;
    /// `RunPyBackend` gets that guarantee from sweeping by published port,
    /// this backend gets it from `HeldLease::container_name`.
    ///
    /// Without a lease there is nothing recorded and nothing to protect, so
    /// the model-derived name is used exactly as it always was.
    ///
    /// NOTE what this does NOT do, deliberately: `RunPyBackend` also carries
    /// a cooperative `cancel` flag so a `/stop` arriving mid-`start` aborts
    /// the bring-up and closes the narrow window where `start` stores a
    /// lease just after `stop` looked for one. `DockerBackend` has never had
    /// that flag (a concurrent `/stop` couldn't cancel an in-flight `docker`
    /// bring-up before this change either), and adding one is a behaviour
    /// change to the unleased path, not part of wiring leasing in. The
    /// exposure it leaves is bounded: an in-flight `start` still owns its
    /// lease through the `LeaseGuard`, so nothing is released twice and
    /// nothing is reset under a live container -- the worst case is a lease
    /// that outlives a raced bring-up until the next `/stop`, visible in
    /// `gozer status`. Worth closing; not silently, as a side effect here.
    fn stop(&self, model: &str) -> Result<()> {
        // A held lease names its own container; only fall back to the
        // model-derived name when nothing is leased (see this method's doc).
        let container_name = self
            .lease
            .lock()
            .expect("lease mutex poisoned")
            .as_ref()
            .map(|held| held.container_name.clone())
            .unwrap_or_else(|| self.container_name(model));
        // Deliberately NOT `?`-propagated: `docker stop` exits non-zero when
        // the container is already stopped or doesn't exist at all, and per
        // this trait's doc (`ServingBackend::stop`) that's not an error --
        // `stop` must be idempotent. `routes.rs::stop_model` now calls
        // `backend.stop` unconditionally (even while idle, so it can cancel
        // an in-flight `/run`), so surfacing a "nothing to stop" failure here
        // would turn a harmless no-op into a 500 at the routes layer. A
        // container that IS actually present and running still gets stopped
        // by this call; only the "there was nothing to stop" outcome is
        // swallowed.
        let _ = self.runner.run(&["docker", "stop", &container_name]);
        *self.status.lock().expect("status mutex poisoned") = ServingStatus::Idle;

        // Release LAST, after the container is actually stopped: gozer
        // resets the released chips as part of the release, and resetting
        // them under a still-running container would be the very hazard this
        // integration is about. (This is also why `POST /stop` needs no
        // separate reset: the release does it.)
        //
        // A refused release (gozer exit 15) fails the stop rather than being
        // swallowed -- the chips were NOT handed back, and a caller told
        // "stopped" would believe they were. The container IS already
        // stopped at that point, and the lease stays recorded so a repeated
        // `/stop` retries the release. A no-gozer/no-lease backend gets
        // `Ok(())` here, so the idempotent-stop contract above is unchanged.
        self.release_lease()
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(self.status.lock().expect("status mutex poisoned").clone())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn gozer_capability(&self) -> Option<&crate::gozer::Capability> {
        self.gozer.as_ref()
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
        /// Canned `run_capturing` response, set via `set_capture`. `None`
        /// (the default) means `run_capturing` reports a plain success:
        /// exit 0, empty stdout/stderr -- fine for every existing caller of
        /// this fake, which only exercises `run`.
        capture: Arc<Mutex<Option<(i32, String, String)>>>,
    }

    impl RecordingFakeRunner {
        fn new() -> Self {
            RecordingFakeRunner {
                commands: Arc::new(Mutex::new(Vec::new())),
                capture: Arc::new(Mutex::new(None)),
            }
        }

        fn commands(&self) -> Vec<Vec<String>> {
            self.commands
                .lock()
                .expect("commands mutex poisoned")
                .clone()
        }

        /// Set the canned exit code/stdout/stderr the next (and any
        /// subsequent) `run_capturing` call returns -- e.g.
        /// `set_capture(12, r#"{"granted":false}"#, "")` to exercise a
        /// gozer-style non-zero exit with a JSON payload on stdout.
        fn set_capture(&self, code: i32, stdout: &str, stderr: &str) {
            *self.capture.lock().expect("capture mutex poisoned") =
                Some((code, stdout.to_string(), stderr.to_string()));
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

        fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
            self.commands
                .lock()
                .expect("commands mutex poisoned")
                .push(args.iter().map(|s| s.to_string()).collect());
            let (code, stdout, stderr) = self
                .capture
                .lock()
                .expect("capture mutex poisoned")
                .clone()
                .unwrap_or((0, String::new(), String::new()));
            Ok(CapturedOutput {
                code,
                stdout,
                stderr,
            })
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

    /// `run_capturing` on the real runner must preserve a NON-ZERO exit code
    /// plus both stdout and stderr, rather than collapsing everything into a
    /// stringified `Err` the way `run`/`run_and_capture` do. This is the
    /// whole reason `run_capturing` exists: gozer's contract lives in its
    /// exit codes (10 queued, 12 unavailable, ...) and it emits `--json` on
    /// stdout even on a non-zero exit -- a caller that only has `run` cannot
    /// read either.
    #[test]
    fn real_command_runner_run_capturing_preserves_nonzero_exit_and_streams() {
        let runner = RealCommandRunner;
        let captured = runner
            .run_capturing(&["sh", "-c", "echo out; echo err >&2; exit 7"])
            .expect("run_capturing should return Ok even on a non-zero exit");
        assert_eq!(captured.code, 7);
        assert_eq!(captured.stdout, "out");
        assert_eq!(captured.stderr, "err");
    }

    /// The success path: exit 0, stdout captured, no error.
    #[test]
    fn real_command_runner_run_capturing_reports_zero_on_success() {
        let runner = RealCommandRunner;
        let captured = runner
            .run_capturing(&["sh", "-c", "echo hi"])
            .expect("run_capturing should succeed");
        assert_eq!(captured.code, 0);
        assert_eq!(captured.stdout, "hi");
        assert_eq!(captured.stderr, "");
    }

    /// A command that fails to spawn at all (unlike a non-zero exit) is
    /// still a genuine `Err` -- there's no exit code to report because the
    /// process never ran.
    #[test]
    fn real_command_runner_run_capturing_errors_when_spawn_fails() {
        let runner = RealCommandRunner;
        assert!(runner
            .run_capturing(&["tt-station-agentd-definitely-not-a-real-binary"])
            .is_err());
    }

    /// `RecordingFakeRunner::run_capturing` must report whatever exit
    /// code/stdout/stderr a test canned via `set_capture`, so `src/`-internal
    /// tests (e.g. `gozer.rs`'s own unit tests, if any land here later) can
    /// exercise a non-zero-exit-with-JSON-on-stdout response without a real
    /// `gozer` binary.
    #[test]
    fn recording_fake_runner_run_capturing_returns_canned_exit_code() {
        let runner = RecordingFakeRunner::new();
        runner.set_capture(12, r#"{"granted":false}"#, "");

        let captured = runner
            .run_capturing(&["gozer", "acquire", "--json"])
            .expect("run_capturing should succeed against the fake");
        assert_eq!(captured.code, 12);
        assert_eq!(captured.stdout, r#"{"granted":false}"#);
        assert_eq!(captured.stderr, "");
    }
}
