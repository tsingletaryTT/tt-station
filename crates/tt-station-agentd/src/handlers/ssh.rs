// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! SSH access to the box: `POST`/`DELETE /ssh/authorize`, `GET /ssh/info`
//! and the public `GET /ssh/host-keys`.

use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::auth::{BearerAuth, OwnerAuth};
use crate::authkeys;
use crate::error::{backend_error, bad_request, ErrorResponse};
use crate::state::AppState;

/// JSON body accepted by `POST /ssh/authorize`.
#[derive(Deserialize)]
pub(crate) struct SshAuthorizeRequest {
    public_key: String,
    label: String,
}

/// JSON body returned by `POST /ssh/authorize` on success.
#[derive(Serialize)]
pub(crate) struct SshAuthorizeResponse {
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
pub(crate) async fn ssh_authorize(
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

/// The SSH server's host public keys in `dir` (`ssh_host_*_key.pub`), each
/// as `"<type> <base64>"` with the comment dropped, sorted by file name.
/// Lines that don't look like a key are skipped; no files is an empty list.
pub fn host_keys_in(dir: &std::path::Path) -> Vec<String> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("ssh_host_") && n.ends_with("_key.pub"))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            let mut words = text.split_whitespace();
            let (kind, b64) = (words.next()?, words.next()?);
            let kind_ok = !kind.is_empty()
                && kind
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '@'));
            let b64_ok = b64.len() >= 16
                && b64
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='));
            (kind_ok && b64_ok).then(|| format!("{kind} {b64}"))
        })
        .collect()
}

/// `GET /ssh/host-keys` (public): `{"keys": ["ssh-ed25519 AAAA...", ...]}`,
/// this box's SSH host keys from `/etc/ssh`. The desktop app reads them over
/// the box's pinned HTTPS and hands them to `ssh` as known hosts, so it never
/// has to trust whichever key answers first before it sends a password.
/// Public keys only, so no token: the app needs them before it's signed in.
pub(crate) async fn ssh_host_keys() -> Json<serde_json::Value> {
    let keys = tokio::task::spawn_blocking(|| host_keys_in(std::path::Path::new("/etc/ssh")))
        .await
        .unwrap_or_default();
    Json(serde_json::json!({ "keys": keys }))
}

/// `GET /ssh/info` (authed): how an SSH app (Termius, a terminal) reaches this
/// box -- the account, port, addresses, whether the SSH server is listening,
/// the keys TT Station has added, and the standard `~/.ssh/config` block that
/// SSH apps import (Termius imports `~/.ssh` on the desktop).
pub(crate) async fn ssh_info(
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
pub(crate) struct SshRevokeRequest {
    label: Option<String>,
    public_key: Option<String>,
}

/// JSON body returned by `DELETE /ssh/authorize` on success.
#[derive(Serialize)]
pub(crate) struct SshRevokeResponse {
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
pub(crate) async fn ssh_revoke(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_keys_are_type_and_key_without_the_comment() {
        let dir = tempfile::tempdir().unwrap();
        let w = |n: &str, body: &str| std::fs::write(dir.path().join(n), body).unwrap();
        w(
            "ssh_host_ed25519_key.pub",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHVuaXQ root@qb2-lab\n",
        );
        w("ssh_host_rsa_key.pub", "ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB\n");
        w("ssh_host_ed25519_key", "PRIVATE KEY, never read");
        w("ssh_host_bad_key.pub", "not a key line at all; nope\n");
        w("other.pub", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOTHER\n");
        assert_eq!(
            host_keys_in(dir.path()),
            vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHVuaXQ".to_string(),
                "ssh-rsa AAAAB3NzaC1yc2EAAAADAQAB".to_string(),
            ]
        );
        assert!(host_keys_in(&dir.path().join("missing")).is_empty());
    }
}
