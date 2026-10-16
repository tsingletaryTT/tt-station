// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Paired devices: one record per pairing, so the owner can see which
//! computers and phones can control the box and sign any one of them out.
//!
//! Each `/pair/complete` mints a 256-bit bearer token (`crate::pairing`) and
//! records a [`Session`] for it: a random device id, the name and platform
//! the device gave (or an honest default), when it paired and when it was
//! last used. The token itself is handed to the device once and never kept:
//! the box stores only its SHA-256. A 256-bit random token has nothing to
//! brute-force, so a plain hash (rather than a slow password hash) is enough
//! to make a copied token file useless, and lookups compare digests in
//! constant time.
//!
//! A session expires after [`IDLE_EXPIRY`] without use; every authenticated
//! request slides the window forward. The in-memory "last used" moves on
//! every request, but it reaches the disk at most every
//! [`TOUCH_WRITE_INTERVAL_S`], so a busy client doesn't turn every request
//! into a file write.
//!
//! The store is a JSON file (`--token-store`, mode 0600):
//!
//! ```json
//! {"version": 2, "sessions": [{"id": "…", "token_sha256": "…", "name": "Jane's MacBook",
//!   "platform": "macOS", "created": 1760000000, "last_used": 1760500000}]}
//! ```
//!
//! Each device is an *owner* or a *member*. Owners manage the box: they see
//! and remove other devices, make other devices owners, and hold SSH keys,
//! app keys, power, updates, reset and the security settings. Members use
//! it. The first device ever paired is an owner, and so is anything paired
//! with the box user's local secret (`/pair/offer`: the desktop app on the
//! box itself, or `tt-station-agentd pair`); everything else pairs as a
//! member until an owner promotes it. The box remembers that its first owner
//! was taken (`owner_claimed`), so removing every owner doesn't hand the
//! role to whoever pairs next; the desktop app on the box can always pair
//! again as an owner.
//!
//! Stores from before roles (version 2 and the raw-token array) carry no
//! roles. On first load the box's own desktop app -- the session whose token
//! is in the `desktop-token` file beside the store, when that file holds it
//! in the clear -- becomes an owner, and every other device a member. A
//! desktop app whose token is sealed in the keychain notices it's a member
//! and pairs again through its local secret, which makes it an owner.
//!
//! Before device tracking the same file was a JSON array of raw tokens.
//! [`Sessions::open`] converts that in place on first load: every old token
//! keeps working, now as a hashed session named [`MIGRATED_NAME`], and the
//! new file replaces the old one in a single atomic rename, so the box never
//! holds neither.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long a paired device may go unused before the box forgets it.
///
/// 90 days: long enough that a laptop back from a long trip, or a phone that
/// only checks in now and then, still works, and short enough that a lost or
/// retired device stops being a live credential within a quarter without
/// anyone having to remember it. Every use resets the clock, so a device in
/// regular use never expires.
pub const IDLE_EXPIRY: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// How stale the on-disk "last used" may get before a request writes it out.
/// An hour is nothing against a 90-day window, and keeps a chatty client to
/// one small write an hour rather than one per request.
pub const TOUCH_WRITE_INTERVAL_S: u64 = 60 * 60;

/// The name given to tokens carried over from before device tracking: which
/// device holds each one, and when it paired, were never recorded.
pub const MIGRATED_NAME: &str = "Paired before device tracking";

/// 3: roles (`owner`, `owner_claimed`). 2: device sessions.
const STORE_VERSION: u32 = 3;
const MAX_NAME: usize = 60;
const MAX_PLATFORM: usize = 30;

/// One paired device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    /// Random, not secret: how routes and the UI name this device.
    pub id: String,
    /// Lower-case hex SHA-256 of the bearer token. The token itself is never stored.
    pub token_sha256: String,
    pub name: String,
    /// `macOS`, `Linux`, `Windows`, `iOS`, … as the device said; `None` when it didn't.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Unix seconds. `None` for sessions migrated from the old token file,
    /// whose pairing date is unknown.
    #[serde(default)]
    pub created: Option<u64>,
    /// Unix seconds.
    pub last_used: u64,
    /// Manages the box (see the module docs); `false` is a member.
    #[serde(default)]
    pub owner: bool,
}

impl Session {
    /// Unix seconds after which this session stops working unless used again.
    pub fn expires(&self) -> u64 {
        self.last_used.saturating_add(IDLE_EXPIRY.as_secs())
    }

    fn expired_at(&self, now: u64) -> bool {
        now > self.expires()
    }
}

/// Why a bearer token was turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// Never issued, removed, or signed out by a reset.
    Unknown,
    /// Was valid, but went unused for longer than [`IDLE_EXPIRY`].
    Expired,
}

impl AuthFailure {
    /// What a person sees: plain, and always the same way out.
    pub fn message(self) -> String {
        match self {
            AuthFailure::Unknown => {
                "This device isn't paired with the box, or it was removed. Pair it again to reconnect.".to_string()
            }
            AuthFailure::Expired => format!(
                "This device's pairing expired after {} days without use. Pair it again to reconnect.",
                IDLE_EXPIRY.as_secs() / 86_400
            ),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    sessions: Vec<Session>,
    /// Some device has been an owner: the next pairing isn't automatically one.
    #[serde(default)]
    owner_claimed: bool,
}

#[derive(Default)]
struct Table {
    sessions: Vec<Session>,
    owner_claimed: bool,
    /// `last_used` as last written to disk, by device id: what the write
    /// throttle compares against.
    on_disk: std::collections::HashMap<String, u64>,
}

pub struct Sessions {
    path: Option<PathBuf>,
    table: Mutex<Table>,
    /// Serializes disk writes, separately from `table`, so authenticating a
    /// request never waits on the disk. Each write takes a fresh snapshot
    /// after acquiring this lock, so whichever write lands last reflects every
    /// change made before it.
    write_lock: Mutex<()>,
    /// Bumped on every revocation, for open WebSockets to re-check their
    /// token (see `AppState::signed_out`).
    revocations: tokio::sync::watch::Sender<u64>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Lower-case hex SHA-256 of a token: what the box keeps instead of it.
pub fn token_digest(token: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Equal-length byte strings compared without an early exit, so how long a
/// comparison takes says nothing about how much of a digest matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A device-supplied label, made safe to store and show: no control
/// characters, trimmed, and bounded. `None` if nothing is left.
pub fn clean_label(raw: Option<&str>, max: usize) -> Option<String> {
    let cleaned: String = raw?.chars().filter(|c| !c.is_control()).collect();
    let cleaned: String = cleaned.trim().chars().take(max).collect();
    let cleaned = cleaned.trim_end().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// `YYYY-MM-DD` for unix seconds (UTC). Howard Hinnant's `civil_from_days`,
/// as in `tt-station`'s `ssh.rs`.
fn ymd(unix_s: u64) -> String {
    let z = (unix_s / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe as i64 + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The name a device gets when it didn't give one.
pub fn default_name(now: u64) -> String {
    format!("Device paired {}", ymd(now))
}

/// What a store file held.
enum Loaded {
    Missing,
    /// Sessions with roles.
    Current(Vec<Session>, bool),
    /// Version 2: sessions from before roles.
    Unroled(Vec<Session>),
    /// The pre-device-tracking format: a JSON array of raw tokens.
    Legacy(Vec<String>),
    Unreadable,
}

fn read_store(path: &Path) -> Loaded {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => {
            eprintln!(
                "tt-station-agentd: failed to read token store at {}: {e} -- starting with no paired devices",
                path.display()
            );
            return Loaded::Unreadable;
        }
    };
    if let Ok(file) = serde_json::from_str::<StoreFile>(&contents) {
        if file.version < STORE_VERSION {
            return Loaded::Unroled(file.sessions);
        }
        return Loaded::Current(file.sessions, file.owner_claimed);
    }
    if let Ok(tokens) = serde_json::from_str::<Vec<String>>(&contents) {
        return Loaded::Legacy(tokens);
    }
    // Moved aside rather than left to be overwritten by the next pairing, so
    // the devices it held can be recovered by hand.
    crate::fsio::quarantine(path, "not valid JSON; starting with no paired devices");
    Loaded::Unreadable
}

/// The digest of the box's own desktop app's token, when the `desktop-token`
/// file beside the store holds it in the clear (a keychain-sealed one,
/// `ttsafe1:…`, can't be read here). Only ever compared, never kept.
fn desktop_token_digest(store: &Path) -> Option<String> {
    let text = std::fs::read_to_string(store.with_file_name("desktop-token")).ok()?;
    let token = text.trim();
    (!token.is_empty() && !token.starts_with("ttsafe")).then(|| token_digest(token))
}

/// Roles for sessions from before roles: the box's own desktop app is an
/// owner, every other device a member (see the module docs).
fn assign_first_roles(sessions: &mut [Session], desktop: Option<&str>) {
    for s in sessions.iter_mut() {
        s.owner =
            desktop.is_some_and(|d| constant_time_eq(d.as_bytes(), s.token_sha256.as_bytes()));
    }
}

impl Sessions {
    fn with(path: Option<PathBuf>, sessions: Vec<Session>) -> Self {
        Self::with_roles(path, sessions, false)
    }

    fn with_roles(path: Option<PathBuf>, sessions: Vec<Session>, owner_claimed: bool) -> Self {
        let on_disk = sessions
            .iter()
            .map(|s| (s.id.clone(), s.last_used))
            .collect();
        let owner_claimed = owner_claimed || sessions.iter().any(|s| s.owner);
        Sessions {
            path,
            table: Mutex::new(Table {
                sessions,
                owner_claimed,
                on_disk,
            }),
            write_lock: Mutex::new(()),
            revocations: tokio::sync::watch::Sender::new(0),
        }
    }

    /// Sessions kept in memory only (`AppState::new`, most tests).
    pub fn in_memory() -> Self {
        Self::with(None, Vec::new())
    }

    /// Sessions persisted at `path`, loading what's there.
    ///
    /// A missing file means no paired devices yet. An unreadable or corrupt
    /// one logs a warning and also starts empty: a bad file must never stop
    /// the agent from starting. A file in the old raw-token format is
    /// migrated (see the module docs).
    pub fn open(path: PathBuf) -> Self {
        Self::open_at(path, unix_now())
    }

    pub(crate) fn open_at(path: PathBuf, now: u64) -> Self {
        match read_store(&path) {
            Loaded::Missing | Loaded::Unreadable => Self::with(Some(path), Vec::new()),
            Loaded::Current(sessions, owner_claimed) => {
                let live: Vec<Session> = sessions
                    .into_iter()
                    .filter(|s| !s.expired_at(now))
                    .collect();
                Self::with_roles(Some(path), live, owner_claimed)
            }
            Loaded::Unroled(sessions) => {
                // A box that was paired before has had its first owner, whoever
                // it was: the next pairing from the network is a member.
                let claimed = !sessions.is_empty();
                let mut live: Vec<Session> = sessions
                    .into_iter()
                    .filter(|s| !s.expired_at(now))
                    .collect();
                assign_first_roles(&mut live, desktop_token_digest(&path).as_deref());
                let owners = live.iter().filter(|s| s.owner).count();
                let upgraded = Self::with_roles(Some(path.clone()), live, claimed);
                match upgraded.save() {
                    Ok(()) => eprintln!(
                        "tt-station-agentd: paired devices now have roles: {owners} owner(s), {} member(s). The desktop app on this box can always pair again as an owner.",
                        upgraded.len() - owners
                    ),
                    Err(e) => eprintln!(
                        "tt-station-agentd: couldn't save device roles to {} ({e:#}); they'll be worked out again on the next start",
                        path.display()
                    ),
                }
                upgraded
            }
            Loaded::Legacy(tokens) => {
                let mut seen = std::collections::HashSet::new();
                let mut sessions: Vec<Session> = tokens
                    .iter()
                    .filter(|t| !t.is_empty() && seen.insert(t.as_str()))
                    .map(|t| Session {
                        id: crate::pairing::issue_device_id(),
                        token_sha256: token_digest(t),
                        name: MIGRATED_NAME.to_string(),
                        platform: None,
                        created: None,
                        // The idle clock starts at the upgrade: the old file
                        // never said when a token was last used.
                        last_used: now,
                        owner: false,
                    })
                    .collect();
                assign_first_roles(&mut sessions, desktop_token_digest(&path).as_deref());
                let claimed = !sessions.is_empty();
                let migrated = Self::with_roles(Some(path.clone()), sessions, claimed);
                // The old file stays in place until the new one is complete,
                // and the swap is one rename: a failure here leaves the old
                // tokens on disk (and working, in memory), to migrate again
                // on the next start.
                match migrated.save() {
                    Ok(()) => eprintln!(
                        "tt-station-agentd: converted {} paired token(s) at {} to hashed device records",
                        migrated.len(),
                        path.display()
                    ),
                    Err(e) => eprintln!(
                        "tt-station-agentd: couldn't convert the token store at {} ({e:#}); the old file is unchanged and will be converted on the next start",
                        path.display()
                    ),
                }
                migrated
            }
        }
    }

    pub fn len(&self) -> usize {
        self.table
            .lock()
            .expect("sessions mutex poisoned")
            .sessions
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Which session `token` belongs to, comparing its digest against every
    /// stored one in constant time. Index into `table.sessions`.
    fn find(table: &Table, token: &str) -> Option<usize> {
        let presented = token_digest(token);
        let mut found = None;
        // No early exit: every stored digest is compared.
        for (i, s) in table.sessions.iter().enumerate() {
            if constant_time_eq(presented.as_bytes(), s.token_sha256.as_bytes()) && found.is_none()
            {
                found = Some(i);
            }
        }
        found
    }

    /// Check a bearer token and, when it's good, count this as a use: the
    /// idle window slides forward, and the disk hears about it at most every
    /// [`TOUCH_WRITE_INTERVAL_S`]. Returns the device id.
    pub fn authenticate(&self, token: &str) -> Result<String, AuthFailure> {
        self.authenticate_at(token, unix_now())
    }

    pub(crate) fn authenticate_at(&self, token: &str, now: u64) -> Result<String, AuthFailure> {
        let (id, flush) = {
            let mut table = self.table.lock().expect("sessions mutex poisoned");
            let i = Self::find(&table, token).ok_or(AuthFailure::Unknown)?;
            if table.sessions[i].expired_at(now) {
                return Err(AuthFailure::Expired);
            }
            let session = &mut table.sessions[i];
            session.last_used = session.last_used.max(now);
            let id = session.id.clone();
            let last_used = session.last_used;
            let written = table.on_disk.get(&id).copied().unwrap_or(0);
            (
                id,
                last_used.saturating_sub(written) >= TOUCH_WRITE_INTERVAL_S,
            )
        };
        if flush {
            self.persist("record when a device was last used");
        }
        Ok(id)
    }

    /// The live session `token` belongs to, without counting a use.
    pub fn session_for(&self, token: &str) -> Option<Session> {
        let now = unix_now();
        let table = self.table.lock().expect("sessions mutex poisoned");
        Self::find(&table, token)
            .map(|i| table.sessions[i].clone())
            .filter(|s| !s.expired_at(now))
    }

    /// One live session by device id.
    pub fn get(&self, id: &str) -> Option<Session> {
        self.list().into_iter().find(|s| s.id == id)
    }

    /// Whether `token` still names a live session, without counting a use.
    pub fn is_live(&self, token: &str) -> bool {
        let now = unix_now();
        let table = self.table.lock().expect("sessions mutex poisoned");
        Self::find(&table, token).is_some_and(|i| !table.sessions[i].expired_at(now))
    }

    /// The device id `token` belongs to, live or not, without counting a use.
    pub fn id_for(&self, token: &str) -> Option<String> {
        let table = self.table.lock().expect("sessions mutex poisoned");
        Self::find(&table, token).map(|i| table.sessions[i].id.clone())
    }

    /// Record a new pairing for `token` and persist it. It's an owner when
    /// `owner` is asked for (paired with the local secret) or when it's the
    /// first device the box ever paired; a member otherwise.
    pub fn pair(
        &self,
        token: &str,
        name: Option<&str>,
        platform: Option<&str>,
        owner: bool,
    ) -> Session {
        self.pair_as(token, name, platform, owner, unix_now())
    }

    /// [`Self::pair`] with no owner asked for (tests).
    #[cfg(test)]
    pub(crate) fn pair_at(
        &self,
        token: &str,
        name: Option<&str>,
        platform: Option<&str>,
        now: u64,
    ) -> Session {
        self.pair_as(token, name, platform, false, now)
    }

    pub(crate) fn pair_as(
        &self,
        token: &str,
        name: Option<&str>,
        platform: Option<&str>,
        owner: bool,
        now: u64,
    ) -> Session {
        let session = {
            let mut table = self.table.lock().expect("sessions mutex poisoned");
            let session = Session {
                id: crate::pairing::issue_device_id(),
                token_sha256: token_digest(token),
                name: clean_label(name, MAX_NAME).unwrap_or_else(|| default_name(now)),
                platform: clean_label(platform, MAX_PLATFORM),
                created: Some(now),
                last_used: now,
                owner: owner || !table.owner_claimed,
            };
            table.owner_claimed |= session.owner;
            table.sessions.push(session.clone());
            session
        };
        self.persist("save a new pairing");
        session
    }

    /// Make a device an owner, or a member again. The last owner can't be
    /// made a member: someone must be able to manage the box.
    pub fn set_owner(&self, id: &str, owner: bool) -> Result<Session, String> {
        let now = unix_now();
        let changed = {
            let mut table = self.table.lock().expect("sessions mutex poisoned");
            let owners = table
                .sessions
                .iter()
                .filter(|s| s.owner && !s.expired_at(now))
                .count();
            let s = table
                .sessions
                .iter_mut()
                .find(|s| s.id == id && !s.expired_at(now))
                .ok_or_else(|| "No such device. It may have been removed.".to_string())?;
            if s.owner && !owner && owners <= 1 {
                return Err(
                    "This is the box's only owner. Make another device an owner first.".to_string(),
                );
            }
            s.owner = owner;
            let s = s.clone();
            table.owner_claimed |= owner;
            s
        };
        self.persist("save a device's role");
        Ok(changed)
    }

    /// Add a session as-is (tests: one with a chosen `last_used`).
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn insert_for_test(&self, session: Session) {
        self.table
            .lock()
            .expect("sessions mutex poisoned")
            .sessions
            .push(session);
    }

    /// Every live session, most recently used first.
    pub fn list(&self) -> Vec<Session> {
        self.list_at(unix_now())
    }

    pub(crate) fn list_at(&self, now: u64) -> Vec<Session> {
        let mut out: Vec<Session> = self
            .table
            .lock()
            .expect("sessions mutex poisoned")
            .sessions
            .iter()
            .filter(|s| !s.expired_at(now))
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.last_used
                .cmp(&a.last_used)
                .then_with(|| a.name.cmp(&b.name))
        });
        out
    }

    /// Sign one device out: its token stops working at once.
    pub fn revoke(&self, id: &str) -> Option<Session> {
        let removed = {
            let mut table = self.table.lock().expect("sessions mutex poisoned");
            let i = table.sessions.iter().position(|s| s.id == id)?;
            table.on_disk.remove(id);
            table.sessions.remove(i)
        };
        self.revocations.send_modify(|n| *n += 1);
        self.persist("save a removed device");
        Some(removed)
    }

    /// Sign every device out (`POST /reset`) and delete the store, so the
    /// box is back to never having been paired.
    pub fn revoke_all(&self) -> usize {
        let count = {
            let mut table = self.table.lock().expect("sessions mutex poisoned");
            let n = table.sessions.len();
            table.sessions.clear();
            table.on_disk.clear();
            // Back to never paired: the next device is the first owner again.
            table.owner_claimed = false;
            n
        };
        self.revocations.send_modify(|n| *n += 1);
        if let Some(path) = &self.path {
            let _guard = self
                .write_lock
                .lock()
                .expect("token-store write lock poisoned");
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!(
                    "tt-station-agentd: failed to remove token store at {} during reset: {e}",
                    path.display()
                ),
            }
        }
        count
    }

    /// Changes whenever a device is signed out.
    pub fn revocations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.revocations.subscribe()
    }

    /// [`Self::save`], logging instead of failing: the in-memory table is
    /// what gates requests, so a failed write shouldn't fail the request
    /// that caused it.
    fn persist(&self, what: &str) {
        if let Err(e) = self.save() {
            if let Some(path) = &self.path {
                eprintln!(
                    "tt-station-agentd: couldn't {what} to the token store at {}: {e:#}",
                    path.display()
                );
            }
        }
    }

    /// Write the current table to the store (no-op without one). Expired
    /// sessions stay until the next start drops them (`open`), so until then
    /// their devices still hear "expired" rather than "not paired".
    fn save(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let _guard = self
            .write_lock
            .lock()
            .expect("token-store write lock poisoned");
        let (snapshot, owner_claimed) = {
            let table = self.table.lock().expect("sessions mutex poisoned");
            (table.sessions.clone(), table.owner_claimed)
        };
        let json = serde_json::to_string_pretty(&StoreFile {
            version: STORE_VERSION,
            sessions: snapshot.clone(),
            owner_claimed,
        })?;
        write_private(path, &json)?;
        let mut table = self.table.lock().expect("sessions mutex poisoned");
        for s in &snapshot {
            table.on_disk.insert(s.id.clone(), s.last_used);
        }
        Ok(())
    }
}

/// Write `contents` to `path` readable by its owner only: the parent is
/// created `0700` if missing, the data goes to a fresh `0600` temp file
/// beside it, and a rename swaps it in, so a reader sees the old file or the
/// new one, never half of either.
pub(crate) fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        // Only a directory this creates gets chmod-ed: an existing shared one
        // (`/tmp` in tests) is left as the box has it.
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    let tmp = path.with_extension("tmp");
    let result = write_tmp_and_rename(&tmp, path, contents);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Create `tmp` fresh at mode `0600` (never a looser file, not even for a
/// moment), write, sync, and rename it onto `path`.
fn write_tmp_and_rename(tmp: &Path, path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;

    // A leftover temp file from a crash would keep its old mode if reused.
    let _ = std::fs::remove_file(tmp);
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;
    const T0: u64 = 1_760_000_000;

    fn temp_store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentd-tokens.json");
        (dir, path)
    }

    #[test]
    fn the_store_holds_digests_never_tokens() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        let token = crate::pairing::issue_token();
        let s = sessions.pair_at(&token, Some("Jane's MacBook"), Some("macOS"), T0);
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains(&token), "raw token written to disk");
        assert!(on_disk.contains(&token_digest(&token)));
        assert_eq!(s.token_sha256, token_digest(&token));
        assert_eq!(s.name, "Jane's MacBook");
        assert_eq!(s.platform.as_deref(), Some("macOS"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // And a fresh load from the file accepts the token.
        let reloaded = Sessions::open_at(path, T0 + 1);
        assert_eq!(reloaded.authenticate_at(&token, T0 + 1), Ok(s.id));
    }

    #[test]
    fn digests_compare_in_constant_time_shape() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn unknown_tokens_are_refused() {
        let sessions = Sessions::in_memory();
        sessions.pair_at("right", None, None, T0);
        assert_eq!(
            sessions.authenticate_at("wrong", T0),
            Err(AuthFailure::Unknown)
        );
    }

    #[test]
    fn a_device_unused_for_the_idle_period_expires() {
        let sessions = Sessions::in_memory();
        sessions.pair_at("tok", None, None, T0);
        let limit = IDLE_EXPIRY.as_secs();
        assert!(
            sessions.authenticate_at("tok", T0 + limit).is_ok(),
            "still good on the last second"
        );
        let sessions = Sessions::in_memory();
        sessions.pair_at("tok", None, None, T0);
        assert_eq!(
            sessions.authenticate_at("tok", T0 + limit + 1),
            Err(AuthFailure::Expired)
        );
        assert!(AuthFailure::Expired.message().contains("90 days"));
    }

    #[test]
    fn each_use_slides_the_expiry_forward() {
        let sessions = Sessions::in_memory();
        sessions.pair_at("tok", None, None, T0);
        // Used every 60 days: never more than 90 idle, so it keeps working
        // well past 90 days from pairing.
        for k in 1..=4 {
            assert!(
                sessions.authenticate_at("tok", T0 + k * 60 * DAY).is_ok(),
                "use {k}"
            );
        }
        let last = T0 + 4 * 60 * DAY;
        assert_eq!(
            sessions.list_at(last)[0].expires(),
            last + IDLE_EXPIRY.as_secs()
        );
        assert_eq!(
            sessions.authenticate_at("tok", last + 91 * DAY),
            Err(AuthFailure::Expired)
        );
    }

    #[test]
    fn last_used_reaches_the_disk_at_most_hourly() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        sessions.pair_at("tok", None, None, T0);
        let on_disk = |p: &Path| -> u64 {
            let f: StoreFile = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            f.sessions[0].last_used
        };
        sessions.authenticate_at("tok", T0 + 60).unwrap();
        assert_eq!(on_disk(&path), T0, "a use a minute later isn't written");
        sessions
            .authenticate_at("tok", T0 + TOUCH_WRITE_INTERVAL_S)
            .unwrap();
        assert_eq!(
            on_disk(&path),
            T0 + TOUCH_WRITE_INTERVAL_S,
            "an hour later it is"
        );
    }

    #[test]
    fn revoking_one_device_leaves_the_others() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        let a = sessions.pair_at("a", Some("A"), None, T0);
        let b = sessions.pair_at("b", Some("B"), None, T0);
        let changes = sessions.revocations();
        assert_eq!(
            sessions.revoke(&a.id).map(|s| s.name),
            Some("A".to_string())
        );
        assert!(changes.has_changed().unwrap(), "open sockets hear about it");
        assert_eq!(sessions.authenticate_at("a", T0), Err(AuthFailure::Unknown));
        assert_eq!(sessions.authenticate_at("b", T0), Ok(b.id.clone()));
        assert!(sessions.revoke(&a.id).is_none(), "already gone");
        let reloaded = Sessions::open_at(path, T0);
        assert_eq!(
            reloaded.authenticate_at("a", T0),
            Err(AuthFailure::Unknown),
            "gone after a restart too"
        );
        assert_eq!(reloaded.list_at(T0).len(), 1);
    }

    #[test]
    fn revoke_all_deletes_the_store() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        sessions.pair_at("a", None, None, T0);
        assert_eq!(sessions.revoke_all(), 1);
        assert!(!path.exists());
        assert_eq!(sessions.authenticate_at("a", T0), Err(AuthFailure::Unknown));
    }

    #[test]
    fn old_raw_token_files_are_migrated_in_place() {
        let (_dir, path) = temp_store();
        std::fs::write(&path, r#"["old-token-1","old-token-2","old-token-1"]"#).unwrap();
        let sessions = Sessions::open_at(path.clone(), T0);

        // Both old tokens keep working, as honestly-named sessions.
        let id1 = sessions.authenticate_at("old-token-1", T0).unwrap();
        assert!(sessions.authenticate_at("old-token-2", T0).is_ok());
        let list = sessions.list_at(T0);
        assert_eq!(list.len(), 2, "duplicates collapse");
        assert!(list
            .iter()
            .all(|s| s.name == MIGRATED_NAME && s.created.is_none() && s.platform.is_none()));

        // The file is now the new format, with no raw token left in it.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("old-token-1") && !on_disk.contains("old-token-2"));
        let file: StoreFile = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(file.version, STORE_VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!path.with_extension("tmp").exists());

        // A restart reads the converted file and keeps the same device ids.
        let again = Sessions::open_at(path, T0 + 10);
        assert_eq!(again.authenticate_at("old-token-1", T0 + 10), Ok(id1));
    }

    #[cfg(unix)]
    #[test]
    fn a_migration_that_cannot_write_keeps_the_old_file_and_access() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, path) = temp_store();
        std::fs::write(&path, r#"["old-token"]"#).unwrap();
        // A read-only directory: the converted file can't be written.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let sessions = Sessions::open_at(path.clone(), T0);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        // Running as root ignores directory modes; nothing to check then.
        if std::fs::read_to_string(&path).unwrap() == r#"["old-token"]"# {
            assert!(
                sessions.authenticate_at("old-token", T0).is_ok(),
                "still works this run"
            );
            let next = Sessions::open_at(path, T0);
            assert!(
                next.authenticate_at("old-token", T0).is_ok(),
                "and is converted next start"
            );
        }
    }

    #[test]
    fn the_first_device_is_an_owner_and_the_rest_members() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        assert!(sessions.pair_at("a", None, None, T0).owner, "first ever");
        assert!(!sessions.pair_at("b", None, None, T0).owner);
        assert!(
            sessions.pair_as("c", None, None, true, T0).owner,
            "paired with the local secret"
        );
        // Removing every owner doesn't hand the role to the next pairing...
        for s in sessions.list_at(T0).into_iter().filter(|s| s.owner) {
            sessions.revoke(&s.id);
        }
        assert!(!sessions.pair_at("d", None, None, T0).owner);
        // ...not even after a restart.
        let reloaded = Sessions::open_at(path, T0);
        assert!(!reloaded.pair_at("e", None, None, T0).owner);
        // A reset is a fresh box: its next device is the first owner again.
        reloaded.revoke_all();
        assert!(reloaded.pair_at("f", None, None, T0).owner);
    }

    #[test]
    fn the_last_owner_stays_one() {
        let sessions = Sessions::in_memory();
        let a = sessions.pair_at("a", None, None, unix_now());
        let b = sessions.pair_at("b", None, None, unix_now());
        assert!(sessions.set_owner(&a.id, false).is_err());
        assert!(sessions.set_owner(&b.id, true).unwrap().owner);
        assert!(!sessions.set_owner(&a.id, false).unwrap().owner);
        assert!(sessions.set_owner(&b.id, false).is_err());
        assert!(sessions.set_owner("nope", true).is_err());
    }

    /// A version-2 store (sessions, no roles) with the desktop app's token in
    /// the clear beside it.
    fn unroled_store(desktop_token: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let (dir, path) = temp_store();
        let s = |id: &str, tok: &str| {
            format!(
                r#"{{"id":"{id}","token_sha256":"{}","name":"{id}","created":{T0},"last_used":{T0}}}"#,
                token_digest(tok)
            )
        };
        std::fs::write(
            &path,
            format!(
                r#"{{"version":2,"sessions":[{},{}]}}"#,
                s("desktop", "desk-tok"),
                s("phone", "phone-tok")
            ),
        )
        .unwrap();
        if let Some(t) = desktop_token {
            std::fs::write(path.with_file_name("desktop-token"), t).unwrap();
        }
        (dir, path)
    }

    #[test]
    fn stores_from_before_roles_make_the_box_desktop_the_owner() {
        let (_dir, path) = unroled_store(Some("desk-tok\n"));
        let sessions = Sessions::open_at(path.clone(), T0);
        let by = |id: &str| {
            sessions
                .list_at(T0)
                .into_iter()
                .find(|s| s.id == id)
                .unwrap()
        };
        assert!(by("desktop").owner);
        assert!(
            !by("phone").owner,
            "other devices are members until promoted"
        );
        assert!(
            !sessions.pair_at("new", None, None, T0).owner,
            "the box was paired before: no free owner"
        );
        // Written back with roles, and the desktop's token itself isn't copied in.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("desk-tok"));
        let file: StoreFile = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(file.version, STORE_VERSION);
        assert!(file.owner_claimed);
        assert!(Sessions::open_at(path, T0)
            .list_at(T0)
            .iter()
            .any(|s| s.id == "desktop" && s.owner));
    }

    #[test]
    fn a_sealed_desktop_token_leaves_every_old_device_a_member() {
        // The desktop app pairs again with its local secret, as an owner.
        let (_dir, path) = unroled_store(Some("ttsafe1:AAAA"));
        let sessions = Sessions::open_at(path, T0);
        assert!(sessions.list_at(T0).iter().all(|s| !s.owner));
        assert!(sessions.pair_as("desk-2", None, None, true, T0).owner);
        let (_dir, path) = unroled_store(None);
        assert!(Sessions::open_at(path, T0)
            .list_at(T0)
            .iter()
            .all(|s| !s.owner));
    }

    #[test]
    fn raw_token_files_give_the_desktop_token_ownership_too() {
        let (_dir, path) = temp_store();
        std::fs::write(&path, r#"["desk-tok","phone-tok"]"#).unwrap();
        std::fs::write(path.with_file_name("desktop-token"), "desk-tok").unwrap();
        let sessions = Sessions::open_at(path, T0);
        let desk = sessions.authenticate_at("desk-tok", T0).unwrap();
        assert!(sessions
            .list_at(T0)
            .iter()
            .all(|s| s.owner == (s.id == desk)));
    }

    #[test]
    fn expired_sessions_are_dropped_on_load() {
        let (_dir, path) = temp_store();
        let sessions = Sessions::open_at(path.clone(), T0);
        sessions.pair_at("tok", None, None, T0);
        let later = Sessions::open_at(path, T0 + IDLE_EXPIRY.as_secs() + 1);
        assert!(later.is_empty());
    }

    #[test]
    fn corrupt_store_starts_empty() {
        let (dir, path) = temp_store();
        std::fs::write(&path, "not json {{").unwrap();
        let sessions = Sessions::open_at(path.clone(), T0);
        assert!(sessions.is_empty());
        // The bad file was kept aside, so pairing again can't overwrite it.
        sessions.pair_at("tok", None, None, T0);
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("agentd-tokens.json.corrupt-"))
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), "not json {{");
    }

    #[test]
    fn names_are_cleaned_and_defaulted() {
        let sessions = Sessions::in_memory();
        let s = sessions.pair_at("t", Some("  Jane's\u{7} Mac\n "), Some(""), 1_760_486_400);
        assert_eq!(s.name, "Jane's Mac");
        assert_eq!(s.platform, None);
        let s = sessions.pair_at("u", None, None, 1_760_486_400);
        assert_eq!(s.name, "Device paired 2025-10-15");
        let long = "x".repeat(500);
        assert_eq!(
            sessions.pair_at("v", Some(&long), None, T0).name.len(),
            MAX_NAME
        );
    }

    #[test]
    fn ymd_matches_known_dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(951_782_400), "2000-02-29");
        assert_eq!(ymd(1_791_936_000), "2026-10-14");
    }
}
