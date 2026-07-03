//! `mock-box`: a development fixture that pretends to be a Tenstorrent box.
//!
//! Two independent fixtures live here, picked by subcommand:
//!
//! - `advertise`: mDNS-only, reproduces just the `_tenstorrent._tcp`
//!   advertisement (Task 3) so `MdnsProvider` (Task 4) can be exercised
//!   without hardware.
//! - `serve` (Task 12): a small HTTP server that fakes the *whole* agent
//!   control API (`/status`, `/pair/*`, `/run`, `/stop`, `/endpoint`) plus a
//!   fake vLLM-style OpenAI endpoint (`/v1/chat/completions`, `/v1/models`),
//!   so the `tt` CLI's discover -> pair -> run -> endpoint -> completion flow
//!   can be end-to-end tested with no real agent or hardware involved.
//!
//! Both subcommands share `libttstation`'s `BoxRecord`/`Endpoint`/
//! `ServingStatus` types and TXT encoding so nothing here can drift from
//! what the real agent and CLI expect on the wire.

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use clap::{Parser, Subcommand};
use libttstation::model::{txt_encode, BoxRecord, Endpoint, ServingStatus};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// mDNS service type all Tenstorrent boxes (real and mocked) advertise under.
const SERVICE_TYPE: &str = "_tenstorrent._tcp.local.";

#[derive(Parser)]
#[command(
    name = "mock-box",
    about = "Pretend to be a Tenstorrent box for dev/test"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Advertise a fake Tenstorrent box via mDNS (_tenstorrent._tcp) until Ctrl-C.
    Advertise {
        /// Box name; used as both the mDNS instance name and the `name` TXT key.
        #[arg(long)]
        name: String,

        /// Control-plane port advertised in the `ctrl` TXT key.
        #[arg(long = "ctrl-port")]
        ctrl_port: u16,

        /// Chip inventory string advertised in the `chips` TXT key.
        #[arg(long, default_value = "4xBH")]
        chips: String,

        /// API version advertised in the `apiver` TXT key.
        #[arg(long, default_value_t = 1)]
        apiver: u8,
    },

    /// Serve a fake agent control API + fake vLLM endpoint over HTTP, so the
    /// `tt` CLI can be driven end-to-end with no real agent/hardware.
    Serve {
        /// Control-plane HTTP port to listen on. The fake vLLM endpoint
        /// `/run` hands back also lives on this same port, under `/v1`.
        #[arg(long = "ctrl-port")]
        ctrl_port: u16,

        /// Box name returned from `/status`.
        #[arg(long, default_value = "mock-box")]
        name: String,

        /// Chip inventory string returned from `/status`.
        #[arg(long, default_value = "4xBH")]
        chips: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Advertise {
            name,
            ctrl_port,
            chips,
            apiver,
        } => advertise(name, ctrl_port, chips, apiver).await,
        Command::Serve {
            ctrl_port,
            name,
            chips,
        } => serve(ctrl_port, name, chips).await,
    }
}

/// Build a [`BoxRecord`] from CLI flags, encode it into mDNS TXT records via
/// `libttstation`'s `txt_encode` (so the keys stay byte-for-byte compatible
/// with what the real DiscoveryProvider decoder expects), and register it
/// with the local mDNS responder. Runs until Ctrl-C, then unregisters
/// cleanly so peers see the service disappear.
async fn advertise(name: String, ctrl_port: u16, chips: String, apiver: u8) -> Result<()> {
    let host = format!("{name}.local.");
    let record = BoxRecord {
        name: name.clone(),
        host: host.clone(),
        ctrl_port,
        chips,
        status: ServingStatus::Idle,
        apiver,
    };

    // Reuse the shared encoder so the advertised TXT keys (name, apiver,
    // chips, status, ctrl) exactly match what Task 4's decoder expects.
    let txt_pairs = txt_encode(&record);
    let txt_refs: Vec<(&str, &str)> = txt_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mdns = ServiceDaemon::new().context("failed to start mDNS daemon")?;

    // Passing an empty address list + enable_addr_auto() lets mdns-sd figure
    // out this host's real LAN address(es) instead of us hardcoding one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &record.name,
        &host,
        "",
        ctrl_port,
        &txt_refs[..],
    )
    .context("failed to build mDNS ServiceInfo")?
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    mdns.register(service_info)
        .context("failed to register mDNS service")?;

    println!(
        "mock-box: advertising '{}' as {} (service type {}) on port {}",
        record.name, fullname, SERVICE_TYPE, ctrl_port
    );
    println!("mock-box: TXT records: {:?}", txt_pairs);
    println!("mock-box: press Ctrl-C to stop advertising");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;

    println!("mock-box: Ctrl-C received, unregistering and shutting down");
    if let Ok(receiver) = mdns.unregister(&fullname) {
        // Best-effort wait for the unregister to be flushed so any browsers
        // on the LAN get a proper goodbye packet before we exit.
        let _ = receiver.recv();
    }
    let _ = mdns.shutdown();

    Ok(())
}

// ---------------------------------------------------------------------
// `serve`: fake agent control API + fake vLLM endpoint.
// ---------------------------------------------------------------------

/// Shared mutable state for the fake agent: what it's called, its chip
/// inventory, and whatever it's currently "serving" (if anything).
///
/// No auth, no pairing-code validation, no per-pair-id bookkeeping: this is
/// a mock built to let the `tt` CLI's happy path run end-to-end without
/// hardware, not a faithful reimplementation of `tt-station-agentd`'s
/// security model. See `pair_init`/`pair_complete` below for exactly what's
/// simplified away.
struct Inner {
    name: String,
    chips: String,
    ctrl_port: u16,
    status: ServingStatus,
    endpoint: Option<Endpoint>,
}

#[derive(Clone)]
struct MockState(Arc<Mutex<Inner>>);

impl MockState {
    fn new(name: String, chips: String, ctrl_port: u16) -> Self {
        MockState(Arc::new(Mutex::new(Inner {
            name,
            chips,
            ctrl_port,
            status: ServingStatus::Idle,
            endpoint: None,
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().expect("mock-box state mutex poisoned")
    }

    /// The model currently "serving", or a placeholder if idle -- used by
    /// the fake vLLM routes (`/v1/chat/completions`, `/v1/models`), which a
    /// real client would only call after `/run`, but which shouldn't panic
    /// if hit while idle either.
    fn current_model(&self) -> String {
        match &self.lock().status {
            ServingStatus::Serving(model) => model.clone(),
            ServingStatus::Idle => "mock-model".to_string(),
        }
    }
}

#[derive(Serialize)]
struct StatusResponse {
    name: String,
    chips: String,
    status: String,
}

async fn get_status(State(state): State<MockState>) -> Json<StatusResponse> {
    let inner = state.lock();
    Json(StatusResponse {
        name: inner.name.clone(),
        chips: inner.chips.clone(),
        status: inner.status.to_txt(),
    })
}

#[derive(Serialize)]
struct PairInitResponse {
    pair_id: String,
}

/// `POST /pair/init`: mint a fixed `pair_id` and print a code the way a
/// box's screen would. The pair_id is fixed (not tracked/validated) because
/// `pair/complete` below accepts any code for any pair_id -- there's nothing
/// to correlate.
async fn pair_init() -> Json<PairInitResponse> {
    let code: u32 = rand::rng().random_range(0..1_000_000);
    println!("mock-box: pairing code: {code:06}");
    Json(PairInitResponse {
        pair_id: "mock-pair".to_string(),
    })
}

#[derive(Deserialize)]
#[allow(dead_code)] // fields accepted for shape-compatibility with the real agent; unused (see module doc)
struct PairCompleteRequest {
    pair_id: String,
    code: String,
}

#[derive(Serialize)]
struct PairCompleteResponse {
    token: String,
}

/// `POST /pair/complete`: accepts ANY `pair_id`/`code` and returns a fixed
/// token. Real pairing (Task 7) validates both against what `/pair/init`
/// minted; the mock skips that entirely so the e2e test doesn't need to
/// scrape a printed code out of a child process's stdout.
async fn pair_complete(Json(_req): Json<PairCompleteRequest>) -> Json<PairCompleteResponse> {
    Json(PairCompleteResponse {
        token: "mock-token".to_string(),
    })
}

#[derive(Deserialize)]
struct RunRequest {
    model: String,
}

#[derive(Serialize)]
struct RunResponse {
    endpoint: Endpoint,
}

/// `POST /run { "model": "..." }`: "start" serving `model` by pointing the
/// endpoint at this same server's own `/v1` routes, so a client that then
/// POSTs to `{base_url}/chat/completions` hits the canned response below --
/// no separate fake vLLM process needed.
async fn run_model(
    State(state): State<MockState>,
    Json(req): Json<RunRequest>,
) -> Json<RunResponse> {
    let mut inner = state.lock();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{}/v1", inner.ctrl_port),
        model: req.model.clone(),
        requires_key: false,
    };
    inner.status = ServingStatus::Serving(req.model);
    inner.endpoint = Some(endpoint.clone());
    Json(RunResponse { endpoint })
}

/// `POST /stop`: go back to idle.
async fn stop_model(State(state): State<MockState>) -> Json<serde_json::Value> {
    let mut inner = state.lock();
    inner.status = ServingStatus::Idle;
    inner.endpoint = None;
    Json(serde_json::json!({}))
}

/// `GET /endpoint`: the current `Endpoint`, or `409` if idle -- same
/// contract as the real agent's `GET /endpoint` (Task 10), so `AgentClient`
/// (Task 11) can be pointed at either without special-casing the mock.
async fn get_endpoint(State(state): State<MockState>) -> Result<Json<Endpoint>, StatusCode> {
    state
        .lock()
        .endpoint
        .clone()
        .map(Json)
        .ok_or(StatusCode::CONFLICT)
}

/// `POST /v1/chat/completions`: a canned OpenAI-style chat completion. The
/// request body is intentionally not validated or even inspected -- the
/// point of this mock is proving `tt`'s discover -> pair -> run -> endpoint
/// -> completion plumbing works, not exercising the completion API surface.
async fn chat_completions(
    State(state): State<MockState>,
    Json(_body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "id": "mock-cmpl-1",
        "object": "chat.completion",
        "model": state.current_model(),
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello from mock-box" },
            "finish_reason": "stop",
        }],
    }))
}

/// `GET /v1/models`: minimal OpenAI-style model list.
async fn list_models(State(state): State<MockState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "data": [{ "id": state.current_model() }],
    }))
}

/// Build the router for a given `MockState`. Side-effect-free (no socket
/// binding) so it mirrors `tt-station-agentd::routes::app` and could be unit
/// tested the same way if this mock ever grows its own tests.
fn app(state: MockState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/endpoint", get(get_endpoint))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .with_state(state)
}

/// Bind `ctrl_port` and serve the fake control API + fake vLLM endpoint
/// until the process is killed. No graceful-shutdown handling (unlike
/// `tt-station-agentd`) -- this is a disposable test fixture, normally
/// killed outright by whatever spawned it (see `crates/tt/tests/e2e_mock.rs`).
async fn serve(ctrl_port: u16, name: String, chips: String) -> Result<()> {
    let state = MockState::new(name.clone(), chips, ctrl_port);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", ctrl_port))
        .await
        .with_context(|| format!("failed to bind control port {ctrl_port}"))?;

    println!("mock-box: serving fake agent '{name}' on port {ctrl_port} (Ctrl-C to stop)");

    axum::serve(listener, app(state))
        .await
        .context("mock-box HTTP server failed")?;

    Ok(())
}
