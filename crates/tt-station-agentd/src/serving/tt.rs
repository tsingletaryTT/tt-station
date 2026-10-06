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
//! 1. Validate the options again (the route already did) and refuse a non-`server` workflow.
//! 2. Check that the configured binary is the official CLI: `--version` must print
//!    `tt <semver>` (see `libttstation::official_tt`).
//! 3. Ask `tt --json serve <model> --dry-run` how it would serve the model. That says whether
//!    the model is a catalog model or a tt-model bundle, which decides which options are
//!    allowed, and it rejects an unknown model before any chips are leased.
//! 4. Stop whatever this backend is already serving and release its lease.
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
//! ## What has been checked so far
//!
//! Steps 2, 3 and the argv shapes were checked against `tt 1.0.1` with `--dry-run` on
//! 2026-10-05. A real `tt serve` under a gozer lease has not been run through this backend yet;
//! that is Task 4 of the Linux tray plan. Open points it must settle:
//!
//! * Whether `TT_VISIBLE_DEVICES` from the gozer grant (passed in `tt serve`'s environment)
//!   reaches the container. With the default lease (`gozer::DEFAULT_LEASE_CHIPS`, every chip)
//!   `tt`'s own device auto-detection describes the leased set anyway.
//! * Whether a bundle served by `tt-model` needs an API key. This backend reports
//!   `requires_key: false`; `tt serve` passes `--no-auth` to `run.py` for catalog models.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use libttstation::model::{Endpoint, ModelInfo, ModelsResponse, ServingStatus};
use libttstation::official_tt;
use libttstation::run_options::{serve_argv_for, OptionError, RunOptions, ServeTarget, Workflow};
use serde::Deserialize;

use super::docker::{CapturedOutput, CommandRunner};
use super::ServingBackend;

/// Default number of `/v1/models` polls before `start` gives up. With
/// [`DEFAULT_HEALTH_POLL_INTERVAL`] that is about 40 minutes, the same ceiling `RunPyBackend`
/// uses: a large model's first serve downloads weights and compiles before it answers.
pub const DEFAULT_HEALTH_POLL_ATTEMPTS: u32 = 1200;

/// Delay between `/v1/models` polls. See [`DEFAULT_HEALTH_POLL_ATTEMPTS`].
pub const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
}

impl Default for TtConfig {
    fn default() -> Self {
        TtConfig {
            tt_bin: official_tt::OFFICIAL_TT_DEFAULT_BIN.to_string(),
            serving_host: "127.0.0.1".to_string(),
            service_port: 8000,
            health_poll_attempts: DEFAULT_HEALTH_POLL_ATTEMPTS,
            health_poll_interval: DEFAULT_HEALTH_POLL_INTERVAL,
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
    /// What this backend started: the model name it passed to `tt serve` (the name `tt model
    /// stop` needs, which can differ from the `/v1/models` id) and the port it served on.
    served: Mutex<Option<(String, u16)>>,
    /// Set by `stop` so an in-flight `start` stops polling; cleared when `start` begins.
    /// Same purpose as `RunPyBackend`'s flag: until `start` returns, the agent's status is
    /// still idle, so `/stop` cannot be gated on "a model is serving".
    cancel: AtomicBool,
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
            cancel: AtomicBool::new(false),
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
    fn stop_inner(&self, hint: &str) -> Result<()> {
        let recorded = self
            .served
            .lock()
            .expect("served mutex poisoned")
            .as_ref()
            .map(|(name, _)| name.clone());
        let name = recorded.or_else(|| (!hint.is_empty()).then(|| hint.to_string()));
        if let Some(name) = name {
            self.tt_model_stop(&name)?;
        }
        *self.served.lock().expect("served mutex poisoned") = None;
        self.release_lease()
    }

    /// `tt --json model ps`, parsed.
    fn ps(&self) -> Result<Vec<ServedModel>> {
        let captured = self.run_tt(&["--json", "model", "ps"], &[])?;
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

        // (1) Options. The route validated them already; checking again costs nothing and
        // keeps this backend safe for any other caller.
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
        // is set on the child only.
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

        // Success. Record what was started, then move the lease into the backend so it
        // outlives this call. This is the only exit that does not release.
        *self.served.lock().expect("served mutex poisoned") = Some((model.to_string(), port));
        if let Some(guard) = lease {
            *self.lease.lock().expect("lease mutex poisoned") = Some(guard.into_lease_id());
            // A `/stop` that arrived after the poll succeeded may have read no lease and
            // released nothing. It has already stopped the server, so give the lease back now.
            if self.cancel.load(Ordering::SeqCst) {
                if let Err(err) = self.release_lease() {
                    eprintln!(
                        "tt-station-agentd: releasing the lease of a serve that a concurrent \
                         stop raced failed: {err:#}"
                    );
                }
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
        self.cancel.store(true, Ordering::SeqCst);
        self.stop_inner(model)
    }

    /// Take ownership of a lease a previous agent process left behind, so the next `stop`
    /// releases (and resets) it. Refuses when leasing is off or a lease is already held.
    fn adopt_lease(&self, lease_id: &str, _who: &str) -> bool {
        if self.gozer.is_none() {
            return false;
        }
        let mut held = self.lease.lock().expect("lease mutex poisoned");
        if held.is_some() {
            return false;
        }
        *held = Some(lease_id.to_string());
        true
    }

    /// What is serving on this backend's port, according to `tt --json model ps`, which is
    /// the source of truth for this backend. A running row on the port counts as serving
    /// whatever its `health`: `tt model ps` probes with a 1.5 s timeout, and a busy server that
    /// misses it shows `starting`, which must not make the agent declare the model gone.
    ///
    /// The port is the one this backend last served on, else the configured `service_port`.
    /// The reported id is the row's `served_id`, else the recorded model name, else the row's
    /// `name`. Because this reads the box through `tt`, it also sees a serve
    /// started before an agent restart. `reconciled_status` uses the default, which calls this.
    fn status(&self) -> Result<ServingStatus> {
        let recorded = self.served.lock().expect("served mutex poisoned").clone();
        let port = recorded
            .as_ref()
            .map(|(_, p)| *p)
            .unwrap_or(self.config.service_port);
        let rows = self.ps()?;
        let row = rows
            .iter()
            .find(|r| r.port == Some(port) && r.status.as_deref() == Some("running"));
        Ok(match row {
            Some(r) => ServingStatus::Serving(
                r.served_id
                    .clone()
                    .or_else(|| recorded.map(|(name, _)| name))
                    .unwrap_or_else(|| r.name.clone()),
            ),
            None => ServingStatus::Idle,
        })
    }

    #[cfg(any(test, feature = "test-hooks"))]
    fn gozer_capability(&self) -> Option<&crate::gozer::Capability> {
        self.gozer.as_ref()
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

    /// A non-empty `tt --json model ps`. NOT captured live: no model was served on this box
    /// while this was written (a real serve is Task 4). It is built field for field from
    /// tt-cli's `ServedModel` dataclass (`src/tenstorrent/models/served.py`), which documents
    /// its field order as the `--json` contract. Replace it with a captured one in Task 4.
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
    }

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

        fn http_get(&self, url: &str) -> Result<String> {
            self.log.lock().unwrap().push(format!("get: {url}"));
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

    #[test]
    fn backend_name_is_tt() {
        assert_eq!(backend(&ScriptedRunner::happy()).backend_name(), "tt");
    }
}
