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

/// What the app's pages may load and talk to: its own files and the box's
/// API (same origin, WebSockets included), images, sound and video it made
/// itself (`blob:`, `data:`), and nothing from elsewhere -- an image or a
/// script from a web address in a model's reply doesn't load. Styles may be
/// inline: React's `style` props, highlighted code and the terminal (xterm)
/// set them. No `eval`, no frames, no plugins, and no one may frame the app.
pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
font-src 'self' data:; media-src 'self' data: blob:; connect-src 'self'; worker-src 'self' blob:; frame-src 'none'; \
object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'none'";

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
                    (header::CONTENT_SECURITY_POLICY, CSP),
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

    /// The app's pages load nothing from elsewhere and run no inline or
    /// eval'd script; what the built app needs stays allowed (checked against
    /// the built bundle in a browser: fonts, styles, blob:/data: images, the
    /// same-origin API and WebSockets).
    #[test]
    fn the_app_runs_only_its_own_script() {
        let dir = |d: &str| CSP.split(';').map(str::trim).find(|x| x.starts_with(&format!("{d} "))).unwrap_or_else(|| panic!("{d}")).to_string();
        assert_eq!(dir("script-src"), "script-src 'self'");
        assert_eq!(dir("connect-src"), "connect-src 'self'");
        assert_eq!(dir("object-src"), "object-src 'none'");
        assert_eq!(dir("frame-ancestors"), "frame-ancestors 'none'");
        assert!(dir("img-src").contains("blob:") && dir("img-src").contains("data:") && !dir("img-src").contains("https:"));
        assert!(!CSP.contains("unsafe-eval") && !CSP.contains('*'));
    }
}
