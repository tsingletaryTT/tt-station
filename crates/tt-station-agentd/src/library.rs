// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The Library: folders on the box whose files the assistant can search.
//!
//! Add a folder (Documents, a notes folder, a repository) and its files --
//! text, Markdown, code, PDFs (`pdftotext`), Word and HTML (`pandoc`) -- are
//! read into the document store ([`crate::docs`]) and kept current: a pass every
//! fifteen minutes picks up new, changed and deleted files. Hidden folders,
//! `node_modules`, build output and big files are skipped.
//!
//! When the CPU embedding service (`tt-embeddings`, [`crate::embeddings`]) is
//! running, every passage also gets a meaning vector, and search blends keyword
//! and meaning rankings. Without it, search is keyword only, and the vectors
//! catch up whenever the service is back.
//!
//! Chat, Tori and Routines search it through the assistant's `search_documents`
//! tool; Research looks there first, before the web.
//!
//! - `GET /library`, `POST /library/sources {path}`, `DELETE /library/sources/{id}`
//! - `POST /library/sources/{id}/reindex`, `POST /library/search {query}`

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth};

type Reply = Result<Json<Value>, ApiError>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Files bigger than this are skipped (logs, datasets, binaries with a text name).
const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// Files per folder, at most.
const MAX_FILES: usize = 20_000;
/// The embedding service, on the box.
const EMBED_URL: &str = "http://127.0.0.1:3040/embed";

const TEXT: &[&str] = &[
    "txt", "md", "markdown", "rst", "org", "tex", "csv", "tsv", "log", "adoc",
];
const CODE: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "kt", "c", "h", "cc", "cpp", "hpp", "cs",
    "rb", "php", "swift", "sh", "bash", "zsh", "toml", "yaml", "yml", "ini", "cfg", "sql", "lua",
    "r", "jl", "scala", "dart", "vue", "svelte", "css", "scss",
];
const CONVERT: &[&str] = &["pdf", "docx", "odt", "html", "htm", "epub", "rtf"];
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "__pycache__",
    "venv",
    ".venv",
    "vendor",
    "site-packages",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub path: String,
    pub added: u64,
    #[serde(default)]
    pub last_indexed: Option<u64>,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub skipped: usize,
    /// What it's doing now ("Reading 120 of 340 files"), while indexing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What `crate::library` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// The sources, loaded from disk on first use.
    sources: Mutex<Option<Vec<Source>>>,
    /// Folders being indexed right now.
    busy: Mutex<HashSet<String>>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().library
}

fn file(state: &AppState) -> PathBuf {
    state.config_dir().join("library.json")
}

fn with_sources<T>(state: &AppState, f: impl FnOnce(&mut Vec<Source>) -> T) -> T {
    let mut held = mine(state).sources.lock().expect("library");
    let list = held.get_or_insert_with(|| {
        std::fs::read(file(state))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    });
    let out = f(list);
    let _ = std::fs::write(
        file(state),
        serde_json::to_vec_pretty(&*list).unwrap_or_default(),
    );
    out
}

fn set(state: &AppState, id: &str, f: impl FnOnce(&mut Source)) {
    with_sources(state, |l| {
        if let Some(s) = l.iter_mut().find(|s| s.id == id) {
            f(s);
        }
    });
}

// ---------------------------------------------------------------- reading files

fn kind(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if TEXT.contains(&ext.as_str()) || CODE.contains(&ext.as_str()) {
        Some("text")
    } else if CONVERT.contains(&ext.as_str()) {
        Some("convert")
    } else {
        None
    }
}

/// The files of a folder worth reading: (path, mtime).
fn walk(root: &Path) -> (Vec<(PathBuf, u64)>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(e.path());
                }
            } else if t.is_file() && kind(&e.path()).is_some() {
                let Ok(m) = e.metadata() else { continue };
                if m.len() > MAX_FILE_BYTES || m.len() == 0 || out.len() >= MAX_FILES {
                    skipped += 1;
                    continue;
                }
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs());
                out.push((e.path(), mtime));
            }
        }
    }
    (out, skipped)
}

/// A file's text, converted where it needs to be.
fn read_text(path: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    let text = match kind(path)? {
        "text" => {
            let bytes = std::fs::read(path).ok()?;
            // Binary under a text name: skip.
            if bytes.iter().take(4096).any(|b| *b == 0) {
                return None;
            }
            String::from_utf8_lossy(&bytes).to_string()
        }
        _ => {
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            let out = if ext == "pdf" {
                Command::new("pdftotext")
                    .args(["-layout", "-q"])
                    .arg(path)
                    .arg("-")
                    .stdin(Stdio::null())
                    .output()
                    .ok()?
            } else {
                Command::new("pandoc")
                    .arg(path)
                    .args(["-t", "plain", "--wrap=none"])
                    .stdin(Stdio::null())
                    .output()
                    .ok()?
            };
            if !out.status.success() {
                return None;
            }
            String::from_utf8_lossy(&out.stdout).to_string()
        }
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then(|| text.chars().take(crate::docs::MAX_DOC_CHARS).collect())
}

// ---------------------------------------------------------------- indexing

/// Brings one folder up to date: new and changed files read, deleted ones
/// dropped. Blocking (files and converters); run it off the async threads.
fn index_blocking(state: &AppState, src: &Source) -> Result<(usize, usize, usize), String> {
    let docs = state.docs().ok_or("The document store isn't available.")?;
    let root = PathBuf::from(&src.path);
    if !root.is_dir() {
        return Err("That folder isn't there any more.".into());
    }
    let (files, skipped) = walk(&root);
    let known: HashMap<String, u64> = docs
        .from_source(&src.id)
        .into_iter()
        .map(|(_, p, m)| (p, m))
        .collect();
    let present: HashSet<String> = files.iter().map(|(p, _)| p.display().to_string()).collect();
    let mut removed = 0;
    for gone in known.keys().filter(|p| !present.contains(*p)) {
        docs.remove_path(&src.id, gone);
        removed += 1;
    }
    let todo: Vec<&(PathBuf, u64)> = files
        .iter()
        .filter(|(p, m)| known.get(&p.display().to_string()) != Some(m))
        .collect();
    let total = todo.len();
    let mut read = 0;
    for (i, (path, mtime)) in todo.into_iter().enumerate() {
        if i % 20 == 0 {
            set(state, &src.id, |s| {
                s.activity = Some(format!("Reading {} of {} files", i + 1, total))
            });
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        let name = format!(
            "{}/{}",
            root.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            rel
        );
        match read_text(path) {
            Some(text) => {
                if docs
                    .add_file(
                        &src.id,
                        &name,
                        &path.display().to_string(),
                        *mtime,
                        &text,
                        now(),
                    )
                    .is_ok()
                {
                    read += 1;
                }
            }
            None => docs.remove_path(&src.id, &path.display().to_string()),
        }
    }
    set(state, &src.id, |s| {
        s.files = files.len();
        s.skipped = skipped;
    });
    Ok((read, removed, files.len()))
}

/// Meaning vectors for passages that have none yet, while the embedding
/// service is up. Returns how many documents got them.
async fn embed_missing(state: &AppState) -> usize {
    let Some(docs) = state.docs() else { return 0 };
    let client = crate::localhttp::local();
    let mut done = 0;
    for id in docs.without_vectors() {
        let Some(passages) = docs.passages(&id) else {
            continue;
        };
        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(passages.len());
        for batch in passages.chunks(16) {
            let r = client
                .post(EMBED_URL)
                .json(&json!({ "inputs": batch, "truncate": true }))
                .timeout(Duration::from_secs(300))
                .send()
                .await;
            let Ok(r) = r else { return done };
            let Ok(v) = r.json::<Vec<Vec<f32>>>().await else {
                return done;
            };
            if v.len() != batch.len() {
                return done;
            }
            vectors.extend(v);
        }
        if docs.set_vectors(&id, vectors).is_ok() {
            done += 1;
        }
    }
    done
}

/// A query's meaning vector, if the embedding service is up.
pub async fn embed_query(text: &str) -> Option<Vec<f32>> {
    let r = crate::localhttp::local()
        .post(EMBED_URL)
        .json(&json!({ "inputs": [text], "truncate": true }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    r.json::<Vec<Vec<f32>>>().await.ok()?.into_iter().next()
}

/// Search, blending meaning in when the embedding service is up.
pub async fn search(
    state: &AppState,
    query: &str,
    top_k: usize,
    in_document: Option<&str>,
) -> Vec<crate::docs::Hit> {
    let Some(docs) = state.docs() else {
        return vec![];
    };
    let vector = embed_query(query).await;
    docs.search_hybrid(query, vector.as_deref(), top_k, in_document)
}

async fn index(state: &AppState, id: &str) {
    if !mine(state)
        .busy
        .lock()
        .expect("library")
        .insert(id.to_string())
    {
        return;
    }
    let Some(src) = with_sources(state, |l| l.iter().find(|s| s.id == id).cloned()) else {
        mine(state).busy.lock().expect("library").remove(id);
        return;
    };
    set(state, id, |s| {
        s.activity = Some("Looking for files".into());
        s.error = None;
    });
    let st = state.clone();
    let result = tokio::task::spawn_blocking(move || index_blocking(&st, &src))
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
    match result {
        Ok((read, removed, _)) => {
            if read + removed > 0 {
                eprintln!("tt-station-agentd: library: {id}: read {read}, dropped {removed}");
            }
            set(state, id, |s| {
                s.activity = Some("Finding what passages mean".into())
            });
            embed_missing(state).await;
            set(state, id, |s| {
                s.activity = None;
                s.last_indexed = Some(now());
            });
        }
        Err(e) => set(state, id, |s| {
            s.activity = None;
            s.error = Some(e);
        }),
    }
    mine(state).busy.lock().expect("library").remove(id);
}

/// Keeps every folder current. Started once, at agent start.
pub async fn index_forever(state: AppState) {
    loop {
        let ids: Vec<String> = with_sources(&state, |l| l.iter().map(|s| s.id.clone()).collect());
        for id in ids {
            index(&state, &id).await;
        }
        // Vectors for uploaded documents too, and for anything the service missed.
        embed_missing(&state).await;
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
    }
}

// ---------------------------------------------------------------- routes

/// `GET /library`
pub(crate) async fn get_library(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let sources = with_sources(&state, |l| l.clone());
    let (docs, coverage) = match state.docs() {
        Some(d) => {
            let all = d.list();
            let counts: HashMap<String, (usize, usize)> = all
                .iter()
                .filter_map(|x| x.source.clone().map(|s| (s, x.passages)))
                .fold(HashMap::new(), |mut m, (s, p)| {
                    let e = m.entry(s).or_insert((0, 0));
                    e.0 += 1;
                    e.1 += p;
                    m
                });
            let uploaded = all.iter().filter(|x| x.source.is_none()).count();
            (
                json!({ "per_source": counts, "uploaded": uploaded }),
                d.vector_coverage(),
            )
        }
        None => (json!({}), (0, 0)),
    };
    let meaning = embed_query("ping").await.is_some();
    Json(json!({
        "home": std::env::var("HOME").unwrap_or_default(),
        "sources": sources, "docs": docs,
        "meaning": { "available": meaning, "passages_done": coverage.0, "passages": coverage.1 },
    }))
}

#[derive(Debug, Deserialize)]
pub struct NewSource {
    pub path: String,
}

/// `POST /library/sources {path}`
pub(crate) async fn post_source(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(b): Json<NewSource>,
) -> Reply {
    let path =
        crate::coding::allowed_folder(&b.path).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let path_s = path.display().to_string();
    if let Some(dup) = with_sources(&state, |l| {
        l.iter()
            .find(|s| path_s.starts_with(&s.path) || s.path.starts_with(&path_s))
            .map(|s| s.path.clone())
    }) {
        return Err(err(
            StatusCode::CONFLICT,
            format!("That overlaps a folder already in the Library ({dup})."),
        ));
    }
    let src = Source {
        id: crate::pairing::issue_pair_id()[..10].to_string(),
        path: path_s,
        added: now(),
        last_indexed: None,
        files: 0,
        skipped: 0,
        activity: Some("Waiting".into()),
        error: None,
    };
    with_sources(&state, |l| l.push(src.clone()));
    let (st, id) = (state.clone(), src.id.clone());
    tokio::spawn(async move { index(&st, &id).await });
    Ok(Json(json!({ "source": src })))
}

/// `DELETE /library/sources/{id}`: the folder leaves the Library (its files stay put).
pub(crate) async fn delete_source(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    let gone = with_sources(&state, |l| {
        let n = l.len();
        l.retain(|s| s.id != id);
        n != l.len()
    });
    if !gone {
        return Err(err(StatusCode::NOT_FOUND, "No such folder."));
    }
    let removed = state.docs().map_or(0, |d| d.remove_source(&id));
    Ok(Json(json!({ "removed": removed })))
}

/// `POST /library/sources/{id}/reindex`
pub(crate) async fn post_reindex(
    State(state): State<AppState>,
    _auth: BearerAuth,
    UrlPath(id): UrlPath<String>,
) -> Reply {
    if !with_sources(&state, |l| l.iter().any(|s| s.id == id)) {
        return Err(err(StatusCode::NOT_FOUND, "No such folder."));
    }
    let st = state.clone();
    tokio::spawn(async move { index(&st, &id).await });
    Ok(Json(json!({ "indexing": true })))
}

#[derive(Debug, Deserialize)]
pub struct Search {
    pub query: String,
    #[serde(default)]
    pub top_k: Option<usize>,
}

/// `POST /library/search {query}`
pub(crate) async fn post_search(
    State(state): State<AppState>,
    _auth: BearerAuth,
    Json(b): Json<Search>,
) -> Json<Value> {
    let hits = search(&state, &b.query, b.top_k.unwrap_or(8).clamp(1, 30), None).await;
    Json(json!({ "hits": hits }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_files_worth_reading_and_skips_the_rest() {
        let root = std::env::temp_dir().join(format!("tt-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["notes", "node_modules/x", ".git", "src"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(
            root.join("notes/lease.md"),
            "Pets are allowed with a deposit.",
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("node_modules/x/index.js"), "skip me").unwrap();
        std::fs::write(root.join(".git/config"), "skip me").unwrap();
        std::fs::write(root.join("photo.jpg"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("blob.txt"), [b'a', 0, b'b']).unwrap();
        let (files, _) = walk(&root);
        let mut names: Vec<String> = files
            .iter()
            .map(|(p, _)| p.strip_prefix(&root).unwrap().display().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["blob.txt", "notes/lease.md", "src/main.rs"]);
        assert_eq!(
            read_text(&root.join("notes/lease.md")).as_deref(),
            Some("Pets are allowed with a deposit.")
        );
        assert_eq!(
            read_text(&root.join("blob.txt")),
            None,
            "binary under a text name"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
