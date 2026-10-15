// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Images the assistant made in chat (`create_image`), kept on the box so a
//! client can fetch them by id (`GET /creations/{id}`) -- the model only ever
//! sees ids, never the pixels. Newest [`KEEP`] are kept.

use std::path::{Path, PathBuf};

use base64::Engine as _;

/// How many images to keep.
pub const KEEP: usize = 300;

#[derive(Debug, Clone)]
pub struct Creations {
    dir: PathBuf,
}

impl Creations {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Creations { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Save a base64 JPEG (with its prompt alongside); returns the new id.
    pub fn save_b64(&self, b64: &str, prompt: &str) -> std::io::Result<String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(std::io::Error::other)?;
        crate::fsio::ensure_private_dir(&self.dir)?;
        let id = crate::pairing::issue_pair_id()[..16].to_string();
        use crate::fsio::{write_atomic, PRIVATE};
        write_atomic(&self.dir.join(format!("{id}.jpg")), &bytes, PRIVATE)?;
        write_atomic(
            &self.dir.join(format!("{id}.txt")),
            prompt.as_bytes(),
            PRIVATE,
        )?;
        self.prune();
        Ok(id)
    }

    /// The JPEG for `id`, if it exists. Ids are hex, so nothing else resolves.
    pub fn get(&self, id: &str) -> Option<Vec<u8>> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read(self.dir.join(format!("{id}.jpg"))).ok()
    }

    pub fn prompt(&self, id: &str) -> Option<String> {
        if !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        std::fs::read_to_string(self.dir.join(format!("{id}.txt"))).ok()
    }

    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut jpgs: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jpg"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        if jpgs.len() <= KEEP {
            return;
        }
        jpgs.sort_by_key(|(t, _)| *t);
        for (_, path) in jpgs.iter().take(jpgs.len() - KEEP) {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(path.with_extension("txt"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_fetches_and_refuses_odd_ids() {
        let dir = std::env::temp_dir().join(format!("tt-creations-{}", std::process::id()));
        let c = Creations::new(&dir);
        let id = c.save_b64("aGk=", "a fox").unwrap();
        assert_eq!(c.get(&id).unwrap(), b"hi");
        assert_eq!(c.prompt(&id).unwrap(), "a fox");
        assert!(c.get("../../etc/passwd").is_none());
        assert!(c.get("").is_none());
        assert!(c.save_b64("not base64!!", "x").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}
