// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Your data: taking it with you, bringing it back, and how long the box
//! keeps it.
//!
//! - `GET /data/settings`, `PUT /data/settings` (owners): the household's data
//!   settings ([`DataSettings`]) -- whether recordings are kept after they're
//!   transcribed, whether transcripts go into the shared Library, how many days
//!   transcripts and research reports are kept, whether the box starts its
//!   last model again after a power cut (`crate::boot`), and who may start and
//!   stop models (`crate::model_control`).
//! - `POST /export` (owners): a `.tar.gz` of the person's data, with a
//!   `manifest.json` (format and agent versions, counts, each file's size and
//!   SHA-256).
//! - `POST /import?dry_run=true` (owners): what importing that archive would
//!   add and overwrite; without `dry_run`, the import itself.
//!
//! **What an export holds** is an allowlist ([`classify`]), never "everything
//! but": research reports with their sources, finished transcripts (and the
//! recording only when it was kept), memories, people (names; not which device
//! is whose), routines and their finished runs, schedules, preferences
//! (models, favorites, idle, embeddings, voice, these data settings),
//! evaluations, Code sessions' metadata (`session.json`, not the projects, the
//! conversation or the sandbox's credentials), and the prompts of images made
//! in chat. **Never** paired-device tokens, app keys, the TLS key, APNs keys,
//! push device tokens, the local secret, the HF token or any other secret: none
//! of them is on the allowlist, and [`looks_secret`] turns away anything that
//! even looks like one.
//!
//! **Importing** reads the archive twice from a private temp file: once to
//! check it and say what would change, and (for a real import) once to write.
//! Any path that isn't plain (absolute, `..`, `.`, empty parts, backslashes) or
//! any entry that isn't a regular file fails the whole import before anything
//! is written; sizes are capped per file, in all, and in number of entries; a
//! file whose SHA-256 doesn't match the manifest fails it too. Files that
//! aren't on the allowlist are skipped and listed. People, memories, schedules
//! and routines merge by id into what the box already has; everything else is
//! written file by file (atomically), replacing a file at the same path.
//! Jobs, reports and runs that weren't finished aren't imported, so an import
//! never starts work. A run's or an evaluation's `id` must be its file's name.
//! Code sessions are exported (as a record) but never imported: a
//! `session.json` names the folder the session works in and what it may do
//! without asking, which an archive mustn't be able to set.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, CallerAuth, OwnerAuth};

/// `manifest.json`'s `format`.
pub const FORMAT: &str = "tt-station-export";
/// Bumped when the layout changes in a way an older agent can't read.
pub const FORMAT_VERSION: u32 = 1;
pub const MANIFEST: &str = "manifest.json";
/// The household's data settings, in the config directory.
pub const SETTINGS_FILE: &str = "data-settings.json";
/// Biggest archive `POST /import` takes.
pub const MAX_IMPORT_BYTES: u64 = 6 * 1024 * 1024 * 1024;
/// Biggest single file in an archive, but a recording.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Biggest recording (what `POST /jobs/transcribe` takes).
pub const MAX_AUDIO_BYTES: u64 = crate::jobs::MAX_UPLOAD_BYTES as u64;
/// Most bytes an archive may unpack to, in all.
pub const MAX_TOTAL_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Most entries in an archive.
pub const MAX_ENTRIES: usize = 100_000;

/// Where an export or an upload is spooled: on the data disk, under the
/// config dir (`0700`), not in `$XDG_RUNTIME_DIR` (a small tmpfs a 6 GB
/// archive would fill).
fn temp_dir(config: &Path) -> PathBuf {
    config.join("tmp")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- settings

/// What the household decided about its data. Everything is off by default:
/// recordings are deleted once transcribed, transcripts stay out of the
/// Library, nothing is deleted on a timer, and nothing starts by itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DataSettings {
    /// Keep a recording after it's transcribed, when its job didn't say.
    #[serde(default)]
    pub keep_audio: bool,
    /// Add finished transcripts to the shared Library, when the job didn't say.
    #[serde(default)]
    pub transcripts_to_library: bool,
    /// Delete transcripts and research reports this many days after they
    /// finished. `None` keeps them.
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// After a power cut or reboot, start the model that was serving
    /// (`crate::boot`).
    #[serde(default)]
    pub restart_last_model: bool,
    /// Who may start, stop and switch the served model (`crate::model_control`).
    #[serde(default)]
    pub model_control: crate::model_control::ModelControl,
}

/// Longest retention the setting takes (ten years).
pub const MAX_RETENTION_DAYS: u32 = 3650;

pub fn settings_in(config: &Path) -> DataSettings {
    crate::fsio::read_json_or_backup(&config.join(SETTINGS_FILE)).unwrap_or_default()
}

pub fn settings(state: &AppState) -> DataSettings {
    settings_in(&state.config_dir())
}

/// `GET /data/settings` (authed): the settings, whether this caller may
/// change them, and whether it may start and stop models
/// (`crate::model_control`).
pub(crate) async fn get_settings(
    axum::extract::State(state): axum::extract::State<AppState>,
    CallerAuth(caller): CallerAuth,
) -> Json<Value> {
    Json(json!({
        "settings": settings(&state),
        "owner": caller.owner(),
        "may_control_models": crate::model_control::allows(&state, &caller),
    }))
}

/// `PUT /data/settings` body: any of the fields; `retention_days: null` (or 0)
/// turns auto-delete off.
#[derive(Debug, Deserialize)]
pub struct SettingsPatch {
    pub keep_audio: Option<bool>,
    pub transcripts_to_library: Option<bool>,
    #[serde(default, deserialize_with = "some_or_null")]
    pub retention_days: Option<Option<u32>>,
    pub restart_last_model: Option<bool>,
    pub model_control: Option<crate::model_control::ModelControl>,
}

/// Tells a missing field (`None`) from an explicit `null` (`Some(None)`).
fn some_or_null<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Option<u32>>, D::Error> {
    Option::<u32>::deserialize(d).map(Some)
}

/// `PUT /data/settings` (owners).
pub(crate) async fn put_settings(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    CallerAuth(caller): CallerAuth,
    Json(patch): Json<SettingsPatch>,
) -> Result<Json<DataSettings>, ApiError> {
    let mut s = settings(&state);
    if let Some(v) = patch.keep_audio {
        s.keep_audio = v;
    }
    if let Some(v) = patch.transcripts_to_library {
        s.transcripts_to_library = v;
    }
    if let Some(v) = patch.retention_days {
        let v = v.filter(|d| *d > 0);
        if v.is_some_and(|d| d > MAX_RETENTION_DAYS) {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("Keep things for at most {MAX_RETENTION_DAYS} days, or always."),
            ));
        }
        s.retention_days = v;
    }
    if let Some(v) = patch.restart_last_model {
        s.restart_last_model = v;
    }
    let was_control = s.model_control;
    if let Some(v) = patch.model_control {
        s.model_control = v;
    }
    crate::fsio::write_json(
        &state.config_dir().join(SETTINGS_FILE),
        &s,
        crate::fsio::PRIVATE,
    )
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let summary = format!(
        "keep audio {}, transcripts to Library {}, delete after {}, restart last model {}",
        on_off(s.keep_audio),
        on_off(s.transcripts_to_library),
        s.retention_days
            .map(|d| format!("{d} days"))
            .unwrap_or_else(|| "never".into()),
        on_off(s.restart_last_model)
    );
    crate::audit::record_as(&state, &caller.actor(), "data_settings", &summary, "ok");
    if s.model_control != was_control {
        crate::audit::record_as(
            &state,
            &caller.actor(),
            "model_control",
            &format!("{} -> {}", was_control.as_str(), s.model_control.as_str()),
            "ok",
        );
    }
    Ok(Json(s))
}

fn on_off(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

// ---------------------------------------------------------------- what's in an export

/// What a file in an export is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Research,
    Transcripts,
    Recordings,
    Memories,
    People,
    Routines,
    RoutineRuns,
    Schedules,
    Prefs,
    Evals,
    Code,
    Creations,
}

impl Kind {
    fn merged(self) -> bool {
        matches!(
            self,
            Kind::Memories | Kind::People | Kind::Schedules | Kind::Routines
        )
    }

    fn cap(self) -> u64 {
        if self == Kind::Recordings {
            MAX_AUDIO_BYTES
        } else {
            MAX_FILE_BYTES
        }
    }
}

/// Settings files an export carries, in the config directory.
pub const PREFS_FILES: &[&str] = &[
    "model-prefs.json",
    "favorites.json",
    "idle.json",
    "embeddings.json",
    "voice.json",
    SETTINGS_FILE,
];
/// A research report's files (`research/<id>/`).
pub const RESEARCH_FILES: &[&str] = &[
    "job.json",
    "plan.json",
    "report.md",
    "sources.json",
    "cited.json",
    "events.jsonl",
];
/// A finished transcription's files (`jobs/<id>/`), but the recording.
pub const TRANSCRIPT_FILES: &[&str] = &[
    "job.json",
    "transcript.txt",
    "transcript.srt",
    "transcript.json",
];

/// An id as the agent makes them: letters, digits, `-` and `_`, starting with
/// a letter or digit, at most 64.
pub fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn json_id(name: &str) -> bool {
    name.strip_suffix(".json").is_some_and(safe_id)
}

/// `input.<ext>`: a job's recording.
fn audio_name(name: &str) -> bool {
    name.strip_prefix("input.").is_some_and(|ext| {
        !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

/// What `rel` (a `/`-separated path under the config directory) is, if it's
/// something an export may hold. Everything else -- tokens, keys, the TLS
/// key, apps, logs, models -- is `None`.
pub fn classify(rel: &str) -> Option<Kind> {
    if looks_secret(rel) {
        return None;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    let kind = match parts.as_slice() {
        ["memories.json"] => Kind::Memories,
        ["people.json"] => Kind::People,
        ["schedules.json"] => Kind::Schedules,
        [f] if PREFS_FILES.contains(f) => Kind::Prefs,
        ["routines", "routines.json"] => Kind::Routines,
        ["routines", "runs", f] if json_id(f) => Kind::RoutineRuns,
        ["evals", f] if json_id(f) => Kind::Evals,
        ["evals", "raw", f] if json_id(f) => Kind::Evals,
        ["research", id, f] if safe_id(id) && RESEARCH_FILES.contains(f) => Kind::Research,
        ["jobs", id, f] if safe_id(id) && TRANSCRIPT_FILES.contains(f) => Kind::Transcripts,
        ["jobs", id, f] if safe_id(id) && audio_name(f) => Kind::Recordings,
        ["code", id, "session.json"] if safe_id(id) => Kind::Code,
        ["creations", f]
            if f.strip_suffix(".txt").is_some_and(|h| {
                !h.is_empty() && h.len() <= 32 && h.chars().all(|c| c.is_ascii_hexdigit())
            }) =>
        {
            Kind::Creations
        }
        _ => return None,
    };
    Some(kind)
}

/// Names that hold credentials somewhere in the agent's config directory (or
/// look like they might): never exported or imported, whatever the allowlist
/// says.
pub fn looks_secret(rel: &str) -> bool {
    const NAMES: &[&str] = &[
        "agentd-tokens.json",
        "app-keys.json",
        "local-secret",
        "secrets.json",
        "desktop-token",
        "notify.json",
        "apns.toml",
        "tls-key.pem",
        "tls-cert.pem",
        "key",
        "claude",
        "claude-home",
        "hermes",
        "apps",
        "coding",
    ];
    rel.split('/').any(|part| {
        let p = part.to_ascii_lowercase();
        NAMES.contains(&p.as_str())
            || p.contains("token")
            || p.contains("secret")
            || p.contains("credential")
            || p.contains("password")
            || p.ends_with(".pem")
            || p.ends_with(".p8")
            || p.ends_with(".key")
            || p.starts_with("hf_")
    })
}

/// One file of an export: where it goes in the archive, and its bytes (read
/// from `from`, or made here, like `people.json` without its devices).
#[derive(Debug, Clone)]
pub struct Item {
    pub path: String,
    pub kind: Kind,
    pub from: Source,
}

#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}

fn regular_file(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file())
}

fn read_value(p: &Path) -> Option<Value> {
    std::fs::read(p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn dir_names(p: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(p)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir() || t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

const FINISHED_RESEARCH: &[&str] = &["done", "failed", "stopped"];

/// Everything an export of `config` holds (see the module docs).
pub fn collect(config: &Path) -> Vec<Item> {
    let mut items = Vec::new();
    let mut add = |rel: String, from: Source| {
        if let Some(kind) = classify(&rel) {
            if let Source::File(p) = &from {
                if !regular_file(p) {
                    return;
                }
            }
            items.push(Item {
                path: rel,
                kind,
                from,
            });
        }
    };
    for f in ["memories.json", "schedules.json"]
        .iter()
        .chain(PREFS_FILES)
    {
        add(f.to_string(), Source::File(config.join(f)));
    }
    // Names only: which device is whose stays on the box it was paired with.
    if let Some(book) = read_value(&config.join("people.json")) {
        let people = json!({ "people": book.get("people").cloned().unwrap_or(json!([])) });
        add(
            "people.json".into(),
            Source::Bytes(serde_json::to_vec_pretty(&people).unwrap_or_default()),
        );
    }
    add(
        "routines/routines.json".into(),
        Source::File(config.join("routines/routines.json")),
    );
    for name in dir_names(&config.join("routines/runs")) {
        let p = config.join("routines/runs").join(&name);
        if read_value(&p).is_some_and(|r| !r["finished"].is_null()) {
            add(format!("routines/runs/{name}"), Source::File(p));
        }
    }
    for name in dir_names(&config.join("evals")) {
        let p = config.join("evals").join(&name);
        if read_value(&p).is_some_and(|e| e["status"] != "running") {
            add(format!("evals/{name}"), Source::File(p));
        }
    }
    for name in dir_names(&config.join("evals/raw")) {
        add(
            format!("evals/raw/{name}"),
            Source::File(config.join("evals/raw").join(name)),
        );
    }
    for id in dir_names(&config.join("research")) {
        let d = config.join("research").join(&id);
        let finished = read_value(&d.join("job.json"))
            .is_some_and(|j| FINISHED_RESEARCH.contains(&j["state"].as_str().unwrap_or("")));
        if finished {
            for f in RESEARCH_FILES {
                add(format!("research/{id}/{f}"), Source::File(d.join(f)));
            }
        }
    }
    for id in dir_names(&config.join("jobs")) {
        let d = config.join("jobs").join(&id);
        let Some(job) = read_value(&d.join("job.json")) else {
            continue;
        };
        if job["status"] != "done" {
            continue;
        }
        for f in TRANSCRIPT_FILES {
            add(format!("jobs/{id}/{f}"), Source::File(d.join(f)));
        }
        // The recording only when the person kept it.
        if job["keep_audio"] == true {
            for f in dir_names(&d).into_iter().filter(|f| audio_name(f)) {
                add(format!("jobs/{id}/{f}"), Source::File(d.join(&f)));
            }
        }
    }
    for id in dir_names(&config.join("code")) {
        add(
            format!("code/{id}/session.json"),
            Source::File(config.join("code").join(&id).join("session.json")),
        );
    }
    for name in dir_names(&config.join("creations")) {
        add(
            format!("creations/{name}"),
            Source::File(config.join("creations").join(name)),
        );
    }
    items
}

fn sha256_of(mut r: impl Read) -> std::io::Result<(String, u64)> {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        ctx.update(&buf[..k]);
        n += k as u64;
    }
    Ok((hex(ctx.finish().as_ref()), n))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Hashes what's read through it, and counts it.
struct Hashing<R> {
    inner: R,
    ctx: ring::digest::Context,
    n: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let k = self.inner.read(buf)?;
        self.ctx.update(&buf[..k]);
        self.n += k as u64;
        Ok(k)
    }
}

fn append(
    tar: &mut tar::Builder<impl Write>,
    path: &str,
    len: u64,
    mtime: u64,
    data: impl Read,
) -> std::io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Regular);
    h.set_size(len);
    h.set_mode(0o600);
    h.set_mtime(mtime);
    tar.append_data(&mut h, path, data)
}

/// Write `items` as a `.tar.gz` to `out`, with `manifest.json` (each file's
/// size and SHA-256, as written) last. Each file is read once. Returns the
/// manifest.
pub fn write_archive(items: &[Item], box_name: &str, out: impl Write) -> std::io::Result<Value> {
    let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let mtime = now();
    let mut files = Vec::new();
    let mut counts: BTreeMap<Kind, usize> = BTreeMap::new();
    for item in items {
        let (reader, len): (Box<dyn Read>, u64) = match &item.from {
            Source::Bytes(b) => (Box::new(&b[..]), b.len() as u64),
            Source::File(p) => match std::fs::File::open(p) {
                Ok(f) => {
                    let len = f.metadata()?.len();
                    (Box::new(f), len)
                }
                // Gone since it was listed: left out.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            },
        };
        let mut hashing = Hashing {
            inner: reader.take(len),
            ctx: ring::digest::Context::new(&ring::digest::SHA256),
            n: 0,
        };
        append(&mut tar, &item.path, len, mtime, &mut hashing)?;
        if hashing.n != len {
            return Err(std::io::Error::other(format!(
                "{} changed while it was being exported; try again",
                item.path
            )));
        }
        let sha256 = hex(hashing.ctx.finish().as_ref());
        files.push(json!({ "path": item.path, "kind": item.kind, "bytes": len, "sha256": sha256 }));
        *counts.entry(item.kind).or_default() += 1;
    }
    let manifest = json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "agent_version": env!("CARGO_PKG_VERSION"),
        "created": now(),
        "box": box_name,
        "counts": counts,
        "files": files,
    });
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(std::io::Error::other)?;
    append(&mut tar, MANIFEST, bytes.len() as u64, mtime, &bytes[..])?;
    tar.into_inner()?.finish()?.flush()?;
    Ok(manifest)
}

/// `POST /export` (owners): the archive, as a download.
pub(crate) async fn post_export(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    CallerAuth(caller): CallerAuth,
) -> Response {
    let config = state.config_dir();
    let box_name = state.name().to_string();
    let made = tokio::task::spawn_blocking(move || -> std::io::Result<(std::fs::File, Value)> {
        let (file, path) =
            crate::fsio::private_temp_file_in(&temp_dir(&config), "tt-export-", ".tar.gz")?;
        let result = (|| {
            let manifest = write_archive(&collect(&config), &box_name, &file)?;
            let back = std::fs::File::open(&path)?;
            Ok((back, manifest))
        })();
        // Open (or failed): the name isn't needed any more.
        let _ = std::fs::remove_file(&path);
        result
    })
    .await
    .map_err(std::io::Error::other)
    .and_then(|r| r);
    let (file, manifest) = match made {
        Ok(v) => v,
        Err(e) => {
            crate::audit::record_as(&state, &caller.actor(), "export", "your data", "failed");
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't make the export: {e}"),
            )
            .into_response();
        }
    };
    let files = manifest["files"].as_array().map_or(0, Vec::len);
    crate::audit::record_as(
        &state,
        &caller.actor(),
        "export",
        &format!("{files} files"),
        "ok",
    );
    let len = file.metadata().map(|m| m.len()).ok();
    let file = tokio::fs::File::from_std(file);
    let stream = futures_util::stream::unfold(file, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 256 * 1024];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(bytes_of(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    let day = crate::schedules::local_now().3;
    let name = format!(
        "tt-station-{}-{day}.tar.gz",
        state
            .name()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            })
            .collect::<String>()
    );
    let mut res = axum::body::Body::from_stream(stream).into_response();
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/gzip"),
    );
    if let Ok(v) = header::HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = len {
        headers.insert(header::CONTENT_LENGTH, header::HeaderValue::from(len));
    }
    res
}

fn bytes_of(v: Vec<u8>) -> axum::body::Bytes {
    axum::body::Bytes::from(v)
}

// ---------------------------------------------------------------- import

/// A path inside an archive, checked: relative, `/`-separated, every part a
/// plain name (no `..`, `.`, empty part, backslash or NUL).
pub fn clean_path(raw: &[u8]) -> Result<String, String> {
    let s = std::str::from_utf8(raw).map_err(|_| "a file name that isn't UTF-8".to_string())?;
    let bad = s.is_empty()
        || s.starts_with('/')
        || s.contains('\\')
        || s.contains('\0')
        || s.split('/').any(|p| p.is_empty() || p == "." || p == "..");
    if bad {
        return Err(format!("an unsafe path in the archive: {s:?}"));
    }
    Ok(s.to_string())
}

/// What an import would do to one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Add,
    Overwrite,
    Same,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Counts {
    pub added: usize,
    pub overwritten: usize,
    pub unchanged: usize,
}

impl Counts {
    fn count(&mut self, c: Change) {
        match c {
            Change::Add => self.added += 1,
            Change::Overwrite => self.overwritten += 1,
            Change::Same => self.unchanged += 1,
        }
    }
}

/// What importing an archive would do (or did).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Preview {
    pub agent_version: Option<String>,
    pub format_version: u64,
    pub created: Option<u64>,
    #[serde(rename = "box")]
    pub box_name: Option<String>,
    /// Per kind of thing: reports, transcripts and sessions count once each;
    /// people, memories, schedules and routines count each entry.
    pub categories: BTreeMap<Kind, Counts>,
    /// What would be replaced (paths, or `kind:id`), the first 200.
    pub overwritten: Vec<String>,
    /// Left out, and why, the first 200.
    pub skipped: Vec<String>,
    pub bytes: u64,
}

/// What the box holds now of the things that merge by id, for the preview.
#[derive(Debug, Clone, Default)]
pub struct Current {
    pub people: Vec<Value>,
    pub memories: Vec<Value>,
    pub schedules: Vec<Value>,
    pub routines: Vec<Value>,
}

impl Current {
    pub fn of(state: &AppState) -> Current {
        Current {
            people: to_values(&state.people().list()),
            memories: to_values(&state.memories().list()),
            // Schedules live on as mode-switching routines (`crate::routines`).
            schedules: to_values(&crate::routines::as_schedules(state)),
            routines: to_values(&crate::routines::list(state)),
        }
    }

    fn of_kind(&self, kind: Kind) -> &[Value] {
        match kind {
            Kind::People => &self.people,
            Kind::Memories => &self.memories,
            Kind::Schedules => &self.schedules,
            _ => &self.routines,
        }
    }
}

fn to_values<T: Serialize>(v: &[T]) -> Vec<Value> {
    v.iter()
        .filter_map(|x| serde_json::to_value(x).ok())
        .collect()
}

/// A merged file's entries, typed-checked: `people.json` is `{people: [...]}`,
/// the others are arrays.
fn merged_entries(kind: Kind, bytes: &[u8]) -> Result<Vec<Value>, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let list = if kind == Kind::People {
        v.get("people").cloned().unwrap_or(json!([]))
    } else {
        v
    };
    // Through the box's own types, so defaults are filled in the same way as
    // what it already holds (and anything malformed is caught here).
    fn typed<T: serde::de::DeserializeOwned + Serialize>(v: Value) -> Option<Vec<Value>> {
        serde_json::from_value::<Vec<T>>(v)
            .ok()
            .map(|list| to_values(&list))
    }
    match kind {
        Kind::People => typed::<crate::people::Person>(list),
        Kind::Memories => typed::<crate::memory::Memory>(list),
        Kind::Schedules => typed::<crate::schedules::Schedule>(list),
        _ => typed::<crate::routines::Routine>(list),
    }
    .ok_or_else(|| "not in the expected shape".to_string())
}

/// A routine as the box would keep it: its `next_run` is worked out on each
/// box, and only real actions are kept (see `routines::merge_imported`).
fn comparable(kind: Kind, v: &Value) -> Value {
    let mut v = v.clone();
    if kind == Kind::Schedules {
        if let Some(o) = v.as_object_mut() {
            o.remove("last_run");
        }
    }
    if kind == Kind::Routines {
        if let Some(o) = v.as_object_mut() {
            o.remove("next_run");
            if let Some(Value::Array(allowed)) = o.get_mut("allowed") {
                allowed.retain(|a| {
                    a.as_str()
                        .is_some_and(|a| crate::routines::ACTIONS.contains(&a))
                });
            }
        }
    }
    v
}

/// One file the import will write (or merge).
#[derive(Debug, Clone)]
pub struct Planned {
    pub kind: Kind,
    pub change: Change,
}

/// The import worked out: what it would do, and which archive paths to write.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub preview: Preview,
    pub files: HashMap<String, Planned>,
}

/// A thing that comes in as several files: a job, a report, an evaluation.
fn group_of(path: &str, kind: Kind) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    match kind {
        Kind::Research | Kind::Transcripts | Kind::Recordings | Kind::Code => {
            format!("{}/{}", parts[0], parts.get(1).unwrap_or(&""))
        }
        Kind::Evals => format!(
            "evals/{}",
            parts.last().unwrap_or(&"").trim_end_matches(".json")
        ),
        _ => path.to_string(),
    }
}

/// Is a metadata file finished work? Unfinished jobs, reports, runs and
/// evaluations aren't imported (an import mustn't start anything).
fn finished(path: &str, kind: Kind, bytes: &[u8], id_dir: Option<&str>) -> Result<(), String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("{path}: {e}"))?;
    let ok = match kind {
        Kind::Transcripts => serde_json::from_value::<crate::jobs::Job>(v.clone()).is_ok_and(|j| {
            j.status == crate::jobs::JobStatus::Done && Some(j.id.as_str()) == id_dir
        }),
        Kind::Research => {
            FINISHED_RESEARCH.contains(&v["state"].as_str().unwrap_or(""))
                && v["id"].as_str() == id_dir
        }
        // A run's and an evaluation's id names its file, and the agent later
        // deletes `<dir>/<id>.json` by that id: it must be a plain id and the
        // file's own name, or an import could aim a delete anywhere.
        Kind::RoutineRuns => !v["finished"].is_null() && id_is_stem(path, &v),
        Kind::Evals if !path.starts_with("evals/raw/") => {
            v["status"] != "running" && id_is_stem(path, &v)
        }
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("{path}: not finished work, so not imported"))
    }
}

/// `v["id"]` is a [`safe_id`] and `path`'s file name is `<id>.json`.
fn id_is_stem(path: &str, v: &Value) -> bool {
    let stem = path
        .rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".json"));
    v["id"]
        .as_str()
        .is_some_and(|id| safe_id(id) && Some(id) == stem)
}

fn open_archive(
    path: &Path,
) -> std::io::Result<tar::Archive<flate2::read::GzDecoder<std::fs::File>>> {
    Ok(tar::Archive::new(flate2::read::GzDecoder::new(
        std::fs::File::open(path)?,
    )))
}

fn push_capped(list: &mut Vec<String>, s: String) {
    if list.len() < 200 {
        list.push(s);
    }
}

/// Read the archive at `archive` and work out what importing it into `config`
/// (holding `current`) would do. Fails, before anything is written, on an
/// unsafe path, a link or device, anything too big, a damaged file, or an
/// archive that isn't an export.
pub fn inspect(archive: &Path, config: &Path, current: &Current) -> Result<Plan, String> {
    let mut ar = open_archive(archive).map_err(|e| e.to_string())?;
    let mut manifest: Option<Value> = None;
    let mut seen: HashSet<String> = HashSet::new();
    // path → (kind, sha256, change, merged entries)
    struct Found {
        kind: Kind,
        sha256: String,
        change: Change,
        merged: Vec<(String, Change)>,
    }
    let mut found: Vec<(String, Found)> = Vec::new();
    let mut skipped = Vec::new();
    let mut bad_groups: HashMap<String, String> = HashMap::new();
    let mut total = 0u64;
    let entries = ar
        .entries()
        .map_err(|e| format!("not a .tar.gz archive: {e}"))?;
    for (n, entry) in entries.enumerate() {
        if n >= MAX_ENTRIES {
            return Err(format!("more than {MAX_ENTRIES} files in the archive"));
        }
        let mut entry = entry.map_err(|e| format!("the archive is damaged: {e}"))?;
        let path = clean_path(&entry.path_bytes())?;
        let kind_of_entry = entry.header().entry_type();
        if kind_of_entry.is_dir() {
            continue;
        }
        if !kind_of_entry.is_file() {
            return Err(format!(
                "{path} is a link or special file; an export only holds plain files"
            ));
        }
        if !seen.insert(path.clone()) {
            return Err(format!("{path} is in the archive twice"));
        }
        let size = entry.size();
        total = total.saturating_add(size);
        if total > MAX_TOTAL_BYTES {
            return Err("the archive unpacks to more than 32 GB".into());
        }
        if path == MANIFEST {
            if size > MAX_FILE_BYTES {
                return Err("the manifest is too big".into());
            }
            let mut b = Vec::new();
            entry.read_to_end(&mut b).map_err(|e| e.to_string())?;
            let m: Value =
                serde_json::from_slice(&b).map_err(|e| format!("the manifest is damaged: {e}"))?;
            if m["format"] != FORMAT {
                return Err("this isn't a TT Station export".into());
            }
            if m["format_version"].as_u64().unwrap_or(0) > FORMAT_VERSION as u64 {
                return Err("this export is from a newer TT Station; update this box first".into());
            }
            manifest = Some(m);
            continue;
        }
        let Some(kind) = classify(&path) else {
            push_capped(
                &mut skipped,
                format!("{path}: not something an export holds"),
            );
            continue;
        };
        if kind == Kind::Code {
            // A Code session's record carries its folder and what it may do
            // unasked; taken from an archive as-is it would hand those to
            // whoever made the archive. It's in an export as a record, and
            // isn't brought back.
            push_capped(
                &mut skipped,
                format!("{path}: Code sessions aren't imported"),
            );
            continue;
        }
        if size > kind.cap() {
            return Err(format!("{path} is too big ({size} bytes)"));
        }
        let target = config.join(&path);
        let (sha256, change, merged) = if kind == Kind::Recordings {
            let (sha, _) = sha256_of(&mut entry).map_err(|e| e.to_string())?;
            let change = file_change(&target, &sha);
            (sha, change, Vec::new())
        } else {
            let mut b = Vec::with_capacity(size as usize);
            (&mut entry)
                .take(kind.cap())
                .read_to_end(&mut b)
                .map_err(|e| e.to_string())?;
            let (sha, _) = sha256_of(&b[..]).map_err(|e| e.to_string())?;
            let parts: Vec<&str> = path.split('/').collect();
            let meta = match kind {
                Kind::Transcripts | Kind::Research => parts.get(2) == Some(&"job.json"),
                Kind::RoutineRuns => true,
                Kind::Evals => true,
                _ => false,
            };
            if meta {
                if let Err(why) = finished(&path, kind, &b, parts.get(1).copied()) {
                    bad_groups.insert(group_of(&path, kind), why);
                }
            }
            if kind.merged() {
                match merged_entries(kind, &b) {
                    Ok(list) => {
                        let have = current.of_kind(kind);
                        let merged = list
                            .iter()
                            .filter_map(|v| v["id"].as_str().map(|id| (id.to_string(), v)))
                            .map(|(id, v)| {
                                let change = match have.iter().find(|h| h["id"] == v["id"]) {
                                    None => Change::Add,
                                    Some(h) if comparable(kind, h) == comparable(kind, v) => {
                                        Change::Same
                                    }
                                    Some(_) => Change::Overwrite,
                                };
                                (id, change)
                            })
                            .collect();
                        (sha, Change::Overwrite, merged)
                    }
                    Err(why) => {
                        push_capped(&mut skipped, format!("{path}: {why}"));
                        continue;
                    }
                }
            } else {
                if path.ends_with(".json") && serde_json::from_slice::<Value>(&b).is_err() {
                    push_capped(&mut skipped, format!("{path}: not valid JSON"));
                    continue;
                }
                let change = file_change(&target, &sha);
                (sha, change, Vec::new())
            }
        };
        found.push((
            path,
            Found {
                kind,
                sha256,
                change,
                merged,
            },
        ));
    }
    let manifest = manifest.ok_or("this isn't a TT Station export (it has no manifest)")?;
    let listed: HashMap<&str, &str> = manifest["files"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|f| Some((f["path"].as_str()?, f["sha256"].as_str()?)))
                .collect()
        })
        .unwrap_or_default();
    let mut plan = Plan::default();
    let p = &mut plan.preview;
    p.agent_version = manifest["agent_version"].as_str().map(String::from);
    p.format_version = manifest["format_version"].as_u64().unwrap_or(0);
    p.created = manifest["created"].as_u64();
    p.box_name = manifest["box"].as_str().map(String::from);
    p.bytes = total;
    p.skipped = skipped;
    // Groups: a job, report or evaluation counts once, as its "most changed" file.
    let mut groups: BTreeMap<String, (Kind, Change)> = BTreeMap::new();
    for (path, f) in found {
        match listed.get(path.as_str()) {
            Some(sha) if *sha == f.sha256 => {}
            Some(_) => return Err(format!("{path} is damaged (it doesn't match the manifest)")),
            None => {
                push_capped(&mut p.skipped, format!("{path}: not in the manifest"));
                continue;
            }
        }
        let group = group_of(&path, f.kind);
        if let Some(why) = bad_groups.get(&group) {
            push_capped(&mut p.skipped, why.clone());
            continue;
        }
        if matches!(
            f.kind,
            Kind::Transcripts | Kind::Recordings | Kind::Research
        ) && !listed.contains_key(job_json_of(&path).as_str())
        {
            push_capped(
                &mut p.skipped,
                format!("{path}: its job.json isn't in the archive"),
            );
            continue;
        }
        if f.kind.merged() {
            let counts = p.categories.entry(f.kind).or_default();
            for (id, change) in &f.merged {
                counts.count(*change);
                if *change == Change::Overwrite {
                    push_capped(&mut p.overwritten, format!("{}:{id}", kind_name(f.kind)));
                }
            }
        } else {
            if f.change == Change::Overwrite {
                push_capped(&mut p.overwritten, path.clone());
            }
            let g = groups
                .entry(format!("{}:{group}", kind_name(f.kind)))
                .or_insert((f.kind, f.change));
            g.1 = combine(g.1, f.change);
        }
        plan.files.insert(
            path,
            Planned {
                kind: f.kind,
                change: f.change,
            },
        );
    }
    for (_, (kind, change)) in groups {
        p.categories.entry(kind).or_default().count(change);
    }
    Ok(plan)
}

/// `jobs/<id>/x` → `jobs/<id>/job.json` (what must come with it).
fn job_json_of(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    format!("{}/{}/job.json", parts[0], parts.get(1).unwrap_or(&""))
}

fn kind_name(k: Kind) -> String {
    serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// A group of files is "added" only when all of it is new, "unchanged" only
/// when all of it is the same.
fn combine(a: Change, b: Change) -> Change {
    if a == b {
        a
    } else {
        Change::Overwrite
    }
}

fn file_change(target: &Path, sha: &str) -> Change {
    if !regular_file(target) {
        return Change::Add;
    }
    match std::fs::File::open(target).and_then(sha256_of) {
        Ok((have, _)) if have == sha => Change::Same,
        _ => Change::Overwrite,
    }
}

/// Do what `plan` says: write each new or changed file into the config
/// directory, merge the merged kinds into the box's stores, and pick up the
/// imported jobs. The archive is read again (it's the agent's own private
/// temp file, checked by [`inspect`]).
///
/// `actor` (who imported it) is recorded in the activity log for a setting
/// that is logged when changed by hand: who may start and stop models.
pub fn apply(
    state: &AppState,
    archive: &Path,
    plan: &Plan,
    actor: &crate::audit::Actor,
) -> Result<Preview, String> {
    let config = state.config_dir();
    let was_control = settings_in(&config).model_control;
    let result = apply_files(state, archive, plan);
    let control = settings_in(&config).model_control;
    if control != was_control {
        crate::audit::record_as(
            state,
            actor,
            "model_control",
            &format!(
                "{} -> {} (imported)",
                was_control.as_str(),
                control.as_str()
            ),
            "ok",
        );
    }
    result
}

fn apply_files(state: &AppState, archive: &Path, plan: &Plan) -> Result<Preview, String> {
    let config = state.config_dir();
    let mut ar = open_archive(archive).map_err(|e| e.to_string())?;
    let mut jobs_written: HashSet<String> = HashSet::new();
    let mut merged: BTreeMap<Kind, Counts> = BTreeMap::new();
    for entry in ar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = clean_path(&entry.path_bytes())?;
        let Some(planned) = plan.files.get(&path) else {
            continue;
        };
        if planned.kind.merged() {
            let mut b = Vec::new();
            (&mut entry)
                .take(MAX_FILE_BYTES)
                .read_to_end(&mut b)
                .map_err(|e| e.to_string())?;
            let list = Value::Array(merged_entries(planned.kind, &b)?);
            let (added, updated) = match planned.kind {
                Kind::People => state
                    .people()
                    .merge_imported(serde_json::from_value(list).map_err(|e| e.to_string())?),
                Kind::Memories => state
                    .memories()
                    .merge_imported(serde_json::from_value(list).map_err(|e| e.to_string())?),
                Kind::Schedules => state
                    .schedules()
                    .merge_imported(serde_json::from_value(list).map_err(|e| e.to_string())?),
                _ => crate::routines::merge_imported(
                    state,
                    serde_json::from_value(list).map_err(|e| e.to_string())?,
                ),
            };
            let c = merged.entry(planned.kind).or_default();
            c.added += added;
            c.overwritten += updated;
            continue;
        }
        if planned.change == Change::Same {
            continue;
        }
        let target = config.join(&path);
        crate::fsio::write_atomic_from(
            &target,
            (&mut entry).take(planned.kind.cap()),
            crate::fsio::PRIVATE,
        )
        .map_err(|e| format!("couldn't write {path}: {e}"))?;
        if planned.kind == Kind::Transcripts || planned.kind == Kind::Recordings {
            if let Some(id) = path.split('/').nth(1) {
                jobs_written.insert(id.to_string());
            }
        }
    }
    if let Some(jobs) = state.jobs() {
        for id in &jobs_written {
            jobs.adopt(id);
        }
    }
    let mut done = plan.preview.clone();
    // What the stores actually did (one may have been full, or turned an
    // entry away): the rest is unchanged.
    for (kind, c) in merged {
        let entry = done.categories.entry(kind).or_default();
        let total = entry.added + entry.overwritten + entry.unchanged;
        entry.added = c.added;
        entry.overwritten = c.overwritten;
        entry.unchanged = total.saturating_sub(c.added + c.overwritten);
    }
    Ok(done)
}

#[derive(Debug, Deserialize)]
pub struct ImportQuery {
    #[serde(default)]
    pub dry_run: Option<bool>,
}

/// `POST /import[?dry_run=true]` (owners), body = an export. `dry_run` says
/// what would happen without doing it.
pub(crate) async fn post_import(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: OwnerAuth,
    CallerAuth(caller): CallerAuth,
    axum::extract::Query(q): axum::extract::Query<ImportQuery>,
    body: axum::body::Body,
) -> Result<Json<Value>, ApiError> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    let dry_run = q.dry_run.unwrap_or(false);
    let (file, path) =
        crate::fsio::private_temp_file_in(&temp_dir(&state.config_dir()), "tt-import-", ".tar.gz")
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // Removed however this ends.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(path.clone());
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = body.into_data_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| err(StatusCode::BAD_REQUEST, format!("upload interrupted: {e}")))?;
        written += chunk.len() as u64;
        if written > MAX_IMPORT_BYTES {
            return Err(err(
                StatusCode::PAYLOAD_TOO_LARGE,
                "That archive is too big (6 GB at most).",
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }
    file.flush().await.ok();
    drop(file);
    if written == 0 {
        return Err(err(StatusCode::BAD_REQUEST, "The upload was empty."));
    }
    let current = Current::of(&state);
    let st = state.clone();
    let actor = caller.actor();
    let result = tokio::task::spawn_blocking(move || {
        let plan = inspect(&path, &st.config_dir(), &current)?;
        if dry_run {
            return Ok(plan.preview);
        }
        apply(&st, &path, &plan, &actor)
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let preview = match result {
        Ok(p) => p,
        Err(why) => {
            if !dry_run {
                crate::audit::record_as(&state, &caller.actor(), "import", &why, "refused");
            }
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("Couldn't import that: {why}."),
            ));
        }
    };
    if !dry_run {
        let (added, overwritten) = preview
            .categories
            .values()
            .fold((0, 0), |(a, o), c| (a + c.added, o + c.overwritten));
        crate::audit::record_as(
            &state,
            &caller.actor(),
            "import",
            &format!("{added} added, {overwritten} replaced"),
            "ok",
        );
    }
    Ok(Json(json!({ "dry_run": dry_run, "preview": preview })))
}

// ---------------------------------------------------------------- retention

/// Whether a research report (its `job.json`) finished before `cutoff`.
pub fn research_expired(job: &Value, cutoff: u64) -> bool {
    FINISHED_RESEARCH.contains(&job["state"].as_str().unwrap_or(""))
        && job["finished"]
            .as_u64()
            .or_else(|| job["created"].as_u64())
            .is_some_and(|t| t < cutoff)
}

/// Delete transcripts and research reports that finished more than `days`
/// days ago. Returns (transcripts, reports) deleted.
pub fn expire(state: &AppState, days: u32) -> (usize, usize) {
    let cutoff = now().saturating_sub(days as u64 * 86_400);
    let transcripts = state.jobs().map_or(0, |j| j.expire(cutoff));
    let root = state.config_dir().join("research");
    let mut reports = 0;
    for id in dir_names(&root) {
        if !safe_id(&id) {
            continue;
        }
        let d = root.join(&id);
        if read_value(&d.join("job.json")).is_some_and(|j| research_expired(&j, cutoff))
            && std::fs::remove_dir_all(&d).is_ok()
        {
            reports += 1;
        }
    }
    (transcripts, reports)
}

/// Apply the retention setting a few minutes after start and then hourly.
pub async fn retention_forever(state: AppState) {
    tokio::time::sleep(Duration::from_secs(180)).await;
    loop {
        if let Some(days) = settings(&state).retention_days.filter(|d| *d > 0) {
            let st = state.clone();
            let (t, r) = tokio::task::spawn_blocking(move || expire(&st, days))
                .await
                .unwrap_or((0, 0));
            if t + r > 0 {
                crate::log::info(format!(
                    "retention: deleted {t} transcript(s) and {r} research report(s) older than {days} days"
                ));
                crate::audit::record_as(
                    &state,
                    &crate::audit::Actor::the_box(),
                    "retention",
                    &format!("{t} transcripts, {r} reports older than {days} days"),
                    "deleted",
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Secret-looking strings planted in a config dir: none may reach an export.
    const SECRETS: &[(&str, &str)] = &[
        ("agentd-tokens.json", "SECRET-device-token"),
        ("app-keys.json", "SECRET-app-key"),
        ("tls-key.pem", "SECRET-tls-key"),
        ("tls-cert.pem", "SECRET-tls-cert"),
        ("apns.toml", "SECRET-apns"),
        ("AuthKey_ABC.p8", "SECRET-apns-key"),
        ("local-secret", "SECRET-local"),
        ("secrets.json", "SECRET-hf-token"),
        ("desktop-token", "SECRET-desktop"),
        ("notify.json", "SECRET-push-device"),
        ("coding/key", "SECRET-coding-key"),
        ("apps/openwebui/key", "SECRET-app"),
        ("hermes/config.json", "SECRET-hermes"),
        ("code/1-a/claude/.credentials.json", "SECRET-claude"),
        ("code/1-a/events.jsonl", "SECRET-conversation"),
    ];

    fn write(root: &Path, rel: &str, body: impl AsRef<[u8]>) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// A box's config dir with one of everything, plus secrets and unfinished work.
    fn sample_box(root: &Path) {
        for (rel, body) in SECRETS {
            write(root, rel, body);
        }
        write(
            root,
            "people.json",
            serde_json::to_vec(&json!({
                "people": [{ "id": "p1", "name": "Ada", "added": 1 }],
                "phones": { "SECRET-phone-digest": "p1" }
            }))
            .unwrap(),
        );
        write(
            root,
            "memories.json",
            serde_json::to_vec(&json!([{ "id": "m1", "text": "Likes tea", "added": 1 }])).unwrap(),
        );
        write(
            root,
            "schedules.json",
            serde_json::to_vec(&json!([{ "id": "s1", "mode": "chat", "time": "07:30", "days": [1, 2], "enabled": true }]))
                .unwrap(),
        );
        write(
            root,
            "routines/routines.json",
            serde_json::to_vec(&json!([{ "id": "r1", "name": "Morning", "goal": "News", "created": 1, "allowed": ["start_model", "rm -rf"] }]))
                .unwrap(),
        );
        write(
            root,
            "routines/runs/1-done.json",
            r#"{"id":"1-done","routine":"r1","name":"n","trigger":"manual","started":1,"finished":2,"status":"done"}"#,
        );
        write(
            root,
            "routines/runs/1-live.json",
            r#"{"id":"1-live","routine":"r1","name":"n","trigger":"manual","started":1,"status":"running"}"#,
        );
        write(root, "model-prefs.json", r#"{"chat":"Qwen3-32B"}"#);
        write(
            root,
            SETTINGS_FILE,
            r#"{"keep_audio":true,"model_control":"owners"}"#,
        );
        write(
            root,
            "evals/1-e.json",
            r#"{"id":"1-e","model":"m","started":1,"status":"done"}"#,
        );
        write(
            root,
            "evals/2-e.json",
            r#"{"id":"2-e","model":"m","started":1,"status":"running"}"#,
        );
        write(root, "evals/raw/1-e.json", "[]");
        write(
            root,
            "research/1-r/job.json",
            r#"{"id":"1-r","question":"q","depth":"quick","created":1,"state":"done","finished":2}"#,
        );
        write(root, "research/1-r/report.md", "# Report");
        write(root, "research/1-r/sources.json", "[]");
        write(
            root,
            "research/2-r/job.json",
            r#"{"id":"2-r","question":"q","depth":"quick","created":1,"state":"running"}"#,
        );
        let job = |id: &str, status: &str, keep: bool| {
            json!({ "id": id, "kind": "transcribe", "title": "t", "when": "now", "status": status,
                "done": 1, "total": 1, "duration_s": 1.0, "created": 1, "started": 1, "finished": 2,
                "error": null, "to_documents": false, "keep_audio": keep })
            .to_string()
        };
        write(
            root,
            "jobs/aaaa1111/job.json",
            job("aaaa1111", "done", false),
        );
        write(root, "jobs/aaaa1111/transcript.txt", "[00:00:00] Hello.");
        write(root, "jobs/aaaa1111/input.ogg", "AUDIO-not-kept");
        write(
            root,
            "jobs/bbbb2222/job.json",
            job("bbbb2222", "done", true),
        );
        write(root, "jobs/bbbb2222/transcript.txt", "[00:00:00] Kept.");
        write(root, "jobs/bbbb2222/input.m4a", "AUDIO-kept");
        write(
            root,
            "jobs/cccc3333/job.json",
            job("cccc3333", "queued", false),
        );
        write(root, "jobs/cccc3333/input.ogg", "AUDIO-queued");
        write(
            root,
            "code/1-a/session.json",
            r#"{"id":"1-a","folder":"/home/x/proj","title":"t","created":1,"sandbox":true,"policy":"ask"}"#,
        );
        write(root, "code/1-a/stderr.log", "log");
        write(root, "creations/0123abcd.txt", "a red fox");
        write(root, "creations/0123abcd.jpg", "JPEG");
        write(root, "metrics-history.json", "{}");
    }

    fn export_to(root: &Path, out: &Path) -> Value {
        let f = std::fs::File::create(out).unwrap();
        write_archive(&collect(root), "qb2", f).unwrap()
    }

    fn unpacked(archive: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut ar = open_archive(archive).unwrap();
        let mut out = BTreeMap::new();
        for e in ar.entries().unwrap() {
            let mut e = e.unwrap();
            let p = String::from_utf8(e.path_bytes().to_vec()).unwrap();
            let mut b = Vec::new();
            e.read_to_end(&mut b).unwrap();
            out.insert(p, b);
        }
        out
    }

    #[test]
    fn the_allowlist_takes_data_and_never_secrets() {
        for (rel, _) in SECRETS {
            assert_eq!(classify(rel), None, "{rel}");
        }
        for rel in [
            "../people.json",
            "jobs/../tls-key.pem",
            "jobs/a b/job.json",
            "jobs/.hidden/job.json",
            "code/1-a/events.jsonl",
            "creations/0123.jpg",
            "research/1-r/other.bin",
            "metrics-history.json",
            "jobs/x/input.verylongext",
        ] {
            assert_eq!(classify(rel), None, "{rel}");
        }
        assert_eq!(classify("jobs/aaaa1111/input.ogg"), Some(Kind::Recordings));
        assert_eq!(classify("research/1-r/report.md"), Some(Kind::Research));
        assert_eq!(classify("code/1-a/session.json"), Some(Kind::Code));
        assert_eq!(classify("data-settings.json"), Some(Kind::Prefs));
    }

    #[test]
    fn an_export_holds_the_data_and_no_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("box");
        sample_box(&root);
        let out = tmp.path().join("x.tar.gz");
        let manifest = export_to(&root, &out);
        let files = unpacked(&out);
        let names: Vec<&str> = files.keys().map(String::as_str).collect();
        for want in [
            MANIFEST,
            "people.json",
            "memories.json",
            "schedules.json",
            "routines/routines.json",
            "routines/runs/1-done.json",
            "model-prefs.json",
            SETTINGS_FILE,
            "evals/1-e.json",
            "evals/raw/1-e.json",
            "research/1-r/job.json",
            "research/1-r/report.md",
            "jobs/aaaa1111/job.json",
            "jobs/aaaa1111/transcript.txt",
            "jobs/bbbb2222/input.m4a",
            "code/1-a/session.json",
            "creations/0123abcd.txt",
        ] {
            assert!(names.contains(&want), "{want} missing from {names:?}");
        }
        for not in [
            "routines/runs/1-live.json",
            "evals/2-e.json",
            "research/2-r/job.json",
            "jobs/aaaa1111/input.ogg",
            "jobs/cccc3333/job.json",
            "code/1-a/stderr.log",
            "creations/0123abcd.jpg",
            "metrics-history.json",
        ] {
            assert!(!names.contains(&not), "{not} shouldn't be exported");
        }
        // Not a byte of any secret, anywhere (the manifest included).
        let mut all = Vec::new();
        for b in files.values() {
            all.extend_from_slice(b);
        }
        let text = String::from_utf8_lossy(&all);
        for (rel, secret) in SECRETS {
            assert!(!text.contains(secret), "{rel}'s contents leaked");
        }
        assert!(
            !text.contains("SECRET-phone-digest"),
            "which phone is whose stays"
        );
        assert!(!text.contains("AUDIO-not-kept") && !text.contains("AUDIO-queued"));
        assert_eq!(manifest["format"], FORMAT);
        assert_eq!(manifest["format_version"], FORMAT_VERSION);
        assert_eq!(manifest["agent_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            manifest["files"].as_array().unwrap().len(),
            files.len() - 1,
            "every file but the manifest is listed"
        );
    }

    /// A .tar.gz with raw entries, paths written as given (no checks).
    fn raw_archive(out: &Path, entries: &[(&str, tar::EntryType, &[u8])]) {
        let gz = flate2::write::GzEncoder::new(
            std::fs::File::create(out).unwrap(),
            flate2::Compression::fast(),
        );
        let mut b = tar::Builder::new(gz);
        for (path, kind, body) in entries {
            let mut h = tar::Header::new_old();
            let name = &mut h.as_old_mut().name;
            name[..path.len()].copy_from_slice(path.as_bytes());
            h.set_entry_type(*kind);
            h.set_size(body.len() as u64);
            h.set_mode(0o600);
            if *kind == tar::EntryType::Symlink {
                h.set_link_name("/etc/passwd").unwrap();
            }
            h.set_cksum();
            b.append(&h, *body).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn unsafe_paths_links_and_damage_fail_the_whole_import() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("box");
        std::fs::create_dir_all(&config).unwrap();
        let manifest =
            serde_json::to_vec(&json!({ "format": FORMAT, "format_version": 1, "files": [] }))
                .unwrap();
        let reg = tar::EntryType::Regular;
        for (name, bad) in [
            ("dotdot", "../../.bashrc"),
            ("nested", "jobs/../../escape.json"),
            ("absolute", "/etc/cron.d/x"),
            ("dot", "./memories.json"),
            ("backslash", "jobs\\..\\x"),
        ] {
            let out = tmp.path().join(format!("{name}.tar.gz"));
            raw_archive(&out, &[(MANIFEST, reg, &manifest), (bad, reg, b"x")]);
            let e = inspect(&out, &config, &Current::default()).unwrap_err();
            assert!(e.contains("unsafe path"), "{name}: {e}");
        }
        let out = tmp.path().join("link.tar.gz");
        raw_archive(
            &out,
            &[
                (MANIFEST, reg, &manifest),
                ("memories.json", tar::EntryType::Symlink, b""),
            ],
        );
        assert!(inspect(&out, &config, &Current::default())
            .unwrap_err()
            .contains("link"));
        // A file that doesn't match its manifest entry.
        let lying = serde_json::to_vec(&json!({ "format": FORMAT, "format_version": 1,
            "files": [{ "path": "model-prefs.json", "sha256": "00" }] }))
        .unwrap();
        let out = tmp.path().join("damaged.tar.gz");
        raw_archive(
            &out,
            &[(MANIFEST, reg, &lying), ("model-prefs.json", reg, b"{}")],
        );
        assert!(inspect(&out, &config, &Current::default())
            .unwrap_err()
            .contains("damaged"));
        // Not an export at all.
        let out = tmp.path().join("other.tar.gz");
        raw_archive(&out, &[("model-prefs.json", reg, b"{}")]);
        assert!(inspect(&out, &config, &Current::default())
            .unwrap_err()
            .contains("manifest"));
        // Nothing was written by any of it.
        assert_eq!(std::fs::read_dir(&config).unwrap().count(), 0);
        assert!(!tmp.path().join("escape.json").exists());
    }

    #[test]
    fn unknown_and_unfinished_files_are_skipped_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("box");
        let reg = tar::EntryType::Regular;
        let job = br#"{"id":"dddd4444","kind":"transcribe","title":"t","when":"now","status":"queued","done":0,"total":0,"duration_s":null,"created":1,"started":null,"finished":null,"error":null}"#;
        let files: Vec<(&str, &[u8])> = vec![
            ("app-keys.json", b"{\"k\":1}"),
            ("jobs/dddd4444/job.json", job),
            ("jobs/dddd4444/input.ogg", b"AUDIO"),
        ];
        let listed: Vec<Value> = files
            .iter()
            .map(|(p, b)| json!({ "path": p, "sha256": sha256_of(*b).unwrap().0 }))
            .collect();
        let manifest =
            serde_json::to_vec(&json!({ "format": FORMAT, "format_version": 1, "files": listed }))
                .unwrap();
        let mut entries: Vec<(&str, tar::EntryType, &[u8])> = vec![(MANIFEST, reg, &manifest)];
        entries.extend(files.iter().map(|(p, b)| (*p, reg, *b)));
        let out = tmp.path().join("a.tar.gz");
        raw_archive(&out, &entries);
        let plan = inspect(&out, &config, &Current::default()).unwrap();
        assert!(plan.files.is_empty(), "{:?}", plan.files);
        let skipped = plan.preview.skipped.join("\n");
        assert!(skipped.contains("app-keys.json"), "{skipped}");
        assert!(skipped.contains("not finished"), "{skipped}");
    }

    #[test]
    fn runs_and_evals_whose_id_isnt_their_file_name_and_code_sessions_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("box");
        let reg = tar::EntryType::Regular;
        let files: Vec<(&str, &[u8])> = vec![
            // An id that walks out of runs/: pruning would delete it.
            (
                "routines/runs/1-evil.json",
                br#"{"id":"../../memories","routine":"r","name":"n","trigger":"manual","started":1,"finished":2,"status":"done"}"#,
            ),
            // A plain id that isn't the file's name.
            (
                "routines/runs/2-a.json",
                br#"{"id":"2-b","routine":"r","name":"n","trigger":"manual","started":1,"finished":2,"status":"done"}"#,
            ),
            (
                "routines/runs/3-ok.json",
                br#"{"id":"3-ok","routine":"r","name":"n","trigger":"manual","started":1,"finished":2,"status":"done"}"#,
            ),
            (
                "evals/4-e.json",
                br#"{"id":"../x","model":"m","started":1,"status":"done"}"#,
            ),
            (
                "code/5-c/session.json",
                br#"{"id":"other","folder":"/","sandbox":false,"policy":"always","always":["Bash"]}"#,
            ),
        ];
        let listed: Vec<Value> = files
            .iter()
            .map(|(p, b)| json!({ "path": p, "sha256": sha256_of(*b).unwrap().0 }))
            .collect();
        let manifest =
            serde_json::to_vec(&json!({ "format": FORMAT, "format_version": 1, "files": listed }))
                .unwrap();
        let mut entries: Vec<(&str, tar::EntryType, &[u8])> = vec![(MANIFEST, reg, &manifest)];
        entries.extend(files.iter().map(|(p, b)| (*p, reg, *b)));
        let out = tmp.path().join("a.tar.gz");
        raw_archive(&out, &entries);
        let plan = inspect(&out, &config, &Current::default()).unwrap();
        let mut planned: Vec<&String> = plan.files.keys().collect();
        planned.sort();
        assert_eq!(planned, vec!["routines/runs/3-ok.json"]);
        let skipped = plan.preview.skipped.join("\n");
        assert!(skipped.contains("1-evil.json"), "{skipped}");
        assert!(skipped.contains("2-a.json"), "{skipped}");
        assert!(skipped.contains("4-e.json"), "{skipped}");
        assert!(
            skipped.contains("Code sessions aren't imported"),
            "{skipped}"
        );
    }

    fn state_at(root: &Path) -> AppState {
        AppState::new(
            "t".into(),
            "1xBH".into(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_config_dir(root.to_path_buf())
        .with_people(crate::people::People::open(root.join("people.json")))
        .with_memories(crate::memory::Memories::open(root.join("memories.json")))
        .with_schedules(crate::schedules::Schedules::open(
            root.join("schedules.json"),
        ))
        .with_jobs(crate::jobs::Jobs::open(root.join("jobs")))
    }

    #[test]
    fn an_export_restores_into_another_box_and_again_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        sample_box(&a);
        let out = tmp.path().join("x.tar.gz");
        export_to(&a, &out);

        // The new box already has a different memory and an older prefs file.
        write(&b, "model-prefs.json", r#"{"chat":"Old"}"#);
        let state = state_at(&b);
        state.memories().add("Has a cat", 5, None).unwrap();

        let preview = inspect(&out, &b, &Current::of(&state)).unwrap().preview;
        assert_eq!(preview.box_name.as_deref(), Some("qb2"));
        let c = |k: Kind| preview.categories.get(&k).cloned().unwrap_or_default();
        assert_eq!(c(Kind::Transcripts).added, 2);
        assert_eq!(c(Kind::Recordings).added, 1);
        assert_eq!(c(Kind::Research).added, 1);
        assert_eq!(c(Kind::People).added, 1);
        assert_eq!(c(Kind::Memories).added, 1);
        assert_eq!(c(Kind::Prefs).overwritten, 1);
        assert!(preview
            .overwritten
            .contains(&"model-prefs.json".to_string()));
        assert!(
            !b.join("jobs/aaaa1111").exists(),
            "a dry run writes nothing"
        );

        let plan = inspect(&out, &b, &Current::of(&state)).unwrap();
        apply(&state, &out, &plan, &crate::audit::Actor::the_box()).unwrap();
        // Who may start models changed with it, and the activity log says so.
        let (log, _) = state.audit().page(0, 50);
        assert!(
            log.iter()
                .any(|e| e.action == "model_control" && e.target == "anyone -> owners (imported)"),
            "{log:?}"
        );
        for rel in [
            "jobs/aaaa1111/transcript.txt",
            "jobs/bbbb2222/input.m4a",
            "research/1-r/report.md",
            "creations/0123abcd.txt",
            "evals/1-e.json",
            "model-prefs.json",
        ] {
            assert_eq!(
                std::fs::read(b.join(rel)).unwrap(),
                std::fs::read(a.join(rel)).unwrap(),
                "{rel}"
            );
        }
        assert!(!b.join("jobs/aaaa1111/input.ogg").exists());
        assert!(
            !b.join("code/1-a/session.json").exists(),
            "Code sessions aren't imported"
        );
        // Merged by id: the box's own memory stays beside the imported one.
        let texts: Vec<String> = state
            .memories()
            .list()
            .into_iter()
            .map(|m| m.text)
            .collect();
        assert!(
            texts.contains(&"Has a cat".to_string()) && texts.contains(&"Likes tea".to_string())
        );
        assert_eq!(state.people().list()[0].name, "Ada");
        assert!(
            crate::routines::as_schedules(&state)
                .iter()
                .any(|s| s.id == "s1"),
            "the schedule came in as a routine"
        );
        let routines = crate::routines::list(&state);
        let r1 = routines.iter().find(|r| r.id == "r1").unwrap();
        assert_eq!(
            r1.allowed,
            vec!["start_model".to_string()],
            "only real actions"
        );
        // The transcripts are on the queue's list, finished.
        let jobs = state.jobs().unwrap();
        assert_eq!(
            jobs.get("bbbb2222").unwrap().status,
            crate::jobs::JobStatus::Done
        );
        assert!(!jobs.has_pending());
        // Restoring the same archive again changes nothing.
        let again = inspect(&out, &b, &Current::of(&state)).unwrap().preview;
        for (kind, counts) in &again.categories {
            assert_eq!((counts.added, counts.overwritten), (0, 0), "{kind:?}");
        }
        assert!(again.overwritten.is_empty(), "{:?}", again.overwritten);
    }

    #[test]
    fn old_reports_and_transcripts_expire() {
        let cutoff = 1_000;
        assert!(research_expired(
            &json!({ "state": "done", "finished": 999 }),
            cutoff
        ));
        assert!(!research_expired(
            &json!({ "state": "done", "finished": 1_001 }),
            cutoff
        ));
        assert!(!research_expired(
            &json!({ "state": "running", "created": 1 }),
            cutoff
        ));
        assert!(research_expired(
            &json!({ "state": "failed", "created": 1 }),
            cutoff
        ));
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("box");
        sample_box(&root);
        let state = state_at(&root);
        // Everything in the sample finished at t=2.
        assert_eq!(expire(&state, 1), (2, 1));
        assert!(!root.join("research/1-r").exists());
        assert!(root.join("research/2-r").exists(), "still running");
        assert!(root.join("jobs/cccc3333").exists(), "still queued");
    }

    #[test]
    fn settings_default_to_off() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(settings_in(tmp.path()), DataSettings::default());
        let s = DataSettings::default();
        assert!(!s.keep_audio && !s.transcripts_to_library && !s.restart_last_model);
        assert_eq!(s.retention_days, None);
    }
}
