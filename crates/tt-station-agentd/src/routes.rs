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
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    routing::{get, post},
    Json, Router,
};
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};
use serde::{Deserialize, Serialize};

use crate::pairing;
use crate::serving::ServingBackend;

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
}

impl AppState {
    /// Construct fresh state for a box that starts out idle, wired to
    /// `backend` for actually starting/stopping model serving. Callers
    /// (`main.rs`, and this crate's tests) build `backend` via
    /// `serving::make_backend` or a test double and hand it in already
    /// wrapped in an `Arc`, since `AppState` never needs to construct a
    /// backend itself.
    pub fn new(name: String, chips: String, backend: Arc<dyn ServingBackend>) -> Self {
        AppState {
            inner: Arc::new(Inner {
                name,
                chips,
                status: Mutex::new(ServingStatus::Idle),
                endpoint: Mutex::new(None),
                backend,
                pending_pairs: Mutex::new(HashMap::new()),
                tokens: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn chips(&self) -> &str {
        &self.inner.chips
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
    fn set_serving(&self, endpoint: Endpoint) {
        let mut status = self.inner.status.lock().expect("status mutex poisoned");
        let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
        *status = ServingStatus::Serving(endpoint.model.clone());
        *stored_endpoint = Some(endpoint);
    }

    /// Record a successful `/stop` (or a no-op `/stop` while already idle):
    /// `status` goes back to `Idle` and any stored `Endpoint` is cleared.
    fn set_idle(&self) {
        let mut status = self.inner.status.lock().expect("status mutex poisoned");
        let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
        *status = ServingStatus::Idle;
        *stored_endpoint = None;
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
    fn insert_token(&self, token: String) {
        self.inner
            .tokens
            .lock()
            .expect("tokens mutex poisoned")
            .insert(token);
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

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/models", get(get_models))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/endpoint", get(get_endpoint))
        .with_state(state)
}
