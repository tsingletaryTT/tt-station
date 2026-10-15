// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! People: one box, a household. Each paired phone says who it belongs to,
//! and the assistant then knows who it's talking with and keeps each person's
//! memories to themselves (memories saved by no one in particular stay
//! shared). Nobody signs up for anything: a person is a name on the box.
//!
//! - `GET /people`: everyone, and who this phone is (`me`).
//! - `POST /people {name, me}`: add someone; `me: true` also makes this phone theirs.
//! - `PUT /people/me {person}`: this phone is that person's (or `null`: no one in particular).
//! - `PATCH /people/{id} {name}`, `DELETE /people/{id}` (their memories go
//!   with them): owners only.
//!
//! Phones are told apart by their pairing token, stored here only as a
//! SHA-256 digest.
//!
//! Being someone means reading their memories, so a device can't just say
//! who it is: a member may make itself a person nobody's device has yet
//! (adding them, or claiming one no device belongs to). Making a device the
//! person another device already belongs to takes an owner -- from that
//! owner's own device here, or for any device with `PUT /devices/{id}/person`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::AppState;

pub use crate::auth::Who;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Person {
    pub id: String,
    pub name: String,
    /// Unix seconds.
    pub added: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    people: Vec<Person>,
    /// SHA-256 of a pairing token → person id.
    phones: HashMap<String, String>,
}

#[derive(Debug)]
pub struct People {
    path: Option<PathBuf>,
    book: Mutex<Book>,
}

pub const MAX_PEOPLE: usize = 12;
const MAX_NAME: usize = 40;

fn digest(token: &str) -> String {
    crate::sessions::token_digest(token)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl People {
    pub fn in_memory() -> Self {
        People {
            path: None,
            book: Mutex::new(Book::default()),
        }
    }

    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let book = crate::fsio::read_json_or_backup(&path).unwrap_or_default();
        People {
            path: Some(path),
            book: Mutex::new(book),
        }
    }

    fn save(&self, book: &Book) {
        let Some(path) = &self.path else { return };
        // Who may use the box, and how: the owner's eyes only.
        crate::fsio::save_json(path, book, crate::fsio::PRIVATE);
    }

    pub fn list(&self) -> Vec<Person> {
        self.book.lock().expect("people mutex").people.clone()
    }

    pub fn get(&self, id: &str) -> Option<Person> {
        self.list().into_iter().find(|p| p.id == id)
    }

    /// Who the phone with this token belongs to.
    pub fn for_token(&self, token: &str) -> Option<Person> {
        let book = self.book.lock().expect("people mutex");
        let id = book.phones.get(&digest(token))?;
        book.people.iter().find(|p| &p.id == id).cloned()
    }

    pub fn add(&self, name: &str) -> Result<Person, String> {
        let name: String = name.trim().chars().take(MAX_NAME).collect();
        if name.is_empty() {
            return Err("Give them a name.".into());
        }
        let mut book = self.book.lock().expect("people mutex");
        if let Some(p) = book
            .people
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(&name))
        {
            return Ok(p.clone());
        }
        if book.people.len() >= MAX_PEOPLE {
            return Err(format!("A box keeps up to {MAX_PEOPLE} people."));
        }
        let person = Person {
            id: crate::pairing::issue_pair_id()[..10].to_string(),
            name,
            added: now(),
        };
        book.people.push(person.clone());
        self.save(&book);
        Ok(person)
    }

    /// Add or rename people from an export (`crate::export`), by id: returns
    /// (added, updated). Which devices are whose isn't part of an export, so
    /// that stays as it is here. Past [`MAX_PEOPLE`] the rest are left out.
    pub fn merge_imported(&self, incoming: Vec<Person>) -> (usize, usize) {
        let mut book = self.book.lock().expect("people mutex");
        let (mut added, mut updated) = (0, 0);
        for p in incoming {
            let name: String = p.name.trim().chars().take(MAX_NAME).collect();
            if name.is_empty() || !crate::export::safe_id(&p.id) {
                continue;
            }
            let p = Person { name, ..p };
            let len = book.people.len();
            match book.people.iter_mut().find(|x| x.id == p.id) {
                Some(cur) if *cur != p => {
                    *cur = p;
                    updated += 1;
                }
                Some(_) => {}
                None if len < MAX_PEOPLE => {
                    book.people.push(p);
                    added += 1;
                }
                None => {}
            }
        }
        if added + updated > 0 {
            self.save(&book);
        }
        (added, updated)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Person, String> {
        let name: String = name.trim().chars().take(MAX_NAME).collect();
        if name.is_empty() {
            return Err("Give them a name.".into());
        }
        let mut book = self.book.lock().expect("people mutex");
        let person = book
            .people
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("No such person.")?;
        person.name = name;
        let person = person.clone();
        self.save(&book);
        Ok(person)
    }

    pub fn remove(&self, id: &str) -> bool {
        let mut book = self.book.lock().expect("people mutex");
        let before = book.people.len();
        book.people.retain(|p| p.id != id);
        book.phones.retain(|_, p| p != id);
        let removed = book.people.len() != before;
        if removed {
            self.save(&book);
        }
        removed
    }

    /// Forget whose the device with this token digest was (it was signed out).
    pub fn forget_phone(&self, token_sha256: &str) {
        let mut book = self.book.lock().expect("people mutex");
        if book.phones.remove(token_sha256).is_some() {
            self.save(&book);
        }
    }

    /// Forget every device's person (the box was reset); the people stay.
    pub fn forget_all_phones(&self) {
        let mut book = self.book.lock().expect("people mutex");
        if !book.phones.is_empty() {
            book.phones.clear();
            self.save(&book);
        }
    }

    /// Make the phone with this token someone's (or no one's).
    pub fn assign(&self, token: &str, person: Option<&str>) -> Result<(), String> {
        self.assign_digest(&digest(token), person)
    }

    /// [`Self::assign`] for the device whose token has this digest.
    pub fn assign_digest(&self, token_sha256: &str, person: Option<&str>) -> Result<(), String> {
        let mut book = self.book.lock().expect("people mutex");
        match person {
            Some(id) => {
                if !book.people.iter().any(|p| p.id == id) {
                    return Err("No such person.".into());
                }
                book.phones.insert(token_sha256.to_string(), id.to_string());
            }
            None => {
                book.phones.remove(token_sha256);
            }
        }
        self.save(&book);
        Ok(())
    }

    /// Who the device whose token has this digest belongs to.
    pub fn person_of_digest(&self, token_sha256: &str) -> Option<Person> {
        let book = self.book.lock().expect("people mutex");
        let id = book.phones.get(token_sha256)?;
        book.people.iter().find(|p| &p.id == id).cloned()
    }

    /// The token digests of the devices that are `person`'s.
    pub fn digests_of(&self, person: &str) -> Vec<String> {
        let book = self.book.lock().expect("people mutex");
        book.phones
            .iter()
            .filter(|(_, p)| *p == person)
            .map(|(d, _)| d.clone())
            .collect()
    }
}

tokio::task_local! {
    /// Who the assistant is talking with in this request, if anyone in particular.
    pub static CALLER: Option<Person>;
}

/// The person behind the current assistant request.
pub fn current() -> Option<Person> {
    CALLER.try_with(Clone::clone).ok().flatten()
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

/// Whether the caller may make its own device `person`'s: an owner always; a
/// member only when no other live device is that person's already.
fn may_claim(state: &AppState, who: &Who, person: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let sessions = state.sessions();
    let mine = digest(&who.token);
    if sessions.session_for(&who.token).is_some_and(|s| s.owner) {
        return Ok(());
    }
    let taken = state.people().digests_of(person);
    let live = sessions.list();
    if live
        .iter()
        .any(|s| s.token_sha256 != mine && taken.contains(&s.token_sha256))
    {
        let name = state
            .people()
            .get(person)
            .map(|p| p.name)
            .unwrap_or_else(|| "That person".into());
        return Err(err(
            StatusCode::FORBIDDEN,
            format!("{name} already has a phone or computer on this box. To make this device {name}'s too, ask an owner: Settings → Pairing and security → Devices."),
        ));
    }
    Ok(())
}

fn listing(state: &AppState, me: Option<&Person>) -> Value {
    json!({ "people": state.people().list(), "me": me.map(|p| p.id.clone()) })
}

pub(crate) async fn get_people(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: Who,
) -> Json<Value> {
    Json(listing(&state, who.person.as_ref()))
}

#[derive(Deserialize)]
pub struct NewPerson {
    pub name: String,
    #[serde(default)]
    pub me: bool,
}

pub(crate) async fn post_person(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: Who,
    Json(req): Json<NewPerson>,
) -> Reply {
    let person = state
        .people()
        .add(&req.name)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let me = if req.me {
        // An existing name is that person: claiming them is as for `PUT /people/me`.
        may_claim(&state, &who, &person.id)?;
        state
            .people()
            .assign(&who.token, Some(&person.id))
            .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        Some(person.clone())
    } else {
        who.person
    };
    Ok(Json(listing(&state, me.as_ref())))
}

#[derive(Deserialize)]
pub struct MeRequest {
    pub person: Option<String>,
}

pub(crate) async fn put_me(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: Who,
    Json(req): Json<MeRequest>,
) -> Reply {
    if let Some(id) = req.person.as_deref() {
        may_claim(&state, &who, id)?;
    }
    state
        .people()
        .assign(&who.token, req.person.as_deref())
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let me = req.person.as_deref().and_then(|id| state.people().get(id));
    Ok(Json(listing(&state, me.as_ref())))
}

#[derive(Deserialize)]
pub struct Rename {
    pub name: String,
}

pub(crate) async fn patch_person(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    who: Who,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(req): Json<Rename>,
) -> Reply {
    state
        .people()
        .rename(&id, &req.name)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
    let me = who.person.and_then(|p| state.people().get(&p.id));
    Ok(Json(listing(&state, me.as_ref())))
}

pub(crate) async fn delete_person(
    axum::extract::State(state): axum::extract::State<AppState>,
    _owner: crate::routes::OwnerAuth,
    who: Who,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    let name = state.people().get(&id).map(|p| p.name).unwrap_or_default();
    if !state.people().remove(&id) {
        return Err(err(StatusCode::NOT_FOUND, "No such person."));
    }
    crate::audit::record(&state, "person_remove", &format!("{name} ({id})"), "ok");
    state.memories().forget_person(&id);
    let me = who.person.filter(|p| p.id != id);
    Ok(Json(listing(&state, me.as_ref())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_book_is_saved_owner_only_and_reloads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("people.json");
        let ana = People::open(&path).add("Ana").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(People::open(&path).get(&ana.id), Some(ana));
    }

    #[test]
    fn phones_belong_to_people_by_digest() {
        let people = People::in_memory();
        let ana = people.add("Ana").unwrap();
        assert_eq!(people.add(" ana ").unwrap(), ana, "same name, same person");
        assert!(people.add("  ").is_err());
        people.assign("token-1", Some(&ana.id)).unwrap();
        assert_eq!(people.for_token("token-1"), Some(ana.clone()));
        assert_eq!(people.for_token("token-2"), None);
        assert!(
            !people.book.lock().unwrap().phones.contains_key("token-1"),
            "tokens are stored hashed"
        );
        people.rename(&ana.id, "Ana B").unwrap();
        assert_eq!(people.for_token("token-1").unwrap().name, "Ana B");
        assert!(people.remove(&ana.id));
        assert_eq!(people.for_token("token-1"), None);
    }
}
