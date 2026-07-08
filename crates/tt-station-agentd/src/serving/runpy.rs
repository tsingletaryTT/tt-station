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

/// The vLLM `--tool-call-parser` value for a model, or `None` if we don't
/// know a safe one (in which case tool calling is NOT enabled -- a wrong
/// parser can break vLLM startup, so we never guess).
///
/// This mirrors the per-model `tool_call_parser_name` metadata in
/// tt-inference-server's `model_spec.py` (which the launcher itself never
/// reads) and tt-studio's own `tool_call_parser_for` heuristic. The parser
/// value is what tt-inference-server would need to launch vLLM with
/// `--enable-auto-tool-choice --tool-call-parser <parser>` so that a served
/// model accepts `tools`/`tool_choice:"auto"` from a coding agent.
///
/// Deliberately CONSERVATIVE: it returns `Some` only for chat/instruct
/// models we can name a parser for. Base (non-chat) checkpoints like
/// `Llama-3.1-70B` return `None` -- they have no tool-aware chat template, so
/// enabling a parser would at best do nothing and at worst fail startup.
///
/// Matching notes:
/// - DeepSeek is checked BEFORE Llama on purpose: `DeepSeek-R1-Distill-Llama-70B`
///   contains "llama" but uses the `deepseek_v3` parser, not `llama3_json`.
/// - Qwen3 / QwQ are chat models by default (no `-Instruct` suffix), so they
///   match without requiring "instruct"; older Qwen2.5 needs the instruct tag.
/// - Llama parser depends on the generation: 3.1/3.3 → `llama3_json`,
///   3.2 → `pythonic`, 4 → `llama4_pythonic` (per the TT vLLM fork docs).
pub fn tool_call_parser_for(model: &str) -> Option<&'static str> {
    let s = model.to_lowercase();

    // DeepSeek reasoning/distill chat models (must precede the llama branch).
    if s.contains("deepseek") {
        return Some("deepseek_v3");
    }
    // gpt-oss chat models.
    if s.contains("gpt-oss") || s.contains("gpt_oss") {
        return Some("openai");
    }
    // Qwen chat: Qwen3 / QwQ are chat by default; Qwen2.5 only when instruct.
    if s.contains("qwq") || s.contains("qwen3") {
        return Some("hermes");
    }
    if s.contains("qwen") && s.contains("instruct") {
        return Some("hermes");
    }
    // Mistral instruct chat.
    if s.contains("mistral") && s.contains("instruct") {
        return Some("mistral");
    }
    // Llama instruct chat -- parser varies by generation.
    if s.contains("llama") && s.contains("instruct") {
        if s.contains("llama-3.2") || s.contains("llama3.2") {
            return Some("pythonic");
        }
        if s.contains("llama-4") || s.contains("llama4") {
            return Some("llama4_pythonic");
        }
        // 3.1 / 3.3 (and any other Llama-3.x instruct) use the JSON parser.
        return Some("llama3_json");
    }

    None
}

/// Build the `--vllm-override-args` JSON payload that enables tool calling
/// with `parser`. Emitted with underscore keys (`enable_auto_tool_choice` /
/// `tool_call_parser`) -- the exact form verified on this box's
/// tt-inference-server; run.py normalizes `-`/`_` either way. serde_json
/// guarantees correct quoting/escaping of the value.
fn tool_calling_override_args(parser: &str) -> String {
    serde_json::json!({
        "enable_auto_tool_choice": true,
        "tool_call_parser": parser,
    })
    .to_string()
}

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
    /// When `true` (the default), `start` enables OpenAI-style tool calling
    /// for any model whose family has a known vLLM tool-call parser (see
    /// [`tool_call_parser_for`]) by passing run.py
    /// `--vllm-override-args '{"enable_auto_tool_choice": true, "tool_call_parser": "<parser>"}'`.
    ///
    /// This is NOT automatic in tt-inference-server: `run.py` only wires the
    /// parser into vLLM when told to (its `model_spec.json` carries a
    /// `tool_call_parser_name` per model, but that metadata is inert -- the
    /// launcher never reads it), and vLLM otherwise REJECTS a
    /// `/v1/chat/completions` request that carries `tools`/`tool_choice:"auto"`.
    /// So a coding agent (Claude Code, opencode, Cursor) pointed at a served
    /// Llama-3.3-70B-Instruct fails on every tool call unless we launch it
    /// with these flags -- hence default-on for the families we know the
    /// parser for. Unknown families get NOTHING (see `tool_call_parser_for`):
    /// a wrong parser can break server startup, so we never guess.
    ///
    /// Set to `false` to suppress the injection entirely (e.g. to pass tool
    /// args by hand via a future override, or to debug a startup issue).
    pub enable_tool_calling: bool,
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
            enable_tool_calling: true,
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

        // The `(board_type, count) -> mesh` mapping is the single shared
        // table in `crate::device::detect_device_mesh` -- both this
        // `--tt-device` resolution and the `/status` route's mesh report
        // read from it, so it lives in exactly one place.
        let resolved = crate::device::detect_device_mesh(&output);

        match &resolved {
            Some(device) => eprintln!("auto-detected tt-device: {device}"),
            None => eprintln!("could not auto-detect tt-device; letting run.py try"),
        }

        resolved
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

        // Enable OpenAI-style tool calling for families we know the vLLM
        // parser for (see `enable_tool_calling` / `tool_call_parser_for`).
        // `run_model` is the org-stripped short name the parser heuristic
        // expects. Unknown families add nothing -- no guessed parser.
        if self.config.enable_tool_calling {
            if let Some(parser) = tool_call_parser_for(run_model) {
                args.push("--vllm-override-args".to_string());
                args.push(tool_calling_override_args(parser));
            }
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
        let run_stdout = self.runner.run_in_dir_with_env(
            &self.config.repo_dir,
            &arg_refs,
            &[("MODEL_SOURCE", self.config.model_source.as_str())],
        )?;

        // `run.py`'s captured stdout carries the underlying container id and
        // both log file paths on a successful launch (see
        // `parse_run_artifacts`). Emit them as breadcrumbs immediately so
        // `journalctl` explains what got started even if the health poll
        // below never completes -- without this, a wedged/slow container
        // left NO trace of which container or log file it was. `artifacts`
        // stays in scope through the poll loop below so the failure branch
        // can tail `container_log` into the journal too.
        let artifacts = parse_run_artifacts(&run_stdout);
        if let Some(id) = &artifacts.container_id {
            eprintln!("tt-station-agentd: serving container id: {id} (docker logs -f {id})");
        }
        if let Some(p) = &artifacts.container_log {
            eprintln!("tt-station-agentd: container log: {p}");
        }
        if let Some(p) = &artifacts.run_log {
            eprintln!("tt-station-agentd: run.py log: {p}");
        }

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
        let served_model = match served_model {
            Some(m) => m,
            None => {
                // On failure, surface the container's own tail so
                // `journalctl` explains why -- best-effort: a missing
                // `container_log` (run.py's output format drifted, or it
                // failed before printing the line at all) or an unreadable
                // file just means no tail, never a hard error on top of the
                // one we're already returning.
                if let Some(p) = &artifacts.container_log {
                    if let Ok(lines) = crate::logs::tail_lines(std::path::Path::new(p), 20) {
                        eprintln!(
                            "tt-station-agentd: last {} lines of container log ({p}):",
                            lines.len()
                        );
                        for l in lines {
                            eprintln!("tt-station-agentd:   {}", crate::logs::redact_line(&l));
                        }
                    }
                }
                return Err(anyhow::anyhow!(
                    "runpy backend: model '{model}' did not become queryable on \
                     {models_url} within {} attempts",
                    self.health_poll_attempts
                ));
            }
        };

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

    /// Return the box to a fresh state (`POST /reset`): stop any serving
    /// container, then reset the board -- exactly the "clear the chips" work
    /// `start` does up front, but run on demand for a demo reset instead of
    /// just before a serve.
    ///
    /// The container stop reuses the same `stop_serving_containers` helper
    /// `start`/`stop` use (idempotent: an empty `docker ps` is success), and
    /// its failure IS propagated -- if we can't even clear a stale container,
    /// the caller should know the reset didn't fully land.
    ///
    /// The board reset (`reset_cmd`, `tt-smi -r`), by contrast, is
    /// best-effort: a failed reset is logged and swallowed rather than
    /// failing the whole `/reset`, so a demo reset still clears serving state
    /// (and, above this call, the agent's tokens/status) even on a box where
    /// `tt-smi` is flaky or absent. It's gated on `reset_before_serve` for
    /// the same reason `start` gates its reset: a box configured with
    /// `--no-device-reset` doesn't want `tt-smi -r` run at all.
    fn reset(&self) -> Result<()> {
        // Stop any serving container first (same helper start/stop use).
        self.stop_serving_containers()
            .context("failed to stop serving container during reset")?;

        // Reset the board best-effort: log on failure, don't fail /reset.
        if self.config.reset_before_serve {
            let reset_cmd_str = self.config.reset_cmd.join(" ");
            eprintln!("resetting board during reset: {reset_cmd_str}");
            let reset_refs: Vec<&str> = self.config.reset_cmd.iter().map(String::as_str).collect();
            if let Err(err) = self.runner.run(&reset_refs) {
                eprintln!(
                    "board reset ({reset_cmd_str}) failed during reset: {err:#} -- continuing"
                );
            }
        }

        *self.status.lock().expect("status mutex poisoned") = ServingStatus::Idle;
        Ok(())
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(self.status.lock().expect("status mutex poisoned").clone())
    }

    /// Reconcile the in-memory status against docker reality: if we think we're
    /// `Serving` but no live `tt-inference-server` endpoint on our serving port
    /// is actually answering `/v1/models` for that model, the container is gone
    /// (a manual `docker stop`, a crash) -- report `Idle`. Probes via this
    /// backend's own `CommandRunner` (real `docker ps` + `/v1/models` in prod;
    /// a `FakeRunner` in tests), reusing the same discovery + decision the
    /// `/serving` route uses. An idle status skips the probe entirely.
    fn reconciled_status(&self) -> Result<ServingStatus> {
        let status = self.status()?;
        if !matches!(status, ServingStatus::Serving(_)) {
            return Ok(status);
        }
        let entries = crate::serving::discovery::discover_serving(
            self.runner.as_ref(),
            &self.config.host,
            self.config.service_port,
            &status,
        );
        Ok(crate::serving::discovery::reconcile_status(
            &status, &entries,
        ))
    }

    /// Read `model_spec.json` (see `model_spec_path`) and enumerate every
    /// model it lists, with the device meshes each one supports -- so a
    /// client (`GET /models`, `tt models`) never has to guess or hardcode
    /// which models this box can actually run.
    ///
    /// `model_spec.json`'s shape (verified on real hardware):
    /// ```json
    /// { "release_version": "0.12.0",
    ///   "model_specs": {
    ///     "<model-id>": { "<DEVICE_MESH>": { "<engine>": {...}, ... }, ... },
    ///     ... } }
    /// ```
    /// The model id is the top-level key under `model_specs`; the supported
    /// device meshes are that entry's own keys (e.g. `GALAXY`, `T3K`,
    /// `P300X2`); and EACH mesh's own keys are engine names -- `"vLLM"` (an
    /// LLM this backend serves via the `run.py` path in `start`) or
    /// `"media"` (a different tt-media server this backend does NOT drive).
    ///
    /// This backend can only serve `"vLLM"` models, so a model is INCLUDED
    /// only if at least one of its meshes has a `"vLLM"` engine key
    /// (case-insensitive), and its reported `devices` are ONLY the meshes
    /// that have one (media-only meshes are dropped). A model with no vLLM
    /// mesh at all (e.g. an image/video/embedding model that's `"media"`
    /// everywhere) is omitted entirely -- otherwise `tt models` would list
    /// models this box can't actually run.
    ///
    /// Parsed via `serde_json::Value` rather than a strict typed struct so an
    /// entry shaped unexpectedly (a non-object model value, or a non-object
    /// mesh value) is just skipped rather than failing the whole enumeration
    /// -- this is a read-only "what's available" listing, not validation of
    /// the spec file itself (that's `run.py`'s job).
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
                // Keep only meshes that expose a `vLLM` engine (the engine
                // this backend actually serves). Each mesh value is itself an
                // object whose keys are engine names; a mesh whose value
                // isn't an object, or that has no `vLLM` key, is dropped.
                let mut devices: Vec<String> = devices_obj
                    .iter()
                    .filter(|(_, engines_val)| {
                        engines_val.as_object().is_some_and(|engines| {
                            engines
                                .keys()
                                .any(|engine| engine.eq_ignore_ascii_case("vLLM"))
                        })
                    })
                    .map(|(mesh, _)| mesh.clone())
                    .collect();
                // No vLLM-servable mesh at all -> this backend can't run the
                // model (e.g. a media/embedding-only model), so omit it.
                if devices.is_empty() {
                    return None;
                }
                devices.sort();
                Some(libttstation::model::ModelInfo {
                    name: name.clone(),
                    devices,
                    // Filled in below by the HF-cache scan.
                    downloaded: false,
                })
            })
            .collect();
        models.sort_by(|a, b| a.name.cmp(&b.name));

        // Best-effort: mark which models already have weights in the box's HF
        // cache, so the UI can show "starts fast" vs "needs a download."
        let downloaded = scan_downloaded_keys(self.config.host_hf_cache.as_deref());
        for m in &mut models {
            m.downloaded = downloaded.contains(&libttstation::catalog::normalize_key(&m.name));
        }

        Ok(ModelsResponse {
            release_version,
            models,
        })
    }
}

/// Scan the box's HuggingFace cache for downloaded model weights and return
/// the set of [`normalize_key`](libttstation::catalog::normalize_key)'d model
/// identifiers found there.
///
/// The HF hub stores each downloaded repo as
/// `<host_hf_cache>/hub/models--<org>--<name>` (a repo id's `/` becomes `--`).
/// We reconstruct `<org>/<name>` and normalize it the same way `classify`
/// keys live models, so `ModelInfo::name` (`meta-llama/Llama-3.3-70B-Instruct`)
/// matches its cache dir (`models--meta-llama--Llama-3.3-70B-Instruct`).
///
/// Best-effort by design: a missing/unreadable cache (or `None` path) yields
/// an empty set (everything reads as not-downloaded) rather than an error --
/// this is a UI hint, never a gate. NOTE: presence of the cache dir is a
/// proxy for "downloaded," not a guarantee the snapshot is 100% complete;
/// that's an acceptable trade for a fast, dependency-free check.
fn scan_downloaded_keys(host_hf_cache: Option<&str>) -> std::collections::HashSet<String> {
    let mut keys = std::collections::HashSet::new();
    let Some(cache) = host_hf_cache else {
        return keys;
    };
    let hub = std::path::Path::new(cache).join("hub");
    let Ok(entries) = std::fs::read_dir(&hub) else {
        return keys;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(repo) = name.strip_prefix("models--") else {
            continue;
        };
        // `models--org--name` → `org/name`; normalize_key drops the org and
        // canonicalizes the rest, matching how live models are keyed.
        let repo_id = repo.replace("--", "/");
        keys.insert(libttstation::catalog::normalize_key(&repo_id));
    }
    keys
}

/// Artifacts `run.py` prints to stdout on a successful launch: the
/// underlying Docker container id, the path to the container's own log file
/// (streamed by `run.py` alongside the container), and the path to `run.py`'s
/// own log file. All optional -- `run.py`'s output format may drift across
/// versions, and a launch can fail before any of these lines are printed at
/// all, so "missing" is a normal outcome, not a parse error.
#[derive(Debug, Default)]
struct RunArtifacts {
    container_id: Option<String>,
    container_log: Option<String>,
    run_log: Option<String>,
}

/// Extract the container id + log paths run.py prints on a successful launch.
/// All fields optional — run.py output format may drift; missing = None.
fn parse_run_artifacts(stdout: &str) -> RunArtifacts {
    let mut a = RunArtifacts::default();
    for line in stdout.lines() {
        if let Some(rest) = line.split("Created Docker container ID:").nth(1) {
            a.container_id = Some(rest.trim().to_string());
        } else if let Some(rest) = line
            .split("Docker logs are also streamed to log file:")
            .nth(1)
        {
            a.container_log = Some(rest.trim().to_string());
        } else if let Some(rest) = line
            .split("This log file is saved on local machine at:")
            .nth(1)
        {
            a.run_log = Some(rest.trim().to_string());
        }
    }
    a
}

#[cfg(test)]
mod runpy_artifact_tests {
    use super::*;

    #[test]
    fn parses_container_id_and_log_paths_from_runpy_stdout() {
        let out = "\
2026-07-07 13:52:50 - run_docker_server.py:352 - INFO: Created Docker container ID: 5d2dd4b5c9d9
2026-07-07 13:52:50 - run_docker_server.py:354 - INFO: Docker logs are also streamed to log file: /home/ttuser/code/tt-inference-server/workflow_logs/docker_server/vllm_x.log
2026-07-07 13:52:50 - run.py:731 - INFO: This log file is saved on local machine at: /home/ttuser/code/tt-inference-server/workflow_logs/run_logs/run_x.log";
        let a = parse_run_artifacts(out);
        assert_eq!(a.container_id.as_deref(), Some("5d2dd4b5c9d9"));
        assert!(a
            .container_log
            .as_deref()
            .unwrap()
            .ends_with("docker_server/vllm_x.log"));
        assert!(a
            .run_log
            .as_deref()
            .unwrap()
            .ends_with("run_logs/run_x.log"));
    }

    #[test]
    fn parse_run_artifacts_tolerates_missing_fields() {
        let a = parse_run_artifacts("nothing useful here");
        assert!(a.container_id.is_none() && a.container_log.is_none() && a.run_log.is_none());
    }
}
