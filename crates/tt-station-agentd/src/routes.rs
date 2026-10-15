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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::RawQuery,
    http::StatusCode,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::authkeys;

pub use crate::auth::*;
pub(crate) use crate::error::*;
pub(crate) use crate::handlers::devices::*;
pub(crate) use crate::handlers::pairing::*;
pub(crate) use crate::handlers::serving::*;
pub(crate) use crate::handlers::status::*;
pub use crate::state::*;

/// `POST /reset` (bearer-guarded): return the box to a fresh-install state
/// for a demo. In order:
///
///   1. Ask the backend to reset (`ServingBackend::reset`) -- stop any
///      serving container and, on `RunPyBackend`, reset the board too. Run
///      via `spawn_blocking` since it shells out (`docker`, `tt-smi`), same
///      rule `/run` and `/stop` follow for the backend's sync methods.
///   2. Sign out EVERY paired device (in-memory sessions + persisted store).
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
///
/// The body is OPTIONAL: `{"force": true}` is `tt-station reset --force`, which
/// overrides the foreign-lease refusal (see `RunPyBackend::reset_forcing`).
/// Every existing client posts `/reset` with no body and no content-type at
/// all, which `Option<Json<..>>` reads as `None` -> unforced, so the polite
/// default is what an older `tt-station` still gets. (A request that DOES declare
/// `application/json` and then carries an unparseable body is a `400` rather
/// than a silent unforced reset -- guessing which one a garbled `{"force":
/// ...}` meant is exactly the wrong thing to do on a destructive route, and
/// failing there resets nothing.)
async fn reset(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    body: Option<Json<ResetRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let backend = state.backend();
    let force = body.map(|Json(req)| req.force).unwrap_or(false);

    // Backend reset shells out (docker/tt-smi) -- never on the async runtime.
    // `reset`/`reset_forced` rather than one call with a bool, for the same
    // reason `/run` splits: the unforced path is byte-for-byte the call it
    // always was.
    tokio::task::spawn_blocking(move || {
        if force {
            backend.reset_forced()
        } else {
            backend.reset()
        }
    })
    .await
    .map_err(|join_err| backend_error(anyhow::anyhow!("reset task panicked: {join_err}")))?
    // A reset REFUSED because another tenant holds chips is a 409, and
    // (note the `?`) it aborts the route before the token clearing
    // below: a refusal must leave the box, and the pairing, untouched.
    .map_err(contention_aware_error)?;

    // Sign out every paired device (the caller's own too -- expected).
    crate::audit::record(&state, "reset", if force { "forced" } else { "" }, "ok");
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
    /// `tt-station power reset-chips --force`: run the whole-box board reset even
    /// though another tenant holds chips (or gozer cannot be asked). Ignored
    /// for suspend/reboot/shutdown, which have no lease check to override.
    /// `#[serde(default)]` keeps every pre-`--force` client working.
    #[serde(default)]
    force: bool,
}

/// JSON body OPTIONALLY accepted by `POST /reset` -- see the `reset` handler.
#[derive(Deserialize)]
struct ResetRequest {
    /// `tt-station reset --force`: run the whole-box `tt-smi -r` even though another
    /// tenant holds chips (or gozer cannot be asked).
    #[serde(default)]
    force: bool,
}

/// `POST /power { "action": "reset-chips" | "suspend" | "reboot" | "shutdown",
/// "force": <bool> }` (bearer-guarded, same `BearerAuth` gate as
/// `/run`/`/stop`/`/reset`): run the configured command for `action` via
/// `AppState::run_power_command`. `force` (default `false`) overrides
/// `reset-chips`'s foreign-lease refusal and is ignored for the machine ops.
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
    _owner: OwnerAuth,
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

    if let Some(holder) = state.chips_holder() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: holder.to_string(),
            }),
        ));
    }
    if action.is_machine_op() {
        let (what, body) = match action {
            crate::power::PowerAction::Suspend => (
                "going to sleep",
                "Wake it from TT Station when you need it.",
            ),
            crate::power::PowerAction::Reboot => {
                ("restarting", "You'll hear when it's back online.")
            }
            _ => ("shutting down", "Turn it back on to use it again."),
        };
        state
            .notifier()
            .emit_and_wait(crate::notify::Draft {
                kind: "power",
                title: format!("{} is {what}", state.name()),
                body: body.to_string(),
            })
            .await;
    }
    let s = state.clone();
    let force = req.force;
    let ran = tokio::task::spawn_blocking(move || s.run_power_command(action, force))
        .await
        .map_err(|join_err| backend_error(anyhow::anyhow!("power task panicked: {join_err}")))?;
    crate::audit::record(
        &state,
        "power",
        &req.action,
        if ran.is_ok() { "ok" } else { "failed" },
    );
    ran.map_err(|e| {
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
                // `reset-chips` refused because another tenant holds chips
                // is a 409, not a 500 -- see `contention_aware_error`.
                contention_aware_error(e)
            }
        })?;

    let body = if action.is_machine_op() {
        serde_json::json!({ "action": req.action, "accepted": true })
    } else {
        serde_json::json!({})
    };
    Ok((power_success_status(action), Json(body)))
}

/// `GET /power/info` (authed): what the phone can do with the machine's power
/// and the one-time setup for the rest. See `crate::power::readiness_blocking`.
async fn power_info(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<crate::power::PowerReadiness> {
    let config_dir = state.config_dir();
    let user = std::env::var("USER").unwrap_or_else(|_| "ttuser".to_string());
    Json(
        tokio::task::spawn_blocking(move || crate::power::readiness_blocking(&user, &config_dir))
            .await
            .unwrap_or_else(|_| crate::power::readiness_blocking("ttuser", &std::env::temp_dir())),
    )
}

/// `GET /telemetry` (the box itself or a paired device, like `/logs`):
/// upgrade to a WebSocket and stream `tt-smi -s` telemetry snapshots.
///
/// It was unauthed once, like `/status`; but the full frame lists the box's
/// processes, and a process's command line can carry paths, prompts and
/// secrets passed as arguments. So: paired callers only, at most
/// [`MAX_TELEMETRY_SOCKETS`] at a time, and for anyone but an owner each
/// process's `cmd` is cut to its executable's name
/// (`crate::procscan::redact_cmd`). Removing the device closes the stream.
///
/// The handshake itself does no I/O -- it just hands the upgraded socket to
/// [`telemetry_stream`], which owns the collect-and-push loop.
/// `GET /energy`: kWh, tokens and requests per month and for the last 31
/// days, from the recorded history (`history::energy_report`).
async fn get_energy(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<serde_json::Value> {
    Json(crate::history::energy_report(&state.history().days()))
}

async fn telemetry_ws(
    ws: WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
    RawQuery(query): RawQuery,
) -> Response {
    use axum::response::IntoResponse;
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
    let Some(slot) = TelemetrySlot::take() else {
        return tt_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many live telemetry views are open on this box. Close one and try again.",
        )
        .into_response();
    };
    let owner = caller.owner();
    let signed_out = state.device_signed_out(caller.device_id());
    // The desktop app's token rides as a subprotocol: answer with ours.
    let ws = ws.protocols([WS_PROTOCOL]);
    ws.on_upgrade(move |socket| async move {
        telemetry_stream(socket, state, lite, owner, signed_out).await;
        drop(slot);
    })
}

/// Live `/telemetry` sockets, and how many may be open at once: each one runs
/// its own process scan every interval.
static TELEMETRY_SOCKETS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub const MAX_TELEMETRY_SOCKETS: usize = 16;

/// One of the [`MAX_TELEMETRY_SOCKETS`], held for a socket's life (and
/// given back however it ends, a failed upgrade included).
struct TelemetrySlot;

impl TelemetrySlot {
    fn take() -> Option<Self> {
        use std::sync::atomic::Ordering::SeqCst;
        TELEMETRY_SOCKETS
            .fetch_update(SeqCst, SeqCst, |n| {
                (n < MAX_TELEMETRY_SOCKETS).then_some(n + 1)
            })
            .ok()
            .map(|_| TelemetrySlot)
    }
}

impl Drop for TelemetrySlot {
    fn drop(&mut self) {
        TELEMETRY_SOCKETS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
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
async fn telemetry_stream(
    mut socket: WebSocket,
    state: AppState,
    lite: bool,
    owner: bool,
    signed_out: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(signed_out);
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
    // Log a failure once per reason, not once a second for hours (an Orchard
    // run pauses readings for its whole length).
    let mut last_error: Option<String> = None;

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

                            if !owner {
                                for p in &mut toplike.processes {
                                    p.cmd = crate::procscan::redact_cmd(&p.cmd);
                                }
                            }
                            crate::telemetry::enrich_frame(&json, Some(&toplike))
                        }
                    }
                    Err(err) => {
                        // Log and send an error frame rather than dropping the
                        // connection -- a transient `tt-smi` failure shouldn't
                        // end the stream.
                        let text = format!("{err:#}");
                        if last_error.as_deref() != Some(text.as_str()) {
                            eprintln!("tt-station-agentd: telemetry snapshot failed: {text}");
                            last_error = Some(text);
                        }
                        telemetry_error_frame(&err)
                    }
                };
                if socket.send(Message::Text(frame.into())).await.is_err() {
                    // Client hung up between ticks -- nothing left to send to.
                    break;
                }
            }
            // Its device was removed (or the box reset): close.
            () = &mut signed_out => {
                let _ = socket.send(Message::Close(Some(signed_out_frame()))).await;
                break;
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

/// Build the small JSON error frame sent when a `tt-smi` snapshot fails, so a
/// client can distinguish "this tick had no data" from a real telemetry
/// payload without the stream dropping. Deliberately a distinct, tiny shape
/// (an object with a single `error` string) -- a real `tt-smi -s` snapshot is
/// a far larger object and never carries a top-level `error` key.
fn telemetry_error_frame(err: &anyhow::Error) -> String {
    serde_json::json!({ "error": err.to_string() }).to_string()
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

/// `GET /logs` (the box itself, or paired -- `LocalOrPaired`): tail the
/// newest tt-inference-server workflow log for `?source=container|run`.
/// Logs say what the box runs and where its files are: not for anyone on
/// the network. See `crate::logs` for the pure tail/redact logic this
/// delegates to. `?source` defaults to `container`; `?tail` defaults to
/// `crate::logs::DEFAULT_TAIL`, capped at `crate::logs::MAX_TAIL`.
///
/// Error contract: an unrecognized `source` is a `400` (caller's mistake); no
/// `tt_inference_repo` configured (non-runpy backend, e.g. dstack) is a `409`
/// (this box simply has no such logs); no log file written yet is NOT an
/// error -- it's a `200` with `lines: []` and `origin: null`, since "nothing
/// served yet" is the normal state of an idle box, not a failure.
async fn get_logs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _caller: LocalOrPaired,
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
        let (origin, lines) = read_log_tail(&repo, source, tail)?;
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

/// The body of `GET /logs`, shared with the assistant's `recent_logs` tool so
/// both read the SAME file the same way: resolve the newest `*.log` for
/// `source` under `repo`'s `workflow_logs/`, take its last `tail` lines, and
/// pass every one through [`crate::logs::redact_line`] (an `hf_…` token in a
/// container log must not reach a chat transcript any more than an HTTP
/// response). Returns `(origin, lines)`: `origin` is the file's path, or
/// `None` -- with no lines -- when nothing has been logged yet.
///
/// Blocking file I/O: call it under `spawn_blocking`.
pub(crate) fn read_log_tail(
    repo: &Path,
    source: crate::logs::LogSource,
    tail: usize,
) -> std::io::Result<(Option<String>, Vec<String>)> {
    let dir = crate::logs::logs_dir(repo, source);
    match crate::logs::newest_log_file(&dir)? {
        Some(path) => {
            let lines = crate::logs::tail_lines(&path, tail)?
                .iter()
                .map(|l| crate::logs::redact_line(l))
                .collect();
            Ok((Some(path.display().to_string()), lines))
        }
        None => Ok((None, Vec::new())),
    }
}

/// `GET /logs/stream` (the box itself or a paired device, like `GET /logs`): upgrade
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
    CallerAuth(caller): CallerAuth,
    RawQuery(query): RawQuery,
) -> Response {
    let (source_str, tail) = parse_logs_query(query.as_deref());
    // Removing the device (or resetting the box) closes the stream.
    let signed_out = state.device_signed_out(caller.device_id());
    ws.protocols([WS_PROTOCOL])
        .on_upgrade(move |socket| logs_stream(socket, state, source_str, tail, signed_out))
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
async fn logs_stream(
    mut socket: WebSocket,
    state: AppState,
    source_str: String,
    tail: usize,
    signed_out: impl std::future::Future<Output = ()>,
) {
    tokio::pin!(signed_out);
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
            () = &mut signed_out => {
                let _ = socket.send(Message::Close(Some(signed_out_frame()))).await;
                break;
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
    _owner: OwnerAuth,
    Json(req): Json<SshAuthorizeRequest>,
) -> Result<Json<SshAuthorizeResponse>, (StatusCode, Json<ErrorResponse>)> {
    authkeys::validate_public_key(&req.public_key).map_err(|err| bad_request(err.to_string()))?;

    let outcome = authkeys::authorize(state.ssh_path(), &req.public_key, &req.label)
        .map_err(|err| bad_request(err.to_string()))?;
    crate::audit::record(&state, "ssh_key_add", &req.label, "ok");

    Ok(Json(SshAuthorizeResponse {
        authorized: true,
        ssh_user: state.ssh_user().to_string(),
        already_present: matches!(outcome, authkeys::AuthorizeOutcome::AlreadyPresent),
    }))
}

/// `text` as a QR code, an SVG of dark squares on white (with a quiet zone).
pub(crate) fn qr_svg(text: &str) -> Option<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let q = 4; // quiet zone, in modules
    let size = w + 2 * q;
    let mut path = String::new();
    for (i, c) in colors.iter().enumerate() {
        if *c == qrcode::Color::Dark {
            path.push_str(&format!("M{} {}h1v1h-1z", i % w + q, i / w + q));
        }
    }
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" shape-rendering=\"crispEdges\"><rect width=\"{size}\" height=\"{size}\" fill=\"#fff\"/><path d=\"{path}\" fill=\"#000\"/></svg>"
    ))
}

/// `GET /ssh/info` (authed): how an SSH app (Termius, a terminal) reaches this
/// box -- the account, port, addresses, whether the SSH server is listening,
/// the keys TT Station has added, and the standard `~/.ssh/config` block that
/// SSH apps import (Termius imports `~/.ssh` on the desktop).
async fn ssh_info(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<serde_json::Value> {
    let user = state.ssh_user().to_string();
    let name = state.name().to_string();
    let path = state.ssh_path().to_path_buf();
    let (lan, listening, keys) = tokio::task::spawn_blocking(move || {
        let listening = std::net::TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], 22)),
            std::time::Duration::from_millis(500),
        )
        .is_ok();
        // Keys TT Station added carry a `ttstation:<label>` marker.
        let keys: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let raw = l
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("ttstation:"))?;
                let mut label = raw;
                // Phones label their keys `ttstation:<device>:<date>`: show the device.
                while let Some(rest) = label.strip_prefix("ttstation:") {
                    label = rest;
                }
                let (label, added) = match label.rsplit_once(':') {
                    Some((name, date))
                        if date.len() == 10 && date.chars().filter(|c| *c == '-').count() == 2 =>
                    {
                        (name, Some(date))
                    }
                    _ => (label, None),
                };
                let kind = l
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("ssh-");
                // `raw` is the stored label, what `DELETE /ssh/authorize` takes.
                Some(
                    serde_json::json!({ "label": label, "type": kind, "added": added, "raw": raw }),
                )
            })
            .collect();
        (crate::pairqr::lan_ipv4(), listening, keys)
    })
    .await
    .unwrap_or((None, false, Vec::new()));
    let mut hosts = vec![
        serde_json::json!({ "label": "On your network, by name", "host": format!("{name}.local") }),
    ];
    if let Some(ip) = lan {
        hosts.push(
            serde_json::json!({ "label": "On your network, by address", "host": ip.to_string() }),
        );
    }
    let primary = format!("{name}.local");
    let link = format!("ssh://{user}@{primary}:22");
    Json(serde_json::json!({
        "listening": listening, "user": user, "port": 22, "name": name, "hosts": hosts, "keys": keys,
        // For a phone to scan: its camera opens the link in an SSH app that handles `ssh://`.
        "qr_svg": qr_svg(&link),
        "command": format!("ssh {user}@{primary}"),
        "link": link,
        "config": format!("Host {name}\n  HostName {primary}\n  User {user}\n  Port 22\n"),
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
    _owner: OwnerAuth,
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
    let what = match &which {
        authkeys::Revoke::Label(l) => l.clone(),
        authkeys::Revoke::Blob(_) => "a key, by its key material".to_string(),
    };
    crate::audit::record(&state, "ssh_key_remove", &what, "ok");

    Ok(Json(SshRevokeResponse { revoked: true }))
}

/// Map an [`crate::assistant::AssistantError`] onto this crate's
/// `{ "error": "..." }` contract:
///
/// | Variant | Status | Why |
/// |---|---|---|
/// | `NotServing` | `409` | Same "well-formed, wrong box state" code `GET /endpoint` uses when idle. The message is written to be shown to a person as-is. |
/// | `BadRequest` | `400` | The client's history was unusable (see `assistant::prepare_history`). |
/// | `UnknownAction` | `404` | No such pending action -- never existed, already confirmed/denied, or expired. |
/// | `Upstream` | `502` | The served model couldn't be reached or answered garbage: the agent is a gateway here, and the failure is upstream of it. |
/// | `Backend` | `409`/`500` | A confirmed action failed -- `contention_aware_error`, exactly like `/run`/`/stop`/`/power`. |
fn assistant_error(err: crate::assistant::AssistantError) -> (StatusCode, Json<ErrorResponse>) {
    use crate::assistant::{AssistantError, PENDING_TTL};
    let (status, error) = match err {
        AssistantError::NotServing => (
            StatusCode::CONFLICT,
            "No model is serving — start one first".to_string(),
        ),
        AssistantError::NotChat(msg) => (StatusCode::CONFLICT, msg),
        AssistantError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        AssistantError::UnknownAction => (
            StatusCode::NOT_FOUND,
            format!(
                "No pending action with that id — it was already answered or expired (actions expire after {} minutes)",
                PENDING_TTL.as_secs() / 60
            ),
        ),
        AssistantError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg),
        AssistantError::Backend(e) => return contention_aware_error(e),
    };
    (status, Json(ErrorResponse { error }))
}

/// `POST /assistant { "messages": [...] }` (bearer-guarded): chat with the
/// box, using the model it is currently serving as the brain. Read-only
/// tools run immediately; disruptive ones come back as
/// `needs_confirmation` actions for `POST /assistant/confirm`. `409` when
/// nothing is serving. The whole loop lives in [`crate::assistant::converse`]
/// -- see that module's doc for the confirmation model.
///
/// `BearerAuth` is listed before the `Json` body extractor so an
/// unauthenticated request is a `401` regardless of what its body looks like.
async fn assistant_chat(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::AssistantRequest>,
) -> Result<Json<crate::assistant::AssistantResponse>, (StatusCode, Json<ErrorResponse>)> {
    let opts = req.options(false);
    // The tools that act on the box's own computer are an owner's.
    let host_ok = caller.owner();
    if let Some(done) = crate::assistant::HOST_OK
        .scope(
            host_ok,
            crate::assistant::quick(&state, &req.messages, opts),
        )
        .await
    {
        return Ok(Json(done));
    }
    crate::people::CALLER
        .scope(
            who.person,
            crate::assistant::HOST_OK.scope(
                host_ok,
                crate::assistant::converse(&state, req.messages, opts),
            ),
        )
        .await
        .map(Json)
        .map_err(assistant_error)
}

/// `POST /assistant/stream` (bearer-guarded): `/assistant`, streamed as
/// server-sent events -- one `data: {"type": ...}` line per
/// [`crate::assistant::StreamEvent`]: `delta`/`reasoning` text as the model
/// writes it, `action`s as tools run, then `done` with the authoritative reply
/// (or `error`). Defaults to the `detailed` (Markdown) style. Problems known
/// before the model is asked (nothing serving, a non-chat model, a bad history)
/// are ordinary HTTP errors, like `/assistant`'s.
async fn assistant_stream(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::AssistantRequest>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, Json<ErrorResponse>),
> {
    let opts = req.options(true);
    let person = who.person;
    let signed_out = state.signed_out(who.token);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // The tools that act on the box's own computer are an owner's.
    let host_ok = caller.owner();
    if let Some(done) = crate::assistant::HOST_OK
        .scope(
            host_ok,
            crate::assistant::quick(&state, &req.messages, opts),
        )
        .await
    {
        // An instant command: already done; tell it like a stream.
        for action in done.actions {
            let _ = tx.send(crate::assistant::StreamEvent::Action { action });
        }
        let _ = tx.send(crate::assistant::StreamEvent::Delta {
            text: done.reply.clone(),
        });
        let _ = tx.send(crate::assistant::StreamEvent::Done {
            reply: done.reply,
            model: done.model,
        });
    } else {
        let chat = crate::people::CALLER
            .scope(
                person.clone(),
                crate::assistant::prepare_stream(&state, req.messages, opts),
            )
            .await
            .map_err(assistant_error)?;
        tokio::spawn(
            crate::people::CALLER.scope(
                person,
                crate::assistant::HOST_OK
                    .scope(host_ok, crate::assistant::converse_stream(state, chat, tx)),
            ),
        );
    }
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let event = rx.recv().await?;
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(axum::response::sse::Event::default().data(data)), rx))
    });
    // Removing the device ends the stream.
    let events = futures_util::StreamExt::take_until(events, Box::pin(signed_out));
    Ok(axum::response::sse::Sse::new(events).keep_alive(axum::response::sse::KeepAlive::default()))
}

/// `POST /assistant/confirm { "action_id": "...", "approve": <bool> }`
/// (bearer-guarded): approve or deny a pending action from
/// `POST /assistant`. Consumes the action either way; `404` if it's unknown
/// or expired. An approved `run_model` returns `"started"` immediately and
/// serves in the background (exactly `/run`'s body, just not awaited);
/// `stop_model`/`reset_chips` run inline and return `"done"`. See
/// [`crate::assistant::confirm`].
async fn assistant_confirm(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::ConfirmRequest>,
) -> Result<Json<crate::assistant::ConfirmResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Resetting the chips is a power action: an owner's, here as on `/power`.
    let tool = state.pending_action_tool(&req.action_id);
    if req.approve && tool.as_deref() == Some("reset_chips") && !caller.owner() {
        crate::audit::record(&state, "denied", "assistant reset_chips", "denied");
        return Err(tt_error(StatusCode::FORBIDDEN, OWNER_ONLY));
    }
    let approve = req.approve;
    let done = crate::assistant::HOST_OK
        .scope(caller.owner(), crate::assistant::confirm(&state, req))
        .await;
    let what = tool.unwrap_or_else(|| "unknown action".to_string());
    crate::audit::record(
        &state,
        "assistant_approval",
        &what,
        match (&done, approve) {
            (Err(_), _) => "failed",
            (Ok(_), true) => "approved",
            (Ok(_), false) => "declined",
        },
    );
    done.map(Json).map_err(assistant_error)
}

/// The plain-HTTP app: the full API for local callers only, plus the routes
/// that only the box itself may use (pairing a phone from the desktop app).
pub fn local_http_app(state: AppState) -> Router {
    let local_only = Router::new()
        .route("/pair/qr", post(pair_qr))
        .route("/security", get(get_security).post(post_security))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state.clone());
    app(state.clone())
        .merge(local_only)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            only_local,
        ))
        .layer(axum::middleware::from_fn_with_state(state, known_host))
}

/// `GET /security` (the box itself): how the box is reachable.
async fn get_security(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "lan_http": lan_http_allowed(&state),
        "https_port": state.ctrl_port() + 1,
        "tls": state.tls(),
    }))
}

#[derive(Deserialize)]
struct SecurityBody {
    lan_http: bool,
}

/// `POST /security` (the box itself): allow or forbid plain HTTP for other
/// computers' apps on `/v1` (phones always use HTTPS).
async fn post_security(
    axum::extract::State(state): axum::extract::State<AppState>,
    caller: Option<axum::extract::Extension<LocalCaller>>,
    _owner: OwnerAuth,
    Json(body): Json<SecurityBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if caller.is_none() {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "Change this from TT Station on the box itself.",
        ));
    }
    let flag = state.config_dir().join("allow-lan-http");
    let done = if body.lan_http {
        std::fs::write(&flag, "")
    } else {
        std::fs::remove_file(&flag).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(())
            } else {
                Err(e)
            }
        })
    };
    done.map_err(|e| tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::audit::record(
        &state,
        "security",
        &format!("lan_http={}", body.lan_http),
        "ok",
    );
    Ok(Json(serde_json::json!({ "lan_http": body.lan_http })))
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/healthz", get(crate::health::healthz))
        .route("/config", get(get_config))
        .route("/models", get(get_models))
        .route("/serving", get(get_serving))
        .route("/logs", get(get_logs))
        .route("/logs/stream", get(logs_ws))
        .route("/telemetry", get(telemetry_ws))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/pair/offer", post(pair_offer))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/reset", post(reset))
        .route("/devices", get(get_devices))
        .route("/devices/{id}", axum::routing::delete(delete_device))
        .route("/devices/{id}/owner", post(post_device_owner))
        .route(
            "/devices/{id}/person",
            axum::routing::put(put_device_person),
        )
        .route("/audit", get(crate::audit::get_audit))
        .route("/power", post(power))
        .route("/endpoint", get(get_endpoint))
        .route("/leases", get(get_leases))
        .route("/ssh/authorize", post(ssh_authorize).delete(ssh_revoke))
        .route("/ssh/info", get(ssh_info))
        .route("/discover", get(crate::discover::get_discover))
        .route("/assistant", post(assistant_chat))
        .route("/assistant/confirm", post(assistant_confirm))
        .route(
            "/assistant/client-result",
            post(crate::assistant::post_client_result),
        )
        .route("/assistant/stream", post(assistant_stream))
        .route("/tt/models", get(tt_models))
        .route("/tt/pulls", get(tt_pulls))
        .route("/tt/pull", post(tt_pull))
        .route("/tt/pull/cancel", post(tt_pull_cancel))
        .route("/serve", get(get_serve).post(post_serve))
        .route(
            "/keys",
            get(crate::gateway::list_keys).post(crate::gateway::create_key),
        )
        .route(
            "/keys/{id}",
            axum::routing::delete(crate::gateway::revoke_key).patch(crate::gateway::change_key),
        )
        .route("/connect", get(crate::gateway::connect_info))
        .route("/bench", post(crate::bench::post_bench))
        .route("/creations/{id}", get(get_creation))
        .route("/memories", get(list_memories).delete(clear_memories))
        .route("/power/info", get(power_info))
        .route("/tls", get(crate::tls::get_tls))
        .route("/jobs", get(crate::jobs::get_jobs))
        .route(
            "/jobs/transcribe",
            post(crate::jobs::post_transcribe).layer(axum::extract::DefaultBodyLimit::max(
                crate::jobs::MAX_UPLOAD_BYTES,
            )),
        )
        .route("/jobs/{id}", axum::routing::delete(crate::jobs::delete_job))
        .route("/jobs/{id}/run", post(crate::jobs::post_run_now))
        .route("/jobs/{id}/result", get(crate::jobs::get_result))
        .route("/jobs/{id}/share", post(crate::jobs::post_share))
        .route(
            "/data/settings",
            get(crate::export::get_settings).put(crate::export::put_settings),
        )
        .route("/export", post(crate::export::post_export))
        .route(
            "/import",
            post(crate::export::post_import).layer(axum::extract::DefaultBodyLimit::max(
                crate::export::MAX_IMPORT_BYTES as usize,
            )),
        )
        .route("/app", get(crate::app_ui::get_app_root))
        .route("/app/", get(crate::app_ui::get_app_root))
        .route("/app/{*rel}", get(crate::app_ui::get_app_path))
        .route("/apps", get(crate::apps::get_apps))
        .route("/apps/{id}/start", post(crate::apps::post_start))
        .route("/apps/{id}/stop", post(crate::apps::post_stop))
        .route(
            "/apps/{id}/network",
            axum::routing::put(crate::apps::put_network),
        )
        .route("/apps/{id}", axum::routing::delete(crate::apps::delete_app))
        .route("/energy", get(get_energy))
        .route("/record", get(crate::recorder::get_record))
        .route("/assistant/brain", get(crate::assistant::get_brain))
        .route("/bringup/engines", get(crate::bringup::get_engines))
        .route("/community", get(crate::community::get_community))
        .route("/community/catalog", get(crate::community::get_catalog))
        .route(
            "/community/info/{ns}/{name}",
            get(crate::community::get_info),
        )
        .route("/community/pull", post(crate::community::post_pull))
        .route("/community/pull/pause", post(crate::community::post_pause))
        .route(
            "/community/pull/cancel",
            post(crate::community::post_cancel),
        )
        .route("/community/serve", post(crate::community::post_serve))
        .route(
            "/community/{ns}/{name}",
            axum::routing::delete(crate::community::delete_package),
        )
        .route("/hermes", get(crate::hermes::get_hermes))
        .route("/hermes/install", post(crate::hermes::post_install))
        .route("/hermes/open", post(crate::hermes::post_open))
        .route("/hermes/key", post(crate::hermes::post_key))
        .route("/vllm/native", get(crate::native_vllm::get_native))
        .route("/vllm/native/setup", post(crate::native_vllm::post_setup))
        .route("/vllm/native/serve", post(crate::native_vllm::post_serve))
        .route("/bringup/survey", post(crate::bringup::post_survey))
        .route(
            "/bringup/claude/login",
            post(crate::bringup::post_claude_login),
        )
        .route(
            "/bringup/codex/runs",
            get(crate::bringup::get_runs).post(crate::bringup::post_run),
        )
        .route(
            "/bringup/codex/runs/{id}",
            get(crate::bringup::get_run).delete(crate::bringup::delete_run),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals",
            post(crate::bringup::post_approval),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals/{aid}/wait",
            post(crate::bringup::post_wait),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals/{aid}",
            post(crate::bringup::post_decide),
        )
        .route(
            "/bringup/codex/runs/{id}/{action}",
            post(crate::bringup::post_action),
        )
        .route("/toplike", get(crate::toplike_term::get_toplike))
        .route("/toplike/term", get(crate::toplike_term::term_ws))
        .route("/coding/term", get(crate::coding::term_ws))
        .route(
            "/preferences/models",
            get(crate::prefs::get_prefs).put(crate::prefs::put_prefs),
        )
        .route("/desktop/apps", get(crate::desktop::get_apps))
        .route("/desktop/open", post(crate::desktop::post_open))
        .route(
            "/voice",
            get(crate::voice::get_voice)
                .put(crate::voice::put_voice)
                .delete(crate::voice::delete_voice),
        )
        .route("/voice/setup", post(crate::voice::post_setup))
        .route("/voice/speak", post(crate::voice::post_speak))
        .route(
            "/voice/hear",
            get(crate::voice::get_hear)
                .post(crate::voice::post_hear)
                .delete(crate::voice::delete_hear),
        )
        .route("/voice/say", post(crate::voice::post_say))
        .route("/voice/hear/finish", post(crate::voice::post_hear_finish))
        .route(
            "/voice/listen",
            post(crate::voice::post_listen)
                .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route("/record/start", post(crate::recorder::post_start))
        .route("/record/stop", post(crate::recorder::post_stop))
        .route(
            "/schedules",
            get(crate::schedules::get_schedules).post(crate::schedules::post_schedule),
        )
        .route(
            "/schedules/{id}",
            axum::routing::patch(crate::schedules::patch_schedule)
                .delete(crate::schedules::delete_schedule),
        )
        .route(
            "/people",
            get(crate::people::get_people).post(crate::people::post_person),
        )
        .route("/people/me", axum::routing::put(crate::people::put_me))
        .route(
            "/people/{id}",
            axum::routing::patch(crate::people::patch_person).delete(crate::people::delete_person),
        )
        .route("/orchard", get(crate::orchard::get_orchard))
        .route("/orchard/setup", post(crate::orchard::post_setup))
        .route("/orchard/preflight", post(crate::orchard::post_preflight))
        .route("/orchard/runs", post(crate::orchard::post_run))
        .route(
            "/orchard/runs/{name}",
            get(crate::orchard::get_run).delete(crate::orchard::delete_run),
        )
        .route(
            "/orchard/runs/{name}/serve",
            post(crate::orchard::post_serve),
        )
        .route("/orchard/packaging", post(crate::orchard::post_packaging))
        .route(
            "/orchard/lab",
            get(crate::orchard_lab::get_lab).put(crate::orchard_lab::put_lab),
        )
        .route("/orchard/lab/check", post(crate::orchard_lab::post_check))
        .route("/orchard/lab/setup", post(crate::orchard_lab::post_setup))
        .route(
            "/orchard/runs/{name}/{action}",
            post(crate::orchard::post_control),
        )
        .route(
            "/orchard/runs/{name}/report/{which}",
            get(crate::orchard::get_report),
        )
        .route("/orchard/runs/{name}/story", get(crate::orchard::get_story))
        .route(
            "/favorites",
            get(crate::favorites::get_favorites).put(crate::favorites::put_favorite),
        )
        .route(
            "/research",
            get(crate::research::get_research).post(crate::research::post_research),
        )
        .route(
            "/research/{id}",
            get(crate::research::get_job).delete(crate::research::delete_job),
        )
        .route("/research/{id}/stop", post(crate::research::post_stop))
        .route("/research/{id}/rename", post(crate::research::post_rename))
        .route(
            "/research/{id}/explain",
            post(crate::research::post_explain),
        )
        .route("/skills", get(crate::skills::get_skills))
        .route(
            "/embeddings",
            get(crate::embeddings::get_embeddings).put(crate::embeddings::put_embeddings),
        )
        .route("/code", get(crate::workspace::get_code))
        .route("/code/setup", post(crate::workspace::post_setup))
        .route("/code/sessions", post(crate::workspace::post_session))
        .route(
            "/code/sessions/{id}",
            get(crate::workspace::get_session).delete(crate::workspace::delete_session),
        )
        .route(
            "/code/sessions/{id}/message",
            post(crate::workspace::post_message),
        )
        .route(
            "/code/sessions/{id}/approve",
            post(crate::workspace::post_approve),
        )
        .route(
            "/code/sessions/{id}/interrupt",
            post(crate::workspace::post_interrupt),
        )
        .route(
            "/code/sessions/{id}/stop",
            post(crate::workspace::post_stop),
        )
        .route(
            "/code/sessions/{id}/changes",
            get(crate::workspace::get_changes),
        )
        .route(
            "/code/sessions/{id}/commit",
            post(crate::workspace::post_commit),
        )
        .route(
            "/code/sessions/{id}/discard",
            post(crate::workspace::post_discard),
        )
        .route(
            "/code/sessions/{id}/forget",
            post(crate::workspace::post_forget),
        )
        .route(
            "/code/sessions/{id}/settings",
            post(crate::workspace::post_settings),
        )
        .route(
            "/code/sessions/{id}/rename",
            post(crate::workspace::post_rename),
        )
        .route(
            "/code/sessions/{id}/speed",
            get(crate::codespeed::get_speed),
        )
        .route("/code/models", get(crate::codespeed::get_models))
        .route("/coding", get(crate::coding::get_coding))
        .route("/coding/sessions", post(crate::coding::post_session))
        .route(
            "/coding/sessions/{id}",
            axum::routing::delete(crate::coding::delete_session),
        )
        .route("/coding/sessions/{id}/stop", post(crate::coding::post_stop))
        .route(
            "/coding/sessions/{id}/changes",
            get(crate::coding::get_changes),
        )
        .route(
            "/coding/folders",
            get(crate::coding::get_folders).post(crate::coding::post_folder),
        )
        .route("/skills/{id}/hermes", post(crate::skills::post_hermes))
        .route(
            "/idle",
            get(crate::idle::get_idle).put(crate::idle::put_idle),
        )
        .route("/idle/wake", post(crate::idle::post_wake))
        .route("/updates", get(crate::updates::get_updates))
        .route("/updates/install", post(crate::updates::post_install))
        .route("/updates/undo", post(crate::updates::post_undo))
        .route("/approvals", get(crate::approvals::get_approvals))
        .route("/approvals/answer", post(crate::approvals::post_answer))
        .route(
            "/evals",
            get(crate::evals::get_evals).post(crate::evals::post_eval),
        )
        .route("/evals/stop", post(crate::evals::post_stop))
        .route(
            "/evals/{id}",
            get(crate::evals::get_eval).delete(crate::evals::delete_eval),
        )
        .route("/library", get(crate::library::get_library))
        .route("/library/sources", post(crate::library::post_source))
        .route(
            "/library/sources/{id}",
            axum::routing::delete(crate::library::delete_source),
        )
        .route(
            "/library/sources/{id}/reindex",
            post(crate::library::post_reindex),
        )
        .route("/library/search", post(crate::library::post_search))
        .route(
            "/routines",
            get(crate::routines::get_routines).post(crate::routines::post_routine),
        )
        .route(
            "/routines/{id}",
            axum::routing::put(crate::routines::put_routine)
                .delete(crate::routines::delete_routine),
        )
        .route("/routines/{id}/run", post(crate::routines::post_run))
        .route("/routines/{id}/runs", get(crate::routines::get_runs))
        .route("/routines/runs/{run}", get(crate::routines::get_run))
        .route(
            "/routines/runs/{run}/approve",
            post(crate::routines::post_approve),
        )
        .route(
            "/routines/runs/{run}/stop",
            post(crate::routines::post_stop),
        )
        .route("/lab", get(crate::lab::get_lab))
        .route("/lab/prepare", post(crate::lab::post_prepare))
        .route("/lab/run", post(crate::lab::post_run))
        .route("/modes", get(crate::modes::get_modes))
        .route("/modes/{id}", post(crate::modes::post_mode))
        .route("/software", get(crate::tttools::get_software))
        .route(
            "/software/cli-update",
            post(crate::tttools::post_cli_update),
        )
        .route(
            "/tt/models/{name}",
            axum::routing::delete(crate::tttools::delete_model),
        )
        .route("/clients", get(crate::tttools::get_clients))
        .route("/clients/{tool}/start", post(crate::tttools::start_client))
        .route("/clients/{tool}/stop", post(crate::tttools::stop_client))
        .route("/doctor", get(crate::doctor::get_doctor))
        .route("/doctor/fix/{fix}", post(crate::doctor::post_fix))
        .route("/diagnostics", post(crate::doctor::post_diagnostics))
        .route("/diagnostics/{id}", get(crate::doctor::get_diagnostics))
        .route("/notify", get(crate::notify::get_notify))
        .route("/notify/devices", post(crate::notify::register_device))
        .route(
            "/notify/devices/{token}",
            axum::routing::delete(crate::notify::unregister_device),
        )
        .route("/notify/test", post(crate::notify::test_notify))
        .route("/events", get(crate::notify::get_events))
        .route("/memories/{id}", axum::routing::delete(delete_memory))
        .route("/v1/models", get(crate::gateway::v1_models))
        .route("/v1/chat/completions", post(crate::gateway::v1_text))
        .route("/v1/completions", post(crate::gateway::v1_text))
        .route("/v1/embeddings", post(crate::gateway::v1_embeddings))
        .route("/v1/rerank", post(crate::gateway::v1_rerank))
        .route("/v1/messages", post(crate::anthropic::v1_messages))
        .route(
            "/v1/messages/count_tokens",
            post(crate::anthropic::count_tokens),
        )
        .route("/v1/images/generations", post(crate::gateway::v1_images))
        .route(
            "/coserve",
            get(crate::coserve::get_coserve).post(crate::coserve::post_coserve),
        )
        .route("/metrics/history", get(metrics_history))
        .route("/metrics/now", get(metrics_now))
        .route(
            "/docs",
            get(list_docs)
                .post(add_doc)
                .layer(axum::extract::DefaultBodyLimit::max(DOC_BODY_LIMIT)),
        )
        .route("/docs/search", post(search_docs))
        .route("/docs/{id}", axum::routing::delete(delete_doc))
        .route("/generate/image", post(generate_image))
        .route("/generate/video", post(generate_video))
        .route("/generate/videos", get(list_videos))
        .route("/generate/video/{id}", get(get_video))
        .route("/generate/video/{id}/download", get(download_video))
        .route("/generate/video/{id}/cancel", post(cancel_video))
        // Default-deny (see `require_auth`): last, so it covers every route above.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state)
}

// ---------------------------------------------------------------------
// Dashboards: `/metrics/*` and the recorder behind them
// ---------------------------------------------------------------------

/// How often the recorder samples the box.
pub const METRICS_INTERVAL: Duration = Duration::from_secs(5);

/// How often the recorder runs `tt-smi -s` itself. A `/telemetry` viewer's
/// reading (shared through `cached_snapshot`) is used whenever there is a fresh
/// one; with nobody watching, a chip reading every 30 s is plenty for the
/// dashboards' history, and each `tt-smi` touches the chips a model is using.
pub const CHIPS_INTERVAL: Duration = Duration::from_secs(30);

/// Whether the recorder should read the chips itself now: no viewer has paid
/// for a fresh reading, and its own last one (if any) is [`CHIPS_INTERVAL`] old.
fn chips_due(last_read: Option<Instant>) -> bool {
    last_read.is_none_or(|t| t.elapsed() >= CHIPS_INTERVAL)
}

/// Sample the box every [`METRICS_INTERVAL`] for as long as the agent runs:
/// chip telemetry (a `/telemetry` viewer's fresh reading, else its own every
/// [`CHIPS_INTERVAL`] through the same deduplicated `tt-smi -s`) and the
/// served model's metrics (the same scrape tt-toplike's view uses), into
/// [`crate::history::History`]. Saves to `save_to` every minute.
pub async fn record_metrics_forever(state: AppState, save_to: Option<PathBuf>) {
    let history = state.history();
    let mut sampler = crate::inference::InferenceSampler::new();
    let mut ticker = tokio::time::interval(METRICS_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u64 = 0;
    let mut chips: Vec<crate::history::ChipPoint> = Vec::new();
    let mut chips_read: Option<Instant> = None;
    loop {
        ticker.tick().await;
        ticks += 1;
        let now = unix_now();
        // Between readings a point carries the last one.
        if let Some(json) = state.peek_snapshot(METRICS_INTERVAL) {
            chips = crate::history::chips_from_snapshot(&json);
        } else if chips_due(chips_read) {
            chips_read = Some(Instant::now());
            chips = state
                .cached_snapshot()
                .await
                .map(|json| crate::history::chips_from_snapshot(&json))
                .unwrap_or_default();
        }
        let scrape = scrape_vllm_metrics(resolve_metrics_port(&state)).await;
        let info = sampler.tick(&state.status(), scrape.as_deref());
        let serving = info.as_ref().and_then(|i| i.serving);
        let point = crate::history::Point {
            t: now,
            chips: chips.clone(),
            gen_tps: serving.as_ref().map(|s| s.generation_tps),
            prompt_tps: serving.as_ref().map(|s| s.prompt_tps),
            running: serving.as_ref().map(|s| s.requests_running),
            waiting: serving.as_ref().map(|s| s.requests_waiting),
            kv_cache: serving.as_ref().map(|s| s.kv_cache_usage),
            ttft_s: serving.as_ref().map(|s| s.ttft_avg_s).filter(|v| *v > 0.0),
            tpot_s: serving.as_ref().map(|s| s.tpot_avg_s).filter(|v| *v > 0.0),
        };
        let completed = serving.as_ref().map(|s| s.completed_delta).unwrap_or(0);
        history.record(point, completed, &crate::history::local_day(now));
        history.set_inference(info.and_then(|i| serde_json::to_value(i).ok()));
        if ticks.is_multiple_of(12) {
            if let Some(path) = &save_to {
                let (h, p) = (Arc::clone(&history), path.clone());
                let _ = tokio::task::spawn_blocking(move || h.save(&p)).await;
            }
        }
    }
}

#[derive(Deserialize)]
struct HistoryQuery {
    range: Option<String>,
}

/// `GET /metrics/history?range=5m|1h|24h` (paired, like `/telemetry`):
/// `{range, step_s, points: [{t, chips: [{temp_c, power_w, aiclk_mhz}], gen_tps,
/// prompt_tps, running, waiting, kv_cache, ttft_s, tpot_s}], totals}`.
async fn metrics_history(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(q): axum::extract::Query<HistoryQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let label = q.range.unwrap_or_else(|| "1h".to_string());
    let range = crate::history::Range::parse(&label)
        .ok_or_else(|| tt_error(StatusCode::BAD_REQUEST, "range must be 5m, 1h or 24h"))?;
    let history = state.history();
    let (step, points) = history.range(range, unix_now());
    Ok(Json(serde_json::json!({
        "range": label,
        "step_s": step,
        "points": points,
        "totals": history.totals(),
    })))
}

/// `GET /metrics/now` (paired): the latest sample, the live inference view
/// (phase, throughput, latency, media counters), the energy/token totals, and
/// how well each model's tool calls fit their schemas (`/v1/messages`).
async fn metrics_now(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    let history = state.history();
    Json(serde_json::json!({
        "point": history.latest(),
        "inference": history.inference(),
        "totals": history.totals(),
        "tool_calls": crate::anthropic::tool_call_health(),
    }))
}

// ---------------------------------------------------------------------
// Image and video generation, proxied to the served media server
// ---------------------------------------------------------------------

/// The served media server's root URL (the endpoint without its `/v1`), after
/// checking the served model is of `kind`.
async fn media_base(
    state: &AppState,
    kind: &'static str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    // Co-serving: the companion slot answers image requests.
    if let Some(base) = state.companion().and_then(|c| c.media_base(kind)) {
        return Ok(base);
    }
    let _ = state.reconciled_status().await;
    let endpoint = state.endpoint().ok_or_else(|| {
        tt_error(
            StatusCode::CONFLICT,
            format!(
                "Nothing is serving. Start a{} model first.",
                if kind == "image" { "n image" } else { " video" }
            ),
        )
    })?;
    let probe = state.clone();
    let served = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    if served != kind {
        return Err(tt_error(
            StatusCode::CONFLICT,
            format!(
                "The box is serving {} ({served}), not {} generation. Start a{} model first.",
                endpoint.model,
                kind,
                if kind == "image" { "n image" } else { " video" }
            ),
        ));
    }
    Ok(endpoint
        .base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string())
}

/// Forward one request to the media server and hand its status + body back.
async fn media_proxy(
    method: reqwest::Method,
    url: String,
    body: Option<serde_json::Value>,
    timeout: Duration,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => {
            return tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    };
    // tt-media-server 0.10 images check `Authorization: Bearer $API_KEY` even
    // when launched with NO_AUTH (only newer servers honour it), and the box
    // never sets API_KEY, so the server's default applies. Newer servers with
    // NO_AUTH ignore the header.
    let api_key =
        std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
    let mut req = client.request(method, &url).bearer_auth(api_key);
    if let Some(b) = body {
        req = req.json(&b);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => {
            return tt_error(
                StatusCode::GATEWAY_TIMEOUT,
                "The model took too long to answer.",
            )
            .into_response()
        }
        Err(e) => {
            return tt_error(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't reach the model server: {e}"),
            )
            .into_response()
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return tt_error(
                StatusCode::BAD_GATEWAY,
                format!("Reading the model's answer failed: {e}"),
            )
            .into_response()
        }
    };
    if !status.is_success() && content_type.starts_with("application/json") {
        // FastAPI's `{"detail": ...}` -> this API's `{"error": ...}`.
        let detail = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.get("detail").map(|d| {
                    d.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| d.to_string())
                })
            });
        if let Some(detail) = detail {
            return tt_error(status, detail).into_response();
        }
    }
    (
        status,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        bytes,
    )
        .into_response()
}

/// `POST /generate/image` (authed): tt-media-server's `/v1/images/generations`
/// body (`prompt`, `negative_prompt`, `num_inference_steps`, `seed`,
/// `number_of_images`, `guidance_scale`, `image_return_format`) passed through;
/// answers `{images: [base64...], generation_time}`.
async fn generate_image(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(mut body): Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Media servers (0.10) require `guidance_scale` (>= 1); FLUX schnell is
    // guidance-distilled, so the usual 3.5 is a safe default for clients that
    // don't set it.
    if let Some(obj) = body.as_object_mut() {
        obj.entry("guidance_scale")
            .or_insert(serde_json::json!(3.5));
    }
    match media_base(&state, "image").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::POST,
                format!("{base}/v1/images/generations"),
                Some(body),
                Duration::from_secs(600),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

/// `POST /generate/video` (authed): submit a text-to-video job (`prompt`,
/// `negative_prompt`, `num_inference_steps`, `seed`); answers the job object.
async fn generate_video(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match media_base(&state, "video").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::POST,
                format!("{base}/v1/videos/generations"),
                Some(body),
                Duration::from_secs(120),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

/// `GET /generate/videos` (authed): every video job the server knows.
async fn list_videos(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match media_base(&state, "video").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::GET,
                format!("{base}/v1/videos/jobs"),
                None,
                Duration::from_secs(30),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

/// `GET /generate/video/{id}` (authed): a job's status.
async fn get_video(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match media_base(&state, "video").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::GET,
                format!("{base}/v1/videos/generations/{id}"),
                None,
                Duration::from_secs(30),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

/// `GET /generate/video/{id}/download` (authed): the finished MP4.
async fn download_video(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match media_base(&state, "video").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::GET,
                format!("{base}/v1/videos/generations/{id}/download"),
                None,
                Duration::from_secs(600),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

/// `POST /generate/video/{id}/cancel` (authed).
async fn cancel_video(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match media_base(&state, "video").await {
        Ok(base) => {
            media_proxy(
                reqwest::Method::POST,
                format!("{base}/v1/videos/generations/{id}/cancel"),
                None,
                Duration::from_secs(30),
            )
            .await
        }
        Err(e) => e.into_response(),
    }
}

// ---------------------------------------------------------------------
// The tt CLI as model manager: `/tt/models`, `/tt/pulls`, `/tt/pull`
// ---------------------------------------------------------------------

/// `POST /tt/pull` / `POST /tt/pull/cancel` body.
#[derive(Deserialize)]
struct PullRequest {
    /// tt catalog name (`Qwen3-32B`) or HF repo id (`Qwen/Qwen3-32B`).
    model: String,
}

fn tt_unavailable() -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::CONFLICT,
        Json(ErrorResponse {
            error: "The official Tenstorrent CLI (tt) isn't installed on this box, so models \
                    can't be listed or downloaded from here."
                .to_string(),
        }),
    )
}

/// `GET /tt/models` (paired, like `/models`): this mesh's catalog from
/// `tt model list` -- support status, tool-call parser, whether the weights are
/// downloaded and their size -- plus this agent's pulls and the free disk where
/// weights land.
async fn tt_models(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    let (catalog, pulls, free) = tokio::task::spawn_blocking(move || {
        (
            tt.catalog(device.as_deref(), false),
            tt.pulls(),
            crate::ttcli::disk_free_bytes(tt.hf_cache()),
        )
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("catalog task panicked: {e}"),
        )
    })?;
    let catalog = catalog.map_err(backend_error)?;
    Ok(Json(serde_json::json!({
        "device": catalog.device,
        "disk_free_bytes": free,
        "models": catalog.models,
        "pulls": pulls,
    })))
}

/// `GET /tt/pulls` (paired): just the pulls and free disk -- cheap enough to
/// poll every couple of seconds while a download runs.
async fn tt_pulls(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let (pulls, free) = tokio::task::spawn_blocking(move || {
        (tt.pulls(), crate::ttcli::disk_free_bytes(tt.hf_cache()))
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pulls task panicked: {e}"),
        )
    })?;
    Ok(Json(
        serde_json::json!({ "pulls": pulls, "disk_free_bytes": free }),
    ))
}

/// `POST /tt/pull` (authed): start downloading a catalog model with
/// `tt model pull`, in the background. `202 {"status":"started"}`, or `200
/// {"status":"already_downloaded"}`. Refuses a model this mesh can't run
/// (`400`), one not in the catalog (`404`), a pull already running or too
/// little free disk for tt's stated requirement (`409`).
async fn tt_pull(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PullRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    tokio::task::spawn_blocking(move || {
        let catalog = tt.catalog(device.as_deref(), true).map_err(backend_error)?;
        let mesh = catalog
            .device
            .clone()
            .unwrap_or_else(|| "this box".to_string());
        let model = catalog.find(&req.model).cloned().ok_or_else(|| {
            tt_error(
                StatusCode::NOT_FOUND,
                format!(
                    "There's no model called {} in this box's catalog.",
                    req.model
                ),
            )
        })?;
        if model.weights == crate::ttcli::Weights::Container {
            // The serving container fetches these itself; pulling to the host
            // cache would download gigabytes nothing reads.
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "status": "not_needed", "model": model.name })),
            ));
        }
        if model.weights == crate::ttcli::Weights::Complete && !tt.is_pulling(&model.name) {
            return Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "status": "already_downloaded", "model": model.name })),
            ));
        }
        if !model.supported {
            return Err(tt_error(
                StatusCode::BAD_REQUEST,
                format!(
                    "{} doesn't run on {mesh}, so there's no point downloading it here.",
                    model.name
                ),
            ));
        }
        if let (Some(free), Some(need_gb)) = (
            crate::ttcli::disk_free_bytes(tt.hf_cache()),
            model.min_disk_gb,
        ) {
            if (free as f64) < need_gb * 1e9 {
                return Err(tt_error(
                    StatusCode::CONFLICT,
                    format!(
                        "{} needs about {need_gb:.0} GB of free disk; the box has {:.0} GB.",
                        model.name,
                        free as f64 / 1e9
                    ),
                ));
            }
        }
        match tt.start_pull(&model) {
            Ok(()) => Ok((
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "status": "started", "model": model.name })),
            )),
            Err(crate::ttcli::PullRefusal::AlreadyRunning) => Err(tt_error(
                StatusCode::CONFLICT,
                format!("{} is already downloading.", model.name),
            )),
            Err(crate::ttcli::PullRefusal::Spawn(e)) => {
                Err(tt_error(StatusCode::INTERNAL_SERVER_ERROR, e))
            }
        }
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("pull task panicked: {e}"),
        )
    })?
}

/// `POST /tt/pull/cancel` (authed): kill a running pull. `404` if none.
async fn tt_pull_cancel(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<PullRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let tt = state.tt_cli().ok_or_else(tt_unavailable)?;
    let device = state.device_mesh().map(str::to_string);
    tokio::task::spawn_blocking(move || {
        // Accept the HF repo id too: resolve it to the tt name the job is keyed by.
        let name = tt
            .catalog(device.as_deref(), false)
            .ok()
            .and_then(|c| c.find(&req.model).map(|m| m.name.clone()))
            .unwrap_or(req.model);
        if tt.cancel_pull(&name) {
            Ok(Json(
                serde_json::json!({ "status": "cancelled", "model": name }),
            ))
        } else {
            Err(tt_error(
                StatusCode::NOT_FOUND,
                format!("{name} isn't downloading."),
            ))
        }
    })
    .await
    .map_err(|e| {
        tt_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cancel task panicked: {e}"),
        )
    })?
}

/// `GET /memories` (authed): what the assistant remembers, oldest first.
async fn list_memories(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
) -> Json<serde_json::Value> {
    let person = who.person.map(|p| p.id);
    Json(serde_json::json!({ "memories": state.memories().list_for(person.as_deref()) }))
}

/// `DELETE /memories` (authed): a member forgets its person's own memories;
/// an owner forgets everything it sees, the household's shared ones too.
async fn clear_memories(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let person = who.person.map(|p| p.id);
    if caller.device.as_ref().is_some_and(|d| d.owner) {
        state.memories().clear(person.as_deref());
    } else if let Some(person) = person {
        state.memories().clear_own(&person);
    } else {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "This device isn't anyone's, so its memories are the household's: only an owner can clear those. Pick who you are in Settings → People first.",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /memories/{id}` (authed).
async fn delete_memory(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> StatusCode {
    let gone = state
        .memories()
        .forget(&id, who.person.map(|p| p.id).as_deref());
    if gone.iter().any(|m| m.id == id) {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `GET /creations/{id}` (authed): a JPEG the assistant made in chat.
async fn get_creation(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match state.creations().get(&id) {
        Some(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "image/jpeg"),
                (
                    axum::http::header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response(),
        None => tt_error(StatusCode::NOT_FOUND, "No such image.").into_response(),
    }
}

// ---------------------------------------------------------------------
// Documents (`/docs`): private Q&A over the user's own files. See `crate::docs`.
// ---------------------------------------------------------------------

/// Upload body ceiling: [`crate::docs::MAX_DOC_CHARS`] of multi-byte text.
const DOC_BODY_LIMIT: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct AddDocRequest {
    name: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct SearchDocsRequest {
    query: String,
    #[serde(default)]
    document: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
}

fn docs_unavailable() -> (StatusCode, Json<ErrorResponse>) {
    tt_error(StatusCode::CONFLICT, "This agent has no document store.")
}

/// `GET /docs` (authed): the uploaded documents, newest first.
async fn list_docs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    Ok(Json(serde_json::json!({ "documents": docs.list() })))
}

/// `POST /docs` (authed) `{name, text}`: add a document (the client extracts the
/// text), replacing any with the same name. `201` with its summary.
async fn add_doc(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<AddDocRequest>,
) -> Result<(StatusCode, Json<crate::docs::DocSummary>), (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    if req.name.trim().is_empty() {
        return Err(tt_error(
            StatusCode::BAD_REQUEST,
            "The document needs a name.",
        ));
    }
    if req.text.trim().is_empty() {
        return Err(tt_error(
            StatusCode::BAD_REQUEST,
            "No text found in that document (a scanned PDF has none to read).",
        ));
    }
    if req.text.chars().count() > crate::docs::MAX_DOC_CHARS {
        return Err(tt_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "That document is too long (2 million characters at most).",
        ));
    }
    let now = unix_now();
    tokio::task::spawn_blocking(move || docs.add(&req.name, &req.text, now))
        .await
        .map_err(|e| tt_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(|summary| (StatusCode::CREATED, Json(summary)))
        .map_err(|e| {
            tt_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't save: {e}"),
            )
        })
}

/// `DELETE /docs/{id}` (authed).
async fn delete_doc(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    let docs = state.docs().ok_or_else(docs_unavailable)?;
    if docs.delete(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(tt_error(StatusCode::NOT_FOUND, "No such document."))
    }
}

/// `POST /docs/search` (authed) `{query, document?, top_k?}`: the matching
/// passages, best first -- what the assistant's `search_documents` tool sees.
async fn search_docs(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    Json(req): Json<SearchDocsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    state.docs().ok_or_else(docs_unavailable)?;
    let hits = crate::library::search(
        &state,
        &req.query,
        req.top_k.unwrap_or(5).clamp(1, 20),
        req.document.as_deref(),
    )
    .await;
    Ok(Json(serde_json::json!({ "hits": hits })))
}

#[cfg(test)]
mod telemetry_inference_tests {
    use super::*;
    use libttstation::model::Endpoint;

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
    fn the_recorder_reads_the_chips_every_thirty_seconds_on_its_own() {
        assert!(chips_due(None), "the first tick reads them");
        assert!(!chips_due(Some(Instant::now())));
        assert!(!chips_due(Some(Instant::now() - METRICS_INTERVAL * 5)));
        assert!(chips_due(Some(Instant::now() - CHIPS_INTERVAL)));
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

    #[test]
    fn power_success_status_is_202_for_machine_ops_200_for_reset_chips() {
        use crate::power::PowerAction;
        assert_eq!(
            power_success_status(PowerAction::ResetChips),
            StatusCode::OK
        );
        assert_eq!(
            power_success_status(PowerAction::Suspend),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Reboot),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Shutdown),
            StatusCode::ACCEPTED
        );
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

#[cfg(test)]
mod signed_out_tests {
    use super::{AppState, TelemetrySlot, MAX_TELEMETRY_SOCKETS};
    use std::time::Duration;

    fn state() -> AppState {
        AppState::new(
            "qb2-lab".to_string(),
            "4xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
    }

    /// An open WebSocket's watch resolves when its own device is removed, and
    /// only then: removing another device leaves it open.
    #[tokio::test]
    async fn open_sockets_close_when_their_device_is_removed() {
        let state = state();
        let mine = state.sessions().pair("mine", Some("Mine"), None, false);
        let other = state.sessions().pair("other", Some("Other"), None, false);
        let watch = tokio::spawn(state.signed_out("mine".to_string()));

        state.sessions().revoke(&other.id);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !watch.is_finished(),
            "another device's removal closed this socket"
        );

        state.sessions().revoke(&mine.id);
        tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .expect("the socket stayed open after its device was removed")
            .unwrap();
    }

    #[test]
    fn telemetry_sockets_are_capped_and_given_back() {
        let slots: Vec<TelemetrySlot> = std::iter::from_fn(TelemetrySlot::take)
            .take(MAX_TELEMETRY_SOCKETS + 5)
            .collect();
        assert_eq!(slots.len(), MAX_TELEMETRY_SOCKETS);
        assert!(TelemetrySlot::take().is_none());
        drop(slots);
        assert!(TelemetrySlot::take().is_some());
    }

    /// The same, for a stream that knows its caller by device id (logs,
    /// telemetry); the box's own user is never signed out.
    #[tokio::test]
    async fn streams_known_by_device_close_when_it_is_removed() {
        let state = state();
        let mine = state.sessions().pair("mine", None, None, false);
        let watch = tokio::spawn(state.device_signed_out(Some(mine.id.clone())));
        let the_box = tokio::spawn(state.device_signed_out(None));
        state.sessions().revoke(&mine.id);
        tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .expect("still open after removal")
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!the_box.is_finished());
        the_box.abort();
    }

    #[tokio::test]
    async fn open_sockets_close_on_reset() {
        let state = state();
        state.sessions().pair("mine", None, None, false);
        let watch = tokio::spawn(state.signed_out("mine".to_string()));
        state.clear_tokens();
        tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .expect("still open after a reset")
            .unwrap();
    }
}
