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

use std::sync::{Arc, Mutex};

use axum::{routing::get, Json, Router};
use libttstation::model::ServingStatus;
use serde::Serialize;

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

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .with_state(state)
}
