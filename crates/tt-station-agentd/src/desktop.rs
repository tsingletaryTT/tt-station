// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Open apps on the box's own desktop ("open VS Code", "open YouTube"), for
//! the assistant's `open_app` tool and `POST /desktop/open`.
//!
//! Apps are the desktop's installed `.desktop` entries (the same list the
//! GNOME app grid shows); a spoken name is matched against their names,
//! generic names and keywords. Launching goes through `gtk-launch` in the
//! logged-in session the agent's user service already shares (Wayland or X).
//! Web addresses open in the default browser. Nothing else runs: no shell, no
//! arbitrary commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::{BearerAuth, ErrorResponse};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DesktopApp {
    /// The desktop file's id, e.g. `com.microsoft.VSCode`.
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generic: Option<String>,
    #[serde(skip)]
    pub keywords: Vec<String>,
}

fn dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut out = vec![home.join(".local/share/applications")];
    let data =
        std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    out.extend(
        data.split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join("applications")),
    );
    out.extend(
        [
            "/usr/share/applications",
            "/var/lib/snapd/desktop/applications",
            "/var/lib/flatpak/exports/share/applications",
        ]
        .map(PathBuf::from),
    );
    out.push(home.join(".local/share/flatpak/exports/share/applications"));
    out.dedup();
    out
}

/// Parse one desktop entry; `None` for hidden entries and non-apps.
pub fn parse_entry(id: &str, text: &str) -> Option<DesktopApp> {
    let mut in_main = false;
    let (mut name, mut generic, mut keywords) = (None, None, Vec::new());
    let mut is_app = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "Type" => is_app = v.trim() == "Application",
            "Name" => name = Some(v.trim().to_string()),
            "GenericName" => generic = Some(v.trim().to_string()),
            "Keywords" => {
                keywords = v
                    .split(';')
                    .map(|k| k.trim().to_lowercase())
                    .filter(|k| !k.is_empty())
                    .collect()
            }
            "NoDisplay" | "Hidden" if v.trim().eq_ignore_ascii_case("true") => return None,
            _ => {}
        }
    }
    (is_app && name.is_some()).then(|| DesktopApp {
        id: id.to_string(),
        name: name.unwrap_or_default(),
        generic,
        keywords,
    })
}

/// Every app on the desktop, by name. The first entry for an id wins (the
/// user's own `~/.local/share/applications` overrides the system's).
pub fn apps() -> Vec<DesktopApp> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for dir in dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("desktop") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(app) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| parse_entry(&id, &t))
            {
                out.push(app);
            }
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn squash(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Spoken names that don't look like the app's real name.
const ALIASES: &[(&str, &str)] = &[
    ("vscode", "visual studio code"),
    ("code", "visual studio code"),
    ("chrome", "google chrome"),
    ("browser", "web browser"),
    ("files", "files"),
    ("terminal", "terminal"),
    ("settings", "settings"),
];

/// How well `app` matches what was asked for (0 = not at all).
pub fn score(app: &DesktopApp, asked: &str) -> u32 {
    let asked_lc = asked.trim().to_lowercase();
    let asked_lc = asked_lc.strip_prefix("the ").unwrap_or(&asked_lc);
    let asked_lc = asked_lc
        .strip_suffix(" app")
        .unwrap_or(asked_lc)
        .to_string();
    let q = squash(&asked_lc);
    if q.is_empty() {
        return 0;
    }
    let target = ALIASES
        .iter()
        .find(|(alias, _)| squash(alias) == q)
        .map(|(_, real)| squash(real))
        .unwrap_or_else(|| q.clone());
    let name = squash(&app.name);
    let id = squash(&app.id);
    let generic = app.generic.as_deref().map(squash).unwrap_or_default();
    if name == target || name == q {
        return 100;
    }
    if id.ends_with(&q) || id.ends_with(&target) {
        return 80;
    }
    if name.starts_with(&target) || name.starts_with(&q) {
        return 70;
    }
    if name.contains(&target) || (q.len() >= 4 && name.contains(&q)) {
        return 60;
    }
    if !generic.is_empty() && (generic == target || generic == q) {
        return 50;
    }
    if app.keywords.iter().any(|k| squash(k) == q) {
        return 40;
    }
    if q.len() >= 4 && id.contains(&q) {
        return 30;
    }
    0
}

/// The best match for a spoken name, plus close runners-up when it's a tie.
pub fn find<'a>(
    apps: &'a [DesktopApp],
    asked: &str,
) -> (Option<&'a DesktopApp>, Vec<&'a DesktopApp>) {
    let mut scored: Vec<(u32, &DesktopApp)> = apps
        .iter()
        .map(|a| (score(a, asked), a))
        .filter(|(s, _)| *s > 0)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.len().cmp(&b.1.name.len())));
    let Some(&(best, app)) = scored.first() else {
        return (None, Vec::new());
    };
    let ties: Vec<&DesktopApp> = scored
        .iter()
        .skip(1)
        .filter(|(s, _)| *s == best && best < 100)
        .map(|(_, a)| *a)
        .take(4)
        .collect();
    (Some(app), ties)
}

fn spawn(program: &str, args: &[&str]) -> Result<(), String> {
    // In its own scope: what this opens is the person's, not the agent's, and
    // must not be left behind in the agent's cgroup (see `crate::scope`).
    let mut child = crate::scope::command(program, args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    // gtk-launch and xdg-open hand off to the session and exit quickly.
    for _ in 0..50 {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            if status.success() {
                return Ok(());
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            return Err(err
                .trim()
                .lines()
                .last()
                .unwrap_or("it didn't start")
                .to_string());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(())
}

/// Is there a desktop session to open things in?
pub fn has_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

pub fn valid_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() < 2048
        && !url.chars().any(|c| c.is_control() || c.is_whitespace())
}

/// Open an app (optionally with a web address), or a web address in the
/// default browser. Returns what was opened, for the reply.
pub fn open(app: Option<&str>, url: Option<&str>) -> Result<Value, String> {
    if !has_session() {
        return Err(
            "nobody is logged in to the box's desktop, so there's nowhere to open it".into(),
        );
    }
    if let Some(url) = url {
        if !valid_url(url) {
            return Err("only web addresses (http or https) can be opened".into());
        }
    }
    let wanted = app.map(str::trim).filter(|a| !a.is_empty());
    match (wanted, url) {
        (None, Some(url)) => {
            spawn("xdg-open", &[url])?;
            Ok(json!({ "opened": "the web browser", "url": url }))
        }
        (Some(name), url) => {
            let all = apps();
            let (best, ties) = find(&all, name);
            let Some(best) = best else {
                let names: Vec<&str> = all.iter().map(|a| a.name.as_str()).take(60).collect();
                return Err(format!(
                    "no app called \"{name}\" is installed. Installed apps include: {}",
                    names.join(", ")
                ));
            };
            if !ties.is_empty() {
                let mut options = vec![best.name.clone()];
                options.extend(ties.iter().map(|a| a.name.clone()));
                return Ok(json!({ "ambiguous": true, "options": options }));
            }
            let mut args = vec![best.id.as_str()];
            if let Some(url) = url {
                args.push(url);
            }
            spawn("gtk-launch", &args)?;
            Ok(json!({ "opened": best.name, "id": best.id, "url": url }))
        }
        (None, None) => Err("say which app or web address to open".into()),
    }
}

// ---------------------------------------------------------------------
// Controlling the computer
// ---------------------------------------------------------------------

/// What `control` can do, for the tool's schema.
pub const CONTROLS: &[&str] = &[
    "volume_set",
    "volume_up",
    "volume_down",
    "mute",
    "unmute",
    "play_pause",
    "pause_media",
    "play_media",
    "next_track",
    "previous_track",
    "stop_media",
    "lock_screen",
    "dark_mode",
    "light_mode",
    "do_not_disturb_on",
    "do_not_disturb_off",
];

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("couldn't run {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .lines()
            .last()
            .unwrap_or("failed")
            .to_string())
    }
}

/// The speakers' volume, 0–150 %.
pub fn volume() -> Option<(u32, bool)> {
    let out = run("wpctl", &["get-volume", "@DEFAULT_AUDIO_SINK@"]).ok()?;
    let v: f64 = out.split_whitespace().nth(1)?.parse().ok()?;
    Some(((v * 100.0).round() as u32, out.contains("MUTED")))
}

/// Media players on the desktop (MPRIS), e.g. Chrome, Firefox, Spotify, mpv.
fn players() -> Vec<String> {
    run(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.ListNames",
        ],
    )
    .map(|out| {
        out.split(['\'', ',', '[', ']', '(', ')', ' '])
            .filter(|n| n.starts_with("org.mpris.MediaPlayer2."))
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

fn media(method: &str) -> Result<String, String> {
    let players = players();
    if players.is_empty() {
        return Err("nothing is playing media on the box".into());
    }
    for p in &players {
        let _ = run(
            "gdbus",
            &[
                "call",
                "--session",
                "--dest",
                p,
                "--object-path",
                "/org/mpris/MediaPlayer2",
                "--method",
                &format!("org.mpris.MediaPlayer2.Player.{method}"),
            ],
        );
    }
    Ok(format!("{} player(s)", players.len()))
}

/// Change something on the box's desktop. `value` is the volume percent for
/// `volume_set`, or the step for `volume_up`/`volume_down` (default 10).
pub fn control(action: &str, value: Option<f64>) -> Result<Value, String> {
    if !has_session() {
        return Err("nobody is logged in to the box's desktop".into());
    }
    let sink = "@DEFAULT_AUDIO_SINK@";
    let step = value.unwrap_or(10.0).clamp(1.0, 100.0);
    let done = match action {
        "volume_set" => {
            let v = value
                .ok_or("say what volume, e.g. 40 (percent)")?
                .clamp(0.0, 150.0);
            run("wpctl", &["set-mute", sink, "0"])?;
            run("wpctl", &["set-volume", sink, &format!("{:.2}", v / 100.0)])?;
            "volume set"
        }
        "volume_up" => {
            run("wpctl", &["set-mute", sink, "0"])?;
            run(
                "wpctl",
                &["set-volume", "-l", "1.5", sink, &format!("{step}%+")],
            )?;
            "volume up"
        }
        "volume_down" => {
            run("wpctl", &["set-volume", sink, &format!("{step}%-")])?;
            "volume down"
        }
        "mute" => {
            run("wpctl", &["set-mute", sink, "1"])?;
            "muted"
        }
        "unmute" => {
            run("wpctl", &["set-mute", sink, "0"])?;
            "unmuted"
        }
        "play_pause" => {
            media("PlayPause")?;
            "play/pause"
        }
        "pause_media" => {
            media("Pause")?;
            "paused"
        }
        "play_media" => {
            media("Play")?;
            "playing"
        }
        "next_track" => {
            media("Next")?;
            "next track"
        }
        "previous_track" => {
            media("Previous")?;
            "previous track"
        }
        "stop_media" => {
            media("Stop")?;
            "stopped"
        }
        "lock_screen" => {
            run("loginctl", &["lock-session"])?;
            "screen locked"
        }
        "dark_mode" => {
            run(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.interface",
                    "color-scheme",
                    "prefer-dark",
                ],
            )?;
            "dark mode on"
        }
        "light_mode" => {
            run(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.interface",
                    "color-scheme",
                    "default",
                ],
            )?;
            "light mode on"
        }
        "do_not_disturb_on" => {
            run(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.notifications",
                    "show-banners",
                    "false",
                ],
            )?;
            "do not disturb on"
        }
        "do_not_disturb_off" => {
            run(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.notifications",
                    "show-banners",
                    "true",
                ],
            )?;
            "do not disturb off"
        }
        other => {
            return Err(format!(
                "\"{other}\" isn't something I can do; try one of {}",
                CONTROLS.join(", ")
            ))
        }
    };
    let mut result = json!({ "done": done });
    if action.starts_with("volume") || action.ends_with("mute") {
        if let Some((v, muted)) = volume() {
            result["volume_percent"] = json!(v);
            result["muted"] = json!(muted);
        }
    }
    Ok(result)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/".into())
}

/// A path in the user's home, from what was said ("Downloads",
/// "~/code/tt-station", "/home/ttuser/x.pdf"). Nothing outside home.
pub fn home_path(said: &str) -> Result<PathBuf, String> {
    let home = home();
    let said = said.trim();
    let named = match said.to_lowercase().trim_end_matches(" folder").trim() {
        "home" | "~" | "" => Some(home.clone()),
        "downloads" => Some(home.join("Downloads")),
        "documents" => Some(home.join("Documents")),
        "desktop" => Some(home.join("Desktop")),
        "pictures" | "photos" => Some(home.join("Pictures")),
        "music" => Some(home.join("Music")),
        "videos" => Some(home.join("Videos")),
        _ => None,
    };
    let path = named.unwrap_or_else(|| {
        if let Some(rest) = said.strip_prefix("~/") {
            home.join(rest)
        } else if said.starts_with('/') {
            PathBuf::from(said)
        } else {
            home.join(said)
        }
    });
    let real = path
        .canonicalize()
        .map_err(|_| format!("there's no {} on the box", path.display()))?;
    if !real.starts_with(&home) {
        return Err("I can only open things in your home folder".into());
    }
    Ok(real)
}

/// Open a folder in Files, or a file in its default app.
pub fn open_path(said: &str) -> Result<Value, String> {
    if !has_session() {
        return Err("nobody is logged in to the box's desktop".into());
    }
    let path = home_path(said)?;
    spawn("gio", &["open", &path.to_string_lossy()])?;
    Ok(json!({ "opened": path.display().to_string() }))
}

/// Files and folders in home whose names contain `query` (hidden ones and
/// build/dependency folders skipped), newest first.
pub fn find_files(query: &str) -> Result<Value, String> {
    let q = query.trim().to_lowercase();
    if q.len() < 2 {
        return Err("say a bit more of the name".into());
    }
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut stack = vec![(home(), 0usize)];
    let mut visited = 0usize;
    const SKIP: &[&str] = &[
        "node_modules",
        "target",
        "venv",
        "__pycache__",
        "snap",
        "build",
        "dist",
    ];
    while let Some((dir, depth)) = stack.pop() {
        visited += 1;
        if visited > 20_000 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            let Ok(meta) = e.metadata() else { continue };
            if name.to_lowercase().contains(&q) {
                found.push((
                    meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                    path.clone(),
                ));
            }
            if meta.is_dir() && depth < 6 {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    let paths: Vec<String> = found
        .into_iter()
        .take(12)
        .map(|(_, p)| p.display().to_string())
        .collect();
    Ok(json!({ "matches": paths }))
}

// ---------------------------------------------------------------------
// TT Station's own pages
// ---------------------------------------------------------------------

const STATION_PAGES: &[(&str, &str)] = &[
    ("home", "/"), ("chat", "/chat"), ("studio", "/studio"), ("transcribe", "/studio#transcribe"),
    ("models", "/models"), ("bringup", "/bringup"), ("hardware", "/hardware"), ("connect", "/connect"),
    ("settings", "/settings"), ("research", "/research"), ("news", "/discover"), ("code", "/code"),
];

/// Show a TT Station page: the running app listens on a socket in the
/// session's runtime dir; if it isn't running, start it.
pub fn open_station(page: &str) -> Result<Value, String> {
    use std::io::Write;
    let (name, route) = STATION_PAGES
        .iter()
        .find(|(p, _)| *p == page)
        .ok_or_else(|| format!("TT Station has no \"{page}\" page"))?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let sock = runtime.join("tt-station-tori.sock");
    let sent = std::os::unix::net::UnixStream::connect(&sock)
        .and_then(|mut s| s.write_all(format!("nav {route}").as_bytes()))
        .is_ok();
    if !sent {
        if !has_session() {
            return Err("nobody is logged in to the box's desktop".into());
        }
        spawn("gtk-launch", &["tt-station"])?;
    }
    Ok(json!({ "page": name, "route": route }))
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

/// `GET /desktop/apps` (authed): the apps the box can open.
pub(crate) async fn get_apps(_auth: BearerAuth) -> Json<Value> {
    let apps = tokio::task::spawn_blocking(apps).await.unwrap_or_default();
    Json(json!({ "session": has_session(), "apps": apps }))
}

#[derive(Debug, Deserialize)]
pub struct OpenBody {
    #[serde(default)]
    pub app: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// `POST /desktop/open` (authed) `{app?, url?}`.
pub(crate) async fn post_open(
    _auth: BearerAuth,
    Json(body): Json<OpenBody>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    tokio::task::spawn_blocking(move || open(body.app.as_deref(), body.url.as_deref()))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map(Json)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str, name: &str) -> DesktopApp {
        DesktopApp {
            id: id.into(),
            name: name.into(),
            generic: None,
            keywords: vec![],
        }
    }

    #[test]
    fn entries_are_parsed_and_hidden_ones_skipped() {
        let vs = "[Desktop Entry]\nName=Visual Studio Code\nGenericName=Text Editor\nType=Application\nKeywords=vscode;\n[Desktop Action new-empty-window]\nName=New Empty Window\n";
        let a = parse_entry("code", vs).expect("an app");
        assert_eq!(a.name, "Visual Studio Code");
        assert_eq!(a.keywords, vec!["vscode"]);
        assert!(parse_entry(
            "x",
            "[Desktop Entry]\nName=X\nType=Application\nNoDisplay=true\n"
        )
        .is_none());
        assert!(parse_entry("l", "[Desktop Entry]\nName=L\nType=Link\n").is_none());
    }

    #[test]
    fn spoken_names_find_the_app() {
        let apps = vec![
            app("com.microsoft.VSCode", "Visual Studio Code"),
            app(
                "com.microsoft.VSCode.UrlHandler",
                "Visual Studio Code - URL Handler",
            ),
            app("google-chrome", "Google Chrome"),
            app("org.gnome.Terminal", "Terminal"),
        ];
        for asked in ["VS Code", "vscode", "visual studio code", "code"] {
            assert_eq!(
                find(&apps, asked).0.map(|a| a.id.as_str()),
                Some("com.microsoft.VSCode"),
                "{asked}"
            );
        }
        assert_eq!(
            find(&apps, "chrome").0.map(|a| a.id.as_str()),
            Some("google-chrome")
        );
        assert_eq!(
            find(&apps, "the terminal app").0.map(|a| a.id.as_str()),
            Some("org.gnome.Terminal")
        );
        assert_eq!(
            find(&apps, "terminal").0.map(|a| a.id.as_str()),
            Some("org.gnome.Terminal")
        );
        assert!(find(&apps, "photoshop").0.is_none());
    }

    #[test]
    fn paths_stay_in_home() {
        assert!(home_path("/etc/passwd").is_err());
        assert!(home_path("~/../../etc").is_err());
    }

    #[test]
    fn unknown_controls_are_refused() {
        if has_session() {
            assert!(control("format_disk", None).is_err());
        }
    }

    #[test]
    fn only_web_addresses_open() {
        assert!(valid_url("https://youtube.com"));
        assert!(!valid_url("file:///etc/passwd"));
        assert!(!valid_url("https://a.com; rm -rf"));
    }
}
