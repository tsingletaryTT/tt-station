// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! How the CLI reaches a box: plain HTTP only on this computer itself
//! (`127.0.0.1`/`localhost`), HTTPS with the box's pinned certificate
//! everywhere else -- the same model as the desktop app and the phone (see
//! `libttstation::transport`).
//!
//! The identity key is still the `host:port` someone typed (see main.rs). For a
//! box on the network, its [`StoredPairing`] holds the HTTPS port and the
//! certificate fingerprint `tt-station pair` saw; every connection registers
//! that pin, so a different certificate is refused before the token is sent.
//!
//! **Migration.** Pairings made before HTTPS stored only the token. The first
//! time one is used, the CLI finds the box's HTTPS (`GET /tls` on the typed port
//! or the one above it), pins the certificate it presents (trust on first use),
//! stores it with the token, and says so on stderr.

use std::future::Future;
use std::io::Write;

use anyhow::{Context, Result};
use libttstation::secrets::{SecretStore, StoredPairing};
use libttstation::transport::{self, BoxCert};

/// Where to send requests for one box, and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conn {
    /// `http://127.0.0.1:8765` or `https://qb2.local:8766`.
    pub base: String,
    /// The pairing token, when this computer has one for the box.
    pub token: Option<String>,
    /// The pinned certificate fingerprint (HTTPS only).
    pub pin: Option<String>,
}

impl Conn {
    /// The token, or the "pair first" error.
    pub fn require_token(&self, key: &str) -> Result<&str> {
        self.token.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no token stored for {key}; run `tt-station pair {key}` first")
        })
    }

    /// The same address for a WebSocket (`ws://` or `wss://`).
    pub fn ws_base(&self) -> String {
        if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else {
            format!("ws://{}", self.base.trim_start_matches("http://"))
        }
    }
}

/// Split a typed `host:port` (IPv6 as `[addr]:port`).
pub fn split_key(key: &str) -> Result<(String, u16)> {
    let (host, port) = key
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected host:port, got {key:?}"))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port in {key:?}"))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Ok((host.to_string(), port))
}

/// `https://host:port`.
pub fn https_base(host: &str, port: u16) -> String {
    format!("https://{}:{port}", transport::url_host(host))
}

/// Register `fp` for `host:tls_port` and return the HTTPS base for it.
pub fn pin_box(host: &str, tls_port: u16, fp: &str) -> String {
    transport::pin(&format!("{}:{tls_port}", transport::url_host(host)), fp);
    https_base(host, tls_port)
}

/// Resolve `key` against `store`, finding (and for an old pairing, pinning and
/// storing) the box's certificate through `probe` -- `transport::find_https`
/// in the CLI, a stand-in in tests. Notes for the person go to `notes`
/// (stderr in the CLI, so `--json` output stays clean).
pub async fn connect_with<P, F>(
    key: &str,
    store: &dyn SecretStore,
    probe: P,
    notes: &mut dyn Write,
) -> Result<Conn>
where
    P: FnOnce(String, u16) -> F,
    F: Future<Output = Result<BoxCert>>,
{
    let (host, port) = split_key(key)?;
    let stored = store.get(key)?.map(|raw| StoredPairing::decode(&raw));

    if transport::is_box_local(&host) {
        return Ok(Conn {
            base: format!("http://{}:{port}", transport::url_host(&host)),
            token: stored.map(|p| p.token),
            pin: None,
        });
    }

    match stored {
        Some(StoredPairing {
            token,
            tls_port: Some(tls_port),
            sha256: Some(fp),
        }) => Ok(Conn {
            base: pin_box(&host, tls_port, &fp),
            token: Some(token),
            pin: Some(fp),
        }),
        Some(old) => {
            // Paired before HTTPS: pin what the box presents now, once.
            let cert = probe(host.clone(), port).await.with_context(|| {
                format!("{key} was paired before TT Station used HTTPS, and its HTTPS couldn't be reached to move the pairing over")
            })?;
            let upgraded = StoredPairing {
                token: old.token,
                tls_port: Some(cert.port),
                sha256: Some(cert.sha256.clone()),
            };
            store.set(key, &upgraded.encode())?;
            let _ = writeln!(
                notes,
                "note: {key} was paired before TT Station used HTTPS. Its certificate is now \
                 pinned (trust on first use) and stored with your pairing:\n  \
                 SHA-256 {}\n  HTTPS port {}\n\
                 From now on this computer talks to it only over HTTPS and only with that \
                 certificate. If the box shows a different fingerprint (TT Station on the box: \
                 Settings → Pairing and security), run `tt-station pair {key}` again.",
                cert.sha256, cert.port
            );
            Ok(Conn {
                base: pin_box(&host, cert.port, &cert.sha256),
                token: Some(upgraded.token),
                pin: Some(cert.sha256),
            })
        }
        None => {
            // Not paired: only unauthenticated reads (status, models) get here,
            // and no secret is sent. Pin what answers for the rest of this run.
            let cert = probe(host.clone(), port).await?;
            Ok(Conn {
                base: pin_box(&host, cert.port, &cert.sha256),
                token: None,
                pin: Some(cert.sha256),
            })
        }
    }
}

/// [`connect_with`] against the real network, notes on stderr.
pub async fn connect(key: &str, store: &dyn SecretStore) -> Result<Conn> {
    connect_with(
        key,
        store,
        |host, port| async move { transport::find_https(&host, port).await },
        &mut std::io::stderr(),
    )
    .await
}

/// What `tt-station pair` (and `pair-init`/`pair-complete`) learned about the
/// box's certificate before sending anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairTarget {
    pub base: String,
    /// `None` on this computer itself (plain HTTP, nothing to pin).
    pub cert: Option<BoxCert>,
}

/// The first 16 hex digits in groups of four: what to read against the box.
pub fn short_fingerprint(sha: &str) -> String {
    sha.chars()
        .take(16)
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Before pairing: find the box's HTTPS, check its certificate against
/// `expected` (a pairing link's `fp`, or `--fp`) when given, pin it, and show
/// it. `tls_port` skips the search when the pairing link names the port.
pub async fn prepare_pair(
    key: &str,
    tls_port: Option<u16>,
    expected: Option<&str>,
    notes: &mut dyn Write,
) -> Result<PairTarget> {
    let (host, port) = split_key(key)?;
    if transport::is_box_local(&host) {
        return Ok(PairTarget {
            base: format!("http://{}:{port}", transport::url_host(&host)),
            cert: None,
        });
    }
    if let Some(fp) = expected {
        if transport::normalize_fingerprint(fp).is_none() {
            anyhow::bail!(
                "{fp:?} isn't a certificate fingerprint (64 hex digits, or the first 16 or more)"
            );
        }
    }
    let cert = match tls_port {
        Some(p) => transport::probe(&host, p).await?,
        None => transport::find_https(&host, port).await?,
    };
    if let Some(fp) = expected {
        if !transport::fingerprint_matches(fp, &cert.sha256) {
            anyhow::bail!(
                "certificate mismatch: the pairing link says the box's certificate is {}, but {key} \
                 presented {}. Nothing was sent. Make sure you're on the same network as the box, \
                 and make a new pairing link on the box.",
                transport::normalize_fingerprint(fp).unwrap_or_default(),
                cert.sha256
            );
        }
    }
    let _ = writeln!(
        notes,
        "{key}: HTTPS on port {}, certificate SHA-256\n  {}\n  ({}{})",
        cert.port,
        cert.sha256,
        short_fingerprint(&cert.sha256),
        if expected.is_some() {
            " -- matches the pairing link"
        } else {
            " -- the box shows this in TT Station → Settings → Pairing and security; check it matches"
        }
    );
    Ok(PairTarget {
        base: pin_box(&host, cert.port, &cert.sha256),
        cert: Some(cert),
    })
}

/// What to store for a finished pairing.
pub fn stored_for(token: &str, target: &PairTarget) -> StoredPairing {
    match &target.cert {
        Some(cert) => StoredPairing {
            token: token.to_string(),
            tls_port: Some(cert.port),
            sha256: Some(cert.sha256.clone()),
        },
        None => StoredPairing::token_only(token),
    }
}

/// A `ttstation://pair?...` link, as the box's QR / "Pair another computer" makes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairLink {
    pub name: Option<String>,
    pub host: String,
    pub port: u16,
    pub tls_port: u16,
    pub fp: String,
    pub pair_id: String,
    pub code: String,
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse a pairing link. Links without `tls`/`fp` come from a box too old to
/// pair securely and are refused, like the desktop app does.
pub fn parse_pair_link(link: &str) -> Result<PairLink> {
    let query = link
        .trim()
        .strip_prefix("ttstation://pair?")
        .ok_or_else(|| {
            anyhow::anyhow!("that isn't a TT Station pairing link (ttstation://pair?...)")
        })?;
    let mut get = std::collections::HashMap::new();
    for part in query.split('&') {
        if let Some((k, v)) = part.split_once('=') {
            get.insert(k.to_string(), percent_decode(v));
        }
    }
    let field = |k: &str| get.get(k).cloned().filter(|v| !v.is_empty());
    let (Some(tls), Some(fp)) = (field("tls"), field("fp")) else {
        anyhow::bail!(
            "that box's TT Station is too old to pair securely (its link has no certificate). Update it first."
        );
    };
    let host = field("host").ok_or_else(|| anyhow::anyhow!("the pairing link has no host"))?;
    let port = field("port")
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("the pairing link has no port"))?;
    let tls_port = tls
        .parse()
        .map_err(|_| anyhow::anyhow!("the pairing link's tls port isn't a number"))?;
    let fp = transport::normalize_fingerprint(&fp)
        .filter(|f| f.len() == 64)
        .ok_or_else(|| anyhow::anyhow!("the pairing link's certificate fingerprint isn't valid"))?;
    Ok(PairLink {
        name: field("name"),
        host,
        port,
        tls_port,
        fp,
        pair_id: field("pair").ok_or_else(|| anyhow::anyhow!("the pairing link has no pair id"))?,
        code: field("code").ok_or_else(|| anyhow::anyhow!("the pairing link has no code"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::secrets::FileStore;
    use std::cell::Cell;

    fn temp_store() -> (tempfile::TempDir, FileStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStore::new(dir.path().join("secrets.json"));
        (dir, store)
    }

    fn cert(port: u16, byte: &str) -> BoxCert {
        BoxCert {
            port,
            sha256: byte.repeat(32),
        }
    }

    #[tokio::test]
    async fn an_old_pairing_is_pinned_on_first_use_and_stored() {
        let (_dir, store) = temp_store();
        store.set("qb2.local:8765", "old-token").unwrap();
        let mut notes = Vec::new();
        let conn = connect_with(
            "qb2.local:8765",
            &store,
            |host, port| async move {
                assert_eq!((host.as_str(), port), ("qb2.local", 8765));
                Ok(cert(8766, "ab"))
            },
            &mut notes,
        )
        .await
        .unwrap();
        assert_eq!(conn.base, "https://qb2.local:8766");
        assert_eq!(conn.token.as_deref(), Some("old-token"));
        assert_eq!(conn.pin, Some("ab".repeat(32)));
        let stored = StoredPairing::decode(&store.get("qb2.local:8765").unwrap().unwrap());
        assert_eq!(stored.token, "old-token");
        assert_eq!(stored.tls_port, Some(8766));
        assert_eq!(stored.sha256, Some("ab".repeat(32)));
        let notes = String::from_utf8(notes).unwrap();
        assert!(notes.contains("trust on first use"), "{notes}");
        assert!(notes.contains(&"ab".repeat(32)), "{notes}");
        assert_eq!(
            transport::pinned("qb2.local:8766"),
            Some("ab".repeat(32)),
            "registered for this run's requests"
        );
    }

    #[tokio::test]
    async fn a_pinned_pairing_connects_without_looking_again() {
        let (_dir, store) = temp_store();
        let pairing = StoredPairing {
            token: "t".into(),
            tls_port: Some(9001),
            sha256: Some("cd".repeat(32)),
        };
        store.set("10.0.0.5:9000", &pairing.encode()).unwrap();
        let probed = Cell::new(false);
        let conn = connect_with(
            "10.0.0.5:9000",
            &store,
            |_, _| {
                probed.set(true);
                async { Ok(cert(1, "00")) }
            },
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert!(!probed.get());
        assert_eq!(conn.base, "https://10.0.0.5:9001");
        assert_eq!(transport::pinned("10.0.0.5:9001"), Some("cd".repeat(32)));
    }

    #[tokio::test]
    async fn this_computer_stays_on_plain_http_with_nothing_pinned() {
        let (_dir, store) = temp_store();
        store.set("127.0.0.1:8765", "local-token").unwrap();
        let conn = connect_with(
            "127.0.0.1:8765",
            &store,
            |_, _| async { panic!("no probe on the box itself") },
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(conn.base, "http://127.0.0.1:8765");
        assert_eq!(conn.token.as_deref(), Some("local-token"));
        assert_eq!(
            store.get("127.0.0.1:8765").unwrap().as_deref(),
            Some("local-token")
        );
        assert_eq!(conn.ws_base(), "ws://127.0.0.1:8765");
    }

    #[tokio::test]
    async fn an_unpaired_box_is_read_over_https_and_nothing_is_stored() {
        let (_dir, store) = temp_store();
        let conn = connect_with(
            "unpaired.local:8765",
            &store,
            |_, _| async { Ok(cert(8766, "ef")) },
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(conn.token, None);
        assert_eq!(conn.ws_base(), "wss://unpaired.local:8766");
        assert!(conn.require_token("unpaired.local:8765").is_err());
        assert_eq!(store.get("unpaired.local:8765").unwrap(), None);
    }

    #[tokio::test]
    async fn a_failed_migration_keeps_the_old_pairing() {
        let (_dir, store) = temp_store();
        store.set("qb2.local:8765", "old-token").unwrap();
        let err = connect_with(
            "qb2.local:8765",
            &store,
            |_, _| async { Err(anyhow::anyhow!("unreachable")) },
            &mut Vec::new(),
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("paired before"), "{err:#}");
        assert_eq!(
            store.get("qb2.local:8765").unwrap().as_deref(),
            Some("old-token")
        );
    }

    #[test]
    fn pairing_links_carry_the_certificate() {
        let fp = "0a".repeat(32);
        let link = format!(
            "ttstation://pair?name=tt%20quietbox&host=192.168.1.20&port=8765&pair=p-1&code=123456&tls=8766&fp={fp}"
        );
        let p = parse_pair_link(&link).unwrap();
        assert_eq!(p.name.as_deref(), Some("tt quietbox"));
        assert_eq!(
            (p.host.as_str(), p.port, p.tls_port),
            ("192.168.1.20", 8765, 8766)
        );
        assert_eq!((p.pair_id.as_str(), p.code.as_str()), ("p-1", "123456"));
        assert_eq!(p.fp, fp);

        let old = "ttstation://pair?name=x&host=h&port=8765&pair=p&code=1";
        assert!(parse_pair_link(old)
            .unwrap_err()
            .to_string()
            .contains("too old"));
        assert!(parse_pair_link("https://example.com").is_err());
    }

    #[test]
    fn keys_split_with_ipv6_brackets_dropped() {
        assert_eq!(
            split_key("qb2.local:8765").unwrap(),
            ("qb2.local".into(), 8765)
        );
        assert_eq!(
            split_key("[fe80::1]:8765").unwrap(),
            ("fe80::1".into(), 8765)
        );
        assert_eq!(https_base("fe80::1", 8766), "https://[fe80::1]:8766");
        assert!(split_key("nope").is_err());
    }

    #[test]
    fn short_fingerprints_read_in_fours() {
        assert_eq!(
            short_fingerprint(&"0123456789abcdef".repeat(4)),
            "0123 4567 89ab cdef"
        );
    }
}
