// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Paired devices and their tokens: checking a bearer token, closing a
//! socket when its device signs out, and the pending pair codes behind
//! `/pair/init` and `/pair/complete`.

use super::*;

impl AppState {
    /// Check a bearer token against the paired devices, counting it as a use
    /// of that device when it's good (see `crate::sessions`): which device the
    /// token belongs to, or why it was refused.
    pub(crate) fn authenticate(&self, token: &str) -> Result<String, crate::sessions::AuthFailure> {
        self.inner.sessions.authenticate(token)
    }

    /// The paired devices.
    pub fn sessions(&self) -> Arc<crate::sessions::Sessions> {
        Arc::clone(&self.inner.sessions)
    }

    /// Resolves once `token` stops being good -- its device was removed or
    /// the box was reset -- so a long-lived WebSocket can close itself rather
    /// than outlive the pairing that opened it.
    pub(crate) fn signed_out(
        &self,
        token: String,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let sessions = self.sessions();
        let mut changes = sessions.revocations();
        async move {
            loop {
                if !sessions.is_live(&token) {
                    return;
                }
                if changes.changed().await.is_err() {
                    // The state is gone (shutdown): nothing more will change.
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// [`Self::signed_out`] for a caller known by its device id (`None`, the
    /// box itself, is never signed out).
    pub(crate) fn device_signed_out(
        &self,
        id: Option<String>,
    ) -> impl std::future::Future<Output = ()> + Send + 'static {
        let sessions = self.sessions();
        let mut changes = sessions.revocations();
        async move {
            let Some(id) = id else {
                return std::future::pending::<()>().await;
            };
            loop {
                if sessions.get(&id).is_none() {
                    return;
                }
                if changes.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        }
    }

    /// Record a freshly-issued pairing attempt: `pair_id` will be accepted
    /// by `complete_pair` if presented with the matching `code` before
    /// `PAIR_TTL` elapses.
    ///
    /// Before inserting, sweeps out any *other* pending entries whose expiry
    /// has already passed. `complete_pair` only ever removes the one
    /// `pair_id` it was asked about, so a client that repeatedly hits
    /// `/pair/init` and never follows up with `/pair/complete` would
    /// otherwise grow `pending_pairs` unbounded -- a cheap, unauthenticated
    /// way to slowly exhaust memory. Sweeping here (rather than on a
    /// timer/background task) keeps the fix O(n) in the number of currently
    /// pending pairs and needs no extra moving parts: every `/pair/init`
    /// call is already a natural point to tidy up.
    /// Whether one more `/pair/init` may go ahead now from `peer`, counting
    /// it if so. Each address gets a few a minute, so one asker can't use up
    /// pairing for everyone; a ceiling across all addresses keeps the
    /// owner's notifications bounded however many addresses ask.
    pub(crate) fn pair_init_allowed(&self, peer: Option<std::net::IpAddr>, now: u64) -> bool {
        let mut recent = self.inner.pair_inits.lock().expect("pair inits");
        recent.retain(|(t, _)| now.saturating_sub(*t) < 60);
        if recent.len() >= PAIR_INITS_PER_MINUTE
            || recent.iter().filter(|(_, p)| *p == peer).count() >= PAIR_INITS_PER_ADDRESS
        {
            return false;
        }
        recent.push((now, peer));
        true
    }

    pub(crate) fn insert_pending_pair(&self, pair_id: String, code: String) {
        self.insert_pending_pair_as(pair_id, code, false);
    }

    /// [`Self::insert_pending_pair`]; `owner` makes the device that completes
    /// it an owner (`/pair/offer`, which takes the local secret).
    pub(crate) fn insert_pending_pair_as(&self, pair_id: String, code: String, owner: bool) {
        let now = Instant::now();
        let expiry = now + PAIR_TTL;
        let mut pending = self
            .inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned");
        pending.retain(|_, (_, exp, _, _)| *exp > now);
        pending.insert(pair_id, (code, expiry, 0, owner));
    }

    /// Check `code` against the pending pairing attempt for `pair_id`.
    ///
    /// Succeeds (`Some`) only when `pair_id` is known, unexpired, and `code`
    /// matches -- in which case the pending entry is consumed (removed) so
    /// the same code can't be replayed. It is `None` for an unknown
    /// pair_id or an expired one (also removed, since it can never succeed
    /// again).
    ///
    /// A code mismatch bumps that pair_id's wrong-attempt counter instead of
    /// leaving it untouched forever: once it reaches `MAX_PAIR_ATTEMPTS` the
    /// entry is removed too, so a LAN client can't keep guessing values from
    /// the 6-digit code space against the same pair_id. Below the cap the
    /// entry is left in place (with the bumped counter) so a human who
    /// mistyped the code still gets a few more real chances before the TTL
    /// runs out.
    ///
    /// `Some(owner)` on success: whether it was offered with the local secret.
    pub(crate) fn complete_pair_as(&self, pair_id: &str, code: &str) -> Option<bool> {
        let mut pending = self
            .inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned");

        // Clone the current entry out (rather than holding a borrow into
        // `pending`) so the branches below are free to call
        // `pending.insert`/`pending.remove` without fighting the borrow
        // checker over a mutex we're already holding.
        let (stored_code, expiry, attempts, owner) = pending.get(pair_id).cloned()?;

        if Instant::now() >= expiry {
            pending.remove(pair_id);
            return None;
        }

        if stored_code == code {
            pending.remove(pair_id);
            return Some(owner);
        }

        let attempts = attempts + 1;
        if attempts >= MAX_PAIR_ATTEMPTS {
            pending.remove(pair_id);
        } else {
            pending.insert(pair_id.to_string(), (stored_code, expiry, attempts, owner));
        }
        None
    }

    /// Sign every paired device out (`POST /reset`): their tokens stop
    /// working at once, open terminals close, and the token store is
    /// deleted, so a restart comes up never-paired too.
    pub(crate) fn clear_tokens(&self) {
        let count = self.inner.sessions.revoke_all();
        self.people().forget_all_phones();
        self.notifier().record(crate::notify::Draft {
            kind: "devices",
            title: "Every device was signed out".to_string(),
            body: format!(
                "The box was reset, so {count} paired device{} must pair again.",
                if count == 1 { "" } else { "s" }
            ),
        });
    }

    /// Test-only accessor: look up the code currently pending for
    /// `pair_id`, i.e. what a human would be reading off the box's screen
    /// right now.
    ///
    /// Compiled in only under `cfg(test)` (this crate's own unit tests) or
    /// the `test-hooks` feature (integration tests -- see the
    /// self-dependency in `Cargo.toml` for how that feature gets turned on
    /// for `cargo test`). A production build has neither, so a real agent
    /// never exposes pairing codes through any API but the log line
    /// `pair_init` prints -- exactly where a human reading the box's screen
    /// would look.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn last_code(&self, pair_id: &str) -> Option<String> {
        self.inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned")
            .get(pair_id)
            .map(|(code, _, _, _)| code.clone())
    }

    /// Test-only seam: insert a pending pair whose expiry is already in the
    /// past, so tests can exercise `complete_pair`'s TTL-expiry branch
    /// without actually sleeping for `PAIR_TTL` (120s).
    ///
    /// Gated the same way as `last_code` -- compiled in only for this
    /// crate's own unit tests or the `test-hooks` feature (integration
    /// tests), never for a normal build.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn insert_expired_pair(&self, pair_id: &str, code: &str) {
        let already_expired = Instant::now() - Duration::from_secs(1);
        self.inner
            .pending_pairs
            .lock()
            .expect("pending_pairs mutex poisoned")
            .insert(
                pair_id.to_string(),
                (code.to_string(), already_expired, 0, false),
            );
    }
}
