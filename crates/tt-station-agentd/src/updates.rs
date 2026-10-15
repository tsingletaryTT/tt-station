// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Updating TT Station itself, with undo.
//!
//! Releases are the project's GitHub releases (a `.deb` per Ubuntu release).
//! An update never needs root: the package is unpacked into the agent's own
//! folder (`dpkg-deb -x`, `~/.local/share/tt-station/versions/<version>`) and
//! the agent's systemd user service is pointed at it with a drop-in
//! (`tt-station-agentd.service.d/50-version.conf`). The installed package
//! stays as it is underneath: removing the drop-in goes back to it.
//!
//! Every switch is watched: a small watchdog outside the agent restarts it on
//! the new version and checks that it comes up and answers. If it doesn't
//! within two minutes, the watchdog puts the previous version back on its
//! own. **Undo** goes back to the version before, any time; the last few stay
//! on disk for that.
//!
//! An agent running from a source checkout (`target/release` of a git repo)
//! is updated with git, not here: it's reported, never replaced.
//!
//! - `GET /updates`: this version, how it's installed, the newest release,
//!   versions on disk, history, and what's happening now.
//! - `POST /updates/install {version?, allow_downgrade?}`, `POST /updates/undo`.
//!
//! What's checked before a release is used: its tag is a plain version
//! ([`valid_version`]) -- it ends up in a path and the watchdog script -- and
//! its package is named for that version; the package has a sha256 from
//! GitHub, and the download matches it; the package is **signed** by a key the
//! box trusts ([`crate::release_keys`]: compiled-in keys plus the owner's
//! channel file), checked against the download's own hash; and it isn't older
//! than the running major.minor unless the request says `allow_downgrade`
//! ([`below_floor`]), so a paired device can't quietly move the box back to a
//! release with known holes. GitHub's digest only catches a broken download;
//! the signature is what shows the package came from whoever holds the release
//! key, not just from whoever can publish to the repository.
//!
//! Where releases come from and which extra keys count is the box's update
//! channel ([`channel`]): [`DEFAULT_REPO`] and the compiled-in keys, unless
//! the owner's `update-channel.toml` in the agent's config directory says
//! otherwise. The only way past a missing or bad signature is
//! [`ALLOW_UNSIGNED_ENV`] in the service's own environment (never a request
//! field), and every use of it is logged as an error.
//! Doc: docs/reference/releases.md.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::release_keys::{self, TrustedKey};
use crate::routes::{AppState, BearerAuth};

type Reply = Result<Json<Value>, ApiError>;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNIT: &str = "tt-station-agentd";
/// Versions kept on disk for undo.
const KEEP: usize = 3;

/// The GitHub repository releases come from when nothing says otherwise.
///
/// TODO(release-channel): this MUST move to an org-owned repository (under
/// Tenstorrent's GitHub organization, with its own branch protection and
/// release environment) before TT Station rolls out company-wide: a personal
/// repository is one account away from shipping code to every box. Which org
/// repository is still being decided; the switch is one line here plus a
/// release from the old repository that carries it (docs/reference/releases.md,
/// "Moving the release channel"). Boxes can follow another repository today
/// through their channel file.
pub const DEFAULT_REPO: &str = "tsingletaryTT/tt-station";

/// The owner's channel file, in the agent's config directory.
pub const CHANNEL_FILE: &str = "update-channel.toml";

/// Set (`1`) in the agent service's environment to install updates whose
/// signature is missing or doesn't check out. For developing the updater only;
/// every use is logged as an error and recorded in the activity log.
pub const ALLOW_UNSIGNED_ENV: &str = "TT_STATION_ALLOW_UNSIGNED_UPDATES";

/// Where this box gets releases, and whose signatures it accepts.
#[derive(Debug, Clone, Serialize)]
pub struct Channel {
    /// `owner/name` on GitHub.
    pub repo: String,
    /// Where `repo` came from: `default`, `channel file` or `environment`
    /// (`TT_STATION_UPDATE_REPO`).
    pub repo_from: String,
    /// Compiled-in keys first, then the channel file's.
    pub keys: Vec<TrustedKey>,
    /// [`ALLOW_UNSIGNED_ENV`] is set on the service.
    pub unsigned_allowed: bool,
}

/// `update-channel.toml`:
///
/// ```toml
/// repo = "example-org/tt-station"          # optional
/// [[keys]]                                 # optional, any number
/// name = "example-2026a"
/// key = "<base64 Ed25519 public key>"
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelFile {
    repo: Option<String>,
    #[serde(default)]
    keys: Vec<ChannelKey>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelKey {
    name: String,
    key: String,
}

/// `owner/name`, GitHub's characters only: it goes into the API URL.
pub fn valid_repo(r: &str) -> bool {
    let ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && !p.starts_with('.')
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    matches!(r.split('/').collect::<Vec<_>>()[..], [o, n] if ok(o) && ok(n))
}

fn unsigned_allowed() -> bool {
    matches!(
        std::env::var(ALLOW_UNSIGNED_ENV).as_deref(),
        Ok("1" | "true" | "yes")
    )
}

/// This box's channel. A channel file that can't be read is an error, not a
/// fallback: a box meant to follow a company channel mustn't quietly go back
/// to the default one.
pub fn channel(config_dir: &Path) -> Result<Channel, String> {
    channel_from(
        config_dir,
        std::env::var("TT_STATION_UPDATE_REPO").ok(),
        unsigned_allowed(),
    )
}

fn channel_from(
    config_dir: &Path,
    env_repo: Option<String>,
    unsigned_allowed: bool,
) -> Result<Channel, String> {
    let path = config_dir.join(CHANNEL_FILE);
    let mut c = Channel {
        repo: DEFAULT_REPO.into(),
        repo_from: "default".into(),
        keys: release_keys::builtin(),
        unsigned_allowed,
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let f: ChannelFile = toml::from_str(&text).map_err(|e| {
                format!(
                    "The update channel file {} isn't valid: {e}",
                    path.display()
                )
            })?;
            if let Some(r) = f.repo {
                if !valid_repo(&r) {
                    return Err(format!(
                        "The update channel file {} names \"{r}\", which isn't a GitHub owner/name.",
                        path.display()
                    ));
                }
                c.repo = r;
                c.repo_from = "channel file".into();
            }
            for k in f.keys {
                c.keys.push(
                    TrustedKey::new(&k.name, &k.key, "channel")
                        .map_err(|e| format!("The update channel file {}: {e}", path.display()))?,
                );
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Couldn't read {}: {e}", path.display())),
    }
    if let Some(r) = env_repo.filter(|r| !r.is_empty()) {
        if !valid_repo(&r) {
            return Err(format!(
                "TT_STATION_UPDATE_REPO is \"{r}\", which isn't a GitHub owner/name."
            ));
        }
        c.repo = r;
        c.repo_from = "environment".into();
    }
    Ok(c)
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
}

/// Where versions are unpacked.
fn root() -> PathBuf {
    home().join(".local/share/tt-station")
}

fn versions_dir() -> PathBuf {
    root().join("versions")
}

fn dropin() -> PathBuf {
    home().join(format!(
        ".config/systemd/user/{UNIT}.service.d/50-version.conf"
    ))
}

fn history_file() -> PathBuf {
    root().join("update-history.json")
}

// ---------------------------------------------------------------- how it's installed

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Install {
    /// Built from a git checkout: updated with git.
    Source {
        repo: String,
        branch: Option<String>,
        changed_files: usize,
    },
    /// The `.deb`'s binary.
    Package,
    /// A version unpacked here, by an earlier update.
    Managed { version: String },
    /// Anywhere else.
    Other { path: String },
}

pub fn install_kind(exe: &Path) -> Install {
    let s = exe.display().to_string();
    if let Ok(rest) = exe.strip_prefix(versions_dir()) {
        let version = rest
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default();
        return Install::Managed { version };
    }
    if s == "/usr/bin/tt-station-agentd" {
        return Install::Package;
    }
    // <repo>/target/{release,debug}/tt-station-agentd
    if let Some(repo) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
        if exe
            .parent()
            .and_then(Path::parent)
            .is_some_and(|t| t.ends_with("target"))
            && repo.join(".git").exists()
        {
            let git = |args: &[&str]| {
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            };
            return Install::Source {
                repo: repo.display().to_string(),
                branch: git(&["rev-parse", "--abbrev-ref", "HEAD"]),
                changed_files: git(&["status", "--porcelain"]).map_or(0, |o| o.lines().count()),
            };
        }
    }
    Install::Other { path: s }
}

/// Ubuntu's codename (`noble`), which picks the package.
fn suite() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("VERSION_CODENAME="))
        .map(|v| v.trim_matches('"').to_string())
}

/// A release tag the updater will use: a digit first, then letters, digits and
/// `.+-` only (`0.12.0`, `1.0.0-rc.1`), at most 64. It becomes a folder name and
/// is written into the watchdog script, so nothing else gets through.
pub fn valid_version(v: &str) -> bool {
    v.len() <= 64
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
}

/// `candidate` is older than `running`'s major.minor (`0.10.9` under `0.11.0`;
/// `0.11.0` under `0.11.3` is not: a patch back within a line is allowed).
pub fn below_floor(running: &str, candidate: &str) -> bool {
    let floor: Vec<&str> = running
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(2)
        .collect();
    older(candidate, &floor.join("."))
}

/// `a < b` for dotted versions (`0.9.0 < 0.11.0`).
pub fn older(a: &str, b: &str) -> bool {
    let parts = |v: &str| {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x < y;
        }
    }
    false
}

// ---------------------------------------------------------------- releases

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Release {
    pub version: String,
    pub name: String,
    pub notes: String,
    pub published: String,
    pub url: String,
    /// The package for this box's Ubuntu, if the release has one.
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// `sha256:<hex>`, when GitHub gives one.
    pub digest: Option<String>,
    /// Where its `<name>.sig` is, if the release has one.
    #[serde(default)]
    pub signature: Option<String>,
}

/// Releases as last fetched: from where, when, and the list.
type Fetched = (String, u64, Vec<Release>);

static RELEASES: LazyLock<Mutex<Option<Fetched>>> = LazyLock::new(|| Mutex::new(None));

/// `repo`'s releases, newest first (cached for an hour).
async fn releases(repo: &str, fresh: bool) -> Result<Vec<Release>, String> {
    if !fresh {
        if let Some((from, at, list)) = RELEASES.lock().expect("updates").clone() {
            if from == repo && now() - at < 3600 {
                return Ok(list);
            }
        }
    }
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=15");
    let v: Value = reqwest::Client::new()
        .get(url)
        .header("user-agent", format!("tt-station/{VERSION}"))
        .header("accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach GitHub: {e}"))?
        .error_for_status()
        .map_err(|e| {
            format!(
                "GitHub answered {}",
                e.status().map_or(String::new(), |s| s.to_string())
            )
        })?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let suite = suite();
    let mut list: Vec<Release> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["draft"] != true && r["prerelease"] != true)
        .filter_map(|r| {
            let version = r["tag_name"].as_str()?.trim_start_matches('v').to_string();
            if !valid_version(&version) {
                return None;
            }
            let assets: Vec<&Value> = r["assets"].as_array().into_iter().flatten().collect();
            let asset = assets.iter().find(|a| {
                let n = a["name"].as_str().unwrap_or("");
                n.starts_with("tt-station_")
                    && n.ends_with(".deb")
                    && suite
                        .as_deref()
                        .is_none_or(|s| n.contains(&format!("_{s}.deb")))
            });
            let signature = |name: &str| {
                let sig = format!("{name}.sig");
                assets
                    .iter()
                    .find(|a| a["name"] == sig.as_str())
                    .and_then(|a| a["browser_download_url"].as_str())
                    .map(str::to_string)
            };
            Some(Release {
                version,
                name: r["name"].as_str().unwrap_or("").to_string(),
                notes: r["body"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(4000)
                    .collect(),
                published: r["published_at"].as_str().unwrap_or("").to_string(),
                url: r["html_url"].as_str().unwrap_or("").to_string(),
                asset: asset.map(|a| Asset {
                    name: a["name"].as_str().unwrap_or("").to_string(),
                    url: a["browser_download_url"].as_str().unwrap_or("").to_string(),
                    size: a["size"].as_u64().unwrap_or(0),
                    digest: a["digest"].as_str().map(str::to_string),
                    signature: signature(a["name"].as_str().unwrap_or("")),
                }),
            })
        })
        .collect();
    list.sort_by(|a, b| {
        if older(&a.version, &b.version) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Less
        }
    });
    *RELEASES.lock().expect("updates") = Some((repo.to_string(), now(), list.clone()));
    Ok(list)
}

// ---------------------------------------------------------------- versions on disk

/// Versions unpacked here, newest first.
fn on_disk() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(versions_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("usr/bin/tt-station-agentd").is_file())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    v.sort_by(|a, b| {
        if older(a, b) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Less
        }
    });
    v
}

/// What the service runs: a version here, or (None) whatever its unit says.
fn selected() -> Option<String> {
    let text = std::fs::read_to_string(dropin()).ok()?;
    let line = text
        .lines()
        .find(|l| l.starts_with("ExecStart=") && l.len() > "ExecStart=".len())?;
    let path = PathBuf::from(&line["ExecStart=".len()..]);
    match install_kind(&path) {
        Install::Managed { version } => Some(version),
        _ => None,
    }
}

/// The drop-in that runs version `v`.
fn dropin_contents(v: &str) -> String {
    let dir = versions_dir().join(v);
    let app = dir.join("usr/share/tt-station/app");
    let mut text = format!(
        "# Written by TT Station's updater: runs version {v}.\n# Delete this file to go back to the installed package.\n[Service]\nExecStart=\nExecStart={}\n",
        dir.join("usr/bin/tt-station-agentd").display()
    );
    if app.join("index.html").is_file() {
        text.push_str(&format!(
            "Environment=TT_STATION_APP_DIR={}\n",
            app.display()
        ));
    }
    text
}

fn write_dropin(version: Option<&str>) -> std::io::Result<()> {
    let path = dropin();
    match version {
        None => match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        Some(v) => {
            std::fs::create_dir_all(path.parent().expect("drop-in folder"))?;
            std::fs::write(&path, dropin_contents(v))
        }
    }
}

// ---------------------------------------------------------------- history

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub at: u64,
    /// Versions; `null` is the installed package (or the unit's own binary).
    pub from: Option<String>,
    pub to: Option<String>,
    pub from_label: String,
    pub to_label: String,
    /// switching | done | rolled_back
    pub status: String,
    #[serde(default)]
    pub note: Option<String>,
}

fn history() -> Vec<Change> {
    crate::fsio::read_json_or_backup(&history_file()).unwrap_or_default()
}

fn save_history(h: &[Change]) {
    let keep = &h[h.len().saturating_sub(30)..];
    crate::fsio::save_json(&history_file(), keep, crate::fsio::PRIVATE);
}

// ---------------------------------------------------------------- doing it

static WORK: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

fn working(what: Option<&str>) {
    *WORK.lock().expect("updates") = what.map(str::to_string);
}

/// The package's sha256 (hex), or why there isn't a usable one: a release
/// without one isn't installed, since nothing would show the download is the
/// file the release lists.
fn checksum(asset: &Asset) -> Result<&str, String> {
    asset
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| format!("{} has no sha256 checksum on its release, so it can't be checked and wasn't installed.", asset.name))
}

/// The package is named for the release's version (`tt-station_0.12.0_…` for
/// tag `v0.12.0`; Debian's `~` stands for semver's `-`). The signature covers
/// the name, so this is what stops a signed old package being re-published
/// under a newer tag.
pub fn names_version(asset_name: &str, version: &str) -> bool {
    asset_name
        .strip_prefix("tt-station_")
        .and_then(|rest| rest.split('_').next())
        .is_some_and(|v| v.replace('~', "-") == version.replace('~', "-"))
}

fn missing_signature(asset: &Asset) -> String {
    format!(
        "{} has no signature ({}.sig) on its release, so it can't be checked and wasn't installed.",
        asset.name, asset.name
    )
}

/// Whether `asset`, whose download hashed to `sha256_hex`, may be installed,
/// given its `.sig` text (`None`: the release has none). Says who signed it;
/// with [`ALLOW_UNSIGNED_ENV`] set, a missing or bad signature is logged as an
/// error and let through.
fn check_signature(
    channel: &Channel,
    asset: &Asset,
    sha256_hex: &str,
    sig: Option<&str>,
) -> Result<String, String> {
    let checked = match sig {
        Some(sig) => release_keys::verify(&channel.keys, &asset.name, sha256_hex, sig)
            .map(|k| format!("signed by {} ({})", k.name, k.fingerprint)),
        None => Err(missing_signature(asset)),
    };
    match checked {
        Err(e) if channel.unsigned_allowed => {
            crate::log::error(format!(
                "update: INSTALLING AN UNVERIFIED PACKAGE because {ALLOW_UNSIGNED_ENV} is set on the service: {e}"
            ));
            Ok(format!("UNVERIFIED ({ALLOW_UNSIGNED_ENV}): {e}"))
        }
        other => other,
    }
}

async fn download_signature(url: &str) -> Result<String, String> {
    let bytes = reqwest::Client::new()
        .get(url)
        .header("user-agent", format!("tt-station/{VERSION}"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Couldn't download the signature: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Couldn't download the signature: {e}"))?;
    if bytes.len() > 4096 {
        return Err("The signature file is far too big to be a signature.".into());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Downloads and unpacks `release`'s package; checks it before it's used.
/// Says how it was verified (for the activity log).
async fn fetch(release: &Release, channel: &Channel) -> Result<String, String> {
    let asset = release.asset.as_ref().ok_or_else(|| {
        format!(
            "Release {} has no package for this box's Ubuntu ({}).",
            release.version,
            suite().unwrap_or_default()
        )
    })?;
    let want = checksum(asset)?.to_string();
    if !valid_version(&release.version)
        || asset.name.starts_with('.')
        || !asset
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '~'))
        || !names_version(&asset.name, &release.version)
    {
        return Err(format!(
            "Release {} has an unexpected name, so it wasn't used.",
            release.version
        ));
    }
    let dest = versions_dir().join(&release.version);
    if dest.join("usr/bin/tt-station-agentd").is_file() {
        return Ok("already on the box".into());
    }
    // The signature first: a release without one isn't worth downloading.
    let sig = match &asset.signature {
        Some(url) => Some(download_signature(url).await?),
        None if channel.unsigned_allowed => None,
        None => return Err(missing_signature(asset)),
    };
    std::fs::create_dir_all(root().join("downloads")).map_err(|e| e.to_string())?;
    let deb = root().join("downloads").join(&asset.name);
    let mut resp = reqwest::Client::new()
        .get(&asset.url)
        .header("user-agent", format!("tt-station/{VERSION}"))
        .timeout(Duration::from_secs(1800))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Couldn't download {}: {e}", asset.name))?;
    let mut file = std::fs::File::create(&deb).map_err(|e| e.to_string())?;
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
    let mut got: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("The download stopped: {e}"))?
    {
        use std::io::Write;
        file.write_all(&chunk).map_err(|e| e.to_string())?;
        hasher.update(&chunk);
        got += chunk.len() as u64;
        if let Some(pct) = (got * 100).checked_div(asset.size) {
            working(Some(&format!("Downloading {} ({}%)", release.version, pct)));
        }
    }
    drop(file);
    let hex: String = hasher
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !want.eq_ignore_ascii_case(&hex) {
        let _ = std::fs::remove_file(&deb);
        return Err("The download didn't match the release's checksum, so it wasn't used.".into());
    }
    let verified = match check_signature(channel, asset, &hex, sig.as_deref()) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&deb);
            return Err(e);
        }
    };
    working(Some(&format!("Unpacking {}", release.version)));
    let tmp = versions_dir().join(format!(".{}.partial", release.version));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let out = tokio::process::Command::new("dpkg-deb")
        .arg("-x")
        .arg(&deb)
        .arg(&tmp)
        .output()
        .await
        .map_err(|e| format!("Couldn't unpack it: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Couldn't unpack it: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bin = tmp.join("usr/bin/tt-station-agentd");
    if !bin.is_file() {
        return Err("The package has no agent in it.".into());
    }
    // It has to at least start: missing libraries show up here, not after the switch.
    let probe = tokio::process::Command::new(&bin)
        .arg("--help")
        .output()
        .await
        .map_err(|e| format!("The new version won't run here: {e}"))?;
    if String::from_utf8_lossy(&probe.stderr).contains("error while loading shared libraries") {
        return Err(format!(
            "The new version won't run here: {}",
            String::from_utf8_lossy(&probe.stderr).trim()
        ));
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&deb);
    Ok(verified)
}

fn label(v: &Option<String>, base: &str) -> String {
    v.clone().unwrap_or_else(|| base.to_string())
}

/// What the service runs without a drop-in, in words.
fn base_label(state: &AppState) -> String {
    let _ = state;
    match install_kind(&unit_exec().unwrap_or_default()) {
        Install::Package => "the installed package".into(),
        Install::Source { .. } => "the source build".into(),
        _ => "the original version".into(),
    }
}

/// The unit's own `ExecStart` (ignoring the drop-in).
fn unit_exec() -> Option<PathBuf> {
    for p in [
        home().join(format!(".config/systemd/user/{UNIT}.service")),
        PathBuf::from(format!("/usr/lib/systemd/user/{UNIT}.service")),
    ] {
        if let Ok(text) = std::fs::read_to_string(&p) {
            if let Some(l) = text
                .lines()
                .find(|l| l.starts_with("ExecStart=") && l.len() > 10)
            {
                return Some(PathBuf::from(
                    l["ExecStart=".len()..]
                        .split_whitespace()
                        .next()
                        .unwrap_or(""),
                ));
            }
        }
    }
    None
}

/// Points the service at `to` and restarts it under a watchdog that puts
/// `from` back if `to` doesn't come up.
fn switch(state: &AppState, from: Option<String>, to: Option<String>) -> Result<(), String> {
    if !state.manages_units() {
        return Err("This agent doesn't manage its own service (a test run).".into());
    }
    // Both end up in the watchdog script (paths in a heredoc and in quotes).
    if let Some(bad) = [&from, &to]
        .into_iter()
        .flatten()
        .find(|v| !valid_version(v))
    {
        return Err(format!(
            "\"{bad}\" isn't a version TT Station will switch to."
        ));
    }
    write_dropin(to.as_deref()).map_err(|e| format!("Couldn't point the service at it: {e}"))?;
    let base = base_label(state);
    let mut h = history();
    h.push(Change {
        at: now(),
        from: from.clone(),
        to: to.clone(),
        from_label: label(&from, &base),
        to_label: label(&to, &base),
        status: "switching".into(),
        note: None,
    });
    save_history(&h);
    let expect = match &to {
        Some(v) => versions_dir().join(v).join("usr/bin/tt-station-agentd"),
        None => unit_exec().unwrap_or_default(),
    };
    // The watchdog runs in its own unit, so restarting the agent doesn't stop it.
    let revert = match &from {
        Some(v) => format!(
            "cat > '{d}' <<'EOF'\n{text}EOF",
            d = dropin().display(),
            text = dropin_contents(v)
        ),
        None => format!("rm -f '{}'", dropin().display()),
    };
    let script = format!(
        r#"set -u
systemctl --user daemon-reload
systemctl --user restart {UNIT}
ok=0
for i in $(seq 1 60); do
  sleep 2
  pid=$(systemctl --user show -p MainPID --value {UNIT})
  exe=$(readlink -f /proc/$pid/exe 2>/dev/null || true)
  if [ "$exe" = "$(readlink -f '{expect}')" ] && curl -fs -m 3 -o /dev/null http://127.0.0.1:{port}/status; then
    ok=$((ok+1)); [ $ok -ge 3 ] && exit 0
  else
    ok=0
  fi
done
# It didn't come up: put the previous version back.
{revert}
systemctl --user daemon-reload
systemctl --user restart {UNIT}
touch '{marker}'
"#,
        expect = expect.display(),
        port = state.ctrl_port(),
        marker = root().join("rolled-back").display(),
    );
    let script_path = root().join("update-watch.sh");
    std::fs::write(&script_path, script).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(root().join("rolled-back"));
    let status = std::process::Command::new("systemd-run")
        .args([
            "--user",
            "--collect",
            "--quiet",
            "--unit=tt-station-update-watch",
            "/bin/bash",
        ])
        .arg(&script_path)
        .status()
        .map_err(|e| format!("Couldn't start the switch: {e}"))?;
    if !status.success() {
        return Err("Couldn't start the switch (is another update still finishing?).".into());
    }
    Ok(())
}

/// At startup: settle the last switch's history entry (it worked if this is
/// the version it switched to; the watchdog left a mark if it rolled back).
pub fn settle() {
    if unsigned_allowed() {
        crate::log::error(format!(
            "update: {ALLOW_UNSIGNED_ENV} is set on this service: updates with a missing or bad signature WILL be installed. Remove it unless you are developing the updater."
        ));
    }
    let mut h = history();
    let Some(last) = h.last_mut().filter(|c| c.status == "switching") else {
        return;
    };
    let marker = root().join("rolled-back");
    if marker.exists() {
        last.status = "rolled_back".into();
        last.note = Some(format!(
            "{} didn't start properly, so {} was put back.",
            last.to_label, last.from_label
        ));
        let _ = std::fs::remove_file(marker);
    } else if selected() == last.to {
        last.status = "done".into();
    } else {
        return;
    }
    save_history(&h);
    prune();
}

/// Keeps the last few versions (and whatever's selected).
fn prune() {
    let keep_sel = selected();
    for (i, v) in on_disk().into_iter().enumerate() {
        if i >= KEEP && Some(&v) != keep_sel.as_ref() {
            let _ = std::fs::remove_dir_all(versions_dir().join(v));
        }
    }
}

// ---------------------------------------------------------------- routes

/// `GET /updates`
pub(crate) async fn get_updates(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    let exe = std::env::current_exe().unwrap_or_default();
    let install = install_kind(&exe);
    let channel = channel(&state.config_dir());
    let (list, problem) = match &channel {
        Ok(c) => match releases(&c.repo, false).await {
            Ok(l) => (l, None),
            Err(e) => (vec![], Some(e)),
        },
        Err(e) => (vec![], Some(e.clone())),
    };
    let latest = list.first().cloned();
    let newer = latest
        .as_ref()
        .filter(|r| older(VERSION, &r.version))
        .cloned();
    let h = history();
    let can_undo = h
        .last()
        .filter(|c| c.status == "done")
        .is_some_and(|c| c.from != c.to);
    Json(json!({
        "version": VERSION,
        "install": install,
        "suite": suite(),
        "latest": latest,
        "update": newer,
        "releases": list.iter().take(6).collect::<Vec<_>>(),
        "on_disk": on_disk(),
        "selected": selected(),
        "base": base_label(&state),
        "history": h.iter().rev().take(10).collect::<Vec<_>>(),
        "undo": if can_undo { h.last().map(|c| json!({ "to": c.from, "label": c.from_label })) } else { None },
        "working": WORK.lock().expect("updates").clone(),
        "channel": channel.ok(),
        "problem": problem,
    }))
}

#[derive(Debug, Deserialize)]
pub struct InstallBody {
    #[serde(default)]
    pub version: Option<String>,
    /// Install a release older than the running major.minor. Any paired
    /// device may say so here; once the route is owner-only (roles), only an
    /// owner can.
    #[serde(default)]
    pub allow_downgrade: bool,
}

fn refuse_source() -> Result<(), ApiError> {
    if let Install::Source { repo, .. } = install_kind(&std::env::current_exe().unwrap_or_default())
    {
        return Err(err(StatusCode::CONFLICT, format!("This box runs TT Station built from source ({repo}). Update it with git and a rebuild; installing a release here would replace your build.")));
    }
    Ok(())
}

/// `POST /updates/install {version?, allow_downgrade?}`: the newest release, or `version`.
/// Owners only, so only an owner can allow a downgrade.
pub(crate) async fn post_install(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
    Json(b): Json<InstallBody>,
) -> Reply {
    refuse_source()?;
    if WORK.lock().expect("updates").is_some() {
        return Err(err(StatusCode::CONFLICT, "An update is already going."));
    }
    let channel = channel(&state.config_dir()).map_err(|e| err(StatusCode::CONFLICT, e))?;
    let list = releases(&channel.repo, true)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let release = match &b.version {
        Some(v) => list
            .iter()
            .find(|r| r.version == v.trim_start_matches('v'))
            .cloned(),
        None => list.first().cloned(),
    }
    .ok_or_else(|| err(StatusCode::NOT_FOUND, "No such release."))?;
    if below_floor(VERSION, &release.version) && !b.allow_downgrade {
        return Err(err(
            StatusCode::CONFLICT,
            format!(
                "{} is older than {VERSION}'s line; going back that far needs allow_downgrade.",
                release.version
            ),
        ));
    }
    if let Some(asset) = &release.asset {
        checksum(asset).map_err(|e| err(StatusCode::CONFLICT, e))?;
        if asset.signature.is_none() && !channel.unsigned_allowed {
            return Err(err(StatusCode::CONFLICT, missing_signature(asset)));
        }
    }
    if release.version == VERSION {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{VERSION} is already running."),
        ));
    }
    crate::audit::record(&state, "update_install", &release.version, "started");
    let st = state.clone();
    tokio::spawn(async move {
        working(Some(&format!("Getting {}", release.version)));
        let result = fetch(&release, &channel).await.and_then(|verified| {
            crate::audit::record(&st, "update_verified", &release.version, &verified);
            working(Some(&format!("Switching to {}", release.version)));
            switch(&st, selected(), Some(release.version.clone()))
        });
        if let Err(e) = result {
            eprintln!("tt-station-agentd: update: {e}");
            let mut h = history();
            h.push(Change {
                at: now(),
                from: selected(),
                to: Some(release.version.clone()),
                from_label: VERSION.into(),
                to_label: release.version.clone(),
                status: "failed".into(),
                note: Some(e),
            });
            save_history(&h);
        }
        working(None);
    });
    Ok(Json(json!({ "started": true })))
}

/// `POST /updates/undo`: back to the version before the last switch.
pub(crate) async fn post_undo(
    State(state): State<AppState>,
    _owner: crate::routes::OwnerAuth,
) -> Reply {
    refuse_source()?;
    let h = history();
    let last = h
        .last()
        .filter(|c| c.status == "done")
        .ok_or_else(|| err(StatusCode::CONFLICT, "There's no update to undo."))?;
    if let Some(v) = &last.from {
        if !versions_dir()
            .join(v)
            .join("usr/bin/tt-station-agentd")
            .is_file()
        {
            return Err(err(
                StatusCode::GONE,
                format!("{v} isn't on the box any more."),
            ));
        }
    }
    let switched = switch(&state, last.to.clone(), last.from.clone());
    crate::audit::record(
        &state,
        "update_undo",
        last.from.as_deref().unwrap_or("the packaged version"),
        if switched.is_ok() { "ok" } else { "failed" },
    );
    switched.map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "switching": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_versions_reach_the_watchdog() {
        for v in ["0.12.0", "1.0.0-rc.1", "0.11.0+noble", "2026.10.15"] {
            assert!(valid_version(v), "{v}");
        }
        for v in [
            "",
            "v0.12.0",
            "0.12.0'; rm -rf ~; '",
            "0.12/../../x",
            "0.12.0\nExecStart=/bin/sh",
            "0.12 0",
            "$(id)",
            "-1",
            &"9".repeat(65),
        ] {
            assert!(!valid_version(v), "{v:?}");
        }
    }

    #[test]
    fn a_release_needs_a_checksum() {
        let asset = |digest: Option<&str>| Asset {
            name: "tt-station_0.12.0_noble.deb".into(),
            url: String::new(),
            size: 1,
            digest: digest.map(str::to_string),
            signature: None,
        };
        let hex = "ab".repeat(32);
        assert_eq!(
            checksum(&asset(Some(&format!("sha256:{hex}")))).unwrap(),
            hex
        );
        assert!(checksum(&asset(None)).unwrap_err().contains("no sha256"));
        assert!(checksum(&asset(Some("sha512:abcd"))).is_err());
        assert!(
            checksum(&asset(Some("sha256:abcd"))).is_err(),
            "not a whole digest"
        );
    }

    fn test_channel(keys: Vec<TrustedKey>, unsigned_allowed: bool) -> Channel {
        Channel {
            repo: DEFAULT_REPO.into(),
            repo_from: "default".into(),
            keys,
            unsigned_allowed,
        }
    }

    #[tokio::test]
    async fn fetch_refuses_a_release_without_a_checksum_before_downloading() {
        let channel = test_channel(vec![], false);
        let r = Release {
            version: "0.99.0".into(),
            asset: Some(Asset {
                name: "tt-station_0.99.0_noble.deb".into(),
                url: "http://127.0.0.1:9/never".into(),
                size: 1,
                digest: None,
                signature: None,
            }),
            ..Default::default()
        };
        assert!(fetch(&r, &channel).await.unwrap_err().contains("no sha256"));
        // A checksum but no signature: refused before anything is downloaded
        // (the URL goes nowhere, so getting further would be a different error).
        let unsigned = Release {
            asset: Some(Asset {
                digest: Some(format!("sha256:{}", "ab".repeat(32))),
                ..r.asset.clone().unwrap()
            }),
            ..r.clone()
        };
        let e = fetch(&unsigned, &channel).await.unwrap_err();
        assert!(e.contains("has no signature"), "{e}");
        // A package named for another version.
        let renamed = Release {
            version: "0.99.1".into(),
            ..unsigned.clone()
        };
        assert!(fetch(&renamed, &channel)
            .await
            .unwrap_err()
            .contains("unexpected name"));
        let r = Release {
            version: "0.99.0'; id; '".into(),
            asset: Some(Asset {
                digest: Some(format!("sha256:{}", "ab".repeat(32))),
                ..r.asset.unwrap()
            }),
            ..Default::default()
        };
        assert!(fetch(&r, &channel)
            .await
            .unwrap_err()
            .contains("unexpected name"));
    }

    fn signed(kp: &ring::signature::Ed25519KeyPair, name: &str, sha: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(
            kp.sign(release_keys::statement(name, sha).as_bytes())
                .as_ref(),
        )
    }

    fn keypair() -> (ring::signature::Ed25519KeyPair, TrustedKey) {
        use base64::Engine as _;
        use ring::signature::KeyPair as _;
        let pkcs8 =
            ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap();
        let kp = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref());
        let key = TrustedKey::new("test-key", &b64, "channel").unwrap();
        (kp, key)
    }

    #[test]
    fn installs_need_a_trusted_signature_unless_the_service_says_otherwise() {
        let (kp, key) = keypair();
        let (other, _) = keypair();
        let asset = Asset {
            name: "tt-station_0.12.0_amd64_noble.deb".into(),
            ..Default::default()
        };
        let sha = "cd".repeat(32);
        let strict = test_channel(vec![key.clone()], false);
        let good = signed(&kp, &asset.name, &sha);
        let who = check_signature(&strict, &asset, &sha, Some(&good)).unwrap();
        assert!(who.contains("signed by test-key"), "{who}");
        let bad = signed(&other, &asset.name, &sha);
        assert!(check_signature(&strict, &asset, &sha, Some(&bad))
            .unwrap_err()
            .contains("isn't signed by a key this box trusts"));
        assert!(check_signature(&strict, &asset, &"ce".repeat(32), Some(&good)).is_err());
        assert!(check_signature(&strict, &asset, &sha, None)
            .unwrap_err()
            .contains("has no signature"));
        // The dev escape hatch lets it through, and says so.
        let dev = test_channel(vec![key], true);
        for sig in [None, Some(bad.as_str())] {
            assert!(check_signature(&dev, &asset, &sha, sig)
                .unwrap()
                .starts_with("UNVERIFIED"));
        }
    }

    #[test]
    fn package_names_carry_the_release_version() {
        assert!(names_version("tt-station_0.12.0_amd64_noble.deb", "0.12.0"));
        assert!(names_version(
            "tt-station_1.0.0~rc.1_amd64_noble.deb",
            "1.0.0-rc.1"
        ));
        assert!(!names_version(
            "tt-station_0.11.0_amd64_noble.deb",
            "0.12.0"
        ));
        assert!(!names_version(
            "tt-station_0.12.01_amd64_noble.deb",
            "0.12.0"
        ));
        assert!(!names_version(
            "tt-station-panel_0.12.0_amd64_noble.deb",
            "0.12.0"
        ));
    }

    #[test]
    fn the_channel_comes_from_the_default_the_owners_file_or_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let c = channel_from(dir.path(), None, false).unwrap();
        assert_eq!(
            (c.repo.as_str(), c.repo_from.as_str()),
            (DEFAULT_REPO, "default")
        );
        assert_eq!(c.keys, release_keys::builtin());

        let (_, key) = keypair();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(key.key);
        std::fs::write(
            dir.path().join(CHANNEL_FILE),
            format!("repo = \"example-org/tt-station\"\n[[keys]]\nname = \"example-2026a\"\nkey = \"{b64}\"\n"),
        )
        .unwrap();
        let c = channel_from(dir.path(), None, false).unwrap();
        assert_eq!(c.repo, "example-org/tt-station");
        assert_eq!(c.repo_from, "channel file");
        let added = c.keys.last().unwrap();
        assert_eq!(
            (added.name.as_str(), added.origin.as_str()),
            ("example-2026a", "channel")
        );
        assert_eq!(added.key, key.key);

        let c = channel_from(dir.path(), Some("other/tt-station".into()), true).unwrap();
        assert_eq!(
            (c.repo.as_str(), c.repo_from.as_str()),
            ("other/tt-station", "environment")
        );
        assert!(c.unsigned_allowed);
        assert!(channel_from(dir.path(), Some("../../x".into()), false).is_err());

        // A broken channel file stops updates rather than falling back.
        for bad in [
            "repo = \"https://evil.example/x\"\n",
            "repo = \"a/b\"\nkeys = [{ name = \"k\", key = \"AAAA\" }]\n",
            "repos = \"a/b\"\n",
            "not toml",
        ] {
            std::fs::write(dir.path().join(CHANNEL_FILE), bad).unwrap();
            assert!(channel_from(dir.path(), None, false).is_err(), "{bad}");
        }
    }

    #[test]
    fn repos_are_owner_and_name() {
        for r in [
            "tsingletaryTT/tt-station",
            "tenstorrent/tt-station",
            "a.b/c_d-e",
        ] {
            assert!(valid_repo(r), "{r}");
        }
        for r in [
            "", "a", "a/b/c", "/b", "a/", "../x", "a/b?x=1", "a /b", ".a/b",
        ] {
            assert!(!valid_repo(r), "{r}");
        }
    }

    #[test]
    fn no_downgrade_below_the_running_line() {
        assert!(below_floor("0.11.3", "0.10.9"));
        assert!(below_floor("0.11.0", "0.9.0"));
        assert!(below_floor("1.0.0", "0.99.0"));
        assert!(
            !below_floor("0.11.3", "0.11.0"),
            "a patch back within the line is fine"
        );
        assert!(!below_floor("0.11.3", "0.11.3"));
        assert!(!below_floor("0.11.3", "0.12.0"));
        assert!(!below_floor("0.11.0-rc.1", "0.11.0"));
    }

    #[test]
    fn compares_versions_and_knows_how_its_installed() {
        assert!(older("0.9.0", "0.11.0"));
        assert!(older("v0.11.0", "0.11.1"));
        assert!(!older("0.11.0", "0.11.0"));
        assert!(!older("1.0.0", "0.99.9"));
        assert_eq!(
            install_kind(Path::new("/usr/bin/tt-station-agentd")),
            Install::Package
        );
        let managed = versions_dir().join("0.12.0/usr/bin/tt-station-agentd");
        assert_eq!(
            install_kind(&managed),
            Install::Managed {
                version: "0.12.0".into()
            }
        );
        assert!(matches!(
            install_kind(Path::new("/opt/x/tt-station-agentd")),
            Install::Other { .. }
        ));
        // This test binary runs from the repo's target/ folder.
        let me = std::env::current_exe().unwrap();
        if me.components().any(|c| c.as_os_str() == "target")
            && Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.git")
                .exists()
        {
            // target/release/deps/<test> is one level deeper than the agent; the agent's own path is what counts.
            let agent = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/release/tt-station-agentd");
            assert!(matches!(install_kind(&agent), Install::Source { .. }));
        }
    }
}
