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

use std::path::PathBuf;
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
use serde::Deserialize;

pub use crate::auth::*;
pub(crate) use crate::error::*;
pub(crate) use crate::handlers::assistant::*;
pub(crate) use crate::handlers::devices::*;
pub(crate) use crate::handlers::logs::*;
pub(crate) use crate::handlers::media::*;
pub(crate) use crate::handlers::pairing::*;
pub(crate) use crate::handlers::power::*;
pub(crate) use crate::handlers::security::*;
pub(crate) use crate::handlers::serving::*;
pub(crate) use crate::handlers::ssh::*;
pub(crate) use crate::handlers::status::*;
pub use crate::state::*;

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
