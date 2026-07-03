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
//! ## Defer to `run.py`, don't second-guess it -- except where it's proven
//! ## wrong on THIS box
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
//! In practice, one of those four auto-resolutions was ALSO verified (on
//! this box, today) to not actually work:
//!   - `run.py`'s own `--tt-device` auto-detect FAILS here with `Unable to
//!     map tt-smi board counts ... {'p300c': 4}` -- it doesn't know this
//!     board combination.
//!
//! So this backend fills that gap itself, via `resolve_tt_device` (parses
//! `tt-smi -s` and maps known board combinations); this is default-ON,
//! since it's a safe, purely-additive fix for a confirmed `run.py` bug.
//! `--impl`/`--engine` genuinely have no such problem and are left entirely
//! to `run.py`/`model_spec.json`.
//!
//! `resolve_image` (picks the newest locally-present RELEASE image from
//! `docker images`) exists for the OTHER gap -- `run.py`'s default image
//! tag (from `model_spec.json`) isn't always pulled/on GHCR on a given box
//! -- but is DELIBERATELY opt-in (`RunPyConfig::auto_image`, default
//! `false`), NOT default-on like `resolve_tt_device`. Verified on real
//! hardware: auto-picking the newest local image chose a tag whose
//! container server rejects `--override-tt-config`, a flag this repo's
//! `run.py` always passes -- image<->`run.py` compatibility is a curated
//! matrix, and "newest" is not a safe stand-in for that curation. See
//! `resolve_image`'s doc comment for the full rationale.
//!
//! `RunPyConfig`'s device/image/impl/engine/device-id fields are all
//! `Option<String>`; `start` computes the RESOLVED device/image once (an
//! explicit `Some(..)` always wins over auto-resolution -- see each
//! `resolve_*` method) and appends the corresponding `run.py` flag only
//! when that resolution produced a value. A known box (this one) therefore
//! needs zero `--tt-device` configuration (auto-detected) but DOES need
//! either `--serving-image` pinned or `--auto-image` opted into, since the
//! image auto-pick is no longer on by default.
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

/// How many times `start` polls the health endpoint before giving up.
///
/// Sized for a REAL `run.py` bring-up, not a test: launching an LLM on
/// hardware downloads/compiles/loads weights and can take many minutes
/// (observed ~10 min for 8B, up to ~40 min for 70B on first run). At the
/// `DEFAULT_HEALTH_POLL_INTERVAL` below this is a ceiling of ~40 minutes,
/// after which `start` returns an error rather than hanging forever. This
/// is deliberately far larger than `docker::DEFAULT_HEALTH_POLL_ATTEMPTS`
/// (the raw `docker run` path assumes an already-built image).
/// `RunPyBackend::with_health_poll` overrides both for tests.
const DEFAULT_HEALTH_POLL_ATTEMPTS: u32 = 1200;

/// Delay between health-poll attempts. See `DEFAULT_HEALTH_POLL_ATTEMPTS`.
/// 2s keeps the probe rate gentle over a multi-minute bring-up.
const DEFAULT_HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The repository name (no registry host, no tag) of the RELEASE serving
/// image `resolve_image` looks for -- as opposed to the `-dev-` variant
/// (`vllm-tt-metal-src-dev-ubuntu-22.04-amd64`), which is a development
/// build this codebase never wants to auto-pick for serving. Matched via
/// `str::ends_with` against each `docker images` repository column, since
/// the full repo also carries a registry/org prefix (e.g.
/// `ghcr.io/tenstorrent/tt-inference-server/`) that varies by mirror.
const RELEASE_IMAGE_REPO_SUFFIX: &str = "vllm-tt-metal-src-release-ubuntu-22.04-amd64";

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
    /// default) means "auto-resolve" -- see `resolve_tt_device`, which
    /// tries `run.py`'s own hardware auto-detection first and, if that's
    /// known to fail (as it does on this box), falls back to parsing
    /// `tt-smi -s` itself. `Some(..)` is an explicit OVERRIDE that skips
    /// auto-resolution entirely.
    pub tt_device: Option<String>,
    /// `--override-docker-image`: the image `run.py` runs the resolved
    /// model in. `None` (the default) means "let `run.py`/`auto_image`
    /// decide" -- see `resolve_image`. `Some(..)` is an explicit OVERRIDE
    /// that skips auto-resolution entirely (and skips consulting
    /// `auto_image` too).
    pub image: Option<String>,
    /// Opt-in switch for the newest-local-release auto-pick `resolve_image`
    /// performs when `image` is `None`. Defaults to `false` -- see this
    /// field's extensive rationale in `resolve_image`'s doc comment: image
    /// vs. `run.py` compatibility is a curated matrix, not something
    /// "newest locally-present" can safely stand in for. When `false` (the
    /// default) and `image` is `None`, `resolve_image` returns `None`
    /// outright without even running `docker images` -- `run.py` then uses
    /// its own `model_spec.json` default image tag.
    pub auto_image: bool,
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
    /// When `true` (the default), `start` runs `reset_cmd` (`tt-smi -r` by
    /// default) BEFORE launching `run.py`.
    ///
    /// Validated on real hardware: stopping a serving container leaves the
    /// p300x2 mesh's ethernet cores wedged, and the NEXT launch fails with
    /// `TT_THROW: ... Timed out while waiting for active ethernet core ...
    /// Try resetting the board`. Resetting before every serve attempt
    /// (rather than only on `stop`) is the robust choice: it also covers
    /// models that were stopped externally (e.g. `docker stop` by hand) or
    /// that crashed without this backend's `stop` ever running.
    ///
    /// Set to `false` on boards where the reset is unwanted or `tt-smi` is
    /// unavailable -- see `main.rs`'s `--no-device-reset` flag.
    pub reset_before_serve: bool,
    /// Argv for the pre-serve board reset (see `reset_before_serve`),
    /// e.g. `["tt-smi", "-r"]`. Kept configurable (rather than a hardcoded
    /// `tt-smi -r` string) so tests can assert on it precisely and so a
    /// board that needs a different reset invocation can supply one.
    pub reset_cmd: Vec<String>,
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
    /// (see the module doc): `impl`/`engine`/`device-id` are left entirely
    /// to `run.py`, `tt_device` is auto-RESOLVED by this backend itself
    /// (`resolve_tt_device`), and `image` is left to `run.py`'s own
    /// `model_spec.json` default UNLESS `auto_image` is explicitly opted
    /// into (see `resolve_image` and `auto_image`'s own doc comments) --
    /// this codebase does not hardcode a guessed value for any of them up
    /// front.
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
            auto_image: false,
            impl_name: None,
            engine: None,
            device_id: None,
            model_spec_path: None,
            reset_before_serve: true,
            reset_cmd: vec!["tt-smi".to_string(), "-r".to_string()],
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

    /// Stop whatever container is publishing `service_port`, exactly like
    /// the operator's own `start_artgen.sh --stop`: `run.py` doesn't hand
    /// back a predictable container name the way `DockerBackend`'s own
    /// `--name` flag does, so this finds it by the one thing that IS
    /// predictable -- the published port -- via `docker ps --filter
    /// publish=<port> -q`, then `docker stop`s whatever comes back. Empty
    /// `docker ps` output (nothing running) is treated as success: this is
    /// idempotent, same contract as `DockerBackend::stop`.
    ///
    /// Shared by both `stop` (the explicit "stop serving" call) and `start`
    /// (which calls this FIRST, before even the board reset, to clear a
    /// stale/crashed container that would otherwise hold the chips and make
    /// run.py's own container-start check time out on the next launch --
    /// see `start`'s doc comment).
    fn stop_serving_containers(&self) -> Result<()> {
        let publish_filter = format!("publish={}", self.config.service_port);
        let ps_output = self
            .runner
            .run(&["docker", "ps", "--filter", &publish_filter, "-q"])?;

        for container_id in ps_output.split_whitespace() {
            self.runner.run(&["docker", "stop", container_id])?;
        }

        Ok(())
    }

    /// Resolve the `--tt-device` value: `config.tt_device` if the caller
    /// explicitly set one (an explicit override always wins, and skips
    /// shelling out to `tt-smi` entirely), otherwise auto-detect it by
    /// parsing `tt-smi -s`'s JSON.
    ///
    /// This exists because `run.py`'s OWN `--tt-device` auto-detection is
    /// verified (on real hardware) to fail on this box with `Unable to map
    /// tt-smi board counts ... {'p300c': 4}` -- it doesn't know this board
    /// combination. Rather than leave the operator to pass `--tt-device`
    /// by hand every time, this fills the gap: run `tt-smi -s`, collect
    /// `device_info[].board_info.board_type` for every board, and map
    /// known (board-type, count) combinations to a `--tt-device` string.
    ///
    /// The map below is DELIBERATELY small and covers only what's needed
    /// to unblock boards where `run.py`'s own detection is known to be
    /// broken -- it is NOT trying to reimplement `run.py`'s full device
    /// catalog. Anything it doesn't recognize (a board type it's never
    /// seen, a mixed fleet, an unparseable `tt-smi` response, or a
    /// `tt-smi` invocation that fails outright) resolves to `None`, which
    /// means `start` omits `--tt-device` and lets `run.py` make its own
    /// attempt (and fail loudly with its own error) rather than this code
    /// inventing a value it has no confirmed mapping for.
    fn resolve_tt_device(&self) -> Option<String> {
        if let Some(device) = &self.config.tt_device {
            return Some(device.clone());
        }

        let output = self.runner.run(&["tt-smi", "-s"]).ok()?;
        let value: serde_json::Value = serde_json::from_str(&output).ok()?;

        // Verified `tt-smi -s` schema: top-level `device_info` is a list,
        // one entry per board, each with a `board_info.board_type` string
        // (e.g. `"p300c"`). Lower-cased for a case-insensitive match, per
        // the board-combination map below.
        let board_types: Vec<String> = value
            .get("device_info")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|device| {
                device
                    .get("board_info")?
                    .get("board_type")?
                    .as_str()
                    .map(str::to_lowercase)
            })
            .collect();

        let count = board_types.len();
        let all_same_type = board_types.windows(2).all(|pair| pair[0] == pair[1]);
        let board_type = if count > 0 && all_same_type {
            board_types[0].as_str()
        } else {
            // Empty `device_info`, or a mixed fleet -- neither is a
            // combination this map has a confirmed answer for.
            ""
        };

        // Board-type/count -> `--tt-device` map. Covers only what run.py's
        // own auto-detect gets wrong (see this method's doc comment above),
        // not a general-purpose device catalog.
        let resolved = match (board_type, count) {
            ("p300c", 4) => Some("p300x2"),
            ("p300c", 2) => Some("p300"),
            ("p150" | "p150c", 4) => Some("p150x4"),
            ("n300", 4) => Some("n300x4"),
            ("n300", 1) => Some("n300"),
            _ => None,
        };

        match resolved {
            Some(device) => eprintln!("auto-detected tt-device: {device} ({count}x {board_type})"),
            None => eprintln!("could not auto-detect tt-device; letting run.py try"),
        }

        resolved.map(str::to_string)
    }

    /// Resolve the `--override-docker-image` value: `config.image` if the
    /// caller explicitly set one (an explicit override always wins, and
    /// skips shelling out to `docker` entirely); else, ONLY when
    /// `config.auto_image` is opted into, auto-pick the newest
    /// locally-present RELEASE serving image via `docker images`; else
    /// `None` (no `docker images` call at all).
    ///
    /// ## Why the auto-pick is opt-in, NOT the default
    ///
    /// This used to unconditionally auto-pick the newest local release
    /// image whenever `config.image` was unset. That was verified (on real
    /// hardware) to be UNSAFE: on one box, the newest locally-present image
    /// (`0.17.0-...`) was auto-picked over an older one, but that image's
    /// container server REJECTS a flag this repo's `run.py` unconditionally
    /// passes (`--override-tt-config`), so serving failed outright.
    /// Image<->`run.py` compatibility is a CURATED matrix, not something
    /// "whichever tag happens to sort newest by `docker images`
    /// `CreatedAt`" can stand in for -- a newer image is not guaranteed
    /// compatible with an older (or differently-patched) `run.py` checkout,
    /// and vice versa. So this now defaults to doing nothing: an unset
    /// `--serving-image` yields no `--override-docker-image` at all, and
    /// `run.py` falls back to its own `model_spec.json` default image
    /// (which, per the module doc, isn't always pulled locally either --
    /// the operator is expected to pin `--serving-image` on boxes where
    /// that default isn't available, rather than this code silently
    /// picking a possibly-incompatible substitute).
    ///
    /// The newest-local-release scan logic itself is UNCHANGED and only
    /// runs when `config.auto_image` is `true`: it queries `docker images
    /// --format '{{.Repository}}:{{.Tag}}\t{{.CreatedAt}}'`, keeps only
    /// lines whose repository ends in `RELEASE_IMAGE_REPO_SUFFIX`
    /// (excluding the `-dev-` variant and any untagged `<none>` image), and
    /// returns the newest one by `CreatedAt`.
    ///
    /// `CreatedAt` looks like `2026-06-25 14:22:23 -0700 PDT` -- comparing
    /// the full string lexically does NOT sort correctly across timezone
    /// suffixes, so only the leading `YYYY-MM-DD HH:MM:SS` (the first 19
    /// bytes, which IS a fixed-width, lexically-sortable format) is used
    /// for ordering. Ties -- including two lines with an unparseable/short
    /// `CreatedAt` -- keep whichever was seen FIRST rather than replacing
    /// it, and any line that doesn't parse as `repo:tag<TAB>created` is
    /// simply skipped rather than aborting the whole scan. If nothing
    /// matches, this returns `None` and `start` omits
    /// `--override-docker-image`, letting `run.py` make its own (possibly
    /// failing) attempt rather than this code inventing an image.
    fn resolve_image(&self) -> Option<String> {
        if let Some(image) = &self.config.image {
            return Some(image.clone());
        }

        if !self.config.auto_image {
            return None;
        }

        let output = self
            .runner
            .run(&[
                "docker",
                "images",
                "--format",
                "{{.Repository}}:{{.Tag}}\t{{.CreatedAt}}",
            ])
            .ok()?;

        // Tracks the best candidate seen so far as (created-prefix,
        // "repo:tag"); replaced only on a STRICTLY newer timestamp so ties
        // keep the first-seen line.
        let mut best: Option<(&str, &str)> = None;

        for line in output.lines() {
            let Some((image_ref, created)) = line.split_once('\t') else {
                continue; // malformed line -- skip rather than abort the scan
            };
            // The tag is whatever follows the LAST `:` (a registry host
            // like `ghcr.io` never itself contains one after the repo
            // path, so `rsplit_once` is unambiguous here).
            let Some((repo, tag)) = image_ref.rsplit_once(':') else {
                continue;
            };
            if tag == "<none>" || !repo.ends_with(RELEASE_IMAGE_REPO_SUFFIX) {
                continue;
            }
            // `created`'s leading 19 bytes (`YYYY-MM-DD HH:MM:SS`) are
            // fixed-width and lexically sortable; anything shorter than
            // that isn't a `CreatedAt` this can order at all.
            let Some(created_prefix) = created.get(..19) else {
                continue;
            };

            let is_newer = best.is_none_or(|(best_created, _)| created_prefix > best_created);
            if is_newer {
                best = Some((created_prefix, image_ref));
            }
        }

        match best {
            Some((_, image_ref)) => {
                eprintln!("auto-picked local release image: {image_ref}");
                Some(image_ref.to_string())
            }
            None => {
                eprintln!("could not auto-pick a local release image; letting run.py try");
                None
            }
        }
    }
}

impl ServingBackend for RunPyBackend {
    fn start(&self, model: &str) -> Result<Endpoint> {
        // Stop any STALE serving container FIRST -- before even the board
        // reset. Validated on real hardware: a leftover/crashed container
        // that's still publishing `service_port` holds the chips, so
        // run.py's own container-start check times out on the next launch.
        // This is unconditional (NOT gated by `reset_before_serve`): a
        // stale container is a problem regardless of whether the board also
        // needs resetting, and clearing it before the reset means the
        // reset itself isn't fighting a container that still has the mesh
        // open.
        self.stop_serving_containers()
            .context("failed to stop stale serving container before launch")?;

        // Reset the board next -- validated on real hardware: stopping a
        // serving container leaves the p300x2 mesh's ethernet cores wedged,
        // and the NEXT launch fails with `TT_THROW: ... Timed out while
        // waiting for active ethernet core ... Try resetting the board`.
        // Doing this here (rather than only in `stop`) also covers models
        // that were stopped externally or crashed without `stop` ever
        // running -- see `RunPyConfig::reset_before_serve`'s doc comment. A
        // failed reset means the upcoming serve attempt will almost
        // certainly fail too (the mesh is still wedged), so surface the
        // error immediately rather than pressing on to a doomed `run.py`
        // invocation.
        if self.config.reset_before_serve {
            let reset_cmd_str = self.config.reset_cmd.join(" ");
            eprintln!("resetting board before serving: {reset_cmd_str}");
            let reset_refs: Vec<&str> = self.config.reset_cmd.iter().map(String::as_str).collect();
            self.runner
                .run(&reset_refs)
                .with_context(|| format!("board reset ({reset_cmd_str}) failed"))?;
        }

        // `run.py --model` wants the SHORT model name (e.g. `Qwen3-32B`,
        // matching `model_spec.json`'s own keys' basenames), but callers
        // (and `tt models`, which lists `model_spec.json`'s HF-id keys
        // verbatim) naturally pass a Hugging Face id like `Qwen/Qwen3-32B`.
        // Stripping any `org/` prefix here means BOTH forms work: `run.py`
        // gets the short name it validates against, while `model` (the
        // original, possibly-HF-id argument) is still available below as
        // the fallback `Endpoint.model` if the authoritative served id
        // can't be fetched. `rsplit('/').next()` always yields `Some(..)`
        // (even for a string with no `/` at all, in which case it's the
        // whole string), so `unwrap_or(model)` is just a defensive
        // fallback, never actually exercised.
        let run_model = model.rsplit('/').next().unwrap_or(model);

        // Resolve the two values `run.py`'s own auto-detection is verified
        // to get wrong on this box -- see `resolve_tt_device`/
        // `resolve_image`'s doc comments. Computed ONCE here (after the
        // stale-container stop and board reset above, so a board reset
        // doesn't race a `tt-smi -s` probe) and reused below when building
        // argv; an explicit `config.tt_device`/`config.image` always wins
        // over auto-resolution.
        let device = self.resolve_tt_device();
        let image = self.resolve_image();

        // Built as owned `String`s (several pieces are computed at
        // runtime) then borrowed as `&str` for `CommandRunner::run_in_dir`,
        // which takes `&[&str]` -- argv-style, no shell involved, so
        // callers never need to worry about quoting.
        //
        // This is the MINIMAL invocation: `--model` (required), `--workflow
        // server --docker-server` (how this codebase always launches
        // serving), and `--service-port`. Everything else below is an
        // OPTIONAL flag appended only when a value is available:
        // `--tt-device`/`--override-docker-image` from the RESOLVED
        // `device`/`image` above (explicit override or auto-resolved --
        // either way, a known box like this one ends up needing zero
        // model-serving flags), and `--impl`/`--engine` only when the
        // caller explicitly configured them -- see the module doc's
        // "Defer to `run.py`, don't second-guess it" section for why those
        // two are left entirely to `run.py`/`model_spec.json`.
        let mut args: Vec<String> = vec![
            "python3".to_string(),
            "run.py".to_string(),
            "--model".to_string(),
            run_model.to_string(),
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
        if let Some(device) = &device {
            args.push("--tt-device".to_string());
            args.push(device.clone());
        }
        if let Some(image) = &image {
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

        // Gate "serving" on the model actually being QUERYABLE on `/v1`, not
        // merely on `/health` returning 200. Verified on real hardware:
        // `GET /health` can go 200 while the model is still loading, so the
        // container is "up" but `/v1` chat/completion requests still fail
        // (a 70B reported "serving" per status while `:8003` was
        // connection-refused). Handing back that endpoint reports a DEAD
        // server as serving. The authoritative readiness signal is
        // `/v1/models` listing at least one model: that only happens once
        // vLLM has the weights loaded and its OpenAI server is answering on
        // `/v1`.
        //
        // So poll `/v1/models` (with a cheap `/health` liveness check first
        // each round, since `/health` comes up strictly before `/v1` and
        // skips a pointless HTTP GET while the container is still booting)
        // until the response parses as JSON with a NON-EMPTY `data` array,
        // within the SAME bounded budget the old `/health` poll used
        // (`health_poll_attempts` x `health_poll_interval`) -- no infinite
        // loop, no lock held across any of it.
        let health_url = format!(
            "http://{}:{}/health",
            self.config.host, self.config.service_port
        );
        let models_url = format!(
            "http://{}:{}/v1/models",
            self.config.host, self.config.service_port
        );

        // On success this holds the served model id to report back. `run.py
        // --model` got the SHORT name (`run_model`, above), but the served
        // OpenAI `/v1/models` id is the real HF id (`Qwen/Qwen3-32B`) a
        // client needs in its own `model` field, so prefer it. Fall back to
        // the ORIGINAL `model` argument only when `data` is non-empty (the
        // model IS queryable) but the `id` field itself can't be read.
        let mut served_model: Option<String> = None;
        for _ in 0..self.health_poll_attempts {
            // Cheap liveness gate: while the container is still coming up,
            // `/health` isn't 200 yet, so skip the `/v1/models` GET until it
            // is. `/v1/models` (below) is the AUTHORITATIVE gate.
            if self.runner.health_ok(&health_url) {
                if let Some(id) = self
                    .runner
                    .http_get(&models_url)
                    .ok()
                    .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                    .and_then(|value| {
                        // A non-empty `data` array means vLLM has the model
                        // loaded and is answering on `/v1` -- the whole point
                        // of this gate. An empty `data` (or a missing/wrong
                        // shape) means "not ready yet": keep polling.
                        let data = value.get("data")?.as_array()?;
                        if data.is_empty() {
                            return None;
                        }
                        Some(
                            data[0]
                                .get("id")
                                .and_then(|id| id.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| model.to_string()),
                        )
                    })
                {
                    served_model = Some(id);
                    break;
                }
            }
            std::thread::sleep(self.health_poll_interval);
        }

        // Timed out: the model never became queryable on `/v1/models` within
        // the budget. Return an error and DO NOT flip status to serving or
        // hand back an Endpoint -- status stays `Idle`, and `/run` turns this
        // into a 500, which is the whole point: never report a dead endpoint
        // as serving.
        let served_model = served_model.ok_or_else(|| {
            anyhow::anyhow!(
                "runpy backend: model '{model}' did not become queryable on \
                 {models_url} within {} attempts",
                self.health_poll_attempts
            )
        })?;

        *self.status.lock().expect("status mutex poisoned") =
            ServingStatus::Serving(served_model.clone());

        Ok(Endpoint {
            base_url: format!(
                "http://{}:{}/v1",
                self.config.host, self.config.service_port
            ),
            model: served_model,
            // Auth is required exactly when `run.py` was NOT invoked with
            // `--no-auth`.
            requires_key: !self.config.no_auth,
        })
    }

    fn stop(&self, _model: &str) -> Result<()> {
        self.stop_serving_containers()?;
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
