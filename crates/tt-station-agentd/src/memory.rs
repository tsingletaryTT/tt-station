// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What the assistant remembers about its owner across conversations -- facts
//! and preferences it saved with `remember` ("prefers metric units", "the dog
//! is called Juno"), included in every chat's system prompt and kept privately
//! on the box (`memories.json`). The owner lists and deletes them from the app
//! (`/memories`).

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Most memories kept; the oldest go first.
pub const MAX_MEMORIES: usize = 60;
/// Longest single memory.
pub const MAX_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Memory {
    pub id: String,
    pub text: String,
    /// Unix seconds.
    pub added: u64,
    /// Whose memory it is (`crate::people`); `None` is the household's,
    /// visible to everyone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
}

/// Whether someone (or no one in particular) sees a memory: their own and
/// the household's.
fn visible(m: &Memory, person: Option<&str>) -> bool {
    m.person.is_none() || m.person.as_deref() == person
}

#[derive(Debug)]
pub struct Memories {
    path: Option<PathBuf>,
    items: Mutex<Vec<Memory>>,
}

impl Memories {
    pub fn in_memory() -> Self {
        Memories {
            path: None,
            items: Mutex::new(Vec::new()),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let items = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Memories {
            path: Some(path),
            items: Mutex::new(items),
        }
    }

    pub fn list(&self) -> Vec<Memory> {
        self.items.lock().expect("memories mutex poisoned").clone()
    }

    /// What one person (or no one in particular) sees.
    pub fn list_for(&self, person: Option<&str>) -> Vec<Memory> {
        self.list()
            .into_iter()
            .filter(|m| visible(m, person))
            .collect()
    }

    /// Drop everything someone saved (they were removed from the box).
    pub fn forget_person(&self, person: &str) {
        let mut items = self.items.lock().expect("memories mutex poisoned");
        let before = items.len();
        items.retain(|m| m.person.as_deref() != Some(person));
        let changed = items.len() != before;
        drop(items);
        if changed {
            self.save();
        }
    }

    /// Save a fact. A near-duplicate (same words, any case) isn't saved twice.
    pub fn add(&self, text: &str, now: u64, person: Option<&str>) -> Result<Memory, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("nothing to remember".to_string());
        }
        let text: String = text.chars().take(MAX_CHARS).collect();
        let key = normalize(&text);
        let mut items = self.items.lock().expect("memories mutex poisoned");
        if let Some(existing) = items
            .iter()
            .find(|m| visible(m, person) && normalize(&m.text) == key)
        {
            return Ok(existing.clone());
        }
        let memory = Memory {
            id: crate::pairing::issue_pair_id()[..12].to_string(),
            text,
            added: now,
            person: person.map(str::to_string),
        };
        items.push(memory.clone());
        let overflow = items.len().saturating_sub(MAX_MEMORIES);
        items.drain(..overflow);
        drop(items);
        self.save();
        Ok(memory)
    }

    /// Forget by id, or every memory containing `text` (case-insensitive).
    /// Returns what was forgotten.
    pub fn forget(&self, id_or_text: &str, person: Option<&str>) -> Vec<Memory> {
        let needle = id_or_text.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut items = self.items.lock().expect("memories mutex poisoned");
        let (gone, kept): (Vec<Memory>, Vec<Memory>) = items.drain(..).partition(|m| {
            visible(m, person)
                && (m.id == id_or_text.trim() || m.text.to_lowercase().contains(&needle))
        });
        *items = kept;
        drop(items);
        if !gone.is_empty() {
            self.save();
        }
        gone
    }

    /// Forget what this person sees: their own memories and the household's.
    pub fn clear(&self, person: Option<&str>) {
        self.items
            .lock()
            .expect("memories mutex poisoned")
            .retain(|m| !visible(m, person));
        self.save();
    }

    /// The system-prompt section, or empty when there's nothing remembered.
    pub fn prompt_note(&self, person: Option<&crate::people::Person>) -> String {
        let items = self.list_for(person.map(|p| p.id.as_str()));
        let who = person
            .map(|p| format!("\nYou are talking with {}.", p.name))
            .unwrap_or_default();
        if items.is_empty() {
            return who;
        }
        let lines: Vec<String> = items.iter().map(|m| format!("- {}", m.text)).collect();
        format!(
            "{who}\nWhat you remember about the user (from earlier conversations; use it when relevant, \
             don't recite it):\n{}",
            lines.join("\n")
        )
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let items = self.list();
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec_pretty(&items).map_err(std::io::Error::other)?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            eprintln!("tt-station-agentd: couldn't save memories: {e}");
        }
    }
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_dedupes_forgets_and_persists() {
        let dir = std::env::temp_dir().join(format!("tt-memories-{}", std::process::id()));
        let path = dir.join("memories.json");
        let m = Memories::open(&path);
        m.add("Prefers metric units.", 1, None).unwrap();
        m.add("prefers metric units", 2, None).unwrap();
        m.add("Their dog is called Juno", 3, None).unwrap();
        assert_eq!(m.list().len(), 2);
        assert!(m.prompt_note(None).contains("- Their dog is called Juno"));
        assert!(m.add("   ", 4, None).is_err());

        let reopened = Memories::open(&path);
        assert_eq!(reopened.list().len(), 2);
        let gone = reopened.forget("juno", None);
        assert_eq!(gone.len(), 1);
        assert_eq!(Memories::open(&path).list().len(), 1);
        reopened.clear(None);
        assert_eq!(reopened.prompt_note(None), "");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_the_newest() {
        let m = Memories::in_memory();
        for i in 0..MAX_MEMORIES + 5 {
            m.add(&format!("fact number {i}"), i as u64, None).unwrap();
        }
        let items = m.list();
        assert_eq!(items.len(), MAX_MEMORIES);
        assert_eq!(items[0].text, "fact number 5");
    }

    #[test]
    fn each_person_sees_their_own_and_the_households() {
        let m = Memories::in_memory();
        m.add("The wifi is called Orchard", 1, None).unwrap();
        m.add("Ana is vegetarian", 2, Some("ana")).unwrap();
        m.add("Ben runs on Sundays", 3, Some("ben")).unwrap();
        let ana: Vec<String> = m
            .list_for(Some("ana"))
            .into_iter()
            .map(|x| x.text)
            .collect();
        assert_eq!(ana, ["The wifi is called Orchard", "Ana is vegetarian"]);
        assert_eq!(
            m.list_for(None).len(),
            1,
            "no one in particular sees the household's only"
        );
        assert!(
            m.forget("sundays", Some("ana")).is_empty(),
            "Ana can't forget Ben's"
        );
        let person = crate::people::Person {
            id: "ana".into(),
            name: "Ana".into(),
            added: 0,
        };
        let note = m.prompt_note(Some(&person));
        assert!(
            note.contains("You are talking with Ana.")
                && note.contains("vegetarian")
                && !note.contains("Sundays")
        );
        m.forget_person("ben");
        assert_eq!(m.list().len(), 2);
    }
}
