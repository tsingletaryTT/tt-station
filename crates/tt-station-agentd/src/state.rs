//! `AppState`: the one piece of shared state every handler reaches through.
//!
//! It's deliberately built as a cheap-to-`Clone` handle (`Arc<Inner>`) around
//! an `Inner` struct so the agent can grow it without reshaping the handle
//! itself or how it's threaded through `axum::State`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use libttstation::model::{ConfigSummary, Endpoint, ServingStatus};
use serde::Serialize;

use crate::pairing;
use crate::serving::discovery::discover_serving_cached;
use crate::serving::docker::{CommandRunner, RealCommandRunner};
use crate::serving::ServingBackend;
use crate::telemetry;

mod hardware;
mod serving;
mod tokens;

/// Default interval between `tt-smi -s` telemetry snapshots pushed on the
/// `GET /telemetry` WebSocket stream, when `AppState` isn't told otherwise.
/// Mirrors `main.rs`'s `--telemetry-interval-ms` default so an `AppState`
/// built without `with_telemetry_config` (e.g. in a test that doesn't care)
/// still behaves like a default agent.
const DEFAULT_TELEMETRY_INTERVAL_MS: u64 = 1000;

/// Default `tt-smi` binary name resolved on `$PATH`, when `AppState` isn't
/// told otherwise. Mirrors `main.rs`'s `--tt-smi-bin` default.
const DEFAULT_TT_SMI_BIN: &str = "tt-smi";

/// Default serving host baked into `GET /serving` `base_url`s when `AppState`
/// isn't told otherwise. Mirrors `main.rs`'s `--serving-host` default.
const DEFAULT_SERVING_HOST: &str = "127.0.0.1";

/// Default serving port `GET /serving` treats as the agent's own, when
/// `AppState` isn't told otherwise. Mirrors `main.rs`'s `--serving-port`
/// default.
const DEFAULT_SERVING_PORT: u16 = 8000;

/// TTL on the cached `gozer status --json` parse shared by `GET /leases`
/// and `GET /status`'s `leasing` field (see `Inner::gozer_leases_cache`).
/// `gozer status` is cheap (sysfs + `/proc`, never a device -- see the
/// design doc), so this is short: long enough to collapse a burst of
/// near-simultaneous callers (a Mac polling `/leases` and `/status` at
/// roughly the same cadence) into one subprocess, short enough that a real
/// lease change on the box (someone else's `gozer acquire`/`release`) shows
/// up well within a human noticing it.
///
/// **Known limitation** (listed with the others in `crate::gozer`'s module
/// doc): because it is cached, `/leases` can lag reality by up to this TTL.
/// That makes it a status VIEW, never an interlock -- nothing may decide
/// whether to touch chips from it. The two reset refusals deliberately call
/// `gozer::foreign_leases` fresh rather than reading this cache.
const GOZER_LEASE_CACHE_TTL: Duration = Duration::from_millis(300);

/// How long a pairing code stays valid after `/pair/init` mints it. Short
/// enough that a code seen once (e.g. shoulder-surfed, or left in shell
/// history) is useless a couple of minutes later; long enough that a human
/// reading a 6-digit code off the box's screen and typing it into the
/// client isn't in a race against the clock.
pub(crate) const PAIR_TTL: Duration = Duration::from_secs(120);

/// How many wrong-code guesses a single `pair_id` tolerates before
/// `complete_pair` invalidates it outright. A 6-digit code (10^6 possible
/// values) with a 120s TTL and no attempt cap would let a LAN client just
/// hammer `/pair/complete` with every value in range; capping wrong guesses
/// at a small number closes that off while still giving a human who
/// fat-fingers the code a few real chances to correct themselves.
pub const MAX_PAIR_ATTEMPTS: u32 = 5;

/// `/pair/init` asks one address may make a minute (each one notifies the owner).
pub const PAIR_INITS_PER_ADDRESS: usize = 5;

/// `/pair/init` asks a minute from every address together.
pub const PAIR_INITS_PER_MINUTE: usize = 20;

/// Seam for re-publishing this box's advertised `status` whenever it
/// changes (`/run` succeeding, `/stop` completing), so the mDNS TXT record
/// `tt-station discover` reads over the LAN never goes stale the way it did before
/// this trait existed (see the module-level findings doc: `tt-station discover`
/// over mDNS kept reporting `idle` while the box was actually serving,
/// because the TXT record was only ever published once, at boot).
///
/// Implemented for real by `tt-station-agentd`'s `main.rs`
/// (`MdnsStatusAdvertiser`, which re-registers the mDNS `ServiceInfo` with
/// the daemon it already created for the boot-time advertisement) and left
/// as a trait here -- rather than `routes.rs`/`AppState` depending on
/// `mdns_sd` directly -- so tests can swap in a fake that just records what
/// it was told, without any real network I/O.
pub trait StatusAdvertiser: Send + Sync {
    /// Re-publish `status` as this box's current advertised status.
    /// Implementations must not panic on failure (log and move on instead)
    /// -- a failed re-publish shouldn't fail the `/run`/`/stop` request that
    /// triggered it, since the control-plane state change already
    /// succeeded.
    fn advertise_status(&self, status: &ServingStatus);
}

/// Shared application state, cheap to `Clone` (just bumps the `Arc`
/// refcount) so it can be handed to every axum handler via `State`.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

/// Mutable state modules keep for the life of one agent. It lives on
/// `AppState` rather than in process-wide statics so that every `AppState`
/// (each test's, too) has its own.
#[derive(Default)]
pub(crate) struct PerAgent {
    pub(crate) idle: crate::idle::PerAgent,
    pub(crate) library: crate::library::PerAgent,
    pub(crate) routines: crate::routines::PerAgent,
}

/// The actual state data, held once behind the `Arc` in `AppState`.
///
/// Fields beyond what Task 6 needs (a pairing-token set for Task 7, a real
/// `ServingBackend` handle for Task 9) get added here -- this struct, not
/// `AppState` itself, is the extension point.
struct Inner {
    /// Human-assigned name for this box (also the mDNS instance name).
    name: String,
    /// Chip inventory string, e.g. `"4xBH"`.
    chips: String,
    /// Current serving status. `Mutex`-guarded because Task 10's control
    /// routes flip it between `Idle` and `Serving(model)`.
    status: Mutex<ServingStatus>,
    /// The `Endpoint` handed back by the backend's last successful `start`,
    /// if anything is currently serving. Kept alongside (not derived from)
    /// `status` so `GET /endpoint` doesn't need to reconstruct `base_url`
    /// from scratch -- `status` only round-trips the model name, not the
    /// full `Endpoint` a backend chose to return.
    endpoint: Mutex<Option<Endpoint>>,
    /// The real serving backend (Docker or dstack, chosen on the command
    /// line -- see `make_backend` in `serving/mod.rs`) that `/run`/`/stop`
    /// delegate to. `Arc<dyn ServingBackend>` rather than `Box` so route
    /// handlers can cheaply clone a handle to move into
    /// `tokio::task::spawn_blocking` (the trait's `start`/`stop` are sync
    /// and must never run directly on the async runtime -- see `serving/mod.rs`).
    backend: Arc<dyn ServingBackend>,
    /// Pairing attempts started by `/pair/init` but not yet completed:
    /// `pair_id -> (code, expiry, wrong_attempts, owner)`, `owner` when it was
    /// offered with the box user's local secret. An entry is removed the
    /// moment it's consumed -- successfully, because it expired, or because
    /// `wrong_attempts` hit `MAX_PAIR_ATTEMPTS` -- by `complete_pair`, so
    /// this map only ever holds pairing attempts that are still "live".
    pending_pairs: Mutex<HashMap<String, (String, Instant, u32, bool)>>,
    /// Recent `/pair/init` asks: when, and from which address (`None` when
    /// the server doesn't know its peers). See `pair_init_allowed`.
    pair_inits: Mutex<Vec<(u64, Option<std::net::IpAddr>)>>,
    /// Paired devices: one session per `/pair/complete`, holding only a
    /// digest of its bearer token, with expiry and per-device revocation (see
    /// `crate::sessions`). Persisted to `--token-store` when `new_persisting`
    /// built this state; in memory only for `new`.
    sessions: Arc<crate::sessions::Sessions>,
    /// Optional hook for re-publishing this box's advertised `status`
    /// whenever `set_serving`/`set_idle` change it. `None` (what every
    /// constructor here builds by default) is a no-op -- exactly the
    /// pre-existing behavior every test other than the `StatusAdvertiser`
    /// ones relies on. Attached after construction via
    /// `AppState::with_status_advertiser` (`main.rs` wires the real mDNS
    /// impl in; tests wire in a fake).
    advertiser: Option<Arc<dyn StatusAdvertiser>>,
    /// `tt-smi` binary the `GET /telemetry` stream runs to collect snapshots.
    /// Defaults to `DEFAULT_TT_SMI_BIN` (`"tt-smi"`, resolved on `$PATH`);
    /// `main.rs` overrides it from `--tt-smi-bin` via `with_telemetry_config`,
    /// and tests point it at a stub script. Purely additive: nothing outside
    /// the `/telemetry` route reads it.
    tt_smi_bin: String,
    /// Milliseconds between telemetry snapshots pushed on `GET /telemetry`.
    /// Defaults to `DEFAULT_TELEMETRY_INTERVAL_MS`; set via
    /// `with_telemetry_config` from `--telemetry-interval-ms`.
    telemetry_interval_ms: u64,
    /// Short-TTL cache of the last raw `tt-smi -s` snapshot, shared across every
    /// `/telemetry` connection. `tt-smi` is a device-touching shell-out that
    /// contends with a running workload; without this, EACH connected client ran
    /// its own `tt-smi` every interval, so N viewers meant N concurrent `tt-smi`
    /// processes hammering the chip. With it, concurrent client ticks within one
    /// interval collapse to a single run (the cheap per-connection process scan +
    /// vLLM scrape still run per client). `tokio::sync::Mutex` because the lock is
    /// held across the `spawn_blocking` shell-out to dedupe a thundering herd.
    /// `None` until the first snapshot; zero clients means zero `tt-smi` (a client
    /// tick is what populates it).
    tt_smi_cache: tokio::sync::Mutex<Option<(Instant, String)>>,
    /// Host baked into the `base_url` of endpoints `GET /serving` reports --
    /// the agent's configured serving host (`--serving-host`), same value
    /// the serving backend uses for its own `Endpoint.base_url`. Defaults to
    /// `DEFAULT_SERVING_HOST`; set via `with_serving_config`. Purely additive:
    /// only the `/serving` route reads it.
    serving_host: String,
    /// The agent's OWN configured serving host port (`--serving-port`). Used
    /// by `GET /serving` to classify a discovered endpoint as `"agent"` (its
    /// port matches this AND the agent's in-memory status is serving that
    /// model) vs `"external"`. Defaults to `DEFAULT_SERVING_PORT`; set via
    /// `with_serving_config`.
    serving_port: u16,
    /// This box's device-mesh label (`"p300x2"`, `"n300x4"`, ...), detected
    /// ONCE at startup by running `tt-smi -s` and mapping its output through
    /// `device::detect_device_mesh` (see `main.rs`). `None` when detection
    /// failed or the fleet doesn't match a known mesh -- never fatal, just
    /// an absent hint. Defaults to `None`; set via `with_device_mesh`.
    /// Purely additive: only `GET /status` reads it, so a client (Task 3's
    /// `tt-station --json status`) can rank models by hardware fit.
    device_mesh: Option<String>,
    /// This box's primary NIC MAC (`"aa:bb:cc:dd:ee:ff"`), detected ONCE at
    /// startup by `net::primary_mac` (see `main.rs`). `None` when detection
    /// failed (no default route, unreadable `/sys/class/net`, no usable
    /// non-loopback iface) -- never fatal, just an absent hint. Defaults to
    /// `None`; set via `with_mac`. Purely additive: only `GET /status` reads
    /// it, so a client (the Mac app) can send a Wake-on-LAN magic packet to
    /// this box when it's off.
    mac: Option<String>,
    /// Redacted view of the agent's resolved serving config, served verbatim
    /// by `GET /config` -- see `libttstation::model::ConfigSummary`'s doc
    /// comment for why it deliberately carries no secrets. Defaults to an
    /// empty-but-valid summary (no active profile, defaults for
    /// backend/serving host/port) so an `AppState` never given
    /// `with_config_summary` (every test other than the `/config` ones)
    /// still answers the route instead of requiring every constructor to
    /// know about config resolution. Real content is wired in by `main.rs`
    /// via `with_config_summary`, built from the actually-resolved config
    /// (Task 3 of the agentd-config-profiles plan).
    config_summary: ConfigSummary,
    /// Where `POST /ssh/authorize`/`DELETE /ssh/authorize` read and write the
    /// target account's `authorized_keys` file. Defaults to an empty
    /// `PathBuf` (see `new_inner`) -- not a guess at a real path, so a
    /// caller who forgets `with_ssh_target` (or a box where `$HOME` never
    /// resolved -- see `main.rs`'s `resolve_ssh_target`) gets an obvious
    /// I/O error from `authkeys::authorize`/`revoke` rather than silently
    /// writing to the wrong file. Set via `with_ssh_target`; `main.rs`
    /// resolves the real value from `$HOME`/`--ssh-user`.
    ssh_authorized_keys_path: PathBuf,
    /// The account name reported back to a client as `ssh_user` in
    /// `POST /ssh/authorize`'s response, so it knows which account to `ssh`
    /// in as after its key lands in `ssh_authorized_keys_path`. Defaults to
    /// `"ttuser"` -- the run-user on QuietBox 2, this codebase's reference
    /// box (see the module-level `CLAUDE.md`). Set via `with_ssh_target`.
    ssh_user: String,
    /// Path to the tt-inference-server checkout, when the runpy backend is
    /// active. `None` for backends without a workflow_logs dir (e.g. dstack).
    /// Enables the `/logs` routes to locate `workflow_logs/{docker_server,run_logs}`.
    tt_inference_repo: Option<std::path::PathBuf>,
    /// Command vector run for `PowerAction::ResetChips` (a `tt-smi -r` board
    /// reset). Defaults to `["tt-smi", "-r"]`; overridden by
    /// `with_power_config` (tests / mock-box inject a harmless stub so no
    /// real board reset fires). See `run_power_command`.
    power_reset_chips_cmd: Vec<String>,
    /// Command vector run for `PowerAction::Suspend`. Defaults to
    /// `["systemctl", "suspend"]`; overridden by `with_power_config`.
    power_suspend_cmd: Vec<String>,
    /// Command vector run for `PowerAction::Reboot`. Defaults to
    /// `["systemctl", "reboot"]`; overridden by `with_power_config`.
    power_reboot_cmd: Vec<String>,
    /// Command vector run for `PowerAction::Shutdown`. Defaults to
    /// `["systemctl", "poweroff"]`; overridden by `with_power_config`.
    power_shutdown_cmd: Vec<String>,
    /// This box's detected `gozer` capability (resolved path + version),
    /// from a single startup probe (see `main.rs`'s `detect_startup_gozer`,
    /// mirroring `detect_startup_device_mesh`). `None` when gozer isn't
    /// installed, the probe failed, or (today) simply hasn't been wired in
    /// by a caller that skips `with_gozer` -- gozer is optional throughout
    /// (see `gozer`'s module doc), so `None` is a normal, fully-supported
    /// value, never an error. Defaults to `None`; set via `with_gozer`.
    /// Purely additive: nothing outside a future leasing-aware route reads
    /// it yet -- Task 1 only stores it.
    gozer: Option<crate::gozer::Capability>,
    /// Short-TTL cache of the last `gozer status --json` parse (see
    /// [`crate::gozer::snapshot_leases`]), shared by `GET /leases` and
    /// `GET /status`'s `leasing` field -- same "collapse a thundering herd
    /// of near-simultaneous callers into one shell-out" reasoning as
    /// `tt_smi_cache`, but for gozer's status instead of `tt-smi`'s. Unlike
    /// `tt_smi_cache` (whose TTL tracks the configurable telemetry
    /// interval), this uses the fixed `GOZER_LEASE_CACHE_TTL`: there's no
    /// per-client polling interval to key off here, just "don't let an
    /// unthrottled `/leases` poller (or `/status`, which is unauthed and
    /// polled far more casually) spawn a `gozer status` subprocess on every
    /// single request." `None` until the first call populates it.
    gozer_leases_cache: tokio::sync::Mutex<Option<(Instant, crate::gozer::LeaseSnapshot)>>,
    /// Disruptive actions the `POST /assistant` chat proposed but did NOT
    /// run, waiting for the human to approve or deny them via
    /// `POST /assistant/confirm`: `action_id -> PendingAction`. Same
    /// "only ever holds live entries" discipline as `pending_pairs`: expired
    /// entries (older than `assistant::PENDING_TTL`) are swept on every insert
    /// and every lookup, and the map is capped at `assistant::MAX_PENDING` by
    /// evicting the oldest -- an authed client that chats endlessly without
    /// ever confirming can't grow it without bound. An entry is removed the
    /// moment it's looked up, approved or not, so an action can run at most
    /// once. See `crate::assistant` for the confirmation model.
    pending_actions: Mutex<HashMap<String, crate::assistant::PendingAction>>,
    /// The official Tenstorrent CLI as this box's model manager (`/tt/*`
    /// routes: catalog + downloads), when `main` found one. `None` makes those
    /// routes answer 409. See `crate::ttcli`.
    tt_cli: Option<Arc<crate::ttcli::TtCli>>,
    /// What the agent is doing with the chips, for clients that can't keep a
    /// request open for the minutes a bring-up takes (a phone): see
    /// [`Lifecycle`] and `GET /serve`.
    lifecycle: Mutex<Lifecycle>,
    /// Telemetry and inference over time, for dashboards (`/metrics/*`).
    history: Arc<crate::history::History>,
    /// The user's uploaded documents, searched by the assistant (`/docs`).
    /// `None` until `main` attaches one; the routes then answer 409.
    docs: Option<Arc<crate::docs::DocStore>>,
    /// The same-user secret that lets `tt-station-agentd pair` mint a pairing
    /// code for a QR (`POST /pair/offer`). `None` disables that route.
    local_secret: Option<String>,
    /// The second serving slot for co-serving (chat + images on separate
    /// cards), when the box can split. See `crate::coserve`.
    companion: Option<Arc<crate::coserve::Companion>>,
    /// Keys for the OpenAI-compatible `/v1` gateway (`crate::gateway`).
    app_keys: Arc<crate::gateway::AppKeys>,
    /// The control port this agent listens on, for URLs it hands out.
    ctrl_port: u16,
    /// Images the assistant made in chat (`GET /creations/{id}`).
    creations: crate::creations::Creations,
    /// What the assistant remembers about its owner (`/memories`).
    memories: Arc<crate::memory::Memories>,
    /// Alerts for the owner (`crate::notify`).
    notifier: Arc<crate::notify::Notifier>,
    /// The agent's config directory (`$TT_CONFIG_DIR` or `~/.config/tt-station`).
    config_dir: PathBuf,
    /// Whether this is the box's own agent, which may stop the box's user services
    /// (the serving unit). Off by default, so tests and mock-box never touch them:
    /// a test's "stop serving" once stopped a real model mid-boot.
    manages_units: bool,
    /// How long each model took to load here (`crate::modes`).
    load_times: Arc<crate::modes::LoadTimes>,
    /// HTTPS, when the agent serves it (`crate::tls`).
    tls: Option<crate::tls::TlsInfo>,
    /// Lab mode (`crate::lab`), when tt-lab is set up.
    lab: Option<Arc<crate::lab::Lab>>,
    /// A tt-lab run owns the chips: no tt-smi, no model starts, no resets.
    lab_busy: std::sync::atomic::AtomicBool,
    /// Long jobs (`crate::jobs`).
    jobs: Option<Arc<crate::jobs::Jobs>>,
    /// Let the assistant think on the CPU (Ollama) when the chips aren't chatting.
    cpu_brain: bool,
    /// Who's who on this box (`crate::people`).
    people: Arc<crate::people::People>,
    /// The activity log (`crate::audit`).
    audit: Arc<crate::audit::AuditLog>,
    /// Scheduled modes (`crate::schedules`).
    schedules: Arc<crate::schedules::Schedules>,
    /// Model bring-ups with tt-orchard (`crate::orchard`), when it's checked out.
    orchard: Option<Arc<crate::orchard::Orchard>>,
    /// A tt-orchard run has the box: no model starts, no resets.
    orchard_busy: std::sync::atomic::AtomicBool,
    /// Its supervisor is live and may be using the chips: no tt-smi either
    /// (it would keep the chips looking busy to gozer's quiet checks).
    orchard_chips: std::sync::atomic::AtomicBool,
    /// Chip resets running (`tt-smi -r`, or gozer resetting a board as it
    /// releases a lease): `tt-smi -s` against chips mid-reset only fails, and
    /// slows the reset down, so readings pause while this is above zero.
    chips_resetting: Arc<std::sync::atomic::AtomicUsize>,
    /// An agent-driven bring-up (Codex, fast-models-fast) holds the whole box.
    bringup_busy: std::sync::atomic::AtomicBool,
    /// Whether the served chat model answers (`crate::liveness`).
    liveness: Arc<crate::liveness::Liveness>,
    /// What modules keep for the life of this agent.
    per_agent: PerAgent,
}

/// The serving lifecycle `GET /serve` reports. `/status` only ever says `idle`
/// or `serving:<id>`; this adds the in-between states a client needs to show
/// ("starting Qwen3-32B for 1m40s", "stopping") and why a start failed.
#[derive(Debug, Clone, Serialize)]
pub struct Lifecycle {
    pub phase: Phase,
    /// The model as it was asked for (the tt catalog name, e.g. `FLUX.1-schnell`),
    /// which for media servers differs from what `/v1/models` reports.
    pub model: Option<String>,
    /// What the server reports in `/v1/models` once up.
    pub served_id: Option<String>,
    /// Unix seconds this phase began.
    pub since: u64,
    /// Why the last start failed (`phase == failed`).
    pub error: Option<String>,
    /// This serve was already running when the agent started and was adopted
    /// rather than launched (see `AppState::adopt_serving`).
    pub adopted: bool,
}

/// From [`AppState::pause_readings`]: chip readings resume when it drops.
pub struct ReadingsPause(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for ReadingsPause {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Starting,
    Serving,
    Stopping,
    Failed,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Lifecycle {
            phase: Phase::Idle,
            model: None,
            served_id: None,
            since: unix_now(),
            error: None,
            adopted: false,
        }
    }
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What kind of experience a served model is: drives which UI a client shows
/// and which proxy routes (`/generate/*`, `/assistant`) apply.
pub fn model_kind(model_type: Option<&str>, served_id: Option<&str>) -> &'static str {
    match model_type {
        Some("llm") | Some("vlm") => return "chat",
        Some("image") => return "image",
        Some("video") => return "video",
        Some("audio") => return "speech",
        Some("text_to_speech") => return "tts",
        Some(_) => return "other",
        None => {}
    }
    // No catalog entry (an adopted or external serve): guess from the id the
    // server reports. Media servers report their runner (`tt-flux.1-schnell`,
    // `tt-sdxl-...`) or a weights path; vLLM reports the HF repo.
    let id = served_id.unwrap_or_default().to_lowercase();
    if id.contains("flux")
        || id.contains("sdxl")
        || id.contains("stable-diffusion")
        || id.contains("sd3")
    {
        "image"
    } else if id.contains("wan")
        || id.contains("mochi")
        || id.contains("t2v")
        || id.contains("skyreels")
    {
        "video"
    } else if id.contains("whisper") {
        "speech"
    } else if id.contains("speecht5") || id.contains("tts") {
        "tts"
    } else if id.is_empty() {
        "other"
    } else {
        "chat"
    }
}

impl AppState {
    /// Construct fresh state for a box that starts out idle, wired to
    /// `backend` for actually starting/stopping model serving. Callers
    /// (`main.rs`, and this crate's tests) build `backend` via
    /// `serving::make_backend` or a test double and hand it in already
    /// wrapped in an `Arc`, since `AppState` never needs to construct a
    /// backend itself.
    pub fn new(name: String, chips: String, backend: Arc<dyn ServingBackend>) -> Self {
        Self::new_inner(name, chips, backend, crate::sessions::Sessions::in_memory())
    }

    /// Construct state whose paired devices are persisted to `token_store`
    /// on disk, so a paired client survives an agent restart instead of
    /// being forced to re-pair.
    ///
    /// Whatever is already at `token_store` is loaded up front, and a file
    /// in the old raw-token format is converted in place (see
    /// `crate::sessions`). A missing file means "no paired devices yet"; an
    /// unreadable or corrupt one logs a warning to stderr and also starts
    /// empty -- a bad token store must never fail agent startup, let alone
    /// panic it.
    pub fn new_persisting(
        name: String,
        chips: String,
        backend: Arc<dyn ServingBackend>,
        token_store: PathBuf,
    ) -> Self {
        let sessions = crate::sessions::Sessions::open(token_store);
        Self::new_inner(name, chips, backend, sessions)
    }

    /// Shared construction path for `new`/`new_persisting`: only where the
    /// paired devices come from (and go to) differs between the two.
    fn new_inner(
        name: String,
        chips: String,
        backend: Arc<dyn ServingBackend>,
        sessions: crate::sessions::Sessions,
    ) -> Self {
        AppState {
            inner: Arc::new(Inner {
                name,
                chips,
                status: Mutex::new(ServingStatus::Idle),
                endpoint: Mutex::new(None),
                backend,
                pending_pairs: Mutex::new(HashMap::new()),
                pair_inits: Mutex::new(Vec::new()),
                sessions: Arc::new(sessions),
                advertiser: None,
                tt_smi_bin: DEFAULT_TT_SMI_BIN.to_string(),
                telemetry_interval_ms: DEFAULT_TELEMETRY_INTERVAL_MS,
                tt_smi_cache: tokio::sync::Mutex::new(None),
                serving_host: DEFAULT_SERVING_HOST.to_string(),
                serving_port: DEFAULT_SERVING_PORT,
                device_mesh: None,
                mac: None,
                config_summary: ConfigSummary {
                    active_profile: None,
                    available_profiles: vec![],
                    backend: "runpy".to_string(),
                    serving_host: "127.0.0.1".to_string(),
                    serving_port: 8000,
                    serving_image: None,
                    tt_inference_repo: None,
                    tt_device: None,
                },
                ssh_authorized_keys_path: PathBuf::new(),
                ssh_user: "ttuser".to_string(),
                tt_inference_repo: None,
                power_reset_chips_cmd: vec!["tt-smi".to_string(), "-r".to_string()],
                power_suspend_cmd: vec!["systemctl".to_string(), "suspend".to_string()],
                power_reboot_cmd: vec!["systemctl".to_string(), "reboot".to_string()],
                power_shutdown_cmd: vec!["systemctl".to_string(), "poweroff".to_string()],
                gozer: None,
                gozer_leases_cache: tokio::sync::Mutex::new(None),
                pending_actions: Mutex::new(HashMap::new()),
                tt_cli: None,
                lifecycle: Mutex::new(Lifecycle::default()),
                history: Arc::new(crate::history::History::new()),
                docs: None,
                local_secret: None,
                companion: None,
                app_keys: Arc::new(crate::gateway::AppKeys::in_memory()),
                ctrl_port: 8765,
                memories: Arc::new(crate::memory::Memories::in_memory()),
                notifier: Arc::new(crate::notify::Notifier::in_memory()),
                config_dir: std::env::temp_dir().join("tt-station"),
                manages_units: false,
                load_times: Arc::new(crate::modes::LoadTimes::in_memory()),
                tls: None,
                lab: None,
                lab_busy: std::sync::atomic::AtomicBool::new(false),
                jobs: None,
                cpu_brain: false,
                orchard: None,
                people: Arc::new(crate::people::People::in_memory()),
                audit: Arc::new(crate::audit::AuditLog::in_memory()),
                schedules: Arc::new(crate::schedules::Schedules::in_memory()),
                orchard_busy: std::sync::atomic::AtomicBool::new(false),
                orchard_chips: std::sync::atomic::AtomicBool::new(false),
                chips_resetting: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bringup_busy: std::sync::atomic::AtomicBool::new(false),
                liveness: Arc::new(crate::liveness::Liveness::default()),
                per_agent: PerAgent::default(),
                creations: crate::creations::Creations::new(
                    std::env::temp_dir()
                        .join(format!("tt-station-creations-{}", std::process::id())),
                ),
            }),
        }
        .wire_notifier()
    }

    /// Let the notifier look up whose each device is, for routing pushes
    /// (`crate::notify::Audience`). Again whenever either side is replaced.
    fn wire_notifier(self) -> Self {
        self.inner.notifier.set_directory(
            Arc::clone(&self.inner.sessions),
            Arc::clone(&self.inner.people),
        );
        self
    }

    /// Attach a [`StatusAdvertiser`] hook, so `set_serving`/`set_idle`
    /// re-publish this box's status whenever it changes.
    ///
    /// Meant to be called immediately after construction (`main.rs` does
    /// `AppState::new(..).with_status_advertiser(..)` before ever cloning
    /// the result into a handler/router), while this `AppState` is still
    /// the sole owner of its `Arc<Inner>` -- `Arc::get_mut` only succeeds
    /// under that condition. If it's ever called after a clone exists
    /// (which shouldn't happen in practice), this logs a warning and leaves
    /// the advertiser unset rather than panicking.
    pub fn with_status_advertiser(mut self, advertiser: Arc<dyn StatusAdvertiser>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.advertiser = Some(advertiser),
            None => eprintln!(
                "tt-station-agentd: with_status_advertiser called on an already-shared AppState; advertiser not attached"
            ),
        }
        self
    }

    /// Configure the `GET /telemetry` stream: which `tt-smi` binary to run
    /// and how often (ms) to push a snapshot. Additive counterpart to
    /// `with_status_advertiser` -- same "call immediately after construction,
    /// while this is still the sole owner of its `Arc<Inner>`" contract
    /// (`Arc::get_mut` only succeeds then). Called after a clone exists, it
    /// logs a warning and leaves the defaults in place rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still streams
    /// telemetry, using `tt-smi` on `$PATH` at the default 1s cadence
    /// (`DEFAULT_TT_SMI_BIN` / `DEFAULT_TELEMETRY_INTERVAL_MS`).
    pub fn with_telemetry_config(mut self, tt_smi_bin: String, telemetry_interval_ms: u64) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => {
                inner.tt_smi_bin = tt_smi_bin;
                inner.telemetry_interval_ms = telemetry_interval_ms;
            }
            None => eprintln!(
                "tt-station-agentd: with_telemetry_config called on an already-shared AppState; telemetry config not applied"
            ),
        }
        self
    }

    /// Configure the additive `GET /serving` route: the serving host baked
    /// into discovered endpoints' `base_url`, and the agent's own serving
    /// port used to classify `agent` vs `external`. Additive counterpart to
    /// `with_telemetry_config`/`with_status_advertiser` -- same "call
    /// immediately after construction, while this is still the sole owner of
    /// its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds then). Called
    /// after a clone exists, it logs a warning and leaves the defaults in
    /// place rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still answers
    /// `/serving`, using `DEFAULT_SERVING_HOST`/`DEFAULT_SERVING_PORT`.
    pub fn with_serving_config(mut self, serving_host: String, serving_port: u16) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => {
                inner.serving_host = serving_host;
                inner.serving_port = serving_port;
            }
            None => eprintln!(
                "tt-station-agentd: with_serving_config called on an already-shared AppState; serving config not applied"
            ),
        }
        self
    }

    /// Point the `/logs` routes at a tt-inference-server checkout. Additive
    /// counterpart to `with_serving_config`/`with_device_mesh`/etc -- same
    /// "call immediately after construction, while this is still the sole
    /// owner of its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds
    /// then). Called after a clone exists, it logs a warning and leaves the
    /// default (`None`, meaning `GET /logs` answers 409) in place rather
    /// than panicking.
    ///
    /// Optional: an `AppState` never given this (e.g. the dstack backend,
    /// which has no `workflow_logs` dir) still answers `/logs` -- with a 409
    /// rather than a fabricated path.
    /// Enable the `/tt/*` model-manager routes with the official tt CLI. Same
    /// "call right after construction, while this is the sole owner of its
    /// `Arc<Inner>`" contract as the other `with_*` builders.
    pub fn with_tt_cli(mut self, tt: Arc<crate::ttcli::TtCli>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.tt_cli = Some(tt),
            None => eprintln!(
                "tt-station-agentd: with_tt_cli called on an already-shared AppState; tt CLI not applied"
            ),
        }
        self
    }

    /// Replace the (empty) history with one loaded from disk. Same "call right
    /// after construction" contract as the other `with_*` builders.
    pub fn with_history(mut self, history: crate::history::History) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.history = Arc::new(history),
            None => {
                eprintln!("tt-station-agentd: with_history called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Attach the document store. Same contract as the other `with_*` builders.
    pub fn with_docs(mut self, docs: crate::docs::DocStore) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.docs = Some(Arc::new(docs)),
            None => eprintln!("tt-station-agentd: with_docs called on a shared AppState; ignored"),
        }
        self
    }

    /// Enable `POST /pair/offer` for holders of `secret` (see `crate::pairqr`).
    pub fn with_local_secret(mut self, secret: String) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.local_secret = Some(secret),
            None => eprintln!(
                "tt-station-agentd: with_local_secret called on a shared AppState; ignored"
            ),
        }
        self
    }

    /// The box user's local secret, when `POST /pair/offer` is enabled.
    pub(crate) fn local_secret(&self) -> Option<&str> {
        self.inner.local_secret.as_deref()
    }

    /// What modules keep for the life of this agent (see [`PerAgent`]).
    pub(crate) fn per_agent(&self) -> &PerAgent {
        &self.inner.per_agent
    }

    /// Attach the co-serving companion slot. Same contract as the other `with_*`.
    pub fn with_companion(mut self, companion: crate::coserve::Companion) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.companion = Some(Arc::new(companion)),
            None => {
                eprintln!("tt-station-agentd: with_companion called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Load the `/v1` gateway's app keys and record the control port. Same
    /// contract as the other `with_*` builders.
    pub fn with_gateway(mut self, keys: crate::gateway::AppKeys, ctrl_port: u16) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => {
                inner.app_keys = Arc::new(keys);
                inner.ctrl_port = ctrl_port;
            }
            None => {
                eprintln!("tt-station-agentd: with_gateway called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Where chat-made images are kept. Same contract as the other `with_*`.
    pub fn with_creations(mut self, creations: crate::creations::Creations) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.creations = creations,
            None => {
                eprintln!("tt-station-agentd: with_creations called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Load the assistant's memories. Same contract as the other `with_*`.
    pub fn with_memories(mut self, memories: crate::memory::Memories) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.memories = Arc::new(memories),
            None => {
                eprintln!("tt-station-agentd: with_memories called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Load alert settings. Same contract as the other `with_*` builders.
    pub fn with_notifier(mut self, notifier: crate::notify::Notifier) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.notifier = Arc::new(notifier),
            None => {
                eprintln!("tt-station-agentd: with_notifier called on a shared AppState; ignored")
            }
        }
        self.wire_notifier()
    }

    /// Record the config directory. Same contract as the other `with_*`.
    pub fn with_config_dir(mut self, dir: PathBuf) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.config_dir = dir,
            None => {
                eprintln!("tt-station-agentd: with_config_dir called on a shared AppState; ignored")
            }
        }
        self
    }

    /// This is the box's own agent: it may stop the box's user services. Same
    /// contract as the other `with_*`.
    pub fn managing_units(mut self) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.manages_units = true,
            None => {
                eprintln!("tt-station-agentd: managing_units called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Whether this agent may stop the box's user services (see `managing_units`).
    pub fn manages_units(&self) -> bool {
        self.inner.manages_units
    }

    /// Load learned load times. Same contract as the other `with_*` builders.
    pub fn with_load_times(mut self, times: crate::modes::LoadTimes) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.load_times = Arc::new(times),
            None => {
                eprintln!("tt-station-agentd: with_load_times called on a shared AppState; ignored")
            }
        }
        self
    }

    /// Record that HTTPS is served. Same contract as the other `with_*`.
    pub fn with_tls(mut self, tls: crate::tls::TlsInfo) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.tls = Some(tls),
            None => eprintln!("tt-station-agentd: with_tls called on a shared AppState; ignored"),
        }
        self
    }

    /// Enable Lab mode. Same contract as the other `with_*` builders.
    pub fn with_lab(mut self, lab: crate::lab::Lab) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.lab = Some(Arc::new(lab)),
            None => eprintln!("tt-station-agentd: with_lab called on a shared AppState; ignored"),
        }
        self
    }

    /// Enable the job queue. Same contract as the other `with_*` builders.
    pub fn with_jobs(mut self, jobs: crate::jobs::Jobs) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.jobs = Some(Arc::new(jobs)),
            None => eprintln!("tt-station-agentd: with_jobs called on a shared AppState; ignored"),
        }
        self
    }

    pub fn jobs(&self) -> Option<Arc<crate::jobs::Jobs>> {
        self.inner.jobs.clone()
    }

    /// Enable tt-orchard bring-ups. Same contract as the other `with_*` builders.
    pub fn with_orchard(mut self, orchard: crate::orchard::Orchard) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.orchard = Some(Arc::new(orchard)),
            None => {
                eprintln!("tt-station-agentd: with_orchard called on a shared AppState; ignored")
            }
        }
        self
    }

    pub fn with_cpu_brain(mut self, on: bool) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.cpu_brain = on,
            None => {
                eprintln!("tt-station-agentd: with_cpu_brain called on a shared AppState; ignored")
            }
        }
        self
    }

    pub fn cpu_brain_enabled(&self) -> bool {
        self.inner.cpu_brain
    }

    pub fn with_people(mut self, people: crate::people::People) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.people = Arc::new(people),
            None => {
                eprintln!("tt-station-agentd: with_people called on a shared AppState; ignored")
            }
        }
        self.wire_notifier()
    }

    pub fn with_schedules(mut self, schedules: crate::schedules::Schedules) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.schedules = Arc::new(schedules),
            None => {
                eprintln!("tt-station-agentd: with_schedules called on a shared AppState; ignored")
            }
        }
        self
    }

    pub fn schedules(&self) -> Arc<crate::schedules::Schedules> {
        Arc::clone(&self.inner.schedules)
    }

    pub fn people(&self) -> Arc<crate::people::People> {
        Arc::clone(&self.inner.people)
    }

    /// Keep the activity log at `audit` (`crate::audit`) rather than in memory.
    pub fn with_audit(mut self, audit: crate::audit::AuditLog) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.audit = Arc::new(audit),
            None => eprintln!("tt-station-agentd: with_audit called on a shared AppState; ignored"),
        }
        self
    }

    pub fn audit(&self) -> Arc<crate::audit::AuditLog> {
        Arc::clone(&self.inner.audit)
    }

    pub fn orchard(&self) -> Option<Arc<crate::orchard::Orchard>> {
        self.inner.orchard.clone()
    }

    pub fn orchard_busy(&self) -> bool {
        self.inner
            .orchard_busy
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_orchard_busy(&self, busy: bool) {
        self.inner
            .orchard_busy
            .store(busy, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn bringup_busy(&self) -> bool {
        self.inner
            .bringup_busy
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn set_bringup_busy(&self, busy: bool) {
        self.inner
            .bringup_busy
            .store(busy, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn chips_resetting(&self) -> bool {
        self.inner
            .chips_resetting
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
    }

    /// Pause chip readings until the guard drops (a reset is under way).
    pub fn pause_readings(&self) -> ReadingsPause {
        self.inner
            .chips_resetting
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ReadingsPause(Arc::clone(&self.inner.chips_resetting))
    }

    pub fn set_orchard_chips(&self, in_use: bool) {
        self.inner
            .orchard_chips
            .store(in_use, std::sync::atomic::Ordering::SeqCst);
    }

    /// Who has the chips when it isn't this agent's serving: a Lab run or a
    /// tt-orchard bring-up. Model starts, chip resets, power actions and
    /// tt-smi all wait while this is `Some` -- it's the message to show.
    pub fn chips_holder(&self) -> Option<&'static str> {
        if self.lab_busy() {
            Some("A silicon check is using the chips. Wait for it to finish.")
        } else if self.orchard_busy() {
            Some(
                "A model bring-up (Orchard) is using the chips. Pause or stop it in Orchard first.",
            )
        } else if self.bringup_busy() {
            Some("A model bring-up is using the chips. Stop it in Bring-up first.")
        } else {
            None
        }
    }

    pub(crate) fn lab(&self) -> Option<Arc<crate::lab::Lab>> {
        self.inner.lab.clone()
    }

    pub fn lab_busy(&self) -> bool {
        self.inner
            .lab_busy
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Claim the chips for a lab run; false if one already has them.
    pub(crate) fn try_set_lab_busy(&self) -> bool {
        self.inner
            .lab_busy
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            )
            .is_ok()
    }

    pub(crate) fn set_lab_busy(&self, busy: bool) {
        self.inner
            .lab_busy
            .store(busy, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn tls(&self) -> Option<crate::tls::TlsInfo> {
        self.inner.tls.clone()
    }

    pub fn load_times(&self) -> Arc<crate::modes::LoadTimes> {
        Arc::clone(&self.inner.load_times)
    }

    pub(crate) fn config_dir(&self) -> PathBuf {
        self.inner.config_dir.clone()
    }

    pub fn notifier(&self) -> Arc<crate::notify::Notifier> {
        Arc::clone(&self.inner.notifier)
    }

    pub fn memories(&self) -> Arc<crate::memory::Memories> {
        Arc::clone(&self.inner.memories)
    }

    pub(crate) fn creations(&self) -> &crate::creations::Creations {
        &self.inner.creations
    }

    pub(crate) fn app_keys(&self) -> Arc<crate::gateway::AppKeys> {
        Arc::clone(&self.inner.app_keys)
    }

    pub(crate) fn ctrl_port(&self) -> u16 {
        self.inner.ctrl_port
    }

    pub fn companion(&self) -> Option<Arc<crate::coserve::Companion>> {
        self.inner.companion.clone()
    }

    pub(crate) fn docs(&self) -> Option<Arc<crate::docs::DocStore>> {
        self.inner.docs.clone()
    }

    pub fn history(&self) -> Arc<crate::history::History> {
        Arc::clone(&self.inner.history)
    }

    /// Whether the served chat model is answering (see `crate::liveness`).
    pub fn liveness(&self) -> Arc<crate::liveness::Liveness> {
        Arc::clone(&self.inner.liveness)
    }

    pub(crate) fn tt_cli(&self) -> Option<Arc<crate::ttcli::TtCli>> {
        self.inner.tt_cli.clone()
    }

    pub fn with_log_source(mut self, repo_dir: impl Into<std::path::PathBuf>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.tt_inference_repo = Some(repo_dir.into()),
            None => eprintln!(
                "tt-station-agentd: with_log_source called on an already-shared AppState; log source not applied"
            ),
        }
        self
    }

    /// Set this box's detected device-mesh label (see the `device_mesh`
    /// field's doc comment). Additive counterpart to `with_serving_config`/
    /// `with_telemetry_config`/`with_status_advertiser` -- same "call
    /// immediately after construction, while this is still the sole owner of
    /// its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds then). Called
    /// after a clone exists, it logs a warning and leaves the default (`None`)
    /// in place rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still answers
    /// `/status` with `"device_mesh": null`.
    pub fn with_device_mesh(mut self, device_mesh: Option<String>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.device_mesh = device_mesh,
            None => eprintln!(
                "tt-station-agentd: with_device_mesh called on an already-shared AppState; device_mesh not applied"
            ),
        }
        self
    }

    /// Set this box's detected primary NIC MAC (see the `mac` field's doc
    /// comment). Additive counterpart to `with_device_mesh`/
    /// `with_serving_config`/etc -- same "call immediately after
    /// construction, while this is still the sole owner of its `Arc<Inner>`"
    /// contract (`Arc::get_mut` only succeeds then). Called after a clone
    /// exists, it logs a warning and leaves the default (`None`) in place
    /// rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still answers
    /// `/status` with `"mac": null` (Wake-on-LAN simply unavailable).
    pub fn with_mac(mut self, mac: Option<String>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.mac = mac,
            None => eprintln!(
                "tt-station-agentd: with_mac called on an already-shared AppState; mac not applied"
            ),
        }
        self
    }

    /// Attach the redacted `ConfigSummary` `GET /config` serves. Additive
    /// counterpart to `with_serving_config`/`with_telemetry_config` -- same
    /// "call immediately after construction, while this is still the sole
    /// owner of its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds
    /// then). Called after a clone exists, it logs a warning and leaves the
    /// empty default summary in place rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still answers
    /// `/config`, with the empty-but-valid default `new_inner` builds
    /// (no active profile, `runpy`/`127.0.0.1`/`8000` defaults).
    pub fn with_config_summary(mut self, summary: ConfigSummary) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.config_summary = summary,
            None => eprintln!(
                "tt-station-agentd: with_config_summary called on an already-shared AppState; config summary not applied"
            ),
        }
        self
    }

    /// Configure the target `authorized_keys` file and account name for
    /// `POST`/`DELETE /ssh/authorize` (Task 2). Additive counterpart to
    /// `with_config_summary`/`with_device_mesh`/etc -- same "call
    /// immediately after construction, while this is still the sole owner
    /// of its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds then).
    /// Called after a clone exists, it logs a warning and leaves the
    /// defaults (empty path, `"ttuser"`) in place rather than panicking.
    ///
    /// Optional: an `AppState` never given this config still answers
    /// `/ssh/authorize`, but against an empty path -- `authkeys::authorize`/
    /// `revoke` will surface an I/O error rather than silently touching a
    /// real `~/.ssh` no one configured. `main.rs` always calls this with a
    /// resolved `$HOME`/`--ssh-user` target (see `resolve_ssh_target`);
    /// tests call it with a temp path.
    pub fn with_ssh_target(mut self, path: PathBuf, user: String) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => {
                inner.ssh_authorized_keys_path = path;
                inner.ssh_user = user;
            }
            None => eprintln!(
                "tt-station-agentd: with_ssh_target called on an already-shared AppState; ssh target not applied"
            ),
        }
        self
    }

    /// Override the four power-action command vectors (tests / mock-box
    /// inject a harmless stub so no real power event fires). Additive
    /// counterpart to `with_ssh_target`/`with_log_source`/etc -- same "call
    /// immediately after construction, while this is still the sole owner
    /// of its `Arc<Inner>`" contract (`Arc::get_mut` only succeeds then).
    /// Called after a clone exists, it logs a warning and leaves the
    /// defaults in place rather than panicking.
    ///
    /// Defaults set in `new_inner` (unchanged if this is never called):
    /// `tt-smi -r` (reset-chips) and `systemctl suspend|reboot|poweroff`.
    pub fn with_power_config(
        mut self,
        reset_chips: Vec<String>,
        suspend: Vec<String>,
        reboot: Vec<String>,
        shutdown: Vec<String>,
    ) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => {
                inner.power_reset_chips_cmd = reset_chips;
                inner.power_suspend_cmd = suspend;
                inner.power_reboot_cmd = reboot;
                inner.power_shutdown_cmd = shutdown;
            }
            None => eprintln!(
                "tt-station-agentd: with_power_config called on an already-shared AppState; power config not applied"
            ),
        }
        self
    }

    /// Set this box's detected `gozer` capability (see the `gozer` field's
    /// doc comment). Additive counterpart to `with_device_mesh`/`with_mac`
    /// -- same "call immediately after construction, while this is still
    /// the sole owner of its `Arc<Inner>`" contract (`Arc::get_mut` only
    /// succeeds then). Called after a clone exists, it logs a warning and
    /// leaves the default (`None`) in place rather than panicking.
    ///
    /// Optional: an `AppState` never given this still behaves exactly as
    /// before gozer existed -- whole-box serving, no leasing. `main.rs`
    /// calls this with the result of a bounded startup probe
    /// (`detect_startup_gozer`, mirroring `detect_startup_device_mesh`);
    /// tests that don't care about gozer never need to call it.
    pub fn with_gozer(mut self, gozer: Option<crate::gozer::Capability>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.gozer = gozer,
            None => eprintln!(
                "tt-station-agentd: with_gozer called on an already-shared AppState; gozer capability not applied"
            ),
        }
        self
    }

    /// This box's detected `gozer` capability, or `None` if gozer is absent
    /// or was never probed for (see `with_gozer`). Not read by any route
    /// yet -- Task 1 only stores it; a later task exposes it via
    /// `GET /status`'s `leasing` field (see the design doc).
    pub fn gozer(&self) -> Option<&crate::gozer::Capability> {
        self.inner.gozer.as_ref()
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn chips(&self) -> &str {
        &self.inner.chips
    }

    /// This box's detected device-mesh label, or `None` if detection failed
    /// or never ran (see `with_device_mesh`). Read by `GET /status`.
    pub fn device_mesh(&self) -> Option<&str> {
        self.inner.device_mesh.as_deref()
    }

    /// This box's detected primary NIC MAC, or `None` if detection failed or
    /// never ran (see `with_mac`). Read by `GET /status`.
    pub fn mac(&self) -> Option<&str> {
        self.inner.mac.as_deref()
    }

    /// Serving host baked into `GET /serving` `base_url`s (see `with_serving_config`).
    pub(crate) fn serving_host(&self) -> &str {
        &self.inner.serving_host
    }

    /// The agent's own serving port, for `GET /serving`'s agent/external
    /// classification (see `with_serving_config`).
    pub(crate) fn serving_port(&self) -> u16 {
        self.inner.serving_port
    }

    /// Snapshot the `ConfigSummary` `GET /config` serves (see
    /// `with_config_summary`).
    pub(crate) fn config_summary(&self) -> ConfigSummary {
        self.inner.config_summary.clone()
    }

    /// The `authorized_keys` path `/ssh/authorize` reads and writes (see
    /// `with_ssh_target`).
    pub(crate) fn ssh_path(&self) -> &Path {
        &self.inner.ssh_authorized_keys_path
    }

    /// The account name `POST /ssh/authorize` reports back as `ssh_user`
    /// (see `with_ssh_target`).
    pub(crate) fn ssh_user(&self) -> &str {
        &self.inner.ssh_user
    }

    /// The tt-inference-server checkout `GET /logs` tails workflow logs
    /// from, if the runpy backend is active (see `with_log_source`).
    pub(crate) fn tt_inference_repo(&self) -> Option<&std::path::Path> {
        self.inner.tt_inference_repo.as_deref()
    }

    /// Park a disruptive action the assistant proposed, returning the fresh
    /// random id `POST /assistant/confirm` will need to approve or deny it.
    ///
    /// Sweeps expired entries first and then, if the map is still at
    /// `assistant::MAX_PENDING`, evicts the OLDEST entry -- see
    /// `Inner::pending_actions` for why the map must stay bounded. The id is
    /// 128 random bits (same generator as `pair_id`), so it can't be guessed
    /// -- though guessing one would only let an already-authed caller confirm
    /// an action another authed caller proposed, since the confirm route is
    /// bearer-guarded too.
    /// The newest pending action proposed within `within`, for "yes, do it".
    pub(crate) fn latest_pending_action(&self, within: std::time::Duration) -> Option<String> {
        let pending = self
            .inner
            .pending_actions
            .lock()
            .expect("pending_actions mutex poisoned");
        pending
            .iter()
            .filter(|(_, a)| a.created.elapsed() < within.min(crate::assistant::PENDING_TTL))
            .max_by_key(|(_, a)| a.created)
            .map(|(k, _)| k.clone())
    }

    pub(crate) fn insert_pending_action(&self, action: crate::assistant::PendingAction) -> String {
        let id = pairing::issue_pair_id();
        let mut pending = self
            .inner
            .pending_actions
            .lock()
            .expect("pending_actions mutex poisoned");
        pending.retain(|_, a| a.created.elapsed() < crate::assistant::PENDING_TTL);
        while pending.len() >= crate::assistant::MAX_PENDING {
            let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, a)| a.created)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            pending.remove(&oldest);
        }
        pending.insert(id.clone(), action);
        id
    }

    /// Which tool a pending assistant action would run, without taking it.
    pub(crate) fn pending_action_tool(&self, id: &str) -> Option<String> {
        self.inner
            .pending_actions
            .lock()
            .expect("pending_actions mutex poisoned")
            .get(id)
            .map(|a| a.tool.clone())
    }

    /// Remove and return the pending action `id`, or `None` if it's unknown
    /// or has outlived `assistant::PENDING_TTL`. Always a REMOVE, never a
    /// peek: whatever the caller decides (approve or deny), the action is
    /// consumed so the same id can never run twice.
    pub(crate) fn take_pending_action(&self, id: &str) -> Option<crate::assistant::PendingAction> {
        let mut pending = self
            .inner
            .pending_actions
            .lock()
            .expect("pending_actions mutex poisoned");
        pending.retain(|_, a| a.created.elapsed() < crate::assistant::PENDING_TTL);
        pending.remove(id)
    }
}

/// Run `tt-smi -s` off the async runtime and return its stdout (the telemetry
/// frame). Wraps [`telemetry::snapshot`] with the real command runner inside
/// `spawn_blocking`, turning a join failure into an `Err` rather than a panic.
async fn collect_snapshot(tt_smi_bin: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        let runner = RealCommandRunner;
        telemetry::snapshot(&tt_smi_bin, &|args| runner.run(args))
    })
    .await
    .map_err(|join_err| anyhow::anyhow!("telemetry snapshot task panicked: {join_err}"))?
}

/// Why a start was refused: another start or a stop is already in flight.
/// Answered as `409` (see `contention_aware_error`).
#[derive(Debug)]
pub struct StartInFlight(pub String);

impl std::fmt::Display for StartInFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StartInFlight {}

#[cfg(test)]
mod pair_init_tests {
    use super::*;

    #[test]
    fn pairing_asks_are_limited_per_address_with_a_ceiling() {
        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        );
        let ip = |n: u8| Some(std::net::IpAddr::from([192, 168, 1, n]));
        for _ in 0..PAIR_INITS_PER_ADDRESS {
            assert!(state.pair_init_allowed(ip(9), 1000));
        }
        assert!(
            !state.pair_init_allowed(ip(9), 1000),
            "one address hammering is stopped"
        );
        assert!(
            state.pair_init_allowed(ip(2), 1000),
            "and doesn't lock out the owner's phone"
        );
        // Many addresses together hit the ceiling.
        let mut n = 10;
        while state.pair_init_allowed(ip(n), 1000) {
            n += 1;
        }
        assert_eq!(
            state.inner.pair_inits.lock().unwrap().len(),
            PAIR_INITS_PER_MINUTE
        );
        // A minute later, everyone may ask again.
        assert!(state.pair_init_allowed(ip(9), 1061));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cached_snapshot_dedupes_tt_smi_within_ttl() {
        // Stub `tt-smi`: a script that records one run (appends a byte to a
        // counter file) and prints a minimal snapshot. Two `cached_snapshot`
        // calls within the TTL must execute it only ONCE — the whole point of
        // the shared cache (N clients → ~one tt-smi per interval, not one each).
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttsmi-dedup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let _ = std::fs::remove_file(&counter);
        let script = dir.join("tt-smi-stub.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{c}'\nprintf '{{\"device_info\":[]}}'\n",
                c = counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // 10s TTL so both calls land in the same window.
        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_telemetry_config(script.to_string_lossy().into_owned(), 10_000);

        let a = state.cached_snapshot().await.unwrap();
        let b = state.cached_snapshot().await.unwrap();
        assert_eq!(a, b, "second call reuses the cached snapshot");
        let runs = std::fs::read(&counter).map(|v| v.len()).unwrap_or(0);
        assert_eq!(
            runs, 1,
            "tt-smi ran once within the TTL, not per call (got {runs})"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn concurrent_telemetry_viewers_share_one_tt_smi() {
        // Eight `/telemetry` sockets ticking at once (each tick is a
        // `cached_snapshot`) run tt-smi once between them, not once each.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttsmi-herd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let _ = std::fs::remove_file(&counter);
        let script = dir.join("tt-smi-stub.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{c}'\nsleep 0.3\nprintf '{{\"device_info\":[]}}'\n",
                c = counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_telemetry_config(script.to_string_lossy().into_owned(), 1_000);
        let ticks: Vec<_> = (0..8)
            .map(|_| {
                let st = state.clone();
                tokio::spawn(async move { st.cached_snapshot().await.is_ok() })
            })
            .collect();
        for t in ticks {
            assert!(t.await.unwrap());
        }
        let runs = std::fs::read(&counter).map(|v| v.len()).unwrap_or(0);
        assert_eq!(runs, 1, "eight viewers, one tt-smi (got {runs})");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_power_command_runs_the_configured_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttpower-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let _ = std::fs::remove_file(&marker);
        let script = dir.join("fake-power.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf x >> '{m}'\n", m = marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Through sh, not exec'd directly: another test forking while this
        // file is still open for writing makes exec fail with "Text file busy".
        let cmd = vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()];

        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_power_config(cmd.clone(), cmd.clone(), cmd.clone(), cmd.clone());

        state
            .run_power_command(crate::power::PowerAction::Reboot, false)
            .expect("power command runs");
        assert!(marker.exists(), "configured power command was executed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pending(tool: &str, age: Duration) -> crate::assistant::PendingAction {
        crate::assistant::PendingAction {
            tool: tool.to_string(),
            args: serde_json::json!({}),
            summary: String::new(),
            created: Instant::now() - age,
        }
    }

    fn bare_state() -> AppState {
        AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
    }

    #[test]
    fn pending_actions_are_taken_once_and_expire_after_the_ttl() {
        let state = bare_state();
        let fresh = state.insert_pending_action(pending("stop_model", Duration::ZERO));
        let stale = state.insert_pending_action(pending(
            "stop_model",
            crate::assistant::PENDING_TTL + Duration::from_secs(1),
        ));
        assert!(state.take_pending_action(&stale).is_none(), "expired");
        assert_eq!(
            state.take_pending_action(&fresh).unwrap().tool,
            "stop_model"
        );
        assert!(state.take_pending_action(&fresh).is_none(), "consumed");
    }

    #[test]
    fn pending_actions_are_capped_by_evicting_the_oldest() {
        let state = bare_state();
        let oldest = state.insert_pending_action(pending("run_model", Duration::from_secs(60)));
        let mut rest = Vec::new();
        for _ in 0..crate::assistant::MAX_PENDING {
            rest.push(state.insert_pending_action(pending("stop_model", Duration::ZERO)));
        }
        assert!(
            state.take_pending_action(&oldest).is_none(),
            "oldest evicted"
        );
        for id in rest {
            assert!(state.take_pending_action(&id).is_some());
        }
    }
}
