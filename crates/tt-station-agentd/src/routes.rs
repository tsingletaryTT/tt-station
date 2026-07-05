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
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use libttstation::model::{ConfigSummary, Endpoint, ModelsResponse, ServingList, ServingStatus};
use serde::{Deserialize, Serialize};

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
                serving_host: DEFAULT_SERVING_HOST.to_string(),
                serving_port: DEFAULT_SERVING_PORT,
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

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn chips(&self) -> &str {
        &self.inner.chips
    }

    /// `tt-smi` binary the `/telemetry` stream runs (see `with_telemetry_config`).
    fn tt_smi_bin(&self) -> &str {
        &self.inner.tt_smi_bin
    }

    /// Interval (ms) between `/telemetry` snapshots (see `with_telemetry_config`).
    fn telemetry_interval_ms(&self) -> u64 {
        self.inner.telemetry_interval_ms
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
}

async fn get_status(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<StatusResponse> {
    Json(StatusResponse {
        name: state.name().to_string(),
        chips: state.chips().to_string(),
        status: state.status().to_txt(),
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

/// `POST /stop` (bearer-guarded): ask the backend to stop whatever model is
/// currently serving.
///
/// If nothing is serving (`current_model()` is `None`), this is a no-op
/// success -- there's no model name to hand the backend, and "stop" on an
/// already-idle box isn't an error (same idempotency `DockerBackend::stop`
/// itself documents for `docker stop` on a missing container). Otherwise the
/// same `spawn_blocking` treatment as `/run` applies: the sync
/// `backend.stop` call must never run directly on the async runtime.
async fn stop_model(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(model) = state.current_model() {
        let backend = state.backend();

        tokio::task::spawn_blocking(move || backend.stop(&model))
            .await
            .map_err(|join_err| backend_error(anyhow::anyhow!("stop task panicked: {join_err}")))?
            .map_err(backend_error)?;
    }

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
///   3. Flip `status` back to `Idle`, drop the stored `Endpoint`, and
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

    // Back to idle: status Idle, endpoint cleared, Idle re-advertised.
    state.set_idle();

    Ok(Json(serde_json::json!({})))
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
) -> Response {
    ws.on_upgrade(move |socket| telemetry_stream(socket, state))
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
async fn telemetry_stream(mut socket: WebSocket, state: AppState) {
    let tt_smi_bin = state.tt_smi_bin().to_string();
    // Clamp to >=1ms: `tokio::time::interval` panics on a zero duration, and this
    // runs in a per-connection task, so `--telemetry-interval-ms 0` would panic
    // every telemetry client (the CLI also rejects 0; this is belt-and-suspenders).
    // `Delay` missed-tick behavior so a slow `tt-smi` under serving load can't make
    // the ticker burst-fire back-to-back and flood the client.
    let mut ticker =
        tokio::time::interval(Duration::from_millis(state.telemetry_interval_ms().max(1)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // Time to push another snapshot. `interval`'s first tick fires
            // immediately, so a freshly-connected client gets a frame right
            // away rather than waiting a full interval.
            _ = ticker.tick() => {
                let frame = match collect_snapshot(tt_smi_bin.clone()).await {
                    Ok(json) => json,
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

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/config", get(get_config))
        .route("/models", get(get_models))
        .route("/serving", get(get_serving))
        .route("/telemetry", get(telemetry_ws))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/reset", post(reset))
        .route("/endpoint", get(get_endpoint))
        .with_state(state)
}
