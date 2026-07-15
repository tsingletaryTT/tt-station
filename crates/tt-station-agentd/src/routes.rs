//! HTTP routes for `tt-station-agentd`.
//!
//! `AppState` is the one piece of shared state every handler (present and
//! future) reaches through. It's deliberately built as a cheap-to-`Clone`
//! handle (`Arc<Inner>`) around an `Inner` struct so later tasks can grow it
//! -- Task 7 adding a pairing-token set, Task 9 swapping in a real serving
//! backend, Task 10 adding control routes that mutate `status` -- without
//! reshaping the handle itself or how it's threaded through `axum::State`.
//!
//! `app()` builds the `Router` from an `AppState` with no side effects (no
//! mDNS registration, no socket binding), so tests can spin up the real
//! router against an ephemeral port without dragging the network stack
//! along for the ride. `main.rs` is responsible for anything that touches
//! the outside world.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::{FromRequestParts, RawQuery},
    http::{request::Parts, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use libttstation::model::{ConfigSummary, Endpoint, ModelsResponse, ServingList, ServingStatus};
use serde::{Deserialize, Serialize};

use crate::authkeys;
use crate::pairing;
use crate::serving::discovery::discover_serving;
use crate::serving::docker::{CommandRunner, RealCommandRunner};
use crate::serving::ServingBackend;
use crate::telemetry;

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

/// How long a pairing code stays valid after `/pair/init` mints it. Short
/// enough that a code seen once (e.g. shoulder-surfed, or left in shell
/// history) is useless a couple of minutes later; long enough that a human
/// reading a 6-digit code off the box's screen and typing it into the
/// client isn't in a race against the clock.
const PAIR_TTL: Duration = Duration::from_secs(120);

/// How many wrong-code guesses a single `pair_id` tolerates before
/// `complete_pair` invalidates it outright. A 6-digit code (10^6 possible
/// values) with a 120s TTL and no attempt cap would let a LAN client just
/// hammer `/pair/complete` with every value in range; capping wrong guesses
/// at a small number closes that off while still giving a human who
/// fat-fingers the code a few real chances to correct themselves.
pub const MAX_PAIR_ATTEMPTS: u32 = 5;

/// Seam for re-publishing this box's advertised `status` whenever it
/// changes (`/run` succeeding, `/stop` completing), so the mDNS TXT record
/// `tt discover` reads over the LAN never goes stale the way it did before
/// this trait existed (see the module-level findings doc: `tt discover`
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
    /// `pair_id -> (code, expiry, wrong_attempts)`. An entry is removed the
    /// moment it's consumed -- successfully, because it expired, or because
    /// `wrong_attempts` hit `MAX_PAIR_ATTEMPTS` -- by `complete_pair`, so
    /// this map only ever holds pairing attempts that are still "live".
    pending_pairs: Mutex<HashMap<String, (String, Instant, u32)>>,
    /// Bearer tokens minted by successful `/pair/complete` calls. Task 10's
    /// control routes will check incoming requests against this set; for
    /// now `/pair/complete` is the only thing that populates it.
    tokens: Mutex<HashSet<String>>,
    /// Optional path to persist the `tokens` set to on disk, so a paired
    /// client survives an agent restart instead of every restart emptying
    /// `tokens` and forcing a re-pair. `None` (the default, what
    /// `AppState::new` builds) means in-memory only -- exactly the
    /// pre-persistence behavior every existing test relies on. Set via
    /// `AppState::new_persisting`, which also loads any tokens already at
    /// this path into `tokens` up front.
    token_store: Option<PathBuf>,
    /// Dedicated lock serializing DISK writes of the token store, kept
    /// separate from `tokens` itself. `persist_tokens` holds this (not
    /// `tokens`) across the snapshot-capture + file-write: concurrent
    /// `/pair/complete` calls each insert into `tokens` under its own brief
    /// lock, then queue up here to write. Because the snapshot is retaken
    /// fresh *after* acquiring this lock (not reused from whatever was
    /// captured back when `insert_token` ran), and `tokens` only ever grows,
    /// whichever write actually lands last on disk is guaranteed to reflect
    /// the union of every insert that happened before it -- a stale snapshot
    /// captured earlier can never clobber a fresher one that already made it
    /// to disk. See `persist_tokens`.
    write_lock: Mutex<()>,
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
    /// `tt --json status`) can rank models by hardware fit.
    device_mesh: Option<String>,
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
}

impl AppState {
    /// Construct fresh state for a box that starts out idle, wired to
    /// `backend` for actually starting/stopping model serving. Callers
    /// (`main.rs`, and this crate's tests) build `backend` via
    /// `serving::make_backend` or a test double and hand it in already
    /// wrapped in an `Arc`, since `AppState` never needs to construct a
    /// backend itself.
    pub fn new(name: String, chips: String, backend: Arc<dyn ServingBackend>) -> Self {
        Self::new_inner(name, chips, backend, HashSet::new(), None)
    }

    /// Construct state whose bearer-token set is persisted to `token_store`
    /// on disk, so a paired client survives an agent restart instead of
    /// being forced to re-pair.
    ///
    /// Any tokens already at `token_store` are loaded into the in-memory set
    /// up front (standing in for "the agent restarted, but the file from
    /// its previous run is still there"). A missing file is treated as "no
    /// tokens yet" -- the normal state for a box that's never persisted a
    /// token before. An unreadable or corrupt file logs a warning to stderr
    /// and also starts empty: a hand-corrupted or half-written token store
    /// must never fail agent startup, let alone panic it.
    ///
    /// From this point on, every successful `/pair/complete` (via
    /// `insert_token`) rewrites the whole token set back out to
    /// `token_store` -- see `persist_tokens`.
    pub fn new_persisting(
        name: String,
        chips: String,
        backend: Arc<dyn ServingBackend>,
        token_store: PathBuf,
    ) -> Self {
        let tokens = load_tokens(&token_store);
        Self::new_inner(name, chips, backend, tokens, Some(token_store))
    }

    /// Shared construction path for `new`/`new_persisting`: only the
    /// starting `tokens` set and whether persistence is enabled differ
    /// between the two.
    fn new_inner(
        name: String,
        chips: String,
        backend: Arc<dyn ServingBackend>,
        tokens: HashSet<String>,
        token_store: Option<PathBuf>,
    ) -> Self {
        AppState {
            inner: Arc::new(Inner {
                name,
                chips,
                status: Mutex::new(ServingStatus::Idle),
                endpoint: Mutex::new(None),
                backend,
                pending_pairs: Mutex::new(HashMap::new()),
                tokens: Mutex::new(tokens),
                token_store,
                write_lock: Mutex::new(()),
                advertiser: None,
                tt_smi_bin: DEFAULT_TT_SMI_BIN.to_string(),
                telemetry_interval_ms: DEFAULT_TELEMETRY_INTERVAL_MS,
                tt_smi_cache: tokio::sync::Mutex::new(None),
                serving_host: DEFAULT_SERVING_HOST.to_string(),
                serving_port: DEFAULT_SERVING_PORT,
                device_mesh: None,
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
            }),
        }
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

    /// `tt-smi` binary the `/telemetry` stream runs (see `with_telemetry_config`).
    fn tt_smi_bin(&self) -> &str {
        &self.inner.tt_smi_bin
    }

    /// Interval (ms) between `/telemetry` snapshots (see `with_telemetry_config`).
    fn telemetry_interval_ms(&self) -> u64 {
        self.inner.telemetry_interval_ms
    }

    /// Raw `tt-smi -s` snapshot, shared across all `/telemetry` clients with a
    /// one-interval TTL. The first caller in each interval runs `tt-smi`; callers
    /// that arrive while that result is still fresh reuse it — so N concurrent
    /// clients cause ~one `tt-smi` per interval instead of one each. The lock is
    /// held across the shell-out on purpose: a thundering herd of ticks blocks on
    /// it, and by the time they acquire it the fresh result is already cached, so
    /// only one `tt-smi` actually runs. Errors are NOT cached — the next tick
    /// retries — and while an entry is stale the previous frame is replaced, never
    /// served past its interval.
    async fn cached_snapshot(&self) -> anyhow::Result<String> {
        let ttl = Duration::from_millis(self.telemetry_interval_ms().max(1));
        let mut cache = self.inner.tt_smi_cache.lock().await;
        if let Some((at, json)) = cache.as_ref() {
            if at.elapsed() < ttl {
                return Ok(json.clone());
            }
        }
        let json = collect_snapshot(self.tt_smi_bin().to_string()).await?;
        *cache = Some((Instant::now(), json.clone()));
        Ok(json)
    }

    /// Serving host baked into `GET /serving` `base_url`s (see `with_serving_config`).
    fn serving_host(&self) -> &str {
        &self.inner.serving_host
    }

    /// The agent's own serving port, for `GET /serving`'s agent/external
    /// classification (see `with_serving_config`).
    fn serving_port(&self) -> u16 {
        self.inner.serving_port
    }

    /// Snapshot the `ConfigSummary` `GET /config` serves (see
    /// `with_config_summary`).
    fn config_summary(&self) -> ConfigSummary {
        self.inner.config_summary.clone()
    }

    /// The `authorized_keys` path `/ssh/authorize` reads and writes (see
    /// `with_ssh_target`).
    fn ssh_path(&self) -> &Path {
        &self.inner.ssh_authorized_keys_path
    }

    /// The account name `POST /ssh/authorize` reports back as `ssh_user`
    /// (see `with_ssh_target`).
    fn ssh_user(&self) -> &str {
        &self.inner.ssh_user
    }

    /// The tt-inference-server checkout `GET /logs` tails workflow logs
    /// from, if the runpy backend is active (see `with_log_source`).
    fn tt_inference_repo(&self) -> Option<&std::path::Path> {
        self.inner.tt_inference_repo.as_deref()
    }

    /// Run the configured command for `action`, blocking (call under
    /// `spawn_blocking` -- this shells out, same rule as `ServingBackend::
    /// start`/`stop`). Machine ops (suspend/reboot/shutdown) best-effort
    /// stop any serving container first so a model isn't hard-killed by the
    /// machine going down; a stop failure is logged but non-fatal here --
    /// we're taking the box down regardless, so refusing the power action
    /// over a stop that didn't work would just strand the operator. Does
    /// NOT touch tokens/SSH/status: unlike `POST /reset` (which unpairs),
    /// every power action here -- including `reset-chips` -- preserves
    /// pairing.
    pub fn run_power_command(&self, action: crate::power::PowerAction) -> anyhow::Result<()> {
        use crate::power::PowerAction;

        if action.is_machine_op() {
            if let Some(ep) = self.endpoint() {
                if let Err(e) = self.backend().stop(&ep.model) {
                    eprintln!(
                        "power: best-effort stop of '{}' before {action:?} failed (continuing): {e}",
                        ep.model
                    );
                }
            }
        }

        let cmd = match action {
            PowerAction::ResetChips => &self.inner.power_reset_chips_cmd,
            PowerAction::Suspend => &self.inner.power_suspend_cmd,
            PowerAction::Reboot => &self.inner.power_reboot_cmd,
            PowerAction::Shutdown => &self.inner.power_shutdown_cmd,
        };
        let (bin, args) = cmd
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty power command configured for {action:?}"))?;
        let status = std::process::Command::new(bin)
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to spawn power command {cmd:?}: {e}"))?;
        if !status.success() {
            anyhow::bail!("power command {cmd:?} exited with {status}");
        }
        Ok(())
    }

    /// Cheap `Arc` clone of the serving backend, for a handler to move into
    /// `tokio::task::spawn_blocking` -- `ServingBackend::start`/`stop` are
    /// sync and must never be called directly from an async fn (see
    /// `serving/mod.rs`).
    fn backend(&self) -> Arc<dyn ServingBackend> {
        Arc::clone(&self.inner.backend)
    }

    /// Snapshot the current serving status (locks briefly, then clones out).
    pub fn status(&self) -> ServingStatus {
        self.inner
            .status
            .lock()
            .expect("status mutex poisoned")
            .clone()
    }

    /// The current serving status, RECONCILED against docker reality, and
    /// self-healing the stored state when it's stale.
    ///
    /// `status` is only the agent's last serving *intent* -- flipped to
    /// `Serving` on `/run`, back to `Idle` only on the agent's own `/stop`. A
    /// model stopped out of band (a manual `docker stop`, a crash) never runs
    /// through `/stop`, so the stored status gets stuck reporting `Serving`
    /// while nothing is actually up. This probes docker (`discover_serving`,
    /// off the async runtime like `/serving`) and applies
    /// [`reconcile_status`]: if the agent's own serve is gone, it calls
    /// `set_idle()` so `/status`, `/endpoint`, AND the mDNS advertisement all
    /// stop lying, then returns the corrected status.
    ///
    /// The docker probe (delegated to the backend) only runs when the stored
    /// status is `Serving` (the only state that can be stale-positive); an
    /// `Idle` status returns immediately without touching the backend, keeping
    /// the common idle poll cheap. The probe is blocking I/O, so it runs on
    /// `spawn_blocking` (like `/serving`).
    async fn reconciled_status(&self) -> ServingStatus {
        let stored = self.status();
        if !matches!(stored, ServingStatus::Serving(_)) {
            return stored;
        }
        let backend = self.backend();
        let reconciled = tokio::task::spawn_blocking(move || backend.reconciled_status())
            .await
            .ok()
            .and_then(Result::ok)
            // A join panic or backend error leaves the stored status as-is
            // (never worse than today's behavior).
            .unwrap_or_else(|| stored.clone());
        if matches!(reconciled, ServingStatus::Idle) {
            // Stale-positive: the backend's serve is gone. Heal the stored
            // state (clears the endpoint + re-advertises Idle over mDNS).
            self.set_idle();
        }
        reconciled
    }

    /// Snapshot the currently-serving `Endpoint`, or `None` if idle.
    fn endpoint(&self) -> Option<Endpoint> {
        self.inner
            .endpoint
            .lock()
            .expect("endpoint mutex poisoned")
            .clone()
    }

    /// Which model is currently serving, read off `status` -- `None` when
    /// idle. Used by `/stop` to know which model to tell the backend to
    /// stop, without needing a separate "current model" field that could
    /// drift from `status`.
    fn current_model(&self) -> Option<String> {
        match &*self.inner.status.lock().expect("status mutex poisoned") {
            ServingStatus::Serving(model) => Some(model.clone()),
            ServingStatus::Idle => None,
        }
    }

    /// Record a successful `/run`: flip `status` to `Serving(endpoint.model)`
    /// and remember `endpoint` for `/endpoint` to hand back later. Both
    /// fields are updated while holding both locks so a concurrent
    /// `/status` or `/endpoint` request never observes one updated without
    /// the other.
    ///
    /// The new status is then re-published via `advertise_status` -- but
    /// only after both locks above are dropped (the block ends first), so
    /// the mDNS re-registration (real I/O in the `main.rs` impl) never runs
    /// while either mutex is held.
    fn set_serving(&self, endpoint: Endpoint) {
        let new_status = ServingStatus::Serving(endpoint.model.clone());
        {
            let mut status = self.inner.status.lock().expect("status mutex poisoned");
            let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
            *status = new_status.clone();
            *stored_endpoint = Some(endpoint);
        }
        self.advertise_status(&new_status);
    }

    /// Record a successful `/stop` (or a no-op `/stop` while already idle):
    /// `status` goes back to `Idle` and any stored `Endpoint` is cleared,
    /// then (same lock-drop-then-advertise discipline as `set_serving`) the
    /// `Idle` status is re-published.
    fn set_idle(&self) {
        {
            let mut status = self.inner.status.lock().expect("status mutex poisoned");
            let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
            *status = ServingStatus::Idle;
            *stored_endpoint = None;
        }
        self.advertise_status(&ServingStatus::Idle);
    }

    /// Re-publish `status` via the attached `StatusAdvertiser`, if any.
    /// No-op when `advertiser` is `None` (every test that doesn't care about
    /// mDNS re-publishing, plus any agent run with persistence/advertising
    /// disabled).
    fn advertise_status(&self, status: &ServingStatus) {
        if let Some(advertiser) = &self.inner.advertiser {
            advertiser.advertise_status(status);
        }
    }

    /// Check a bearer token against the valid-token set minted by
    /// `/pair/complete`.
    fn is_valid_token(&self, token: &str) -> bool {
        self.inner
            .tokens
            .lock()
            .expect("tokens mutex poisoned")
            .contains(token)
    }

    /// Record a freshly-issued pairing attempt: `pair_id` will be accepted
    /// by `complete_pair` if presented with the matching `code` before
    /// `PAIR_TTL` elapses.
    ///
    /// Before inserting, sweeps out any *other* pending entries whose expiry
    /// has already passed. `complete_pair` only ever removes the one
    /// `pair_id` it was asked about, so a client that repeatedly hits
    /// `/pair/init` and never follows up with `/pair/complete` would
    /// otherwise grow `pending_pairs` unbounded -- a cheap, unauthenticated
    /// way to slowly exhaust memory. Sweeping here (rather than on a
    /// timer/background task) keeps the fix O(n) in the number of currently
    /// pending pairs and needs no extra moving parts: every `/pair/init`
    /// call is already a natural point to tidy up.
    fn insert_pending_pair(&self, pair_id: String, code: String) {
        let now = Instant::now();
        let expiry = now + PAIR_TTL;
        let mut pending = self
            .inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned");
        pending.retain(|_, (_, exp, _)| *exp > now);
        pending.insert(pair_id, (code, expiry, 0));
    }

    /// Check `code` against the pending pairing attempt for `pair_id`.
    ///
    /// Returns `true` only when `pair_id` is known, unexpired, and `code`
    /// matches -- in which case the pending entry is consumed (removed) so
    /// the same code can't be replayed. Returns `false` for an unknown
    /// pair_id or an expired one (also removed, since it can never succeed
    /// again).
    ///
    /// A code mismatch bumps that pair_id's wrong-attempt counter instead of
    /// leaving it untouched forever: once it reaches `MAX_PAIR_ATTEMPTS` the
    /// entry is removed too, so a LAN client can't keep guessing values from
    /// the 6-digit code space against the same pair_id. Below the cap the
    /// entry is left in place (with the bumped counter) so a human who
    /// mistyped the code still gets a few more real chances before the TTL
    /// runs out.
    fn complete_pair(&self, pair_id: &str, code: &str) -> bool {
        let mut pending = self
            .inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned");

        // Clone the current entry out (rather than holding a borrow into
        // `pending`) so the branches below are free to call
        // `pending.insert`/`pending.remove` without fighting the borrow
        // checker over a mutex we're already holding.
        let Some((stored_code, expiry, attempts)) = pending.get(pair_id).cloned() else {
            return false;
        };

        if Instant::now() >= expiry {
            pending.remove(pair_id);
            return false;
        }

        if stored_code == code {
            pending.remove(pair_id);
            return true;
        }

        let attempts = attempts + 1;
        if attempts >= MAX_PAIR_ATTEMPTS {
            pending.remove(pair_id);
        } else {
            pending.insert(pair_id.to_string(), (stored_code, expiry, attempts));
        }
        false
    }

    /// Add a newly-minted bearer token to the valid-token set. Task 10's
    /// control routes will read this set back to authenticate requests.
    ///
    /// If persistence is enabled (`token_store` is `Some`), the whole
    /// updated set is also written out to disk afterward -- see
    /// `persist_tokens` for the write-lock + fresh-snapshot discipline that
    /// keeps concurrent `/pair/complete` calls from writing stale snapshots
    /// out of order, and from ever doing file I/O while holding the
    /// `tokens` mutex.
    fn insert_token(&self, token: String) {
        self.inner
            .tokens
            .lock()
            .expect("tokens mutex poisoned")
            .insert(token);
        self.persist_tokens();
    }

    /// Write the CURRENT `tokens` set to `token_store`, if persistence is
    /// enabled; a no-op when it isn't (`AppState::new`'s `None` path).
    ///
    /// Deliberately takes no snapshot argument -- unlike an earlier version
    /// of this method, which took an already-cloned snapshot from the
    /// caller. That let two concurrent `/pair/complete` calls each capture
    /// their own snapshot *before* racing for the disk write, so whichever
    /// one's (possibly older/stale) snapshot happened to write LAST would
    /// silently clobber a newer one already on disk and lose a token that
    /// really was inserted. Instead, this method:
    ///
    ///   1. Acquires `write_lock` (distinct from `tokens`) to serialize
    ///      writes -- only one persist runs at a time.
    ///   2. THEN takes a fresh snapshot of `tokens` (briefly locking and
    ///      immediately dropping that lock -- file I/O never runs while
    ///      `tokens` is held, since `is_valid_token` locks it on every
    ///      authed request and must never block on disk).
    ///   3. Writes that fresh snapshot to disk.
    ///
    /// Because the snapshot is retaken after acquiring `write_lock` rather
    /// than reused from whenever the caller happened to insert, and
    /// `tokens` only ever grows, the write that actually lands last on disk
    /// is always a superset of every earlier one -- the last IN-MEMORY
    /// state wins on disk, not just the last write to *start*.
    ///
    /// A failed write is logged to stderr rather than propagated: the
    /// in-memory set (what actually gates auth for the rest of this
    /// process's life) was already updated by the time this is called, so a
    /// persistence failure shouldn't fail the pairing request that
    /// triggered it -- it just means a subsequent restart won't remember
    /// this token.
    fn persist_tokens(&self) {
        let Some(path) = &self.inner.token_store else {
            return;
        };

        let _write_guard = self
            .inner
            .write_lock
            .lock()
            .expect("token-store write lock poisoned");
        let snapshot = self
            .inner
            .tokens
            .lock()
            .expect("tokens mutex poisoned")
            .clone();

        if let Err(err) = save_tokens(path, &snapshot) {
            eprintln!(
                "tt-station-agentd: failed to persist token store at {}: {err:#}",
                path.display()
            );
        }
    }

    /// Clear ALL issued bearer tokens: empty the in-memory set AND, if
    /// persistence is enabled, delete the on-disk token store -- so a demo
    /// `POST /reset` returns the box to "never been paired" and every token
    /// any client is still holding stops working.
    ///
    /// Same lock discipline as `persist_tokens`: the in-memory clear happens
    /// under the `tokens` lock, which is then DROPPED before any disk I/O
    /// runs (file work is serialized under `write_lock` instead), because
    /// `is_valid_token` locks `tokens` on every authed request and must
    /// never block on the disk. Deleting the file (rather than writing an
    /// empty JSON array) matches a fresh box, which has no token store at all
    /// -- and `load_tokens` treats a missing file as "no tokens yet", so a
    /// later restart comes up empty either way.
    fn clear_tokens(&self) {
        {
            self.inner
                .tokens
                .lock()
                .expect("tokens mutex poisoned")
                .clear();
        }

        let Some(path) = &self.inner.token_store else {
            return;
        };

        let _write_guard = self
            .inner
            .write_lock
            .lock()
            .expect("token-store write lock poisoned");
        match fs::remove_file(path) {
            Ok(()) => {}
            // A missing store is already the desired end state -- not an error.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!(
                "tt-station-agentd: failed to remove token store at {} during reset: {err}",
                path.display()
            ),
        }
    }

    /// Test-only accessor: look up the code currently pending for
    /// `pair_id`, i.e. what a human would be reading off the box's screen
    /// right now.
    ///
    /// Compiled in only under `cfg(test)` (this crate's own unit tests) or
    /// the `test-hooks` feature (integration tests -- see the
    /// self-dependency in `Cargo.toml` for how that feature gets turned on
    /// for `cargo test`). A production build has neither, so a real agent
    /// never exposes pairing codes through any API but the log line
    /// `pair_init` prints -- exactly where a human reading the box's screen
    /// would look.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn last_code(&self, pair_id: &str) -> Option<String> {
        self.inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned")
            .get(pair_id)
            .map(|(code, _, _)| code.clone())
    }

    /// Test-only seam: insert a pending pair whose expiry is already in the
    /// past, so tests can exercise `complete_pair`'s TTL-expiry branch
    /// without actually sleeping for `PAIR_TTL` (120s).
    ///
    /// Gated the same way as `last_code` -- compiled in only for this
    /// crate's own unit tests or the `test-hooks` feature (integration
    /// tests), never for a normal build.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn insert_expired_pair(&self, pair_id: &str, code: &str) {
        let already_expired = Instant::now() - Duration::from_secs(1);
        self.inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned")
            .insert(pair_id.to_string(), (code.to_string(), already_expired, 0));
    }
}

/// Load a persisted bearer-token set from `path` (a JSON array of
/// strings). Never returns `Err` -- a missing file (the normal case for a
/// box that's never persisted a token before) and an unreadable/corrupt one
/// both resolve to an empty set, differing only in whether a warning gets
/// printed to stderr. Startup must never fail, and never panic, just
/// because the token store is absent or got hand-edited into garbage.
fn load_tokens(path: &Path) -> HashSet<String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return HashSet::new(),
        Err(err) => {
            eprintln!(
                "tt-station-agentd: failed to read token store at {}: {err} -- starting with an empty token set",
                path.display()
            );
            return HashSet::new();
        }
    };

    match serde_json::from_str::<Vec<String>>(&contents) {
        Ok(tokens) => tokens.into_iter().collect(),
        Err(err) => {
            eprintln!(
                "tt-station-agentd: token store at {} is not valid JSON ({err}) -- starting with an empty token set",
                path.display()
            );
            HashSet::new()
        }
    }
}

/// Persist `tokens` to `path` as a JSON array of strings.
///
/// Creates the parent directory if it doesn't exist yet (mode `0700` on
/// unix), then writes to a temp file in that same directory and renames it
/// into place -- so a concurrent reader (there shouldn't be one, but belt
/// and suspenders) never observes a partially-written file -- and sets the
/// final file to mode `0600` on unix. These are bearer secrets: anything
/// less than owner-only permissions on both the directory and the file
/// would let another local user on the box read them.
///
/// Sorts the tokens before serializing purely so the file's byte content is
/// deterministic given the same set (easier to eyeball/diff by hand); the
/// on-disk representation is otherwise just "the whole current set."
fn save_tokens(path: &Path, tokens: &HashSet<String>) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        // Only create (and chmod) the parent when it doesn't already exist.
        // The common case in practice is a pre-existing, already-shared
        // directory (`/tmp` in tests; a config dir a previous run already
        // created) -- unconditionally chmod-ing that out from under whatever
        // it currently is would both fight the box's own conventions and,
        // for something like `/tmp` that this process doesn't own, simply
        // fail with EPERM.
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            }
        }
    }

    let mut sorted: Vec<&String> = tokens.iter().collect();
    sorted.sort();
    let json = serde_json::to_string(&sorted)?;

    // Write-temp-then-rename rather than writing `path` directly: a rename
    // on the same filesystem is atomic, so any reader of `path` (in
    // practice, just this process's own next `load_tokens` call on a future
    // restart) always sees either the old complete contents or the new
    // complete contents, never a half-written file.
    let tmp_path = path.with_extension("tmp");

    let result = write_tmp_and_rename(&tmp_path, path, &json);
    if result.is_err() {
        // Best-effort cleanup: a failed write or rename shouldn't leave a
        // stale `.tmp` file lying around for the next `save_tokens` call
        // (or a curious operator) to trip over. Ignore the removal's own
        // result -- if the file's already gone, or removing it also fails,
        // that's not this function's problem to escalate; the original
        // error from `write_tmp_and_rename` is what matters and is returned
        // below regardless.
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// Create `tmp_path` fresh and write `json` to it, then atomically rename it
/// onto `path`.
///
/// On unix, `tmp_path` is created with `OpenOptions` specifying mode `0600`
/// from the moment the file is created (`create(true)` + `.mode(0o600)`)
/// rather than the previous `fs::write` (which creates the file with the
/// process's default umask-derived mode, e.g. `0644`) followed by a
/// separate `set_permissions` call -- that older sequence left a brief
/// window where the file existed on disk World/group-readable before the
/// chmod landed. Creating it restrictively from the start closes that
/// window entirely: the bearer tokens in this file are never written to a
/// less-than-owner-only-readable file, not even momentarily.
fn write_tmp_and_rename(tmp_path: &Path, path: &Path, json: &str) -> anyhow::Result<()> {
    use std::io::Write;

    // Remove any stale tmp first so we always create the file fresh. `.mode(0600)`
    // below only applies when OpenOptions *creates* the file; a leftover tmp from a
    // crash-between-write-and-rename would otherwise be reused/truncated while keeping
    // its old (possibly looser) permissions. Ignore NotFound.
    let _ = fs::remove_file(tmp_path);

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp_path)?
    };
    #[cfg(not(unix))]
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(tmp_path)?;

    file.write_all(json.as_bytes())?;
    drop(file);

    fs::rename(tmp_path, path)?;

    Ok(())
}

/// JSON body returned by `GET /status`.
#[derive(Serialize)]
struct StatusResponse {
    name: String,
    chips: String,
    /// TXT string form (`idle` / `serving:<model>`) -- the same
    /// representation used on the wire for mDNS, so agent and CLI never
    /// have to reconcile two different status encodings.
    status: String,
    /// This box's detected device-mesh label (`"p300x2"`, `"n300x4"`, ...),
    /// or `null` when detection failed/didn't run -- see
    /// `AppState::with_device_mesh`. Lets a client (Task 3's
    /// `tt --json status`) rank models by hardware fit without its own
    /// `tt-smi` access.
    device_mesh: Option<String>,
}

async fn get_status(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<StatusResponse> {
    // Reconcile against docker reality so a model stopped out of band (manual
    // `docker stop`, crash) isn't reported as still serving. Self-heals the
    // stored state as a side effect -- see `AppState::reconciled_status`.
    let status = state.reconciled_status().await;
    Json(StatusResponse {
        name: state.name().to_string(),
        chips: state.chips().to_string(),
        status: status.to_txt(),
        device_mesh: state.device_mesh().map(str::to_string),
    })
}

/// `GET /config` (UNAUTHED, like `GET /status`): the agent's redacted
/// serving-config summary -- active/available profiles plus the resolved
/// backend/serving-host/port/image/repo/device -- so the GTK panel, the `tt
/// config` CLI, and the Mac app can render "what am I actually about to
/// serve with" without pairing first. `ConfigSummary` carries no secrets by
/// construction (no `hf_token` field exists on it), so there's nothing for
/// this handler to redact -- it just serves the stored summary verbatim.
async fn get_config(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<ConfigSummary> {
    Json(state.config_summary())
}

/// `GET /models` (UNAUTHED, like `GET /status`): enumerate the models this
/// box's backend can serve (see `ServingBackend::list_models`), so a client
/// never has to guess or hardcode a model id before calling `/run`.
/// Unauthed for the same reason `/status` is -- it's read-only discovery
/// info, not a control action, and a client needs it to even know what to
/// pass to the (bearer-gated) `/run`.
///
/// `ServingBackend::list_models` is sync (like `start`/`stop`), so it's run
/// via `spawn_blocking` rather than called directly from this async
/// handler -- same rule the module doc in `serving/mod.rs` states for every
/// `ServingBackend` method, even though `RunPyBackend`'s implementation
/// (a single small file read) is fast in practice.
async fn get_models(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<ModelsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();

    let result = tokio::task::spawn_blocking(move || backend.list_models())
        .await
        .map_err(|join_err| {
            backend_error(anyhow::anyhow!("list_models task panicked: {join_err}"))
        })?;

    result.map(Json).map_err(backend_error)
}

/// JSON body returned by `POST /pair/init`.
#[derive(Serialize)]
struct PairInitResponse {
    pair_id: String,
}

/// `POST /pair/init` starts a pairing attempt: mint a `pair_id` and a
/// 6-digit code, print the code the way a box's screen would display it,
/// remember the pair for `PAIR_TTL`, and hand the `pair_id` back so the
/// client can present it (with the code the human read off the screen) to
/// `/pair/complete`.
async fn pair_init(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<PairInitResponse> {
    let pair_id = pairing::issue_pair_id();
    let code = pairing::issue_code();

    // Stand-in for the box's physical display: this is the one place a
    // human ever sees the code, so it must go somewhere they can read it --
    // stdout, which journald/systemd captures on a real box.
    println!("tt-station-agentd: pairing code: {code}");

    state.insert_pending_pair(pair_id.clone(), code);

    Json(PairInitResponse { pair_id })
}

/// JSON body accepted by `POST /pair/complete`.
#[derive(Deserialize)]
struct PairCompleteRequest {
    pair_id: String,
    code: String,
}

/// JSON body returned by `POST /pair/complete` on success.
#[derive(Serialize)]
struct PairCompleteResponse {
    token: String,
}

/// `POST /pair/complete` finishes a pairing attempt: if `code` matches the
/// still-unexpired code for `pair_id`, mint a bearer token, add it to the
/// valid-token set, and return it. Otherwise (unknown pair_id, expired, or
/// wrong code) respond `401 Unauthorized` without minting anything -- the
/// caller can't tell *why* it failed, which avoids leaking whether a given
/// pair_id ever existed.
async fn pair_complete(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(req): Json<PairCompleteRequest>,
) -> Result<Json<PairCompleteResponse>, StatusCode> {
    if !state.complete_pair(&req.pair_id, &req.code) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let token = pairing::issue_token();
    state.insert_token(token.clone());

    Ok(Json(PairCompleteResponse { token }))
}

/// Extractor guarding the control routes (`/run`, `/stop`, `/endpoint`):
/// requires `Authorization: Bearer <token>` where `<token>` is in the
/// valid-token set minted by `/pair/complete`. Missing header, a non-Bearer
/// scheme, or a token not in the set all reject identically with `401` --
/// deliberately indistinguishable, same reasoning as `pair_complete` not
/// saying *why* a pairing attempt failed.
///
/// Implemented as a real extractor (rather than an inline check duplicated
/// in each handler) so adding it to a route is just adding it to the
/// handler's argument list, and so it composes with axum's normal extractor
/// ordering instead of needing a separate middleware layer wired up per
/// route.
struct BearerAuth;

impl FromRequestParts<AppState> for BearerAuth {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, StatusCode> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));

        match token {
            Some(token) if state.is_valid_token(token) => Ok(BearerAuth),
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }
}

/// JSON body accepted by `POST /run`.
#[derive(Deserialize)]
struct RunRequest {
    model: String,
}

/// JSON body returned by `POST /run` on success.
#[derive(Serialize)]
struct RunResponse {
    endpoint: Endpoint,
}

/// JSON body returned when a control route fails after auth passes (backend
/// `start`/`stop` error, or a `spawn_blocking` join failure). Kept as a
/// simple `{ "error": "<message>" }` shape -- there's exactly one consumer
/// (Task 11's `AgentClient`) and it doesn't need anything richer than a
/// human-readable reason to log/surface.
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

fn backend_error(err: anyhow::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: err.to_string(),
        }),
    )
}

/// `POST /run { "model": "..." }` (bearer-guarded): ask the backend to start
/// serving `model`.
///
/// `backend.start` is sync and, for the real Docker backend, blocks on a
/// `reqwest::blocking` health probe -- calling it directly here would panic
/// (blocking calls are forbidden inside a Tokio worker thread). It's run via
/// `tokio::task::spawn_blocking` instead, on a cloned `Arc<dyn
/// ServingBackend>` handle so the closure doesn't need to borrow `state`
/// across the `.await`. On success, `status`/`endpoint` are updated only
/// *after* the blocking call returns -- no mutex guard is ever held across
/// the `.await`.
async fn run_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<RunRequest>,
) -> Result<Json<RunResponse>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();
    let model = req.model;

    let result = tokio::task::spawn_blocking(move || backend.start(&model))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("run task panicked: {join_err}")))?;

    let endpoint = result.map_err(backend_error)?;
    state.set_serving(endpoint.clone());

    Ok(Json(RunResponse { endpoint }))
}

/// `POST /stop` (bearer-guarded): ask the backend to stop serving.
///
/// This ALWAYS calls `backend.stop()`, even when `current_model()` is `None`.
/// A `/run` brings a model up on a `spawn_blocking` thread and `AppState`'s
/// status stays `Idle` until `start` returns -- so a `/stop` that arrives
/// mid-bring-up sees `None` here, yet must still reach the backend to trip its
/// cancel flag (`RunPyBackend::stop`) and abort the in-flight serve rather than
/// leaving it grinding to the health-poll ceiling. Every `ServingBackend` is
/// contractually idempotent here (see the doc on `ServingBackend::stop`) -- a
/// `docker stop`/equivalent on nothing running must be a no-op, not an error --
/// so calling `backend.stop` while genuinely idle is harmless. The same
/// `spawn_blocking` treatment as `/run` applies: the sync `backend.stop` call
/// must never run directly on the async runtime.
async fn stop_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    // Breadcrumb before we stop -- distinguishes stopping a live model from a
    // mid-bring-up abort (status is still `Idle` in the latter case).
    eprintln!(
        "tt-station-agentd: stop requested (was {})",
        match state.current_model() {
            Some(m) => format!("serving {m}"),
            None => "idle/starting".to_string(),
        }
    );

    // Model name is only used for logging by backends that track one;
    // `RunPyBackend::stop` ignores the argument entirely. When nothing is
    // serving there's no name to pass, so hand it an empty placeholder.
    let model = state.current_model().unwrap_or_default();
    let backend = state.backend();

    tokio::task::spawn_blocking(move || backend.stop(&model))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("stop task panicked: {join_err}")))?
        .map_err(backend_error)?;

    state.set_idle();
    Ok(Json(serde_json::json!({})))
}

/// `POST /reset` (bearer-guarded): return the box to a fresh-install state
/// for a demo. In order:
///
///   1. Ask the backend to reset (`ServingBackend::reset`) -- stop any
///      serving container and, on `RunPyBackend`, reset the board too. Run
///      via `spawn_blocking` since it shells out (`docker`, `tt-smi`), same
///      rule `/run` and `/stop` follow for the backend's sync methods.
///   2. Clear ALL issued bearer tokens (in-memory set + persisted store).
///   3. Best-effort revoke every `ttstation:<label>`-tagged SSH key the pair
///      flow ever installed (`authkeys::revoke_all_ttstation`) -- a reset
///      should also demo losing the keyless-SSH access pairing granted, not
///      just the bearer token. Non-fatal: an unwritable/missing SSH file
///      must never fail the route.
///   4. Flip `status` back to `Idle`, drop the stored `Endpoint`, and
///      re-advertise `Idle` (all via `set_idle`).
///
/// Clearing the tokens invalidates the caller's OWN bearer token -- that's
/// expected for a reset, and harmless here: auth was already checked at
/// entry (the `BearerAuth` extractor), so this handler still runs to
/// completion and returns `200 {}`.
async fn reset(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();

    // Backend reset shells out (docker/tt-smi) -- never on the async runtime.
    tokio::task::spawn_blocking(move || backend.reset())
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("reset task panicked: {join_err}")))?
        .map_err(backend_error)?;

    // Forget every issued token (invalidates the caller's own -- expected).
    state.clear_tokens();

    // Best-effort: also revoke every keyless-SSH key the pair flow ever
    // installed (see `authkeys::revoke_all_ttstation`), so a reset actually
    // demos losing SSH access, not just losing the bearer token. Never fail
    // the route over this -- a reset must still succeed even if the SSH
    // file is missing/unwritable (e.g. permissions, or the feature was
    // never used on this box).
    if let Err(err) = authkeys::revoke_all_ttstation(state.ssh_path()) {
        eprintln!("reset: failed to revoke ttstation SSH keys (non-fatal): {err}");
    }

    // Back to idle: status Idle, endpoint cleared, Idle re-advertised.
    state.set_idle();

    Ok(Json(serde_json::json!({})))
}

/// The success status for a power action: reset-chips completes synchronously
/// (200), while machine ops only *initiate* teardown before the box goes down
/// (202 Accepted).
fn power_success_status(action: crate::power::PowerAction) -> StatusCode {
    if action.is_machine_op() {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

/// JSON body accepted by `POST /power`.
#[derive(Deserialize)]
struct PowerRequest {
    action: String,
}

/// `POST /power { "action": "reset-chips" | "suspend" | "reboot" | "shutdown" }`
/// (bearer-guarded, same `BearerAuth` gate as `/run`/`/stop`/`/reset`): run
/// the configured command for `action` via `AppState::run_power_command`.
///
/// Status codes:
///   - `400` if `action` doesn't parse (`PowerAction::parse`) -- caller error,
///     checked before anything runs.
///   - `200 {}` on a successful `reset-chips` -- it completes synchronously
///     (board reset), so the caller can trust the response body.
///   - `202 { "action", "accepted": true }` on a successful machine op
///     (suspend/reboot/shutdown) -- the command only *initiates* teardown;
///     the box may go down before a `200` could ever be observed, so this
///     reports "accepted", not "done".
///   - `403` if the command fails with a permission/polkit-shaped error
///     (no rule installed for the run-user to invoke `systemctl`/`tt-smi`
///     without a password) -- the operator's box to fix, not this agent's,
///     so the message points at `docs/reference/power-controls.md`.
///   - `500` for any other command failure (e.g. the binary itself is
///     missing) -- `backend_error`, same as every other route's fallback.
async fn power(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PowerRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let action = crate::power::PowerAction::parse(&req.action).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("unknown power action: {}", req.action),
            }),
        )
    })?;

    let s = state.clone();
    tokio::task::spawn_blocking(move || s.run_power_command(action))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("power task panicked: {join_err}")))?
        .map_err(|e| {
            // A permission failure (no polkit rule) is the operator's to fix --
            // distinguish it from a generic 500 with a pointer to the doc.
            let msg = e.to_string();
            if msg.contains("Interactive authentication required")
                || msg.contains("Access denied")
                || msg.contains("not authorized")
            {
                (
                    StatusCode::FORBIDDEN,
                    Json(ErrorResponse {
                        error: format!(
                            "{msg} — the box is not permitted to {}. Install the polkit rule (see docs/reference/power-controls.md).",
                            req.action
                        ),
                    }),
                )
            } else {
                backend_error(e)
            }
        })?;

    let body = if action.is_machine_op() {
        serde_json::json!({ "action": req.action, "accepted": true })
    } else {
        serde_json::json!({})
    };
    Ok((power_success_status(action), Json(body)))
}

/// `GET /endpoint` (bearer-guarded): the `Endpoint` of whatever's currently
/// serving, or `409 Conflict` if the box is idle. `409` rather than `404`
/// because the route itself exists and is reachable -- what's missing is a
/// *resource* (a live endpoint), which is exactly what `409` communicates:
/// the request is well-formed but conflicts with the box's current state.
async fn get_endpoint(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Endpoint>, StatusCode> {
    // Reconcile first: if the agent's serve died out of band, this clears the
    // stored endpoint (via set_idle), so we return 409 (idle) instead of
    // handing back a dead base_url.
    let _ = state.reconciled_status().await;
    state.endpoint().map(Json).ok_or(StatusCode::CONFLICT)
}

/// `GET /telemetry` (UNAUTHED, like `GET /status` and `GET /models`):
/// upgrade to a WebSocket and stream `tt-smi -s` telemetry snapshots.
///
/// Unauthed for the same reason `/status`/`/models` are -- a telemetry stream
/// is exactly as read-only as they are (it mutates no box state), and the
/// remote-QuietBox design deliberately decided telemetry is unauthed (see
/// `REMOTE_QUIETBOX_DESIGN.md` §1: "the WebSocket upgrade should be unauthed
/// for v1, consistent with `/status`/`/models`"). Anyone on the LAN who can
/// reach the control port can already read `/status`; this extends that same
/// exposure rather than creating a new class of it.
///
/// The handshake itself does no I/O -- it just hands the upgraded socket to
/// [`telemetry_stream`], which owns the collect-and-push loop.
async fn telemetry_ws(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    // `?view=lite` opts a client into the trimmed dashboard-only frame -- no
    // process scan, no vLLM scrape (see `telemetry::lite_frame`). The macOS
    // app's device strip / temp chip request this. tt-toplike does the
    // OPPOSITE: it wants the full frame WITH the `tt_toplike` process/inference
    // enrichment (see `TT_TOPLIKE_STREAM.md`), so it connects to plain
    // `/telemetry` (no query) and must NOT pass `?view=lite`. A bare
    // string-split rather than a typed `Query<T>` extractor: this is one
    // boolean flag, not worth a serde struct. Any malformed/missing query just
    // means `lite == false`, i.e. today's full frame -- the safe default.
    let lite = query
        .as_deref()
        .map(|q| q.split('&').any(|kv| kv == "view=lite"))
        .unwrap_or(false);
    ws.on_upgrade(move |socket| telemetry_stream(socket, state, lite))
}

/// The per-connection telemetry loop behind `GET /telemetry`.
///
/// Every `telemetry_interval_ms`, produce a snapshot (the verbatim stdout of
/// `tt-smi -s`) and push it as a `Message::Text` frame. The loop is bounded by
/// the client: it exits the moment the socket closes or errors. A transient
/// `tt-smi` failure does NOT kill the connection -- it's logged and sent as a
/// small JSON error frame so the client learns this tick had no data but the
/// stream stays alive (`tt-smi` is known to flake under serving load).
///
/// `tt-smi` is a blocking subprocess, so it runs on `spawn_blocking` (via
/// [`collect_snapshot`]) rather than directly on the async runtime -- the same
/// off-the-runtime discipline every `ServingBackend` call in this crate
/// follows.
async fn telemetry_stream(mut socket: WebSocket, state: AppState, lite: bool) {
    // Clamp to >=1ms: `tokio::time::interval` panics on a zero duration, and this
    // runs in a per-connection task, so `--telemetry-interval-ms 0` would panic
    // every telemetry client (the CLI also rejects 0; this is belt-and-suspenders).
    // `Delay` missed-tick behavior so a slow `tt-smi` under serving load can't make
    // the ticker burst-fire back-to-back and flood the client.
    let mut ticker =
        tokio::time::interval(Duration::from_millis(state.telemetry_interval_ms().max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Owned once per connection (not per tick): `ProcessSampler` wraps a
    // `sysinfo::System`, and cpu% is only meaningful as a delta between two
    // refreshes, so it needs to persist across ticks for the life of this
    // stream. Skipped entirely in `lite` mode -- constructing (and never
    // sampling) it would just be wasted setup, and leaving it `None` means
    // there's no way for a future edit to accidentally call `.sample()` on
    // the lite path.
    let mut sampler = (!lite).then(crate::procscan::ProcessSampler::new);
    // Same "persist across ticks" reasoning as `sampler` above: vLLM's
    // counters are cumulative, so generation/prompt tps and the completed/
    // errored/preemption deltas can only be computed against the PREVIOUS
    // tick's scrape -- see `inference::InferenceSampler`. Also skipped in
    // `lite` mode.
    let mut inference_sampler = (!lite).then(crate::inference::InferenceSampler::new);

    loop {
        tokio::select! {
            // Time to push another snapshot. `interval`'s first tick fires
            // immediately, so a freshly-connected client gets a frame right
            // away rather than waiting a full interval.
            _ = ticker.tick() => {
                let frame = match state.cached_snapshot().await {
                    Ok(json) => {
                        if lite {
                            // `?view=lite`: skip the process scan and the
                            // vLLM scrape entirely -- just trim the raw
                            // `tt-smi` frame down to the dashboard fields.
                            // `sampler`/`inference_sampler` are `None` on
                            // this path (see construction above), so there's
                            // nothing here to call `.sample()`/`.tick()` on.
                            crate::telemetry::lite_frame(&json)
                        } else {
                            // Additive: fold the box's process list into the
                            // frame. The scan is a fast local /proc read
                            // (unlike tt-smi's shell-out), so it runs inline;
                            // a hiccup yields the frame verbatim, never an
                            // error. This only touches the success path --
                            // the error/skip frame below is untouched, so a
                            // `tt-smi` failure still yields exactly the small
                            // JSON error shape clients already know how to
                            // detect.
                            let mut toplike = sampler
                                .as_mut()
                                .expect("sampler is Some whenever !lite")
                                .sample();

                            // Additive: fold the box's inference workload in
                            // too. Scrape whatever port is actually serving
                            // (falling back to the agent's configured
                            // serving port when nothing is), off the async
                            // runtime -- see `scrape_vllm_metrics`.
                            // `state.status()` is read fresh each tick so a
                            // `/run`/`/stop` that lands mid-stream is
                            // reflected on the very next frame.
                            let status = state.status();
                            let port = resolve_metrics_port(&state);
                            let scrape_body = scrape_vllm_metrics(port).await;
                            toplike.inference = inference_sampler
                                .as_mut()
                                .expect("inference_sampler is Some whenever !lite")
                                .tick(&status, scrape_body.as_deref())
                                .map(|entry| vec![entry]);

                            crate::telemetry::enrich_frame(&json, Some(&toplike))
                        }
                    }
                    Err(err) => {
                        // Log and send an error frame rather than dropping the
                        // connection -- a transient `tt-smi` failure shouldn't
                        // end the stream.
                        eprintln!("tt-station-agentd: telemetry snapshot failed: {err:#}");
                        telemetry_error_frame(&err)
                    }
                };
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    // Client hung up between ticks -- nothing left to send to.
                    break;
                }
            }
            // Watch the inbound half so a client disconnect (or a Close frame,
            // or a socket error) ends the loop promptly instead of only being
            // noticed on the next failed send. We don't act on client
            // payloads -- this is a one-way telemetry push.
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Resolve the port `GET /telemetry` scrapes for `/metrics`: prefer the port
/// baked into the agent's own last-successful `/run` `Endpoint.base_url`
/// (e.g. `"http://host:8003/v1"` -> `8003`) -- the actual port a model is
/// being served on, which can differ from the agent's configured default
/// (e.g. a config profile override) -- falling back to the agent's own
/// configured serving port (`--serving-port`, `DEFAULT_SERVING_PORT`) when
/// nothing is currently serving (or, in the should-never-happen case, the
/// stored `base_url` doesn't parse).
fn resolve_metrics_port(state: &AppState) -> u16 {
    state
        .endpoint()
        .and_then(|e| port_from_base_url(&e.base_url))
        .unwrap_or_else(|| state.serving_port())
}

/// Parse the port out of a base URL shaped like `"http://host:8003/v1"`.
/// `None` on any shape that doesn't parse (no `://`, no port after the last
/// `:`, or a non-numeric port) -- callers treat that identically to "nothing
/// is serving," since a malformed `base_url` can't be scraped either way.
fn port_from_base_url(base_url: &str) -> Option<u16> {
    let after_scheme = base_url.split_once("://")?.1;
    let host_port = after_scheme.split('/').next()?;
    host_port.rsplit(':').next()?.parse().ok()
}

/// Blocking HTTP client for the `/metrics` scrape, with a SHORT (2s
/// connect + 2s total) timeout -- deliberately its own client rather than
/// reusing `serving::docker`'s `probe_client` (2s connect / 5s total),
/// because this one runs on every telemetry tick (as often as every 1s by
/// default): a hung vLLM process must not stall a tick for as long as
/// `probe_client`'s 5s bound, which is fine for the occasional `/serving` or
/// `/v1/models` call but too slow to run in this hot a loop.
fn metrics_client() -> reqwest::Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
}

/// Scrape `http://127.0.0.1:<port>/metrics` off the async runtime (a
/// blocking `reqwest` GET, same off-the-runtime discipline as
/// `collect_snapshot`'s `tt-smi` shell-out). Returns `None` on ANY failure --
/// connection refused (nothing listening / still starting), a non-200
/// status, a body read error, or a `spawn_blocking` join panic. Every one of
/// those collapses to the same "scrape failed" signal `InferenceSampler::tick`
/// consumes (see `inference::build_inference`'s phase table) -- there's no
/// caller here that needs to distinguish *why* the scrape failed.
async fn scrape_vllm_metrics(port: u16) -> Option<String> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let resp = metrics_client()?
            .get(format!("http://127.0.0.1:{port}/metrics"))
            .send()?;
        if !resp.status().is_success() {
            return Err(anyhow::anyhow!(
                "metrics scrape returned status {}",
                resp.status()
            ));
        }
        Ok(resp.text()?)
    })
    .await
    .ok()
    .and_then(Result::ok)
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

/// Build the small JSON error frame sent when a `tt-smi` snapshot fails, so a
/// client can distinguish "this tick had no data" from a real telemetry
/// payload without the stream dropping. Deliberately a distinct, tiny shape
/// (an object with a single `error` string) -- a real `tt-smi -s` snapshot is
/// a far larger object and never carries a top-level `error` key.
fn telemetry_error_frame(err: &anyhow::Error) -> String {
    serde_json::json!({ "error": err.to_string() }).to_string()
}

/// `GET /serving` (UNAUTHED, like `GET /status` and `GET /models`): list
/// EVERY live `tt-inference-server` `/v1` endpoint on the box, whoever
/// launched it (the agent's own `/run`, tt-studio's FastAPI, or a manual
/// `run.py`). Read-only discovery, so unauthed for the same reason
/// `/status`/`/models` are.
///
/// Additive to (not a replacement for) `/status`: `/status` still reports the
/// agent's own last serving intent, while `/serving` reflects docker reality.
/// A discovered endpoint is tagged `source: "agent"` when it's on the agent's
/// own configured serving port and the agent's in-memory status is serving
/// that model, else `"external"`.
///
/// The whole scan -- `docker ps` plus a `/v1/models` probe per candidate --
/// is blocking I/O, so it runs on `tokio::task::spawn_blocking` with a
/// `RealCommandRunner`, holding no mutex across it (the status snapshot is
/// taken up front). A `docker`-missing / no-containers / probe-failure box
/// yields `{"serving":[]}` -- never an error, never a panic. See
/// `serving::discovery::discover_serving`.
async fn get_serving(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<ServingList> {
    let serving_host = state.serving_host().to_string();
    let serving_port = state.serving_port();
    let status = state.status();

    let serving = tokio::task::spawn_blocking(move || {
        let runner = RealCommandRunner;
        discover_serving(&runner, &serving_host, serving_port, &status)
    })
    .await
    // A join failure (the blocking task panicked) is reported as "nothing
    // serving" rather than a 500 -- `/serving` is best-effort discovery and
    // must never fail a clean box.
    .unwrap_or_default();

    Json(ServingList { serving })
}

/// Body returned by `GET /logs`.
#[derive(Serialize)]
struct LogsResponse {
    source: String,
    /// Absolute path of the file being tailed, or `null` when nothing has been
    /// logged yet for this source.
    origin: Option<String>,
    lines: Vec<String>,
}

/// `GET /logs` (UNAUTHED, like `/status`/`/models`/`/serving`): tail the
/// newest tt-inference-server workflow log for `?source=container|run`.
///
/// Read-only file access, exactly as exposed as the other unauthed discovery
/// routes -- see `crate::logs` for the pure tail/redact logic this delegates
/// to. `?source` defaults to `container`; `?tail` defaults to
/// `crate::logs::DEFAULT_TAIL`, capped at `crate::logs::MAX_TAIL`.
///
/// Error contract: an unrecognized `source` is a `400` (caller's mistake); no
/// `tt_inference_repo` configured (non-runpy backend, e.g. dstack) is a `409`
/// (this box simply has no such logs); no log file written yet is NOT an
/// error -- it's a `200` with `lines: []` and `origin: null`, since "nothing
/// served yet" is the normal state of an idle box, not a failure.
async fn get_logs(
    axum::extract::State(state): axum::extract::State<AppState>,
    RawQuery(query): RawQuery,
) -> (StatusCode, Json<serde_json::Value>) {
    let (source_str, tail) = parse_logs_query(query.as_deref());
    let source = match crate::logs::LogSource::parse(&source_str) {
        Some(s) => s,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("unknown source '{source_str}'") })),
            )
        }
    };
    let repo = match state.tt_inference_repo() {
        Some(p) => p.to_path_buf(),
        None => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "logs unavailable: no tt-inference-server repo configured (non-runpy backend)"
                })),
            )
        }
    };

    let resp = tokio::task::spawn_blocking(move || {
        let dir = crate::logs::logs_dir(&repo, source);
        let file = crate::logs::newest_log_file(&dir)?;
        let (origin, lines) = match file {
            Some(path) => {
                let lines = crate::logs::tail_lines(&path, tail)?
                    .iter()
                    .map(|l| crate::logs::redact_line(l))
                    .collect();
                (Some(path.display().to_string()), lines)
            }
            None => (None, Vec::new()),
        };
        Ok::<_, std::io::Error>(LogsResponse {
            source: source_str,
            origin,
            lines,
        })
    })
    .await;

    match resp {
        Ok(Ok(r)) => (StatusCode::OK, Json(serde_json::to_value(r).unwrap())),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "failed to read logs" })),
        ),
    }
}

/// `GET /logs/stream` (UNAUTHED, like `GET /logs`/`GET /telemetry`): upgrade
/// to a WebSocket and follow the same source `GET /logs` tails, pushing new
/// lines as they're written instead of returning a single snapshot.
///
/// The handshake does no I/O itself -- it just parses `?source=`/`?tail=`
/// (via the same [`parse_logs_query`] `GET /logs` uses) and hands the
/// upgraded socket to [`logs_stream`], which owns the replay-then-follow
/// loop.
async fn logs_ws(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
    RawQuery(query): RawQuery,
) -> Response {
    let (source_str, tail) = parse_logs_query(query.as_deref());
    ws.on_upgrade(move |socket| logs_stream(socket, state, source_str, tail))
}

/// Follow-interval for the log tail poll. Short enough to feel live, long
/// enough to be cheap. (Mirrors telemetry's interval+Delay approach.)
const LOG_FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

/// The per-connection log-follow loop behind `GET /logs/stream`.
///
/// On connect: resolve the newest log file for `source`, replay its last
/// `tail` lines (through [`crate::logs::redact_line`], same as `GET /logs`),
/// then remember (path, end-of-file offset) and start following. Every
/// [`LOG_FOLLOW_INTERVAL`] tick, re-resolve the newest file -- a fresh serve
/// rotates to a new timestamped file, which this detects by the resolved
/// path changing and handles by restarting the replay from byte 0 of the new
/// file -- and otherwise reads whatever's been appended since `offset` via
/// [`crate::logs::read_new_lines`]. If the file vanishes mid-follow (e.g. log
/// rotation/cleanup outside this process), that's treated as "nothing to
/// follow right now," not an error: `cur_path` is cleared and the next tick
/// re-resolves rather than the connection dying.
///
/// An unknown `source` or a non-runpy backend (no `tt_inference_repo`
/// configured) sends a single `{"error": "..."}` text frame and returns --
/// same error shapes `GET /logs` uses, just delivered over the socket instead
/// of as an HTTP status since a WebSocket upgrade has already committed to
/// `101 Switching Protocols`.
async fn logs_stream(mut socket: WebSocket, state: AppState, source_str: String, tail: usize) {
    let source = match crate::logs::LogSource::parse(&source_str) {
        Some(s) => s,
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "error": format!("unknown source '{source_str}'") })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let repo = match state.tt_inference_repo() {
        Some(p) => p.to_path_buf(),
        None => {
            let _ = socket
                .send(Message::Text(
                    serde_json::json!({ "error": "logs unavailable: non-runpy backend" })
                        .to_string()
                        .into(),
                ))
                .await;
            return;
        }
    };
    let dir = crate::logs::logs_dir(&repo, source);

    // Resolve the current newest file + replay its tail, tracking (path, offset).
    let mut cur_path: Option<std::path::PathBuf> = None;
    let mut offset: u64 = 0;

    // Replay tail synchronously on connect.
    if let Ok(Some(path)) = crate::logs::newest_log_file(&dir) {
        if let Ok(lines) = crate::logs::tail_lines(&path, tail) {
            for l in lines {
                if socket
                    .send(Message::Text(crate::logs::redact_line(&l).into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        cur_path = Some(path);
    }

    let mut ticker = tokio::time::interval(LOG_FOLLOW_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // Re-resolve newest: a fresh serve creates a new timestamped file.
                let newest = crate::logs::newest_log_file(&dir).ok().flatten();
                if newest != cur_path {
                    // Rotated to a new file -- replay it from the start.
                    cur_path = newest.clone();
                    offset = 0;
                }
                if let Some(path) = &cur_path {
                    match crate::logs::read_new_lines(path, offset) {
                        Ok((lines, new_off)) => {
                            offset = new_off;
                            for l in lines {
                                if socket
                                    .send(Message::Text(crate::logs::redact_line(&l).into()))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(_) => { /* file vanished mid-follow; re-resolve next tick */ cur_path = None; }
                    }
                }
            }
            msg = socket.recv() => {
                match msg {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}

/// Parse `?source=<>&tail=<>` from a raw query string. Defaults: source
/// `"container"`, tail `DEFAULT_TAIL`, capped at `MAX_TAIL`. A bare
/// string-split rather than a typed `Query<T>` extractor -- same rationale as
/// `telemetry_ws`'s `?view=lite` parsing: two simple params, not worth a
/// serde struct, and any malformed/missing query just falls back to the safe
/// defaults rather than erroring.
fn parse_logs_query(query: Option<&str>) -> (String, usize) {
    let mut source = "container".to_string();
    let mut tail = crate::logs::DEFAULT_TAIL;
    if let Some(q) = query {
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("source=") {
                source = v.to_string();
            } else if let Some(v) = kv.strip_prefix("tail=") {
                if let Ok(n) = v.parse::<usize>() {
                    tail = n.min(crate::logs::MAX_TAIL);
                }
            }
        }
    }
    (source, tail)
}

/// Build the small `{ "error": "<message>" }` response for a `400 Bad
/// Request` -- a validation failure the CALLER can fix (a bad/private key,
/// a malformed label, a revoke request naming neither `label` nor
/// `public_key`), as opposed to `backend_error`'s `500`, which is for
/// failures on THIS box's side (a backend/I-O error after auth and
/// validation both passed).
fn bad_request(message: String) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse { error: message }),
    )
}

/// JSON body accepted by `POST /ssh/authorize`.
#[derive(Deserialize)]
struct SshAuthorizeRequest {
    public_key: String,
    label: String,
}

/// JSON body returned by `POST /ssh/authorize` on success.
#[derive(Serialize)]
struct SshAuthorizeResponse {
    authorized: bool,
    /// The account the newly-installed key can `ssh` in as -- the agent's
    /// RUN-USER (`ttuser` on QuietBox 2), not necessarily whoever the
    /// client happens to be paired as. Read straight off `AppState`
    /// (`with_ssh_target`) so it can never drift from where the key was
    /// actually written.
    ssh_user: String,
    /// Whether this exact key (by base64 blob, see `authkeys::key_blob`)
    /// was already present -- lets a client tell "freshly installed" apart
    /// from "already there, no-op" without needing a separate `GET`.
    already_present: bool,
}

/// `POST /ssh/authorize { "public_key": "...", "label": "..." }`
/// (bearer-guarded, same `BearerAuth` gate as `/run`/`/stop`/`/reset`):
/// install a paired client's SSH public key into the agent RUN-USER's
/// `authorized_keys` file (`state.ssh_path()`), so the client can `ssh` into
/// the box directly instead of only reaching it through this control API.
///
/// Two layers of validation before anything touches disk:
///   1. `BearerAuth` (an unauthenticated/mis-authenticated request is
///      rejected before this handler body even runs -- see the extractor's
///      doc comment).
///   2. `authkeys::validate_public_key`, called here explicitly so a bad key
///      (private-key material, multi-line input, an unrecognized key type)
///      gets a clear `400` -- `authkeys::authorize` re-validates internally
///      too (belt and suspenders, per its own doc comment), but by the time
///      its `anyhow::Error` reaches this handler it's already wrapped with
///      an "invalid public key: " prefix, so failing fast here keeps the
///      error message this handler returns simpler and keeps the intent
///      (client-input error, not a server error) explicit at the call site.
///
/// Any remaining failure from `authkeys::authorize` (most likely an invalid
/// `label` -- see `authkeys::validate_label` -- since a valid, already-
/// checked key rarely fails to write) is ALSO reported as `400`: both
/// failure modes are the caller's request being malformed, not this box
/// having a problem, so neither should read as a `500`.
async fn ssh_authorize(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<SshAuthorizeRequest>,
) -> Result<Json<SshAuthorizeResponse>, (StatusCode, Json<ErrorResponse>)> {
    authkeys::validate_public_key(&req.public_key).map_err(|err| bad_request(err.to_string()))?;

    let outcome = authkeys::authorize(state.ssh_path(), &req.public_key, &req.label)
        .map_err(|err| bad_request(err.to_string()))?;

    Ok(Json(SshAuthorizeResponse {
        authorized: true,
        ssh_user: state.ssh_user().to_string(),
        already_present: matches!(outcome, authkeys::AuthorizeOutcome::AlreadyPresent),
    }))
}

/// JSON body accepted by `DELETE /ssh/authorize`. Exactly one of `label`/
/// `public_key` is expected -- `label` takes priority if both are somehow
/// given (matches `authorize`'s own append-by-label convention).
#[derive(Deserialize)]
struct SshRevokeRequest {
    label: Option<String>,
    public_key: Option<String>,
}

/// JSON body returned by `DELETE /ssh/authorize` on success.
#[derive(Serialize)]
struct SshRevokeResponse {
    revoked: bool,
}

/// `DELETE /ssh/authorize { "label": "..." }` or `{ "public_key": "..." }`
/// (bearer-guarded, same gate as the `POST`): remove a previously-installed
/// key from `state.ssh_path()`, identified either by the `ttstation:<label>`
/// marker `authorize` tagged it with, or by the key blob itself.
///
/// Neither `label` nor `public_key` given is a `400` -- there's nothing to
/// revoke, and (unlike `authkeys::revoke` itself, which treats "nothing
/// matched" as success) that's the caller's request being malformed, not a
/// legitimate no-op. Matching but not FINDING anything, by contrast, is
/// still success (`revoked: true`) -- same idempotency `authkeys::revoke`
/// documents for its own no-match case.
async fn ssh_revoke(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<SshRevokeRequest>,
) -> Result<Json<SshRevokeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let which = if let Some(label) = req.label {
        authkeys::Revoke::Label(label)
    } else if let Some(public_key) = req.public_key {
        let blob = authkeys::key_blob(&public_key)
            .ok_or_else(|| bad_request("public_key missing key material".to_string()))?
            .to_string();
        authkeys::Revoke::Blob(blob)
    } else {
        return Err(bad_request(
            "request must include either \"label\" or \"public_key\"".to_string(),
        ));
    };

    authkeys::revoke(state.ssh_path(), &which).map_err(backend_error)?;

    Ok(Json(SshRevokeResponse { revoked: true }))
}

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/config", get(get_config))
        .route("/models", get(get_models))
        .route("/serving", get(get_serving))
        .route("/logs", get(get_logs))
        .route("/logs/stream", get(logs_ws))
        .route("/telemetry", get(telemetry_ws))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/reset", post(reset))
        .route("/power", post(power))
        .route("/endpoint", get(get_endpoint))
        .route("/ssh/authorize", post(ssh_authorize).delete(ssh_revoke))
        .with_state(state)
}

#[cfg(test)]
mod telemetry_inference_tests {
    use super::*;

    #[test]
    fn port_from_base_url_parses_the_common_shapes() {
        assert_eq!(port_from_base_url("http://host:8003/v1"), Some(8003));
        assert_eq!(port_from_base_url("http://127.0.0.1:8000/v1"), Some(8000));
        // No port at all.
        assert_eq!(port_from_base_url("http://host/v1"), None);
        // No scheme separator.
        assert_eq!(port_from_base_url("host:8003/v1"), None);
        // Non-numeric "port".
        assert_eq!(port_from_base_url("http://host:abc/v1"), None);
    }

    #[test]
    fn resolve_metrics_port_prefers_stored_endpoint_over_configured_default() {
        // No endpoint stored (idle box) -> falls back to the configured
        // serving port.
        let state = AppState::new(
            "qb2-lab".to_string(),
            "4xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_serving_config("127.0.0.1".to_string(), 9000);
        assert_eq!(resolve_metrics_port(&state), 9000);

        // Once something is (recorded as) serving, its endpoint's own port
        // wins over the agent's configured default.
        state.set_serving(Endpoint {
            base_url: "http://127.0.0.1:8003/v1".to_string(),
            model: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
            requires_key: false,
        });
        assert_eq!(resolve_metrics_port(&state), 8003);
    }

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
        let cmd = vec![script.to_string_lossy().into_owned()];

        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_power_config(cmd.clone(), cmd.clone(), cmd.clone(), cmd.clone());

        state
            .run_power_command(crate::power::PowerAction::Reboot)
            .expect("power command runs");
        assert!(marker.exists(), "configured power command was executed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn power_success_status_is_202_for_machine_ops_200_for_reset_chips() {
        use crate::power::PowerAction;
        assert_eq!(power_success_status(PowerAction::ResetChips), StatusCode::OK);
        assert_eq!(power_success_status(PowerAction::Suspend), StatusCode::ACCEPTED);
        assert_eq!(power_success_status(PowerAction::Reboot), StatusCode::ACCEPTED);
        assert_eq!(power_success_status(PowerAction::Shutdown), StatusCode::ACCEPTED);
    }
}

#[cfg(test)]
mod logs_query_tests {
    use super::*;

    #[test]
    fn parse_logs_query_defaults_to_container_and_default_tail() {
        assert_eq!(
            parse_logs_query(None),
            ("container".to_string(), crate::logs::DEFAULT_TAIL)
        );
        assert_eq!(
            parse_logs_query(Some("")),
            ("container".to_string(), crate::logs::DEFAULT_TAIL)
        );
    }

    #[test]
    fn parse_logs_query_reads_explicit_source_and_tail() {
        assert_eq!(
            parse_logs_query(Some("source=run&tail=50")),
            ("run".to_string(), 50)
        );
        // Order shouldn't matter.
        assert_eq!(
            parse_logs_query(Some("tail=50&source=run")),
            ("run".to_string(), 50)
        );
    }

    #[test]
    fn parse_logs_query_caps_tail_at_max() {
        let (_, tail) = parse_logs_query(Some("tail=999999"));
        assert_eq!(tail, crate::logs::MAX_TAIL);
    }

    #[test]
    fn parse_logs_query_ignores_unparseable_tail() {
        // A non-numeric tail falls back to the default rather than erroring.
        assert_eq!(
            parse_logs_query(Some("tail=notanumber")),
            ("container".to_string(), crate::logs::DEFAULT_TAIL)
        );
    }
}
