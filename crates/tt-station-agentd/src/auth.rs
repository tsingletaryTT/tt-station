// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Who may call what: the bearer-token extractors (`BearerAuth`,
//! `OwnerAuth`, `CallerAuth`, `Who`, `LocalOrPaired`, `ThisDevice`, `PlainHttp`),
//! the route allow-lists (`PUBLIC_ROUTES`, `HANDLER_AUTHED_ROUTES`) and the
//! layers that enforce them (`require_auth`, `only_local`, `known_host`).

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    response::Response,
    Json,
};

use crate::error::{tt_error, ErrorResponse};
use crate::people::Person;
use crate::state::AppState;

/// The bearer token on a request, if it carries one.
fn bearer(parts: &Parts) -> Option<&str> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// Why an authed route turned a request away: always `401`, with a plain
/// `{"error": ..}` saying what to do (pair again). A missing token, a
/// malformed header, and a token that was never issued or was removed all
/// read the same, so the answer says nothing about which tokens exist; only
/// a token that really was paired, and went unused too long, hears that it
/// expired.
#[derive(Debug)]
pub struct AuthRejection(crate::sessions::AuthFailure);

impl axum::response::IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let mut res = (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: self.0.message(),
            }),
        )
            .into_response();
        res.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        res
    }
}

/// Check the request's bearer token, returning the caller's device id.
fn authenticate_parts(parts: &Parts, state: &AppState) -> Result<String, AuthRejection> {
    let token = bearer(parts).ok_or(AuthRejection(crate::sessions::AuthFailure::Unknown))?;
    state.authenticate(token).map_err(AuthRejection)
}

/// Extractor guarding the control routes (`/run`, `/stop`, `/endpoint`):
/// requires `Authorization: Bearer <token>` where `<token>` belongs to a
/// live paired device (see `crate::sessions`). Anything else is a `401`
/// ([`AuthRejection`]).
///
/// Implemented as a real extractor (rather than an inline check duplicated
/// in each handler) so adding it to a route is just adding it to the
/// handler's argument list, and so it composes with axum's normal extractor
/// ordering instead of needing a separate middleware layer wired up per
/// route.
pub(crate) struct BearerAuth;

impl FromRequestParts<AppState> for BearerAuth {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, AuthRejection> {
        authenticate_parts(parts, state).map(|_| BearerAuth)
    }
}

/// [`BearerAuth`], keeping which paired device is calling (`GET /devices`
/// marks it; `DELETE /devices/{id}` knows when a device signs itself out).
pub(crate) struct ThisDevice(pub String);

impl FromRequestParts<AppState> for ThisDevice {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, AuthRejection> {
        authenticate_parts(parts, state).map(ThisDevice)
    }
}

/// How the box closes a WebSocket whose device was signed out: 1008
/// (policy violation), with the reason a person would want.
pub(crate) fn signed_out_frame() -> axum::extract::ws::CloseFrame {
    axum::extract::ws::CloseFrame {
        code: axum::extract::ws::close_code::POLICY,
        reason: "This device was signed out of the box.".into(),
    }
}

/// The subprotocol a WebSocket from the desktop app asks for; it carries
/// its token as a second one, `tt-token.<token>` (see [`ws_token`]).
pub const WS_PROTOCOL: &str = "tt-station";

/// The bearer token a WebSocket upgrade carries: the `Authorization` header
/// (the phone sets it), or -- a browser can't set headers on a WebSocket --
/// a `tt-token.<token>` entry in `Sec-WebSocket-Protocol` (the desktop app).
/// Never the URL: URLs end up in logs and process lists.
pub(crate) fn ws_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(t) = bearer {
        return Some(t.to_string());
    }
    headers
        .get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|p| p.trim().strip_prefix("tt-token.").map(str::to_string))
}

/// The box itself (plain HTTP from this machine, see `only_local`) or a
/// paired caller: for what the box's own tools (`tt-station console`, the
/// panel) read without pairing, but the network may not.
pub(crate) struct LocalOrPaired;

impl FromRequestParts<AppState> for LocalOrPaired {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, AuthRejection> {
        if parts.extensions.get::<LocalCaller>().is_some() {
            return Ok(LocalOrPaired);
        }
        BearerAuth::from_request_parts(parts, state)
            .await
            .map(|_| LocalOrPaired)
    }
}

/// What a member hears when it asks for something only an owner may do.
pub(crate) const OWNER_ONLY: &str =
    "Only an owner of this box can do that. An owner can make this device one in Settings → Pairing and security → Devices.";

/// Who is making a request: the box's own user (plain HTTP from its own
/// account, see `only_local`), a paired device, or both (the desktop app on
/// the box, which also sends its token).
#[derive(Debug, Clone)]
pub struct Caller {
    pub local: bool,
    pub device: Option<crate::sessions::Session>,
}

impl Caller {
    /// Owners manage the box. The box's own user is one: it can read the
    /// local secret that pairs as an owner anyway.
    pub fn owner(&self) -> bool {
        self.local || self.device.as_ref().is_some_and(|d| d.owner)
    }

    /// An owner by its paired device's token alone, local trust aside: who
    /// may answer what a program on the box asks permission for.
    pub fn paired_owner(&self) -> bool {
        self.device.as_ref().is_some_and(|d| d.owner)
    }

    pub fn device_id(&self) -> Option<String> {
        self.device.as_ref().map(|d| d.id.clone())
    }

    pub fn actor(&self) -> crate::audit::Actor {
        match &self.device {
            Some(d) => crate::audit::Actor {
                device_id: Some(d.id.clone()),
                device_name: d.name.clone(),
            },
            None => crate::audit::Actor::the_box(),
        }
    }
}

/// The caller of a request with these headers, without counting a use: a
/// live device token (header or WebSocket subprotocol, [`ws_token`]) and/or
/// the box itself. When it's neither, the error says why the token (if any)
/// was turned away.
pub(crate) fn resolve_caller(
    headers: &axum::http::HeaderMap,
    local: bool,
    state: &AppState,
) -> Result<Caller, crate::sessions::AuthFailure> {
    let token = ws_token(headers);
    let device = token
        .as_deref()
        .and_then(|t| state.sessions().session_for(t));
    if device.is_none() && !local {
        let known = token
            .as_deref()
            .is_some_and(|t| state.sessions().id_for(t).is_some());
        return Err(if known {
            crate::sessions::AuthFailure::Expired
        } else {
            crate::sessions::AuthFailure::Unknown
        });
    }
    Ok(Caller { local, device })
}

tokio::task_local! {
    /// The caller of the request being handled (set by [`require_auth`], next
    /// to `crate::audit::ACTOR`), for code a route reaches that has no
    /// extractor of its own: the assistant's tools, a routine's approvals.
    static CALLER: Caller;
}

/// Who is making the request being handled; `None` outside a request (the
/// box's own background work: schedules, the boot restart, queues).
pub(crate) fn current_caller() -> Option<Caller> {
    CALLER.try_with(Clone::clone).ok()
}

/// Runs `f` as `caller`'s request would (tests, and work done on a person's behalf).
pub async fn as_caller<F: std::future::Future>(caller: Caller, f: F) -> F::Output {
    CALLER.scope(caller, f).await
}

/// [`LocalOrPaired`], keeping who the caller is.
pub(crate) struct CallerAuth(pub Caller);

impl FromRequestParts<AppState> for CallerAuth {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, AuthRejection> {
        let local = parts.extensions.get::<LocalCaller>().is_some();
        resolve_caller(&parts.headers, local, state)
            .map(CallerAuth)
            .map_err(AuthRejection)
    }
}

/// Extractor for what only an owner may do (`crate::sessions`): manage other
/// devices, SSH and app keys, power, updates, reset and security settings.
/// A member gets `403` with [`OWNER_ONLY`], and the refusal is logged
/// (`crate::audit`); no credentials at all is the usual `401`.
pub(crate) struct OwnerAuth;

impl FromRequestParts<AppState> for OwnerAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        use axum::response::IntoResponse;
        let CallerAuth(caller) = CallerAuth::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if caller.owner() {
            return Ok(OwnerAuth);
        }
        let what = format!("{} {}", parts.method, parts.uri.path());
        crate::audit::record_as(state, &caller.actor(), "denied", &what, "denied");
        Err(tt_error(StatusCode::FORBIDDEN, OWNER_ONLY).into_response())
    }
}

/// An owner's *paired device*, by its bearer token: for what a program on
/// the box must never decide for itself, answering a bring-up's approvals.
/// The box's own local trust doesn't count here (a bring-up runs as the box's
/// account and reaches plain HTTP too): no token is `401`, a member's is `403`
/// with [`OWNER_ONLY`], logged like [`OwnerAuth`]'s refusals.
pub(crate) struct DeviceOwnerAuth;

impl FromRequestParts<AppState> for DeviceOwnerAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        use axum::response::IntoResponse;
        // A live token (this counts its use), then who it belongs to.
        ThisDevice::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        let caller = resolve_caller(&parts.headers, false, state)
            .map_err(|f| AuthRejection(f).into_response())?;
        if caller.paired_owner() {
            return Ok(DeviceOwnerAuth);
        }
        let what = format!("{} {}", parts.method, parts.uri.path());
        crate::audit::record_as(state, &caller.actor(), "denied", &what, "denied");
        Err(tt_error(StatusCode::FORBIDDEN, OWNER_ONLY).into_response())
    }
}

/// Requests served over plain HTTP (`local_http_app`) carry this; HTTPS ones
/// don't. The model API only takes app keys there (`crate::gateway`).
#[derive(Clone, Copy, Debug)]
pub struct PlainHttp;

/// Routes anyone may call without pairing: what a device needs to find the
/// box and pair with it, and the app's own pages (no data in them).
/// `(method, route)`; a GET route answers HEAD too.
pub const PUBLIC_ROUTES: &[(&str, &str)] = &[
    ("GET", "/status"),
    // Up or not, nothing more (`crate::health`): for supervisors and load balancers.
    ("GET", "/healthz"),
    ("GET", "/tls"),
    ("POST", "/pair/init"),
    ("POST", "/pair/complete"),
    // Checks the local secret itself.
    ("POST", "/pair/offer"),
    ("GET", "/app"),
    ("GET", "/app/"),
    ("GET", "/app/{*rel}"),
];

/// Routes whose handler checks a credential of its own that isn't a device
/// token: app keys on the model API (`crate::gateway::GatewayAuth`) and a
/// bring-up run's secret for its driver (`crate::bringup`). The auth layer
/// lets them through to that check; without it they still answer `401`.
pub const HANDLER_AUTHED_ROUTES: &[&str] = &[
    "/v1/models",
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/embeddings",
    "/v1/rerank",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/v1/images/generations",
    "/bringup/codex/runs/{id}/approvals",
    "/bringup/codex/runs/{id}/approvals/{aid}/wait",
];

/// Whether `method route` is on [`PUBLIC_ROUTES`].
pub fn is_public_route(method: &axum::http::Method, route: &str) -> bool {
    let m = if method == axum::http::Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    PUBLIC_ROUTES
        .iter()
        .any(|(pm, pr)| *pm == m && *pr == route)
}

/// Default-deny: every route needs a paired device or the box itself unless
/// it's on [`PUBLIC_ROUTES`] or [`HANDLER_AUTHED_ROUTES`]. Each handler's own
/// extractor still applies on top (a route that wants a device token still
/// refuses the box's own user without one). Runs the handler with its caller
/// in [`crate::audit::ACTOR`], for the activity log.
pub(crate) async fn require_auth(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::response::IntoResponse;
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let local = req.extensions().get::<LocalCaller>().is_some();
    let caller = resolve_caller(req.headers(), local, &state);
    let open =
        is_public_route(req.method(), &route) || HANDLER_AUTHED_ROUTES.contains(&route.as_str());
    match caller {
        Ok(c) => {
            let actor = c.actor();
            CALLER
                .scope(c, crate::audit::ACTOR.scope(actor, next.run(req)))
                .await
        }
        Err(_) if open => {
            let anyone = crate::audit::Actor {
                device_id: None,
                device_name: "Not paired".to_string(),
            };
            let nobody = Caller {
                local: false,
                device: None,
            };
            CALLER
                .scope(nobody, crate::audit::ACTOR.scope(anyone, next.run(req)))
                .await
        }
        Err(failure) => AuthRejection(failure).into_response(),
    }
}

/// Build the router for a given `AppState`. Side-effect-free: no sockets,
/// no mDNS -- safe to call directly from tests.
/// A caller on the box itself (or one of its Docker apps), reaching plain HTTP.
#[derive(Clone, Copy, Debug)]
pub struct LocalCaller;

/// Callers plain HTTP serves: the box itself and its Docker apps (bridge
/// networks are in 172.16.0.0/12). Everyone else -- phones, other computers --
/// must use HTTPS, where the box's certificate is pinned.
pub fn local_peer(ip: std::net::IpAddr) -> bool {
    if own_addresses().contains(&ip) {
        return true; // the box calling its own network address
    }
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| local_peer(std::net::IpAddr::V4(v4)))
        }
    }
}

/// A Docker container on this box (its networks are in 172.16.0.0/12). Apps
/// in containers may use the model API (`/v1`) over plain HTTP and nothing
/// else: code running in a container (an agent's sandbox, an app that runs
/// code) must never pair itself or control the box.
pub fn container_peer(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.octets()[0] == 172 && (16..32).contains(&v4.octets()[1]),
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .is_some_and(|v4| container_peer(std::net::IpAddr::V4(v4))),
    }
}

/// This box's own addresses (refreshed every minute: DHCP can change them).
fn own_addresses() -> Vec<std::net::IpAddr> {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<std::net::IpAddr>)>> =
        std::sync::Mutex::new(None);
    let mut c = CACHE.lock().expect("own addresses");
    if let Some((at, ips)) = c.as_ref() {
        if at.elapsed() < std::time::Duration::from_secs(60) {
            return ips.clone();
        }
    }
    let out = std::process::Command::new("hostname").arg("-I").output();
    let ips: Vec<std::net::IpAddr> = out
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .filter_map(|w| w.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    *c = Some((std::time::Instant::now(), ips.clone()));
    ips
}

/// `/bringup/codex/runs/{id}/approvals` and `…/approvals/{aid}/wait`: what a
/// bring-up's driver calls (with its run secret) from inside its sandbox.
pub fn is_bringup_driver_path(path: &str) -> bool {
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    match seg.as_slice() {
        ["bringup", "codex", "runs", id, "approvals"] => plain(id),
        ["bringup", "codex", "runs", id, "approvals", aid, "wait"] => plain(id) && plain(aid),
        _ => false,
    }
}

/// Whether the owner turned on plain HTTP for other computers on the network
/// (for apps that can't use the box's own certificate). Off by default.
pub fn lan_http_allowed(state: &AppState) -> bool {
    state.config_dir().join("allow-lan-http").exists()
}

pub(crate) async fn only_local(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    req.extensions_mut().insert(PlainHttp);
    // Trusted as the box itself only when the program on the other end runs
    // as the agent's own account (`crate::localpeer`): not another user on
    // the box, not root, not a container on the host network -- and not a
    // bring-up, Orchard or Code run, which run as that account too.
    let who = if local_peer(peer.ip()) {
        tokio::task::spawn_blocking(move || crate::localpeer::peer_trust(peer))
            .await
            .unwrap_or(crate::localpeer::Peer::Other)
    } else {
        crate::localpeer::Peer::Other
    };
    match who {
        crate::localpeer::Peer::Me => {
            req.extensions_mut().insert(LocalCaller);
            return next.run(req).await;
        }
        crate::localpeer::Peer::Confined(unit) => {
            // The model API (with an app key) and a bring-up driver's own
            // routes (with its run secret); nothing else.
            let path = req.uri().path();
            if path.starts_with("/v1/") || is_bringup_driver_path(path) {
                return next.run(req).await;
            }
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": format!(
                        "Programs in a bring-up, Orchard or Code run ({unit}) aren't the box's owner: they use the model API (/v1) with an app key, and a bring-up's driver its run secret."
                    ),
                })),
            )
                .into_response();
        }
        crate::localpeer::Peer::Other => {}
    }
    if (container_peer(peer.ip()) || local_peer(peer.ip())) && req.uri().path().starts_with("/v1/")
    {
        return next.run(req).await;
    }
    if local_peer(peer.ip()) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": format!(
                    "Only TT Station's own account on this box can use this connection. Other programs use the model API (/v1) with an app key, or pair over https on port {}.",
                    state.ctrl_port() + 1
                ),
                "https_port": state.ctrl_port() + 1,
            })),
        )
            .into_response();
    }
    // Phones never get plain HTTP; other computers only if the owner allowed it.
    if lan_http_allowed(&state) && req.uri().path().starts_with("/v1/") {
        return next.run(req).await;
    }
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({
            "error": format!(
                "This box only takes encrypted connections from the network. Use https on port {} (pair your phone again from TT Station on the box).",
                state.ctrl_port() + 1
            ),
            "https_port": state.ctrl_port() + 1,
        })),
    )
        .into_response()
}

/// A web page in a browser on the box can't use the plain-HTTP API through a
/// name of its own that it points at 127.0.0.1 (DNS rebinding): requests must
/// name the box as the box knows itself. (HTTPS needs no such check: a
/// browser won't talk to the box's own certificate under another name.)
pub(crate) async fn known_host(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let host = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|h| h.to_str().ok());
    match host {
        Some(h) if !host_is_the_box(h, state.name()) => (
            StatusCode::MISDIRECTED_REQUEST,
            Json(serde_json::json!({ "error": format!("This box doesn't answer to {h}.") })),
        )
            .into_response(),
        _ => next.run(req).await,
    }
}

/// Whether a `Host` header names this box: loopback, its own addresses, its
/// names (`<name>.local`, its hostname), or Docker's name for it.
pub fn host_is_the_box(host: &str, name: &str) -> bool {
    // Drop the port: "[::1]:8765" -> "::1", "box.local:8765" -> "box.local".
    let h = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host
            .rsplit_once(':')
            .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
            .map_or(host, |(h, _)| h),
    };
    let h = h.trim_end_matches('.').to_ascii_lowercase();
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return ip.is_loopback()
            || own_addresses().contains(&ip)
            || ip.to_canonical().is_loopback();
    }
    let me = name.to_ascii_lowercase();
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "host.docker.internal"
        || (!me.is_empty() && (h == me || h == format!("{me}.local")))
        || (!hostname.is_empty() && (h == hostname || h == format!("{hostname}.local")))
}

/// An authenticated request, with its token and the person it belongs to.
pub struct Who {
    pub token: String,
    pub person: Option<Person>,
}

impl FromRequestParts<AppState> for Who {
    type Rejection = AuthRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        BearerAuth::from_request_parts(parts, state).await?;
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string();
        let person = state.people().for_token(&token);
        Ok(Who { token, person })
    }
}

#[cfg(test)]
mod local_http_tests {
    use super::{container_peer, host_is_the_box, local_peer};

    #[test]
    fn plain_http_is_for_the_box_and_its_containers_get_only_v1() {
        assert!(local_peer("127.0.0.1".parse().unwrap()));
        assert!(local_peer("::1".parse().unwrap()));
        // Containers are not the box: they get the model API only (see `container_peer`).
        assert!(!local_peer("172.17.0.5".parse().unwrap()));
        assert!(container_peer("172.17.0.5".parse().unwrap()));
        assert!(container_peer("172.31.255.1".parse().unwrap()));
        assert!(!container_peer("172.32.0.1".parse().unwrap()));
        assert!(!local_peer("172.32.0.1".parse().unwrap()));
        assert!(!local_peer("192.168.1.20".parse().unwrap()));
        assert!(!local_peer("100.100.1.2".parse().unwrap()));
    }

    #[test]
    fn a_confined_run_reaches_only_its_drivers_routes() {
        use super::is_bringup_driver_path as d;
        assert!(d("/bringup/codex/runs/abc123/approvals"));
        assert!(d("/bringup/codex/runs/abc123/approvals/a1b2/wait"));
        // Answering one is an owner's paired device's.
        assert!(!d("/bringup/codex/runs/abc123/approvals/a1b2"));
        assert!(!d("/bringup/codex/runs/abc123/stop"));
        assert!(!d("/bringup/codex/runs"));
        assert!(!d("/bringup/codex/runs//approvals"));
        assert!(!d("/bringup/codex/runs/../approvals"));
        assert!(!d("/pair/offer"));
        assert!(!d("/bringup/codex/runs/a/approvals/b/wait/x"));
    }

    #[test]
    fn the_plain_http_api_answers_only_to_the_boxs_own_names() {
        for h in [
            "127.0.0.1:8765",
            "localhost:8765",
            "localhost",
            "[::1]:8765",
            "app.localhost:8765",
            "host.docker.internal:8765",
            "qb2-lab.local:8765",
            "QB2-LAB:8765",
            "127.0.0.1",
        ] {
            assert!(host_is_the_box(h, "qb2-lab"), "{h}");
        }
        for h in [
            "evil.example:8765",
            "evil.example",
            "127.0.0.1.evil.example:8765",
            "qb2-lab.local.evil.example",
            "localhost.evil.example:8765",
            "8.8.8.8:8765",
            "[2606:4700::1111]:80",
        ] {
            assert!(!host_is_the_box(h, "qb2-lab"), "{h}");
        }
    }
}
