// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Agent and serving logs: `GET /logs` and the `GET /logs/stream` WebSocket.

use std::path::Path;
use std::time::Duration;

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    extract::RawQuery,
    http::StatusCode,
    response::Response,
    Json,
};
use serde::Serialize;

use crate::auth::{signed_out_frame, CallerAuth, LocalOrPaired, WS_PROTOCOL};
use crate::state::AppState;

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
pub(crate) async fn get_logs(
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
pub(crate) async fn logs_ws(
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
