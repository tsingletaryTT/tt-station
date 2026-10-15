// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The real tt-toplike, streamed: `GET /toplike/term` (WebSocket) runs
//! `tt-toplike --remote 127.0.0.1:<this agent>` in a private terminal and
//! streams the screen to the desktop app or the phone, which draw it with a
//! terminal emulator. So TT Station shows exactly tt-toplike's own views,
//! fed by this agent's `/telemetry` -- nothing is re-drawn or imitated.
//!
//! Query: `token` (a pairing token: browsers can't set headers on a socket),
//! `mode` (one of [`MODES`]), `cols`, `rows`. Messages from the client:
//! `{"type":"input","data":"v"}` (keys, as a terminal sends them) and
//! `{"type":"resize","cols":N,"rows":N}`. The server sends the terminal's
//! output as binary frames. Only the tt-toplike binary runs, with fixed
//! arguments; at most a few at a time.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::Value;

use crate::routes::AppState;

/// TT Station's name → (`--mode`, keys to press once it's up).
pub const MODES: &[(&str, &str, &str)] = &[
    ("insights", "normal", ""),
    ("starfield", "starfield", ""),
    ("castle", "castle", ""),
    ("flow", "flow", ""),
    ("arcade", "arcade", ""),
    // Defrag follows Arcade in tt-toplike's `v` cycle; the monitor is `i`.
    ("defrag", "arcade", "v"),
    ("inference", "normal", "i"),
    ("hivemind", "hivemind", ""),
    ("training", "training", ""),
];

const MAX_SESSIONS: usize = 4;
static SESSIONS: AtomicUsize = AtomicUsize::new(0);

fn binary() -> Option<String> {
    std::env::var("TT_TOPLIKE_BIN")
        .ok()
        .or_else(|| ["/usr/bin/tt-toplike", "/usr/local/bin/tt-toplike"].iter().find(|p| std::path::Path::new(p).exists()).map(|p| p.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct TermQuery {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

/// `GET /toplike` (unauthed, like `/telemetry`): whether tt-toplike is installed and its views.
pub(crate) async fn get_toplike() -> axum::Json<Value> {
    axum::Json(serde_json::json!({
        "installed": binary().is_some(),
        "modes": MODES.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
    }))
}

/// `GET /toplike/term` (WebSocket; the token in a header, `routes::ws_token`).
pub(crate) async fn term_ws(ws: WebSocketUpgrade, State(state): State<AppState>, headers: axum::http::HeaderMap, Query(q): Query<TermQuery>) -> Response {
    if !crate::routes::ws_token(&headers).is_some_and(|t| state.is_valid_token(&t)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let ws = ws.protocols([crate::routes::WS_PROTOCOL]);
    let Some(bin) = binary() else {
        return (StatusCode::NOT_FOUND, "tt-toplike isn't installed on this box").into_response();
    };
    let wanted = q.mode.as_deref().unwrap_or("starfield");
    let Some(&(_, mode, keys)) = MODES.iter().find(|(name, _, _)| *name == wanted) else {
        return (StatusCode::BAD_REQUEST, "unknown view").into_response();
    };
    if SESSIONS.load(Ordering::SeqCst) >= MAX_SESSIONS {
        return (StatusCode::TOO_MANY_REQUESTS, "too many tt-toplike views open").into_response();
    }
    let cols = q.cols.unwrap_or(120).clamp(40, 320);
    let rows = q.rows.unwrap_or(40).clamp(12, 120);
    let port = state.ctrl_port();
    ws.on_upgrade(move |socket| async move {
        SESSIONS.fetch_add(1, Ordering::SeqCst);
        run(socket, bin, port, mode, keys, cols, rows).await;
        SESSIONS.fetch_sub(1, Ordering::SeqCst);
    })
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// A pseudo-terminal: (master, slave).
fn openpty(cols: u16, rows: u16) -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(cols, rows);
    let rc = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

async fn run(mut socket: WebSocket, bin: String, port: u16, mode: &'static str, keys: &'static str, cols: u16, rows: u16) {
    let (master, slave) = match openpty(cols, rows) {
        Ok(p) => p,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("couldn't open a terminal: {e}").into())).await;
            return;
        }
    };
    let stdio = |fd: &OwnedFd| fd.try_clone().map(Stdio::from);
    let (Ok(i), Ok(o), Ok(e)) = (stdio(&slave), stdio(&slave), stdio(&slave)) else { return };
    let mut cmd = Command::new(&bin);
    cmd.args(["--remote", &format!("127.0.0.1:{port}"), "--mode", mode, "-q"])
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .stdin(i)
        .stdout(o)
        .stderr(e);
    unsafe {
        cmd.pre_exec(|| {
            // Its own session, with the terminal as its controlling tty.
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = socket.send(Message::Text(format!("couldn't start tt-toplike: {e}").into())).await;
            return;
        }
    };
    drop(slave);
    let master_fd = master.as_raw_fd();
    let mut writer = File::from(master.try_clone().expect("dup pty"));
    let mut reader = File::from(master);

    // The screen, read on a thread (the pty is blocking), handed over in chunks.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    if !keys.is_empty() {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let _ = writer.write_all(keys.as_bytes());
    }

    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(bytes) => {
                    if socket.send(Message::Binary(bytes.into())).await.is_err() { break; }
                }
                None => break, // tt-toplike exited
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                    match v["type"].as_str() {
                        Some("input") => {
                            if let Some(data) = v["data"].as_str() {
                                let _ = writer.write_all(data.as_bytes());
                            }
                        }
                        Some("resize") => {
                            let c = v["cols"].as_u64().unwrap_or(cols as u64).clamp(40, 320) as u16;
                            let r = v["rows"].as_u64().unwrap_or(rows as u64).clamp(12, 120) as u16;
                            let ws = winsize(c, r);
                            unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws) };
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Binary(bytes))) => { let _ = writer.write_all(&bytes); }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = tokio::task::spawn_blocking(move || {
        for _ in 0..20 {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_map_to_real_modes() {
        let valid = ["normal", "starfield", "castle", "flow", "arcade", "hivemind", "training"];
        for (_, mode, _) in MODES {
            assert!(valid.contains(mode), "{mode}");
        }
    }
}
