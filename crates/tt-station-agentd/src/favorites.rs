// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Favorite models: any model a person stars (an official one, a community
//! package, one of their bring-ups, a package in the community catalog), kept
//! on the box so every app sees the same list.
//!
//! - `GET /favorites`: `{ favorites: [{ key, name, kind, added }] }`, newest first.
//! - `PUT /favorites`: `{ key, name, kind, on }` stars (`on: true`) or unstars a model.
//!
//! `key` is `<kind>:<id>` as the apps build it (`official:Qwen3-32B`,
//! `community:tt-hous/muse-glimmer-30b`, ...); the box only stores it.

use std::path::PathBuf;
use std::sync::Mutex;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Favorite {
    pub key: String,
    pub name: String,
    pub kind: String,
    pub added: u64,
}

#[derive(Debug, Deserialize)]
pub struct FavoriteBody {
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    pub on: bool,
}

/// What `crate::favorites` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// One read-modify-write of favorites.json at a time.
    lock: Mutex<()>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().favorites
}

fn file(state: &AppState) -> PathBuf {
    state.config_dir().join("favorites.json")
}

fn load(state: &AppState) -> Vec<Favorite> {
    std::fs::read(file(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The list after starring or unstarring `body` (newest first, one entry per key).
pub fn apply(mut list: Vec<Favorite>, body: &FavoriteBody, now: u64) -> Vec<Favorite> {
    list.retain(|f| f.key != body.key);
    if body.on {
        let name = if body.name.trim().is_empty() {
            body.key
                .split_once(':')
                .map_or(body.key.as_str(), |(_, id)| id)
        } else {
            body.name.trim()
        };
        list.insert(
            0,
            Favorite {
                key: body.key.clone(),
                name: name.to_string(),
                kind: body.kind.trim().to_string(),
                added: now,
            },
        );
    }
    list
}

fn valid(body: &FavoriteBody) -> bool {
    let k = &body.key;
    !k.is_empty()
        && k.len() <= 300
        && k.contains(':')
        && !k.chars().any(char::is_control)
        && body.name.len() <= 300
        && body.kind.len() <= 40
}

/// `GET /favorites`.
pub(crate) async fn get_favorites(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    Json(json!({ "favorites": load(&state) }))
}

/// `PUT /favorites`.
pub(crate) async fn put_favorite(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(body): Json<FavoriteBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !valid(&body) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "That isn't a model to favorite." })),
        ));
    }
    let _guard = mine(&state).lock.lock().expect("favorites");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let list = apply(load(&state), &body, now);
    let path = file(&state);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&list).unwrap_or_default()).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    Ok(Json(json!({ "favorites": list })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(key: &str, on: bool) -> FavoriteBody {
        FavoriteBody {
            key: key.into(),
            name: String::new(),
            kind: "official".into(),
            on,
        }
    }

    #[test]
    fn stars_and_unstars_once_per_key() {
        let l = apply(vec![], &body("official:Qwen3-32B", true), 1);
        let l = apply(l, &body("community:tt-hous/muse", true), 2);
        let l = apply(l, &body("official:Qwen3-32B", true), 3);
        assert_eq!(
            l.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(),
            ["official:Qwen3-32B", "community:tt-hous/muse"]
        );
        assert_eq!(l[0].name, "Qwen3-32B");
        let l = apply(l, &body("official:Qwen3-32B", false), 4);
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn refuses_odd_keys() {
        assert!(!valid(&body("no-kind", true)));
        assert!(!valid(&body("official:a\nb", true)));
        assert!(valid(&body("official:Qwen3-32B", true)));
    }
}
