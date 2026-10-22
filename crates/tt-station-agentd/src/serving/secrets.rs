// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Secrets handed to model servers as files, not environment variables.
//!
//! A container's environment is readable by anyone who can run `docker
//! inspect`, and a `systemd-run --setenv` unit's by anyone who can run
//! `systemctl --user show`. A Hugging Face token passed either way stays on
//! display for as long as the server runs. Instead the agent writes the token
//! to a file only this user can read and points the server at it with
//! `HF_TOKEN_PATH`, which `huggingface_hub` reads in place of
//! `$HF_HOME/token` (checked in the hub versions the box's images and venvs
//! carry: 1.16.1 in the tt-inference-server release images and tt-metal's
//! `python_env`, 2.1.1 in tt-model's venv).
//!
//! The file lives in `$XDG_RUNTIME_DIR/tt-station` (a per-user tmpfs, so the
//! token never reaches the disk from here), else in the agent's own config
//! directory under `secrets/`. Directory 0700, file 0400, replaced atomically:
//! a running container keeps the file it was started with, and a changed
//! token takes effect at the next model start.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The file name of the Hugging Face token inside [`default_dir`].
pub const HF_TOKEN_FILE: &str = "hf_token";

/// Where a container finds the token file it is given (`--volume ...:ro`).
pub const CONTAINER_HF_TOKEN_PATH: &str = "/run/secrets/hf_token";

/// The directory the agent keeps launch secrets in: `$XDG_RUNTIME_DIR/tt-station`
/// when the runtime directory exists, else `$TT_CONFIG_DIR/secrets` or
/// `~/.config/tt-station/secrets`.
pub fn default_dir() -> PathBuf {
    if let Some(run) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if run.is_absolute() && run.is_dir() {
            return run.join("tt-station");
        }
    }
    let config = std::env::var_os("TT_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/tt-station")
        });
    config.join("secrets")
}

/// Write `value` to `dir/name`, readable by this user only (0400) in a
/// directory only this user can enter (0700), and return its path.
///
/// Written to a fresh temporary file and renamed into place, so a reader
/// never sees a half-written token and a container still holding the old
/// file is unaffected.
pub fn write_secret(dir: &Path, name: &str, value: &str) -> Result<PathBuf> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("couldn't create {}", dir.display()))?;
    // `mode` only applies to directories it creates: tighten one that was there.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("couldn't restrict {}", dir.display()))?;

    let path = dir.join(name);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.{nanos}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(value.as_bytes())?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o400))?;
        fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("couldn't write {}", path.display()));
    }
    Ok(path)
}

/// Write the Hugging Face token into `dir` (see [`write_secret`]).
pub fn write_hf_token(dir: &Path, token: &str) -> Result<PathBuf> {
    write_secret(dir, HF_TOKEN_FILE, token.trim())
}

/// The Hugging Face token the agent knows: `HF_TOKEN` in its environment,
/// else the one `huggingface-cli login` saved (`~/.cache/huggingface/token`).
pub fn hf_token() -> Option<String> {
    std::env::var("HF_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(|| {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            fs::read_to_string(home.join(".cache/huggingface/token"))
                .ok()
                .map(|t| t.trim().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn token_file_is_private_and_holds_only_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tt-station");
        let path = write_hf_token(&dir, " hf_abc \n").unwrap();
        assert_eq!(path, dir.join(HF_TOKEN_FILE));
        assert_eq!(fs::read_to_string(&path).unwrap(), "hf_abc");
        assert_eq!(mode(&path), 0o400);
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn rewriting_replaces_a_read_only_file_and_tightens_the_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("s");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        write_hf_token(&dir, "old").unwrap();
        let path = write_hf_token(&dir, "new").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(mode(&dir), 0o700);
        // No temporary files left behind.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(HF_TOKEN_FILE)]);
    }
}
