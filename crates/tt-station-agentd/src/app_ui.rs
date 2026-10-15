// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The desktop app's interface (`desktop/`), served at `/app`.
//!
//! The TT Station desktop app is an Electron window that loads its screens from
//! here, so the screens and the API share one origin (no CORS, no second
//! server). Built files come from `$TT_STATION_APP_DIR`, else the repo's
//! `desktop/dist` next to this binary, else `/usr/share/tt-station/app`.
//! Unknown paths fall back to `index.html` (the app routes itself).

use std::path::{Component, Path, PathBuf};

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

/// Where the built app lives, if anywhere.
pub fn app_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![];
    if let Ok(dir) = std::env::var("TT_STATION_APP_DIR") {
        candidates.push(dir.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        // target/{release,debug}/tt-station-agentd → <repo>/desktop/dist
        if let Some(repo) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            candidates.push(repo.join("desktop/dist"));
        }
    }
    candidates.push("/usr/share/tt-station/app".into());
    candidates
        .into_iter()
        .find(|d| d.join("index.html").is_file())
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// A request path as a file under `root`, refusing anything that leaves it.
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = Path::new(rel.trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(root.join(rel))
}

async fn serve(rel: &str) -> Response {
    let Some(root) = app_dir() else {
        return (
            StatusCode::NOT_FOUND,
            "The TT Station app isn't built. Run `npm run build` in desktop/.",
        )
            .into_response();
    };
    let file = match resolve(&root, rel) {
        Some(p) if p.is_file() => p,
        Some(_) => root.join("index.html"),
        None => return StatusCode::BAD_REQUEST.into_response(),
    };
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            // Hashed build assets never change; the page itself always revalidates.
            let cache = if file.starts_with(root.join("assets")) {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, content_type(&file)),
                    (header::CACHE_CONTROL, cache),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn get_app_root() -> Response {
    serve("index.html").await
}

pub(crate) async fn get_app_path(
    axum::extract::Path(rel): axum::extract::Path<String>,
) -> Response {
    serve(&rel).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_the_app() {
        let root = Path::new("/srv/app");
        assert_eq!(
            resolve(root, "assets/x.js"),
            Some(PathBuf::from("/srv/app/assets/x.js"))
        );
        assert_eq!(
            resolve(root, "/index.html"),
            Some(PathBuf::from("/srv/app/index.html"))
        );
        assert_eq!(resolve(root, "../etc/passwd"), None);
        assert_eq!(resolve(root, "assets/../../x"), None);
        assert_eq!(content_type(Path::new("a.woff2")), "font/woff2");
    }
}
