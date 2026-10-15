// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What the owner wants to do, rather than which model to run: **Chat**,
//! **Images**, **Chat + images**, **Video**, **Transcribe**. Each mode picks the
//! best model this box has downloaded (or says what to download), knows how
//! long it usually takes to load here, and starts with one request -- a whole-box
//! serve or a co-serve (`crate::coserve`) as the mode needs.
//!
//! Load times are learned: every finished start records how long it took
//! (`load-times.json`), and the median is what `GET /serve` and `GET /modes`
//! report as `eta_s`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse, Phase};

/// Past load times per model (seconds), newest last.
#[derive(Debug)]
pub struct LoadTimes {
    path: Option<PathBuf>,
    times: Mutex<HashMap<String, Vec<u64>>>,
}

const KEEP_TIMES: usize = 8;

fn key(model: &str) -> String {
    crate::notify::display_model(model).to_lowercase()
}

impl LoadTimes {
    pub fn in_memory() -> Self {
        LoadTimes {
            path: None,
            times: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let times = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        LoadTimes {
            path: Some(path),
            times: Mutex::new(times),
        }
    }

    pub fn record(&self, model: &str, secs: u64) {
        if secs == 0 {
            return;
        }
        {
            let mut times = self.times.lock().expect("load times poisoned");
            let list = times.entry(key(model)).or_default();
            list.push(secs);
            let extra = list.len().saturating_sub(KEEP_TIMES);
            list.drain(..extra);
        }
        if let Some(path) = &self.path {
            let times = self.times.lock().expect("load times poisoned").clone();
            if let Ok(bytes) = serde_json::to_vec_pretty(&times) {
                let _ = std::fs::write(path, bytes);
            }
        }
    }

    /// The median of past loads, if any.
    pub fn estimate(&self, model: &str) -> Option<u64> {
        let times = self.times.lock().expect("load times poisoned");
        let mut list = times.get(&key(model))?.clone();
        if list.is_empty() {
            return None;
        }
        list.sort_unstable();
        Some(list[list.len() / 2])
    }
}

/// A mode's definition: model preferences, best first.
struct ModeDef {
    id: &'static str,
    title: &'static str,
    blurb: &'static str,
    icon: &'static str,
    /// One model on the whole box, or (chat, image) on half a box each.
    models: Models,
}

enum Models {
    Whole(&'static [&'static str]),
    Pair(&'static [&'static str], &'static [&'static str]),
}

/// A mode's title, by id (`None` for an unknown id).
pub fn mode_title(id: &str) -> Option<&'static str> {
    MODES.iter().find(|m| m.id == id).map(|m| m.title)
}

const MODES: [ModeDef; 5] = [
    ModeDef {
        id: "chat",
        title: "Chat",
        blurb: "The best answers: one large model on the whole box.",
        icon: "sparkles",
        models: Models::Whole(&[
            "Qwen3-32B",
            "Llama-3.3-70B-Instruct",
            "gemma-4-31B-it",
            "Qwen3.6-27B",
            "Llama-3.1-8B-Instruct",
        ]),
    },
    ModeDef {
        id: "images",
        title: "Images",
        blurb: "Pictures from a prompt in a few seconds each.",
        icon: "photo.on.rectangle.angled",
        models: Models::Whole(&["FLUX.1-schnell", "FLUX.1-dev"]),
    },
    ModeDef {
        id: "chat_images",
        title: "Chat + images",
        blurb: "Chat and draw at the same time, with a smaller chat model.",
        icon: "rectangle.split.2x1",
        models: Models::Pair(
            &["Llama-3.1-8B-Instruct"],
            &["FLUX.1-schnell", "FLUX.1-dev"],
        ),
    },
    ModeDef {
        id: "video",
        title: "Video",
        blurb: "Short clips from a prompt; each takes several minutes.",
        icon: "film",
        models: Models::Whole(&["Wan2.2-T2V-A14B-Diffusers"]),
    },
    ModeDef {
        id: "transcribe",
        title: "Transcribe",
        blurb: "Speech to text for recordings and meetings.",
        icon: "waveform",
        models: Models::Whole(&["whisper-large-v3", "distil-large-v3"]),
    },
];

/// A mode as the phone sees it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Mode {
    pub id: &'static str,
    pub title: &'static str,
    pub blurb: &'static str,
    pub icon: &'static str,
    /// The models it would run (one, or chat + image).
    pub models: Vec<String>,
    /// All of them are downloaded.
    pub ready: bool,
    /// What to download first.
    pub needs_download: Vec<String>,
    pub active: bool,
    /// Usual load time on this box.
    pub eta_s: Option<u64>,
    pub note: Option<&'static str>,
}

/// (name, ready) of the catalog models that run on `device`.
fn catalog(state: &AppState, device: Option<&str>) -> Vec<(String, bool)> {
    let Some(tt) = state.tt_cli() else {
        return Vec::new();
    };
    tt.catalog(device, false)
        .map(|c| {
            c.models
                .iter()
                .filter(|m| m.supported)
                .map(|m| {
                    (
                        m.name.clone(),
                        matches!(
                            m.weights,
                            crate::ttcli::Weights::Complete | crate::ttcli::Weights::Container
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The best of `prefs` this box can run: a ready one first, else the first
/// supported (to download). `None` when none runs here.
fn pick(prefs: &[&str], available: &[(String, bool)]) -> Option<(String, bool)> {
    let find = |name: &str| available.iter().find(|(n, _)| n.eq_ignore_ascii_case(name));
    prefs
        .iter()
        .filter_map(|p| find(p))
        .find(|(_, ready)| *ready)
        .or_else(|| prefs.iter().find_map(|p| find(p)))
        .cloned()
}

/// The owner's preferred model when this box can run it (downloaded or not),
/// else the best of the mode's own list.
fn choose(mine: Option<&str>, prefs: &[&str], available: &[(String, bool)]) -> Option<(String, bool)> {
    mine.and_then(|m| available.iter().find(|(n, _)| n.eq_ignore_ascii_case(m)).cloned())
        .or_else(|| pick(prefs, available))
}

/// The modes this box offers right now (blocking: reads the tt catalog).
pub fn modes_blocking(state: &AppState) -> Vec<Mode> {
    let whole = catalog(state, state.device_mesh());
    let half = catalog(state, Some(crate::coserve::SLOT_DEVICE));
    let life = state.lifecycle();
    let serving = [
        life.model.clone(),
        life.served_id.clone(),
        state.current_model(),
    ]
    .into_iter()
    .flatten()
    .map(|m| key(&m))
    .collect::<Vec<_>>();
    let split = crate::coserve::is_split(state);
    let companion_model = state.companion().and_then(|c| {
        let l = c.lifecycle();
        l.model.or(l.served_id).map(|m| key(&m))
    });
    let times = state.load_times();
    let mine = crate::prefs::load(state);
    let can_split = state.companion().is_some() && state.device_mesh() == Some("p300x2");

    MODES
        .iter()
        .filter_map(|def| {
            let kind = match def.id {
                "images" => "image",
                other => other,
            };
            let (picked, note): (Vec<(String, bool)>, Option<&'static str>) = match &def.models {
                Models::Whole(prefs) => (vec![choose(mine.for_kind(kind), prefs, &whole)?], None),
                Models::Pair(chat, image) => {
                    if !can_split {
                        return None;
                    }
                    let image = choose(mine.image.as_deref(), image, &half)?;
                    let note = crate::coserve::half_box_note(&image.0);
                    (vec![choose(mine.chat.as_deref(), chat, &half)?, image], note)
                }
            };
            let models: Vec<String> = picked.iter().map(|(n, _)| n.clone()).collect();
            let active = match &def.models {
                Models::Whole(_) => !split && serving.contains(&key(&models[0])),
                Models::Pair(..) => {
                    split
                        && serving.contains(&key(&models[0]))
                        && companion_model.as_deref() == Some(key(&models[1]).as_str())
                }
            };
            let eta_s = models.iter().filter_map(|m| times.estimate(m)).max();
            Some(Mode {
                id: def.id,
                title: def.title,
                blurb: def.blurb,
                icon: def.icon,
                ready: picked.iter().all(|(_, r)| *r),
                needs_download: picked
                    .iter()
                    .filter(|(_, r)| !r)
                    .map(|(n, _)| n.clone())
                    .collect(),
                models,
                active: active && life.phase == Phase::Serving,
                eta_s,
                note,
            })
        })
        .collect()
}

pub async fn modes(state: &AppState) -> Vec<Mode> {
    let probe = state.clone();
    tokio::task::spawn_blocking(move || modes_blocking(&probe))
        .await
        .unwrap_or_default()
}

/// Start `id`. `Ok(started)` is false when it's already active.
pub async fn start(state: &AppState, id: &str) -> Result<bool, String> {
    let all = modes(state).await;
    let mode = all
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("This box has no \"{id}\" mode."))?;
    if mode.active {
        return Ok(false);
    }
    if !mode.ready {
        return Err(format!(
            "{} needs {} downloaded first (Models tab).",
            mode.title,
            mode.needs_download.join(" and ")
        ));
    }
    if mode.models.len() == 2 {
        let _ = crate::coserve::post_coserve(
            axum::extract::State(state.clone()),
            BearerAuth,
            Json(crate::coserve::CoserveRequest {
                chat: mode.models[0].clone(),
                image: Some(mode.models[1].clone()),
                second: None,
            }),
        )
        .await
        .map_err(|(_, e)| e.0.error)?;
    } else {
        crate::routes::begin_serve(state, &mode.models[0], false)
            .await
            .map_err(|(_, e)| e.0.error)?;
    }
    Ok(true)
}

/// `GET /modes` (the box itself or a paired device, like `/serve`).
pub(crate) async fn get_modes(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<Value> {
    Json(json!({ "modes": modes(&state).await }))
}

/// `POST /modes/{id}` (authed): switch the box to a mode. `202` when it
/// started, `200` when it was already active.
pub(crate) async fn post_mode(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<ErrorResponse>)> {
    let started = start(&state, &id).await;
    crate::audit::record(&state, "mode_switch", &id, match &started { Ok(true) => "starting", Ok(false) => "already_active", Err(_) => "failed" });
    match started {
        Ok(true) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({ "status": "starting", "mode": id })),
        )),
        Ok(false) => Ok((
            StatusCode::OK,
            Json(json!({ "status": "active", "mode": id })),
        )),
        Err(e) => Err((StatusCode::CONFLICT, Json(ErrorResponse { error: e }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_times_learn_a_median_per_model() {
        let t = LoadTimes::in_memory();
        assert_eq!(t.estimate("Qwen/Qwen3-32B"), None);
        for s in [300, 900, 360] {
            t.record("Qwen/Qwen3-32B", s);
        }
        assert_eq!(t.estimate("Qwen3-32B"), Some(360), "same model by any name");
        for s in 0..20 {
            t.record("x", 100 + s);
        }
        assert_eq!(t.estimate("x"), Some(116), "only the newest 8 count");
    }

    #[test]
    fn picks_a_downloaded_model_before_a_better_missing_one() {
        let available = vec![
            ("Qwen3-32B".to_string(), false),
            ("Llama-3.1-8B-Instruct".to_string(), true),
        ];
        let prefs = ["Qwen3-32B", "Llama-3.1-8B-Instruct"];
        assert_eq!(
            pick(&prefs, &available),
            Some(("Llama-3.1-8B-Instruct".into(), true))
        );
        let none_ready = vec![("Qwen3-32B".to_string(), false)];
        assert_eq!(pick(&prefs, &none_ready), Some(("Qwen3-32B".into(), false)));
        assert_eq!(pick(&["Other"], &available), None);
    }
}
