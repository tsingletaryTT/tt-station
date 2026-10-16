// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Orchard: bring up a new Hugging Face model on this box, with this box.
//!
//! [tt-orchard](https://github.com/tsingletaryTT/tt-orchard) takes a model id,
//! works out how it differs from a model that already runs on Blackhole,
//! writes and runs the tests, brings it up on 1, 2 and 4 chips against a CPU
//! reference, and stops at a review bundle (results, risks, a model card and
//! the publish commands as text). Its agents are open models served on the
//! same box, so nothing leaves it. A run takes hours; this module lets the
//! phone start one, watch it, pause or stop it, and read the harvest.
//!
//! - `GET /orchard`: is tt-orchard checked out, its setup checks, its config,
//!   and every run.
//! - `POST /orchard/setup`: run tt-orchard's own setup (`setup_machine --yes`),
//!   cloning the checkout first if it's missing.
//! - `POST /orchard/preflight {model}`: tt-orchard's preflight for a model:
//!   can it start, how big is the download, what blocks it.
//! - `POST /orchard/runs {model, stop_serving}`: start (or resume) a bring-up
//!   as its own systemd user unit, so it outlives agent restarts.
//! - `GET /orchard/runs/{name}`, `POST /orchard/runs/{name}/{pause|resume|abort|retry}`,
//!   `GET /orchard/runs/{name}/report/{results|risks|card|publish|blocked|console}`.
//!
//! While a run is live the chips are tt-orchard's: model starts, chip resets,
//! power actions and tt-smi wait (`AppState::chips_holder`), and its
//! containers aren't strays to the doctor. A watcher sends a notification as
//! each stage passes or fails and when the run ends, then starts the model
//! that was serving before.
//!
//! Status comes from tt-orchard itself (`tt-orchard status --json`, a
//! read-only view of the run's ledger), and setup and preflight from its own
//! Python (`orchard_bridge.py`), so this module never re-implements its rules.
//!
//! Everything here that runs something (setup, preflight, runs and their
//! controls, serving a harvest, packaging) is an owner's (`routes::OwnerAuth`):
//! tt-orchard and its agents run as the box's own account, outside any sandbox.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::notify::Draft;
use crate::routes::{AppState, BearerAuth, Phase};

const BRIDGE: &str = include_str!("orchard_bridge.py");
const REPO: &str = "https://github.com/tsingletaryTT/tt-orchard.git";
const SETUP_UNIT: &str = "tt-orchard-setup";

/// tt-orchard's own names for its stages (`orchard/lexicon.py`).
pub const STAGES: [(&str, &str); 9] = [
    ("🔭", "survey"),
    ("🧪", "soil test"),
    ("🌱", "graft"),
    ("🌳", "trunk"),
    ("🌿", "rows"),
    ("🚪", "farm gate"),
    ("🍯", "taste test"),
    ("📦", "crate"),
    ("🧺", "harvest"),
];

/// How a run was started, so the agent can follow it and put the box back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Launch {
    pub run_name: String,
    pub model: String,
    /// The systemd user unit, or `None` when started as a plain child.
    pub unit: Option<String>,
    pub pid: Option<u32>,
    pub log: PathBuf,
    pub started: u64,
    /// The model to serve again when the run ends.
    pub restore: Option<String>,
}

/// A download's recent bytes-on-disk readings.
type Sample = std::collections::VecDeque<(Instant, u64)>;

/// One run as the watcher last saw it.
#[derive(Debug, Clone)]
struct Seen {
    ledger_mtime: Option<SystemTime>,
    status: Option<Value>,
}

pub struct Orchard {
    pub checkout: PathBuf,
    pub python: PathBuf,
    /// `<config dir>/orchard`: launches and console logs.
    dir: PathBuf,
    /// Start runs as systemd user units (the default) or as plain children.
    systemd: bool,
    poll: Duration,
    info: Mutex<Option<(Instant, Value)>>,
    seen: Mutex<HashMap<String, Seen>>,
    primed: AtomicBool,
    /// The Hub, for download sizes (`None` offline, in tests).
    hub: Option<String>,
    /// Repo sizes from the Hub: `None` when the lookup failed (retried after 5 min).
    totals: Mutex<HashMap<String, (Instant, Option<u64>)>>,
    /// Last bytes-on-disk sample per download, for its speed.
    samples: Mutex<HashMap<PathBuf, Sample>>,
    /// How long Stop waits for the supervisor before SIGTERM.
    abort_grace: Duration,
    /// How long the box stays quiet after a run before the old model starts.
    settle: Duration,
    /// The tt-smi gozer resets with (`GOZER_RESET_CMD`), when not the one on PATH.
    tt_smi: Option<String>,
}

impl std::fmt::Debug for Orchard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Orchard")
            .field("checkout", &self.checkout)
            .finish()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `org/name`, as tt-orchard accepts it.
pub fn valid_model(model: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    matches!(model.split_once('/'), Some((org, name)) if part(org) && part(name))
}

/// A run directory's name: tt-orchard's slug, `org--name` in lower case.
pub fn valid_run_name(name: &str) -> bool {
    name.split_once("--").is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && !name.contains("..")
            && name.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
            })
    })
}

/// `Cloudflare/clef` → `cloudflare--clef` (tt-orchard's `bringup_config.slug`).
pub fn run_name(model: &str) -> String {
    model.to_lowercase().replace('/', "--")
}

fn unit_name(run_name: &str) -> String {
    format!("tt-orchard-{run_name}")
}

fn pid_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(')').map(|(_, rest)| rest.trim().to_string()))
        .is_some_and(|rest| !rest.starts_with('Z'))
}

/// The last `n` lines of a file.
pub fn tail(path: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read(path) else {
        return vec![];
    };
    let text = String::from_utf8_lossy(&text);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

impl Orchard {
    pub fn new(
        checkout: impl Into<PathBuf>,
        python: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
    ) -> Orchard {
        Orchard {
            checkout: checkout.into(),
            python: python.into(),
            dir: dir.into(),
            systemd: true,
            poll: Duration::from_secs(15),
            info: Mutex::new(None),
            seen: Mutex::new(HashMap::new()),
            primed: AtomicBool::new(false),
            hub: Some("https://huggingface.co".into()),
            totals: Mutex::new(HashMap::new()),
            samples: Mutex::new(HashMap::new()),
            abort_grace: Duration::from_secs(30),
            settle: Duration::from_secs(30),
            tt_smi: None,
        }
    }

    /// `~/code/tt-orchard` (or `$TT_ORCHARD_DIR`), run with Python 3.12 (or
    /// `$TT_ORCHARD_PYTHON`) -- tt-orchard needs 3.12 or newer.
    pub fn from_env(config_dir: &Path) -> Orchard {
        let home = std::env::var("HOME").unwrap_or_default();
        let checkout =
            std::env::var("TT_ORCHARD_DIR").unwrap_or_else(|_| format!("{home}/code/tt-orchard"));
        let python = std::env::var("TT_ORCHARD_PYTHON")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let path = std::env::var("PATH").unwrap_or_default();
                std::env::split_paths(&path)
                    .chain([PathBuf::from(format!("{home}/.local/bin"))])
                    .map(|d| d.join("python3.12"))
                    .find(|p| p.is_file())
                    .unwrap_or_else(|| PathBuf::from("python3"))
            });
        let systemd = Command::new("systemd-run")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        let mut o = Orchard::new(checkout, python, config_dir.join("orchard"));
        o.systemd = systemd;
        o
    }

    /// Start runs as plain child processes instead of systemd units (tests,
    /// and boxes without a systemd user manager).
    pub fn without_systemd(mut self) -> Orchard {
        self.systemd = false;
        self
    }

    pub fn poll_every(mut self, every: Duration) -> Orchard {
        self.poll = every;
        self
    }

    /// The tt-smi for gozer's resets in a run. tt-smi 3.x rejects the PCI
    /// addresses gozer resets by, and it can come first on PATH (it ships
    /// with tt-inference-server), so a run's resets silently didn't happen.
    pub fn with_tt_smi(mut self, tt_smi: &str) -> Orchard {
        self.tt_smi = (tt_smi != "tt-smi").then(|| tt_smi.to_string());
        self
    }

    pub fn settle_after_run(mut self, settle: Duration) -> Orchard {
        self.settle = settle;
        self
    }

    pub fn abort_grace(mut self, grace: Duration) -> Orchard {
        self.abort_grace = grace;
        self
    }

    /// Never ask the Hub for download sizes (tests).
    pub fn offline(mut self) -> Orchard {
        self.hub = None;
        self
    }

    /// A repo's size on the Hub, summed over its files.
    fn hub_total(&self, repo: &str, revision: Option<&str>) -> Option<u64> {
        let key = format!("{repo}@{}", revision.unwrap_or("main"));
        if let Some((at, total)) = self.totals.lock().expect("orchard totals").get(&key) {
            if total.is_some() || at.elapsed() < Duration::from_secs(300) {
                return *total;
            }
        }
        let hub = self.hub.as_ref()?;
        let url = match revision {
            Some(rev) => format!("{hub}/api/models/{repo}/revision/{rev}?blobs=true"),
            None => format!("{hub}/api/models/{repo}?blobs=true"),
        };
        let total = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()
            .and_then(|c| c.get(url).send().ok())
            .filter(|r| r.status().is_success())
            .and_then(|r| r.json::<Value>().ok())
            .and_then(|v| {
                v["siblings"]
                    .as_array()
                    .map(|files| files.iter().filter_map(|f| f["size"].as_u64()).sum::<u64>())
            })
            .filter(|t| *t > 0);
        self.totals
            .lock()
            .expect("orchard totals")
            .insert(key, (Instant::now(), total));
        total
    }

    /// A Hugging Face download: bytes on disk now, its size, and its speed.
    fn download(
        &self,
        label: &str,
        hf_home: &Path,
        repo: &str,
        revision: Option<&str>,
    ) -> crate::orchard_progress::Download {
        let dir = hf_home
            .join("hub")
            .join(format!("models--{}", repo.replace('/', "--")));
        let done = crate::orchard_progress::dir_bytes(&dir);
        let now = Instant::now();
        // Hugging Face writes in bursts: average over the last 90 s.
        let rate = {
            let mut samples = self.samples.lock().expect("orchard samples");
            let history = samples.entry(dir).or_default();
            history.retain(|(at, _)| now.duration_since(*at) < Duration::from_secs(90));
            history.push_back((now, done));
            let (first_at, first) = history.front().copied().unwrap_or((now, done));
            let span = now.duration_since(first_at).as_secs_f64();
            (span >= 10.0).then(|| done.saturating_sub(first) as f64 / span)
        };
        crate::orchard_progress::Download {
            label: label.to_string(),
            done,
            total: self.hub_total(repo, revision),
            rate,
        }
    }

    /// The agents' model's weights repo, from its tt-model manifest.
    fn coder_weights(&self, target: &str) -> Option<(String, Option<String>)> {
        let home = std::env::var("HOME").ok()?;
        let manifest = PathBuf::from(home)
            .join(".cache/tt-model/pulled")
            .join(target.replace('/', "__"))
            .join("tt_kernel_manifest.json");
        let v: Value = serde_json::from_slice(&std::fs::read(manifest).ok()?).ok()?;
        Some((
            v["weights"]["repo_id"].as_str()?.to_string(),
            v["weights"]["revision"].as_str().map(str::to_string),
        ))
    }

    /// Add the progress card (`orchard_progress`) to a run summary.
    fn attach_progress(&self, run: &mut Value) {
        let info = self.info_blocking(false);
        let Some(name) = run["run_name"].as_str().map(str::to_string) else {
            return;
        };
        let Some(root) = info["config"]["runs_root"].as_str().map(PathBuf::from) else {
            return;
        };
        let dir = root.join(&name);
        let facts = crate::orchard_progress::facts(&crate::orchard_progress::read_ledger(
            &dir.join("ledger.jsonl"),
        ));
        let state = run["state"].as_str().unwrap_or("").to_string();
        let live = run["live"].as_bool() == Some(true);
        let hf_home = info["config"]["hf_home"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/huggingface")
            });
        let model = run["model"].as_str().map(str::to_string);
        let model_download = model
            .as_deref()
            .filter(|_| live)
            .map(|m| self.download(short(m), &hf_home, m, None));
        let coder_download = (live && facts.coder_starting.is_some() && !facts.coder_ready)
            .then(|| {
                info["config"]["coder"]["target"]
                    .as_str()
                    .and_then(|t| self.coder_weights(t))
            })
            .flatten()
            .map(|(repo, rev)| self.download(short(&repo), &hf_home, &repo, rev.as_deref()));
        let turns = facts.open_stage.and_then(|n| {
            let since = facts.stage_started.get(&n).copied().unwrap_or(0.0);
            crate::orchard_progress::agent_turns(&dir.join("stages").join(n.to_string()), since)
        });
        let stages = run["stages"].as_array().cloned().unwrap_or_default();
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let p = crate::orchard_progress::progress(&crate::orchard_progress::Inputs {
            state: &state,
            now,
            facts: &facts,
            stages: &stages,
            model_download,
            coder_download,
            turns,
            blocked_reason: run["blocked"]["reason"].as_str(),
            pause_reason: run["pause"]["reason"].as_str(),
        });
        run["progress"] = p;
    }

    pub fn installed(&self) -> bool {
        self.checkout.join("bin").join("tt-orchard").is_file()
    }

    fn launches_path(&self) -> PathBuf {
        self.dir.join("launches.json")
    }

    pub fn launches(&self) -> Vec<Launch> {
        std::fs::read(self.launches_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save_launches(&self, launches: &[Launch]) {
        let _ = std::fs::create_dir_all(&self.dir);
        if let Ok(bytes) = serde_json::to_vec_pretty(launches) {
            let tmp = self.launches_path().with_extension("tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(tmp, self.launches_path());
            }
        }
    }

    fn launch_for(&self, run_name: &str) -> Option<Launch> {
        self.launches().into_iter().find(|l| l.run_name == run_name)
    }

    pub(crate) fn log_path(&self, name: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{name}.log"))
    }

    /// Run the bridge: one JSON object back.
    pub(crate) fn bridge(&self, args: &[&str]) -> anyhow::Result<Value> {
        let out = Command::new(&self.python)
            .arg("-c")
            .arg(BRIDGE)
            .arg(&self.checkout)
            .args(args)
            .current_dir(&self.checkout)
            .env("NO_COLOR", "1")
            .env("ORCHARD_PLAIN", "1")
            .stdin(Stdio::null())
            .output()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().rev().find(|l| l.trim_start().starts_with('{'));
        match line.and_then(|l| serde_json::from_str::<Value>(l).ok()) {
            Some(v) => Ok(v),
            None => anyhow::bail!(
                "tt-orchard didn't answer: {}",
                String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .last()
                    .unwrap_or("")
            ),
        }
    }

    /// Setup checks and config, cached for a minute.
    pub fn info_blocking(&self, fresh: bool) -> Value {
        if !fresh {
            if let Some((at, v)) = self.info.lock().expect("orchard info").as_ref() {
                if at.elapsed() < Duration::from_secs(60) {
                    return v.clone();
                }
            }
        }
        let v = self
            .bridge(&["info"])
            .unwrap_or_else(|e| json!({ "error": e.to_string() }));
        *self.info.lock().expect("orchard info") = Some((Instant::now(), v.clone()));
        v
    }

    fn runs_root(&self) -> Option<PathBuf> {
        self.info_blocking(false)["config"]["runs_root"]
            .as_str()
            .map(PathBuf::from)
    }

    pub fn preflight_blocking(&self, model: &str, opts: &RunOptions) -> anyhow::Result<Value> {
        let mut args = vec!["preflight", model];
        if opts.refuse_credentials {
            args.push("--refuse-credentials");
        }
        if let Some(base) = &opts.base {
            args.extend(["--base", base.as_str()]);
        }
        let v = self.bridge(&args)?;
        if let Some(e) = v["error"].as_str() {
            anyhow::bail!("{e}");
        }
        Ok(v)
    }

    /// tt-orchard's own status of a run (`status --json`), or `None` when the
    /// run has no ledger yet.
    pub fn status_blocking(&self, run_dir: &Path) -> Option<Value> {
        if !run_dir.join("ledger.jsonl").is_file() {
            return None;
        }
        let out = Command::new(&self.python)
            .arg(self.checkout.join("bin").join("tt-orchard"))
            .args(["status", "--json", "--run-dir"])
            .arg(run_dir)
            .current_dir(&self.checkout)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        serde_json::from_slice(&out.stdout).ok()
    }

    fn control_blocking(&self, run_dir: &Path, word: &str) -> anyhow::Result<()> {
        let out = Command::new(&self.python)
            .arg(self.checkout.join("bin").join("tt-orchard"))
            .arg(word)
            .arg("--run-dir")
            .arg(run_dir)
            .current_dir(&self.checkout)
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "{}",
                String::from_utf8_lossy(&out.stderr)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("tt-orchard refused")
            );
        }
        Ok(())
    }

    /// systemd user units named `tt-orchard-*` that are running.
    fn active_units(&self) -> Vec<String> {
        if !self.systemd {
            return vec![];
        }
        let out = Command::new("systemctl")
            .args([
                "--user",
                "list-units",
                "tt-orchard-*",
                "--state=active,activating,reloading",
                "--no-legend",
                "--plain",
            ])
            .stdin(Stdio::null())
            .output();
        let Ok(out) = out else { return vec![] };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(|u| u.trim_end_matches(".service").to_string())
            .collect()
    }

    fn launch_active(launch: &Launch, units: &[String]) -> bool {
        match (&launch.unit, launch.pid) {
            (Some(unit), _) => units.contains(unit),
            (None, Some(pid)) => pid_alive(pid),
            _ => false,
        }
    }

    /// Start a process the way runs are started: a systemd user unit with its
    /// output appended to `log`, or a detached child.
    pub(crate) fn spawn(
        &self,
        unit: &str,
        log: &Path,
        argv: &[String],
    ) -> anyhow::Result<(Option<String>, Option<u32>)> {
        std::fs::create_dir_all(log.parent().unwrap_or(&self.dir))?;
        let home = std::env::var("HOME").unwrap_or_default();
        let mut path: Vec<PathBuf> =
            std::env::split_paths(&std::env::var("PATH").unwrap_or_default()).collect();
        for extra in [
            format!("{home}/.local/bin"),
            format!("{home}/.tenstorrent-venv/bin"),
        ] {
            let extra = PathBuf::from(extra);
            if !path.contains(&extra) {
                path.push(extra);
            }
        }
        let path = std::env::join_paths(path)?;
        if self.systemd {
            let status = Command::new("systemd-run")
                .args(["--user", "--collect", "--quiet"])
                .arg(format!("--unit={unit}"))
                .arg(format!(
                    "--property=WorkingDirectory={}",
                    self.checkout.display()
                ))
                .arg(format!(
                    "--property=StandardOutput=append:{}",
                    log.display()
                ))
                .arg(format!("--property=StandardError=append:{}", log.display()))
                .arg(format!("--setenv=PATH={}", path.to_string_lossy()))
                .arg(format!("--setenv=HOME={home}"))
                .arg("--setenv=NO_COLOR=1")
                .arg("--setenv=PYTHONUNBUFFERED=1")
                .args(
                    self.tt_smi
                        .iter()
                        .map(|t| format!("--setenv=GOZER_RESET_CMD={t}")),
                )
                .arg("--setenv=ORCHARD_PLAIN=1")
                .args(argv)
                .stdin(Stdio::null())
                .output()?;
            if !status.status.success() {
                anyhow::bail!(
                    "couldn't start {unit}: {}",
                    String::from_utf8_lossy(&status.stderr).trim()
                );
            }
            return Ok((Some(unit.to_string()), None));
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        let mut child = Command::new(&argv[0]);
        child
            .args(&argv[1..])
            .current_dir(if self.checkout.is_dir() {
                self.checkout.clone()
            } else {
                self.dir.clone()
            })
            .env("PATH", path)
            .env("PYTHONUNBUFFERED", "1")
            .envs(self.tt_smi.iter().map(|t| ("GOZER_RESET_CMD", t.as_str())))
            .env("NO_COLOR", "1")
            .env("ORCHARD_PLAIN", "1")
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            child.process_group(0);
        }
        let mut child = child.spawn()?;
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok((None, Some(pid)))
    }

    /// Start (or resume) a bring-up. tt-orchard runs its preflight again and
    /// refuses on its own if something changed.
    pub fn launch_blocking(
        &self,
        model: &str,
        opts: &RunOptions,
        restore: Option<String>,
    ) -> anyhow::Result<Launch> {
        let name = run_name(model);
        let log = self.log_path(&name);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .and_then(|mut f| {
                use std::io::Write;
                writeln!(f, "--- tt-station: bringup {model} at {} ---", unix_now())
            });
        let mut argv = vec![
            self.python.display().to_string(),
            self.checkout
                .join("bin")
                .join("tt-orchard")
                .display()
                .to_string(),
            "bringup".to_string(),
            model.to_string(),
        ];
        // Credentials visible to agent shells are accepted by default (tt-orchard's own default).
        if opts.refuse_credentials {
            argv.push("--refuse-credentials-visible".into());
        }
        if let Some(base) = &opts.base {
            argv.extend(["--base".into(), base.clone()]);
        }
        let (unit, pid) = self.spawn(&unit_name(&name), &log, &argv)?;
        let launch = Launch {
            run_name: name.clone(),
            model: model.to_string(),
            unit,
            pid,
            log,
            started: unix_now(),
            restore,
        };
        let mut all: Vec<Launch> = self
            .launches()
            .into_iter()
            .filter(|l| l.run_name != name)
            .collect();
        all.insert(0, launch.clone());
        self.save_launches(&all);
        Ok(launch)
    }

    pub fn setup_blocking(&self) -> anyhow::Result<()> {
        let script = "test -d \"$1/.git\" || git clone --quiet \"$3\" \"$1\" || exit 1; \
                      cd \"$1\" && exec \"$2\" -m orchard.setup_machine --yes";
        let argv: Vec<String> = vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
            "sh".into(),
            self.checkout.display().to_string(),
            self.python.display().to_string(),
            REPO.into(),
        ];
        let log = self.log_path("setup");
        let _ = std::fs::remove_file(&log);
        self.spawn(SETUP_UNIT, &log, &argv)?;
        *self.info.lock().expect("orchard info") = None;
        Ok(())
    }

    fn setup_running(&self, units: &[String]) -> bool {
        units.iter().any(|u| u == SETUP_UNIT)
    }

    /// Every run: run directories under `runs_root`, plus launches that
    /// haven't written a ledger yet. Re-reads a run's status only when its
    /// ledger changed or it's live.
    fn scan(&self) -> Scan {
        let units = self.active_units();
        let launches = self.launches();
        let mut names: Vec<String> = launches.iter().map(|l| l.run_name.clone()).collect();
        let root = self.runs_root();
        if let Some(root) = &root {
            if let Ok(rd) = std::fs::read_dir(root) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if valid_run_name(&name)
                        && e.path().join("ledger.jsonl").is_file()
                        && !names.contains(&name)
                    {
                        names.push(name);
                    }
                }
            }
        }
        let mut runs = vec![];
        for name in names {
            let launch = launches.iter().find(|l| l.run_name == name);
            let active = launch.is_some_and(|l| Orchard::launch_active(l, &units));
            let status = match &root {
                Some(root) => {
                    let dir = root.join(&name);
                    let mtime = std::fs::metadata(dir.join("ledger.jsonl"))
                        .and_then(|m| m.modified())
                        .ok();
                    let cached = self.seen.lock().expect("orchard seen").get(&name).cloned();
                    let live = cached
                        .as_ref()
                        .and_then(|c| c.status.as_ref())
                        .is_some_and(|s| s["supervisor"]["alive"].as_bool() == Some(true));
                    match cached {
                        Some(c) if c.ledger_mtime == mtime && !live && !active => c.status,
                        _ => {
                            let status = self.status_blocking(&dir);
                            self.seen.lock().expect("orchard seen").insert(
                                name.clone(),
                                Seen {
                                    ledger_mtime: mtime,
                                    status: status.clone(),
                                },
                            );
                            status
                        }
                    }
                }
                None => None,
            };
            let mut run = summary(&name, status.as_ref(), launch, active);
            self.attach_progress(&mut run);
            runs.push(run);
        }
        runs.sort_by_key(|r| std::cmp::Reverse(r["started"].as_u64().unwrap_or(0)));
        Scan { units, runs }
    }
}

struct Scan {
    units: Vec<String>,
    runs: Vec<Value>,
}

/// A run for the phone: tt-orchard's status plus how this agent started it.
pub fn summary(
    name: &str,
    status: Option<&Value>,
    launch: Option<&Launch>,
    unit_active: bool,
) -> Value {
    let alive = status.is_some_and(|s| s["supervisor"]["alive"].as_bool() == Some(true));
    let state = match status.and_then(|s| s["state"].as_str()) {
        // Between `tt-orchard bringup` starting and its supervisor taking the
        // ledger: the preflight and the download.
        _ if unit_active && !alive => "preparing",
        Some(s) => s,
        None if launch.is_some() => "refused",
        None => "not-started",
    };
    let started = status
        .and_then(|s| s["last_events"][0]["ts"].as_f64())
        .map(|t| t as u64)
        .into_iter()
        .chain(launch.map(|l| l.started))
        .max()
        .unwrap_or(0);
    json!({
        "run_name": name,
        "model": status.and_then(|s| s["model"].as_str()).or(launch.map(|l| l.model.as_str())),
        "state": state,
        "live": alive || unit_active,
        "supervisor_alive": alive,
        "stage": status.map(|s| s["stage"].clone()).unwrap_or(Value::Null),
        "stages": status.map(|s| s["stages"].clone()).unwrap_or(json!([])),
        "counts": status.map(|s| s["counts"].clone()).unwrap_or(Value::Null),
        "blocked": status.map(|s| s["blocked"].clone()).unwrap_or(Value::Null),
        "pause": status.map(|s| s["pause"].clone()).unwrap_or(Value::Null),
        "hint": status.map(|s| s["hint"].clone()).unwrap_or(Value::Null),
        "disk": status.map(|s| s["disk"].clone()).unwrap_or(Value::Null),
        "last_events": status.map(|s| s["last_events"].clone()).unwrap_or(json!([])),
        "started": started,
        "restore": launch.and_then(|l| l.restore.clone()),
    })
}

fn short(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn stage_title(n: u64) -> String {
    STAGES
        .get(n as usize)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| format!("stage {n}"))
}

/// Notifications for what changed in a run between two summaries.
pub fn changes(prev: Option<&Value>, now: &Value) -> Vec<Draft> {
    let model = now["model"].as_str().unwrap_or("the model").to_string();
    let who = short(&model).to_string();
    let mut out = vec![];
    let before = |n: u64| -> Option<String> {
        prev?["stages"]
            .as_array()?
            .iter()
            .find(|s| s["stage"].as_u64() == Some(n))
            .and_then(|s| s["status"].as_str())
            .map(str::to_string)
    };
    for s in now["stages"].as_array().into_iter().flatten() {
        let (Some(n), Some(status)) = (s["stage"].as_u64(), s["status"].as_str()) else {
            continue;
        };
        if before(n).as_deref() == Some(status) {
            continue;
        }
        let real = s["name"].as_str().unwrap_or("");
        let mins = s["wall_s"].as_f64().map(|w| (w / 60.0).round() as u64);
        let took = mins.map(|m| format!(" after {m} min")).unwrap_or_default();
        match status {
            "pass" => out.push(Draft {
                kind: "orchard",
                title: format!(
                    "{} {}: {} passed",
                    STAGES.get(n as usize).map(|s| s.0).unwrap_or("✅"),
                    who,
                    stage_title(n)
                ),
                body: format!("Stage {n} of 8 ({real}) passed{took}."),
            }),
            "fail" | "escalated" => out.push(Draft {
                kind: "orchard",
                title: format!(
                    "{who}: {} {}",
                    stage_title(n),
                    if status == "fail" {
                        "failed"
                    } else {
                        "escalated"
                    }
                ),
                body: format!(
                    "Stage {n} ({real}) {}{took}. Orchard is handing it to its diagnose model.",
                    if status == "fail" {
                        "failed"
                    } else {
                        "needed help"
                    }
                ),
            }),
            _ => {}
        }
    }
    let was = prev.and_then(|p| p["state"].as_str());
    let state = now["state"].as_str().unwrap_or("");
    if was != Some(state) {
        let draft = match state {
            "ready-for-operator-review" => Some(Draft {
                kind: "orchard_done",
                title: format!("🍎 {who} is ripe"),
                body: format!("The bring-up of {model} is ready for your review: results, risks and a model card. Nothing was published."),
            }),
            "blocked" => Some(Draft {
                kind: "orchard_done",
                title: format!("❄️ {who} bring-up stopped"),
                body: format!(
                    "{} The chips are free again; retry from Orchard.",
                    now["blocked"]["reason"].as_str().unwrap_or("Orchard couldn't go on.")
                ),
            }),
            "paused" => Some(Draft {
                kind: "orchard",
                title: format!("🌧 {who} bring-up paused"),
                body: now["pause"]["reason"].as_str().unwrap_or("It's waiting for you.").to_string(),
            }),
            "aborted" => Some(Draft {
                kind: "orchard_done",
                title: format!("🍂 {who} bring-up stopped"),
                body: "The run was aborted and the chips are free again.".into(),
            }),
            "stopped-or-crashed" if matches!(was, Some("running" | "paused" | "preparing")) => Some(Draft {
                kind: "orchard_done",
                title: format!("⛈ {who} bring-up stopped"),
                body: "It stopped on an error and released the chips. Retry from Orchard to resume where it left off.".into(),
            }),
            "refused" if was == Some("preparing") || was.is_none() && prev.is_some() => Some(Draft {
                kind: "orchard_done",
                title: format!("{who} bring-up didn't start"),
                body: "tt-orchard's preflight refused it. Open Orchard to see why.".into(),
            }),
            _ => None,
        };
        out.extend(draft);
    }
    out
}

/// Keep `orchard_busy` true while a run is live, tell the owner what changed,
/// and serve the old model again when a run ends.
pub async fn watch_forever(state: AppState) {
    let Some(orchard) = state.orchard() else {
        return;
    };
    let mut prev: HashMap<String, Value> = HashMap::new();
    let mut quiet_since: Option<Instant> = None;
    loop {
        let o = orchard.clone();
        let Ok(scan) = tokio::task::spawn_blocking(move || o.scan()).await else {
            tokio::time::sleep(orchard.poll).await;
            continue;
        };
        let live: Vec<&Value> = scan
            .runs
            .iter()
            .filter(|r| r["live"].as_bool() == Some(true))
            .collect();
        state.set_orchard_busy(!live.is_empty());
        state.set_orchard_chips(
            scan.runs
                .iter()
                .any(|r| r["supervisor_alive"].as_bool() == Some(true)),
        );

        let primed = orchard.primed.swap(true, Ordering::SeqCst);
        for run in &scan.runs {
            let name = run["run_name"].as_str().unwrap_or("").to_string();
            if primed {
                for draft in changes(prev.get(&name), run) {
                    state.notifier().emit(draft);
                }
            }
            prev.insert(name, run.clone());
        }

        // A run that ended: put back what was serving before it, once
        // tt-orchard has finished releasing (and resetting) the chips -- a
        // reset started while it still was failed live.
        quiet_since = if live.is_empty() {
            quiet_since.or(Some(Instant::now()))
        } else {
            None
        };
        if quiet_since.is_some_and(|q| q.elapsed() >= orchard.settle) {
            let mut launches = orchard.launches();
            if let Some(i) = launches
                .iter()
                .position(|l| l.restore.is_some() && !Orchard::launch_active(l, &scan.units))
            {
                let model = launches[i].restore.take().unwrap_or_default();
                orchard.save_launches(&launches);
                eprintln!("tt-station-agentd: orchard run ended; starting {model} again");
                if let Err((_, Json(e))) = crate::routes::begin_serve(&state, &model, false).await {
                    eprintln!("tt-station-agentd: couldn't restart {model}: {}", e.error);
                }
            }
        }
        tokio::time::sleep(orchard.poll).await;
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

type Reply = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub(crate) fn need(state: &AppState) -> Result<std::sync::Arc<Orchard>, (StatusCode, Json<Value>)> {
    let o = state.orchard().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "Orchard isn't enabled on this agent.",
        )
    })?;
    if !o.installed() {
        return Err(err(
            StatusCode::PRECONDITION_FAILED,
            "tt-orchard isn't set up on this box yet. Run Set up in Orchard.",
        ));
    }
    Ok(o)
}

pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, (StatusCode, Json<Value>)> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

pub(crate) async fn get_orchard(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> Reply {
    let Some(o) = state.orchard() else {
        return Err(err(
            StatusCode::NOT_FOUND,
            "Orchard isn't enabled on this agent.",
        ));
    };
    let fresh = q.contains_key("fresh");
    let gozer_in_agent = state.gozer().is_some();
    let o2 = o.clone();
    let (info, scan, setup_log) = blocking(move || {
        let installed = o2.installed();
        let info = if installed {
            o2.info_blocking(fresh)
        } else {
            json!({})
        };
        let scan = if installed {
            o2.scan()
        } else {
            Scan {
                units: o2.active_units(),
                runs: vec![],
            }
        };
        (info, scan, tail(&o2.log_path("setup"), 30))
    })
    .await?;
    let setup_running = o.setup_running(&scan.units);
    let setup = info["setup"].as_array().cloned().unwrap_or_default();
    let ready = !setup.is_empty()
        && setup
            .iter()
            .all(|s| s["status"] == "ok" || s["status"] == "warn")
        && info["config"].is_object();
    let gozer_ok = setup
        .iter()
        .any(|s| s["name"] == "tt-gozer" && s["status"] != "todo" && s["status"] != "fail");
    let mut notes = vec![];
    if gozer_ok && !gozer_in_agent {
        notes.push(json!({
            "text": "gozer is installed, but this agent started before it was. Restart the agent so serving shares the chips through gozer leases.",
            "command": "systemctl --user restart tt-station-agentd",
        }));
    }
    let coder_port = info["config"]["coder"]["port"].as_u64();
    if coder_port.is_some_and(|p| p == 8000) {
        notes.push(json!({
            "text": "Orchard's coder serves on port 8000, which TT-Studio's backend also uses. Don't run TT-Studio during a bring-up.",
        }));
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "installed": o.installed(),
            "checkout": o.checkout,
            "version": info["version"],
            "ready": ready,
            "setup": setup,
            "setup_error": info["setup_error"],
            "setup_running": setup_running,
            "setup_log": setup_log,
            "config": info["config"],
            "packaging": info["config"]["package_format"].is_string(),
            "config_error": info["config_error"],
            "busy": state.orchard_busy(),
            "runs": scan.runs,
            "notes": notes,
            "stages": STAGES.iter().map(|(e, n)| json!({ "emoji": e, "name": n })).collect::<Vec<_>>(),
        })),
    ))
}

pub(crate) async fn post_setup(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    let o = state.orchard().ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "Orchard isn't enabled on this agent.",
        )
    })?;
    let o2 = o.clone();
    let running = blocking(move || o2.setup_running(&o2.active_units())).await?;
    if running {
        return Err(err(StatusCode::CONFLICT, "Setup is already running."));
    }
    let o2 = o.clone();
    blocking(move || o2.setup_blocking())
        .await?
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    crate::audit::record(&state, "host_run", "Orchard setup", "ok");
    Ok((StatusCode::ACCEPTED, Json(json!({ "started": true }))))
}

#[derive(Debug, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    #[serde(default)]
    pub stop_serving: bool,
    /// Older clients; credentials are accepted by default now.
    #[serde(default)]
    pub accept_credentials_visible: bool,
    #[serde(default)]
    pub refuse_credentials_visible: bool,
    /// The model to base the run on (a tt-model bundle or a Hugging Face id);
    /// default: the model card's base_model.
    #[serde(default)]
    pub base: Option<String>,
}

/// How a run starts: tt-orchard's `bringup` options TT Station passes on.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub refuse_credentials: bool,
    pub base: Option<String>,
}

impl RunOptions {
    fn from_request(refuse: bool, base: Option<&str>) -> Result<Self, (StatusCode, Json<Value>)> {
        let base = base
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_string);
        if let Some(b) = &base {
            let ok = b.len() <= 160
                && b.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./:".contains(c))
                && !b.starts_with('-');
            if !ok {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "That isn't a model or bundle name.",
                ));
            }
        }
        Ok(Self {
            refuse_credentials: refuse,
            base,
        })
    }
}

pub(crate) async fn post_preflight(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<ModelRequest>,
) -> Reply {
    let o = need(&state)?;
    let model = req.model.trim().to_string();
    if !valid_model(&model) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Give a Hugging Face model id like org/name.",
        ));
    }
    let opts = RunOptions::from_request(req.refuse_credentials_visible, req.base.as_deref())?;
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        blocking(move || o.preflight_blocking(&model, &opts)),
    )
    .await
    .map_err(|_| err(StatusCode::GATEWAY_TIMEOUT, "The preflight took too long."))??
    .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    Ok((StatusCode::OK, Json(result)))
}

/// Start a run once the box is free; shared by `POST /orchard/runs` and retry.
async fn start(
    state: &AppState,
    o: std::sync::Arc<Orchard>,
    model: String,
    stop_serving: bool,
    opts: RunOptions,
) -> Reply {
    if state.lab_busy() {
        return Err(err(
            StatusCode::CONFLICT,
            "A silicon check is using the chips. Wait for it to finish.",
        ));
    }
    if state.bringup_busy() {
        return Err(err(
            StatusCode::CONFLICT,
            "A Codex bring-up is using the chips. One bring-up at a time.",
        ));
    }
    if let Some(jobs) = state.jobs() {
        if jobs.list().iter().any(|j| {
            matches!(
                j.status,
                crate::jobs::JobStatus::Running | crate::jobs::JobStatus::Switching
            )
        }) {
            return Err(err(
                StatusCode::CONFLICT,
                "A transcription job is running. Wait for it to finish.",
            ));
        }
    }
    let o2 = o.clone();
    let scan = blocking(move || o2.scan()).await?;
    if let Some(live) = scan.runs.iter().find(|r| r["live"].as_bool() == Some(true)) {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "{} is already being brought up. One bring-up at a time.",
                live["model"].as_str().unwrap_or("Another model")
            ),
        ));
    }
    let life = state.lifecycle();
    let split = crate::coserve::is_split(state);
    let serving = !matches!(life.phase, Phase::Idle | Phase::Failed) || split;
    let previous = life.model.clone().or(life.served_id.clone());
    if serving && !stop_serving {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "A bring-up needs all four chips. Stop {} first?",
                    previous.as_deref().map(crate::notify::display_model).unwrap_or_else(|| "the model".into())
                ),
                "needs_stop": true,
            })),
        ));
    }
    let restore = if serving {
        crate::coserve::leave_split(state).await;
        state.stop_serving().await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't stop the model: {e:#}"),
            )
        })?;
        previous.map(|m| crate::notify::display_model(&m))
    } else {
        None
    };
    // Fresh chips: a model that served on them last (TT-Metal) leaves them
    // in a state the next runtime can't open ("timed out while waiting for
    // active ethernet core … try resetting the board"; seen live when the
    // coder started after a Wan2.2 serve). tt-orchard's first coder start
    // doesn't reset them, so the agent does, as it does for Lab.
    let reset = state.clone();
    blocking(move || reset.run_power_command(crate::power::PowerAction::ResetChips, false))
        .await?
        .map_err(|e| {
            err(
                StatusCode::CONFLICT,
                format!("couldn't reset the chips before the run: {e:#}"),
            )
        })?;
    state.set_orchard_busy(true);
    let m = model.clone();
    let launched = blocking(move || o.launch_blocking(&m, &opts, restore)).await?;
    let launch = match launched {
        Ok(l) => l,
        Err(e) => {
            state.set_orchard_busy(false);
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")));
        }
    };
    state.notifier().record(Draft {
        kind: "orchard",
        title: format!("🌰 Bringing up {}", short(&model)),
        body: "Orchard is checking, downloading and starting the run. It takes hours; you'll get a notification at each stage.".into(),
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({ "run_name": launch.run_name, "model": launch.model, "restore": launch.restore }),
        ),
    ))
}

pub(crate) async fn post_run(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<ModelRequest>,
) -> Reply {
    let o = need(&state)?;
    let model = req.model.trim().to_string();
    if !valid_model(&model) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "Give a Hugging Face model id like org/name.",
        ));
    }
    let opts = RunOptions::from_request(req.refuse_credentials_visible, req.base.as_deref())?;
    let r = start(&state, o, model.clone(), req.stop_serving, opts).await;
    if r.is_ok() {
        crate::audit::record(
            &state,
            "host_run",
            &format!("Orchard bring-up of {model}"),
            "ok",
        );
    }
    r
}

fn run_dir(o: &Orchard, name: &str) -> Result<PathBuf, (StatusCode, Json<Value>)> {
    if !valid_run_name(name) {
        return Err(err(StatusCode::BAD_REQUEST, "No such run."));
    }
    let root = o.runs_root().ok_or_else(|| {
        err(
            StatusCode::PRECONDITION_FAILED,
            "tt-orchard has no bringup config yet.",
        )
    })?;
    Ok(root.join(name))
}

/// `DELETE /orchard/runs/{name}` (owners): remove a finished or stopped run's
/// folder (its stages, logs, and any harvest), and forget its launch.
pub(crate) async fn delete_run(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Reply {
    let o = need(&state)?;
    let o2 = o.clone();
    let n = name.clone();
    blocking(move || {
        let dir = run_dir(&o2, &n)?;
        let status = o2.status_blocking(&dir);
        let units = o2.active_units();
        let live = o2
            .launch_for(&n)
            .is_some_and(|l| Orchard::launch_active(&l, &units))
            || status
                .as_ref()
                .and_then(|s| s["supervisor"]["alive"].as_bool())
                == Some(true);
        if live {
            return Err(err(
                StatusCode::CONFLICT,
                "Stop the run before deleting it.",
            ));
        }
        let root = o2.runs_root().ok_or_else(|| {
            err(
                StatusCode::PRECONDITION_FAILED,
                "tt-orchard has no runs folder.",
            )
        })?;
        if dir.starts_with(&root) && dir != root && dir.is_dir() {
            std::fs::remove_dir_all(&dir).map_err(|e| {
                err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("couldn't remove it: {e}"),
                )
            })?;
        }
        let launches: Vec<Launch> = o2
            .launches()
            .into_iter()
            .filter(|l| l.run_name != n)
            .collect();
        o2.save_launches(&launches);
        Ok((StatusCode::OK, Json(json!({ "deleted": n }))))
    })
    .await?
}

pub(crate) async fn get_run(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Reply {
    let o = need(&state)?;
    let o2 = o.clone();
    let n = name.clone();
    let (run, console, reports, servable) = blocking(move || {
        let dir = run_dir(&o2, &n)?;
        let launch = o2.launch_for(&n);
        let units = o2.active_units();
        let active = launch
            .as_ref()
            .is_some_and(|l| Orchard::launch_active(l, &units));
        let status = o2.status_blocking(&dir);
        if status.is_none() && launch.is_none() {
            return Err(err(StatusCode::NOT_FOUND, "No such run."));
        }
        let mut run = summary(&n, status.as_ref(), launch.as_ref(), active);
        o2.attach_progress(&mut run);
        let console = tail(&o2.log_path(&n), 60);
        // Where each hardware test ran (this box or the lab) and the configs not run.
        run["hardware"] = crate::orchard_progress::hardware(&crate::orchard_progress::read_ledger(
            &dir.join("ledger.jsonl"),
        ));
        // tt-orchard says `refused: ...` when it won't start (a lab that changed, packaging with a lab, ...).
        run["refused"] = json!(console.iter().rev().find_map(|l| l
            .trim()
            .strip_prefix("refused:")
            .map(|r| r.trim().to_string())));
        let servable = harvest_dir(&dir).is_some();
        let reports: Vec<&str> = REPORTS
            .iter()
            .filter(|(_, file)| dir.join(file).is_file())
            .map(|(id, _)| *id)
            .chain((!console.is_empty()).then_some("console"))
            .collect();
        Ok((run, console, reports, servable))
    })
    .await??;
    let mut run = run;
    run["console"] = json!(console);
    run["reports"] = json!(reports);
    run["servable"] = json!(servable);
    Ok((StatusCode::OK, Json(run)))
}

/// `GET /orchard/runs/{name}/story`: the run told the way tt-orchard tells it
/// (`orchard_bridge.py story`): its status facts and `next:` hint, the
/// narration `tt-orchard watch` prints, what it's doing now, the graft stage 0
/// chose (class, and the model it builds on), and for a blocked run what was
/// tried and how to unblock it.
pub(crate) async fn get_story(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Reply {
    let o = need(&state)?;
    let v = blocking(move || {
        let dir = run_dir(&o, &name)?;
        if !dir.join("ledger.jsonl").is_file() {
            return Ok(json!({ "facts": null, "narration": [], "agent": [] }));
        }
        o.bridge(&["story", &dir.display().to_string()])
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))
    })
    .await??;
    if let Some(e) = v["error"].as_str() {
        return Err(err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
    }
    Ok((StatusCode::OK, Json(v)))
}

/// Files a person reads at the end of a run.
const REPORTS: [(&str, &str); 5] = [
    ("results", "stages/8/bundle/RESULTS.md"),
    ("risks", "stages/8/bundle/RISKS.md"),
    ("card", "stages/8/bundle/card.md"),
    ("publish", "stages/8/bundle/PUBLISH_COMMANDS.txt"),
    ("blocked", "BLOCKED.md"),
];

pub(crate) async fn get_report(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path((name, which)): axum::extract::Path<(String, String)>,
) -> Result<impl IntoResponse, (StatusCode, Json<Value>)> {
    let o = need(&state)?;
    let text = blocking(move || {
        let path = if which == "console" {
            if !valid_run_name(&name) {
                return Err(err(StatusCode::BAD_REQUEST, "No such run."));
            }
            o.log_path(&name)
        } else {
            let file = REPORTS
                .iter()
                .find(|(id, _)| *id == which)
                .map(|(_, f)| *f)
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such report."))?;
            run_dir(&o, &name)?.join(file)
        };
        std::fs::read(&path)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .map_err(|_| err(StatusCode::NOT_FOUND, "That report isn't written yet."))
    })
    .await??;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text))
}

#[derive(Debug, Default, Deserialize)]
pub struct ControlRequest {
    #[serde(default)]
    pub stop_serving: bool,
    #[serde(default)]
    pub accept_credentials_visible: bool,
    #[serde(default)]
    pub refuse_credentials_visible: bool,
    #[serde(default)]
    pub base: Option<String>,
}

pub(crate) async fn post_control(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Path((name, action)): axum::extract::Path<(String, String)>,
    body: Option<Json<ControlRequest>>,
) -> Reply {
    let o = need(&state)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let o2 = o.clone();
    let n = name.clone();
    let (dir, launch, active, status) = blocking(move || {
        let dir = run_dir(&o2, &n)?;
        let launch = o2.launch_for(&n);
        let units = o2.active_units();
        let active = launch
            .as_ref()
            .is_some_and(|l| Orchard::launch_active(l, &units));
        let status = o2.status_blocking(&dir);
        Ok::<_, (StatusCode, Json<Value>)>((dir, launch, active, status))
    })
    .await??;
    let alive = status
        .as_ref()
        .is_some_and(|s| s["supervisor"]["alive"].as_bool() == Some(true));
    match action.as_str() {
        "pause" | "resume" if alive => {
            let word = action.clone();
            blocking(move || o.control_blocking(&dir, &word))
                .await?
                .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
        }
        "pause" | "resume" => {
            return Err(err(
                StatusCode::CONFLICT,
                "That run isn't running. Use Retry to pick it up again.",
            ));
        }
        "abort" if alive => {
            let o2 = o.clone();
            let d = dir.clone();
            blocking(move || o2.control_blocking(&d, "abort"))
                .await?
                .map_err(|e| err(StatusCode::CONFLICT, format!("{e:#}")))?;
            // The supervisor reads its control file only between steps; a
            // coder boot can hold it for up to 45 min without looking (seen
            // live: the coder had exited and the run kept polling its
            // health). If it hasn't stopped after the grace period, SIGTERM:
            // tt-orchard's own clean abort (records it, stops the coder,
            // releases the chips).
            let pid = status
                .as_ref()
                .and_then(|s| s["supervisor"]["pid"].as_u64());
            tokio::spawn(async move {
                let deadline = Instant::now() + o.abort_grace;
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let (o3, d) = (o.clone(), dir.clone());
                    let alive = tokio::task::spawn_blocking(move || o3.status_blocking(&d))
                        .await
                        .ok()
                        .flatten()
                        .is_some_and(|s| s["supervisor"]["alive"].as_bool() == Some(true));
                    if !alive {
                        return;
                    }
                    if Instant::now() >= deadline {
                        if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
                            eprintln!("tt-station-agentd: orchard supervisor {pid} didn't stop; sending SIGTERM");
                            // SAFETY: kill(2) with a pid read from the run's own supervisor.pid.
                            unsafe {
                                libc::kill(pid, libc::SIGTERM);
                            }
                        }
                        return;
                    }
                }
            });
        }
        "abort" if active => {
            // Still in the preflight or the download: no supervisor to tell.
            let l = launch.expect("active implies a launch");
            blocking(move || {
                if let Some(unit) = &l.unit {
                    let _ = Command::new("systemctl")
                        .args(["--user", "stop", unit])
                        .status();
                } else if let Some(pid) = l.pid {
                    let _ = Command::new("kill").arg(format!("-{pid}")).status();
                }
            })
            .await?;
        }
        "abort" => return Err(err(StatusCode::CONFLICT, "That run isn't running.")),
        "retry" => {
            if alive || active {
                return Err(err(StatusCode::CONFLICT, "That run is still going."));
            }
            let model = status
                .as_ref()
                .and_then(|s| s["model"].as_str().map(str::to_string))
                .or(launch.map(|l| l.model))
                .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such run."))?;
            let opts =
                RunOptions::from_request(req.refuse_credentials_visible, req.base.as_deref())?;
            return start(&state, o, model, req.stop_serving, opts).await;
        }
        _ => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("No action called {action}."),
            ))
        }
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "run_name": name, "action": action })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_and_run_names() {
        assert!(valid_model("Cloudflare/clef"));
        assert!(valid_model("Qwen/Qwen3.8-27B"));
        assert!(!valid_model("clef"));
        assert!(!valid_model("a/b/c"));
        assert!(!valid_model("../etc"));
        assert!(!valid_model("org/-x"));
        assert_eq!(run_name("Cloudflare/clef"), "cloudflare--clef");
        assert!(valid_run_name("cloudflare--clef"));
        assert!(valid_run_name("qwen--qwen3.8-27b"));
        assert!(!valid_run_name("cloudflare"));
        assert!(!valid_run_name("../x--y"));
        assert!(!valid_run_name("a--b/../c"));
    }

    fn run(state: &str, stages: Value) -> Value {
        json!({ "run_name": "acme--tiny", "model": "acme/Tiny", "state": state, "stages": stages,
                "blocked": { "code": "stage-failed", "reason": "stage 2 failed twice" },
                "pause": { "reason": "operator asked" } })
    }

    #[test]
    fn stage_passes_and_endings_become_notifications() {
        let a = run(
            "running",
            json!([{ "stage": 0, "name": "intake", "status": "pass", "wall_s": 1620.0 },
                                      { "stage": 1, "name": "reference", "status": "running" }]),
        );
        let b = run(
            "running",
            json!([{ "stage": 0, "name": "intake", "status": "pass", "wall_s": 1620.0 },
                                      { "stage": 1, "name": "reference", "status": "pass", "wall_s": 480.0 }]),
        );
        let c = changes(Some(&a), &b);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "🧪 Tiny: soil test passed");
        assert_eq!(c[0].body, "Stage 1 of 8 (reference) passed after 8 min.");
        assert!(changes(Some(&b), &b).is_empty());

        let ripe = run("ready-for-operator-review", b["stages"].clone());
        let c = changes(Some(&b), &ripe);
        assert_eq!(c[0].kind, "orchard_done");
        assert_eq!(c[0].title, "🍎 Tiny is ripe");

        let frost = changes(Some(&b), &run("blocked", b["stages"].clone()));
        assert!(frost[0].body.starts_with("stage 2 failed twice"));
        let crash = changes(Some(&b), &run("stopped-or-crashed", b["stages"].clone()));
        assert_eq!(crash[0].title, "⛈ Tiny bring-up stopped");
        // A run that was already stopped when the agent first saw it says nothing.
        assert!(changes(
            Some(&run("stopped-or-crashed", json!([]))),
            &run("stopped-or-crashed", json!([]))
        )
        .is_empty());
    }

    #[test]
    fn a_launch_without_a_ledger_is_preparing_then_refused() {
        let l = Launch {
            run_name: "acme--tiny".into(),
            model: "acme/Tiny".into(),
            unit: Some("tt-orchard-acme--tiny".into()),
            pid: None,
            log: PathBuf::from("/nonexistent"),
            started: 5,
            restore: None,
        };
        assert_eq!(
            summary("acme--tiny", None, Some(&l), true)["state"],
            "preparing"
        );
        assert_eq!(
            summary("acme--tiny", None, Some(&l), false)["state"],
            "refused"
        );
        let s =
            json!({ "state": "running", "model": "acme/Tiny", "supervisor": { "alive": true } });
        let r = summary("acme--tiny", Some(&s), Some(&l), true);
        assert_eq!(r["state"], "running");
        assert_eq!(r["live"], true);
    }
}

// ---------------------------------------------------------------------
// Serving a finished bring-up
// ---------------------------------------------------------------------

pub(crate) const HARVEST_UNIT: &str = "tt-orchard-serve";

/// The package stage 7 built and boot-checked: `stages/7/verify/` holds an
/// installed copy (`bundle/run.sh`) and an `hf/` cache that links only the
/// new model, exactly what tt-orchard served for its own check.
pub fn harvest_dir(run_dir: &Path) -> Option<PathBuf> {
    let verify = run_dir.join("stages/7/verify");
    verify.join("bundle/run.sh").is_file().then_some(verify)
}

/// Stop a served harvest, if one runs (called whenever the agent stops serving).
/// Only the box's own agent does: a test's state must never stop the real unit.
pub async fn stop_harvest(state: &AppState) {
    if !state.manages_units() {
        return;
    }
    let _ = tokio::process::Command::new("systemctl")
        .args(["--user", "stop", HARVEST_UNIT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    // The unit is stopped, so its lease can go -- and be reset (see
    // `gozer::lease_gated_unit`).
    let ids = crate::gozer::holder_lease_file(&state.config_dir()).take();
    crate::gozer::release_after_stop(state, ids).await;
}

pub(crate) async fn post_serve(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Path(name): axum::extract::Path<String>,
    body: Option<Json<ControlRequest>>,
) -> Reply {
    let o = need(&state)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let o2 = o.clone();
    let n = name.clone();
    let (dir, status) = blocking(move || {
        let dir = run_dir(&o2, &n)?;
        let status = o2.status_blocking(&dir);
        Ok::<_, (StatusCode, Json<Value>)>((dir, status))
    })
    .await??;
    if status.as_ref().and_then(|s| s["state"].as_str()) != Some("ready-for-operator-review") {
        return Err(err(
            StatusCode::CONFLICT,
            "Only a finished (ripe) bring-up can be served.",
        ));
    }
    let model = status
        .as_ref()
        .and_then(|s| s["model"].as_str())
        .unwrap_or("the model")
        .to_string();
    let Some(verify) = harvest_dir(&dir) else {
        return Err(err(
            StatusCode::CONFLICT,
            "This run didn't build a package (packaging was off), so there's nothing to serve. \
             Turn on packaging in Orchard and run it again.",
        ));
    };
    if let Some(holder) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, holder));
    }
    let life = state.lifecycle();
    if !matches!(life.phase, Phase::Idle | Phase::Failed) || crate::coserve::is_split(&state) {
        if !req.stop_serving {
            return Err((
                StatusCode::CONFLICT,
                Json(
                    json!({ "error": "Serving it replaces what's serving now. Stop it first?", "needs_stop": true }),
                ),
            ));
        }
        state.stop_serving().await.map_err(|e| {
            err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't stop the model: {e:#}"),
            )
        })?;
    }
    let reset = state.clone();
    blocking(move || reset.run_power_command(crate::power::PowerAction::ResetChips, false))
        .await?
        .map_err(|e| {
            err(
                StatusCode::CONFLICT,
                format!("couldn't reset the chips: {e:#}"),
            )
        })?;

    let port = state.serving_port();
    let log = o.log_path(&format!("{name}-serve"));
    let _ = std::fs::create_dir_all(log.parent().unwrap_or(Path::new("/tmp")));
    // With gozer: leased to the unit's own main process (`gozer::lease_gated_unit`).
    let gate = crate::gozer::unit_gate(&state, "harvest");
    let serve: Vec<String> = ["bash", "run.sh", "--port", &port.to_string()]
        .map(String::from)
        .to_vec();
    let serve = match &gate {
        Some(fifo) => crate::gozer::gated_argv(fifo, &serve),
        None => serve,
    };
    let status = Command::new("systemd-run")
        .args(["--user", "--collect", "--quiet"])
        .arg(format!("--unit={HARVEST_UNIT}"))
        .arg(format!(
            "--property=WorkingDirectory={}",
            verify.join("bundle").display()
        ))
        .arg(format!(
            "--property=StandardOutput=append:{}",
            log.display()
        ))
        .arg(format!("--property=StandardError=append:{}", log.display()))
        .arg(format!("--setenv=HF_HOME={}", verify.join("hf").display()))
        .arg("--setenv=HF_HUB_OFFLINE=1")
        .arg(format!(
            "--setenv=PATH={}",
            std::env::var("PATH").unwrap_or_default()
        ))
        .args(&serve)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !status.status.success() {
        return Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "couldn't start it: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            ),
        ));
    }
    if let Some(fifo) = gate {
        let lease = crate::gozer::lease_started_unit(&state, HARVEST_UNIT, fifo, &model)
            .await
            .map_err(|why| err(StatusCode::CONFLICT, format!("couldn't start it: {why}")))?;
        crate::gozer::holder_lease_file(&state.config_dir()).add(&lease);
    }
    adopt_when_serving(&state, port, model);
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "serving": name, "log": log })),
    ))
}

/// A model started outside the agent's own serving (an Orchard package, a
/// native vLLM server) on the serving port: show it starting, and adopt it as
/// the served model once it answers, like a model the agent started itself.
pub(crate) fn adopt_when_serving(state: &AppState, port: u16, model: String) {
    state.set_lifecycle(Phase::Starting, Some(model.clone()), None, None);
    let bg = state.clone();
    tokio::spawn(async move {
        let base = format!("http://127.0.0.1:{port}/v1");
        let client = crate::localhttp::local();
        let deadline = Instant::now() + Duration::from_secs(3600);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(10)).await;
            // Stopped (or replaced) meanwhile: stop waiting.
            if !matches!(bg.lifecycle().phase, Phase::Starting) {
                return;
            }
            let served = client
                .get(format!("{base}/models"))
                .send()
                .await
                .ok()
                .filter(|r| r.status().is_success());
            if let Some(r) = served {
                let id = r
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|v| v["data"][0]["id"].as_str().map(str::to_string))
                    .unwrap_or(model.clone());
                bg.adopt_serving(libttstation::model::Endpoint {
                    base_url: base,
                    model: id,
                    requires_key: false,
                });
                // The lifecycle change itself notifies ("… is ready").
                return;
            }
        }
        bg.set_lifecycle(
            Phase::Failed,
            Some(model.clone()),
            None,
            Some("it didn't answer within an hour".into()),
        );
    });
}

#[derive(Debug, Deserialize)]
pub struct Packaging {
    pub enabled: bool,
    /// The Hugging Face namespace written into the model card and the
    /// publish commands (Orchard never publishes).
    #[serde(default)]
    pub namespace: Option<String>,
}

/// Turn tt-orchard's packaging (stage 7, a v6 bundle) on or off in its
/// `bringup.toml`. Lines the agent adds are marked, so it only ever removes
/// its own.
pub fn set_packaging(config: &str, enabled: bool, namespace: &str) -> String {
    const MARK: &str = "# set by tt-station";
    let kept: Vec<&str> = config
        .lines()
        .filter(|l| !l.ends_with(MARK))
        .filter(|l| {
            // An active (uncommented) setting tt-station is about to own.
            let key = l.split('=').next().unwrap_or("").trim();
            !(enabled && (key == "package_format" || key == "package_namespace"))
        })
        .collect();
    // Top-level keys must come before the first table.
    let split = kept
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(kept.len());
    let mut out: Vec<String> = kept[..split].iter().map(|l| l.to_string()).collect();
    if enabled {
        out.push(format!("package_format = \"v6\" {MARK}"));
        out.push(format!("package_namespace = \"{namespace}\" {MARK}"));
    }
    out.extend(kept[split..].iter().map(|l| l.to_string()));
    out.join("\n") + "\n"
}

pub(crate) async fn post_packaging(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<Packaging>,
) -> Reply {
    let o = need(&state)?;
    let path = o.info_blocking(true)["config"]["path"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| {
            err(
                StatusCode::PRECONDITION_FAILED,
                "tt-orchard has no bringup config yet.",
            )
        })?;
    let namespace: String = req
        .namespace
        .unwrap_or_else(|| "local".into())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let namespace = if namespace.is_empty() {
        "local".to_string()
    } else {
        namespace
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    std::fs::write(&path, set_packaging(&text, req.enabled, &namespace))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let info = o.info_blocking(true);
    if let Some(e) = info["config_error"].as_str() {
        // Put it back rather than leave tt-orchard unable to start.
        let _ = std::fs::write(&path, text);
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("tt-orchard refused the change: {e}"),
        ));
    }
    Ok((
        StatusCode::OK,
        Json(json!({ "packaging": req.enabled, "config": info["config"] })),
    ))
}

#[cfg(test)]
mod packaging_tests {
    use super::*;

    #[test]
    fn packaging_lines_are_ours_and_go_before_tables() {
        let cfg =
            "runs_root = \"/r\"\n# package_format = \"v6\"\n\n[env]\nHF_HUB_OFFLINE = \"1\"\n";
        let on = set_packaging(cfg, true, "jon");
        let first_table = on.find("[env]").unwrap();
        assert!(
            on.find("package_format = \"v6\" # set by tt-station")
                .unwrap()
                < first_table
        );
        assert!(on.contains("package_namespace = \"jon\" # set by tt-station"));
        assert!(
            on.contains("# package_format = \"v6\"\n"),
            "the example comment stays"
        );
        let again = set_packaging(&on, true, "ana");
        assert_eq!(
            again.matches("package_format").count(),
            2,
            "comment + one setting"
        );
        assert!(again.contains("\"ana\""));
        let off = set_packaging(&again, false, "ana");
        assert!(!off.contains("# set by tt-station"));
        assert_eq!(off.trim_end(), cfg.trim_end());
    }
}
