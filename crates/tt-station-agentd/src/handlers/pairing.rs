//! Pairing a phone or Mac with the box: `POST /pair/init`, `/pair/offer`,
//! `/pair/complete`, and the `POST /pair/qr` code the box shows.

use axum::{http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::auth::LocalCaller;
use crate::error::{tt_error, ErrorResponse};
use crate::pairing;
use crate::routes::qr_svg;
use crate::state::{unix_now, AppState, PAIR_TTL};

/// JSON body returned by `POST /pair/init`.
#[derive(Serialize)]
pub(crate) struct PairInitResponse {
    pair_id: String,
}

/// `POST /pair/init` starts a pairing attempt: mint a `pair_id` and a
/// 6-digit code, print the code the way a box's screen would display it,
/// remember the pair for `PAIR_TTL`, and hand the `pair_id` back so the
/// client can present it (with the code the human read off the screen) to
/// `/pair/complete`.
pub(crate) async fn pair_init(
    axum::extract::State(state): axum::extract::State<AppState>,
    peer: Option<axum::extract::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    body: axum::body::Bytes,
) -> Result<Json<PairInitResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Anyone on the network can ask, and each ask notifies the owner: a few a
    // minute from any one address, and a ceiling for all of them together.
    let peer =
        peer.map(|axum::extract::Extension(axum::extract::ConnectInfo(a))| a.ip().to_canonical());
    if !state.pair_init_allowed(peer, unix_now()) {
        return Err(tt_error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pairing requests in the last minute. Wait a minute and try again.",
        ));
    }
    let pair_id = pairing::issue_pair_id();
    let code = pairing::issue_code();
    // `{"device": "Jane's MacBook"}`: who's asking, for the person reading the code.
    let device: String = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v["device"]
                .as_str()
                .map(|d| d.chars().filter(|c| !c.is_control()).take(60).collect())
        })
        .unwrap_or_else(|| "a device".to_string());

    // The box has no screen of its own: the code goes where its owner reads
    // things -- the desktop app and the phone (as an event). Never the
    // journal, which anyone who can read the box's logs could use.
    println!("tt-station-agentd: pairing requested by {device}");
    let fp = state
        .tls()
        .map(|t| t.sha256.chars().take(8).collect::<String>())
        .unwrap_or_default();
    state.notifier().emit(crate::notify::Draft {
        kind: "pairing",
        title: format!("Pairing code for {device}: {} {}", &code[..3.min(code.len())], &code[3.min(code.len())..]),
        body: if fp.is_empty() {
            "Type it on that device. If you didn't start pairing, ignore this.".to_string()
        } else {
            format!("Type it on that device; it should show this box as {} {}. If you didn't start pairing, ignore this.", &fp[..4], &fp[4..])
        },
    });

    state.insert_pending_pair(pair_id.clone(), code);

    Ok(Json(PairInitResponse { pair_id }))
}

/// `POST /pair/offer`: like `/pair/init`, but hands the code back too -- for
/// `tt-station-agentd pair`, which shows it as a QR. Only for callers holding
/// the box user's local secret (`crate::pairqr`); anyone else gets 403.
pub(crate) async fn pair_offer(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let presented = headers
        .get(crate::pairqr::SECRET_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let allowed = state
        .local_secret()
        .is_some_and(|secret| crate::pairqr::secrets_match(secret, presented));
    if !allowed {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "Run `tt-station-agentd pair` on the box, as the user the agent runs as.",
        ));
    }
    let pair_id = pairing::issue_pair_id();
    let code = pairing::issue_code();
    // Only the box's own user holds the secret: what pairs with it is an owner.
    state.insert_pending_pair_as(pair_id.clone(), code.clone(), true);
    Ok(Json(serde_json::json!({
        "pair_id": pair_id,
        "code": code,
        "expires_in_s": PAIR_TTL.as_secs(),
        "tls": state.tls(),
        "name": state.name(),
    })))
}

/// JSON body accepted by `POST /pair/complete`.
///
/// `name`/`platform` say what's pairing (`"Jane's MacBook"`, `"macOS"`), for
/// the device list; both are optional, so every older client still pairs and
/// gets an honest default name. `label` is what earlier desktop apps sent
/// for the same purpose, used when `name` isn't given.
#[derive(Deserialize)]
pub(crate) struct PairCompleteRequest {
    pair_id: String,
    code: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    platform: Option<String>,
    #[serde(default)]
    label: Option<String>,
}

/// JSON body returned by `POST /pair/complete` on success.
#[derive(Serialize)]
pub(crate) struct PairCompleteResponse {
    token: String,
    /// This device's id in `GET /devices`.
    device_id: String,
    /// Where this box serves HTTPS and the certificate to pin (`crate::tls`).
    #[serde(skip_serializing_if = "Option::is_none")]
    tls: Option<crate::tls::TlsInfo>,
}

/// `POST /pair/complete` finishes a pairing attempt: if `code` matches the
/// still-unexpired code for `pair_id`, mint a bearer token, record a device
/// session for it (only the token's digest is kept), and return the token.
/// Otherwise (unknown pair_id, expired, or wrong code) respond `401
/// Unauthorized` without minting anything -- the caller can't tell *why* it
/// failed, which avoids leaking whether a given pair_id ever existed.
///
/// A successful pairing is an event (and a push to paired phones), so the
/// owner hears about every new device that can control the box.
pub(crate) async fn pair_complete(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(req): Json<PairCompleteRequest>,
) -> Result<Json<PairCompleteResponse>, StatusCode> {
    let Some(owner) = state.complete_pair_as(&req.pair_id, &req.code) else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let token = pairing::issue_token();
    let name = req
        .name
        .as_deref()
        .filter(|n| !n.trim().is_empty())
        .or(req.label.as_deref());
    let session = state
        .sessions()
        .pair(&token, name, req.platform.as_deref(), owner);
    let actor = crate::audit::Actor {
        device_id: Some(session.id.clone()),
        device_name: session.name.clone(),
    };
    crate::audit::record_as(
        &state,
        &actor,
        "pair",
        if session.owner { "owner" } else { "member" },
        "ok",
    );
    let what = match &session.platform {
        Some(p) => format!("{} ({p})", session.name),
        None => session.name.clone(),
    };
    state.notifier().emit(crate::notify::Draft {
        kind: "devices",
        title: format!("New device paired: {what}"),
        body: if session.owner {
            "It's an owner: it can use this box and manage it. If you don't recognize it, remove it in Settings → Pairing and security → Devices.".to_string()
        } else {
            "It's a member: it can use this box, but not manage it. If you don't recognize it, remove it in Settings → Pairing and security → Devices.".to_string()
        },
    });

    Ok(Json(PairCompleteResponse {
        token,
        device_id: session.id,
        tls: state.tls(),
    }))
}

/// `POST /pair/qr` (the box itself): a one-time pairing link and its QR, for
/// an iPhone's camera. It carries the box's address on the home network, the
/// HTTPS port and the certificate fingerprint the phone pins, and the account
/// the phone's SSH key is added to. Nothing in it is a credential that lasts:
/// the code expires and works once.
pub(crate) async fn pair_qr(
    axum::extract::State(state): axum::extract::State<AppState>,
    caller: Option<axum::extract::Extension<LocalCaller>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    if caller.is_none() {
        return Err(tt_error(
            StatusCode::FORBIDDEN,
            "Pair a phone from TT Station on the box itself.",
        ));
    }
    let Some(tls) = state.tls() else {
        return Err(tt_error(
            StatusCode::CONFLICT,
            "HTTPS isn't running on this box, so a phone can't connect securely.",
        ));
    };
    let (port, sha) = (tls.port, tls.sha256.as_str());
    let pair_id = pairing::issue_pair_id();
    let code = pairing::issue_code();
    state.insert_pending_pair(pair_id.clone(), code.clone());
    let name = state.name().to_string();
    let host = tokio::task::spawn_blocking(crate::pairqr::lan_ipv4)
        .await
        .ok()
        .flatten()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| format!("{name}.local"));
    let link = crate::pairqr::PairLink {
        name: name.clone(),
        host: host.clone(),
        port: state.ctrl_port(),
        pair_id,
        code,
        remote_host: None,
        tls_port: Some(port),
        cert_sha256: Some(sha.to_string()),
    };
    let url = format!(
        "{}&ssh={}",
        link.url(),
        crate::pairqr::encode_component(state.ssh_user())
    );
    Ok(Json(serde_json::json!({
        "link": url,
        "qr_svg": qr_svg(&url),
        "expires_in_s": PAIR_TTL.as_secs(),
        "host": host, "https_port": port,
        // The first 16 hex digits, to compare by eye on the phone if asked.
        "fingerprint": sha.chars().take(16).collect::<String>(),
    })))
}
