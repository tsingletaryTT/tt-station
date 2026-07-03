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

use tt_station_agentd::routes::{app, AppState};
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
    #[arg(long)]
    name: String,

    /// Control-plane HTTP port to listen on and advertise in the `ctrl` TXT key.
    #[arg(long = "ctrl-port")]
    ctrl_port: u16,

    /// Which serving backend to use. `serving::make_backend` turns this
    /// into the real `ServingBackend` trait object `/run`/`/stop` delegate
    /// to. Defaults to `runpy` -- see `Backend`'s doc comment.
    #[arg(long, value_enum, default_value_t = Backend::Runpy)]
    backend: Backend,

    /// Chip inventory string advertised in the `chips` TXT key and returned
    /// from `/status`.
    #[arg(long, default_value = "4xBH")]
    chips: String,

    /// API version advertised in the `apiver` TXT key.
    #[arg(long, default_value_t = 1)]
    apiver: u8,

    /// Host the serving container/VM is reachable on, baked into the
    /// `base_url` of any `Endpoint` `/run` returns. Only meaningful for the
    /// Docker backend today. Defaults to loopback since the PoC's client and
    /// agent are expected to run on the same box; a real deployment would
    /// pass the box's LAN address.
    #[arg(long = "serving-host", default_value = "127.0.0.1")]
    serving_host: String,

    /// Host port the serving container/VM's HTTP port is mapped to. Only
    /// meaningful for the Docker backend today.
    #[arg(long = "serving-port", default_value_t = 8000)]
    serving_port: u16,

    /// Container image to run the resolved model in
    /// (`run.py --override-docker-image`, or `docker run <image>` for the
    /// `docker` fallback backend).
    ///
    /// OPTIONAL for the `runpy` backend (the default): without it, `run.py`
    /// picks the correct image itself from the model's own entry in
    /// `model_spec.json` -- the flag name says it all, it's an OVERRIDE, not
    /// a requirement, and is normally unnecessary. Setting this bypasses
    /// that resolution.
    ///
    /// The `docker` fallback backend has no such resolution of its own (it
    /// has no `model_spec.json` to consult), so when this is unset it falls
    /// back to `DEFAULT_DOCKER_SERVING_IMAGE` below -- an EXAMPLE tag that
    /// MUST be reviewed and pinned before real use. See
    /// `docs/reference/tt-inference-server-docker.md`.
    #[arg(long = "serving-image")]
    serving_image: Option<String>,

    /// `--tt-device` value passed to `tt-inference-server`, e.g. `n300`,
    /// `p150x4`, `p300x2`. Shared by both the `runpy` and `docker` backends.
    ///
    /// OPTIONAL for the `runpy` backend (the default): per `run.py --help`,
    /// omitting it "Defaults to the largest supported device available on
    /// the host" -- i.e. `run.py` detects this box's hardware and picks the
    /// mesh itself. Setting this OVERRIDES that auto-detection and is
    /// normally unnecessary.
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
    #[arg(long = "cache-volume", default_value = "tt-station-cache")]
    cache_volume: String,

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
    #[arg(long = "device-path", default_value = "/dev/tenstorrent")]
    device_path: String,

    /// Host path bind-mounted onto itself inside the container (`--mount
    /// type=bind,src=...,dst=...`) for tt-metal's 1G-hugepages DMA
    /// requirement. Only meaningful for the Docker backend today.
    #[arg(long = "hugepages-src", default_value = "/dev/hugepages-1G")]
    hugepages_src: String,

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
    #[arg(long = "model-source", default_value = "huggingface")]
    model_source: String,

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

/// Resolve the default `tt-inference-server` checkout to use when
/// `--tt-inference-repo` isn't given: prefer a vendored copy at
/// `./vendor/tt-inference-server` (relative to the current working
/// directory the agent was launched from) if one exists on disk, else fall
/// back to `$HOME/code/tt-inference-server` -- the operator's convention
/// for standalone checkouts elsewhere on this box.
fn default_tt_inference_repo() -> String {
    let vendored = std::path::Path::new("./vendor/tt-inference-server");
    if vendored.exists() {
        return vendored.to_string_lossy().into_owned();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/code/tt-inference-server")
}

/// Resolve the default Hugging Face cache path used when `--host-hf-cache`
/// isn't given: `$HOME/.cache/huggingface`, matching the operator's real
/// `HF_HOME`/`huggingface-cli` default rather than a hardcoded absolute
/// path baked in at build time.
fn default_host_hf_cache() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.cache/huggingface")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `--hf-token` wins if given explicitly; otherwise fall back to the
    // `HF_TOKEN` environment variable so operators can keep the token out of
    // shell history / process listings. Either way, only pass through a
    // non-empty value -- `DockerBackend` already guards on this too, but
    // resolving it here keeps `main`'s CLI-to-config mapping honest about
    // where the value actually comes from.
    let hf_token = cli
        .hf_token
        .clone()
        .or_else(|| std::env::var("HF_TOKEN").ok())
        .filter(|token| !token.is_empty());

    // `DockerBackend` (the manual escape hatch -- see `Backend`'s doc
    // comment) has no auto-resolution of its own the way `run.py` does, so
    // it needs CONCRETE device/image values even when the operator didn't
    // pass `--tt-device`/`--serving-image` -- fall back to this box's known
    // values rather than leaving it half-configured. `RunPyConfig` below
    // deliberately does NOT do this: it passes the raw `Option`s straight
    // through so `run.py` can auto-resolve them itself.
    let docker_config = DockerConfig {
        image: cli
            .serving_image
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_SERVING_IMAGE.to_string()),
        host: cli.serving_host.clone(),
        host_port: cli.serving_port,
        tt_device: cli
            .tt_device
            .clone()
            .unwrap_or_else(|| DEFAULT_DOCKER_TT_DEVICE.to_string()),
        hf_token,
        cache_volume: cli.cache_volume.clone(),
        no_auth: !cli.require_auth,
        device_path: cli.device_path.clone(),
        hugepages_src: cli.hugepages_src.clone(),
    };

    let runpy_config = RunPyConfig {
        repo_dir: cli
            .tt_inference_repo
            .clone()
            .unwrap_or_else(default_tt_inference_repo),
        host: cli.serving_host.clone(),
        service_port: cli.serving_port,
        no_auth: !cli.require_auth,
        model_source: cli.model_source.clone(),
        // `--host-hf-cache` isn't part of run.py's device/image/impl/engine
        // auto-resolution (see the module doc in serving/runpy.rs) -- it's
        // just a real host path this codebase always wants bind-mounted, so
        // (unlike tt_device/image/impl/engine below) this always resolves
        // to `Some`, never passed through as a bare, possibly-absent
        // `Option`.
        host_hf_cache: Some(
            cli.host_hf_cache
                .clone()
                .unwrap_or_else(default_host_hf_cache),
        ),
        // Passed straight through as `Option`s -- `None` here means "let
        // run.py auto-resolve it," which is the DEFAULT for a fresh CLI
        // invocation (see each flag's own doc comment above). Do NOT apply
        // a fallback the way `docker_config` above does.
        tt_device: cli.tt_device.clone(),
        image: cli.serving_image.clone(),
        engine: cli.engine.clone(),
        impl_name: cli.impl_name.clone(),
        device_id: cli.device_id.clone(),
        model_spec_path: cli.model_spec.clone(),
        // `--no-device-reset` is an opt-OUT flag (default `false`), so the
        // real default here is `reset_before_serve: true` -- see
        // `RunPyConfig::reset_before_serve`'s doc comment for why resetting
        // before every serve is the robust default.
        reset_before_serve: !cli.no_device_reset,
        reset_cmd: vec!["tt-smi".to_string(), "-r".to_string()],
    };

    let backend = make_backend(&cli.backend.to_string(), docker_config, runpy_config)
        .context("failed to construct serving backend")?;

    let state = AppState::new(cli.name.clone(), cli.chips.clone(), Arc::from(backend));

    // Bind the control-plane socket FIRST, then advertise on the LAN, so
    // discovery never races ahead of the control-plane API actually being
    // reachable.
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.ctrl_port))
        .await
        .with_context(|| format!("failed to bind control port {}", cli.ctrl_port))?;

    // Advertise the box's current status (read from the same `AppState` that
    // backs `/status`) so the mDNS TXT record and the HTTP status endpoint
    // can never desync at boot -- there's exactly one source of truth.
    let _mdns_guard =
        advertise(&cli, state.status()).context("failed to start mDNS advertisement")?;

    println!(
        "tt-station-agentd: '{}' serving on port {} (backend={}, chips={})",
        cli.name, cli.ctrl_port, cli.backend, cli.chips
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
struct MdnsGuard {
    daemon: ServiceDaemon,
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

/// Build a [`BoxRecord`] from CLI flags plus the box's current `status`,
/// encode it into mDNS TXT records via `libttstation`'s `txt_encode` (the
/// exact same helper `mock-box` uses, so the keys can't drift from what
/// `MdnsProvider` decodes), and register the `_tenstorrent._tcp` service
/// with the local mDNS responder.
///
/// `status` is passed in (rather than hardcoded) so the caller can source it
/// straight from the same `AppState` that backs `/status` -- one source of
/// truth for what the box's status is at boot.
fn advertise(cli: &Cli, status: ServingStatus) -> Result<MdnsGuard> {
    let host = format!("{}.local.", cli.name);
    let record = BoxRecord {
        name: cli.name.clone(),
        host: host.clone(),
        ctrl_port: cli.ctrl_port,
        chips: cli.chips.clone(),
        status,
        apiver: cli.apiver,
    };

    let txt_pairs = txt_encode(&record);
    let txt_refs: Vec<(&str, &str)> = txt_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;

    // Empty address + enable_addr_auto() lets mdns-sd discover this host's
    // real LAN address(es) instead of us hardcoding one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &record.name,
        &host,
        "",
        cli.ctrl_port,
        &txt_refs[..],
    )
    .context("failed to build mDNS ServiceInfo")?
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    daemon
        .register(service_info)
        .context("failed to register mDNS service")?;

    Ok(MdnsGuard { daemon, fullname })
}
