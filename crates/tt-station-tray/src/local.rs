//! This machine, driven through the official `tt` CLI (`tenstorrent/tt-cli`).
//!
//! Everything goes through `tt`: `tt --json model list`, `tt --json model ps`,
//! `tt --json serve`, `tt --json model stop`, and `tt model logs <name> --tail N` for logs. The
//! tray does not talk to Docker or run.py.
//!
//! **Which server the tray acts on.** `tt model ps` can list servers the tray did not start (on a
//! shared box, or TT-Studio's). `stop` and `logs` act only on the server [`LocalStation::state`]
//! reports as serving, the one the UI shows, and never on "everything running".
//!
//! **Where the default binary comes from.** [`LocalStation::from_env`] uses
//! `official_tt::resolve_bin`, not the `ToolNames` of `tt-station console`. `ToolNames` names the
//! tools of a box this program operates over SSH or systemd. The local station needs the
//! official `tt` on this machine, and `resolve_bin` is the shared lookup that also searches
//! `~/.local/bin` (where `uv tool install tenstorrent` puts it), which a short desktop `PATH`
//! lacks.
//!
//! **Process rules.** `tt` is started with [`tokio::process::Command`] and an argument vector.
//! No shell string is ever built. Model names and options are validated before anything is
//! spawned, so a bad request never starts a process. The `tt serve` argv comes from
//! `libttstation::run_options::serve_argv_for`, the function the agent and the run sheet's
//! command preview also use, so all three show the same command.
//!
//! **Official `tt` only.** Before the first real use, `tt --version` must print `tt <semver>`
//! (`libttstation::official_tt::parse_version`). A stale pre-rename copy of tt-station's own CLI
//! has shadowed the official one before; it is refused and never run.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use libttstation::official_tt::{not_official_message, parse_version, resolve_bin};
use libttstation::run_options::{
    run_schema, serve_argv_for, validate_model_name, RunOptions, RunSchema, ServeTarget, Workflow,
};
use serde::Deserialize;
use tokio::process::Command;

use crate::perf::{self, PerfRef};
use crate::station::{fits, ModelRow, ServingNow, StationId, StationKind, StationState};

/// Port used for `tt serve --port` when the options do not set one. It matches the default the
/// agent's `tt` backend uses for the same command.
pub const DEFAULT_SERVE_PORT: u16 = 20000;

/// How long [`LocalStation::perf_reference`] waits for `tt --version` plus the
/// `tt serve --dry-run` that names the tt-inference-server checkout. It normally takes under a
/// second; past this the lookup falls back to scanning the tools directory.
pub const PERF_DRY_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long a failed reference-table lookup (no file anywhere) is remembered, so a machine
/// without tt-inference-server does not run a dry run on every Models page load.
pub const PERF_MISS_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// What [`LocalStation::perf_reference`] remembers between calls.
enum PerfCache {
    /// Not looked up yet, or the remembered miss has expired.
    Unknown,
    /// A table was found; kept for the life of the station.
    Found(Arc<PerfRef>),
    /// No table was found at this instant; trusted until [`LocalStation::perf_miss_ttl`] passes.
    Missing(std::time::Instant),
}

/// What a finished `tt` invocation left behind.
struct Captured {
    success: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// This machine as a station.
pub struct LocalStation {
    tt_bin: PathBuf,
    name: String,
    default_port: u16,
    /// Mesh label (`p300x2`). Seeded by [`LocalStation::with_mesh`] or learned from the `device`
    /// field of `tt --json model list` the first time [`LocalStation::models`] runs, so that
    /// polling [`LocalStation::state`] does not have to list the whole catalog each time.
    mesh: Mutex<Option<String>>,
    /// Where the official CLI installs tt-inference-server checkouts
    /// (`~/.local/share/tenstorrent/tools`), searched by [`LocalStation::perf_reference`] when
    /// the dry run does not name a checkout. `None` when `HOME` is unset.
    tools_dir: Option<PathBuf>,
    /// The reference performance lookup's result. A found table is kept for the life of the
    /// station. A miss is kept for `perf_miss_ttl`, then retried, so a checkout installed
    /// while the tray runs is picked up within a few minutes.
    perf: Mutex<PerfCache>,
    /// Limit on the dry run inside [`LocalStation::perf_reference`] ([`PERF_DRY_RUN_TIMEOUT`]).
    perf_dry_run_timeout: std::time::Duration,
    /// How long a miss is remembered ([`PERF_MISS_TTL`]).
    perf_miss_ttl: std::time::Duration,
}

impl LocalStation {
    /// A local station that runs the given `tt` binary.
    pub fn new(tt_bin: impl Into<PathBuf>) -> Self {
        Self {
            tt_bin: tt_bin.into(),
            name: "This machine".to_string(),
            default_port: DEFAULT_SERVE_PORT,
            mesh: Mutex::new(None),
            tools_dir: std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/share/tenstorrent/tools")),
            perf: Mutex::new(PerfCache::Unknown),
            perf_dry_run_timeout: PERF_DRY_RUN_TIMEOUT,
            perf_miss_ttl: PERF_MISS_TTL,
        }
    }

    /// Use `limit` instead of [`PERF_DRY_RUN_TIMEOUT`] for the perf lookup's dry run (tests).
    pub fn with_perf_dry_run_timeout(mut self, limit: std::time::Duration) -> Self {
        self.perf_dry_run_timeout = limit;
        self
    }

    /// Remember a failed perf lookup for `ttl` instead of [`PERF_MISS_TTL`] (tests).
    pub fn with_perf_miss_ttl(mut self, ttl: std::time::Duration) -> Self {
        self.perf_miss_ttl = ttl;
        self
    }

    /// Search `dir` instead of `~/.local/share/tenstorrent/tools` for tt-inference-server
    /// checkouts when the dry run names none (tests).
    pub fn with_tools_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.tools_dir = Some(dir.into());
        self
    }

    /// A local station using the binary `official_tt::resolve_bin` finds: the
    /// `TTS_OFFICIAL_TT_BIN` override, then `PATH`, then `~/.local/bin/tt`.
    pub fn from_env() -> Self {
        Self::new(resolve_bin())
    }

    /// Set the mesh label up front (for example from device detection).
    pub fn with_mesh(self, mesh: impl Into<String>) -> Self {
        *self.mesh.lock().expect("mesh mutex poisoned") = Some(mesh.into());
        self
    }

    pub fn id(&self) -> StationId {
        StationId("local".to_string())
    }

    pub fn kind(&self) -> StationKind {
        StationKind::Local
    }

    /// The state shown when a poll of this machine did not finish in time: unreachable, with
    /// the mesh already known. Built without running anything.
    pub fn offline_state(&self) -> StationState {
        StationState {
            id: self.id(),
            name: self.name.clone(),
            reachable: false,
            mesh: self.mesh_now(),
            serving: None,
            paired: true,
            contention: None,
            mac: None,
        }
    }

    fn mesh_now(&self) -> Option<String> {
        self.mesh.lock().expect("mesh mutex poisoned").clone()
    }

    /// Run `program args...` with no shell (only ever called with the `tt` binary). Stdin is closed and the child is killed if the
    /// future is dropped.
    async fn exec(program: &std::ffi::OsStr, args: &[&str]) -> Result<Captured> {
        // Retry a few times on ETXTBSY ("text file busy", errno 26). It happens when a script
        // was written a moment ago and another thread forked while the write handle was still
        // open. It is transient and never means the program is wrong.
        let mut attempt = 0;
        let out = loop {
            let res = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await;
            match res {
                Err(e) if e.raw_os_error() == Some(26) && attempt < 5 => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                other => {
                    break other.with_context(|| {
                        format!("could not run `{}`", program.to_string_lossy())
                    })?
                }
            }
        };
        Ok(Captured {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    async fn tt(&self, args: &[&str]) -> Result<Captured> {
        Self::exec(self.tt_bin.as_os_str(), args).await
    }

    /// Check that the binary is the official `tt`. The error text says `tt <semver>`.
    async fn vet(&self) -> Result<()> {
        let bin = self.tt_bin.to_string_lossy();
        let out = self.tt(&["--version"]).await?;
        match parse_version(&out.stdout) {
            Some(_) if out.success => Ok(()),
            _ => Err(anyhow!(not_official_message(&bin, &out.stdout))),
        }
    }

    /// Run a `tt --json ...` command and parse its stdout as JSON. A non-zero exit is an error
    /// carrying the first lines of stderr.
    async fn tt_json(&self, args: &[&str]) -> Result<serde_json::Value> {
        let out = self.tt(args).await?;
        if !out.success {
            bail!(
                "`tt {}` failed (exit {:?}): {}",
                args.join(" "),
                out.code,
                brief(&out.stderr)
            );
        }
        serde_json::from_str(&out.stdout)
            .with_context(|| format!("`tt {}` did not print JSON", args.join(" ")))
    }

    /// The running servers according to `tt --json model ps`.
    async fn served(&self) -> Result<Vec<Served>> {
        let v = self.tt_json(&["--json", "model", "ps"]).await?;
        let ps: Ps = serde_json::from_value(v)
            .context("`tt --json model ps` output did not parse as {\"served\": [...]}")?;
        Ok(ps.served.into_iter().filter(Served::is_running).collect())
    }

    /// Snapshot of this machine. `reachable` means the official `tt` runs here. A failing
    /// `model ps` leaves `serving` empty and does not make the station unreachable.
    pub async fn state(&self) -> StationState {
        let mut state = StationState {
            id: self.id(),
            name: self.name.clone(),
            reachable: false,
            mesh: self.mesh_now(),
            serving: None,
            paired: true,
            contention: None,
            mac: None,
        };
        if self.vet().await.is_err() {
            return state;
        }
        state.reachable = true;
        if let Ok(served) = self.served().await {
            state.serving = served.iter().find_map(Served::now);
        }
        state
    }

    /// The catalog from `tt --json model list`: `cached`, `source` and `type` per model, and
    /// `fits` against the `device` the same output names (which also becomes this station's
    /// mesh). Refuses a `tt` that is not the official one.
    pub async fn models(&self) -> Result<Vec<ModelRow>> {
        self.vet().await?;
        let v = self.tt_json(&["--json", "model", "list"]).await?;
        let device = v.get("device").and_then(|d| d.as_str()).map(str::to_string);
        if device.is_some() {
            *self.mesh.lock().expect("mesh mutex poisoned") = device.clone();
        }
        let entries = v
            .get("models")
            .and_then(|m| m.as_array())
            .context("`tt --json model list` output has no `models` array")?;
        Ok(entries
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?.to_string();
                let text = |k: &str| m.get(k).and_then(|x| x.as_str()).map(str::to_string);
                let hardware: Vec<String> = m
                    .get("hardware")
                    .and_then(|h| h.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(ModelRow {
                    fits: fits(&hardware, device.as_deref()),
                    kind: text("type").or_else(|| text("model_type")),
                    source: text("source").unwrap_or_else(|| "tt catalog".to_string()),
                    cached: m.get("cached").and_then(|c| c.as_bool()).unwrap_or(false),
                    hf_repo: text("hf_repo"),
                    hardware,
                    name,
                })
            })
            .collect())
    }

    /// Start `model` with `tt serve`. `force` has no meaning here (there is no other tenant to
    /// override) and is ignored.
    ///
    /// Order: validate the model name and options (nothing is spawned if they fail), check that
    /// `tt` is official, ask `tt --json serve <model> --dry-run` whether the model is a catalog
    /// model or a bundle, build the argv with [`local_serve_argv`], run it, then read `tt model ps`
    /// for the URL. `tt serve` returns once the server's container exists, so the model may
    /// still be loading when this returns; `state()` shows it as serving once `ps` lists it.
    ///
    /// **Cancelling.** Dropping the returned future kills the `tt` process (`kill_on_drop`) but
    /// not a container that `tt serve` already created. The UI task is to call a best-effort
    /// `stop()` after a cancel.
    pub async fn run(&self, model: &str, _force: bool, options: &RunOptions) -> Result<ServingNow> {
        validate_model_name(model)?;
        options.validate()?;
        // `benchmarks` and `evals` make `tt serve` run in the foreground until the whole
        // workflow finishes, which would block this call (and the tray) for that long. The
        // run schema offers only `server`; this refuses a request that names another anyway.
        if let Some(w) = options.workflow.filter(|w| *w != Workflow::Server) {
            bail!(
                "workflow: the tray serves models over /v1; the `{}` workflow runs in the \
                 foreground until it finishes, so start it from a terminal with \
                 `tt serve {model} --workflow {}`",
                w.as_str(),
                w.as_str()
            );
        }
        // `serve_plan` vets `tt` before the dry run.
        let plan = self.serve_plan(model).await?;
        let argv = local_serve_argv(&plan, model, self.default_port, options)?;
        let mut args: Vec<&str> = vec!["--json"];
        args.extend(argv.iter().map(String::as_str));
        let out = self.tt(&args).await?;
        if !out.success {
            bail!(
                "`tt {}` failed (exit {:?}): {}",
                argv.join(" "),
                out.code,
                brief(&out.stderr)
            );
        }

        let port = options.port.unwrap_or(self.default_port);
        let served = self.served().await.unwrap_or_default();
        let found = served
            .iter()
            .find(|s| s.port == Some(port))
            .or_else(|| served.iter().find(|s| s.name == model))
            .and_then(Served::now);
        Ok(found.unwrap_or_else(|| ServingNow {
            model: model.to_string(),
            base_url: format!("http://127.0.0.1:{port}/v1"),
        }))
    }

    /// The binary this station runs as `tt`, for showing the exact command in the run sheet.
    pub fn tt_bin(&self) -> &std::path::Path {
        &self.tt_bin
    }

    /// The port `tt serve --port` gets when the options do not set one.
    pub fn default_port(&self) -> u16 {
        self.default_port
    }

    /// `tt --json serve <model> --dry-run`: the JSON plan `tt` prints without starting anything.
    /// [`local_serve_argv`] reads it to tell a catalog model from a bundle. [`LocalStation::run`]
    /// calls this before every real serve, and the run sheet calls it so its command preview
    /// uses the same classification. Validates the name and vets `tt` first.
    pub async fn serve_plan(&self, model: &str) -> Result<serde_json::Value> {
        validate_model_name(model)?;
        self.vet().await?;
        self.tt_json(&["--json", "serve", model, "--dry-run"]).await
    }

    /// tt-inference-server's reference performance table ([`crate::perf`]) from the checkout
    /// `tt serve` would use, or `None`.
    ///
    /// The checkout is the directory of the `run.py` in the argv of
    /// `tt --json serve <sample_model> [--device <mesh>] --dry-run`. The dry run is read-only:
    /// tt's `serve` command returns the plan before any preflight, install or container start.
    /// When the mesh is known it is passed as `--device`, which also makes tt skip its own
    /// device auto-detection (a `tt-smi` snapshot), so the dry run reads nothing from the
    /// device. The version check and the dry run together get [`PERF_DRY_RUN_TIMEOUT`]; the
    /// child is killed if the limit passes (`kill_on_drop`).
    ///
    /// When there is no sample, the dry run fails or times out, or its checkout has no
    /// reference file, the newest `tt-inference-server-v*/src` under the tools directory that
    /// has one is used instead. The file is read and parsed on a blocking-pool thread, never on
    /// the caller's (GTK) thread. A found table is cached for the life of this station; a miss
    /// is cached for [`PERF_MISS_TTL`]. While either is cached, calls start no process.
    pub async fn perf_reference(&self, sample_model: Option<&str>) -> Option<Arc<PerfRef>> {
        match &*self.perf.lock().expect("perf mutex poisoned") {
            PerfCache::Found(p) => return Some(p.clone()),
            PerfCache::Missing(at) if at.elapsed() < self.perf_miss_ttl => return None,
            _ => {}
        }
        let found = self.find_perf_reference(sample_model).await;
        *self.perf.lock().expect("perf mutex poisoned") = match &found {
            Some(p) => PerfCache::Found(p.clone()),
            None => PerfCache::Missing(std::time::Instant::now()),
        };
        found
    }

    /// The uncached lookup behind [`LocalStation::perf_reference`].
    async fn find_perf_reference(&self, sample_model: Option<&str>) -> Option<Arc<PerfRef>> {
        let mut checkout = None;
        if let Some(model) = sample_model {
            let plan = tokio::time::timeout(self.perf_dry_run_timeout, self.perf_plan(model)).await;
            if let Ok(Ok(plan)) = plan {
                checkout =
                    perf::checkout_from_plan(&plan).filter(|c| perf::reference_path(c).is_some());
            }
        }
        if checkout.is_none() {
            checkout = self.tools_dir.as_deref().and_then(perf::newest_checkout);
        }
        let checkout = checkout?;
        tokio::task::spawn_blocking(move || perf::load(&checkout))
            .await
            .ok()
            .flatten()
            .map(Arc::new)
    }

    /// `tt --json serve <model> [--device <mesh>] --dry-run` for the perf lookup: like
    /// [`LocalStation::serve_plan`], plus `--device` when the mesh is known.
    async fn perf_plan(&self, model: &str) -> Result<serde_json::Value> {
        validate_model_name(model)?;
        let mesh = self.mesh_now();
        if let Some(m) = &mesh {
            // The mesh comes from `tt model list` output; check it like any name before it
            // reaches a command line.
            validate_model_name(m)?;
        }
        self.vet().await?;
        let mut args = vec!["--json", "serve", model];
        if let Some(m) = mesh.as_deref() {
            args.extend(["--device", m]);
        }
        args.push("--dry-run");
        self.tt_json(&args).await
    }

    /// Pick the server `stop` and `logs` act on. Rules:
    ///
    /// 1. If any running row has a URL, the first such row is the one the UI shows (the same
    ///    choice [`LocalStation::state`] makes). That is the target.
    /// 2. Otherwise, one running row is the target, none means there is nothing to act on, and
    ///    several is an error asking the caller to choose. It never means "all of them".
    ///
    /// TT-Studio rows are never returned when `allow_studio` is false (`stop`); a shown Studio
    /// row is then an error, because Studio manages its own containers.
    async fn target(&self, allow_studio: bool) -> Result<Option<Served>> {
        let rows = self.served().await?;
        let shown = rows.iter().position(|s| s.now().is_some());
        let pick = match shown {
            Some(i) => Some(i),
            None => match rows.len() {
                0 => None,
                1 => Some(0),
                n => bail!(
                    "{n} model servers are running and none is the one this app shows; stop the \
                     one you mean with `tt model stop <name>`"
                ),
            },
        };
        let Some(i) = pick else { return Ok(None) };
        let row = rows.into_iter().nth(i).expect("index from position");
        if !allow_studio && row.backend.as_deref() == Some("studio") {
            bail!("`{}` is managed by TT-Studio; stop it there", row.name);
        }
        // The name and profile come from `tt model ps`, which reads other tools' containers.
        // Check them like any client-supplied name before they reach a command line.
        validate_model_name(&row.name)?;
        if let Some(p) = &row.profile {
            validate_model_name(p)?;
        }
        Ok(Some(row))
    }

    /// The run name (`tt model ps`'s `name`, what `tt serve` was given) of the server this app
    /// shows as serving, picked by the same rule as [`LocalStation::stop`]. `None` when nothing
    /// runs. Restart uses it, so it never has to guess a run name from the served id.
    pub async fn shown_run_name(&self) -> Result<Option<String>> {
        self.vet().await?;
        Ok(self.target(true).await?.map(|row| row.name))
    }

    /// Stop the server this app shows as serving (see the module doc), with
    /// `tt model stop <name>` and `--profile <p>` when the row has one. Does nothing when
    /// nothing is running. Refuses a TT-Studio server, an unsafe name, and the ambiguous case
    /// where several servers run and none is the shown one.
    pub async fn stop(&self) -> Result<()> {
        self.vet().await?;
        let Some(row) = self.target(false).await? else {
            return Ok(());
        };
        let mut args = vec!["--json", "model", "stop", row.name.as_str()];
        if let Some(p) = row.profile.as_deref() {
            args.extend(["--profile", p]);
        }
        self.tt_json(&args).await.map(|_| ())
    }

    /// The last `tail` lines of the shown server's log, from `tt model logs <name> --tail N`
    /// (`--profile <p>` when the row has one). Empty when nothing is running.
    pub async fn logs(&self, tail: usize) -> Result<Vec<String>> {
        self.vet().await?;
        let Some(row) = self.target(true).await? else {
            return Ok(Vec::new());
        };
        let tail = tail.to_string();
        let mut args = vec!["model", "logs", row.name.as_str(), "--tail", tail.as_str()];
        if let Some(p) = row.profile.as_deref() {
            args.extend(["--profile", p]);
        }
        let out = self.tt(&args).await?;
        if !out.success {
            bail!(
                "`tt {}` failed (exit {:?}): {}",
                args.join(" "),
                out.code,
                brief(&out.stderr)
            );
        }
        Ok(out.stdout.lines().map(str::to_string).collect())
    }

    /// The run options `tt serve` accepts through this app. Does not start a process.
    pub async fn schema(&self) -> Result<RunSchema> {
        Ok(run_schema())
    }
}

/// The `tt` arguments (without the binary) that [`LocalStation::run`] runs for `model`.
///
/// `plan` is the output of `tt --json serve <model> --dry-run` ([`LocalStation::serve_plan`]).
/// It decides catalog against bundle through `ServeTarget::from_dry_run`; a plan that is not
/// known yet (`serde_json::Value::Null`) counts as the catalog, the stricter case. The run
/// sheet's command preview calls this same function, so the preview and the real serve cannot
/// build different commands.
pub fn local_serve_argv(
    plan: &serde_json::Value,
    model: &str,
    default_port: u16,
    options: &RunOptions,
) -> std::result::Result<Vec<String>, libttstation::run_options::OptionError> {
    serve_argv_for(
        ServeTarget::from_dry_run(plan),
        model,
        default_port,
        options,
    )
}

/// First few lines of a process's stderr, for an error message.
fn brief(stderr: &str) -> String {
    let s: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(4)
        .collect();
    if s.is_empty() {
        "(no output)".to_string()
    } else {
        s.join(" | ")
    }
}

/// The shape of `tt --json model ps`: `{"served": [...], "probed": bool}`.
#[derive(Deserialize)]
struct Ps {
    served: Vec<Served>,
}

/// The fields of one `served` row the tray uses. Every field but `name` is optional so a row
/// with missing keys still parses.
#[derive(Deserialize, Clone)]
struct Served {
    name: String,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    served_id: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

impl Served {
    /// `tt model ps` lists running containers by default; treat a row with no status as running.
    fn is_running(&self) -> bool {
        self.status.as_deref().is_none_or(|s| s == "running")
    }

    /// The model id and URL for this row, when the row has a way to reach it.
    fn now(&self) -> Option<ServingNow> {
        let base_url = match (&self.base_url, self.port) {
            (Some(u), _) => u.clone(),
            (None, Some(p)) => format!("http://127.0.0.1:{p}/v1"),
            (None, None) => return None,
        };
        Some(ServingNow {
            model: self.served_id.clone().unwrap_or_else(|| self.name.clone()),
            base_url,
        })
    }
}
