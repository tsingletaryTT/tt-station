//! This machine, driven through the official `tt` CLI (`tenstorrent/tt-cli`).
//!
//! Everything goes through `tt`: `tt --json model list`, `tt --json model ps`,
//! `tt --json serve`, `tt --json model stop`. The tray does not talk to Docker or run.py for
//! those. The one exception is [`LocalStation::logs`]: `tt` has no `logs` command (checked with
//! tt 1.0.1), so it reads the serving container's log with `docker logs`, using the container
//! name that `tt model ps` reports.
//!
//! **Process rules.** `tt` is started with [`tokio::process::Command`] and an argument vector.
//! No shell string is ever built. Model names and options are validated before anything is
//! spawned, so a bad request never starts a process. The `tt serve` argv comes from
//! `libttstation::run_options::serve_argv_for`, the function the agent and the run sheet's
//! command preview also use, so all three show the same command.
//!
//! **Official `tt` only.** Before the first real use, `tt --version` must print `tt <semver>`
//! (`libttstation::official_tt::parse_version`). A stale pre-rename copy of tt-station's own CLI
//! has shadowed the official one before; it is refused, not run.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};
use libttstation::official_tt::{not_official_message, parse_version, resolve_bin};
use libttstation::run_options::{
    run_schema, serve_argv_for, validate_model_name, RunOptions, RunSchema, ServeTarget,
};
use serde::Deserialize;
use tokio::process::Command;

use crate::station::{fits, ModelRow, ServingNow, StationId, StationKind, StationState};

/// Port used for `tt serve --port` when the options do not set one. It matches the default the
/// agent's `tt` backend uses for the same command.
pub const DEFAULT_SERVE_PORT: u16 = 20000;

/// What a finished `tt` (or `docker`) invocation left behind.
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
}

impl LocalStation {
    /// A local station that runs the given `tt` binary.
    pub fn new(tt_bin: impl Into<PathBuf>) -> Self {
        Self {
            tt_bin: tt_bin.into(),
            name: "This machine".to_string(),
            default_port: DEFAULT_SERVE_PORT,
            mesh: Mutex::new(None),
        }
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

    fn mesh_now(&self) -> Option<String> {
        self.mesh.lock().expect("mesh mutex poisoned").clone()
    }

    /// Run `program args...` with no shell. Stdin is closed and the child is killed if the
    /// future is dropped.
    async fn exec(program: &std::ffi::OsStr, args: &[&str]) -> Result<Captured> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("could not run `{}`", program.to_string_lossy()))?;
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
    /// model or a bundle, build the argv with `serve_argv_for`, run it, then read `tt model ps`
    /// for the URL. `tt serve` returns once the server's container exists, so the model may
    /// still be loading when this returns; `state()` shows it as serving once `ps` lists it.
    pub async fn run(&self, model: &str, _force: bool, options: &RunOptions) -> Result<ServingNow> {
        validate_model_name(model)?;
        options.validate()?;
        self.vet().await?;

        let plan = self
            .tt_json(&["--json", "serve", model, "--dry-run"])
            .await?;
        let argv = serve_argv_for(
            ServeTarget::from_dry_run(&plan),
            model,
            self.default_port,
            options,
        )?;
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

    /// Stop every running model server `tt model ps` lists, except TT-Studio's containers (the
    /// tray did not start those and Studio manages them). Succeeds with nothing to do when
    /// nothing is running. If several stops fail, the first error is returned after all were
    /// tried.
    pub async fn stop(&self) -> Result<()> {
        self.vet().await?;
        let mut first_err = None;
        for s in self.served().await? {
            if s.backend.as_deref() == Some("studio") {
                continue;
            }
            if let Err(e) = self.tt_json(&["--json", "model", "stop", &s.name]).await {
                first_err.get_or_insert(e);
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// The last `tail` lines of the running server's container log, via `docker logs` (`tt` has
    /// no logs command). Empty when nothing is running.
    pub async fn logs(&self, tail: usize) -> Result<Vec<String>> {
        self.vet().await?;
        let Some(container) = self.served().await?.into_iter().find_map(|s| s.container) else {
            return Ok(Vec::new());
        };
        let tail = tail.to_string();
        let out = Self::exec(
            std::ffi::OsStr::new("docker"),
            &["logs", "--tail", &tail, &container],
        )
        .await?;
        if !out.success {
            bail!("`docker logs {container}` failed: {}", brief(&out.stderr));
        }
        // Docker writes a container's stderr to our stderr; the log reader wants both, in order
        // as far as two pipes allow.
        Ok(out
            .stdout
            .lines()
            .chain(out.stderr.lines())
            .map(str::to_string)
            .collect())
    }

    /// The run options `tt serve` accepts through this app. Does not start a process.
    pub async fn schema(&self) -> Result<RunSchema> {
        Ok(run_schema())
    }
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
#[derive(Deserialize)]
struct Served {
    name: String,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    container: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    served_id: Option<String>,
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
