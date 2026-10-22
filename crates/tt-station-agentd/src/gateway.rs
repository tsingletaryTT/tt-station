// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Your box as a private OpenAI-compatible API: one stable endpoint,
//! `http://<box>:<ctrl-port>/v1`, for the apps on your laptop and phone
//! (Open WebUI, Continue, LangChain, the `openai` SDK, curl...).
//!
//! It follows whatever the box is doing: chat and completions go to the served
//! chat model, `/v1/images/generations` to the image model -- so a client never chases ports or
//! model ids. The `model` a client sends is rewritten to the one actually
//! serving, so tools that insist on a model name just work.
//!
//! Access needs an *app key* (`ttq-...`), minted and revoked by an owner from
//! a paired phone or the desktop app (`/keys`), or -- over HTTPS only -- a
//! paired device's own token. Plain HTTP (the box itself, its containers, and
//! other computers when the owner allows it) takes app keys only: a device
//! token there could be read off the wire, and it opens the whole box, not
//! just the model API.
//!
//! Keys live in `app-keys.json` (mode 0600) in the agent's config directory,
//! as SHA-256 digests: a key is shown once, when it's made, and the file
//! alone can't be used to call the box. (Files from before kept the keys
//! themselves; they're converted on load.) A key is 256 random bits, and can
//! be made to expire.
//!
//! For a team sharing one box, each key can have limits -- requests a minute,
//! tokens a day -- and a list of what it may use (chat, images, embeddings),
//! and can be paused. Every key's tokens are counted per day (the model
//! server's own counts where it gives them, otherwise an estimate), so
//! `/keys` shows who uses the box how much.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse};

/// At most this many keys; more is a mistake, not a use case.
const MAX_KEYS: usize = 32;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppKey {
    id: String,
    name: String,
    /// The key itself, in files from before keys were hashed: read once, to
    /// convert, and never written.
    #[serde(default, skip_serializing)]
    key: Option<String>,
    /// Lower-case hex SHA-256 of the key.
    #[serde(default)]
    key_sha256: String,
    /// The key's first characters (`ttq-1a2b`), so a person can tell keys apart.
    #[serde(default)]
    hint: String,
    created: u64,
    /// Unix seconds after which the key stops working; `None` never.
    #[serde(default)]
    expires: Option<u64>,
    #[serde(default)]
    last_used: Option<u64>,
    #[serde(default)]
    requests: u64,
    #[serde(default)]
    limits: Limits,
    /// What it may use ([`SCOPES`]); empty is everything.
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    paused: bool,
    /// Per local day (`YYYY-MM-DD`), the last [`USAGE_DAYS`].
    #[serde(default)]
    usage: std::collections::BTreeMap<String, Usage>,
}

/// What a key may do, at most. `None` is no limit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Limits {
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    #[serde(default)]
    pub tokens_per_day: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// What a key can be allowed to use.
pub const SCOPES: [&str; 3] = ["chat", "images", "embeddings"];
const USAGE_DAYS: usize = 31;

/// Which scope a `/v1` path needs.
pub fn scope_of(path: &str) -> &'static str {
    if path.starts_with("/v1/images") {
        "images"
    } else if path.starts_with("/v1/embeddings") || path.starts_with("/v1/rerank") {
        "embeddings"
    } else {
        "chat"
    }
}

/// Why a key was turned away.
#[derive(Debug, Clone, PartialEq)]
pub enum Denied {
    Unknown,
    Paused,
    Scope(&'static str),
    /// Seconds until a request would be let through.
    Rate(u64),
    Budget,
    /// Past its expiry.
    Expired,
}

/// The characters of a key shown in lists.
const HINT_LEN: usize = 8;

fn key_digest(key: &str) -> String {
    crate::sessions::token_digest(key)
}

/// Today in local time, `YYYY-MM-DD`.
fn day_of(unix: u64) -> String {
    let t = unix as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

/// A key as listed (never the secret).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KeySummary {
    pub id: String,
    pub name: String,
    /// The first characters, so a person can tell keys apart.
    pub hint: String,
    pub created: u64,
    pub expires: Option<u64>,
    pub last_used: Option<u64>,
    pub requests: u64,
    pub limits: Limits,
    pub scopes: Vec<String>,
    pub paused: bool,
    pub today: Usage,
    /// The last 14 days, oldest first: (day, usage).
    pub days: Vec<(String, Usage)>,
}

#[derive(Debug)]
pub struct AppKeys {
    path: Option<PathBuf>,
    keys: Mutex<Vec<AppKey>>,
    last_saved: Mutex<u64>,
    /// Each key's requests in the last minute (unix seconds), for its rate limit.
    recent: Mutex<std::collections::HashMap<String, std::collections::VecDeque<u64>>>,
}

/// Changes to a key's limits, scopes or pause (absent fields stay as they are).
#[derive(Debug, Default, Deserialize)]
pub struct KeyChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub limits: Option<Limits>,
    #[serde(default)]
    pub scopes: Option<Vec<String>>,
    #[serde(default)]
    pub paused: Option<bool>,
}

impl AppKeys {
    /// In memory only (tests, or no config dir).
    pub fn in_memory() -> Self {
        AppKeys {
            path: None,
            keys: Mutex::new(Vec::new()),
            last_saved: Mutex::new(0),
            recent: Mutex::new(Default::default()),
        }
    }

    /// Loaded from (and saved to) `path`; a missing file is no keys. One
    /// that won't parse is moved aside first (`crate::fsio`), so the next save
    /// can't overwrite the keys in it. Keys kept in the clear by older
    /// versions are hashed and the file rewritten.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut keys: Vec<AppKey> = crate::fsio::read_json_or_backup(&path).unwrap_or_default();
        let mut converted = 0;
        for k in &mut keys {
            if let Some(raw) = k.key.take() {
                k.key_sha256 = key_digest(&raw);
                k.hint = raw.chars().take(HINT_LEN).collect();
                converted += 1;
            }
        }
        let opened = AppKeys {
            path: Some(path),
            keys: Mutex::new(keys),
            last_saved: Mutex::new(0),
            recent: Mutex::new(Default::default()),
        };
        if converted > 0 {
            opened.save();
            eprintln!("tt-station-agentd: app keys are now stored hashed ({converted} converted)");
        }
        opened
    }

    fn summary(k: &AppKey) -> KeySummary {
        let now = unix_now();
        let days: Vec<(String, Usage)> = (0..14u64)
            .rev()
            .map(|d| {
                let day = day_of(now.saturating_sub(d * 86_400));
                let u = k.usage.get(&day).copied().unwrap_or_default();
                (day, u)
            })
            .collect();
        KeySummary {
            id: k.id.clone(),
            name: k.name.clone(),
            hint: format!("{}…", k.hint),
            created: k.created,
            expires: k.expires,
            last_used: k.last_used,
            requests: k.requests,
            limits: k.limits.clone(),
            scopes: k.scopes.clone(),
            paused: k.paused,
            today: k.usage.get(&day_of(now)).copied().unwrap_or_default(),
            days,
        }
    }

    pub fn list(&self) -> Vec<KeySummary> {
        self.keys
            .lock()
            .expect("keys mutex poisoned")
            .iter()
            .map(Self::summary)
            .collect()
    }

    /// Mint a key for `name`; the secret is returned only here.
    pub fn create(&self, name: &str, now: u64) -> Result<(KeySummary, String), String> {
        self.create_expiring(name, now, None)
    }

    /// [`Self::create`], stopping working at `expires` (unix seconds) if given.
    pub fn create_expiring(
        &self,
        name: &str,
        now: u64,
        expires: Option<u64>,
    ) -> Result<(KeySummary, String), String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 60 {
            return Err("Give the key a short name, like the app it's for.".to_string());
        }
        let mut keys = self.keys.lock().expect("keys mutex poisoned");
        if keys.len() >= MAX_KEYS {
            return Err(format!(
                "This box already has {MAX_KEYS} app keys; revoke one first."
            ));
        }
        // 256 random bits, like a device token.
        let secret = format!("ttq-{}", crate::pairing::issue_token());
        let key = AppKey {
            id: crate::pairing::issue_pair_id()[..12].to_string(),
            name: name.to_string(),
            key: None,
            key_sha256: key_digest(&secret),
            hint: secret.chars().take(HINT_LEN).collect(),
            created: now,
            expires,
            last_used: None,
            requests: 0,
            limits: Limits::default(),
            scopes: Vec::new(),
            paused: false,
            usage: Default::default(),
        };
        keys.push(key);
        let summary = Self::summary(keys.last().expect("just pushed"));
        drop(keys);
        self.save();
        Ok((summary, secret))
    }

    pub fn revoke(&self, id: &str) -> bool {
        let mut keys = self.keys.lock().expect("keys mutex poisoned");
        let before = keys.len();
        keys.retain(|k| k.id != id);
        let removed = keys.len() != before;
        drop(keys);
        if removed {
            self.save();
        }
        removed
    }

    /// Whether `presented` is a live key; counts the use.
    pub fn check(&self, presented: &str, now: u64) -> bool {
        self.admit(presented, now, "chat").is_ok()
    }

    /// Lets a request with `presented` through for `scope`, or says why not;
    /// counts the request. The key's id when it's let through.
    pub fn admit(&self, presented: &str, now: u64, scope: &str) -> Result<String, Denied> {
        let presented = key_digest(presented);
        let mut keys = self.keys.lock().expect("keys mutex poisoned");
        let Some(key) = keys
            .iter_mut()
            .find(|k| crate::pairqr::secrets_match(&k.key_sha256, &presented))
        else {
            return Err(Denied::Unknown);
        };
        if key.expires.is_some_and(|e| now > e) {
            return Err(Denied::Expired);
        }
        if key.paused {
            return Err(Denied::Paused);
        }
        if !key.scopes.is_empty() && !key.scopes.iter().any(|s| s == scope) {
            return Err(Denied::Scope(
                SCOPES
                    .iter()
                    .find(|s| **s == scope)
                    .copied()
                    .unwrap_or("chat"),
            ));
        }
        let today = day_of(now);
        if let Some(budget) = key.limits.tokens_per_day {
            if key.usage.get(&today).is_some_and(|u| u.tokens() >= budget) {
                return Err(Denied::Budget);
            }
        }
        if let Some(rpm) = key.limits.requests_per_minute.filter(|r| *r > 0) {
            let mut recent = self.recent.lock().expect("keys mutex poisoned");
            let times = recent.entry(key.id.clone()).or_default();
            while times.front().is_some_and(|t| now.saturating_sub(*t) >= 60) {
                times.pop_front();
            }
            if times.len() >= rpm as usize {
                let wait = 60 - now.saturating_sub(*times.front().unwrap_or(&now));
                return Err(Denied::Rate(wait.max(1)));
            }
            times.push_back(now);
        }
        key.last_used = Some(now);
        key.requests += 1;
        key.usage.entry(today).or_default().requests += 1;
        while key.usage.len() > USAGE_DAYS {
            let oldest = key.usage.keys().next().cloned().unwrap_or_default();
            key.usage.remove(&oldest);
        }
        let id = key.id.clone();
        drop(keys);
        // Usage counters are worth keeping, not worth a disk write per request.
        let due = {
            let mut last = self.last_saved.lock().expect("keys mutex poisoned");
            let due = now.saturating_sub(*last) >= 60;
            if due {
                *last = now;
            }
            due
        };
        if due {
            self.save();
        }
        Ok(id)
    }

    /// Counts a finished request's tokens against its key.
    pub fn record_tokens(&self, id: &str, input: u64, output: u64, now: u64) {
        let mut keys = self.keys.lock().expect("keys mutex poisoned");
        if let Some(k) = keys.iter_mut().find(|k| k.id == id) {
            let u = k.usage.entry(day_of(now)).or_default();
            u.input_tokens += input;
            u.output_tokens += output;
        }
    }

    /// Changes a key's name, limits, scopes or pause.
    pub fn change(&self, id: &str, c: KeyChange) -> Result<KeySummary, String> {
        if let Some(scopes) = &c.scopes {
            if let Some(bad) = scopes.iter().find(|s| !SCOPES.contains(&s.as_str())) {
                return Err(format!(
                    "Unknown permission \"{bad}\" (chat, images or embeddings)."
                ));
            }
        }
        let mut keys = self.keys.lock().expect("keys mutex poisoned");
        let k = keys.iter_mut().find(|k| k.id == id).ok_or("No such key.")?;
        if let Some(n) = c.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            k.name = n.chars().take(60).collect();
        }
        if let Some(l) = c.limits {
            k.limits = Limits {
                requests_per_minute: l.requests_per_minute.filter(|r| *r > 0),
                tokens_per_day: l.tokens_per_day.filter(|t| *t > 0),
            };
            self.recent.lock().expect("keys mutex poisoned").remove(id);
        }
        if let Some(sc) = c.scopes {
            k.scopes = if SCOPES.iter().all(|s| sc.iter().any(|x| x == s)) {
                Vec::new()
            } else {
                sc
            };
        }
        if let Some(p) = c.paused {
            k.paused = p;
        }
        let out = Self::summary(k);
        drop(keys);
        self.save();
        Ok(out)
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let keys = self.keys.lock().expect("keys mutex poisoned").clone();
        // A fresh 0600 temp file (never one left over, which could keep a
        // looser mode), then a rename: `sessions::write_private`.
        let write = || -> anyhow::Result<()> {
            crate::sessions::write_private(path, &serde_json::to_string_pretty(&keys)?)
        };
        if let Err(e) = write() {
            eprintln!("tt-station-agentd: couldn't save app keys: {e}");
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// OpenAI-style error body, which is what OpenAI clients know how to show.
fn openai_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message.into(), "type": "tt_station", "code": status.as_u16() } })),
    )
        .into_response()
}

/// Access to `/v1`: an app key (whose id it carries, to count its tokens) or a
/// paired device's token.
pub struct GatewayAuth {
    pub key: Option<String>,
}

impl GatewayAuth {
    /// Counts a finished request's tokens against the app key that made it.
    pub fn record(&self, state: &AppState, input: u64, output: u64) {
        if let Some(id) = &self.key {
            state
                .app_keys()
                .record_tokens(id, input, output, unix_now());
        }
    }
}

impl FromRequestParts<AppState> for GatewayAuth {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        // `Authorization: Bearer <key>`, or Anthropic's own `x-api-key: <key>`.
        let presented = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .or_else(|| parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()))
            .unwrap_or("")
            .trim()
            .to_string();
        let presented = presented.as_str();
        if presented.is_empty() {
            return Err(openai_error(
                StatusCode::UNAUTHORIZED,
                "Missing API key. Create one in TT Station (Connect → App keys).",
            ));
        }
        let scope = scope_of(parts.uri.path());
        match state.app_keys().admit(presented, unix_now(), scope) {
            Ok(id) => Ok(GatewayAuth { key: Some(id) }),
            Err(Denied::Unknown) => {
                // A device's own token only over HTTPS: on plain HTTP it could
                // be read off the wire, and it opens far more than `/v1`.
                if parts.extensions.get::<crate::routes::PlainHttp>().is_some() {
                    return Err(openai_error(StatusCode::UNAUTHORIZED, "This connection takes app keys only (ttq-…). Create one in TT Station (Connect → App keys)."));
                }
                if BearerAuth::from_request_parts(parts, state).await.is_ok() {
                    Ok(GatewayAuth { key: None })
                } else {
                    Err(openai_error(
                        StatusCode::UNAUTHORIZED,
                        "Unknown API key. Create one in TT Station (Connect → App keys).",
                    ))
                }
            }
            Err(Denied::Expired) => Err(openai_error(
                StatusCode::UNAUTHORIZED,
                "This key has expired. Create a new one in TT Station (Connect → App keys).",
            )),
            Err(Denied::Paused) => Err(openai_error(
                StatusCode::FORBIDDEN,
                "This key is paused. The box's owner can turn it back on in TT Station.",
            )),
            Err(Denied::Scope(what)) => Err(openai_error(
                StatusCode::FORBIDDEN,
                format!("This key isn't allowed to use {what} on this box."),
            )),
            Err(Denied::Budget) => Err(openai_error(
                StatusCode::TOO_MANY_REQUESTS,
                "This key has used its tokens for today. It resets at midnight on the box.",
            )),
            Err(Denied::Rate(wait)) => {
                let mut r = openai_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("Too many requests for this key: try again in {wait} s."),
                );
                if let Ok(v) = header::HeaderValue::from_str(&wait.to_string()) {
                    r.headers_mut().insert(header::RETRY_AFTER, v);
                }
                Err(r)
            }
        }
    }
}

// ---------------------------------------------------------------------
// Key management (paired devices only)
// ---------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateKey {
    pub name: String,
    /// Stop working after this many days; absent or 0, never.
    #[serde(default)]
    pub expires_in_days: Option<u32>,
}

pub(crate) async fn list_keys(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    Json(json!({ "keys": state.app_keys().list() }))
}

pub(crate) async fn create_key(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(req): Json<CreateKey>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ErrorResponse>)> {
    let now = unix_now();
    let expires = req
        .expires_in_days
        .filter(|d| *d > 0)
        .map(|d| now + u64::from(d) * 86_400);
    let (summary, key) = state
        .app_keys()
        .create_expiring(&req.name, now, expires)
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error: e })))?;
    crate::audit::record(
        &state,
        "app_key_create",
        &format!("{} ({})", summary.name, summary.id),
        "ok",
    );
    Ok((
        StatusCode::CREATED,
        Json(json!({ "key": key, "info": summary })),
    ))
}

/// `PATCH /keys/{id}` `{name?, limits?, scopes?, paused?}`
pub(crate) async fn change_key(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(change): Json<KeyChange>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let changed = state.app_keys().change(&id, change);
    crate::audit::record(
        &state,
        "app_key_change",
        &id,
        if changed.is_ok() { "ok" } else { "failed" },
    );
    changed
        .map(|k| Json(json!({ "info": k })))
        .map_err(|e| (StatusCode::BAD_REQUEST, Json(ErrorResponse { error: e })))
}

pub(crate) async fn revoke_key(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> StatusCode {
    if state.app_keys().revoke(&id) {
        crate::audit::record(&state, "app_key_revoke", &id, "ok");
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

/// `GET /connect` (paired): everything a "connect your apps" screen needs --
/// the addresses the API answers on and what it's serving.
pub(crate) async fn connect_info(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let port = state.ctrl_port();
    let lan = tokio::task::spawn_blocking(crate::pairqr::lan_ipv4)
        .await
        .unwrap_or(None);
    let name = state.name().to_string();
    // From other computers the API is encrypted (HTTPS with the box's own
    // certificate); on the box itself plain HTTP still works.
    let tls = state.tls();
    let mut urls = Vec::new();
    if let Some(t) = &tls {
        urls.push(json!({ "label": "Bonjour name (encrypted)", "url": format!("https://{name}.local:{}/v1", t.port) }));
        if let Some(ip) = lan {
            urls.push(json!({ "label": "Home network (encrypted)", "url": format!("https://{ip}:{}/v1", t.port) }));
        }
    }
    let lan_http = crate::routes::lan_http_allowed(&state);
    if lan_http {
        urls.push(json!({ "label": "Home network (unencrypted, allowed in Settings)", "url": format!("http://{name}.local:{port}/v1") }));
    }
    urls.push(json!({ "label": "On this box", "url": format!("http://127.0.0.1:{port}/v1") }));
    let models = served_models(&state).await;
    Json(json!({
        "urls": urls, "models": models, "keys": state.app_keys().list(),
        "fingerprint": tls.as_ref().map(|t| t.sha256.clone()), "lan_http": lan_http,
    }))
}

// ---------------------------------------------------------------------
// The OpenAI-compatible surface
// ---------------------------------------------------------------------

/// What the box serves right now, as OpenAI model entries plus our `kind`.
async fn served_models(state: &AppState) -> Vec<Value> {
    let mut models = Vec::new();
    if let Some(chat) = chat_endpoint(state).await {
        models.push(json!({ "id": chat.model, "object": "model", "owned_by": state.name(), "kind": "chat" }));
    }
    if let Some((_, id)) = image_target(state).await {
        models.push(
            json!({ "id": id, "object": "model", "owned_by": state.name(), "kind": "image" }),
        );
    }
    // The CPU services beside the chips (`crate::embeddings`).
    if let Some(id) = crate::embeddings::ready(state, &crate::embeddings::EMBED).await {
        models.push(
            json!({ "id": id, "object": "model", "owned_by": state.name(), "kind": "embedding" }),
        );
    }
    if let Some(id) = crate::embeddings::ready(state, &crate::embeddings::RERANK).await {
        models.push(
            json!({ "id": id, "object": "model", "owned_by": state.name(), "kind": "rerank" }),
        );
    }
    models
}

/// `POST /v1/embeddings`: the box's embeddings service (on the CPU) when it's
/// on; otherwise the served chat model, which may or may not embed.
pub(crate) async fn v1_embeddings(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth: GatewayAuth,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    if let Some(r) = crate::embeddings::v1_embeddings(&state, body.clone()).await {
        return r;
    }
    v1_text(axum::extract::State(state), auth, uri, Json(body)).await
}

/// `POST /v1/rerank` (Cohere/Jina format): the box's reranker.
pub(crate) async fn v1_rerank(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: GatewayAuth,
    Json(body): Json<Value>,
) -> Response {
    crate::embeddings::v1_rerank(&state, body).await
}

/// The chat model's endpoint, when the main slot serves one.
pub(crate) async fn chat_endpoint(state: &AppState) -> Option<libttstation::model::Endpoint> {
    let endpoint = state.endpoint()?;
    let probe = state.clone();
    let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    matches!(kind, "chat" | "other").then_some(endpoint)
}

/// The image server's base URL and model id, when the box serves one.
pub(crate) async fn image_target(state: &AppState) -> Option<(String, String)> {
    let endpoint = state.endpoint()?;
    let probe = state.clone();
    let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
        .await
        .unwrap_or("other");
    (kind == "image").then(|| {
        (
            endpoint
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_string(),
            endpoint.model,
        )
    })
}

/// `GET /v1/models`.
pub(crate) async fn v1_models(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: GatewayAuth,
) -> Json<Value> {
    Json(json!({ "object": "list", "data": served_models(&state).await }))
}

/// `POST /v1/chat/completions`, `/v1/completions`, `/v1/embeddings`: forwarded
/// to the served chat model, streaming included.
pub(crate) async fn v1_text(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth: GatewayAuth,
    uri: axum::http::Uri,
    Json(mut body): Json<Value>,
) -> Response {
    let Some(endpoint) = chat_endpoint(&state).await else {
        if let Some(waking) = crate::idle::wake_for_request(&state).await {
            let mut r = openai_error(StatusCode::SERVICE_UNAVAILABLE, waking);
            r.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
            return r;
        }
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No chat model is serving on the box right now. Start one in TT Station.",
        );
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".to_string(), json!(endpoint.model));
    }
    let path = uri.path().trim_start_matches("/v1");
    let url = format!("{}{path}", endpoint.base_url.trim_end_matches('/'));
    let prompt_chars = body
        .get("messages")
        .or(body.get("prompt"))
        .or(body.get("input"))
        .map_or(0, |v| v.to_string().len()) as u64;
    // A stream's token counts come in a last chunk the client didn't ask for:
    // ask for it, count it, and leave it out of what the client gets.
    let strip =
        auth.key.is_some() && body["stream"] == true && body.get("stream_options").is_none();
    if strip {
        body["stream_options"] = json!({ "include_usage": true });
    }
    let resp = forward(url, body, None).await;
    if auth.key.is_none() || !resp.status().is_success() {
        return resp;
    }
    count_tokens(resp, strip, move |input, output| {
        // The model server's own counts; otherwise about four characters a token.
        let input = if input > 0 { input } else { prompt_chars / 4 };
        auth.record(&state, input, output);
    })
}

/// What a response used, gathered as it passes through.
struct Tally<F: FnOnce(u64, u64)> {
    /// Leave out the usage-only chunk (the client didn't ask for it).
    strip: bool,
    pending: Vec<u8>,
    whole: Vec<u8>,
    input: u64,
    output: u64,
    pieces: u64,
    done: Option<F>,
}

impl<F: FnOnce(u64, u64)> Tally<F> {
    /// Takes a chunk; answers what to pass on.
    fn feed(&mut self, chunk: &[u8]) -> axum::body::Bytes {
        if self.whole.len() < 4 << 20 {
            self.whole.extend_from_slice(chunk);
        }
        self.pending.extend_from_slice(chunk);
        let Some(last) = self.pending.iter().rposition(|b| *b == b'\n') else {
            return if self.strip {
                axum::body::Bytes::new()
            } else {
                axum::body::Bytes::copy_from_slice(chunk)
            };
        };
        let lines: Vec<u8> = self.pending.drain(..=last).collect();
        let mut out = Vec::with_capacity(lines.len());
        for line in lines.split_inclusive(|b| *b == b'\n') {
            let mut keep = true;
            if let Some(data) = line.strip_prefix(b"data:") {
                if let Ok(v) = serde_json::from_slice::<Value>(data.trim_ascii()) {
                    self.usage(&v);
                    let choice = &v["choices"][0];
                    if choice["delta"]["content"]
                        .as_str()
                        .is_some_and(|c| !c.is_empty())
                        || choice["text"].as_str().is_some_and(|c| !c.is_empty())
                    {
                        self.pieces += 1;
                    }
                    keep = !(self.strip
                        && v["choices"].as_array().is_some_and(Vec::is_empty)
                        && v.get("usage").is_some());
                }
            }
            if keep {
                out.extend_from_slice(line);
            }
        }
        if self.strip {
            axum::body::Bytes::from(out)
        } else {
            axum::body::Bytes::copy_from_slice(chunk)
        }
    }

    /// Whatever's left at the end (a last line with no newline).
    fn flush(&mut self) -> axum::body::Bytes {
        let rest = std::mem::take(&mut self.pending);
        if self.strip {
            axum::body::Bytes::from(rest)
        } else {
            axum::body::Bytes::new()
        }
    }

    fn usage(&mut self, v: &Value) {
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.input = u["prompt_tokens"].as_u64().unwrap_or(self.input);
            self.output = u["completion_tokens"].as_u64().unwrap_or(self.output);
        }
    }
}

impl<F: FnOnce(u64, u64)> Drop for Tally<F> {
    fn drop(&mut self) {
        if self.output == 0 && self.pieces == 0 {
            // Not a stream: one JSON body.
            if let Ok(v) = serde_json::from_slice::<Value>(&self.whole) {
                self.usage(&v);
            }
        }
        if let Some(done) = self.done.take() {
            done(
                self.input,
                if self.output > 0 {
                    self.output
                } else {
                    self.pieces
                },
            );
        }
    }
}

/// Passes a response through (less the usage chunk when `strip`), and once
/// it's over -- or the client leaves -- calls `done` with its token counts:
/// the model server's own, or else output counted from the streamed pieces.
fn count_tokens(
    resp: Response,
    strip: bool,
    done: impl FnOnce(u64, u64) + Send + 'static,
) -> Response {
    use futures_util::StreamExt;
    let tally = std::sync::Arc::new(Mutex::new(Tally {
        strip,
        pending: Vec::new(),
        whole: Vec::new(),
        input: 0,
        output: 0,
        pieces: 0,
        done: Some(done),
    }));
    let (parts, body) = resp.into_parts();
    let feeding = tally.clone();
    let body = body
        .into_data_stream()
        .map(move |chunk| chunk.map(|c| feeding.lock().expect("tally").feed(&c)));
    let tail = futures_util::stream::once(async move {
        Ok::<_, axum::Error>(tally.lock().expect("tally").flush())
    });
    Response::from_parts(parts, Body::from_stream(body.chain(tail)))
}

/// `POST /v1/images/generations`: OpenAI's image API on top of tt-media-server
/// (`n` → `number_of_images`, `b64_json` answers), so OpenAI clients -- Open
/// WebUI's image button among them -- can use the box's image model.
pub(crate) async fn v1_images(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: GatewayAuth,
    Json(body): Json<Value>,
) -> Response {
    let Some((base, model)) = image_target(&state).await else {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No image model is serving on the box right now. Start one in TT Station.",
        );
    };
    if body.get("response_format").and_then(Value::as_str) == Some("url") {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "This box returns images inline: use response_format \"b64_json\".",
        );
    }
    let request = media_image_request(&body, &model);
    let resp = forward(
        format!("{base}/v1/images/generations"),
        request,
        Some(Duration::from_secs(600)),
    )
    .await;
    if !resp.status().is_success() {
        return resp;
    }
    let bytes = match axum::body::to_bytes(resp.into_body(), 256 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => return openai_error(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    let media: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    let data: Vec<Value> = media["images"]
        .as_array()
        .map(|imgs| imgs.iter().map(|b64| json!({ "b64_json": b64 })).collect())
        .unwrap_or_default();
    if data.is_empty() {
        return openai_error(
            StatusCode::BAD_GATEWAY,
            "The image model returned no image.",
        );
    }
    Json(json!({ "created": unix_now(), "data": data })).into_response()
}

/// An OpenAI image request in tt-media-server's terms. FLUX schnell is a 4-step
/// model; anything else defaults to 20.
pub fn media_image_request(openai: &Value, model: &str) -> Value {
    let steps = openai
        .get("num_inference_steps")
        .or_else(|| openai.get("steps"))
        .and_then(Value::as_u64)
        .unwrap_or(if model.to_lowercase().contains("schnell") {
            4
        } else {
            20
        });
    let mut request = json!({
        "prompt": openai.get("prompt").cloned().unwrap_or(json!("")),
        "number_of_images": openai.get("n").and_then(Value::as_u64).unwrap_or(1).clamp(1, 4),
        "num_inference_steps": steps.clamp(4, 50),
        "guidance_scale": openai.get("guidance_scale").cloned().unwrap_or(json!(3.5)),
        "image_return_format": "JPEG",
    });
    for passthrough in ["negative_prompt", "seed"] {
        if let Some(v) = openai.get(passthrough) {
            request[passthrough] = v.clone();
        }
    }
    request
}

/// POST `body` to `url` and stream the answer back (status, content type and
/// body as-is), so SSE streaming works end to end.
async fn forward(url: String, body: Value, timeout: Option<Duration>) -> Response {
    let mut client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(600));
    if let Some(t) = timeout {
        client = client.timeout(t);
    }
    let client = match client.build() {
        Ok(c) => c,
        Err(e) => return openai_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    // Media servers (0.10) want their default bearer key; vLLM ignores it.
    let api_key =
        std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
    let upstream = match client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_GATEWAY,
                format!("Couldn't reach the model on the box: {e}"),
            )
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = upstream
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|e| openai_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_have_limits_permissions_and_usage() {
        let keys = AppKeys::in_memory();
        let (info, secret) = keys.create("Team laptop", 100).unwrap();
        let now = unix_now();
        assert_eq!(scope_of("/v1/images/generations"), "images");
        assert_eq!(scope_of("/v1/rerank"), "embeddings");
        assert_eq!(scope_of("/v1/messages"), "chat");

        // Two a minute: the third waits.
        keys.change(
            &info.id,
            KeyChange {
                limits: Some(Limits {
                    requests_per_minute: Some(2),
                    tokens_per_day: None,
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(keys.admit(&secret, now, "chat").is_ok());
        assert!(keys.admit(&secret, now + 1, "chat").is_ok());
        assert!(matches!(
            keys.admit(&secret, now + 2, "chat"),
            Err(Denied::Rate(58))
        ));
        assert!(
            keys.admit(&secret, now + 61, "chat").is_ok(),
            "a minute later it's through"
        );

        // Chat only.
        keys.change(
            &info.id,
            KeyChange {
                scopes: Some(vec!["chat".into()]),
                limits: Some(Limits::default()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            keys.admit(&secret, now + 70, "images"),
            Err(Denied::Scope("images"))
        );

        // A day's tokens, then no more.
        keys.change(
            &info.id,
            KeyChange {
                limits: Some(Limits {
                    requests_per_minute: None,
                    tokens_per_day: Some(1000),
                }),
                ..Default::default()
            },
        )
        .unwrap();
        keys.record_tokens(&info.id, 700, 400, now + 80);
        assert_eq!(keys.admit(&secret, now + 90, "chat"), Err(Denied::Budget));
        let listed = &keys.list()[0];
        assert_eq!(listed.days.len(), 14);
        assert!(listed.days.iter().any(|(_, u)| u.tokens() == 1100));

        // Paused, and every permission is the same as none.
        keys.change(
            &info.id,
            KeyChange {
                paused: Some(true),
                scopes: Some(SCOPES.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(keys.admit(&secret, now + 100, "chat"), Err(Denied::Paused));
        assert!(keys.list()[0].scopes.is_empty());
        assert_eq!(keys.admit("ttq-nope", now, "chat"), Err(Denied::Unknown));
        assert!(keys
            .change(
                &info.id,
                KeyChange {
                    scopes: Some(vec!["video".into()]),
                    ..Default::default()
                }
            )
            .is_err());
    }

    #[test]
    fn counts_a_streams_tokens_without_showing_the_usage_chunk() {
        let got = std::sync::Arc::new(Mutex::new(None));
        let g = got.clone();
        let mut t = Tally {
            strip: true,
            pending: Vec::new(),
            whole: Vec::new(),
            input: 0,
            output: 0,
            pieces: 0,
            done: Some(move |i, o| *g.lock().unwrap() = Some((i, o))),
        };
        let mut out = Vec::new();
        // Split mid-line, as the network does.
        out.extend_from_slice(
            &t.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: {\"choi"),
        );
        out.extend_from_slice(&t.feed(b"ces\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\ndata: [DONE]\n\n"));
        out.extend_from_slice(&t.flush());
        drop(t);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"Hi\"") && text.contains("[DONE]"));
        assert!(
            !text.contains("usage"),
            "the client didn't ask for usage: {text}"
        );
        assert_eq!(*got.lock().unwrap(), Some((12, 7)));
    }

    #[test]
    fn keys_are_minted_listed_checked_and_revoked() {
        let dir = std::env::temp_dir().join(format!("tt-keys-{}", std::process::id()));
        let path = dir.join("app-keys.json");
        let keys = AppKeys::open(&path);
        let (info, secret) = keys.create("Open WebUI", 100).unwrap();
        assert!(secret.starts_with("ttq-") && secret.len() > 30);
        assert!(info.hint.starts_with("ttq-"));
        assert!(
            !keys.list()[0].hint.contains(&secret[10..]),
            "never the whole key"
        );
        assert!(keys.check(&secret, 200));
        assert!(!keys.check("ttq-nope", 200));
        assert_eq!(keys.list()[0].requests, 1);
        assert!(keys.create("  ", 1).is_err());

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains(&secret),
            "the key itself never reaches the disk"
        );
        assert!(on_disk.contains(&key_digest(&secret)));
        assert_eq!(secret.len(), 4 + 64, "256 bits");
        let reopened = AppKeys::open(&path);
        assert!(reopened.check(&secret, 300), "persisted");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(reopened.revoke(&info.id));
        assert!(!reopened.check(&secret, 400));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_from_before_hashing_are_converted_and_keep_working() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app-keys.json");
        std::fs::write(&path, r#"[{"id":"k1","name":"Open WebUI","key":"ttq-0123456789abcdef0123456789abcdef","created":5,"requests":3}]"#).unwrap();
        let keys = AppKeys::open(&path);
        assert!(keys.check("ttq-0123456789abcdef0123456789abcdef", 10));
        assert_eq!(keys.list()[0].hint, "ttq-0123…");
        assert_eq!(keys.list()[0].requests, 4);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("0123456789abcdef0123456789abcdef"),
            "converted in place: {on_disk}"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            AppKeys::open(&path).check("ttq-0123456789abcdef0123456789abcdef", 20),
            "and after a restart"
        );
    }

    #[test]
    fn a_damaged_key_file_is_set_aside_not_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app-keys.json");
        std::fs::write(&path, b"[{\"id\": \"k1\", ").unwrap();
        let keys = AppKeys::open(&path);
        keys.create("new", 1).unwrap();
        let aside: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("app-keys.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(
            std::fs::read(tmp.path().join(&aside[0])).unwrap(),
            b"[{\"id\": \"k1\", "
        );
    }

    #[test]
    fn keys_can_expire() {
        let keys = AppKeys::in_memory();
        let (info, secret) = keys.create_expiring("Demo", 100, Some(200)).unwrap();
        assert_eq!(info.expires, Some(200));
        assert!(
            keys.admit(&secret, 200, "chat").is_ok(),
            "good through its last second"
        );
        assert_eq!(keys.admit(&secret, 201, "chat"), Err(Denied::Expired));
    }

    #[test]
    fn openai_image_requests_translate() {
        let r = media_image_request(
            &json!({ "prompt": "fox", "n": 9, "size": "1024x1024" }),
            "tt-flux.1-schnell",
        );
        assert_eq!(r["number_of_images"], 4);
        assert_eq!(r["num_inference_steps"], 4);
        assert_eq!(r["guidance_scale"], 3.5);
        let r = media_image_request(&json!({ "prompt": "fox", "seed": 3 }), "tt-flux.1-dev");
        assert_eq!(r["num_inference_steps"], 20);
        assert_eq!(r["seed"], 3);
    }
}
