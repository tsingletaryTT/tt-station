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
use tt_station_agentd::device::detect_device_mesh;
use tt_station_agentd::gozer;
use tt_station_agentd::net;
use tt_station_agentd::routes::{app, AppState, StatusAdvertiser};
use tt_station_agentd::serving::docker::{CommandRunner, DockerConfig, RealCommandRunner};
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

    /// `gozer` binary this agent probes for once at startup (see
    /// `gozer::probe`) to arbitrate chip leases with other tenants on the
    /// same box. Optional -- gozer is not a hard dependency (see the
    /// `gozer` module doc): when unset, resolution falls back to `gozer` on
    /// `$PATH`, and if that isn't found either, this box simply serves
    /// whole-box with no leasing, exactly as it always has.
    ///
    /// No clap default: `resolve` supplies `None` (meaning "search $PATH")
    /// when this and `[global].gozer_path` are both absent -- unlike
    /// `--tt-smi-bin`, there's no built-in default STRING, since `gozer`'s
    /// own `$PATH` search already covers the common case.
    #[arg(long = "gozer-path")]
    gozer_path: Option<String>,

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

    /// Override the SSH account `POST`/`DELETE /ssh/authorize` (Task 2)
    /// installs/removes keys for. Defaults to the agent's own run-user
    /// (`$USER`, falling back to `whoami`, falling back to `"ttuser"`),
    /// with `authorized_keys` resolved under that user's own `$HOME`.
    ///
    /// When given explicitly, the target home is resolved as
    /// `/home/<name>/.ssh/authorized_keys` instead of `$HOME` -- e.g. an
    /// agent running as one user but installing keys for `ttuser`, the
    /// account a paired client actually wants to SSH in as (the common case
    /// on QuietBox 2, where `ttuser` is the run-user this whole agent
    /// assumes elsewhere -- see the module-level `CLAUDE.md`).
    #[arg(long = "ssh-user")]
    ssh_user: Option<String>,

    /// Override the `reset-chips` `POST /power` command (default:
    /// `tt-smi -r`). Space-separated argv, e.g. `--power-reset-chips-cmd
    /// tt-smi -r --some-flag`. Only wired via `with_power_config` when at
    /// least one `--power-*-cmd` flag is given; any flag left unset falls
    /// back to ITS OWN built-in default, not an empty command. See
    /// `docs/reference/power-controls.md`.
    #[arg(long = "power-reset-chips-cmd", num_args = 1..)]
    power_reset_chips_cmd: Option<Vec<String>>,

    /// Override the `suspend` `POST /power` command (default: `systemctl
    /// suspend`). See `--power-reset-chips-cmd` for the argv/fallback rules.
    #[arg(long = "power-suspend-cmd", num_args = 1..)]
    power_suspend_cmd: Option<Vec<String>>,

    /// Override the `reboot` `POST /power` command (default: `systemctl
    /// reboot`). See `--power-reset-chips-cmd` for the argv/fallback rules.
    #[arg(long = "power-reboot-cmd", num_args = 1..)]
    power_reboot_cmd: Option<Vec<String>>,

    /// Override the `shutdown` `POST /power` command (default: `systemctl
    /// poweroff`). See `--power-reset-chips-cmd` for the argv/fallback rules.
    #[arg(long = "power-shutdown-cmd", num_args = 1..)]
    power_shutdown_cmd: Option<Vec<String>>,
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

/// How long `detect_startup_device_mesh` waits for `<tt_smi_bin> -s` before
/// giving up and reporting `device_mesh: None`. `tt-smi` is "known to flake
/// under serving load" (see `telemetry_stream`'s doc comment in routes.rs)
/// and this codebase documents wedged mesh ethernet cores as a live
/// possibility on this hardware -- a hang here must not become a hang of
/// the whole daemon (see this fn's doc comment).
const STARTUP_DEVICE_MESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Detect this box's device-mesh label by running `<tt_smi_bin> -s` once at
/// startup, through the same `RealCommandRunner` argv-style command seam
/// `GET /telemetry` uses (see `telemetry::snapshot`/`collect_snapshot` in
/// routes.rs) -- reusing that seam rather than inventing a second subprocess
/// call for `tt-smi`.
///
/// Bounded, not just non-fatal: the blocking `RealCommandRunner::run` call
/// runs on `tokio::task::spawn_blocking` (same off-the-runtime discipline as
/// `collect_snapshot` in routes.rs) wrapped in `tokio::time::timeout(
/// STARTUP_DEVICE_MESH_TIMEOUT, ..)`, so a hung `tt-smi` (a real possibility
/// on this hardware -- wedged mesh ethernet cores, or the flakiness
/// `telemetry_stream` also guards against) can delay agent startup by AT
/// MOST ~10s, never indefinitely. The `main` call site awaits this before
/// `TcpListener::bind`, so that ~10s ceiling is the absolute worst case added
/// to boot time -- after which startup proceeds regardless.
///
/// Never fatal: a timeout, a `spawn_blocking` join failure (task panic), a
/// spawn/non-zero-exit error, or output `device::detect_device_mesh` can't
/// map to a known mesh all degrade to `None` with a distinguishing
/// `eprintln!` note, so a box without `tt-smi` on `$PATH` (or mid-reset, or
/// an unrecognized fleet, or a wedged `tt-smi`) still boots normally --
/// `/status` just reports `"device_mesh": null`.
async fn detect_startup_device_mesh(tt_smi_bin: &str) -> Option<String> {
    let bin = tt_smi_bin.to_string();
    let run_result = tokio::time::timeout(
        STARTUP_DEVICE_MESH_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let runner = RealCommandRunner;
            runner.run(&[bin.as_str(), "-s"])
        }),
    )
    .await;

    match run_result {
        // Ran to completion within the timeout, and the blocking task didn't panic.
        Ok(Ok(Ok(stdout))) => {
            let mesh = detect_device_mesh(&stdout);
            if mesh.is_none() {
                eprintln!(
                    "tt-station-agentd: '{tt_smi_bin} -s' output didn't map to a known device mesh; device_mesh will report null"
                );
            }
            mesh
        }
        // Ran to completion within the timeout, but `tt-smi` itself failed
        // (missing binary, non-zero exit, etc).
        Ok(Ok(Err(err))) => {
            eprintln!(
                "tt-station-agentd: failed to run '{tt_smi_bin} -s' for device-mesh detection: {err:#}; device_mesh will report null"
            );
            None
        }
        // The `spawn_blocking` task panicked.
        Ok(Err(join_err)) => {
            eprintln!(
                "tt-station-agentd: device-mesh detection task panicked: {join_err}; device_mesh will report null"
            );
            None
        }
        // Blew past STARTUP_DEVICE_MESH_TIMEOUT -- likely a hung/wedged
        // `tt-smi`. The spawned blocking task keeps running in the
        // background (there's no cooperative way to kill it), but we stop
        // waiting on it so startup can proceed.
        Err(_elapsed) => {
            eprintln!(
                "tt-station-agentd: '{tt_smi_bin} -s' timed out after {:?}; skipping device-mesh detection, device_mesh will report null",
                STARTUP_DEVICE_MESH_TIMEOUT
            );
            None
        }
    }
}

/// How long the startup `gozer` probe waits for `<path> --version` before
/// giving up and treating gozer as absent. Mirrors
/// `STARTUP_DEVICE_MESH_TIMEOUT` -- gozer is an external, optional binary
/// this agent doesn't control (see the `gozer` module doc), so a probe here
/// must not become a hang of the whole daemon.
const STARTUP_GOZER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Probe for `gozer` ONCE at startup (not per request), through the same
/// bounded `spawn_blocking` + `timeout` shape `detect_startup_device_mesh`
/// uses for `tt-smi`. gozer is OPTIONAL: a missing binary, a non-zero exit,
/// malformed output, a hang, or a `spawn_blocking` panic all degrade to
/// `None` -- never a startup failure. `gozer::probe` itself already logs the
/// found/absent outcome for every case it can see; this wrapper only adds
/// the timeout-specific case `probe` can't observe on its own (a genuinely
/// hung subprocess never returns control to it).
///
/// Bounded to `STARTUP_GOZER_PROBE_TIMEOUT` (~5s -- gozer's own `--version`
/// is expected to be near-instant, unlike `tt-smi -s`'s device scan, hence
/// the shorter ceiling than `STARTUP_DEVICE_MESH_TIMEOUT`), so a wedged
/// `gozer` binary can delay the socket bind below by at most that ceiling,
/// same "everything between here and the bind delays startup" note as
/// `detect_startup_device_mesh`.
async fn detect_startup_gozer(gozer_path: Option<&str>) -> Option<gozer::Capability> {
    let path_hint = gozer_path.map(|s| s.to_string());
    let probe_result = tokio::time::timeout(
        STARTUP_GOZER_PROBE_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let runner = RealCommandRunner;
            gozer::probe(&runner, path_hint.as_deref())
        }),
    )
    .await;

    match probe_result {
        // Ran to completion within the timeout, and the blocking task
        // didn't panic -- `gozer::probe` already logged which of "found" or
        // "absent" this is and why.
        Ok(Ok(capability)) => capability,
        // The `spawn_blocking` task panicked.
        Ok(Err(join_err)) => {
            eprintln!(
                "tt-station-agentd: gozer probe task panicked: {join_err}; leasing unavailable, serving whole-box as before"
            );
            None
        }
        // Blew past STARTUP_GOZER_PROBE_TIMEOUT -- a wedged `gozer` binary.
        // The spawned blocking task keeps running in the background (no
        // cooperative way to kill it), but startup stops waiting on it.
        Err(_elapsed) => {
            eprintln!(
                "tt-station-agentd: gozer probe timed out after {:?}; leasing unavailable, serving whole-box as before",
                STARTUP_GOZER_PROBE_TIMEOUT
            );
            None
        }
    }
}

/// How long the startup lease sweep may take before agentd stops waiting on
/// it. Everything between the capability probe and the socket bind delays
/// serving (same note as `detect_startup_device_mesh`), and this one shells
/// out three or four times -- `docker ps`, `gozer status`, `gozer history`,
/// and a `gozer release` per stale lease -- so it gets a ceiling of its own.
/// Generous relative to the probe's 5s because `docker ps` on a loaded box
/// is not instant, and because giving up early only postpones the reclaim to
/// the next restart.
const STARTUP_LEASE_SWEEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Reclaim this agent's own abandoned chip leases at startup: release every
/// `tt-station:` lease whose service port has nothing serving on it (see
/// `gozer::startup_sweep`, which holds the rule and every safety property).
///
/// A no-op -- not a single subprocess -- when gozer is absent, which is the
/// whole "gozer is optional" contract. Bounded and never fatal for the same
/// reasons `detect_startup_device_mesh`/`detect_startup_gozer` are: a wedged
/// `docker`/`gozer` must delay the socket bind by at most
/// `STARTUP_LEASE_SWEEP_TIMEOUT`, never hang the daemon, and a sweep that
/// times out or panics just leaves the leases where they are.
///
/// Called AFTER the capability probe (it needs the probe's result) and
/// before the bind, so a Mac connecting to a freshly-restarted agent sees a
/// box whose lease state already matches what is actually running.
async fn sweep_stale_leases_at_startup(
    capability: Option<&gozer::Capability>,
    serving_port: u16,
    backend: &dyn tt_station_agentd::serving::ServingBackend,
) {
    let Some(capability) = capability.cloned() else {
        return;
    };

    let sweep = tokio::time::timeout(
        STARTUP_LEASE_SWEEP_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            let runner = RealCommandRunner;
            gozer::startup_sweep(&runner, &capability, serving_port)
        }),
    )
    .await;

    match sweep {
        Ok(Ok(report)) => {
            // ADOPT what the sweep KEPT on our own serving port. That is a
            // lease a PREVIOUS process of this agent took for a serve that is
            // still running: the freshly-built backend has no record of it,
            // so without this the next `/stop` would stop the container and
            // release nothing, leaving the lease to gozer's reap -- and a
            // reaped lease is NEVER reset, handing the next tenant un-reset
            // silicon with wedged ethernet cores. See
            // `ServingBackend::adopt_lease`.
            for lease in &report.adoptable {
                if backend.adopt_lease(&lease.lease_id, &lease.who) {
                    eprintln!(
                        "tt-station-agentd: adopted lease '{}' ({}) from a previous agentd \
                         process -- `/stop` will release it (and reset its chips). NOTE its \
                         --owner-pid names the dead process, so gozer may reap it first; a \
                         reaped lease is not reset.",
                        lease.lease_id, lease.who
                    );
                } else {
                    // Loudly, by id: an unowned lease on our own port is an
                    // operator-visible problem and this line is what explains
                    // it. Never silent.
                    eprintln!(
                        "tt-station-agentd: WARNING: UNOWNED LEASE '{}' ({}) -- it is on this \
                         agent's own serving port, but this serving backend could not adopt \
                         it, so nothing here will ever release it and its chips will not be \
                         reset when the serve ends. Clear it by hand with `gozer release {}` \
                         once the serve is stopped.",
                        lease.lease_id, lease.who, lease.lease_id
                    );
                }
            }
            // `startup_sweep` already logs each decision as it makes it; one
            // summary line here is what an operator greps for at boot.
            if !report.released.is_empty()
                || !report.unresolved.is_empty()
                || !report.adoptable.is_empty()
            {
                eprintln!(
                    "tt-station-agentd: startup lease sweep released {} stale lease(s), kept {} \
                     ({} of ours, adopted), could not resolve {}",
                    report.released.len(),
                    report.kept.len(),
                    report.adoptable.len(),
                    report.unresolved.len()
                );
            }
        }
        Ok(Err(join_err)) => {
            eprintln!(
                "tt-station-agentd: startup lease sweep task panicked: {join_err}; any stale \
                 leases stay held"
            );
        }
        // Blew past the ceiling. Like `detect_startup_device_mesh`, the
        // spawned blocking task keeps running in the background (there is no
        // cooperative way to kill it) -- and unlike that one, this task can
        // still issue a `gozer release`, which resets chips. That is safe
        // for a specific reason worth stating: every lease id it can act on
        // was read from a `gozer status`/`history` snapshot taken BEFORE the
        // socket bound, so a straggler can only ever release a lease that
        // was already stale when the sweep started. It cannot name a lease
        // acquired after the bind (a `/run` arriving on the freshly-open
        // socket), because it never saw one. The window is bounded by the
        // data, not just by the timeout.
        Err(_elapsed) => {
            eprintln!(
                "tt-station-agentd: startup lease sweep timed out after {STARTUP_LEASE_SWEEP_TIMEOUT:?}; \
                 any stale leases stay held (they are reclaimed on the next restart)"
            );
        }
    }
}

/// Build the serving backend this process will run on, PROBING FOR GOZER
/// FIRST so the probed capability can be handed to the backend that is
/// actually constructed.
///
/// **This ordering is the whole point of the function.** Until Task 3, `main`
/// called `make_backend` first and `detect_startup_gozer` a hundred lines
/// further down -- one already-constructed backend too late. The
/// capability reached `AppState` (where nothing reads it) and never reached
/// the backend (where every leasing code path lives), so on a real box
/// `RunPyBackend::with_gozer` was called by tests and by nothing else:
/// agentd still reset the whole box before every serve and still pinned
/// nothing. Probing here, and returning BOTH the backend and the capability,
/// makes it structurally impossible to build a backend without having
/// decided what to do about leasing first.
///
/// Returns the capability alongside the backend because `AppState` wants it
/// too (`with_gozer`, read back by `AppState::gozer()`); one probe, two
/// consumers, no second `gozer --version` spawn.
///
/// Never fails on gozer's account: the probe degrades to `None` for every
/// failure mode (absent binary, non-zero exit, hang -- see
/// `detect_startup_gozer`), and a `None` capability means every backend
/// behaves exactly as it did before leasing existed. The only error this can
/// return is an unknown `--backend` kind, from `make_backend` itself.
async fn build_serving_backend(
    kind: &str,
    gozer_path: Option<&str>,
    docker_config: DockerConfig,
    runpy_config: RunPyConfig,
) -> Result<(
    Box<dyn tt_station_agentd::serving::ServingBackend>,
    Option<gozer::Capability>,
)> {
    let gozer_capability = detect_startup_gozer(gozer_path).await;
    let backend = make_backend(kind, docker_config, runpy_config, gozer_capability.clone())
        .context("failed to construct serving backend")?;
    Ok((backend, gozer_capability))
}

/// Resolve the `(ssh_user, authorized_keys_path)` target for `POST`/`DELETE
/// /ssh/authorize` (Task 2).
///
/// - `ssh_user_override` (`--ssh-user`) given: the user is that name
///   verbatim, and the path is resolved against `/home/<name>` (NOT this
///   process's own `$HOME`) -- the whole point of the override is targeting
///   an account other than whoever the agent process happens to run as
///   (e.g. root installing a key for `ttuser`).
/// - Not given: the user is resolved from `$USER`, falling back to running
///   `whoami` (covers a `$USER`-less service-manager environment), falling
///   back to the literal `"ttuser"` (QuietBox 2's run-user -- see the
///   module-level `CLAUDE.md`) if even that fails; the path is
///   `$HOME/.ssh/authorized_keys`.
///
/// Never fails, never panics: an unresolved `$HOME` (no override given)
/// degrades to an EMPTY `PathBuf` with a warning printed to stderr, rather
/// than aborting agent startup over a feature most boots won't even use --
/// `authkeys::authorize`/`revoke` will then surface a clear I/O error the
/// first time a client actually calls `/ssh/authorize`, instead of this
/// function guessing a wrong path silently.
fn resolve_ssh_target(ssh_user_override: Option<String>) -> (String, std::path::PathBuf) {
    if let Some(user) = ssh_user_override {
        let path = std::path::PathBuf::from(format!("/home/{user}")).join(".ssh/authorized_keys");
        return (user, path);
    }

    let user = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("whoami")
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "ttuser".to_string());

    let path = match std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        Some(home) => std::path::PathBuf::from(home).join(".ssh/authorized_keys"),
        None => {
            eprintln!(
                "tt-station-agentd: $HOME unresolved and --ssh-user not given; /ssh/authorize will fail on first use until one is set"
            );
            std::path::PathBuf::new()
        }
    };

    (user, path)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Config file path: explicit --config wins; else $TT_CONFIG_DIR/agentd.toml
    // if set; else $HOME/.config/tt-station/agentd.toml.
    let explicit = cli.config.is_some();
    let config_path = cli
        .config
        .clone()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let dir = std::env::var("TT_CONFIG_DIR").unwrap_or_else(|_| {
                format!(
                    "{}/.config/tt-station",
                    std::env::var("HOME").unwrap_or_default()
                )
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
        gozer_path: cli.gozer_path.clone(),
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
        // `--device-path` is a single value on the CLI and stays that way:
        // one `--device` flag is exactly the pre-leasing behaviour, and
        // `DockerConfig::device_path` is a `Vec` only so a LEASE can replace
        // it with one entry per granted chip (see `DockerBackend::start`).
        // An operator has no reason to pin several device paths by hand --
        // gozer is what decides which chips this agent may open.
        device_path: vec![rc.device_path.clone()],
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
        // Default-on: enable OpenAI-style tool calling for families we know
        // the vLLM parser for (see `RunPyConfig::enable_tool_calling`). This
        // is what lets a served instruct model (e.g. Llama-3.3-70B-Instruct)
        // accept tool calls from a coding agent -- tt-inference-server does
        // NOT wire this up on its own. No CLI opt-out yet; the field stays
        // configurable for tests and a future flag.
        enable_tool_calling: true,
    };

    // Probe for gozer and build the backend TOGETHER, in that order -- see
    // `build_serving_backend`'s doc comment for why the ordering is the
    // point. `gozer_capability` is carried down to `AppState::with_gozer`
    // below rather than re-probed there.
    let (backend, gozer_capability) = build_serving_backend(
        &rc.backend,
        rc.gozer_path.as_deref(),
        docker_config,
        runpy_config,
    )
    .await?;

    // Reconcile gozer's lease state against what is actually running, ONCE,
    // right after the probe that produced `gozer_capability` -- before the
    // socket bind below, so the first `/leases` a Mac ever asks for already
    // reflects reality. A no-op when gozer is absent. See
    // `sweep_stale_leases_at_startup` (bounded, never fatal).
    sweep_stale_leases_at_startup(gozer_capability.as_ref(), rc.serving_port, backend.as_ref())
        .await;

    // Persist issued bearer tokens across restarts by default (see
    // `--token-store`'s doc comment) -- `--no-token-persistence` (folded into
    // `rc.token_store` being `None` by `resolve`) opts back out to the
    // pre-persistence in-memory-only behavior.
    let backend: Arc<dyn tt_station_agentd::serving::ServingBackend> = Arc::from(backend);
    let state = match &rc.token_store {
        None => AppState::new(rc.name.clone(), rc.chips.clone(), backend),
        Some(path) => {
            println!(
                "tt-station-agentd: persisting bearer tokens to {}",
                path.display()
            );
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

    // Configure the additive `GET /logs` route (see routes.rs): only the
    // runpy backend writes `workflow_logs/{docker_server,run_logs}` under the
    // tt-inference-server checkout, so this is deliberately gated on
    // `rc.backend == "runpy"` -- other backends (dstack, docker) leave the
    // `AppState` default of `None`, and `/logs` answers 409 rather than
    // pointing at a directory that will never exist. Applied here, before any
    // clone of `state` exists, for the same sole-owner reason the other
    // `with_*` builders above are. `rc.tt_inference_repo` is the same path
    // `RunPyConfig::repo_dir` above was built from, so `/logs` can never see
    // a different checkout than the one actually serving.
    let state = if rc.backend == "runpy" {
        state.with_log_source(rc.tt_inference_repo.clone())
    } else {
        state
    };

    // Configure the additive `POST`/`DELETE /ssh/authorize` routes (Task 2,
    // see routes.rs): which account's `authorized_keys` a paired client's
    // key lands in. Applied here, before any clone of `state` exists, for
    // the same sole-owner reason the other `with_*` builders above are.
    // `--ssh-user` overrides the account; unset, it defaults to this
    // process's own run-user and `$HOME` -- see `resolve_ssh_target`.
    let (ssh_user, ssh_authorized_keys_path) = resolve_ssh_target(cli.ssh_user.clone());
    let state = state.with_ssh_target(ssh_authorized_keys_path, ssh_user);

    // Configure the additive `POST /power` route (Task 2, see routes.rs):
    // the four command vectors `run_power_command` shells out to. Only
    // applied when the operator gave at least one `--power-*-cmd` flag --
    // otherwise `AppState::new_inner`'s built-in defaults (`tt-smi -r` /
    // `systemctl suspend|reboot|poweroff`) stand untouched, same "no-op
    // unless asked" contract every other `with_*` builder here follows. A
    // flag left unset even when a SIBLING flag is given still falls back to
    // ITS OWN default (not an empty command) -- e.g. `--power-suspend-cmd`
    // alone doesn't blank out reboot/shutdown/reset-chips.
    let state = if cli.power_reset_chips_cmd.is_some()
        || cli.power_suspend_cmd.is_some()
        || cli.power_reboot_cmd.is_some()
        || cli.power_shutdown_cmd.is_some()
    {
        let reset_chips = cli
            .power_reset_chips_cmd
            .clone()
            .unwrap_or_else(|| vec!["tt-smi".to_string(), "-r".to_string()]);
        let suspend = cli
            .power_suspend_cmd
            .clone()
            .unwrap_or_else(|| vec!["systemctl".to_string(), "suspend".to_string()]);
        let reboot = cli
            .power_reboot_cmd
            .clone()
            .unwrap_or_else(|| vec!["systemctl".to_string(), "reboot".to_string()]);
        let shutdown = cli
            .power_shutdown_cmd
            .clone()
            .unwrap_or_else(|| vec!["systemctl".to_string(), "poweroff".to_string()]);
        state.with_power_config(reset_chips, suspend, reboot, shutdown)
    } else {
        state
    };

    // Detect this box's device mesh ONCE at startup (not per-request): run
    // `tt-smi -s` through the exact same command seam `GET /telemetry` uses
    // (`RealCommandRunner`, see `collect_snapshot` in routes.rs) and map its
    // stdout through `device::detect_device_mesh`. Reported on `/status` so
    // a client (Task 3's `tt --json status`) can rank models by hardware fit
    // without its own `tt-smi` access. ANY failure here (binary missing,
    // non-zero exit, unrecognized/mixed fleet, OR a hang) degrades to `None`
    // -- bounded to `STARTUP_DEVICE_MESH_TIMEOUT` (~10s) via
    // `tokio::time::timeout` around a `spawn_blocking`'d call, so a hung/
    // wedged `tt-smi` can delay the socket bind below by at most that
    // ceiling, never indefinitely; startup then proceeds regardless of the
    // outcome. See `detect_startup_device_mesh`'s doc comment.
    let device_mesh = detect_startup_device_mesh(&rc.tt_smi_bin).await;
    let state = state.with_device_mesh(device_mesh.clone());

    // Record the `gozer` capability probed ONCE at startup, up in
    // `build_serving_backend` -- the backend that actually leases already
    // holds it; this stores the same value on `AppState` so `/status`-side
    // readers (`AppState::gozer()`) can report it without a second
    // `gozer --version` spawn. Applied before any clone of `state` exists
    // (same `Arc::get_mut` requirement every other `with_*` builder here
    // relies on).
    let state = state.with_gozer(gozer_capability);

    // Detect this box's primary NIC MAC ONCE at startup (mirrors the
    // device-mesh detection immediately above): best-effort, synchronous
    // (no external `tt-smi`-style process that can hang, just `ip route
    // get`/`/sys/class/net` reads -- see `net::primary_mac`'s doc comment),
    // so it needs neither a timeout ceiling nor a `spawn_blocking` wrapper.
    // Reported on `/status` and the mDNS TXT record so the Mac can send a
    // Wake-on-LAN magic packet to this box when it's off. ANY failure here
    // (no default route, unreadable `/sys/class/net`, no non-loopback iface)
    // degrades to `None` -- Wake is simply unavailable for that box, never a
    // startup failure.
    let mac = net::primary_mac();
    if mac.is_none() {
        eprintln!(
            "tt-station-agentd: could not detect a primary NIC MAC; Wake-on-LAN will be unavailable for this box"
        );
    }
    let state = state.with_mac(mac.clone());

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
    let (_mdns_guard, status_advertiser) = advertise(&rc, state.status(), device_mesh, mac)
        .context("failed to start mDNS advertisement")?;
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
    /// This box's startup-detected device-mesh label (or `None` if detection
    /// failed/didn't run), captured once at construction in `advertise` and
    /// re-emitted on every status re-publish -- see the identically-named
    /// field on [`BoxRecord`] (Task 3.5).
    device_mesh: Option<String>,
    /// This box's startup-detected primary NIC MAC (or `None` if detection
    /// failed/didn't run), captured once at construction in `advertise` and
    /// re-emitted on every status re-publish -- see the identically-named
    /// field on [`BoxRecord`] (Task 3, Wake-on-LAN).
    mac: Option<String>,
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
            // Threaded through from the startup-detected mesh (Task 3.5) so
            // the mDNS TXT record carries `device_mesh` just like `/status`
            // does, keeping every discovery path hardware-aware.
            device_mesh: self.device_mesh.clone(),
            // Threaded through from the startup-detected primary NIC MAC
            // (Task 3) so the mDNS TXT record carries `mac` just like
            // `/status` does, letting the Mac send a Wake-on-LAN magic
            // packet without a separate probe.
            mac: self.mac.clone(),
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
/// truth for what the box's status is at boot. Likewise `device_mesh` is
/// passed in from the same startup detection `main` feeds to
/// `AppState::with_device_mesh`, so the mDNS TXT record and `/status` never
/// disagree about this box's hardware (Task 3.5). `mac` mirrors the same
/// pattern for the box's startup-detected primary NIC MAC (Task 3), so the
/// TXT record and `/status` never disagree about the Wake-on-LAN target
/// either.
///
/// Returns both the [`MdnsGuard`] (unregister/shutdown on drop, same as
/// before this function grew a second return value) and an
/// [`MdnsStatusAdvertiser`] sharing the same `Arc<ServiceDaemon>`, so `main`
/// can attach the latter to `AppState` and let `/run`/`/stop` keep the TXT
/// record's `status` key truthful after boot.
fn advertise(
    rc: &config::ResolvedConfig,
    status: ServingStatus,
    device_mesh: Option<String>,
    mac: Option<String>,
) -> Result<(MdnsGuard, MdnsStatusAdvertiser)> {
    let host = format!("{}.local.", rc.name);
    let record = BoxRecord {
        name: rc.name.clone(),
        host: host.clone(),
        ctrl_port: rc.ctrl_port,
        chips: rc.chips.clone(),
        status,
        apiver: rc.apiver,
        device_mesh: device_mesh.clone(),
        mac: mac.clone(),
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
        device_mesh,
        mac,
    };

    Ok((guard, status_advertiser))
}

#[cfg(test)]
mod startup_wiring_tests {
    use super::*;

    /// Write an executable stand-in for the `gozer` binary into `dir` and
    /// return its path.
    ///
    /// It answers `--version` (the ONLY verb `gozer::probe` ever runs) and
    /// refuses everything else with a non-zero exit, so a regression that
    /// made startup call `acquire`/`release`/`run`/`reconcile` would fail
    /// this test rather than quietly touching the real gate. Nothing here
    /// can reach real gozer, real chips, or another agent's lease: the path
    /// handed to the probe is this script, in a temp dir, and `$PATH` is
    /// never consulted because the hint is non-empty (see
    /// `gozer::resolve_path`).
    fn write_stub_gozer(dir: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("gozer");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             if [ \"$1\" = \"--version\" ]; then echo 'gozer 9.9.9-stub'; exit 0; fi\n\
             echo \"stub gozer refuses to run: $*\" >&2\n\
             exit 99\n",
        )
        .expect("write stub gozer");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod stub gozer");
        path.to_string_lossy().into_owned()
    }

    /// THE PRODUCTION-WIRING TEST (both halves -- see the note mid-body for
    /// why the absent-gozer case shares this function). Runs the exact
    /// sequence `main` runs to
    /// obtain a backend -- the real `detect_startup_gozer` probe against a
    /// real (stub) binary, then the real `make_backend` -- and asserts the
    /// backend that comes out is holding the capability that went in.
    ///
    /// How this fails if the wiring is removed:
    ///
    /// * Reorder `build_serving_backend` so `make_backend` runs before the
    ///   probe: the only capability available to pass is `None`, so the
    ///   backend reports `None` while `capability` is `Some` -- the
    ///   `assert_eq!` on the last line fails.
    /// * Drop the capability argument from `make_backend`'s call: the crate
    ///   stops compiling (the parameter is required, not defaulted).
    /// * Drop `.with_gozer(..)` inside `make_backend`: the backend reports
    ///   `None` and this fails, as does
    ///   `serving::mod`'s `make_backend_hands_the_capability_to_the_backend_it_builds`.
    ///
    /// What it does NOT cover, stated plainly: if `main` stopped calling
    /// `build_serving_backend` altogether and built a backend some other
    /// way, no test here would notice. `build_serving_backend` is the single
    /// path in this file to a `Box<dyn ServingBackend>`, and
    /// `make_backend`'s required capability parameter forces any alternative
    /// to make the same decision explicitly -- but that is a structural
    /// argument, not an assertion.
    #[tokio::test]
    async fn build_serving_backend_hands_the_probed_capability_to_the_backend() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = write_stub_gozer(dir.path());

        for kind in ["runpy", "docker"] {
            let (backend, capability) = build_serving_backend(
                kind,
                Some(&stub),
                DockerConfig::default(),
                RunPyConfig::default(),
            )
            .await
            .expect("backend should construct");

            assert_eq!(
                capability.as_ref().map(|c| c.version.as_str()),
                Some("gozer 9.9.9-stub"),
                "the startup probe should have found the stub gozer"
            );
            assert_eq!(
                backend.gozer_capability(),
                capability.as_ref(),
                "the {kind} backend main.rs actually builds must be holding the \
                 probed capability -- otherwise leasing is wired to nothing and \
                 the whole feature is inert on a real box"
            );
        }

        // The ABSENT-gozer half of the same contract -- which is the normal
        // case on a real box: a configured path that doesn't resolve makes
        // the probe degrade to `None` (never an error), and the backend is
        // built with leasing OFF, i.e. byte-identical pre-integration
        // behaviour.
        //
        // Deliberately in this SAME test function rather than its own, and
        // not for brevity: `#[test]`s in one binary run on parallel threads,
        // and this crate already has a load-sensitive `ETXTBSY` flake from
        // exactly that shape (`routes.rs`'s
        // `run_power_command_runs_the_configured_command` writes an
        // executable and spawns it). A sibling test that spawns anything
        // concurrently with `write_stub_gozer` above can inherit the
        // still-open write fd across its own fork, making the `exec` of the
        // stub fail with "Text file busy" -- which `gozer::probe` correctly
        // swallows as "gozer absent", turning a real race into a confusing
        // assertion failure on the line above. One thread, no fork window.
        //
        // The path below is a non-existent ABSOLUTE path rather than `None`:
        // `None` would send `gozer::probe` searching `$PATH`, where a
        // developer box may well have a REAL gozer installed, and the test's
        // outcome would depend on the machine it runs on.
        let (backend, capability) = build_serving_backend(
            "runpy",
            Some("/nonexistent/tt-station-agentd-test/gozer"),
            DockerConfig::default(),
            RunPyConfig::default(),
        )
        .await
        .expect("an absent gozer must not fail startup");

        assert_eq!(capability, None, "an unresolvable path must probe to None");
        assert_eq!(
            backend.gozer_capability(),
            None,
            "without a capability the backend must be in its pre-leasing state"
        );
    }
}
