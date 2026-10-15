// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The official Tenstorrent CLI (`tt`, from `tenstorrent/tt-cli`) as this box's
//! model manager. Per this repo's naming rule, when the official CLI covers a
//! capability we delegate to it rather than rolling our own:
//!
//! * **Catalog** -- `tt --json model list --hw <mesh>`: the curated list of models
//!   this mesh can run, with per-device support status, tool-call parser, whether
//!   the weights are already in the HF cache (`cached`) and how big they are.
//! * **Downloads** -- `tt --json model pull <name>`, run in the background as a
//!   tracked job so a phone can start one and walk away. Progress is the size of
//!   the model's HF cache directory, measured when asked (`tt` itself reports
//!   nothing until it exits).
//!
//! Serving through `tt serve` lives in the runpy backend (`--serve-with tt`, see
//! `serving::runpy::tt_serve_argv`), which keeps its own sweep/reset/readiness
//! handling around the launch.
//!
//! Every method here blocks (subprocesses, filesystem walks); call them from
//! `tokio::task::spawn_blocking`.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::Value;

/// How long a `tt model list` result is reused (~0.3s and a Python start per
/// call on a QuietBox). What tt lists only changes when a model is pulled,
/// removed or tt is updated, and each of those drops the cached catalog.
pub const CATALOG_TTL: Duration = Duration::from_secs(60);

/// How long a cached catalog's weights measurement (on disk, see
/// [`weights_state`]) is reused: downloads made outside tt show up this soon.
pub const WEIGHTS_TTL: Duration = Duration::from_secs(10);

/// How often a pull's waiter thread checks whether `tt model pull` exited.
const PULL_POLL: Duration = Duration::from_millis(500);

/// One model in the box's catalog, flattened to what a client needs: the
/// per-device fields are taken from the entry for the box's own mesh.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogModel {
    /// The tt catalog name (`Qwen3-32B`): what `tt serve`/`tt model pull` take.
    pub name: String,
    /// The Hugging Face repo (`Qwen/Qwen3-32B`), when tt knows it.
    pub hf_repo: Option<String>,
    /// `llm`, `audio`, `image`, `video`, `text_to_speech`, ...
    pub model_type: String,
    /// Support status on this mesh: `COMPLETE`, `FUNCTIONAL`, `EXPERIMENTAL`.
    pub status: Option<String>,
    /// Whether tt says this mesh can run it.
    pub supported: bool,
    pub engines: Vec<String>,
    /// The vLLM tool-call parser tt serves it with; `None` means no tool calling,
    /// which the box assistant needs.
    pub tool_call_parser: Option<String>,
    /// Weights already in the HF cache.
    pub cached: bool,
    pub cache_size_bytes: u64,
    /// Free disk tt says a full download + serve needs.
    pub min_disk_gb: Option<f64>,
    /// Billions of parameters, when known.
    pub param_count: Option<f64>,
    /// The longest context tt serves it with on this mesh, in tokens.
    pub max_context: Option<u64>,
    /// Whether this model's weights are really all here -- measured by the agent,
    /// because tt's `cached` only means "something for this repo is in the HF
    /// cache" (a 385 MB stub of a 62 GB model counts). See [`weights_state`].
    pub weights: Weights,
    /// Bytes of this model in the HF cache right now.
    pub downloaded_bytes: u64,
}

/// Where a model's weights stand on this box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Weights {
    /// Every weight file is present: ready to serve.
    Complete,
    /// Some of it is here: an interrupted or in-progress download.
    Partial,
    /// Nothing is here.
    Missing,
    /// The serving container fetches its own weights (speech-to-text and TTS
    /// containers, and models whose weights ship in the image), so there is
    /// nothing to download first.
    Container,
}

impl Weights {
    /// Can this model be started without downloading first?
    pub fn ready(self) -> bool {
        matches!(self, Weights::Complete | Weights::Container)
    }
}

/// How recently an `.incomplete` blob must have been written to count as a
/// download in progress (see [`weights_state`]).
const ACTIVE_DOWNLOAD_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Model types whose serving container downloads its own weights; mirrors tt's
/// `_CONTAINER_FETCHED_TYPES` (`tenstorrent/modelhub/hub.py`).
const CONTAINER_FETCHED_TYPES: [&str; 2] = ["audio", "text_to_speech"];

/// Measure `model`'s weights in the HF cache: [`Weights`] plus bytes on disk.
///
/// Complete means all of: no `*.incomplete` blobs (an unfinished download), every
/// file named by every `*.index.json` in the snapshot present (sharded
/// safetensors -- including diffusers' per-component indexes), and at least one
/// weight file (`.safetensors`/`.bin`/`.pth`/`.pt`/`.gguf`) at all, so a
/// config-only stub doesn't pass.
pub fn weights_state(hf_cache: &Path, model: &CatalogModel) -> (Weights, u64) {
    let repo = match &model.hf_repo {
        Some(r)
            if r.contains('/') && !CONTAINER_FETCHED_TYPES.contains(&model.model_type.as_str()) =>
        {
            r
        }
        _ => return (Weights::Container, 0),
    };
    let dir = hf_repo_dir(hf_cache, repo);
    let bytes = dir_size(&dir);
    if bytes == 0 {
        return (Weights::Missing, 0);
    }
    // An `.incomplete` blob written to in the last few minutes is a download in
    // flight. Older ones are leftovers of interrupted attempts that a later
    // download superseded -- seen live: 16 stale ones beside a complete Qwen3-32B
    // -- and don't make an otherwise complete repo partial. (A missing shard is
    // still caught below: a snapshot links a file only once it finished.)
    let downloading = std::fs::read_dir(dir.join("blobs"))
        .map(|it| {
            it.flatten().any(|e| {
                e.file_name().to_string_lossy().ends_with(".incomplete")
                    && e.metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| t.elapsed().ok())
                        .is_some_and(|age| age < ACTIVE_DOWNLOAD_WINDOW)
            })
        })
        .unwrap_or(false);
    if downloading {
        return (Weights::Partial, bytes);
    }
    // Newest snapshot (a repo can hold several revisions).
    let snapshot = std::fs::read_dir(dir.join("snapshots"))
        .ok()
        .and_then(|it| {
            it.flatten()
                .filter(|e| e.path().is_dir())
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        })
        .map(|e| e.path());
    let Some(snapshot) = snapshot else {
        return (Weights::Partial, bytes);
    };
    let mut files = Vec::new();
    collect_files(&snapshot, &mut files);
    let is_weight = |p: &Path| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("safetensors" | "bin" | "pth" | "pt" | "gguf")
        )
    };
    // `metadata` follows the snapshot's symlinks into `blobs/`, so a dangling
    // link (blob not downloaded yet) reads as absent.
    let present = |p: &Path| std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false);
    if !files.iter().any(|f| is_weight(f) && present(f)) {
        return (Weights::Partial, bytes);
    }
    for index in files
        .iter()
        .filter(|f| f.to_string_lossy().ends_with(".index.json"))
    {
        let Some(map) = std::fs::read_to_string(index)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| v.get("weight_map").cloned())
        else {
            continue;
        };
        let base = index.parent().unwrap_or(&snapshot);
        let all_there = map
            .as_object()
            .map(|m| {
                m.values()
                    .filter_map(Value::as_str)
                    .all(|name| present(&base.join(name)))
            })
            .unwrap_or(true);
        if !all_there {
            return (Weights::Partial, bytes);
        }
    }
    (Weights::Complete, bytes)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        // `is_dir` follows symlinks; snapshot dirs are real, files are links.
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Catalog {
    /// The mesh the catalog was filtered for (`p300x2`).
    pub device: Option<String>,
    pub models: Vec<CatalogModel>,
}

impl Catalog {
    /// Find a model by tt name or HF repo id, case-insensitively.
    pub fn find(&self, name: &str) -> Option<&CatalogModel> {
        let want = name.trim().to_lowercase();
        self.models.iter().find(|m| {
            m.name.to_lowercase() == want
                || m.hf_repo.as_deref().map(str::to_lowercase) == Some(want.clone())
        })
    }
}

/// Parse `tt --json model list` output. `device` picks which per-device entry
/// supplies `status`/`supported`/`engines`/`tool_call_parser`; it defaults to the
/// output's own top-level `device`.
pub fn parse_catalog(json: &str, device: Option<&str>) -> Result<Catalog> {
    let root: Value = serde_json::from_str(json).context("tt model list printed invalid JSON")?;
    let device = device
        .map(str::to_string)
        .or_else(|| {
            root.get("device")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|d| d.to_lowercase());
    let models = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tt model list output has no `models` array"))?;

    let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let parsed = models
        .iter()
        .filter_map(|m| {
            let name = str_of(m, "name")?;
            let per_device = device
                .as_deref()
                .and_then(|d| m.get("devices").and_then(|ds| ds.get(d)))
                .cloned()
                .unwrap_or(Value::Null);
            let engines_of = |v: &Value| -> Vec<String> {
                v.get("engines")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let engines = match engines_of(&per_device) {
                e if !e.is_empty() => e,
                _ => engines_of(m),
            };
            Some(CatalogModel {
                name,
                hf_repo: str_of(m, "hf_repo"),
                model_type: str_of(m, "model_type").unwrap_or_else(|| "unknown".to_string()),
                status: str_of(&per_device, "status"),
                supported: per_device
                    .get("supported")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                engines,
                tool_call_parser: str_of(&per_device, "tool_call_parser"),
                cached: m.get("cached").and_then(Value::as_bool).unwrap_or(false),
                cache_size_bytes: m
                    .get("cache_size_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                min_disk_gb: m.get("min_disk_gb").and_then(Value::as_f64),
                param_count: m.get("param_count").and_then(Value::as_f64),
                max_context: per_device.get("max_context").and_then(Value::as_u64),
                // Filled in from the disk by `TtCli::catalog`.
                weights: Weights::Missing,
                downloaded_bytes: 0,
            })
        })
        .collect();
    Ok(Catalog {
        device,
        models: parsed,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PullState {
    Running,
    Done,
    Failed,
    Cancelled,
}

/// A `tt model pull` this agent started.
#[derive(Debug, Clone, Serialize)]
pub struct PullJob {
    /// The tt catalog name being pulled.
    pub model: String,
    pub hf_repo: Option<String>,
    pub state: PullState,
    /// Why it failed, in a sentence: tt's structured error (`what` + `next_step`)
    /// when it printed one, else the last lines it printed.
    pub error: Option<String>,
    /// A page the user can act on to fix the failure -- e.g. a gated repo's
    /// license page on huggingface.co.
    pub action_url: Option<String>,
    /// Unix seconds.
    pub started_at: u64,
    pub finished_at: Option<u64>,
    /// Bytes in the model's HF cache dir right now (measured on read).
    pub bytes_on_disk: u64,
    /// While it runs: bytes against the repo's full size, speed and time left
    /// (`download::progress`), measured on read.
    pub progress: Option<serde_json::Value>,
    #[serde(skip)]
    log_path: PathBuf,
}

/// Why `start_pull` refused.
#[derive(Debug, PartialEq)]
pub enum PullRefusal {
    AlreadyRunning,
    Spawn(String),
}

/// A cached catalog: when tt listed it, when its weights were measured, the catalog.
type CachedCatalog = (Instant, Instant, Catalog);

/// The official tt CLI, plus this agent's pull jobs.
pub struct TtCli {
    /// How to invoke `tt` -- normally just its path. A list so tests can run a
    /// stub script through `sh` (exec'ing a freshly written script races ETXTBSY
    /// under parallel tests).
    argv: Vec<String>,
    /// The host Hugging Face cache root (`~/.cache/huggingface`) where pulled
    /// weights land; used to measure download progress.
    hf_cache: PathBuf,
    /// Extra environment for `tt model pull` (e.g. `HF_TOKEN` for gated repos).
    pull_env: Vec<(String, String)>,
    /// Where pull logs go.
    log_dir: PathBuf,
    /// Recent catalogs by device (`p300x2`, and `p300` for co-serving's halves):
    /// when tt listed it, when its weights were measured, the catalog.
    catalog: Arc<Mutex<HashMap<String, CachedCatalog>>>,
    jobs: Arc<Mutex<HashMap<String, PullJob>>>,
    children: Arc<Mutex<HashMap<String, Child>>>,
    /// Pulls this agent didn't spawn but adopted at startup (`pid` per model):
    /// not our children, so cancel signals them instead of `Child::kill`.
    adopted: Arc<Mutex<HashMap<String, u32>>>,
}

impl TtCli {
    pub fn new(argv: Vec<String>, hf_cache: impl Into<PathBuf>) -> Self {
        TtCli {
            argv,
            hf_cache: hf_cache.into(),
            pull_env: Vec::new(),
            log_dir: std::env::temp_dir().join("tt-station-pulls"),
            catalog: Arc::new(Mutex::new(HashMap::new())),
            jobs: Arc::new(Mutex::new(HashMap::new())),
            children: Arc::new(Mutex::new(HashMap::new())),
            adopted: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_pull_env(mut self, env: Vec<(String, String)>) -> Self {
        self.pull_env = env;
        self
    }

    pub fn with_log_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.log_dir = dir.into();
        self
    }

    /// How to invoke `tt` (for `--serve-with tt`).
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    pub fn hf_cache(&self) -> &Path {
        &self.hf_cache
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]).args(args);
        // `tt model pull` can prompt for an HF token; never on the box's console.
        crate::serving::docker::detach_from_terminal(&mut cmd);
        cmd
    }

    /// Run `tt --json <args>` and parse its answer. tt's structured failures
    /// (`{"error": {"what", "next_step"}}`) become a readable sentence.
    pub fn run_json(&self, args: &[&str]) -> std::result::Result<serde_json::Value, String> {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self
            .command(&full)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("couldn't run tt: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // Warnings can precede the JSON; it starts at the first `{` or `[`.
        let start = stdout.find(['{', '[']).unwrap_or(stdout.len());
        let value: Option<serde_json::Value> = serde_json::from_str(&stdout[start..]).ok();
        if let Some(error) = value
            .as_ref()
            .and_then(|v| v.get("error"))
            .filter(|e| e.is_object())
        {
            let what = error["what"].as_str().unwrap_or("tt failed");
            return Err(match error["next_step"].as_str() {
                Some(next) => format!("{what} {next}"),
                None => what.to_string(),
            });
        }
        match value {
            Some(v) if out.status.success() => Ok(v),
            _ => Err(format!(
                "`tt {}` failed: {}",
                args.join(" "),
                last_lines(&String::from_utf8_lossy(&out.stderr), 3)
            )),
        }
    }

    /// Forget the cached catalog (after removing a model, say).
    pub fn invalidate_catalog(&self) {
        self.catalog.lock().expect("catalog mutex poisoned").clear();
    }

    /// The Python that runs `tt` (the `python3` beside its script), when `tt` is a
    /// virtualenv's script: the one `pip install --upgrade tenstorrent` must use.
    pub fn venv_python(&self) -> Option<PathBuf> {
        let py = std::path::Path::new(self.argv.first()?)
            .parent()?
            .join("python3");
        py.is_file().then_some(py)
    }

    /// `tt --version`'s first line (e.g. `tt 1.0.1`), or `None` if it doesn't run
    /// or isn't the official CLI.
    pub fn version(&self) -> Option<String> {
        let out = self.command(&["--version"]).output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().next()?.trim().to_string();
        (out.status.success() && line.starts_with("tt ")).then_some(line)
    }

    /// The catalog for `device`, reusing one younger than [`CATALOG_TTL`] unless
    /// `fresh`.
    pub fn catalog(&self, device: Option<&str>, fresh: bool) -> Result<Catalog> {
        let key = device.map(str::to_lowercase).unwrap_or_default();
        if !fresh {
            let cached = self
                .catalog
                .lock()
                .expect("catalog mutex poisoned")
                .get(&key)
                .filter(|(at, _, _)| at.elapsed() < CATALOG_TTL)
                .cloned();
            if let Some((at, weighed, mut cat)) = cached {
                if weighed.elapsed() >= WEIGHTS_TTL {
                    self.weigh(&mut cat);
                    self.catalog
                        .lock()
                        .expect("catalog mutex poisoned")
                        .insert(key, (at, Instant::now(), cat.clone()));
                }
                return Ok(cat);
            }
        }
        let mut args = vec!["--json", "model", "list"];
        if let Some(d) = device {
            args.extend(["--hw", d]);
        }
        let out = self
            .command(&args)
            .output()
            .context("running `tt model list`")?;
        if !out.status.success() {
            return Err(anyhow!(
                "`tt model list` failed: {}",
                last_lines(&String::from_utf8_lossy(&out.stderr), 3)
            ));
        }
        let mut cat = parse_catalog(&String::from_utf8_lossy(&out.stdout), device)?;
        self.weigh(&mut cat);
        self.catalog
            .lock()
            .expect("catalog mutex poisoned")
            .insert(key, (Instant::now(), Instant::now(), cat.clone()));
        Ok(cat)
    }

    /// Measure each model's weights on disk now.
    fn weigh(&self, cat: &mut Catalog) {
        for model in &mut cat.models {
            (model.weights, model.downloaded_bytes) = weights_state(&self.hf_cache, model);
        }
    }

    /// Start `tt model pull <model.name>` in the background. A finished job for
    /// the same model is replaced; a running one refuses.
    pub fn start_pull(&self, model: &CatalogModel) -> Result<(), PullRefusal> {
        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        if jobs
            .get(&model.name)
            .is_some_and(|j| j.state == PullState::Running)
        {
            return Err(PullRefusal::AlreadyRunning);
        }
        std::fs::create_dir_all(&self.log_dir).map_err(|e| PullRefusal::Spawn(e.to_string()))?;
        let log_path = self
            .log_dir
            .join(format!("pull-{}.log", sanitize(&model.name)));
        let log = File::create(&log_path).map_err(|e| PullRefusal::Spawn(e.to_string()))?;
        let log_err = log
            .try_clone()
            .map_err(|e| PullRefusal::Spawn(e.to_string()))?;
        let child = self
            .command(&["--json", "model", "pull", &model.name])
            .envs(self.pull_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdout(log)
            .stderr(log_err)
            .spawn()
            .map_err(|e| PullRefusal::Spawn(format!("couldn't run tt: {e}")))?;

        jobs.insert(
            model.name.clone(),
            PullJob {
                model: model.name.clone(),
                hf_repo: model.hf_repo.clone(),
                state: PullState::Running,
                error: None,
                action_url: None,
                started_at: unix_now(),
                finished_at: None,
                bytes_on_disk: 0,
                progress: None,
                log_path: log_path.clone(),
            },
        );
        drop(jobs);
        self.children
            .lock()
            .expect("children mutex poisoned")
            .insert(model.name.clone(), child);
        eprintln!(
            "tt-station-agentd: pulling {} (log: {})",
            model.name,
            log_path.display()
        );

        // Waiter: poll the child (holding the lock only briefly, so a cancel can
        // get in), then record the outcome and drop the stale catalog.
        let (name, jobs, children, catalog) = (
            model.name.clone(),
            Arc::clone(&self.jobs),
            Arc::clone(&self.children),
            Arc::clone(&self.catalog),
        );
        std::thread::spawn(move || loop {
            let status = {
                let mut kids = children.lock().expect("children mutex poisoned");
                match kids.get_mut(&name).map(Child::try_wait) {
                    Some(Ok(Some(status))) => {
                        kids.remove(&name);
                        Some(Ok(status))
                    }
                    Some(Ok(None)) => None,
                    Some(Err(e)) => {
                        kids.remove(&name);
                        Some(Err(e))
                    }
                    None => Some(Err(std::io::Error::other("pull process vanished"))),
                }
            };
            let Some(result) = status else {
                std::thread::sleep(PULL_POLL);
                continue;
            };
            let mut jobs = jobs.lock().expect("jobs mutex poisoned");
            if let Some(job) = jobs.get_mut(&name) {
                job.finished_at = Some(unix_now());
                if job.state == PullState::Cancelled {
                    // `cancel_pull` already said so; the kill is why it exited.
                } else {
                    match result {
                        Ok(s) if s.success() => job.state = PullState::Done,
                        Ok(s) => {
                            job.state = PullState::Failed;
                            let log = std::fs::read_to_string(&job.log_path).unwrap_or_default();
                            let (message, url) = explain_pull_failure(&log);
                            job.error =
                                Some(message.unwrap_or_else(|| match last_lines(&log, 4) {
                                    t if t.is_empty() => format!("tt model pull exited with {s}"),
                                    t => t,
                                }));
                            job.action_url = url;
                        }
                        Err(e) => {
                            job.state = PullState::Failed;
                            job.error = Some(e.to_string());
                        }
                    }
                }
                eprintln!(
                    "tt-station-agentd: pull of {name} finished: {:?}",
                    job.state
                );
            }
            // What tt lists as downloaded changed with it.
            catalog.lock().expect("catalog mutex poisoned").clear();
            break;
        });
        // The catalog's `cached` flags are about to change.
        self.catalog.lock().expect("catalog mutex poisoned").clear();
        Ok(())
    }

    /// Kill a running pull. `false` if there was none.
    pub fn cancel_pull(&self, name: &str) -> bool {
        let mut jobs = self.jobs.lock().expect("jobs mutex poisoned");
        let Some(job) = jobs.get_mut(name).filter(|j| j.state == PullState::Running) else {
            return false;
        };
        job.state = PullState::Cancelled;
        drop(jobs);
        if let Some(child) = self
            .children
            .lock()
            .expect("children mutex poisoned")
            .get_mut(name)
        {
            let _ = child.kill();
        } else if let Some(pid) = self
            .adopted
            .lock()
            .expect("adopted mutex poisoned")
            .get(name)
        {
            // SAFETY: plain kill(2) on a pid we found running `tt model pull`.
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGTERM);
            }
        }
        true
    }

    /// Adopt `tt model pull` processes already running when the agent starts --
    /// left by a previous agent that was restarted mid-download -- so the app
    /// still shows them (and can cancel them). They aren't our children, so the
    /// outcome is judged from the disk when the process goes away: complete
    /// weights mean done; anything else is an interrupted download to resume.
    pub fn adopt_running_pulls(self: &Arc<Self>) -> usize {
        let found = running_pulls();
        for (pid, name) in &found {
            eprintln!("tt-station-agentd: adopting running download of {name} (pid {pid})");
            let hf_repo = self
                .catalog(None, false)
                .ok()
                .and_then(|c| c.find(name).and_then(|m| m.hf_repo.clone()));
            self.jobs.lock().expect("jobs mutex poisoned").insert(
                name.clone(),
                PullJob {
                    model: name.clone(),
                    hf_repo,
                    state: PullState::Running,
                    error: None,
                    action_url: None,
                    started_at: unix_now(),
                    finished_at: None,
                    bytes_on_disk: 0,
                    progress: None,
                    log_path: PathBuf::new(),
                },
            );
            self.adopted
                .lock()
                .expect("adopted mutex poisoned")
                .insert(name.clone(), *pid);
            let (me, name, pid) = (Arc::clone(self), name.clone(), *pid);
            std::thread::spawn(move || {
                while Path::new(&format!("/proc/{pid}")).exists() {
                    std::thread::sleep(Duration::from_secs(2));
                }
                me.adopted
                    .lock()
                    .expect("adopted mutex poisoned")
                    .remove(&name);
                me.catalog.lock().expect("catalog mutex poisoned").clear();
                let complete = me
                    .catalog(None, true)
                    .ok()
                    .and_then(|c| c.find(&name).map(|m| m.weights == Weights::Complete))
                    .unwrap_or(false);
                let mut jobs = me.jobs.lock().expect("jobs mutex poisoned");
                if let Some(job) = jobs.get_mut(&name) {
                    job.finished_at = Some(unix_now());
                    if job.state != PullState::Cancelled {
                        if complete {
                            job.state = PullState::Done;
                        } else {
                            job.state = PullState::Failed;
                            job.error = Some(
                                "The download stopped before finishing. Resume it to continue \
                                 where it left off."
                                    .to_string(),
                            );
                        }
                    }
                }
            });
        }
        found.len()
    }

    /// Every pull this agent started, newest first, with `bytes_on_disk` measured now.
    pub fn pulls(&self) -> Vec<PullJob> {
        let mut out: Vec<PullJob> = self
            .jobs
            .lock()
            .expect("jobs mutex poisoned")
            .values()
            .cloned()
            .collect();
        for job in &mut out {
            if let Some(repo) = &job.hf_repo {
                job.bytes_on_disk = dir_size(&hf_repo_dir(&self.hf_cache, repo));
                if job.state == PullState::Running {
                    let hub = hf_repo_dir(&self.hf_cache, repo)
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    let done = crate::download::repo_on_disk(&hub, repo);
                    job.bytes_on_disk = done;
                    job.progress = Some(crate::download::progress(
                        &format!("tt:{}", job.model),
                        done,
                        crate::download::repo_total(repo),
                    ));
                }
            }
        }
        out.sort_by_key(|j| std::cmp::Reverse(j.started_at));
        out
    }

    /// Whether a pull for `name` is running right now.
    pub fn is_pulling(&self, name: &str) -> bool {
        self.jobs
            .lock()
            .expect("jobs mutex poisoned")
            .get(name)
            .is_some_and(|j| j.state == PullState::Running)
    }
}

/// `tt model pull <name>` processes running on this box, as `(pid, name)`,
/// found through `/proc/*/cmdline`.
fn running_pulls() -> Vec<(u32, String)> {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    procs
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let raw = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<String> = raw
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).to_string())
                .collect();
            let at = args
                .windows(2)
                .position(|w| w[0] == "model" && w[1] == "pull")?;
            // The tt entry point (`.../bin/tt`) somewhere before "model pull".
            if !args[..at].iter().any(|a| a == "tt" || a.ends_with("/tt")) {
                return None;
            }
            args.get(at + 2)
                .filter(|n| !n.starts_with('-'))
                .map(|n| (pid, n.clone()))
        })
        .collect()
}

/// `<hf_cache>/hub/models--Org--Name`, the directory huggingface_hub downloads
/// `Org/Name` into. Accepts a cache root that already ends in `hub`.
pub fn hf_repo_dir(hf_cache: &Path, repo: &str) -> PathBuf {
    let hub = if hf_cache.ends_with("hub") {
        hf_cache.to_path_buf()
    } else {
        hf_cache.join("hub")
    };
    hub.join(format!("models--{}", repo.replace('/', "--")))
}

/// Disk space the regular files under `dir` actually occupy (allocated blocks),
/// not following symlinks -- an HF cache's `snapshots/` are symlinks into
/// `blobs/`, so each blob counts once. Blocks rather than file length because
/// interrupted downloads can leave sparse, preallocated `.incomplete` blobs: seen
/// live, 7.5 GB of apparent size holding 28 MB of data.
pub fn dir_size(dir: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.blocks() * 512).unwrap_or(0),
            // A finished blob kept in a shared store (`hub/blobs/..`) and linked from the repo's own `blobs/`.
            Ok(t) if t.is_symlink() && dir.file_name().is_some_and(|n| n == "blobs") => {
                std::fs::metadata(e.path())
                    .ok()
                    .filter(|m| m.is_file())
                    .map(|m| m.blocks() * 512)
                    .unwrap_or(0)
            }
            _ => 0,
        })
        .sum()
}

/// Free bytes on the filesystem holding `path` (via `df`; `None` if it can't tell).
pub fn disk_free_bytes(path: &Path) -> Option<u64> {
    let probe = std::iter::successors(Some(path), |p| p.parent()).find(|p| p.exists())?;
    let out = Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(probe)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

/// Find the official tt CLI: `explicit` if given, else the first working `tt` on
/// `$PATH`, else `~/.tenstorrent-venv/bin/tt` (where the official installer puts
/// it). "Working" means `tt --version` answers `tt <version>` -- an unrelated
/// `tt` binary is skipped rather than trusted.
pub fn locate(explicit: Option<&str>) -> Option<TtCli> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(PathBuf::from(p));
    } else {
        if let Some(path) = std::env::var_os("PATH") {
            candidates.extend(std::env::split_paths(&path).map(|d| d.join("tt")));
        }
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(".tenstorrent-venv/bin/tt"));
        }
    }
    let hf_cache = std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".cache/huggingface"))
        .unwrap_or_else(|| PathBuf::from(".cache/huggingface"));
    candidates
        .into_iter()
        .filter(|p| p.is_file())
        .map(|p| TtCli::new(vec![p.display().to_string()], hf_cache.clone()))
        .find(|tt| tt.version().is_some())
}

/// The newest tt-managed tt-inference-server checkout
/// (`~/.local/share/tenstorrent/tools/tt-inference-server-v*/src`): where
/// `tt serve` runs run.py, so where its `workflow_logs/` land.
pub fn managed_inference_checkout() -> Option<PathBuf> {
    let tools = PathBuf::from(std::env::var_os("HOME")?).join(".local/share/tenstorrent/tools");
    let mut found: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(tools)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let version = name.strip_prefix("tt-inference-server-v")?;
            let key = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
            let src = e.path().join("src");
            src.join("run.py").is_file().then_some((key, src))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, p)| p)
}

/// Turn `tt --json`'s structured failure into a sentence for a person, plus any
/// link it points at. tt prints `{"error": {"what", "why", "next_step", ...}}`;
/// for a gated repo the next step names the license page.
pub fn explain_pull_failure(log: &str) -> (Option<String>, Option<String>) {
    let parsed = log
        .find('{')
        .zip(log.rfind('}'))
        .and_then(|(a, b)| serde_json::from_str::<Value>(&log[a..=b]).ok());
    let Some(error) = parsed.as_ref().and_then(|v| v.get("error")) else {
        return (None, None);
    };
    let what = error
        .get("what")
        .and_then(Value::as_str)
        .unwrap_or("The download failed.");
    let next = error.get("next_step").and_then(Value::as_str).unwrap_or("");
    let url = next
        .split_whitespace()
        .find(|w| w.starts_with("https://"))
        .map(|w| w.trim_end_matches(['.', ',', ')', ';']).to_string());
    let message = if what.to_lowercase().contains("gated") {
        format!(
            "{what} Accept its license on Hugging Face with the account this box is logged in as, then try again."
        )
    } else if next.is_empty() {
        what.to_string()
    } else {
        format!("{what} {next}")
    };
    (Some(message), url)
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "device": "p300x2",
      "models": [
        { "name": "Qwen3-32B", "hf_repo": "Qwen/Qwen3-32B", "model_type": "llm",
          "engines": ["vLLM"], "cached": true, "cache_size_bytes": 65500000000,
          "min_disk_gb": 84, "param_count": 32,
          "devices": { "p300x2": { "engines": ["vLLM"], "status": "FUNCTIONAL", "supported": true,
                                    "tool_call_parser": "hermes", "max_context": 131072 } } },
        { "name": "whisper-large-v3", "hf_repo": "openai/whisper-large-v3", "model_type": "audio",
          "engines": ["media"], "cached": false, "cache_size_bytes": 0, "min_disk_gb": 15,
          "param_count": null,
          "devices": { "n150": { "status": "COMPLETE", "supported": true } } }
      ]
    }"#;

    #[test]
    fn parse_catalog_flattens_the_entry_for_this_mesh() {
        let cat = parse_catalog(SAMPLE, None).unwrap();
        assert_eq!(cat.device.as_deref(), Some("p300x2"));
        let qwen = &cat.models[0];
        assert_eq!(qwen.name, "Qwen3-32B");
        assert_eq!(qwen.status.as_deref(), Some("FUNCTIONAL"));
        assert!(qwen.supported && qwen.cached);
        assert_eq!(qwen.tool_call_parser.as_deref(), Some("hermes"));
        assert_eq!(qwen.cache_size_bytes, 65_500_000_000);
        assert_eq!(qwen.max_context, Some(131_072));
        // No entry for this mesh: not supported, engines fall back to the model's.
        let whisper = &cat.models[1];
        assert!(!whisper.supported);
        assert_eq!(whisper.status, None);
        assert_eq!(whisper.engines, vec!["media"]);
        assert_eq!(whisper.param_count, None);
        assert_eq!(whisper.max_context, None);
    }

    #[test]
    fn catalog_find_matches_name_or_repo() {
        let cat = parse_catalog(SAMPLE, Some("P300X2")).unwrap();
        assert_eq!(cat.find("qwen3-32b").unwrap().name, "Qwen3-32B");
        assert_eq!(cat.find("Qwen/Qwen3-32B").unwrap().name, "Qwen3-32B");
        assert!(cat.find("banana").is_none());
    }

    #[test]
    fn hf_repo_dir_follows_the_hub_layout() {
        assert_eq!(
            hf_repo_dir(Path::new("/h/.cache/huggingface"), "Qwen/Qwen3-32B"),
            PathBuf::from("/h/.cache/huggingface/hub/models--Qwen--Qwen3-32B")
        );
        assert_eq!(
            hf_repo_dir(Path::new("/h/hub"), "a/b"),
            PathBuf::from("/h/hub/models--a--b")
        );
    }

    /// A throwaway HF cache with one repo laid out the way huggingface_hub does:
    /// blobs, plus a snapshot of symlinks into them.
    fn fake_repo(files: &[(&str, &str)], blob_extra: &[&str]) -> (PathBuf, CatalogModel) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "ttcli-weights-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let repo = root.join("hub/models--org--M");
        let snap = repo.join("snapshots/rev1");
        std::fs::create_dir_all(repo.join("blobs")).unwrap();
        for (i, (name, body)) in files.iter().enumerate() {
            let blob = repo.join("blobs").join(format!("b{i}"));
            std::fs::write(&blob, body).unwrap();
            let link = snap.join(name);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&blob, &link).unwrap();
        }
        for extra in blob_extra {
            std::fs::write(repo.join("blobs").join(extra), "x").unwrap();
        }
        let model = parse_catalog(SAMPLE, None).unwrap().models[0].clone();
        (
            root,
            CatalogModel {
                hf_repo: Some("org/M".into()),
                ..model
            },
        )
    }

    #[test]
    fn weights_complete_when_every_indexed_shard_is_present() {
        let index = r#"{"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors"}}"#;
        let (root, m) = fake_repo(
            &[
                ("model.safetensors.index.json", index),
                ("model-1.safetensors", "w"),
                ("model-2.safetensors", "w"),
            ],
            &[],
        );
        assert_eq!(weights_state(&root, &m).0, Weights::Complete);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn weights_partial_when_a_shard_is_missing_or_still_downloading() {
        let index = r#"{"weight_map": {"a": "model-1.safetensors", "b": "model-2.safetensors"}}"#;
        let (root, m) = fake_repo(
            &[
                ("model.safetensors.index.json", index),
                ("model-1.safetensors", "w"),
            ],
            &[],
        );
        assert_eq!(weights_state(&root, &m).0, Weights::Partial);
        let _ = std::fs::remove_dir_all(root);

        let (root, m) = fake_repo(&[("model.safetensors", "w")], &["deadbeef.incomplete"]);
        assert_eq!(weights_state(&root, &m).0, Weights::Partial);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn stale_incomplete_leftovers_do_not_make_a_complete_repo_partial() {
        let (root, m) = fake_repo(&[("model.safetensors", "w")], &["old.incomplete"]);
        let stale = root.join("hub/models--org--M/blobs/old.incomplete");
        let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(an_hour_ago)
            .unwrap();
        assert_eq!(weights_state(&root, &m).0, Weights::Complete);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn weights_partial_for_a_config_only_stub_and_missing_when_absent() {
        let (root, m) = fake_repo(
            &[("config.json", "{}"), ("transformer/config.json", "{}")],
            &[],
        );
        assert_eq!(weights_state(&root, &m).0, Weights::Partial);
        let other = CatalogModel {
            hf_repo: Some("org/Absent".into()),
            ..m.clone()
        };
        assert_eq!(weights_state(&root, &other), (Weights::Missing, 0));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn container_fetched_types_need_no_download() {
        let cat = parse_catalog(SAMPLE, None).unwrap();
        assert_eq!(
            weights_state(Path::new("/nonexistent"), &cat.models[1]).0,
            Weights::Container
        );
        assert!(
            Weights::Container.ready() && Weights::Complete.ready() && !Weights::Partial.ready()
        );
    }

    #[test]
    fn gated_pull_failures_read_as_a_sentence_with_the_license_link() {
        let log = r#"{
  "error": {
    "what": "Model FLUX.1-schnell is gated on HuggingFace.",
    "why": "403 Client Error.",
    "next_step": "Accept the license at https://huggingface.co/black-forest-labs/FLUX.1-schnell and log in with `hf auth login`.",
    "exit_code": 1
  }
}"#;
        let (message, url) = explain_pull_failure(log);
        let message = message.unwrap();
        assert!(
            message.starts_with("Model FLUX.1-schnell is gated"),
            "{message}"
        );
        assert!(message.contains("Accept its license"), "{message}");
        assert_eq!(
            url.as_deref(),
            Some("https://huggingface.co/black-forest-labs/FLUX.1-schnell")
        );
        assert_eq!(explain_pull_failure("Traceback: boom"), (None, None));
    }

    #[test]
    fn the_catalog_is_listed_once_a_minute_and_again_after_a_pull_or_removal() {
        let dir = std::env::temp_dir().join(format!("tt-catalog-ttl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (counter, catalog) = (dir.join("runs"), dir.join("catalog.json"));
        std::fs::write(&catalog, SAMPLE).unwrap();
        let script = dir.join("tt.sh");
        std::fs::write(
            &script,
            format!(
                "case \"$*\" in\n  *\"model list\"*) printf x >> '{}'; cat '{}';;\n  *\"model pull\"*) sleep 0.3;;\nesac\n",
                counter.display(),
                catalog.display()
            ),
        )
        .unwrap();
        let tt = TtCli::new(
            vec!["sh".into(), script.display().to_string()],
            dir.join("hf"),
        )
        .with_log_dir(dir.join("logs"));
        let runs = || std::fs::read(&counter).map(|v| v.len()).unwrap_or(0);
        for _ in 0..5 {
            tt.catalog(Some("p300x2"), false).unwrap();
        }
        assert_eq!(runs(), 1, "reused within the minute");
        tt.invalidate_catalog();
        tt.catalog(Some("p300x2"), false).unwrap();
        assert_eq!(runs(), 2, "listed again after a removal");
        // A pull that finishes drops it too: what tt lists as downloaded changed.
        let model = tt.catalog(Some("p300x2"), false).unwrap().models[0].clone();
        tt.start_pull(&model).unwrap();
        tt.catalog(Some("p300x2"), false).unwrap();
        assert_eq!(runs(), 3, "listed again when the pull started");
        let t0 = Instant::now();
        while tt.is_pulling(&model.name) && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(50));
        }
        tt.catalog(Some("p300x2"), false).unwrap();
        assert_eq!(runs(), 4, "and again when it finished");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_lines_skips_blank_lines() {
        assert_eq!(last_lines("a\n\nb\nc\n\n", 2), "b\nc");
        assert_eq!(last_lines("", 3), "");
    }
}
