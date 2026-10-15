//! Live and recorded readings: the `GET /telemetry` WebSocket, `GET /energy`,
//! `GET /metrics/history` and `/metrics/now`, and the recorder that keeps the
//! history (`record_metrics_forever`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::auth::{signed_out_frame, BearerAuth, CallerAuth, WS_PROTOCOL};
use crate::error::{tt_error, ErrorResponse};
use crate::state::{unix_now, AppState};
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::RawQuery,
    http::StatusCode,
    response::Response,
    Json,
};
use serde::Deserialize;

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
pub(crate) async fn get_energy(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<serde_json::Value> {
    Json(crate::history::energy_report(&state.history().days()))
}

pub(crate) async fn telemetry_ws(
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
pub(crate) fn resolve_metrics_port(state: &AppState) -> u16 {
    state
        .endpoint()
        .and_then(|e| port_from_base_url(&e.base_url))
        .unwrap_or_else(|| state.serving_port())
}

/// Parse the port out of a base URL shaped like `"http://host:8003/v1"`.
/// `None` on any shape that doesn't parse (no `://`, no port after the last
/// `:`, or a non-numeric port) -- callers treat that identically to "nothing
/// is serving," since a malformed `base_url` can't be scraped either way.
pub(crate) fn port_from_base_url(base_url: &str) -> Option<u16> {
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
pub(crate) fn chips_due(last_read: Option<Instant>) -> bool {
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
pub(crate) struct HistoryQuery {
    range: Option<String>,
}

/// `GET /metrics/history?range=5m|1h|24h` (paired, like `/telemetry`):
/// `{range, step_s, points: [{t, chips: [{temp_c, power_w, aiclk_mhz}], gen_tps,
/// prompt_tps, running, waiting, kv_cache, ttft_s, tpot_s}], totals}`.
pub(crate) async fn metrics_history(
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
pub(crate) async fn metrics_now(
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

#[cfg(test)]
mod metrics_tests {
    use super::*;
    use libttstation::model::Endpoint;
    use std::time::Instant;

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

    #[test]
    fn the_recorder_reads_the_chips_every_thirty_seconds_on_its_own() {
        assert!(chips_due(None), "the first tick reads them");
        assert!(!chips_due(Some(Instant::now())));
        assert!(!chips_due(Some(Instant::now() - METRICS_INTERVAL * 5)));
        assert!(chips_due(Some(Instant::now() - CHIPS_INTERVAL)));
    }
}
