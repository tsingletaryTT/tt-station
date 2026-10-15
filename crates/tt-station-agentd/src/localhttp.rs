// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! One HTTP client for the box's own servers (the model, embeddings, metrics).
//!
//! `reqwest::Client::new()` builds a new connection pool and TLS setup each
//! time, and the request it makes opens a new connection; Code alone makes one
//! model request per step. Sharing one client keeps connections to the model
//! alive between requests. Each caller still sets its own `.timeout(..)`.

use std::sync::OnceLock;
use std::time::Duration;

/// The shared client: connect within 10 s; a streamed answer may pause up to
/// 15 min between chunks (a long prefill on a big context).
pub fn local() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(900))
            .build()
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_caller_gets_the_same_client() {
        assert!(std::ptr::eq(super::local(), super::local()));
    }
}
