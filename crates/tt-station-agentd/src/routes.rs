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
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use libttstation::model::ServingStatus;
use serde::{Deserialize, Serialize};

use crate::pairing;

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
    /// routes will need to flip it between `Idle` and `Serving(model)`.
    status: Mutex<ServingStatus>,
    /// Which serving backend was requested on the command line
    /// (`"docker"` or `"dstack"`). Task 9 turns this into a real
    /// `ServingBackend` trait object; for now we just remember the choice.
    backend: String,
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
    /// Construct fresh state for a box that starts out idle.
    pub fn new(name: String, chips: String, backend: String) -> Self {
        AppState {
            inner: Arc::new(Inner {
                name,
                chips,
                status: Mutex::new(ServingStatus::Idle),
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

    pub fn backend(&self) -> &str {
        &self.inner.backend
    }

    /// Snapshot the current serving status (locks briefly, then clones out).
    pub fn status(&self) -> ServingStatus {
        self.inner
            .status
            .lock()
            .expect("status mutex poisoned")
            .clone()
    }

    /// Record a freshly-issued pairing attempt: `pair_id` will be accepted
    /// by `complete_pair` if presented with the matching `code` before
    /// `PAIR_TTL` elapses.
    fn insert_pending_pair(&self, pair_id: String, code: String) {
        let expiry = Instant::now() + PAIR_TTL;
        self.inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned")
            .insert(pair_id, (code, expiry, 0));
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

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .with_state(state)
}
