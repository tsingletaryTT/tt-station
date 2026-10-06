//! The `tt` serving backend: serve models by delegating to Tenstorrent's official `tt` CLI.
//!
//! The repo rule is to prefer the official CLI (`tenstorrent/tt-cli`) over reimplementing what
//! it does. `tt serve` already knows how to start a catalog model through tt-inference-server's
//! `run.py`, or a Hub bundle through `tt-model`. `tt --json model ps` lists every model server
//! on the box, and `tt model stop` stops one. This backend is a thin layer over those three
//! commands plus the parts only the agent can do: lease chips through gozer, wait until the
//! model is actually listed on `/v1/models`, and hand back an [`Endpoint`].
//!
//! It is also the only backend that applies per-request [`RunOptions`] (`POST /run`'s
//! `options`): they become `tt serve` arguments through
//! [`libttstation::run_options::serve_argv_for`], the same function the tray and the run
//! sheet's command preview use.
//!
//! ## What `start` does, in order
//!
//! 1. Validate the model name and the options again (the route already did) and refuse a
//!    non-`server` workflow.
//! 2. Check that the configured binary is the official CLI: `--version` must print
//!    `tt <semver>` (see `libttstation::official_tt`).
//! 3. Ask `tt --json serve <model> --dry-run` how it would serve the model. That says whether
//!    the model is a catalog model or a tt-model bundle, which decides which options are
//!    allowed, and it rejects an unknown model before any chips are leased.
//! 4. Stop whatever this backend is already serving and release its lease. A lease adopted
//!    from a previous agent process is released only after its server, found with
//!    `tt model ps`, has been stopped; if that server cannot be identified, the start is refused.
//! 5. Lease chips through gozer, when gozer is installed.
//! 6. Run `tt --json serve ...`. For a catalog model it returns once tt-inference-server's
//!    container exists. For a bundle the argv carries `--detach`, which makes it return once
//!    the container is up.
//! 7. Poll `GET http://<serving_host>:<port>/v1/models` until it lists a model, and report that
//!    model id.
//!
//! Every failure after step 6 starts runs `tt model stop <model>` before the lease guard is
//! dropped, because `gozer release` resets the released chips and must not do that under a
//! running container.
//!
//! ## After an agent restart
//!
//! A new agent process starts idle. `discover_own_serve` finds a `tt model ps` row on this
//! backend's own port (the port in an adopted lease's `who`, else `service_port`) and the route
//! records it as serving, so `/status`, `/endpoint` and `/stop` act on it. `serving_entries`
//! feeds `GET /serving` from `tt model ps`, because docker discovery only recognises
//! tt-inference-server images and tt-model images are `tt-model/<name>:<tag>`.
//!
//! Two limits follow from that rule. Only a serve on the agent's own port is rediscovered: the
//! port of an adopted lease, else `service_port`. A serve started with `options.port` on
//! another port, whose lease was not adopted, stays invisible until the next `/run`. And any
//! running row on that port is treated as the agent's, including one started by hand or by
//! another tenant, so `/stop` will stop it.
//!
//! These read-only paths go through a short `ps` cache with single-flight and a hard timeout
//! ([`PS_CACHE_TTL`], [`PS_TIMEOUT`], [`PS_STALE_LIMIT`]), because idle `/status` polls are
//! unauthenticated. The paths that decide what to stop before a lease release always run a
//! fresh `ps`.
//!
//! ## What has been checked so far
//!
//! Live on 2026-10-05 (Task 4, bundle `changh95/qwen3.8-27b-p300x2` on 4x p300c, tt 1.0.1):
//! steps 2 to 7, `options` reaching the container, stop-then-release, and the adopted-lease swap.
//! A bundle's `/v1` needs no API key (`requires_key: false` holds). Catalog models were checked
//! only with `--dry-run`.
//!
//! **The lease does not restrict which chips the container opens.** The grant's
//! `TT_VISIBLE_DEVICES` is set in `tt serve`'s environment, but it does not reach the
//! container: tt-model picks the devices itself and mounts `/dev/tenstorrent/0` to `3` (seen in
//! `docker inspect`, and `tt-model serve --print` ignores the variable). This is harmless only
//! while the lease covers every chip (`gozer::DEFAULT_LEASE_CHIPS = "all"`, today's setting).
//! Before leases narrow, the backend must pass explicit devices derived from the grant.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use libttstation::model::{Endpoint, ModelInfo, ModelsResponse, ServingEntry, ServingStatus};
use libttstation::official_tt;
use libttstation::run_options::{
    serve_argv_for, validate_model_name, OptionError, RunOptions, ServeTarget, Workflow,
};
use serde::Deserialize;

use super::docker::{CapturedOutput, CommandRunner};
use super::ServingBackend;

/// Default number of `/v1/models` polls before `start` gives up. With
/// [`DEFAULT_HEALTH_POLL_INTERVAL`] that is about 40 minutes, the same ceiling `RunPyBackend`
/// uses: a large model's first serve downloads weights and compiles before it answers.
pub const DEFAULT_HEALTH_POLL_ATTEMPTS: u32 = 1200;

/// Delay between `/v1/models` polls. See [`DEFAULT_HEALTH_POLL_ATTEMPTS`].
pub const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a `tt --json model ps` outcome is reused by the read-only paths (`/status`,
/// `/endpoint`, `/serving`, and the rediscovery that `/stop` runs before it acts). Idle `/status`
/// polls are unauthenticated and each one asks the backend, so without this every poll would
/// start a Python CLI and a few docker calls. 2.5 s keeps a tray polling every few seconds to
/// about one `ps` per poll. A failed or timed-out run is reused for the same period, so a hung
/// `tt` costs at most one [`PS_TIMEOUT`] per period. A name found this way is never acted on
/// alone: before `tt model stop` and a lease release, `stop_inner` confirms it with a fresh read.
pub const PS_CACHE_TTL: Duration = Duration::from_millis(2500);

/// Hard limit on one `tt --json model ps` run. After it the child is killed and the call
/// fails, so a hung `tt` or a wedged docker cannot hang a request forever. `ps` normally takes
/// well under a second.
pub const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// A `/run` or `/stop` that needs a fresh read queues on the same lock as the idle polls, so
/// behind a hung `tt` it can wait about 2x this (the poll's run, then its own) before failing.
///
/// When a cached-path `ps` fails or times out, the last good rows are still returned if they are
/// younger than this. Older rows are not trusted, and the error is returned.
pub const PS_STALE_LIMIT: Duration = Duration::from_secs(10);

/// Settings for [`TtBackend`].
#[derive(Debug, Clone)]
pub struct TtConfig {
    /// The official CLI to run. Production resolves it with
    /// `libttstation::official_tt::resolve_bin` (the `TTS_OFFICIAL_TT_BIN` override, then
    /// `PATH`, then `~/.local/bin/tt`), because a `systemctl --user` unit's `PATH` usually lacks
    /// `~/.local/bin`, where `uv tool install tenstorrent` puts it. The default is the bare name
    /// from `official_tt::OFFICIAL_TT_DEFAULT_BIN`.
    pub tt_bin: String,
    /// Host the served `/v1` is reachable on. It goes into the returned `Endpoint`'s `base_url`
    /// and into the readiness URL this backend polls.
    pub serving_host: String,
    /// Port passed to `tt serve --port` when the request does not set one. The agent owns the
    /// port so it knows where to poll and what to advertise.
    pub service_port: u16,
    /// How many times `start` polls `/v1/models`. See [`DEFAULT_HEALTH_POLL_ATTEMPTS`].
    pub health_poll_attempts: u32,
    /// Delay between polls. See [`DEFAULT_HEALTH_POLL_INTERVAL`].
    pub health_poll_interval: Duration,
    /// See [`PS_CACHE_TTL`]. A field so tests can use milliseconds.
    pub ps_cache_ttl: Duration,
    /// See [`PS_TIMEOUT`].
    pub ps_timeout: Duration,
    /// See [`PS_STALE_LIMIT`].
    pub ps_stale_limit: Duration,
}

impl Default for TtConfig {
    fn default() -> Self {
        TtConfig {
            tt_bin: official_tt::OFFICIAL_TT_DEFAULT_BIN.to_string(),
            serving_host: "127.0.0.1".to_string(),
            service_port: 8000,
            health_poll_attempts: DEFAULT_HEALTH_POLL_ATTEMPTS,
            health_poll_interval: DEFAULT_HEALTH_POLL_INTERVAL,
            ps_cache_ttl: PS_CACHE_TTL,
            ps_timeout: PS_TIMEOUT,
            ps_stale_limit: PS_STALE_LIMIT,
        }
    }
}

/// One row of `tt --json model ps`'s `served` array.
///
/// The field list mirrors tt-cli's `ServedModel` dataclass
/// (`src/tenstorrent/models/served.py`), whose docstring says the field order is the `--json`
/// contract and the `backend`/`health` vocabularies are closed:
/// `backend` is `inference-server`, `model-manager`, `studio` or `unknown`; `health` is
/// `healthy`, `starting`, `stopped` or `unknown`. Every field except `name` is optional here so
/// a row with missing keys still parses.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ServedModel {
    /// Catalog name when tt knows it, else the best identity it has.
    pub name: String,
    /// Which tool started the server (see the struct doc).
    #[serde(default)]
    pub backend: Option<String>,
    /// Container name. `None` for a server found only by probing.
    #[serde(default)]
    pub container: Option<String>,
    /// Container id, 12 characters.
    #[serde(default)]
    pub container_id: Option<String>,
    /// Container image.
    #[serde(default)]
    pub image: Option<String>,
    /// Published host port. `None` when the container publishes none.
    #[serde(default)]
    pub port: Option<u16>,
    /// `http://127.0.0.1:<port>/v1` when the port is known.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Docker's `State.Status`, verbatim: `running`, `exited`, `created`, ...
    #[serde(default)]
    pub status: Option<String>,
    /// `healthy` once `/v1/models` answered with a model list (see the struct doc).
    #[serde(default)]
    pub health: Option<String>,
    /// ISO 8601 start time.
    #[serde(default)]
    pub started_at: Option<String>,
    /// Seconds since start, for running rows.
    #[serde(default)]
    pub uptime_s: Option<u64>,
    /// tt-model profile, for bundles.
    #[serde(default)]
    pub profile: Option<String>,
    /// The id `GET /v1/models` reports, once healthy.
    #[serde(default)]
    pub served_id: Option<String>,
}

/// The shape of `tt --json model ps`: `{"served": [...], "probed": <bool>}`.
#[derive(Debug, Deserialize)]
struct PsPayload {
    served: Vec<ServedModel>,
}

/// Parse `tt --json model ps` output. Pure.
///
/// An empty `served` array is the normal answer on an idle box (`tt` exits 0 with an empty
/// list when nothing is served). Output without a `served` array is an error. If tt changes
/// its format, the agent reports that, and does not conclude that nothing is running.
pub fn parse_ps(json: &str) -> Result<Vec<ServedModel>> {
    let payload: PsPayload = serde_json::from_str(json)
        .context("`tt --json model ps` output did not parse as {\"served\": [...]}")?;
    Ok(payload.served)
}

/// Parse `tt --json model list` into the agent's `GET /models` shape. Pure.
///
/// Keeps models with a vLLM engine (`vLLM` for catalog models, `vllm-plugin` for bundles),
/// because this backend waits for an OpenAI-compatible `/v1/models` and only vLLM servers
/// provide one. `devices` is tt's `hardware` list and `downloaded` is tt's `cached` flag.
/// `release_version` is `None`: `tt` does not report one in this listing.
pub fn parse_model_list(json: &str) -> Result<ModelsResponse> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("`tt --json model list` output is not JSON")?;
    let models = value
        .get("models")
        .and_then(serde_json::Value::as_array)
        .context("`tt --json model list` output has no `models` array")?;
    let strings = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok(ModelsResponse {
        release_version: None,
        models: models
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let engines = strings(m.get("engines"));
                if !engines
                    .iter()
                    .any(|e| e.to_ascii_lowercase().contains("vllm"))
                {
                    return None;
                }
                Some(ModelInfo {
                    name,
                    devices: strings(m.get("hardware")),
                    downloaded: m
                        .get("cached")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect(),
    })
}

/// Read the served model id from a `/v1/models` body: the first entry of a non-empty `data`
/// array. `None` means "not ready yet" (no body, not JSON, empty or missing `data`).
fn listed_model_id(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let first = value.get("data")?.as_array()?.first()?;
    first.get("id")?.as_str().map(str::to_string)
}

/// Turn a failed `tt` call into one readable line of explanation.
///
/// Under `--json`, `tt` prints errors as `{"error": {"what", "why", "next_step", ...}}` on
/// stdout. Without that object, stderr is used, then the last lines of stdout (where `tt serve`
/// streams `run.py`'s own output), then "(no output)".
fn tt_failure_detail(captured: &CapturedOutput) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&captured.stdout) {
        if let Some(err) = value.get("error") {
            let field = |k: &str| err.get(k).and_then(serde_json::Value::as_str);
            let mut out = field("what").unwrap_or("tt reported an error").to_string();
            if let Some(why) = field("why") {
                out.push_str(&format!(" {why}"));
            }
            if let Some(next) = field("next_step") {
                out.push_str(&format!(" Next step: {next}"));
            }
            return out;
        }
    }
    let tail = |s: &str| {
        let lines: Vec<&str> = s.lines().collect();
        lines[lines.len().saturating_sub(20)..].join("\n")
    };
    if !captured.stderr.is_empty() {
        return tail(&captured.stderr);
    }
    if !captured.stdout.is_empty() {
        return tail(&captured.stdout);
    }
    "(no output)".to_string()
}

/// The `tt` serving backend. See the module doc.
pub struct TtBackend {
    config: TtConfig,
    runner: Box<dyn CommandRunner>,
    /// The gozer binary this backend leases chips through, or `None` when gozer is not
    /// installed (leasing off, the default). Set with [`TtBackend::with_gozer`].
    gozer: Option<crate::gozer::Capability>,
    /// The lease held for the running serve, if any. Set only when `start` succeeds (the
    /// guard is disarmed and its id moved here). `stop` releases it after the server is
    /// stopped. Every failing `start` leaves it `None` and lets the guard release instead.
    lease: Mutex<Option<String>>,
    /// What this backend is serving: the model name (the name `tt model stop` needs, which can
    /// differ from the `/v1/models` id), the port, and where the record came from. See
    /// [`Served`].
    served: Mutex<Option<Served>>,
    /// The serving port named in the `who` of a lease adopted from a previous agent process
    /// (`tt-station:<port>:<model>`). Used to find that process's server with `tt model ps`
    /// before the adopted lease is released. See `stop_inner`.
    adopted_port: Mutex<Option<u16>>,
    /// Set by `stop` so an in-flight `start` stops polling; cleared when `start` begins.
    /// Same purpose as `RunPyBackend`'s flag: until `start` returns, the agent's status is
    /// still idle, so `/stop` cannot be gated on "a model is serving". `Arc` so tests can set
    /// it from inside a fake runner call, at an exact point in `start`.
    cancel: Arc<AtomicBool>,
    /// Serialises the end of a successful `start` (check `cancel`, record `served`, move the
    /// lease in) against `stop` setting `cancel`. With it, a `/stop` lands either before the
    /// handover (then `start` sees `cancel`, stops the model and lets the lease guard release)
    /// or after it (then `stop` finds `served` and the lease, and stops and releases them).
    /// There is no third case where both sides think the other one cleaned up.
    handover: Mutex<()>,
    /// True while a `start` is in flight. `discover_own_serve` then reports nothing, so a
    /// `/status` poll during bring-up does not record the loading server as serving.
    starting: AtomicBool,
    /// The last good `tt --json model ps` rows and when they were read. The mutex is also the
    /// single-flight lock: it is held while `ps` runs, so concurrent callers wait for that one
    /// run and then reuse its result instead of each starting a `tt` process.
    ps_cache: Mutex<PsCache>,
}

/// The serve this backend has recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Served {
    /// The name to pass to `tt model stop`.
    name: String,
    /// The port it serves on.
    port: u16,
    /// `false` when this process's own `start` recorded it (trusted). `true` when
    /// `discover_own_serve` adopted it from a possibly cached `tt model ps` read: then
    /// `stop_inner` confirms it with a fresh read before it stops anything or releases a lease.
    discovered: bool,
}

/// The `ps` cache. Behind one mutex, which is also the single-flight lock (held while `ps`
/// runs).
#[derive(Default)]
struct PsCache {
    /// The last run: when it started and finished, and its outcome, success or failure. Callers
    /// that queued behind a run reuse this, so a failing or hung `tt` is not run once per caller.
    last: Option<LastPs>,
    /// The last successful rows and when they were read, for the stale fallback.
    good: Option<(std::time::Instant, Vec<ServedModel>)>,
}

struct LastPs {
    started: std::time::Instant,
    finished: std::time::Instant,
    outcome: std::result::Result<Vec<ServedModel>, String>,
}

/// Whether a `ps` read may come from the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PsRead {
    /// Read-only paths: reuse rows younger than `ps_cache_ttl`, and fall back to rows younger
    /// than `ps_stale_limit` when the run fails.
    Cached,
    /// Paths that decide what to stop or release (chips get reset): always run `ps`, never fall
    /// back to old rows.
    Fresh,
}

/// Clears `TtBackend::starting` when `start_with_options` returns, on every path.
struct StartingFlag<'a>(&'a AtomicBool);

impl Drop for StartingFlag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl TtBackend {
    /// Build a backend with leasing off. Nothing is assumed to be serving.
    pub fn new(config: TtConfig, runner: Box<dyn CommandRunner>) -> Self {
        TtBackend {
            config,
            runner,
            gozer: None,
            lease: Mutex::new(None),
            served: Mutex::new(None),
            adopted_port: Mutex::new(None),
            cancel: Arc::new(AtomicBool::new(false)),
            handover: Mutex::new(()),
            starting: AtomicBool::new(false),
            ps_cache: Mutex::new(PsCache::default()),
        }
    }

    /// Attach the gozer capability the agent probed for at startup, turning leasing on.
    /// `None` keeps it off. `make_backend` calls it in production.
    pub fn with_gozer(mut self, gozer: Option<crate::gozer::Capability>) -> Self {
        self.gozer = gozer;
        self
    }

    /// Test-only: the lease id this backend currently holds.
    #[cfg(test)]
    fn held_lease(&self) -> Option<String> {
        self.lease.lock().expect("lease mutex poisoned").clone()
    }

    /// Test-only: what this backend has recorded as serving.
    #[cfg(test)]
    fn served(&self) -> Option<(String, u16)> {
        self.served
            .lock()
            .expect("served mutex poisoned")
            .as_ref()
            .map(|s| (s.name.clone(), s.port))
    }

    /// `[tt_bin, args...]` as owned strings.
    fn tt_argv(&self, args: &[&str]) -> Vec<String> {
        std::iter::once(self.config.tt_bin.clone())
            .chain(args.iter().map(|s| s.to_string()))
            .collect()
    }

    /// Run the official CLI with `args` and capture everything. A spawn failure (no such
    /// binary) becomes an error that says how to install it.
    fn run_tt(&self, args: &[&str], env: &[(&str, &str)]) -> Result<CapturedOutput> {
        let argv = self.tt_argv(args);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        self.runner
            .run_capturing_with_env(&refs, env)
            .with_context(|| {
                format!(
                    "could not run `{}`; install the official CLI: uv tool install tenstorrent",
                    self.config.tt_bin
                )
            })
    }

    /// Prove the configured binary is the official CLI. Returns its version.
    ///
    /// The official CLI answers `--version` with `tt <semver>`. Anything else (a stale copy of
    /// tt-station's own pre-rename CLI, an unrelated `tt`) is refused, before any chips are
    /// leased. See `libttstation::official_tt` for the incident behind this check.
    fn check_official(&self) -> Result<String> {
        let captured = self.run_tt(&["--version"], &[])?;
        official_tt::parse_version(&captured.stdout)
            .filter(|_| captured.code == 0)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "tt backend: {}",
                    official_tt::not_official_message(&self.config.tt_bin, &captured.stdout)
                )
            })
    }

    /// Ask `tt --json serve <model> --dry-run` how it would serve `model`. Read-only: a dry
    /// run needs no docker and opens no device. An unknown model fails here, with tt's own
    /// explanation, before anything is leased or started.
    fn classify(&self, model: &str) -> Result<ServeTarget> {
        let captured = self.run_tt(&["--json", "serve", model, "--dry-run"], &[])?;
        if captured.code != 0 {
            return Err(anyhow::anyhow!(
                "tt backend: `tt serve {model} --dry-run` refused the model (exit {}): {}",
                captured.code,
                tt_failure_detail(&captured)
            ));
        }
        let plan: serde_json::Value =
            serde_json::from_str(&captured.stdout).with_context(|| {
                format!("tt backend: `tt --json serve {model} --dry-run` did not print JSON")
            })?;
        Ok(ServeTarget::from_dry_run(&plan))
    }

    /// `tt --json model stop <name>`. `tt` treats "nothing running for that model" as success.
    ///
    /// Note that `tt model stop` stops every tt-inference-server container serving that model
    /// on the box, whatever port it is on. It does not stop a container it cannot identify; it
    /// reports it and exits non-zero, which this surfaces as an error.
    fn tt_model_stop(&self, name: &str) -> Result<()> {
        let captured = self.run_tt(&["--json", "model", "stop", name], &[])?;
        if captured.code != 0 {
            return Err(anyhow::anyhow!(
                "tt backend: `tt model stop {name}` failed (exit {}): {}",
                captured.code,
                tt_failure_detail(&captured)
            ));
        }
        Ok(())
    }

    /// Best-effort stop on a failed `start`, run BEFORE the lease guard drops. `gozer release`
    /// resets the released chips, so releasing under a container that is still running would
    /// reset chips in use and then advertise them free. A failed stop is logged; the caller is
    /// already returning an error.
    fn stop_after_failure(&self, model: &str) {
        if let Err(err) = self.tt_model_stop(model) {
            eprintln!(
                "tt-station-agentd: could not stop '{model}' after a failed serve: {err:#} -- \
                 if a lease is held, releasing it will reset chips that may still be in use"
            );
        }
    }

    /// Hand back the lease held for a running serve, if any. `take()` under one lock, so two
    /// concurrent stops cannot both release the same id (gozer reuses ids, so the second
    /// release could land on another tenant's lease). A refused release puts the id back,
    /// unless a newer lease has been stored meanwhile, so a later `stop` can retry.
    fn release_lease(&self) -> Result<()> {
        let Some(capability) = &self.gozer else {
            return Ok(());
        };
        let Some(lease_id) = self.lease.lock().expect("lease mutex poisoned").take() else {
            return Ok(());
        };
        if let Err(err) = crate::gozer::release(self.runner.as_ref(), capability, &lease_id) {
            let mut slot = self.lease.lock().expect("lease mutex poisoned");
            if slot.is_none() {
                *slot = Some(lease_id);
            } else {
                eprintln!(
                    "tt-station-agentd: lease '{lease_id}' was refused release and a newer \
                     lease has since been recorded; clean it up by hand -- check `gozer status`"
                );
            }
            return Err(err);
        }
        Ok(())
    }

    /// Stop what this backend is serving, then release its lease. The order matters: the
    /// release resets chips, so the server must be gone first. A failed stop returns before the
    /// release, leaving the lease recorded so a later stop retries both.
    ///
    /// `hint` is used when this process did not start the serve itself, for example after an
    /// agent restart: the route passes the model id it last reported, and `tt model stop`
    /// accepts a catalog model's Hugging Face id as well as its catalog name.
    ///
    /// **A lease held with nothing recorded.** After an agent restart, `adopt_lease` records the
    /// previous process's lease while `served` is empty, and the route may pass no `hint`
    /// either. Releasing that lease straight away would reset the chips under the old server,
    /// which is still running. So in that case the server is looked up with `tt model ps` (the
    /// running row on the adopted lease's port, else `service_port`) and stopped by name first.
    /// If `tt model ps` fails, or the row's name is not a safe model name, this returns an error
    /// and releases nothing. No running row on that port means nothing is using the lease, and
    /// it is released.
    ///
    /// **A name found by discovery.** A name `discover_own_serve` recorded may come from a
    /// cached `tt model ps` read, possibly long ago. It is always confirmed with a fresh read
    /// first (see `confirm_discovered`), with or without a lease: the stop must reach only the
    /// server now on our port, and with a lease the stop is followed by a chip reset.
    fn stop_inner(&self, hint: &str) -> Result<()> {
        let recorded = self.served.lock().expect("served mutex poisoned").clone();
        let lease_held = self.lease.lock().expect("lease mutex poisoned").is_some();
        // A discovered name is always confirmed with a fresh read, lease or not: it may be
        // seconds or hours old, and `tt model stop` must not hit a server that is no longer on
        // our port. That fresh read already answers "what runs on our port", so the
        // identification below is skipped after it (no second `ps`, and a failure of a second
        // read cannot refuse a release that the first clean read allowed).
        let (mut name, confirmed) = match recorded {
            Some(Served {
                name,
                discovered: true,
                ..
            }) => (self.confirm_discovered(&name)?, true),
            Some(served) => (Some(served.name), false),
            None => ((!hint.is_empty()).then(|| hint.to_string()), false),
        };
        if name.is_none() && lease_held && !confirmed {
            name = self.identify_untracked_serve()?;
        }
        if let Some(name) = name {
            self.tt_model_stop(&name)?;
        }
        *self.served.lock().expect("served mutex poisoned") = None;
        *self
            .adopted_port
            .lock()
            .expect("adopted_port mutex poisoned") = None;
        self.release_lease()
    }

    /// Confirm a discovered serve with a fresh `tt model ps` before stopping it (and before
    /// releasing a lease, when one is held). Returns the name to stop: the running row on this backend's own port, which may
    /// differ from `recorded` if the server changed since the cached read. `Ok(None)` when
    /// nothing runs anywhere under that name and nothing runs on our port: the lease is then
    /// released as for any idle lease. Refuses (error, lease kept) when `ps` fails, when the
    /// row's name is unsafe, or when `recorded` is now running on another port: that server is
    /// not on this agent's port, so it is not this agent's to stop, and releasing would reset
    /// chips under it.
    fn confirm_discovered(&self, recorded: &str) -> Result<Option<String>> {
        let port = self.own_port();
        let rows = self.ps(PsRead::Fresh).context(
            "tt backend: could not confirm the running server with `tt model ps`; refusing to \
             stop it (and to release any lease)",
        )?;
        let running = |r: &&ServedModel| r.status.as_deref() == Some("running");
        if let Some(row) = rows.iter().filter(running).find(|r| r.port == Some(port)) {
            validate_model_name(&row.name).map_err(|_| {
                anyhow::anyhow!(
                    "tt backend: `tt model ps` names the server on port {port} {:?}, which \
                     cannot be passed to `tt model stop`; refusing to stop it (and to release \
                     any lease)",
                    row.name
                )
            })?;
            return Ok(Some(row.name.clone()));
        }
        if let Some(moved) = rows.iter().filter(running).find(|r| r.name == recorded) {
            return Err(anyhow::anyhow!(
                "tt backend: '{recorded}' is now running on port {:?}, not on this agent's port \
                 {port}; refusing to stop it (and to release any lease). Check `tt model ps`",
                moved.port
            ));
        }
        Ok(None)
    }

    /// Find the server running under a lease this process did not start (see `stop_inner`).
    /// `Ok(None)` means nothing is running on the lease's port.
    fn identify_untracked_serve(&self) -> Result<Option<String>> {
        let port = self.own_port();
        let row = self.own_running_row(PsRead::Fresh).context(
            "tt backend: a chip lease is held but this agent did not start the server using \
             it, and `tt model ps` could not say what is running; refusing to release the lease \
             (that would reset chips under a live server). Stop the server by hand, then retry",
        )?;
        let Some(row) = row else {
            return Ok(None);
        };
        validate_model_name(&row.name).map_err(|_| {
            anyhow::anyhow!(
                "tt backend: a chip lease is held for the server on port {port}, but `tt model \
                 ps` names it {:?}, which cannot be passed to `tt model stop`; refusing to \
                 release the lease. Stop that server by hand, then retry",
                row.name
            )
        })?;
        Ok(Some(row.name))
    }

    /// The port this backend's serve lives on: the one it last served on, else the port in an
    /// adopted lease's `who`, else the configured `service_port`.
    fn own_port(&self) -> u16 {
        let recorded = self
            .served
            .lock()
            .expect("served mutex poisoned")
            .as_ref()
            .map(|s| s.port);
        recorded
            .or(*self
                .adopted_port
                .lock()
                .expect("adopted_port mutex poisoned"))
            .unwrap_or(self.config.service_port)
    }

    /// The running `tt model ps` row on [`TtBackend::own_port`], if any. This is the single
    /// rule for "which server is ours" used by the adopted-lease stop, by
    /// `discover_own_serve`, `status` and the `agent` source in `serving_entries`.
    fn own_running_row(&self, read: PsRead) -> Result<Option<ServedModel>> {
        let port = self.own_port();
        Ok(self
            .ps(read)?
            .into_iter()
            .find(|r| r.port == Some(port) && r.status.as_deref() == Some("running")))
    }

    /// `tt --json model ps`, through the cache (see [`PsCache`] and [`PsRead`]).
    ///
    /// The cache mutex is held across the run, so concurrent callers queue behind one run. A
    /// queued caller reuses the outcome of the run it waited for, failure included:
    /// * `Cached`: any outcome that finished less than `ps_cache_ttl` ago. On a failed outcome,
    ///   rows younger than `ps_stale_limit` are returned instead of the error.
    /// * `Fresh`: only an outcome whose run STARTED after this caller arrived (so it is as new
    ///   as a run of its own would be). Otherwise it runs `ps` itself. No stale fallback.
    fn ps(&self, read: PsRead) -> Result<Vec<ServedModel>> {
        let arrived = std::time::Instant::now();
        // The cache holds plain data, so a panic while it was held leaves nothing half-updated
        // that matters: recover the guard instead of panicking on every later call.
        let mut cache = self
            .ps_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let reusable = cache.last.as_ref().and_then(|last| {
            let ok = match read {
                PsRead::Cached => last.finished.elapsed() < self.config.ps_cache_ttl,
                PsRead::Fresh => last.started >= arrived,
            };
            ok.then(|| last.outcome.clone())
        });
        let outcome = match reusable {
            Some(outcome) => outcome,
            None => {
                let started = std::time::Instant::now();
                let outcome = self.ps_run().map_err(|e| format!("{e:#}"));
                let finished = std::time::Instant::now();
                if let Ok(rows) = &outcome {
                    cache.good = Some((finished, rows.clone()));
                }
                cache.last = Some(LastPs {
                    started,
                    finished,
                    outcome: outcome.clone(),
                });
                outcome
            }
        };
        match outcome {
            Ok(rows) => Ok(rows),
            Err(msg) => match (read, cache.good.as_ref()) {
                (PsRead::Cached, Some((at, rows))) if at.elapsed() < self.config.ps_stale_limit => {
                    eprintln!(
                        "tt-station-agentd: {msg}; using `tt model ps` rows from {:?} ago",
                        at.elapsed()
                    );
                    Ok(rows.clone())
                }
                _ => Err(anyhow::anyhow!(msg)),
            },
        }
    }

    /// One `tt --json model ps` run, bounded by `ps_timeout`, parsed.
    fn ps_run(&self) -> Result<Vec<ServedModel>> {
        let argv = self.tt_argv(&["--json", "model", "ps"]);
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        let captured = self
            .runner
            .run_capturing_with_timeout(&refs, &[], self.config.ps_timeout)
            .with_context(|| {
                format!("tt backend: `{} model ps` did not run", self.config.tt_bin)
            })?;
        if captured.code != 0 {
            return Err(anyhow::anyhow!(
                "tt backend: `tt model ps` failed (exit {}): {}",
                captured.code,
                tt_failure_detail(&captured)
            ));
        }
        parse_ps(&captured.stdout)
    }
}

impl ServingBackend for TtBackend {
    fn backend_name(&self) -> &'static str {
        "tt"
    }

    /// The no-options form: exactly `start_with_options` with empty options.
    fn start_forcing(&self, model: &str, force: bool) -> Result<Endpoint> {
        self.start_with_options(model, force, &RunOptions::default())
    }

    /// Start `model` through `tt serve`. See the module doc for the steps and their order.
    ///
    /// Option problems are returned as [`OptionError`] (the route answers 400 with the field
    /// name), and they are all found before any chips are leased or anything is started.
    /// `force` reaches only the lease decision (see `gozer::acquire_for_serve`).
    fn start_with_options(
        &self,
        model: &str,
        force: bool,
        options: &RunOptions,
    ) -> Result<Endpoint> {
        // A `/stop` from an earlier run must not cancel this one.
        self.cancel.store(false, Ordering::SeqCst);
        self.starting.store(true, Ordering::SeqCst);
        let _starting = StartingFlag(&self.starting);

        // (1) The model name and the options. The route validated both already; checking again
        // costs nothing and keeps this backend safe for any other caller. The model name goes
        // into several argvs and into gozer's `--who`, so nothing runs before this check.
        validate_model_name(model)?;
        options.validate()?;
        if let Some(w) = options.workflow.filter(|w| *w != Workflow::Server) {
            // `benchmarks` and `evals` run in the foreground until they finish, then stop the
            // container, so there is no lasting `/v1` to hand back. `tt serve` would block
            // this request (and hold a lease) for the whole run and then fail the readiness
            // poll.
            return Err(OptionError {
                field: "workflow".to_string(),
                message: format!(
                    "the agent serves models over /v1; the `{}` workflow is not supported \
                     through POST /run",
                    w.as_str()
                ),
            }
            .into());
        }

        // (2) The binary must be the official CLI.
        let version = self.check_official()?;

        // (3) Catalog model or bundle, from tt itself. Then the exact argv, refusing options
        // that do not apply to this kind of model.
        let target = self.classify(model)?;
        let port = options.port.unwrap_or(self.config.service_port);
        let serve_args = serve_argv_for(target, model, self.config.service_port, options)?;
        eprintln!(
            "tt-station-agentd: tt {version}: serving '{model}' as a {} model on port {port}",
            match target {
                ServeTarget::Catalog => "catalog",
                ServeTarget::Bundle => "tt-model bundle",
            }
        );

        // (4) Swap: stop what this backend is already serving and give back its lease, so the
        // new lease is not requested while the old one is still held. Stop first, release
        // second (the release resets chips).
        self.stop_inner("")
            .context("tt backend: failed to stop the previous serve before starting a new one")?;

        // (5) Lease. The guard releases on drop, which covers every failure exit below. Only
        // the success path disarms it.
        let lease = crate::gozer::acquire_for_serve(
            self.runner.as_ref(),
            self.gozer.as_ref(),
            "tt",
            port,
            model,
            force,
        )?;

        // (6) `tt --json serve ...`. The grant's environment (today `TT_VISIBLE_DEVICES=<bdfs>`)
        // is set on the child only. tt-model ignores it and mounts every device (see the module
        // doc), so it pins nothing; it is passed so a future tt that honours it gets it.
        let child_env: Vec<(&str, &str)> = lease
            .as_ref()
            .map(|guard| {
                guard
                    .grant()
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        let mut args: Vec<&str> = vec!["--json"];
        args.extend(serve_args.iter().map(String::as_str));
        let served = self.run_tt(&args, &child_env);
        match &served {
            Ok(captured) if captured.code == 0 => {}
            _ => {
                // `tt serve` can start a container and then fail. Stop before the lease guard
                // drops, so the release does not reset chips under it.
                self.stop_after_failure(model);
                return Err(match served {
                    Ok(captured) => anyhow::anyhow!(
                        "tt backend: `tt serve {model}` failed (exit {}): {}",
                        captured.code,
                        tt_failure_detail(&captured)
                    ),
                    Err(err) => err,
                });
            }
        }

        // (7) Wait until `/v1/models` lists a model. `poll_until_healthy` does the bounded
        // "probe, sleep" step, one attempt per round, so a `/stop` is noticed within one
        // interval. A 200 with an empty `data` array means vLLM is up but not ready, so the
        // body is checked too.
        let models_url = format!("http://{}:{port}/v1/models", self.config.serving_host);
        let mut served_id = None;
        for _ in 0..self.config.health_poll_attempts {
            if self.cancel.load(Ordering::SeqCst) {
                self.stop_after_failure(model);
                return Err(anyhow::anyhow!(
                    "tt backend: serve of '{model}' aborted by a stop request"
                ));
            }
            if super::poll_until_healthy(
                self.runner.as_ref(),
                &models_url,
                1,
                self.config.health_poll_interval,
            ) {
                if let Some(id) = self
                    .runner
                    .http_get(&models_url)
                    .ok()
                    .and_then(|body| listed_model_id(&body))
                {
                    served_id = Some(id);
                    break;
                }
                std::thread::sleep(self.config.health_poll_interval);
            }
        }
        let Some(served_id) = served_id else {
            self.stop_after_failure(model);
            return Err(anyhow::anyhow!(
                "tt backend: '{model}' was not listed on {models_url} within {} attempts",
                self.config.health_poll_attempts
            ));
        };

        // Handover, under the `handover` lock (see that field). A `/stop` that arrived after the
        // last poll and before this point aborts the start: the model is stopped here and the
        // lease guard releases on return. Otherwise record what was started and move the lease
        // into the backend so it outlives this call. This is the only exit that does not release.
        {
            let _handover = self.handover.lock().expect("handover mutex poisoned");
            if self.cancel.load(Ordering::SeqCst) {
                self.stop_after_failure(model);
                *self.served.lock().expect("served mutex poisoned") = None;
                return Err(anyhow::anyhow!(
                    "tt backend: serve of '{model}' aborted by a stop request"
                ));
            }
            *self.served.lock().expect("served mutex poisoned") = Some(Served {
                name: model.to_string(),
                port,
                discovered: false,
            });
            if let Some(guard) = lease {
                *self.lease.lock().expect("lease mutex poisoned") = Some(guard.into_lease_id());
            }
        }

        Ok(Endpoint {
            base_url: format!("http://{}:{port}/v1", self.config.serving_host),
            model: served_id,
            requires_key: false,
        })
    }

    /// Stop the serve: `tt model stop <name>` with the name this backend passed to `tt serve`
    /// (falling back to `model`, the id the agent last reported), then release the lease.
    /// Also cancels an in-flight `start`.
    fn stop(&self, model: &str) -> Result<()> {
        {
            // Under `handover`, so this lands wholly before or wholly after the end of an
            // in-flight `start`. See the `handover` field.
            let _handover = self.handover.lock().expect("handover mutex poisoned");
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.stop_inner(model)
    }

    /// Take ownership of a lease a previous agent process left behind, so the next `stop`
    /// releases (and resets) it. Refuses when leasing is off or a lease is already held.
    ///
    /// The port in `who` (`tt-station:<port>:<model>`) is kept, so the next stop can find the
    /// previous process's server with `tt model ps` before it releases (see `stop_inner`).
    fn adopt_lease(&self, lease_id: &str, who: &str) -> bool {
        if self.gozer.is_none() {
            return false;
        }
        let mut held = self.lease.lock().expect("lease mutex poisoned");
        if held.is_some() {
            return false;
        }
        *held = Some(lease_id.to_string());
        *self
            .adopted_port
            .lock()
            .expect("adopted_port mutex poisoned") = crate::gozer::who_service_port(who);
        true
    }

    /// What is serving on this backend's port, according to `tt --json model ps`, which is
    /// the source of truth for this backend. A running row on the port counts as serving
    /// whatever its `health`: `tt model ps` probes with a 1.5 s timeout, and a busy server that
    /// misses it shows `starting`, which must not make the agent declare the model gone.
    ///
    /// The port is [`TtBackend::own_port`], the same rule as every other path. The `ps` read
    /// may come from the short cache (see [`PS_CACHE_TTL`]). The reported id is the row's `served_id`, else the recorded model name, else the row's
    /// `name`. Because this reads the box through `tt`, it also sees a serve
    /// started before an agent restart once the route asks. The route consults the backend
    /// only while it believes a model is serving; after a restart `discover_own_serve` is what
    /// brings the stored status back to serving.
    fn status(&self) -> Result<ServingStatus> {
        let recorded = self.served.lock().expect("served mutex poisoned").clone();
        let row = self.own_running_row(PsRead::Cached)?;
        Ok(match row {
            Some(r) => ServingStatus::Serving(
                r.served_id
                    .clone()
                    .or_else(|| recorded.map(|s| s.name))
                    .unwrap_or_else(|| r.name.clone()),
            ),
            None => ServingStatus::Idle,
        })
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn gozer_capability(&self) -> Option<&crate::gozer::Capability> {
        self.gozer.as_ref()
    }

    /// `/serving` entries from `tt --json model ps`, which (unlike docker discovery) recognises
    /// tt-model containers as well as tt-inference-server and TT-Studio ones.
    ///
    /// One entry per running row that has a published port and a `served_id` (tt sets it only
    /// once `/v1/models` answered, matching discovery's readiness rule). `model` is the
    /// `served_id`, `base_url` uses the configured `serving_host` and the row's port,
    /// `container` is the container name. `source` is `agent` for the row on this backend's own
    /// port ([`TtBackend::own_port`]) and `external` for the rest. `None` when `tt model ps`
    /// fails, so `/serving` falls back to discovery alone.
    fn serving_entries(&self) -> Option<Vec<ServingEntry>> {
        let rows = match self.ps(PsRead::Cached) {
            Ok(rows) => rows,
            Err(err) => {
                eprintln!("tt-station-agentd: /serving: {err:#}; using docker discovery only");
                return None;
            }
        };
        let own = self.own_port();
        Some(
            rows.into_iter()
                .filter(|r| r.status.as_deref() == Some("running"))
                .filter_map(|r| {
                    let port = r.port?;
                    let model = r.served_id?;
                    Some(ServingEntry {
                        model,
                        base_url: format!("http://{}:{port}/v1", self.config.serving_host),
                        host_port: port,
                        container: r.container.unwrap_or_default(),
                        source: if port == own { "agent" } else { "external" }.to_string(),
                    })
                })
                .collect(),
        )
    }

    /// The serve on this backend's own port, found with `tt --json model ps`, when the backend
    /// has no record of it (after an agent restart). It is then recorded as this backend's
    /// serve, so `stop` passes the row's name to `tt model stop` (for a bundle the `/v1/models`
    /// id is the weights repo, which `tt model stop` would not recognise).
    ///
    /// Reports nothing while a `start` is in flight, when a serve is already recorded, when
    /// `tt model ps` fails, when the row has no `served_id` yet (still loading), or when its
    /// name is not a safe model name. A row on any other port is not ours.
    fn discover_own_serve(&self) -> Option<Endpoint> {
        if self.starting.load(Ordering::SeqCst)
            || self.served.lock().expect("served mutex poisoned").is_some()
        {
            return None;
        }
        let row = self.own_running_row(PsRead::Cached).ok()??;
        let port = row.port?;
        let model = row.served_id.clone()?;
        validate_model_name(&row.name).ok()?;
        // Re-check under `handover`: a `start` may have begun, or recorded its own serve, while
        // `ps` ran. A stale discovery must not overwrite that.
        let _handover = self.handover.lock().expect("handover mutex poisoned");
        let mut served = self.served.lock().expect("served mutex poisoned");
        if self.starting.load(Ordering::SeqCst) || served.is_some() {
            return None;
        }
        *served = Some(Served {
            name: row.name,
            port,
            discovered: true,
        });
        Some(Endpoint {
            base_url: format!("http://{}:{port}/v1", self.config.serving_host),
            model,
            requires_key: false,
        })
    }

    /// Models from `tt --json model list` that have a vLLM engine. See [`parse_model_list`].
    fn list_models(&self) -> Result<ModelsResponse> {
        let captured = self.run_tt(&["--json", "model", "list"], &[])?;
        if captured.code != 0 {
            return Err(anyhow::anyhow!(
                "tt backend: `tt model list` failed (exit {}): {}",
                captured.code,
                tt_failure_detail(&captured)
            ));
        }
        parse_model_list(&captured.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// `tt --json model ps` on this box with nothing served (tt 1.0.1, 2026-10-05), verbatim.
    const PS_EMPTY: &str = r#"{
  "served": [],
  "probed": true
}"#;

    /// A non-empty `tt --json model ps` with a catalog row and a tt-studio row. Constructed,
    /// not captured: it was built field for field from tt-cli's `ServedModel` dataclass
    /// (`src/tenstorrent/models/served.py`). Task 4's live capture ([`PS_LIVE_BUNDLE`]) has the
    /// same thirteen keys in the same order, so the shape is confirmed. The catalog
    /// (`inference-server`) and `studio` rows themselves have not been seen live: only a
    /// tt-model bundle was served on this box (no catalog model is cached here).
    const PS_ONE_RUNNING: &str = r#"{
  "served": [
    {
      "name": "Llama-3.1-8B-Instruct",
      "backend": "inference-server",
      "container": "tt-inference-server-3f2a",
      "container_id": "0123456789ab",
      "image": "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.19.0-b204341-9bd099c",
      "port": 8003,
      "base_url": "http://127.0.0.1:8003/v1",
      "status": "running",
      "health": "healthy",
      "started_at": "2026-10-05T18:00:00Z",
      "uptime_s": 120,
      "profile": null,
      "served_id": "meta-llama/Llama-3.1-8B-Instruct"
    },
    {
      "name": "Qwen3-8B",
      "backend": "studio",
      "container": "Qwen3-8B",
      "container_id": "ba9876543210",
      "image": null,
      "port": 9000,
      "base_url": "http://127.0.0.1:9000/v1",
      "status": "running",
      "health": "starting",
      "started_at": null,
      "uptime_s": null,
      "profile": null,
      "served_id": null
    }
  ],
  "probed": true
}"#;

    /// `tt --json model ps` captured live on tsingletaryTT-quietbox (4x p300c, tt 1.0.1,
    /// 2026-10-05), verbatim, while this backend served the tt-model bundle
    /// `changh95/qwen3.8-27b-p300x2` on port 20010 (Task 4 of the Linux tray plan). Note `name` is the bundle id, `backend` is
    /// `model-manager`, `profile` is set, and `served_id` is the weights repo vLLM reports.
    const PS_LIVE_BUNDLE: &str = r#"{
  "served": [
    {
      "name": "changh95/qwen3.8-27b-p300x2",
      "backend": "model-manager",
      "container": "tt-model-qwen3.8-27b-p300x2-batch32",
      "container_id": "68a7a7fd598f",
      "image": "tt-model/qwen3.8-27b-p300x2:0becf4834925",
      "port": 20010,
      "base_url": "http://127.0.0.1:20010/v1",
      "status": "running",
      "health": "healthy",
      "started_at": "2026-10-06T02:33:40Z",
      "uptime_s": 286,
      "profile": "batch32",
      "served_id": "Qwen/Qwen3.8-27B"
    }
  ],
  "probed": true
}"#;

    /// Trimmed from `tt --json serve Llama-3.1-8B-Instruct --dry-run` (tt 1.0.1, 2026-10-05).
    const DRY_RUN_CATALOG: &str = r#"{"model": "Llama-3.1-8B-Instruct", "workflow": "server",
        "served_through": null, "default_port": "20000", "argv": ["python", "run.py"]}"#;

    /// Trimmed from `tt --json serve Adartras/qwen3.6-a3b-blackhole --dry-run` (tt 1.0.1).
    const DRY_RUN_BUNDLE: &str = r#"{"bundle": null, "backend": "tt-model",
        "model": "Adartras/qwen3.6-a3b-blackhole", "argv": ["tt-model", "serve"]}"#;

    /// `tt --json model stop Nope-Model` (tt 1.0.1, 2026-10-05), verbatim.
    const TT_UNKNOWN_MODEL: &str = r#"{
  "error": {
    "what": "Unknown model 'Nope-Model'.",
    "why": "It is not in the model catalog (bundled model_support.json).",
    "next_step": "Run `tt model list` to browse available models.",
    "exit_code": 2,
    "code": "USAGE",
    "reason": "model.unknown",
    "details": {}
  }
}"#;

    /// A gozer grant, shaped like gozer's own `cmd_acquire --json` output.
    const GRANT_JSON: &str = r#"{"granted":true,"lease_id":"lease-abc123",
        "units":["0100014311601055"],"chips":["0000:01:00.0","0000:02:00.0"],
        "dev_indices":[2,3],"expanded":true,"env":{"TT_VISIBLE_DEVICES":"0000:01:00.0,0000:02:00.0"}}"#;

    const MODELS_BODY: &str =
        r#"{"object":"list","data":[{"id":"meta-llama/Llama-3.1-8B-Instruct","object":"model"}]}"#;

    /// A scripted `CommandRunner`. Every call is appended to one ordered log, so tests can
    /// assert on the ORDER of `gozer acquire`, `tt serve`, `tt model stop` and `gozer release`.
    /// Responses are chosen by the first registered substring of the space-joined argv.
    /// One recorded call's argv (space-joined) and the environment it was given.
    type EnvRecord = (String, Vec<(String, String)>);
    /// A canned response: argv substring, exit code, stdout, stderr.
    type Response = (String, i32, String, String);

    #[derive(Clone, Default)]
    struct ScriptedRunner {
        log: Arc<Mutex<Vec<String>>>,
        envs: Arc<Mutex<Vec<EnvRecord>>>,
        responses: Arc<Mutex<Vec<Response>>>,
        /// `http_get` bodies, consumed in order and sticking at the last one.
        bodies: Arc<Mutex<Vec<String>>>,
        healthy: Arc<AtomicBool>,
        /// Argv substrings whose call fails to SPAWN (`Err`), like a missing binary.
        spawn_failures: Arc<Mutex<Vec<String>>>,
        /// When set, every `http_get` sets this flag first. A test hands in the backend's
        /// cancel flag to land a `/stop` exactly while the readiness GET is answering.
        cancel_on_get: Arc<Mutex<Option<Arc<AtomicBool>>>>,
        /// When set, every `http_get` runs this first (with no lock held), so a test can act
        /// on the backend at an exact point inside `start`.
        on_get: Arc<Mutex<Option<GetHook>>>,
        /// When true, `tt model ps` hangs until the caller's timeout runs out.
        hang_ps: Arc<AtomicBool>,
        /// Extra time each `tt model ps` takes, so concurrent callers really overlap.
        ps_delay: Arc<Mutex<Duration>>,
        /// When set, every `tt model ps` runs this first (with no lock held).
        on_ps: Arc<Mutex<Option<GetHook>>>,
    }

    type GetHook = Arc<dyn Fn() + Send + Sync>;

    impl ScriptedRunner {
        /// The happy path: the official CLI, a catalog model, `tt serve` succeeds, the model is
        /// listed on the first poll, gozer grants and releases.
        fn happy() -> Self {
            let r = ScriptedRunner::default();
            r.healthy.store(true, Ordering::SeqCst);
            r.on("--version", 0, "tt 1.0.1", "");
            r.on("--dry-run", 0, DRY_RUN_CATALOG, "");
            r.on("gozer acquire", 0, GRANT_JSON, "");
            r.on("gozer release", 0, r#"{"released":true}"#, "");
            r.on("model ps", 0, PS_EMPTY, "");
            r.on("model list", 0, r#"{"models":[]}"#, "");
            *r.bodies.lock().unwrap() = vec![MODELS_BODY.to_string()];
            r
        }

        /// Register a response. Later registrations win over earlier ones with an overlapping
        /// substring, so a test can override part of `happy()`.
        fn on(&self, needle: &str, code: i32, stdout: &str, stderr: &str) {
            self.responses.lock().unwrap().insert(
                0,
                (
                    needle.to_string(),
                    code,
                    stdout.to_string(),
                    stderr.to_string(),
                ),
            );
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// How many times `tt model ps` was run.
        fn ps_runs(&self) -> usize {
            self.log().iter().filter(|l| l.contains("model ps")).count()
        }

        /// Index of the first log line containing `needle`.
        fn pos(&self, needle: &str) -> Option<usize> {
            self.log().iter().position(|l| l.contains(needle))
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, args: &[&str]) -> Result<String> {
            let c = self.run_capturing(args)?;
            if c.code == 0 {
                Ok(c.stdout)
            } else {
                Err(anyhow::anyhow!("{} failed: {}", args.join(" "), c.stderr))
            }
        }

        fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
            self.run_capturing_with_env(args, &[])
        }

        fn run_capturing_with_env(
            &self,
            args: &[&str],
            env: &[(&str, &str)],
        ) -> Result<CapturedOutput> {
            let joined = args.join(" ");
            self.log.lock().unwrap().push(format!("run: {joined}"));
            if self
                .spawn_failures
                .lock()
                .unwrap()
                .iter()
                .any(|n| joined.contains(n.as_str()))
            {
                return Err(anyhow::anyhow!(
                    "failed to spawn {joined}: No such file or directory (os error 2)"
                ));
            }
            self.envs.lock().unwrap().push((
                joined.clone(),
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ));
            let found = self
                .responses
                .lock()
                .unwrap()
                .iter()
                .find(|(needle, ..)| joined.contains(needle.as_str()))
                .cloned();
            let (code, stdout, stderr) =
                found
                    .map(|(_, c, o, e)| (c, o, e))
                    .unwrap_or((0, String::new(), String::new()));
            Ok(CapturedOutput {
                code,
                stdout,
                stderr,
            })
        }

        fn health_ok(&self, url: &str) -> bool {
            self.log.lock().unwrap().push(format!("health: {url}"));
            self.healthy.load(Ordering::SeqCst)
        }

        fn run_capturing_with_timeout(
            &self,
            args: &[&str],
            env: &[(&str, &str)],
            timeout: Duration,
        ) -> Result<CapturedOutput> {
            let joined = args.join(" ");
            if joined.contains("model ps") {
                let hook = self.on_ps.lock().unwrap().clone();
                if let Some(hook) = hook {
                    hook();
                }
                let delay = *self.ps_delay.lock().unwrap();
                std::thread::sleep(delay);
                if self.hang_ps.load(Ordering::SeqCst) {
                    self.log
                        .lock()
                        .unwrap()
                        .push(format!("run: {joined} (hung)"));
                    std::thread::sleep(timeout);
                    return Err(anyhow::anyhow!(
                        "{joined} did not finish within {timeout:?}"
                    ));
                }
            }
            self.run_capturing_with_env(args, env)
        }

        fn http_get(&self, url: &str) -> Result<String> {
            self.log.lock().unwrap().push(format!("get: {url}"));
            let hook = self.on_get.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook();
            }
            if let Some(flag) = self.cancel_on_get.lock().unwrap().as_ref() {
                flag.store(true, Ordering::SeqCst);
            }
            let mut bodies = self.bodies.lock().unwrap();
            match bodies.len() {
                0 => Err(anyhow::anyhow!("no body")),
                1 => Ok(bodies[0].clone()),
                _ => Ok(bodies.remove(0)),
            }
        }
    }

    fn config() -> TtConfig {
        TtConfig {
            tt_bin: "/fake/tt".to_string(),
            serving_host: "127.0.0.1".to_string(),
            service_port: 8003,
            health_poll_attempts: 3,
            health_poll_interval: Duration::from_millis(1),
            // No caching by default, so each test sees every `ps` it scripts. The cache tests
            // below set their own limits.
            ps_cache_ttl: Duration::ZERO,
            ps_timeout: Duration::from_secs(1),
            ps_stale_limit: Duration::ZERO,
        }
    }

    fn capability() -> crate::gozer::Capability {
        crate::gozer::Capability {
            path: "gozer".to_string(),
            version: "gozer 0.1.0".to_string(),
        }
    }

    fn backend(runner: &ScriptedRunner) -> TtBackend {
        TtBackend::new(config(), Box::new(runner.clone()))
    }

    fn leasing_backend(runner: &ScriptedRunner) -> TtBackend {
        backend(runner).with_gozer(Some(capability()))
    }

    fn opts(v: serde_json::Value) -> RunOptions {
        RunOptions::from_value(&v).unwrap()
    }

    // ---- pure parsers ----

    #[test]
    fn parse_ps_reads_served_entries() {
        let rows = parse_ps(PS_ONE_RUNNING).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "Llama-3.1-8B-Instruct");
        assert_eq!(rows[0].backend.as_deref(), Some("inference-server"));
        assert_eq!(rows[0].port, Some(8003));
        assert_eq!(rows[0].status.as_deref(), Some("running"));
        assert_eq!(rows[0].health.as_deref(), Some("healthy"));
        assert_eq!(
            rows[0].served_id.as_deref(),
            Some("meta-llama/Llama-3.1-8B-Instruct")
        );
        assert_eq!(rows[1].served_id, None);
        assert_eq!(rows[1].health.as_deref(), Some("starting"));
    }

    /// The live capture parses, and `status` on its port reports vLLM's id (the row's
    /// `served_id`), not the bundle id in `name`. An adopted lease on that port is stopped by
    /// the bundle id, which must pass `validate_model_name`.
    #[test]
    fn parse_ps_reads_a_live_bundle_capture() {
        let rows = parse_ps(PS_LIVE_BUNDLE).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "changh95/qwen3.8-27b-p300x2");
        assert_eq!(row.backend.as_deref(), Some("model-manager"));
        assert_eq!(row.port, Some(20010));
        assert_eq!(row.status.as_deref(), Some("running"));
        assert_eq!(row.health.as_deref(), Some("healthy"));
        assert_eq!(row.profile.as_deref(), Some("batch32"));
        assert_eq!(row.uptime_s, Some(286));
        assert_eq!(row.served_id.as_deref(), Some("Qwen/Qwen3.8-27B"));
        assert!(validate_model_name(&row.name).is_ok());

        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = TtBackend::new(
            TtConfig {
                service_port: 20010,
                ..config()
            },
            Box::new(runner.clone()),
        );
        assert_eq!(
            b.status().unwrap(),
            ServingStatus::Serving("Qwen/Qwen3.8-27B".to_string())
        );
    }

    #[test]
    fn parse_ps_tolerates_empty() {
        assert!(parse_ps(PS_EMPTY).unwrap().is_empty());
        // A row with only a name still parses.
        assert_eq!(
            parse_ps(r#"{"served":[{"name":"x"}]}"#).unwrap()[0].name,
            "x"
        );
        // No `served` array means the format changed. It must not parse as an empty list.
        assert!(parse_ps(r#"{"probed":true}"#).is_err());
        assert!(parse_ps("not json").is_err());
    }

    #[test]
    fn parse_model_list_keeps_vllm_models() {
        // Trimmed from `tt --json model list` on this box (tt 1.0.1, 2026-10-05).
        let json = r#"{"device":"p300x2","scope":"all","models":[
            {"name":"Adartras/qwen3.6-a3b-blackhole","engines":["vllm-plugin"],
             "hardware":["p150"],"cached":false},
            {"name":"bge-m3","engines":["forge"],"hardware":["p300x2"],"cached":false},
            {"name":"Llama-3.1-8B-Instruct","engines":["vLLM","forge"],
             "hardware":["n150","p300x2"],"cached":true}]}"#;
        let got = parse_model_list(json).unwrap();
        let names: Vec<_> = got.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["Adartras/qwen3.6-a3b-blackhole", "Llama-3.1-8B-Instruct"]
        );
        assert_eq!(got.models[1].devices, ["n150", "p300x2"]);
        assert!(got.models[1].downloaded);
        assert!(parse_model_list(r#"{"error":{}}"#).is_err());
    }

    #[test]
    fn tt_errors_are_explained_from_the_json_error_object() {
        let detail = tt_failure_detail(&CapturedOutput {
            code: 2,
            stdout: TT_UNKNOWN_MODEL.to_string(),
            stderr: String::new(),
        });
        assert!(detail.contains("Unknown model 'Nope-Model'."), "{detail}");
        assert!(detail.contains("tt model list"), "{detail}");
    }

    // ---- start ----

    #[test]
    fn refuses_a_stale_tt() {
        let runner = ScriptedRunner::happy();
        runner.on("--version", 0, "tt-cli 0.0.3", "");
        let err = leasing_backend(&runner)
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap_err();
        assert!(format!("{err:#}").contains("tt <semver>"), "{err:#}");
        // Refused before anything else ran: no dry run, no lease, no serve.
        assert_eq!(runner.log(), ["run: /fake/tt --version"]);

        // The real official version line proceeds.
        let runner = ScriptedRunner::happy();
        let ep = leasing_backend(&runner)
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap();
        assert_eq!(ep.model, "meta-llama/Llama-3.1-8B-Instruct");
    }

    #[test]
    fn start_serves_through_tt_and_reports_the_listed_model() {
        let runner = ScriptedRunner::happy();
        let b = backend(&runner);
        let ep = b
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap();
        assert_eq!(
            ep,
            Endpoint {
                base_url: "http://127.0.0.1:8003/v1".to_string(),
                model: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
                requires_key: false,
            }
        );
        let log = runner.log();
        assert!(
            log.contains(&"run: /fake/tt --json serve Llama-3.1-8B-Instruct --dry-run".to_string()),
            "{log:?}"
        );
        assert!(
            log.contains(
                &"run: /fake/tt --json serve Llama-3.1-8B-Instruct --port 8003".to_string()
            ),
            "catalog models get no --detach and the agent's port: {log:?}"
        );
        assert!(log.contains(&"get: http://127.0.0.1:8003/v1/models".to_string()));
        assert!(
            runner.pos("gozer").is_none(),
            "no gozer, no leasing: {log:?}"
        );
    }

    #[test]
    fn start_acquires_lease_before_serve_and_releases_on_failure() {
        let runner = ScriptedRunner::happy();
        runner.on("--port 8003", 1, "", "run.py: container failed to start");
        let b = leasing_backend(&runner);
        let err = b
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("container failed to start"),
            "{err:#}"
        );

        let acquire = runner.pos("gozer acquire").expect("a lease was requested");
        let serve = runner.pos("--port 8003").expect("tt serve ran");
        let stop = runner
            .pos("model stop Llama-3.1-8B-Instruct")
            .expect("stop ran");
        let release = runner
            .pos("gozer release lease-abc123")
            .expect("lease released");
        assert!(
            acquire < serve,
            "lease must be taken before tt serve: {:?}",
            runner.log()
        );
        assert!(
            serve < stop && stop < release,
            "stop, then release: {:?}",
            runner.log()
        );
        assert_eq!(
            b.held_lease(),
            None,
            "a failed serve must leave no lease held"
        );
    }

    #[test]
    fn a_successful_serve_keeps_its_lease_until_stop() {
        let runner = ScriptedRunner::happy();
        let b = leasing_backend(&runner);
        b.start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap();
        assert_eq!(b.held_lease().as_deref(), Some("lease-abc123"));
        assert!(runner.pos("gozer release").is_none(), "{:?}", runner.log());
        // The grant's environment reaches `tt serve`, and only `tt serve`.
        let envs = runner.envs.lock().unwrap().clone();
        let (_, serve_env) = envs
            .iter()
            .find(|(cmd, _)| cmd.contains("--port 8003"))
            .unwrap();
        assert_eq!(
            serve_env,
            &[(
                "TT_VISIBLE_DEVICES".to_string(),
                "0000:01:00.0,0000:02:00.0".to_string()
            )]
        );
    }

    #[test]
    fn a_readiness_timeout_stops_the_model_then_releases() {
        let runner = ScriptedRunner::happy();
        // `/v1/models` answers but never lists a model.
        *runner.bodies.lock().unwrap() = vec![r#"{"data":[]}"#.to_string()];
        let b = leasing_backend(&runner);
        let err = b
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap_err();
        assert!(format!("{err:#}").contains("not listed"), "{err:#}");
        let stop = runner.pos("model stop").unwrap();
        let release = runner.pos("gozer release").unwrap();
        assert!(stop < release);
        assert_eq!(b.held_lease(), None);
    }

    #[test]
    fn a_held_board_is_a_contention_and_nothing_is_served() {
        let runner = ScriptedRunner::happy();
        runner.on(
            "gozer acquire",
            12,
            r#"{"granted":false,"holder":"claude:bringup"}"#,
            "",
        );
        let err = leasing_backend(&runner)
            .start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap_err();
        assert!(err.chain().any(|c| c.is::<crate::gozer::Contention>()));
        assert!(format!("{err:#}").contains("claude:bringup"), "{err:#}");
        assert!(runner.pos("--port 8003").is_none(), "{:?}", runner.log());
    }

    #[test]
    fn an_unknown_model_is_refused_by_tt_before_any_lease() {
        let runner = ScriptedRunner::happy();
        runner.on("--dry-run", 2, TT_UNKNOWN_MODEL, "");
        let err = leasing_backend(&runner)
            .start_with_options("Nope-Model", false, &RunOptions::default())
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("Unknown model 'Nope-Model'."),
            "{err:#}"
        );
        assert!(runner.pos("gozer").is_none(), "{:?}", runner.log());
    }

    #[test]
    fn options_reach_tt_serve_and_the_requested_port_is_used() {
        let runner = ScriptedRunner::happy();
        let ep = backend(&runner)
            .start_with_options(
                "Llama-3.1-8B-Instruct",
                false,
                &opts(serde_json::json!({"port": 20005, "device": "p300x2", "offline": true})),
            )
            .unwrap();
        assert_eq!(ep.base_url, "http://127.0.0.1:20005/v1");
        let log = runner.log();
        assert!(
            log.contains(
                &"run: /fake/tt --json serve Llama-3.1-8B-Instruct --device p300x2 --port 20005 --offline"
                    .to_string()
            ),
            "{log:?}"
        );
        assert!(log.contains(&"get: http://127.0.0.1:20005/v1/models".to_string()));
    }

    #[test]
    fn a_bundle_is_detached_and_keeps_its_vllm_flags() {
        let runner = ScriptedRunner::happy();
        runner.on("--dry-run", 0, DRY_RUN_BUNDLE, "");
        backend(&runner)
            .start_with_options(
                "Adartras/qwen3.6-a3b-blackhole",
                false,
                &opts(serde_json::json!({"vllm_args":[{"flag":"max-model-len","value":"4096"}]})),
            )
            .unwrap();
        assert!(runner.log().contains(
            &"run: /fake/tt --json serve Adartras/qwen3.6-a3b-blackhole --detach --port 8003 -- --max-model-len 4096"
                .to_string()
        ), "{:?}", runner.log());
    }

    /// A Hugging Face id has a slash and is still a catalog model: `tt` resolves
    /// `meta-llama/Llama-3.1-8B-Instruct` to `Llama-3.1-8B-Instruct` (checked with `--dry-run`).
    /// The backend must classify from tt's dry run, so it adds no `--detach` (which `tt`
    /// refuses for a catalog model) and refuses vLLM pass-through flags.
    #[test]
    fn a_hugging_face_id_is_classified_by_tt_not_by_its_slash() {
        let runner = ScriptedRunner::happy();
        backend(&runner)
            .start("meta-llama/Llama-3.1-8B-Instruct")
            .unwrap();
        assert!(
            runner.log().contains(
                &"run: /fake/tt --json serve meta-llama/Llama-3.1-8B-Instruct --port 8003"
                    .to_string()
            ),
            "{:?}",
            runner.log()
        );

        let runner = ScriptedRunner::happy();
        let err = backend(&runner)
            .start_with_options(
                "meta-llama/Llama-3.1-8B-Instruct",
                false,
                &opts(serde_json::json!({"vllm_args":[{"flag":"max-model-len","value":"4096"}]})),
            )
            .unwrap_err();
        assert!(err.chain().any(|c| c.is::<OptionError>()), "{err:#}");
    }

    #[test]
    fn bundle_only_options_on_a_catalog_model_are_refused_before_anything_starts() {
        let runner = ScriptedRunner::happy();
        let err = leasing_backend(&runner)
            .start_with_options(
                "Llama-3.1-8B-Instruct",
                false,
                &opts(serde_json::json!({"vllm_args":[{"flag":"max-model-len","value":"4096"}]})),
            )
            .unwrap_err();
        let option_err = err
            .chain()
            .find_map(|c| c.downcast_ref::<OptionError>())
            .expect("an OptionError the route can answer 400 with");
        assert_eq!(option_err.field, "vllm_args");
        assert!(runner.pos("gozer").is_none() && runner.pos("--port").is_none());
    }

    #[test]
    fn a_non_server_workflow_is_refused_before_anything_runs() {
        let runner = ScriptedRunner::happy();
        let err = backend(&runner)
            .start_with_options(
                "Llama-3.1-8B-Instruct",
                false,
                &opts(serde_json::json!({"workflow": "benchmarks"})),
            )
            .unwrap_err();
        let option_err = err
            .chain()
            .find_map(|c| c.downcast_ref::<OptionError>())
            .unwrap();
        assert_eq!(option_err.field, "workflow");
        assert!(runner.log().is_empty(), "{:?}", runner.log());
    }

    #[test]
    fn start_forcing_is_start_with_empty_options() {
        let runner = ScriptedRunner::happy();
        backend(&runner).start("Llama-3.1-8B-Instruct").unwrap();
        assert!(runner
            .log()
            .contains(&"run: /fake/tt --json serve Llama-3.1-8B-Instruct --port 8003".to_string()));
    }

    // ---- stop / status ----

    #[test]
    fn stop_calls_tt_model_stop_with_the_served_name() {
        let runner = ScriptedRunner::happy();
        let b = leasing_backend(&runner);
        b.start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap();
        // The route passes the id it reported (the /v1/models id). `tt model stop` gets the
        // name `tt serve` was given.
        b.stop("meta-llama/Llama-3.1-8B-Instruct").unwrap();
        let stop = runner
            .pos("run: /fake/tt --json model stop Llama-3.1-8B-Instruct")
            .expect("tt model stop with the served name");
        let release = runner.pos("gozer release lease-abc123").unwrap();
        assert!(
            stop < release,
            "stop first, then release: {:?}",
            runner.log()
        );
        assert_eq!(b.held_lease(), None);
    }

    #[test]
    fn a_failed_stop_keeps_the_lease() {
        let runner = ScriptedRunner::happy();
        let b = leasing_backend(&runner);
        b.start_with_options("Llama-3.1-8B-Instruct", false, &RunOptions::default())
            .unwrap();
        runner.on("model stop", 1, "", "docker: cannot connect");
        assert!(b.stop("x").is_err());
        assert!(runner.pos("gozer release").is_none(), "{:?}", runner.log());
        assert_eq!(b.held_lease().as_deref(), Some("lease-abc123"));
    }

    #[test]
    fn stop_after_a_restart_uses_the_reported_id() {
        let runner = ScriptedRunner::happy();
        backend(&runner)
            .stop("meta-llama/Llama-3.1-8B-Instruct")
            .unwrap();
        assert!(runner
            .pos("model stop meta-llama/Llama-3.1-8B-Instruct")
            .is_some());
        // Nothing known at all: nothing to stop.
        let runner = ScriptedRunner::happy();
        backend(&runner).stop("").unwrap();
        assert!(runner.pos("model stop").is_none());
    }

    #[test]
    fn status_uses_model_ps() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = backend(&runner);
        assert_eq!(
            b.status().unwrap(),
            ServingStatus::Serving("meta-llama/Llama-3.1-8B-Instruct".to_string())
        );
        assert!(runner.pos("run: /fake/tt --json model ps").is_some());

        let idle = ScriptedRunner::happy();
        assert_eq!(backend(&idle).status().unwrap(), ServingStatus::Idle);

        // A running row on another port is not this backend's serve.
        let other = ScriptedRunner::happy();
        other.on("model ps", 0, &PS_ONE_RUNNING.replace("8003", "8004"), "");
        assert_eq!(backend(&other).status().unwrap(), ServingStatus::Idle);

        // `tt model ps` failing is an error (the route then keeps its stored status).
        let broken = ScriptedRunner::happy();
        broken.on("model ps", 1, "", "docker not running");
        assert!(backend(&broken).status().is_err());
    }

    #[test]
    fn status_counts_a_starting_row_on_our_port_as_serving() {
        let runner = ScriptedRunner::happy();
        runner.on(
            "model ps",
            0,
            r#"{"served":[{"name":"Llama-3.1-8B-Instruct","port":8003,"status":"running",
                "health":"starting","served_id":null}],"probed":true}"#,
            "",
        );
        assert_eq!(
            backend(&runner).status().unwrap(),
            ServingStatus::Serving("Llama-3.1-8B-Instruct".to_string())
        );
    }

    #[test]
    fn list_models_reads_tt_model_list() {
        let runner = ScriptedRunner::happy();
        runner.on(
            "model list",
            0,
            r#"{"models":[{"name":"Llama-3.1-8B-Instruct","engines":["vLLM"],"hardware":["p300x2"],"cached":true}]}"#,
            "",
        );
        let got = backend(&runner).list_models().unwrap();
        assert_eq!(got.models.len(), 1);
        assert!(runner.pos("run: /fake/tt --json model list").is_some());
    }

    #[test]
    fn adopt_lease_needs_gozer_and_an_empty_slot() {
        let runner = ScriptedRunner::happy();
        assert!(!backend(&runner).adopt_lease("l1", "tt-station:8003:m"));
        let b = leasing_backend(&runner);
        assert!(b.adopt_lease("l1", "tt-station:8003:m"));
        assert!(!b.adopt_lease("l2", "tt-station:8003:m"));
        assert_eq!(b.held_lease().as_deref(), Some("l1"));
    }

    // ---- fix round 1 ----

    #[test]
    fn an_unsafe_model_name_is_refused_before_anything_runs() {
        for bad in ["--port=1", "-x", "a:b", "a\nb", ""] {
            let runner = ScriptedRunner::happy();
            let err = leasing_backend(&runner)
                .start_with_options(bad, false, &RunOptions::default())
                .unwrap_err();
            let option_err = err
                .chain()
                .find_map(|c| c.downcast_ref::<OptionError>())
                .expect("an OptionError naming the model");
            assert_eq!(option_err.field, "model", "{bad:?}");
            assert!(runner.log().is_empty(), "{bad:?}: {:?}", runner.log());
        }
    }

    /// A missing `tt` is a spawn error, not an exit code. It must say how to install the
    /// official CLI, and nothing may run after the failed `--version`.
    #[test]
    fn a_missing_tt_says_how_to_install_it() {
        let runner = ScriptedRunner::happy();
        runner
            .spawn_failures
            .lock()
            .unwrap()
            .push("--version".to_string());
        let err = leasing_backend(&runner)
            .start("Llama-3.1-8B-Instruct")
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("could not run `/fake/tt`"), "{msg}");
        assert!(msg.contains("uv tool install tenstorrent"), "{msg}");
        assert_eq!(runner.log(), ["run: /fake/tt --version"]);
    }

    #[test]
    fn a_failing_version_command_is_refused_even_with_the_right_text() {
        let runner = ScriptedRunner::happy();
        runner.on("--version", 1, "tt 1.0.1", "something broke");
        let err = leasing_backend(&runner)
            .start("Llama-3.1-8B-Instruct")
            .unwrap_err();
        assert!(format!("{err:#}").contains("tt <semver>"), "{err:#}");
        assert_eq!(runner.log(), ["run: /fake/tt --version"]);
    }

    /// After an agent restart the adopted lease is held while `served` is empty. The next
    /// `/run` must stop the old server (found with `tt model ps`) before it releases that
    /// lease, because the release resets the chips.
    #[test]
    fn an_adopted_lease_is_released_only_after_its_server_is_stopped() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = leasing_backend(&runner);
        assert!(b.adopt_lease("lease-old", "tt-station:8003:Llama-3.1-8B-Instruct"));
        b.start("Llama-3.1-8B-Instruct").unwrap();
        let stop = runner
            .pos("model stop Llama-3.1-8B-Instruct")
            .expect("the old server was stopped");
        let release = runner
            .pos("gozer release lease-old")
            .expect("the adopted lease was released");
        assert!(stop < release, "stop before release: {:?}", runner.log());
        assert!(release < runner.pos("gozer acquire").unwrap());
    }

    #[test]
    fn an_adopted_lease_is_kept_when_its_server_cannot_be_identified() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 1, "", "docker: cannot connect");
        let b = leasing_backend(&runner);
        assert!(b.adopt_lease("lease-old", "tt-station:8003:Llama-3.1-8B-Instruct"));
        let err = b.start("Llama-3.1-8B-Instruct").unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to release the lease"),
            "{err:#}"
        );
        assert!(runner.pos("gozer").is_none(), "{:?}", runner.log());
        assert!(runner.pos("--port 8003").is_none(), "nothing started");
        assert_eq!(b.held_lease().as_deref(), Some("lease-old"));
    }

    #[test]
    fn an_adopted_lease_with_nothing_running_is_released() {
        let runner = ScriptedRunner::happy(); // `tt model ps` lists nothing
        let b = leasing_backend(&runner);
        assert!(b.adopt_lease("lease-old", "tt-station:8003:m"));
        b.stop("").unwrap();
        assert!(runner.pos("model stop").is_none());
        assert!(runner.pos("gozer release lease-old").is_some());
        assert_eq!(b.held_lease(), None);
    }

    /// A `/stop` from another thread while `start` is still polling: `start` notices within one
    /// interval, stops the model, and the lease guard releases after the stop.
    #[test]
    fn a_stop_during_bring_up_aborts_the_start_and_releases_after_stopping() {
        let runner = ScriptedRunner::happy();
        runner.healthy.store(false, Ordering::SeqCst);
        let b = TtBackend::new(
            TtConfig {
                // About 4 s of polling: far longer than the stop below takes to land, and short
                // enough that a regression fails the test instead of hanging it.
                health_poll_attempts: 2_000,
                health_poll_interval: Duration::from_millis(2),
                ..config()
            },
            Box::new(runner.clone()),
        )
        .with_gozer(Some(capability()));
        let result = std::thread::scope(|scope| {
            let starter = scope.spawn(|| b.start("Llama-3.1-8B-Instruct"));
            // Wait until polling has begun, then stop.
            for _ in 0..5_000 {
                if runner.pos("health:").is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            b.stop("").unwrap();
            starter.join().unwrap()
        });
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("aborted"), "{err:#}");
        let stop = runner.pos("model stop Llama-3.1-8B-Instruct").unwrap();
        let release = runner.pos("gozer release lease-abc123").unwrap();
        assert!(stop < release, "{:?}", runner.log());
        assert_eq!(b.held_lease(), None);
        assert_eq!(b.served(), None);
    }

    /// The race after the last poll: a `/stop` that lands while the readiness GET is answering
    /// (after polling succeeded, before `served` is recorded). The start must report "aborted",
    /// not hand back an endpoint, and must leave nothing recorded or leased.
    #[test]
    fn a_stop_between_poll_success_and_handover_aborts_the_start() {
        let runner = ScriptedRunner::happy();
        let b = leasing_backend(&runner);
        *runner.cancel_on_get.lock().unwrap() = Some(b.cancel.clone());
        let err = b.start("Llama-3.1-8B-Instruct").unwrap_err();
        assert!(format!("{err:#}").contains("aborted"), "{err:#}");
        let stop = runner.pos("model stop Llama-3.1-8B-Instruct").unwrap();
        let release = runner.pos("gozer release lease-abc123").unwrap();
        assert!(
            runner.pos("get:").unwrap() < stop && stop < release,
            "{:?}",
            runner.log()
        );
        assert_eq!(b.held_lease(), None);
        assert_eq!(b.served(), None);
    }

    // ---- Task 4b: /serving and restart ----

    fn backend_on_20010(runner: &ScriptedRunner) -> TtBackend {
        TtBackend::new(
            TtConfig {
                service_port: 20010,
                serving_host: "qb2-lab.local".to_string(),
                ..config()
            },
            Box::new(runner.clone()),
        )
    }

    #[test]
    fn serving_entries_come_from_tt_model_ps() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let entries = backend_on_20010(&runner).serving_entries().unwrap();
        assert_eq!(
            entries,
            vec![ServingEntry {
                model: "Qwen/Qwen3.8-27B".to_string(),
                base_url: "http://qb2-lab.local:20010/v1".to_string(),
                host_port: 20010,
                container: "tt-model-qwen3.8-27b-p300x2-batch32".to_string(),
                source: "agent".to_string(),
            }]
        );
        // Another port: listed, but not ours.
        let entries = backend(&runner).serving_entries().unwrap();
        assert_eq!(entries[0].source, "external");
        // A row still loading (no served_id) is not listed.
        let loading = ScriptedRunner::happy();
        loading.on(
            "model ps",
            0,
            &PS_LIVE_BUNDLE.replace(r#""Qwen/Qwen3.8-27B""#, "null"),
            "",
        );
        assert!(backend_on_20010(&loading)
            .serving_entries()
            .unwrap()
            .is_empty());
        // `tt model ps` failing: None, so the route uses discovery alone.
        let broken = ScriptedRunner::happy();
        broken.on("model ps", 1, "", "docker: cannot connect");
        assert_eq!(backend_on_20010(&broken).serving_entries(), None);
    }

    #[test]
    fn discover_own_serve_adopts_the_row_on_our_port_and_stop_uses_its_name() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = backend_on_20010(&runner);
        let ep = b.discover_own_serve().expect("the row on 20010 is ours");
        assert_eq!(ep.base_url, "http://qb2-lab.local:20010/v1");
        assert_eq!(ep.model, "Qwen/Qwen3.8-27B");
        assert!(!ep.requires_key);
        // Already recorded: asking again reports nothing new.
        assert_eq!(b.discover_own_serve(), None);
        // The route then passes the /v1 id; `tt model stop` must get the bundle name.
        b.stop("Qwen/Qwen3.8-27B").unwrap();
        assert!(
            runner
                .pos("run: /fake/tt --json model stop changh95/qwen3.8-27b-p300x2")
                .is_some(),
            "{:?}",
            runner.log()
        );

        // A row on another port is not ours; a loading row is not serving yet.
        assert_eq!(backend(&runner).discover_own_serve(), None);
        let loading = ScriptedRunner::happy();
        loading.on(
            "model ps",
            0,
            &PS_LIVE_BUNDLE.replace(r#""Qwen/Qwen3.8-27B""#, "null"),
            "",
        );
        assert_eq!(backend_on_20010(&loading).discover_own_serve(), None);
    }

    #[test]
    fn discover_own_serve_reports_nothing_while_a_start_is_in_flight() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = backend_on_20010(&runner);
        b.starting.store(true, Ordering::SeqCst);
        assert_eq!(b.discover_own_serve(), None);
        b.starting.store(false, Ordering::SeqCst);
        assert!(b.discover_own_serve().is_some());
    }

    // ---- Task 4b fix round 1: the `ps` cache and the in-flight start guard ----

    fn cached_backend(
        runner: &ScriptedRunner,
        ttl_ms: u64,
        stale_ms: u64,
        timeout_ms: u64,
    ) -> TtBackend {
        TtBackend::new(
            TtConfig {
                ps_cache_ttl: Duration::from_millis(ttl_ms),
                ps_stale_limit: Duration::from_millis(stale_ms),
                ps_timeout: Duration::from_millis(timeout_ms),
                ..config()
            },
            Box::new(runner.clone()),
        )
    }

    /// Ten concurrent idle polls start one `tt model ps`, not ten.
    #[test]
    fn concurrent_reads_share_one_ps_run() {
        let runner = ScriptedRunner::happy();
        // A row on another port: nothing is adopted, so every caller really reads `ps`.
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        *runner.ps_delay.lock().unwrap() = Duration::from_millis(20);
        let b = cached_backend(&runner, 10_000, 10_000, 1_000);
        let barrier = std::sync::Barrier::new(10);
        std::thread::scope(|scope| {
            for _ in 0..10 {
                scope.spawn(|| {
                    barrier.wait();
                    assert_eq!(b.discover_own_serve(), None);
                });
            }
        });
        assert_eq!(runner.ps_runs(), 1, "{:?}", runner.log());
    }

    #[test]
    fn a_read_within_the_ttl_reuses_ps_and_one_after_it_runs_again() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = cached_backend(&runner, 30, 10_000, 1_000);
        b.serving_entries().unwrap();
        b.serving_entries().unwrap();
        assert_eq!(runner.ps_runs(), 1);
        std::thread::sleep(Duration::from_millis(40));
        b.serving_entries().unwrap();
        assert_eq!(runner.ps_runs(), 2);
    }

    /// A hung `tt model ps` is cut off at the timeout, and the poll reports nothing.
    #[test]
    fn a_hung_ps_returns_within_the_timeout() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, ""); // would be adopted if it answered
        runner.hang_ps.store(true, Ordering::SeqCst);
        let b = cached_backend(&runner, 2_500, 10_000, 30);
        let started = std::time::Instant::now();
        assert_eq!(b.discover_own_serve(), None);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(b.serving_entries(), None);
    }

    #[test]
    fn a_failed_ps_falls_back_to_recent_rows_only() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = cached_backend(&runner, 0, 30, 1_000);
        assert_eq!(b.serving_entries().unwrap().len(), 1);
        runner.on("model ps", 1, "", "docker: cannot connect");
        assert_eq!(
            b.serving_entries().map(|e| e.len()),
            Some(1),
            "rows younger than the stale limit are used"
        );
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(b.serving_entries(), None, "older rows are not trusted");
    }

    /// The paths that decide what to stop before a lease release read `ps` fresh, even with a
    /// cached value in hand.
    #[test]
    fn stop_and_start_read_ps_fresh_despite_the_cache() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = cached_backend(&runner, 10_000, 10_000, 1_000).with_gozer(Some(capability()));
        b.serving_entries().unwrap(); // fills the cache
        assert_eq!(runner.ps_runs(), 1);
        assert!(b.adopt_lease("lease-old", "tt-station:8003:Llama-3.1-8B-Instruct"));
        b.stop("").unwrap();
        assert_eq!(
            runner.ps_runs(),
            2,
            "stop identified the server with a fresh ps"
        );

        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = cached_backend(&runner, 10_000, 10_000, 1_000).with_gozer(Some(capability()));
        b.serving_entries().unwrap();
        assert!(b.adopt_lease("lease-old", "tt-station:8003:Llama-3.1-8B-Instruct"));
        b.start("Llama-3.1-8B-Instruct").unwrap();
        assert_eq!(
            runner.ps_runs(),
            2,
            "start's swap read ps fresh: {:?}",
            runner.log()
        );
        assert!(runner.pos("model stop").unwrap() < runner.pos("gozer release lease-old").unwrap());
    }

    /// While a start is in flight, an idle poll must not record the loading server as an
    /// already-running serve. The poll runs from inside `start` (in the readiness GET), with a
    /// `ps` row on the port.
    #[test]
    fn discover_own_serve_reports_nothing_during_a_real_start() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = Arc::new(backend(&runner));
        let seen: Arc<Mutex<Vec<Option<Endpoint>>>> = Arc::default();
        {
            let b = b.clone();
            let seen = seen.clone();
            *runner.on_get.lock().unwrap() = Some(Arc::new(move || {
                seen.lock().unwrap().push(b.discover_own_serve());
            }));
        }
        b.start("Llama-3.1-8B-Instruct").unwrap();
        *runner.on_get.lock().unwrap() = None; // break the Arc cycle
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty(), "the hook ran during start");
        assert!(seen.iter().all(Option::is_none), "{seen:?}");
    }

    // ---- Task 4b fix round 2 ----

    /// Five concurrent idle polls against a hung `tt`: one run, about one timeout in total.
    #[test]
    fn concurrent_callers_share_one_failed_run() {
        let runner = ScriptedRunner::happy();
        runner.hang_ps.store(true, Ordering::SeqCst);
        let b = cached_backend(&runner, 10_000, 10_000, 100);
        let barrier = std::sync::Barrier::new(5);
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..5 {
                scope.spawn(|| {
                    barrier.wait();
                    assert_eq!(b.serving_entries(), None);
                });
            }
        });
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(runner.ps_runs(), 1, "{:?}", runner.log());
    }

    #[test]
    fn a_failed_run_is_reused_within_the_ttl_and_retried_after() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 1, "", "docker: cannot connect");
        let b = cached_backend(&runner, 40, 0, 1_000);
        assert_eq!(b.serving_entries(), None);
        assert_eq!(b.serving_entries(), None);
        assert_eq!(runner.ps_runs(), 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(b.serving_entries(), None);
        assert_eq!(runner.ps_runs(), 2);
    }

    /// Fresh reads queue on the same lock but do not compound one timeout per waiter: a waiter
    /// reuses a run that started after it arrived.
    #[test]
    fn concurrent_fresh_reads_do_not_compound_timeouts() {
        let runner = ScriptedRunner::happy();
        runner.hang_ps.store(true, Ordering::SeqCst);
        let b = cached_backend(&runner, 10_000, 10_000, 100);
        let barrier = std::sync::Barrier::new(5);
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..5 {
                scope.spawn(|| {
                    barrier.wait();
                    assert!(b.ps(PsRead::Fresh).is_err());
                });
            }
        });
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "took {:?}",
            started.elapsed()
        );
        assert!(runner.ps_runs() <= 2, "{:?}", runner.log());
    }

    /// The row `tt model ps` lists on port 8003 once the server there changed.
    const PS_OTHER_ON_8003: &str = r#"{"served":[{"name":"Qwen3-8B","port":8003,
        "status":"running","health":"healthy","served_id":"Qwen/Qwen3-8B"}],"probed":true}"#;

    /// Discovery adopts a serve from a cached read, with a lease held (adopted after a restart).
    fn discovered_with_lease(runner: &ScriptedRunner) -> TtBackend {
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = cached_backend(runner, 10_000, 10_000, 1_000).with_gozer(Some(capability()));
        assert!(b.adopt_lease("lease-old", "tt-station:8003:Llama-3.1-8B-Instruct"));
        assert!(b.discover_own_serve().is_some());
        assert_eq!(b.served().unwrap().0, "Llama-3.1-8B-Instruct");
        b
    }

    #[test]
    fn a_discovered_name_is_confirmed_fresh_and_the_current_row_is_stopped() {
        let runner = ScriptedRunner::happy();
        let b = discovered_with_lease(&runner);
        runner.on("model ps", 0, PS_OTHER_ON_8003, ""); // the cache still says Llama
        b.stop("").unwrap();
        let stop = runner
            .pos("model stop Qwen3-8B")
            .expect("the row now on our port was stopped");
        assert!(
            runner.pos("model stop Llama-3.1-8B-Instruct").is_none(),
            "{:?}",
            runner.log()
        );
        assert!(stop < runner.pos("gozer release lease-old").unwrap());
    }

    #[test]
    fn a_discovered_serve_that_is_gone_is_not_stopped_and_the_lease_is_released() {
        let runner = ScriptedRunner::happy();
        let b = discovered_with_lease(&runner);
        runner.on("model ps", 0, PS_EMPTY, "");
        b.stop("").unwrap();
        assert!(runner.pos("model stop").is_none(), "{:?}", runner.log());
        assert!(runner.pos("gozer release lease-old").is_some());
        assert_eq!(b.held_lease(), None);
    }

    #[test]
    fn a_discovered_serve_now_on_another_port_is_refused_and_the_lease_kept() {
        let runner = ScriptedRunner::happy();
        let b = discovered_with_lease(&runner);
        runner.on("model ps", 0, &PS_ONE_RUNNING.replace("8003", "9001"), "");
        let err = b.stop("").unwrap_err();
        assert!(format!("{err:#}").contains("refusing"), "{err:#}");
        assert!(runner.pos("model stop").is_none(), "{:?}", runner.log());
        assert!(runner.pos("gozer release").is_none());
        assert_eq!(b.held_lease().as_deref(), Some("lease-old"));
    }

    /// A start that begins while discovery's `ps` runs wins: discovery records nothing.
    #[test]
    fn discovery_rechecks_for_a_start_under_the_handover_lock() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = Arc::new(backend(&runner));
        {
            let b = b.clone();
            *runner.on_ps.lock().unwrap() =
                Some(Arc::new(move || b.starting.store(true, Ordering::SeqCst)));
        }
        assert_eq!(b.discover_own_serve(), None);
        *runner.on_ps.lock().unwrap() = None; // break the Arc cycle
        assert_eq!(b.served(), None);
    }

    #[test]
    fn a_poisoned_ps_cache_recovers() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        let b = cached_backend(&runner, 0, 0, 1_000);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = b.ps_cache.lock().unwrap();
            panic!("poison the cache");
        }));
        assert!(b.ps_cache.is_poisoned());
        assert_eq!(b.serving_entries().map(|e| e.len()), Some(1));
    }

    // ---- Task 4b fix round 3 ----

    /// Discovery with gozer off (no lease), from a cached read.
    fn discovered_without_lease(runner: &ScriptedRunner) -> TtBackend {
        runner.on("model ps", 0, PS_ONE_RUNNING, "");
        let b = cached_backend(runner, 10_000, 10_000, 1_000);
        assert!(b.discover_own_serve().is_some());
        b
    }

    #[test]
    fn without_a_lease_a_discovered_serve_on_another_port_is_not_stopped() {
        let runner = ScriptedRunner::happy();
        let b = discovered_without_lease(&runner);
        runner.on("model ps", 0, &PS_ONE_RUNNING.replace("8003", "9001"), "");
        let err = b.stop("").unwrap_err();
        assert!(format!("{err:#}").contains("refusing"), "{err:#}");
        assert!(runner.pos("model stop").is_none(), "{:?}", runner.log());
    }

    #[test]
    fn without_a_lease_the_row_now_on_our_port_is_stopped() {
        let runner = ScriptedRunner::happy();
        let b = discovered_without_lease(&runner);
        runner.on("model ps", 0, PS_OTHER_ON_8003, "");
        b.stop("").unwrap();
        assert!(
            runner.pos("model stop Qwen3-8B").is_some(),
            "{:?}",
            runner.log()
        );
        assert!(runner.pos("model stop Llama-3.1-8B-Instruct").is_none());
    }

    #[test]
    fn without_a_lease_a_discovered_serve_that_is_gone_is_not_stopped() {
        let runner = ScriptedRunner::happy();
        let b = discovered_without_lease(&runner);
        runner.on("model ps", 0, PS_EMPTY, "");
        b.stop("").unwrap();
        assert!(runner.pos("model stop").is_none(), "{:?}", runner.log());
        assert_eq!(b.served(), None);
    }

    /// With a lease and nothing running, the confirmation's fresh read is the only one: no
    /// second `ps` for the identification, and a failing second read cannot block the release.
    #[test]
    fn a_confirmed_empty_port_needs_one_fresh_read() {
        let runner = ScriptedRunner::happy();
        let b = discovered_with_lease(&runner);
        assert_eq!(runner.ps_runs(), 1, "the discovery read");
        runner.on("model ps", 0, PS_EMPTY, "");
        b.stop("").unwrap();
        assert_eq!(
            runner.ps_runs(),
            2,
            "exactly one fresh read: {:?}",
            runner.log()
        );
        assert!(runner.pos("gozer release lease-old").is_some());
    }

    /// A fresh reader that arrives while a cached run is already in flight must not reuse that
    /// run (it began before the reader asked); it runs `ps` again.
    #[test]
    fn a_fresh_reader_does_not_reuse_a_run_that_started_before_it_arrived() {
        let runner = ScriptedRunner::happy();
        runner.on("model ps", 0, PS_LIVE_BUNDLE, "");
        *runner.ps_delay.lock().unwrap() = Duration::from_millis(80);
        let b = cached_backend(&runner, 10_000, 10_000, 1_000);
        std::thread::scope(|scope| {
            scope.spawn(|| b.ps(PsRead::Cached).unwrap());
            std::thread::sleep(Duration::from_millis(20)); // the cached run is in flight now
            scope.spawn(|| b.ps(PsRead::Fresh).unwrap());
        });
        assert_eq!(runner.ps_runs(), 2, "{:?}", runner.log());
    }

    #[test]
    fn backend_name_is_tt() {
        assert_eq!(backend(&ScriptedRunner::happy()).backend_name(), "tt");
    }
}
