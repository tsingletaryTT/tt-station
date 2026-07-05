//! `tt-station-agentd`: the box-side daemon that runs on a QuietBox.
//!
//! Bootstraps shared `AppState`, advertises the box on the LAN via mDNS
//! (`_tenstorrent._tcp`, same TXT-record shape `mock-box` uses -- see Task
//! 3), and serves the HTTP control-plane API (`GET /status` today; pairing
//! routes, control routes, and a real serving backend arrive in Tasks
//! 7/9/10 and extend `AppState` rather than replacing it).
//!
//! Keep this file to bootstrap only: parse args, build state, spawn mDNS,
//! serve. Route handlers live in `routes.rs`.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use libttstation::discovery::SERVICE_TYPE;
use libttstation::model::{txt_encode, BoxRecord, ServingStatus};
use mdns_sd::{ServiceDaemon, ServiceInfo};

use tt_station_agentd::config;
use tt_station_agentd::routes::{app, AppState, StatusAdvertiser};
use tt_station_agentd::serving::docker::DockerConfig;
use tt_station_agentd::serving::make_backend;
use tt_station_agentd::serving::runpy::RunPyConfig;

/// Which serving backend to use for running models.
///
/// `Runpy` is the DEFAULT: it's how the operator's PROVEN scripts actually
/// launch LLMs (`tt-inference-server/run.py`, not a hand-rolled `docker
/// run` -- see `docs/reference/tt-inference-server-docker.md`'s "⭐ Ground
/// truth" section). `Docker` remains available as a best-effort fallback
/// for when `run.py`/its repo checkout isn't available. `Dstack` is the M4
/// direction and still an intentional stub.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Runpy,
    Docker,
    Dstack,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Runpy => write!(f, "runpy"),
            Backend::Docker => write!(f, "docker"),
            Backend::Dstack => write!(f, "dstack"),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "tt-station-agentd",
    about = "Box-side daemon for a Tenstorrent QuietBox"
)]
struct Cli {
    /// Box name; used as both the mDNS instance name and the `name` TXT/JSON key.
    ///
    /// `Option` because it can also come from `[global].name` in the config
    /// file -- `resolve` enforces that ONE of the two is present, so this
    /// remains effectively required for an operator not using a config file.
    #[arg(long)]
    name: Option<String>,

    /// Control-plane HTTP port to listen on and advertise in the `ctrl` TXT key.
    ///
    /// `Option` for the same reason as `--name` -- see its doc comment.
    #[arg(long = "ctrl-port")]
    ctrl_port: Option<u16>,

    /// Which serving backend to use. `serving::make_backend` turns this
    /// into the real `ServingBackend` trait object `/run`/`/stop` delegate
    /// to. Defaults to `runpy` -- see `Backend`'s doc comment.
    ///
    /// No clap default: `resolve` supplies `"runpy"` when this, the active
    /// profile, and `[global]` are all absent.
    #[arg(long, value_enum)]
    backend: Option<Backend>,

    /// Chip inventory string advertised in the `chips` TXT key and returned
    /// from `/status`.
    ///
    /// No clap default: `resolve` supplies `"4xBH"` when this and
    /// `[global].chips` are both absent.
    #[arg(long)]
    chips: Option<String>,

    /// API version advertised in the `apiver` TXT key.
    ///
    /// No clap default: `resolve` supplies `1` when this and
    /// `[global].apiver` are both absent.
    #[arg(long)]
    apiver: Option<u8>,

    /// Host the serving container/VM is reachable on, baked into the
    /// `base_url` of any `Endpoint` `/run` returns. Only meaningful for the
    /// Docker backend today. Defaults to loopback since the PoC's client and
    /// agent are expected to run on the same box; a real deployment would
    /// pass the box's LAN address.
    ///
    /// No clap default: `resolve` supplies `"127.0.0.1"` when this and the
    /// active profile's `serving_host` are both absent.
    #[arg(long = "serving-host")]
    serving_host: Option<String>,

    /// Host port the serving container/VM's HTTP port is mapped to. Only
    /// meaningful for the Docker backend today.
    ///
    /// No clap default: `resolve` supplies `8000` when this and the active
    /// profile's `serving_port` are both absent.
    #[arg(long = "serving-port")]
    serving_port: Option<u16>,

    /// Container image to run the resolved model in
    /// (`run.py --override-docker-image`, or `docker run <image>` for the
    /// `docker` fallback backend).
    ///
    /// This is the RELIABLE per-box choice for the `runpy` backend (the
    /// default): pin it explicitly to whatever image is confirmed
    /// compatible with this checkout's `run.py` on this box. When unset,
    /// the agent does NOT guess -- `run.py` falls back to its own
    /// `model_spec.json` default image tag, which isn't always pulled/on
    /// GHCR on a given box (see `--auto-image` for an opt-in, but riskier,
    /// alternative to pinning this).
    ///
    /// The `docker` fallback backend has no such resolution of its own (it
    /// has no `model_spec.json` to consult), so when this is unset it falls
    /// back to `DEFAULT_DOCKER_SERVING_IMAGE` below -- an EXAMPLE tag that
    /// MUST be reviewed and pinned before real use. See
    /// `docs/reference/tt-inference-server-docker.md`.
    #[arg(long = "serving-image")]
    serving_image: Option<String>,

    /// Opt in to auto-picking the newest locally-present release image
    /// (`RunPyBackend::resolve_image`) when `--serving-image` is unset.
    /// Only meaningful for the `runpy` backend (the default).
    ///
    /// OFF by default because image<->run.py compatibility is a curated
    /// matrix -- a newer local image can be incompatible with this
    /// checkout's run.py (observed: run.py passes `--override-tt-config`,
    /// which a newer image's server rejects). Pin `--serving-image` per box
    /// unless you know only compatible images are present there.
    #[arg(long = "auto-image", action = clap::ArgAction::SetTrue)]
    auto_image: bool,

    /// `--tt-device` value passed to `tt-inference-server`, e.g. `n300`,
    /// `p150x4`, `p300x2`. Shared by both the `runpy` and `docker` backends.
    ///
    /// OPTIONAL override for the `runpy` backend (the default). When unset,
    /// the agent auto-detects the device from `tt-smi`
    /// (`RunPyBackend::resolve_tt_device` -- `run.py`'s own hardware
    /// auto-detect is known to fail on some boards, e.g. this one); set
    /// this only to override that.
    ///
    /// The `docker` fallback backend has no auto-detection of its own, so
    /// when this is unset it falls back to `"p300x2"` -- CONFIRMED as the
    /// string for *this* box (a P300X2 machine, 4x p300c) in
    /// `docs/reference/tt-inference-server-docker.md`'s "Device string is
    /// box- AND model-specific" section. `p150x4` is the OTHER Blackhole
    /// "BH QuietBox" variant, not this box -- pass this flag explicitly if
    /// you're actually targeting that hardware with the `docker` backend.
    #[arg(long = "tt-device")]
    tt_device: Option<String>,

    /// Hugging Face access token for gated model repos (e.g. Llama), passed
    /// into the serving container as `--env HF_TOKEN=...`. Only meaningful
    /// for the Docker backend today.
    ///
    /// If not given on the command line, falls back to the `HF_TOKEN`
    /// environment variable. Passed through to the container only when the
    /// resulting value is non-empty -- most local/open models need no token
    /// at all.
    #[arg(long = "hf-token")]
    hf_token: Option<String>,

    /// Name of the Docker volume mounted at
    /// `/home/container_app_user/cache_root` inside the serving container,
    /// used to persist downloaded model weights/HF cache across container
    /// restarts. Only meaningful for the Docker backend today.
    ///
    /// No clap default: `resolve` supplies `"tt-station-cache"` when unset.
    #[arg(long = "cache-volume")]
    cache_volume: Option<String>,

    /// Require JWT bearer auth on the serving container instead of running
    /// it with `--no-auth`. Only meaningful for the Docker backend today.
    ///
    /// Defaults to `false` -- i.e. the server runs with `--no-auth` by
    /// default -- for PoC simplicity, since minting/managing a JWT
    /// client-side is out of scope here.
    #[arg(long = "require-auth", action = clap::ArgAction::SetTrue)]
    require_auth: bool,

    /// Host path passed to `docker run --device` so the container can reach
    /// the Tenstorrent accelerator. Only meaningful for the Docker backend
    /// today.
    ///
    /// No clap default: `resolve` supplies `"/dev/tenstorrent"` when unset.
    #[arg(long = "device-path")]
    device_path: Option<String>,

    /// Host path bind-mounted onto itself inside the container (`--mount
    /// type=bind,src=...,dst=...`) for tt-metal's 1G-hugepages DMA
    /// requirement. Only meaningful for the Docker backend today.
    ///
    /// No clap default: `resolve` supplies `"/dev/hugepages-1G"` when unset.
    #[arg(long = "hugepages-src")]
    hugepages_src: Option<String>,

    /// Local checkout of `tt-inference-server`, whose `run.py` is the
    /// ground-truth way to launch LLM serving (see
    /// `docs/reference/tt-inference-server-docker.md`). Only meaningful for
    /// the `runpy` backend.
    ///
    /// No static default: resolved at startup by `default_tt_inference_repo`
    /// so operators who vendor the repo (`<checkout>/vendor/tt-inference-server`)
    /// get that for free, while a bare clone falls back to
    /// `$HOME/code/tt-inference-server` -- the operator's convention
    /// elsewhere on this box.
    #[arg(long = "tt-inference-repo")]
    tt_inference_repo: Option<String>,

    /// Host path bind-mounted for the Hugging Face weights cache
    /// (`run.py`'s `--host-hf-cache`). Only meaningful for the `runpy`
    /// backend.
    ///
    /// No static default: resolved at startup as `$HOME/.cache/huggingface`
    /// so it doesn't hardcode a stale absolute path for whichever operator
    /// happens to build this.
    #[arg(long = "host-hf-cache")]
    host_hf_cache: Option<String>,

    /// `run.py`'s `--engine` flag, e.g. `vllm`. Only meaningful for the
    /// `runpy` backend.
    ///
    /// OPTIONAL: `run.py` defaults it to the model's own entry in
    /// `model_spec.json` when omitted. Setting this OVERRIDES that
    /// resolution and is normally unnecessary.
    #[arg(long = "engine")]
    engine: Option<String>,

    /// `run.py`'s `--impl` flag, e.g. `tt-transformers`. Only meaningful for
    /// the `runpy` backend.
    ///
    /// OPTIONAL: `run.py` defaults it to the model's own entry in
    /// `model_spec.json` when omitted. Setting this OVERRIDES that
    /// resolution and is normally unnecessary.
    #[arg(long = "impl")]
    impl_name: Option<String>,

    /// `run.py`'s `--device-id` flag, e.g. `0,1`, to pin serving to specific
    /// chips. Only meaningful for the `runpy` backend. Omitted from the
    /// `run.py` invocation entirely when not given -- most runs let `run.py`
    /// pick the device mesh itself.
    #[arg(long = "device-id")]
    device_id: Option<String>,

    /// `run.py`'s `MODEL_SOURCE` environment variable, e.g. `huggingface`.
    /// Only meaningful for the `runpy` backend.
    ///
    /// No clap default: `resolve` supplies `"huggingface"` when unset.
    #[arg(long = "model-source")]
    model_source: Option<String>,

    /// Path to `model_spec.json` -- the ground-truth model/device-mesh
    /// catalog `run.py` validates `--model`/`--tt-device` against, and that
    /// `RunPyBackend::list_models` (`GET /models`, `tt models`) reads to
    /// enumerate what this box can serve. Only meaningful for the `runpy`
    /// backend.
    ///
    /// OPTIONAL: when omitted, `RunPyBackend` itself resolves this to
    /// `<tt-inference-repo>/model_spec.json` at call time (see
    /// `RunPyBackend::model_spec_path`), so this file doesn't need to
    /// duplicate `default_tt_inference_repo`'s logic.
    #[arg(long = "model-spec")]
    model_spec: Option<String>,

    /// Skip the `tt-smi -r` board reset before serving. The reset clears
    /// wedged mesh ethernet cores left by a previously-stopped model;
    /// disable only on boards where it's unwanted or `tt-smi` is
    /// unavailable. Only meaningful for the `runpy` backend.
    #[arg(long = "no-device-reset", action = clap::ArgAction::SetTrue)]
    no_device_reset: bool,

    /// File to persist issued bearer tokens (from `/pair/complete`) to, so a
    /// paired client (e.g. the macOS app) doesn't have to re-pair every time
    /// this agent process restarts. Tokens are bearer secrets: the file (and
    /// its parent directory, if this agent creates it) is written mode
    /// `0600`/`0700` on unix.
    ///
    /// No static default: resolved at startup by `default_token_store` to
    /// `$HOME/.config/tt-station/agentd-tokens.json`, same pattern as
    /// `--host-hf-cache`. Ignored entirely when `--no-token-persistence` is
    /// set.
    #[arg(long = "token-store")]
    token_store: Option<String>,

    /// Opt OUT of persisting bearer tokens across restarts: with this set,
    /// `--token-store` is ignored and the agent behaves exactly as it did
    /// before this feature existed -- issued tokens live in memory only, so
    /// every restart forces every paired client to re-pair.
    ///
    /// Off by default because the whole point of `--token-store` is to
    /// spare the common case (an agent that gets restarted -- a reboot, a
    /// `systemctl restart`, an upgrade) from re-pairing; pass this only if
    /// persisting bearer secrets to disk on this box is unacceptable for
    /// some reason.
    #[arg(long = "no-token-persistence", action = clap::ArgAction::SetTrue)]
    no_token_persistence: bool,

    /// Interval (milliseconds) between `tt-smi -s` telemetry snapshots pushed
    /// on the `GET /telemetry` WebSocket stream (the publisher half of the
    /// "remote QuietBox" feature -- see src/telemetry.rs). Each connected
    /// client receives one frame per interval. Defaults to `1000` (1s), a
    /// live-but-not-hammering cadence for a chip-telemetry dashboard.
    ///
    /// No clap default: `resolve` supplies `1000` when this and
    /// `[global].telemetry_interval_ms` are both absent. The range validator
    /// still applies to whatever value IS passed on the command line.
    #[arg(long = "telemetry-interval-ms", value_parser = clap::value_parser!(u64).range(1..))]
    telemetry_interval_ms: Option<u64>,

    /// `tt-smi` binary the `GET /telemetry` stream runs (as `<bin> -s`) to
    /// collect each snapshot. Defaults to `tt-smi`, resolved on `$PATH`; set
    /// this to an absolute path when `tt-smi` isn't on the agent's `$PATH`.
    ///
    /// No clap default: `resolve` supplies `"tt-smi"` when this and
    /// `[global].tt_smi_bin` are both absent.
    #[arg(long = "tt-smi-bin")]
    tt_smi_bin: Option<String>,

    /// Path to agentd.toml. Defaults to `$TT_CONFIG_DIR/agentd.toml` if
    /// `TT_CONFIG_DIR` is set, else `$HOME/.config/tt-station/agentd.toml`.
    /// An explicit path that is missing/unreadable is a hard error.
    #[arg(long)]
    config: Option<String>,

    /// Name of the `[profile.<name>]` in the config file to activate.
    /// Overrides `default_profile`. Errors if the named profile is absent.
    #[arg(long)]
    profile: Option<String>,

    /// Resolve the config, print it (secrets redacted) as JSON, and exit
    /// without binding the control port. For verifying precedence/profiles.
    #[arg(long = "print-config", action = clap::ArgAction::SetTrue)]
    print_config: bool,
}

/// `docker` fallback-backend default serving image, used only when
/// `--serving-image` is omitted AND `--backend docker` is selected. The
/// `runpy` backend (the default) never uses this -- it lets `run.py`
/// resolve the image itself; see `--serving-image`'s doc comment.
///
/// NO `latest` tag exists for `tt-inference-server` -- tags are
/// `<semver>-<tt-metal-commit>-<vllm-commit>` (e.g. `0.9.0-84b4c53-222ee06`).
/// This is an EXAMPLE tag only; it MUST be reviewed and pinned to the tag
/// actually intended for a given release before real use. See
/// `docs/reference/tt-inference-server-docker.md`.
const DEFAULT_DOCKER_SERVING_IMAGE: &str =
    "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.9.0-84b4c53-222ee06";

/// `docker` fallback-backend default `--tt-device`, used only when
/// `--tt-device` is omitted AND `--backend docker` is selected. The `runpy`
/// backend (the default) never uses this -- it lets `run.py` auto-detect
/// the device mesh itself; see `--tt-device`'s doc comment.
const DEFAULT_DOCKER_TT_DEVICE: &str = "p300x2";

/// Build the redacted `ConfigSummary` (Task 4 type) from a `ResolvedConfig`.
/// NEVER includes `hf_token` or token-store contents.
fn config_summary(rc: &config::ResolvedConfig) -> libttstation::model::ConfigSummary {
    libttstation::model::ConfigSummary {
        active_profile: rc.active_profile.clone(),
        available_profiles: rc.available_profiles.clone(),
        backend: rc.backend.clone(),
        serving_host: rc.serving_host.clone(),
        serving_port: rc.serving_port,
        serving_image: rc.serving_image.clone(),
        tt_inference_repo: Some(rc.tt_inference_repo.clone()),
        tt_device: rc.tt_device.clone(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Config file path: explicit --config wins; else $TT_CONFIG_DIR/agentd.toml
    // if set; else $HOME/.config/tt-station/agentd.toml.
    let explicit = cli.config.is_some();
    let config_path = cli.config.clone().map(std::path::PathBuf::from).unwrap_or_else(|| {
        let dir = std::env::var("TT_CONFIG_DIR").unwrap_or_else(|_| {
            format!("{}/.config/tt-station", std::env::var("HOME").unwrap_or_default())
        });
        std::path::PathBuf::from(dir).join("agentd.toml")
    });
    let file = config::load_config(&config_path, explicit).context("failed to load config file")?;

    // `--hf-token` wins if given explicitly; otherwise `resolve` falls back
    // to the `HF_TOKEN` environment variable so operators can keep the token
    // out of shell history / process listings -- see `config::resolve`'s
    // precedence (flag > env > profile > global > default).
    let env_hf_token = std::env::var("HF_TOKEN").ok();

    // Every overridable flag, `None` where the operator didn't pass it, so
    // `resolve` can layer env/profile/global/built-in defaults underneath.
    // This is the ONLY place CLI flags are read directly in `main` from here
    // on -- everything past `resolve` reads from `rc: ResolvedConfig`.
    let overrides = config::CliOverrides {
        name: cli.name.clone(),
        ctrl_port: cli.ctrl_port,
        chips: cli.chips.clone(),
        apiver: cli.apiver,
        token_store: cli.token_store.clone(),
        no_token_persistence: cli.no_token_persistence,
        telemetry_interval_ms: cli.telemetry_interval_ms,
        tt_smi_bin: cli.tt_smi_bin.clone(),
        backend: cli.backend.map(|b| b.to_string()),
        tt_inference_repo: cli.tt_inference_repo.clone(),
        serving_image: cli.serving_image.clone(),
        auto_image: cli.auto_image,
        tt_device: cli.tt_device.clone(),
        serving_host: cli.serving_host.clone(),
        serving_port: cli.serving_port,
        host_hf_cache: cli.host_hf_cache.clone(),
        hf_token: cli.hf_token.clone(),
        no_device_reset: cli.no_device_reset,
        cache_volume: cli.cache_volume.clone(),
        require_auth: cli.require_auth,
        device_path: cli.device_path.clone(),
        hugepages_src: cli.hugepages_src.clone(),
        engine: cli.engine.clone(),
        impl_name: cli.impl_name.clone(),
        device_id: cli.device_id.clone(),
        model_source: cli.model_source.clone(),
        model_spec: cli.model_spec.clone(),
    };
    let rc = config::resolve(overrides, env_hf_token, file, cli.profile.as_deref())
        .context("failed to resolve configuration")?;

    // `--print-config` is purely diagnostic: resolve, print the redacted
    // summary, exit -- WITHOUT binding the control port or touching mDNS.
    // Lets an operator verify precedence/profile selection without actually
    // starting the daemon.
    if cli.print_config {
        let summary = config_summary(&rc);
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    // `DockerBackend` (the manual escape hatch -- see `Backend`'s doc
    // comment) has no auto-resolution of its own the way `run.py` does, so
    // it needs CONCRETE device/image values even when the operator didn't
    // pass `--tt-device`/`--serving-image` -- fall back to this box's known
    // values rather than leaving it half-configured. `RunPyConfig` below
    // deliberately does NOT do this: it passes the raw `Option`s straight
    // through so `run.py` can auto-resolve them itself.
    let docker_config = DockerConfig {
        image: rc
            .serving_image
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_SERVING_IMAGE.to_string()),
        host: rc.serving_host.clone(),
        host_port: rc.serving_port,
        tt_device: rc
            .tt_device
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_TT_DEVICE.to_string()),
        hf_token: rc.hf_token.clone(),
        cache_volume: rc.cache_volume.clone(),
        no_auth: !rc.require_auth,
        device_path: rc.device_path.clone(),
        hugepages_src: rc.hugepages_src.clone(),
    };

    let runpy_config = RunPyConfig {
        repo_dir: rc.tt_inference_repo.clone(),
        host: rc.serving_host.clone(),
        service_port: rc.serving_port,
        no_auth: !rc.require_auth,
        model_source: rc.model_source.clone(),
        // `--host-hf-cache` isn't part of run.py's device/image/impl/engine
        // auto-resolution (see the module doc in serving/runpy.rs) -- it's
        // just a real host path this codebase always wants bind-mounted, so
        // (unlike tt_device/image/impl/engine below) this always resolves
        // to `Some`, never passed through as a bare, possibly-absent
        // `Option`. `resolve` already applied `default_host_hf_cache`/tilde
        // expansion, so this is always a concrete path by the time we get here.
        host_hf_cache: Some(rc.host_hf_cache.clone()),
        // Passed straight through as `Option`s -- `None` here means "auto-
        // resolve it," which `RunPyBackend::start` does itself via
        // `resolve_tt_device`/`resolve_image` (see each flag's own doc
        // comment above, and the module doc in serving/runpy.rs). Do NOT
        // apply a fallback the way `docker_config` above does -- that would
        // bypass auto-resolution.
        tt_device: rc.tt_device.clone(),
        image: rc.serving_image.clone(),
        // Opt-in only -- see `--auto-image`'s doc comment and
        // `RunPyConfig::auto_image`/`RunPyBackend::resolve_image` for why
        // this defaults to `false` (image<->run.py compatibility is a
        // curated matrix, not something "newest locally-present" can
        // safely stand in for).
        auto_image: rc.auto_image,
        engine: rc.engine.clone(),
        impl_name: rc.impl_name.clone(),
        device_id: rc.device_id.clone(),
        model_spec_path: rc.model_spec.clone(),
        // `--no-device-reset` is an opt-OUT flag (default `false`), so the
        // real default here is `reset_before_serve: true` -- see
        // `RunPyConfig::reset_before_serve`'s doc comment for why resetting
        // before every serve is the robust default.
        reset_before_serve: !rc.no_device_reset,
        reset_cmd: vec!["tt-smi".to_string(), "-r".to_string()],
    };

    let backend = make_backend(&rc.backend, docker_config, runpy_config)
        .context("failed to construct serving backend")?;

    // Persist issued bearer tokens across restarts by default (see
    // `--token-store`'s doc comment) -- `--no-token-persistence` (folded into
    // `rc.token_store` being `None` by `resolve`) opts back out to the
    // pre-persistence in-memory-only behavior.
    let backend: Arc<dyn tt_station_agentd::serving::ServingBackend> = Arc::from(backend);
    let state = match &rc.token_store {
        None => AppState::new(rc.name.clone(), rc.chips.clone(), backend),
        Some(path) => {
            println!("tt-station-agentd: persisting bearer tokens to {}", path.display());
            AppState::new_persisting(rc.name.clone(), rc.chips.clone(), backend, path.clone())
        }
    };

    // Configure the additive `GET /telemetry` stream (see src/telemetry.rs).
    // Applied here, before any clone of `state` exists, for the same
    // sole-owner reason `with_status_advertiser` is (both rely on
    // `Arc::get_mut`). No-op for every existing route -- purely additive.
    let state = state.with_telemetry_config(rc.tt_smi_bin.clone(), rc.telemetry_interval_ms);

    // Configure the additive `GET /serving` discovery route (see routes.rs):
    // the serving host baked into discovered endpoints' `base_url`, and the
    // agent's own serving port used to classify `agent` vs `external`. Applied
    // here, before any clone of `state` exists, for the same sole-owner reason
    // `with_telemetry_config`/`with_status_advertiser` are (all rely on
    // `Arc::get_mut`). No-op for every existing route -- purely additive.
    let state = state.with_serving_config(rc.serving_host.clone(), rc.serving_port);

    // Configure the additive `GET /config` route (see routes.rs): a redacted
    // snapshot of the fully-resolved config, built once here from `rc` so it
    // can never drift from what the backend/state above were actually built
    // from. Applied here, before any clone of `state` exists, for the same
    // sole-owner reason the other `with_*` builders above are.
    let state = state.with_config_summary(config_summary(&rc));

    // Bind the control-plane socket FIRST, then advertise on the LAN, so
    // discovery never races ahead of the control-plane API actually being
    // reachable.
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", rc.ctrl_port))
        .await
        .with_context(|| format!("failed to bind control port {}", rc.ctrl_port))?;

    // Advertise the box's current status (read from the same `AppState` that
    // backs `/status`) so the mDNS TXT record and the HTTP status endpoint
    // can never desync at boot -- there's exactly one source of truth.
    // `advertise` hands back both the `MdnsGuard` (unregisters on drop, kept
    // alive for the process lifetime below) and an `MdnsStatusAdvertiser`
    // sharing the same underlying daemon, which gets attached to `state` so
    // `/run`/`/stop` can re-publish `status` whenever it changes instead of
    // it going stale after boot (see `StatusAdvertiser`'s doc comment).
    let (_mdns_guard, status_advertiser) =
        advertise(&rc, state.status()).context("failed to start mDNS advertisement")?;
    let state = state.with_status_advertiser(Arc::new(status_advertiser));

    println!(
        "tt-station-agentd: '{}' serving on port {} (backend={}, chips={})",
        rc.name, rc.ctrl_port, rc.backend, rc.chips
    );

    // Serve until a shutdown signal arrives, then return normally so
    // `_mdns_guard` drops and unregisters the mDNS service. Without this,
    // the usual way to stop a daemon (SIGINT/SIGTERM) would kill the
    // process before Rust destructors run, leaving the box falsely
    // advertised until the mDNS TTL expires.
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("agent HTTP server failed")?;

    println!("tt-station-agentd: shutdown signal received, unregistering mDNS and exiting");

    Ok(())
}

/// Resolves once a shutdown signal (Ctrl-C, or on Unix also SIGTERM) is
/// received, so it can be handed to `axum::serve(..).with_graceful_shutdown`.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Handle to the running mDNS advertisement. Unregisters and shuts down the
/// daemon on drop so the box cleanly disappears from discovery. `main`'s
/// graceful-shutdown handling (see `shutdown_signal`) ensures this drop runs
/// on a normal exit *and* on Ctrl-C/SIGTERM, not just process exit.
///
/// Holds an `Arc<ServiceDaemon>` shared with `MdnsStatusAdvertiser` (built
/// alongside this in `advertise`) rather than its own daemon, so a status
/// re-publish and the eventual shutdown-time unregister both talk to the
/// exact same mDNS responder thread.
struct MdnsGuard {
    daemon: Arc<ServiceDaemon>,
    fullname: String,
}

impl Drop for MdnsGuard {
    fn drop(&mut self) {
        if let Ok(receiver) = self.daemon.unregister(&self.fullname) {
            let _ = receiver.recv();
        }
        let _ = self.daemon.shutdown();
    }
}

/// Real, mDNS-backed [`StatusAdvertiser`] impl: re-publishes this box's
/// `status` TXT key by rebuilding the [`BoxRecord`]/TXT pairs with the new
/// status and re-registering a [`ServiceInfo`] under the *same* fullname
/// (instance name + service type + domain) on the daemon `advertise`
/// already started at boot.
///
/// Re-registering the same fullname on a live `ServiceDaemon` UPDATES the
/// existing advertisement (mdns-sd re-announces it) rather than erroring or
/// creating a duplicate -- that's what makes `/run`/`/stop` re-publishing
/// via this struct actually fix the staleness `docs/client-agent-integration-findings.md`
/// #1 describes, instead of needing a separate unregister/re-register dance.
///
/// Holds everything `advertise`'s original `BoxRecord` needed except
/// `status` itself (which changes per call and is instead the argument to
/// `advertise_status`) -- name, host, ctrl_port, chips, apiver are all
/// static for the process's lifetime.
struct MdnsStatusAdvertiser {
    daemon: Arc<ServiceDaemon>,
    name: String,
    host: String,
    ctrl_port: u16,
    chips: String,
    apiver: u8,
}

impl StatusAdvertiser for MdnsStatusAdvertiser {
    fn advertise_status(&self, status: &ServingStatus) {
        let record = BoxRecord {
            name: self.name.clone(),
            host: self.host.clone(),
            ctrl_port: self.ctrl_port,
            chips: self.chips.clone(),
            status: status.clone(),
            apiver: self.apiver,
        };

        let txt_pairs = txt_encode(&record);
        let txt_refs: Vec<(&str, &str)> = txt_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        // Mirror the boot-time `ServiceInfo::new(..).enable_addr_auto()` call
        // in `advertise` exactly, so the re-registered record is identical
        // in every field except `status` -- including the fullname mdns-sd
        // derives from `name`/`SERVICE_TYPE`/domain, which is what makes
        // this an UPDATE rather than a second, duplicate service.
        let service_info = match ServiceInfo::new(
            SERVICE_TYPE,
            &record.name,
            &self.host,
            "",
            self.ctrl_port,
            &txt_refs[..],
        ) {
            Ok(info) => info.enable_addr_auto(),
            Err(err) => {
                // Log and give up rather than panic: a failed re-publish
                // shouldn't fail (or crash) the `/run`/`/stop` request that
                // triggered it -- the control-plane state change already
                // succeeded, and a subsequent `/status` re-publish (or the
                // next `/run`/`/stop`) gets another chance.
                eprintln!(
                    "tt-station-agentd: failed to build mDNS ServiceInfo while re-publishing status: {err:#}"
                );
                return;
            }
        };

        if let Err(err) = self.daemon.register(service_info) {
            eprintln!("tt-station-agentd: failed to re-publish mDNS status: {err:#}");
        }
    }
}

/// Build a [`BoxRecord`] from CLI flags plus the box's current `status`,
/// encode it into mDNS TXT records via `libttstation`'s `txt_encode` (the
/// exact same helper `mock-box` uses, so the keys can't drift from what
/// `MdnsProvider` decodes), and register the `_tenstorrent._tcp` service
/// with the local mDNS responder.
///
/// `status` is passed in (rather than hardcoded) so the caller can source it
/// straight from the same `AppState` that backs `/status` -- one source of
/// truth for what the box's status is at boot.
///
/// Returns both the [`MdnsGuard`] (unregister/shutdown on drop, same as
/// before this function grew a second return value) and an
/// [`MdnsStatusAdvertiser`] sharing the same `Arc<ServiceDaemon>`, so `main`
/// can attach the latter to `AppState` and let `/run`/`/stop` keep the TXT
/// record's `status` key truthful after boot.
fn advertise(
    rc: &config::ResolvedConfig,
    status: ServingStatus,
) -> Result<(MdnsGuard, MdnsStatusAdvertiser)> {
    let host = format!("{}.local.", rc.name);
    let record = BoxRecord {
        name: rc.name.clone(),
        host: host.clone(),
        ctrl_port: rc.ctrl_port,
        chips: rc.chips.clone(),
        status,
        apiver: rc.apiver,
    };

    let txt_pairs = txt_encode(&record);
    let txt_refs: Vec<(&str, &str)> = txt_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let daemon = Arc::new(ServiceDaemon::new().context("failed to start mDNS daemon")?);

    // Empty address + enable_addr_auto() lets mdns-sd discover this host's
    // real LAN address(es) instead of us hardcoding one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &record.name,
        &host,
        "",
        rc.ctrl_port,
        &txt_refs[..],
    )
    .context("failed to build mDNS ServiceInfo")?
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    daemon
        .register(service_info)
        .context("failed to register mDNS service")?;

    let guard = MdnsGuard {
        daemon: Arc::clone(&daemon),
        fullname,
    };
    let status_advertiser = MdnsStatusAdvertiser {
        daemon,
        name: record.name,
        host,
        ctrl_port: rc.ctrl_port,
        chips: rc.chips.clone(),
        apiver: rc.apiver,
    };

    Ok((guard, status_advertiser))
}
