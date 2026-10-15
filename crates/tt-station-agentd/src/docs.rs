// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The user's documents, searchable by the assistant -- private Q&A over your own
//! files, entirely on the box.
//!
//! Clients upload plain text (the phone extracts it: PDFKit for PDFs), the agent
//! splits it into overlapping passages, and the assistant's `search_documents`
//! tool ranks passages with BM25. Lexical retrieval on purpose: the box serves one
//! model at a time, so an embedding model can't run beside the chat model -- and
//! for "what does my lease say about pets" keyword ranking does well.
//!
//! Stored as one JSON file per document under the docs directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Target passage size, in characters, and how much consecutive passages overlap.
const PASSAGE_CHARS: usize = 900;
const OVERLAP_CHARS: usize = 150;
/// Upload ceiling: a long book is ~1M characters.
/// How much keyword matching counts in a blended search (meaning counts for the rest).
const KEYWORD_WEIGHT: f32 = 0.35;

pub const MAX_DOC_CHARS: usize = 2_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub added: u64,
    pub chars: usize,
    pub passages: Vec<String>,
    /// The Library folder it came from (its id), for documents indexed from a folder.
    #[serde(default)]
    pub source: Option<String>,
    /// Its file on the box, for documents indexed from a folder.
    #[serde(default)]
    pub path: Option<String>,
    /// The file's modification time when indexed (unix seconds).
    #[serde(default)]
    pub mtime: u64,
}

/// A document's listing (no text).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DocSummary {
    pub id: String,
    pub name: String,
    pub added: u64,
    pub chars: usize,
    pub passages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One search result.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Hit {
    pub document: String,
    pub passage: usize,
    pub score: f32,
    pub text: String,
    /// The file on the box, when it came from a Library folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug)]
pub struct DocStore {
    dir: PathBuf,
    docs: Mutex<Vec<Document>>,
    /// Each document's passages as meaning vectors (one per passage), when the
    /// embedding service made them: `<id>.vec`, little-endian f32.
    vectors: Mutex<HashMap<String, Vec<Vec<f32>>>>,
}

impl DocStore {
    /// Open (and load) the store in `dir`; a missing directory is an empty store.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let mut docs: Vec<Document> = std::fs::read_dir(&dir)
            .map(|it| {
                it.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| std::fs::read(e.path()).ok())
                    .filter_map(|b| serde_json::from_slice::<Document>(&b).ok())
                    .collect()
            })
            .unwrap_or_default();
        docs.sort_by_key(|d| std::cmp::Reverse(d.added));
        let mut vectors = HashMap::new();
        for d in &docs {
            if let Some(v) = read_vectors(&dir.join(format!("{}.vec", d.id)), d.passages.len()) {
                vectors.insert(d.id.clone(), v);
            }
        }
        DocStore {
            dir,
            docs: Mutex::new(docs),
            vectors: Mutex::new(vectors),
        }
    }

    pub fn list(&self) -> Vec<DocSummary> {
        self.docs
            .lock()
            .expect("docs mutex poisoned")
            .iter()
            .map(|d| DocSummary {
                id: d.id.clone(),
                name: d.name.clone(),
                added: d.added,
                chars: d.chars,
                passages: d.passages.len(),
                source: d.source.clone(),
            })
            .collect()
    }

    /// Documents from one Library folder: (id, path, mtime).
    pub fn from_source(&self, source: &str) -> Vec<(String, String, u64)> {
        self.docs
            .lock()
            .expect("docs mutex poisoned")
            .iter()
            .filter(|d| d.source.as_deref() == Some(source))
            .map(|d| (d.id.clone(), d.path.clone().unwrap_or_default(), d.mtime))
            .collect()
    }

    /// Passages of a document, for embedding.
    pub fn passages(&self, id: &str) -> Option<Vec<String>> {
        self.docs
            .lock()
            .expect("docs mutex poisoned")
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.passages.clone())
    }

    /// Documents still without meaning vectors (ids).
    pub fn without_vectors(&self) -> Vec<String> {
        let have = self.vectors.lock().expect("docs mutex poisoned");
        self.docs
            .lock()
            .expect("docs mutex poisoned")
            .iter()
            .filter(|d| !have.contains_key(&d.id) && !d.passages.is_empty())
            .map(|d| d.id.clone())
            .collect()
    }

    /// How many passages have meaning vectors, of all.
    pub fn vector_coverage(&self) -> (usize, usize) {
        let have = self.vectors.lock().expect("docs mutex poisoned");
        let docs = self.docs.lock().expect("docs mutex poisoned");
        let total = docs.iter().map(|d| d.passages.len()).sum();
        let done = docs
            .iter()
            .filter(|d| have.contains_key(&d.id))
            .map(|d| d.passages.len())
            .sum();
        (done, total)
    }

    /// Stores a document's meaning vectors (one per passage, in order).
    pub fn set_vectors(&self, id: &str, vectors: Vec<Vec<f32>>) -> std::io::Result<()> {
        let mut bytes =
            Vec::with_capacity(vectors.len() * vectors.first().map_or(0, Vec::len) * 4 + 4);
        bytes.extend_from_slice(&(vectors.first().map_or(0, Vec::len) as u32).to_le_bytes());
        for v in &vectors {
            for x in v {
                bytes.extend_from_slice(&x.to_le_bytes());
            }
        }
        std::fs::write(self.dir.join(format!("{id}.vec")), bytes)?;
        self.vectors
            .lock()
            .expect("docs mutex poisoned")
            .insert(id.to_string(), vectors);
        Ok(())
    }

    /// Adds (or replaces) a file from a Library folder.
    pub fn add_file(
        &self,
        source: &str,
        name: &str,
        path: &str,
        mtime: u64,
        text: &str,
        now: u64,
    ) -> std::io::Result<String> {
        self.remove_path(source, path);
        let doc = Document {
            id: crate::pairing::issue_pair_id(),
            name: name.to_string(),
            added: now,
            chars: text.chars().count(),
            passages: split_passages(text),
            source: Some(source.to_string()),
            path: Some(path.to_string()),
            mtime,
        };
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.path(&doc.id),
            serde_json::to_vec(&doc).map_err(std::io::Error::other)?,
        )?;
        let id = doc.id.clone();
        self.docs
            .lock()
            .expect("docs mutex poisoned")
            .insert(0, doc);
        Ok(id)
    }

    /// Removes the document for one file of a Library folder.
    pub fn remove_path(&self, source: &str, path: &str) {
        let ids: Vec<String> = self
            .docs
            .lock()
            .expect("docs mutex poisoned")
            .iter()
            .filter(|d| d.source.as_deref() == Some(source) && d.path.as_deref() == Some(path))
            .map(|d| d.id.clone())
            .collect();
        for id in ids {
            self.delete(&id);
        }
    }

    /// Removes everything from one Library folder.
    pub fn remove_source(&self, source: &str) -> usize {
        let ids: Vec<String> = self
            .from_source(source)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        for id in &ids {
            self.delete(id);
        }
        ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.lock().expect("docs mutex poisoned").is_empty()
    }

    /// Add a document (replacing one with the same name).
    pub fn add(&self, name: &str, text: &str, now: u64) -> std::io::Result<DocSummary> {
        let passages = split_passages(text);
        let doc = Document {
            id: crate::pairing::issue_pair_id(),
            name: name.trim().to_string(),
            added: now,
            chars: text.chars().count(),
            passages,
            source: None,
            path: None,
            mtime: 0,
        };
        std::fs::create_dir_all(&self.dir)?;
        let mut docs = self.docs.lock().expect("docs mutex poisoned");
        if let Some(old) = docs.iter().position(|d| d.name == doc.name) {
            let removed = docs.remove(old);
            let _ = std::fs::remove_file(self.path(&removed.id));
        }
        std::fs::write(
            self.path(&doc.id),
            serde_json::to_vec(&doc).map_err(std::io::Error::other)?,
        )?;
        let summary = DocSummary {
            id: doc.id.clone(),
            name: doc.name.clone(),
            added: doc.added,
            chars: doc.chars,
            passages: doc.passages.len(),
            source: None,
        };
        docs.insert(0, doc);
        Ok(summary)
    }

    pub fn delete(&self, id: &str) -> bool {
        let mut docs = self.docs.lock().expect("docs mutex poisoned");
        let Some(index) = docs.iter().position(|d| d.id == id) else {
            return false;
        };
        docs.remove(index);
        let _ = std::fs::remove_file(self.path(id));
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.vec")));
        self.vectors.lock().expect("docs mutex poisoned").remove(id);
        true
    }

    /// The `top_k` passages most relevant to `query` (BM25), across all documents
    /// or only those whose name contains `in_document`.
    pub fn search(&self, query: &str, top_k: usize, in_document: Option<&str>) -> Vec<Hit> {
        self.search_hybrid(query, None, top_k, in_document)
    }

    /// Keyword (BM25) ranking, blended with meaning when `query_vector` is given
    /// (a weighted blend of the two scores): "pets" also finds the
    /// passage about "animals", and an exact name still ranks first.
    pub fn search_hybrid(
        &self,
        query: &str,
        query_vector: Option<&[f32]>,
        top_k: usize,
        in_document: Option<&str>,
    ) -> Vec<Hit> {
        let docs = self.docs.lock().expect("docs mutex poisoned");
        let vectors = self.vectors.lock().expect("docs mutex poisoned");
        let filter = in_document.map(str::to_lowercase);
        let corpus: Vec<(&Document, usize, &str)> = docs
            .iter()
            .filter(|d| {
                filter
                    .as_deref()
                    .is_none_or(|f| d.name.to_lowercase().contains(f))
            })
            .flat_map(|d| {
                d.passages
                    .iter()
                    .enumerate()
                    .map(move |(i, p)| (d, i, p.as_str()))
            })
            .collect();
        let terms = tokenize(query);
        if corpus.is_empty() || (terms.is_empty() && query_vector.is_none()) {
            return Vec::new();
        }
        let tokenized: Vec<Vec<String>> = corpus.iter().map(|(_, _, t)| tokenize(t)).collect();
        let n = tokenized.len() as f32;
        let avg_len = tokenized.iter().map(Vec::len).sum::<usize>() as f32 / n;
        let mut df: HashMap<&str, usize> = HashMap::new();
        for doc in &tokenized {
            let mut seen: Vec<&str> = doc.iter().map(String::as_str).collect();
            seen.sort_unstable();
            seen.dedup();
            for t in seen {
                *df.entry(t).or_default() += 1;
            }
        }
        let (k1, b) = (1.2_f32, 0.75_f32);
        let mut scored: Vec<(f32, usize)> = tokenized
            .iter()
            .enumerate()
            .map(|(i, doc)| {
                let len = doc.len() as f32;
                let score: f32 = terms
                    .iter()
                    .map(|term| {
                        let tf = doc.iter().filter(|t| *t == term).count() as f32;
                        if tf == 0.0 {
                            return 0.0;
                        }
                        let df = *df.get(term.as_str()).unwrap_or(&0) as f32;
                        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                        idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * len / avg_len))
                    })
                    .sum();
                (score, i)
            })
            .filter(|(s, _)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Blend in meaning: rank passages by cosine similarity too, then fuse ranks.
        let fused: Vec<(f32, usize)> = match query_vector {
            Some(q) => {
                let qn = norm(q);
                let mut by_meaning: Vec<(f32, usize)> = corpus
                    .iter()
                    .enumerate()
                    .filter_map(|(i, (d, p, _))| {
                        let v = vectors.get(&d.id)?.get(*p)?;
                        Some((dot(q, v) / (qn * norm(v)).max(1e-9), i))
                    })
                    .collect();
                by_meaning.sort_by(|a, b| b.0.total_cmp(&a.0));
                // A weighted blend of the two scores, each scaled to 0..1 over
                // its top 100: meaning counts for more, so one shared common
                // word ("due") can't beat a passage that's plainly about the
                // question; exact terms (names, error codes) still lift a passage.
                let mut score: HashMap<usize, f32> = HashMap::new();
                let top_kw = scored.first().map_or(0.0, |s| s.0);
                for (s, i) in scored.iter().take(100) {
                    *score.entry(*i).or_default() += KEYWORD_WEIGHT * s / top_kw.max(1e-9);
                }
                let meaning = &by_meaning[..by_meaning.len().min(100)];
                if let (Some(hi), Some(lo)) = (meaning.first(), meaning.last()) {
                    let span = (hi.0 - lo.0).max(1e-6);
                    for (s, i) in meaning {
                        *score.entry(*i).or_default() += (1.0 - KEYWORD_WEIGHT) * (s - lo.0) / span;
                    }
                }
                let mut out: Vec<(f32, usize)> = score.into_iter().map(|(i, s)| (s, i)).collect();
                out.sort_by(|a, b| b.0.total_cmp(&a.0));
                out
            }
            None => scored,
        };
        fused
            .into_iter()
            .take(top_k)
            .map(|(score, i)| Hit {
                document: corpus[i].0.name.clone(),
                passage: corpus[i].1,
                score,
                text: corpus[i].2.to_string(),
                path: corpus[i].0.path.clone(),
            })
            .collect()
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// A `.vec` file: a u32 dimension, then `passages` vectors of it.
fn read_vectors(path: &Path, passages: usize) -> Option<Vec<Vec<f32>>> {
    let bytes = std::fs::read(path).ok()?;
    let dim = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if dim == 0 || bytes.len() != 4 + passages * dim * 4 {
        return None;
    }
    Some(
        bytes[4..]
            .chunks_exact(dim * 4)
            .map(|c| {
                c.chunks_exact(4)
                    .map(|f| f32::from_le_bytes([f[0], f[1], f[2], f[3]]))
                    .collect()
            })
            .collect(),
    )
}

/// Split text into ~[`PASSAGE_CHARS`] passages on paragraph/sentence boundaries,
/// overlapping by ~[`OVERLAP_CHARS`] so an answer straddling a cut is still found.
pub fn split_passages(text: &str) -> Vec<String> {
    let clean: String = text.replace("\r\n", "\n");
    let mut passages = Vec::new();
    let chars: Vec<char> = clean.chars().collect();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + PASSAGE_CHARS).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            // Prefer to end at a paragraph break, else a sentence end, else a space,
            // in the last third of the window.
            let window_start = start + PASSAGE_CHARS * 2 / 3;
            let find =
                |pred: &dyn Fn(usize) -> bool| (window_start..hard_end).rev().find(|&i| pred(i));
            end = find(&|i| chars[i] == '\n' && i > 0 && chars[i - 1] == '\n')
                .or_else(|| {
                    find(&|i| {
                        matches!(chars[i], '.' | '!' | '?')
                            && chars.get(i + 1).is_some_and(|c| c.is_whitespace())
                    })
                    .map(|i| i + 1)
                })
                .or_else(|| find(&|i| chars[i].is_whitespace()))
                .unwrap_or(hard_end);
        }
        let passage: String = chars[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if !passage.is_empty() {
            passages.push(passage);
        }
        if end >= chars.len() {
            break;
        }
        start = end.saturating_sub(OVERLAP_CHARS).max(start + 1);
    }
    passages
}

const STOPWORDS: [&str; 34] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "does", "for", "from", "how", "i", "in",
    "is", "it", "my", "of", "on", "or", "that", "the", "this", "to", "was", "what", "when",
    "where", "which", "who", "why", "with", "you",
];

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(w))
        .map(|w| {
            // A light stem: "pets" and "pet" should match.
            if w.len() > 4 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> DocStore {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        DocStore::open(std::env::temp_dir().join(format!(
            "tt-docs-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )))
    }

    #[test]
    fn passages_cover_the_text_overlap_and_respect_sentences() {
        let text = "Sentence one is here. ".repeat(200);
        let passages = split_passages(&text);
        assert!(passages.len() > 3);
        assert!(passages.iter().all(|p| p.chars().count() <= PASSAGE_CHARS));
        assert!(
            passages[0].ends_with('.'),
            "{:?}",
            &passages[0][passages[0].len() - 20..]
        );
        assert_eq!(split_passages("short"), vec!["short".to_string()]);
        assert!(split_passages("   ").is_empty());
    }

    #[test]
    fn search_ranks_the_relevant_passage_first_and_survives_reload() {
        let s = store();
        s.add(
            "Lease.pdf",
            &format!(
                "{} Pets: tenants may keep one cat or one small dog with a $300 deposit. {}",
                "Rent is due on the first of the month. ".repeat(40),
                "Parking spaces are assigned by the landlord. ".repeat(40)
            ),
            1,
        )
        .unwrap();
        s.add(
            "Recipes.txt",
            &"Mix flour and water, then bake at 220C. ".repeat(60),
            2,
        )
        .unwrap();
        let hits = s.search("Can I have a dog or pets?", 3, None);
        assert_eq!(hits[0].document, "Lease.pdf");
        assert!(hits[0].text.contains("small dog"), "{}", hits[0].text);
        assert!(
            s.search("dog", 3, Some("recipes")).is_empty(),
            "filter by document"
        );

        let reopened = DocStore::open(s.dir());
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(
            reopened.search("bake flour", 1, None)[0].document,
            "Recipes.txt"
        );
    }

    #[test]
    fn adding_the_same_name_replaces_and_delete_removes() {
        let s = store();
        let first = s.add("notes.md", "alpha beta", 1).unwrap();
        s.add("notes.md", "gamma delta", 2).unwrap();
        assert_eq!(s.list().len(), 1);
        assert!(s.search("alpha", 1, None).is_empty());
        assert!(!s.delete(&first.id), "the replaced id is gone");
        let id = s.list()[0].id.clone();
        assert!(s.delete(&id));
        assert!(s.is_empty());
    }
}
