//! The list of boxes this app knows about, and the run options last used for each model.
//!
//! **No secrets.** The file holds a name, host, port and MAC per box, and the last
//! [`RunOptions`] per (station, model). Pairing tokens live in the shared token store (see
//! [`crate::pairing_flow`]), never here, so the file can be copied or shown without leaking
//! access.
//!
//! **Forgiving reads, careful writes.** [`Registry::load`] treats a missing or unreadable file
//! as empty and never panics. [`Registry::save`] writes a temporary file in the same directory
//! and renames it over the real one, so a crash or a full disk leaves the previous file whole.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::Context;
use libttstation::run_options::RunOptions;
use serde::{Deserialize, Serialize};

use crate::station::StationId;

/// A box the person added by hand or paired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownStation {
    /// Display name. For a box added by address this is the address until `/status` says more.
    pub name: String,
    /// Bare host name or IP literal, with no brackets (an IPv6 literal is stored as `::1`).
    pub host: String,
    pub port: u16,
    /// Network card MAC, kept so a powered-off box can be woken. Unknown until seen.
    pub mac: Option<String>,
}

impl KnownStation {
    /// The host as it goes inside a URL or a `host:port` string: an IPv6 literal gets brackets.
    pub fn url_host(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// `"<host>:<port>"` (IPv6 hosts bracketed). Also the key of the box's pairing token.
    pub fn id(&self) -> StationId {
        StationId(format!("{}:{}", self.url_host(), self.port))
    }

    /// The agent's control-plane URL, `http://<host>:<port>`.
    pub fn base(&self) -> String {
        format!("http://{}", self.id().0)
    }
}

/// Why [`Registry::add_manual`] said no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddError {
    /// The address or port is not usable. The text says what is wrong.
    BadAddress(String),
    /// A station with the same host and port is already listed.
    Duplicate,
}

impl std::fmt::Display for AddError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddError::BadAddress(why) => write!(f, "{why}"),
            AddError::Duplicate => write!(f, "that station is already in the list"),
        }
    }
}

impl std::error::Error for AddError {}

/// One remembered run, in the on-disk list form. JSON object keys must be strings, so the
/// in-memory `(station, model)` map is stored as a list of these.
#[derive(Serialize, Deserialize)]
struct RunMemory {
    station: String,
    model: String,
    options: RunOptions,
}

/// The file's shape. Unknown extra keys are ignored on read so a newer file still loads.
#[derive(Serialize, Deserialize, Default)]
struct OnDisk {
    #[serde(default)]
    stations: Vec<KnownStation>,
    #[serde(default)]
    last_run: Vec<RunMemory>,
    /// The model most recently run per station id, for the tray's "Run last model". Absent in
    /// files written before it existed, which load with no last model.
    #[serde(default)]
    last_model: BTreeMap<String, String>,
}

/// The known stations plus remembered run options, backed by one JSON file.
pub struct Registry {
    path: PathBuf,
    stations: Vec<KnownStation>,
    /// Keyed by (station id text, model name).
    last_run: BTreeMap<(String, String), RunOptions>,
    /// Station id text -> the model most recently passed to [`Registry::remember_run`]. Kept
    /// apart from `last_run` because that map is ordered by model name and has no record of time.
    last_model: BTreeMap<String, String>,
}

impl Registry {
    /// Read the registry at `path`. A missing, unreadable or corrupt file gives an empty
    /// registry bound to that path; the next [`Registry::save`] replaces whatever was there.
    pub fn load(path: &Path) -> Registry {
        let parsed: OnDisk = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Registry {
            path: path.to_path_buf(),
            stations: parsed.stations,
            last_run: parsed
                .last_run
                .into_iter()
                .map(|r| ((r.station, r.model), r.options))
                .collect(),
            last_model: parsed.last_model,
        }
    }

    /// Write the registry to its file, atomically.
    ///
    /// The data goes to a temporary file in the same directory (so the rename cannot cross a
    /// filesystem), is flushed to disk, then renamed over the target. On any failure the
    /// temporary file is removed and the previous file is untouched. The parent directory is
    /// created if needed.
    ///
    /// The write happens under the registry's advisory lock (see [`Registry::update`]).
    pub fn save(&self) -> anyhow::Result<()> {
        let _lock = lock_registry(&self.path)?;
        self.save_locked()
    }

    /// Load the file, apply `f`, and save, all under one exclusive advisory lock on a sibling
    /// `.<name>.lock` file (`std::fs::File::lock`, an `flock` on Linux).
    ///
    /// Use this for a small change made long after the last load, such as the tray recording a
    /// MAC it learned while polling. Reading the file again under the lock keeps whatever another
    /// writer saved in the meantime (a station the window added, a run it remembered, a removal).
    ///
    /// **Limitation.** The lock only orders writers that take it, and it cannot protect a writer
    /// that saves an old in-memory copy: the window loads the registry once and later saves its
    /// whole copy with [`Registry::save`], which can still overwrite a MAC the tray wrote after
    /// that load. The lock prevents two writes from interleaving and makes this function's
    /// read-modify-write atomic; it does not merge stale copies. The lock is advisory, so a
    /// process that ignores it (an editor) is not stopped.
    ///
    /// `f` must only change the registry. A closure that calls [`Registry::save`] deadlocks
    /// itself, because `save` takes the lock again through a new open file and an `flock` held
    /// by one open file description blocks another, even in the same process.
    pub fn update(path: &Path, f: impl FnOnce(&mut Registry)) -> anyhow::Result<()> {
        let _lock = lock_registry(path)?;
        let mut reg = Registry::load(path);
        f(&mut reg);
        reg.save_locked()
    }

    /// The body of [`Registry::save`]; the caller holds the lock.
    fn save_locked(&self) -> anyhow::Result<()> {
        let dir = parent_dir(&self.path);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let on_disk = OnDisk {
            stations: self.stations.clone(),
            last_run: self
                .last_run
                .iter()
                .map(|((station, model), options)| RunMemory {
                    station: station.clone(),
                    model: model.clone(),
                    options: options.clone(),
                })
                .collect(),
            last_model: self.last_model.clone(),
        };
        let json = serde_json::to_vec_pretty(&on_disk).context("serializing the registry")?;

        let file_name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "stations.json".into());
        let tmp = dir.join(format!(".{file_name}.tmp.{}", std::process::id()));

        let write_and_rename = || -> anyhow::Result<()> {
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&json).context("writing the registry")?;
            f.sync_all().context("flushing the registry to disk")?;
            drop(f);
            std::fs::rename(&tmp, &self.path)
                .with_context(|| format!("replacing {}", self.path.display()))
        };
        let result = write_and_rename();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// The known stations, in the order they were added.
    pub fn stations(&self) -> &[KnownStation] {
        &self.stations
    }

    /// Add a station by address. `host` must be a bare host name, an IPv4 literal or an IPv6
    /// literal (no scheme, brackets, path, user, query or spaces) and `port` must be 1..=65535.
    /// The display name starts as the host. Returns the new station's id (`host:port`).
    pub fn add_manual(&mut self, host: &str, port: u16) -> Result<StationId, AddError> {
        validate_address(host, port)?;
        if self
            .stations
            .iter()
            .any(|s| s.port == port && s.host.eq_ignore_ascii_case(host))
        {
            return Err(AddError::Duplicate);
        }
        let station = KnownStation {
            name: host.to_string(),
            host: host.to_string(),
            port,
            mac: None,
        };
        let id = station.id();
        self.stations.push(station);
        Ok(id)
    }

    /// Forget a station and the run options remembered for it. Unknown ids are ignored.
    pub fn remove(&mut self, id: &StationId) {
        self.stations.retain(|s| s.id() != *id);
        self.last_run.retain(|(station, _), _| *station != id.0);
        self.last_model.remove(&id.0);
    }

    /// Record `mac` as the network card MAC of station `id`, so it can be woken while off.
    /// Unknown ids are ignored. Call [`Registry::save`] afterwards to keep it.
    pub fn set_mac(&mut self, id: &StationId, mac: &str) {
        if let Some(s) = self.stations.iter_mut().find(|s| s.id() == *id) {
            s.mac = Some(mac.to_string());
        }
    }

    /// Remember `opts` as the options last used to run `model` on `station`, and `model` as the
    /// most recent model run there ([`Registry::last_model`]).
    pub fn remember_run(&mut self, station: &StationId, model: &str, opts: &RunOptions) {
        self.last_run
            .insert((station.0.clone(), model.to_string()), opts.clone());
        self.last_model.insert(station.0.clone(), model.to_string());
    }

    /// The model most recently remembered for `station`, if any. The tray's "Run last model"
    /// offers this one, with its options from [`Registry::last_run`].
    pub fn last_model(&self, station: &StationId) -> Option<&str> {
        self.last_model.get(&station.0).map(String::as_str)
    }

    /// The options last used for `model` on `station`, if any.
    pub fn last_run(&self, station: &StationId, model: &str) -> Option<&RunOptions> {
        self.last_run.get(&(station.0.clone(), model.to_string()))
    }
}

/// The registry file's directory, or `.` for a bare file name.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Take the exclusive advisory lock for the registry at `path`: an `flock` on
/// `<dir>/.<name>.lock`, created if needed and held until the returned file is dropped. Blocks
/// while another holder has it; every holder only reads and writes one small file.
fn lock_registry(path: &Path) -> anyhow::Result<std::fs::File> {
    let dir = parent_dir(path);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "stations.json".into());
    let lock_path = dir.join(format!(".{name}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    Ok(file)
}

/// The address checks [`Registry::add_manual`] applies, without adding anything: a strict
/// bare host (see `validate_host`) and a port of 1 to 65535. The Add station dialog calls this
/// before pairing, so an address the registry would refuse is never paired with.
pub fn validate_address(host: &str, port: u16) -> Result<(), AddError> {
    validate_host(host)?;
    if port == 0 {
        return Err(AddError::BadAddress("port must be 1 to 65535".into()));
    }
    Ok(())
}

/// Strict check of a bare host: an IP literal, or a DNS-style name of letters, digits, `-`, `_`
/// and `.` whose labels are non-empty and do not start or end with `-`.
fn validate_host(host: &str) -> Result<(), AddError> {
    let bad = |why: &str| Err(AddError::BadAddress(format!("{why}: {host:?}")));
    if host.is_empty() {
        return bad("the address is empty");
    }
    // An IP literal needs no further checks. Brackets are not accepted: they are part of a URL.
    if host.parse::<Ipv4Addr>().is_ok() || host.parse::<Ipv6Addr>().is_ok() {
        return Ok(());
    }
    if host.contains("://") {
        return bad("leave out the scheme (no http://)");
    }
    if host.len() > 253 {
        return bad("the name is too long");
    }
    if host.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return bad("not a valid IPv4 address");
    }
    for label in host.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return bad("use a plain host name or IP address (no spaces, ports, paths or symbols)");
        }
    }
    Ok(())
}
