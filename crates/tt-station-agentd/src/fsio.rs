// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Writing and reading the agent's state files safely.
//!
//! A plain `fs::write` truncates the file and then writes it, so a crash, a full
//! disk or a power cut in between leaves half a file -- and the loaders, which
//! read with `.ok()`, then quietly start from nothing (no paired devices, no
//! jobs, default settings). Here:
//!
//! * [`write_atomic`] writes a fresh temp file beside the target (`create_new`,
//!   with the final mode from the start), fsyncs it, renames it over the target,
//!   and fsyncs the directory, so a reader sees the old file or the new one,
//!   never half of either, and the rename survives a power cut.
//! * [`read_json_or_backup`] treats a file that won't parse as something to keep,
//!   not drop: it moves it aside to `<name>.corrupt-<unix-seconds>` and logs an
//!   error naming both paths, then lets the caller start fresh. The next save
//!   can't overwrite what was there.
//! * [`ensure_private_dir`] creates a directory `0700` (and tightens an existing
//!   one), for the config dir at startup.
//! * [`private_temp_file`] is a temp file only this user can read, with an
//!   unpredictable name (no `/tmp/<name>-<pid>` for someone else to pre-create).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Owner read/write only: tokens, people, recordings, settings.
pub const PRIVATE: u32 = 0o600;

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A name for a temp file nobody can guess.
fn random_suffix() -> String {
    format!("{:016x}", rand::random::<u64>())
}

fn open_new(path: &Path, mode: u32) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    opts.open(path)
}

/// fsync a directory, so a rename into it is on disk.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Replace `path` with `bytes`, atomically and durably, at `mode` (e.g.
/// [`PRIVATE`]). The parent directory is created if missing.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = parent.join(format!(".{name}.tmp-{}", random_suffix()));
    let result = (|| {
        let mut file = open_new(&tmp, mode)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        sync_dir(&parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// [`write_atomic`] from a reader, for contents too big to hold in memory (a
/// recording restored from an export). Returns the bytes written.
pub fn write_atomic_from(path: &Path, mut from: impl io::Read, mode: u32) -> io::Result<u64> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = parent.join(format!(".{name}.tmp-{}", random_suffix()));
    let result = (|| {
        let mut file = open_new(&tmp, mode)?;
        let n = io::copy(&mut from, &mut file)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        sync_dir(&parent)?;
        Ok(n)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// [`write_atomic`] of `value` as pretty JSON.
pub fn write_json<T: Serialize + ?Sized>(path: &Path, value: &T, mode: u32) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes, mode)
}

/// [`write_json`], logging a failure instead of returning it, for the saves
/// that never reported one.
pub fn save_json<T: Serialize + ?Sized>(path: &Path, value: &T, mode: u32) {
    if let Err(e) = write_json(path, value, mode) {
        crate::log::error(format!("couldn't save {}: {e}", path.display()));
    }
}

/// Move a file that won't load out of the way, to `<name>.corrupt-<unix-seconds>`
/// beside it, and say so loudly. Returns where it went.
pub fn quarantine(path: &Path, why: &str) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    let mut aside = path.with_file_name(format!("{name}.corrupt-{}", unix_now()));
    if aside.exists() {
        aside = path.with_file_name(format!("{name}.corrupt-{}-{}", unix_now(), random_suffix()));
    }
    match std::fs::rename(path, &aside) {
        Ok(()) => {
            crate::log::error(format!(
                "{} is unreadable ({why}); moved it to {} and starting without it",
                path.display(),
                aside.display()
            ));
            Some(aside)
        }
        Err(e) => {
            crate::log::error(format!(
                "{} is unreadable ({why}) and couldn't be moved aside ({e}); starting without it",
                path.display()
            ));
            None
        }
    }
}

/// Load `path` as JSON. `None` when it doesn't exist, can't be read (logged), or
/// doesn't parse -- in which case it's moved aside first (see [`quarantine`]).
pub fn read_json_or_backup<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            // Not the content's fault (permissions, I/O): leave it where it is.
            crate::log::error(format!("couldn't read {}: {e}", path.display()));
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => Some(v),
        Err(e) => {
            quarantine(path, &e.to_string());
            None
        }
    }
}

/// Create `dir` (and parents) if missing and make it `0700`.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// A new, empty `0600` file in `$XDG_RUNTIME_DIR` (else the temp dir) named
/// `<prefix><random><suffix>`, created exclusively so no one can have planted it.
pub fn private_temp_file(prefix: &str, suffix: &str) -> io::Result<(File, PathBuf)> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    for _ in 0..8 {
        let path = dir.join(format!("{prefix}{}{suffix}", random_suffix()));
        match open_new(&path, PRIVATE) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "no free temp file name",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn write_atomic_replaces_the_file_at_the_mode_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");
        write_atomic(&path, b"one", PRIVATE).unwrap();
        write_atomic(&path, b"two", PRIVATE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_tightens_a_file_that_was_world_readable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("people.json");
        std::fs::write(&path, b"{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        write_json(&path, &serde_json::json!({ "a": 1 }), PRIVATE).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn write_atomic_from_streams_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/input.ogg");
        let n = write_atomic_from(&path, &b"0123456789"[..], PRIVATE).unwrap();
        assert_eq!(n, 10);
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789");
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn a_corrupt_file_is_moved_aside_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.json");
        std::fs::write(&path, b"{\"half\": ").unwrap();
        let loaded: Option<serde_json::Value> = read_json_or_backup(&path);
        assert!(loaded.is_none());
        assert!(!path.exists());
        let aside: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].starts_with("jobs.json.corrupt-"), "{aside:?}");
        assert_eq!(
            std::fs::read(dir.path().join(&aside[0])).unwrap(),
            b"{\"half\": "
        );
    }

    #[test]
    fn missing_and_good_files_load_as_expected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        assert!(read_json_or_backup::<serde_json::Value>(&path).is_none());
        write_json(&path, &vec![1, 2], PRIVATE).unwrap();
        assert_eq!(read_json_or_backup::<Vec<u32>>(&path), Some(vec![1, 2]));
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs_and_temp_files_are_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("tt-station");
        std::fs::create_dir(&cfg).unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o775)).unwrap();
        ensure_private_dir(&cfg).unwrap();
        assert_eq!(mode(&cfg), 0o700);

        let (_f, a) = private_temp_file("tt-say-", ".wav").unwrap();
        let (_g, b) = private_temp_file("tt-say-", ".wav").unwrap();
        assert_ne!(a, b);
        assert_eq!(mode(&a), 0o600);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }
}
