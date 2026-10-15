// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! How a client reaches a box: HTTPS with the box's own certificate pinned.
//!
//! A box serves its control plane over plain HTTP only to itself; every other
//! computer has to use HTTPS on the TLS port (the control port + 1, 8766 by
//! default; mDNS TXT `tls`). The box's certificate is self-signed, so trust
//! comes from **pinning**, not a certificate authority: the client learns the
//! certificate's SHA-256 when it pairs (from `GET /tls`, or the pairing link's
//! `fp`), stores it with the token, and from then on accepts only that
//! certificate. It's the same model as the desktop app (`desktop/electron/
//! main.cjs`: `certSha256`, `pinnedBox`, `trustPinnedBoxes`) and the phone.
//!
//! Pins are process-wide, keyed by `host:port` -- like Electron's
//! `setCertificateVerifyProc`, which is per session -- so every request this
//! crate makes ([`client_for`]) checks the pin without each call site having to
//! carry it. The `tt-station` CLI registers one pin per run (see
//! `crates/tt-station/src/remote.rs`).
//!
//! The verifier still checks the handshake signature against the presented
//! certificate (so the server must hold the pinned certificate's key); only
//! the CA-chain and name checks are replaced by the fingerprint comparison.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, OtherError, SignatureScheme};

/// SHA-256 of DER bytes, lowercase hex: how a box states its fingerprint.
pub fn cert_sha256(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fingerprint as people may paste it (`AB:CD:..`, spaces, upper case) as
/// lowercase hex, or `None` when it isn't hex at all. Accepts a prefix (at
/// least 16 digits, what the box's screen shows) -- callers decide whether a
/// prefix is enough ([`fingerprint_matches`]).
pub fn normalize_fingerprint(raw: &str) -> Option<String> {
    let hex: String = raw
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let ok = (16..=64).contains(&hex.len()) && hex.chars().all(|c| c.is_ascii_hexdigit());
    ok.then_some(hex)
}

/// Whether `expected` (a full fingerprint, or the first 16+ hex digits of one,
/// as the box shows them) names the certificate `actual` (full, lowercase).
pub fn fingerprint_matches(expected: &str, actual: &str) -> bool {
    normalize_fingerprint(expected).is_some_and(|e| actual.starts_with(&e))
}

/// The address is this computer itself: the only place plain HTTP is used.
/// Deliberately exact (`127.0.0.1`, `localhost`, `::1`): anything else is
/// reached over HTTPS, even other loopback addresses.
pub fn is_box_local(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    matches!(
        h.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// `host` as it goes in a URL (IPv6 literals bracketed).
pub fn url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// The box presented a certificate other than the one this computer pinned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateChanged {
    /// `host:port` that was dialled.
    pub authority: String,
    pub expected: String,
    pub presented: String,
}

impl std::fmt::Display for CertificateChanged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "certificate changed: {} presented a certificate with SHA-256 {}, but this computer \
             paired with {}. Nothing was sent. Either the box was reinstalled (its certificate \
             is made once and kept) or something on the network is answering for it. If you're \
             sure it's your box, check the fingerprint on the box (Settings → Pairing and \
             security) and pair again.",
            self.authority, self.presented, self.expected
        )
    }
}

impl std::error::Error for CertificateChanged {}

/// Pinned fingerprints by `host:port`, for [`client_for`].
static PINS: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
/// The last pin mismatch, so [`explain`] can say so plainly: by the time it
/// reaches a caller, the rustls error is buried inside reqwest's.
static LAST_MISMATCH: Mutex<Option<CertificateChanged>> = Mutex::new(None);

/// Pin `sha256` for HTTPS connections to `authority` (`host:port`) made
/// through [`client_for`] in this process.
pub fn pin(authority: &str, sha256: &str) {
    PINS.lock()
        .expect("pins")
        .insert(authority.to_ascii_lowercase(), sha256.to_ascii_lowercase());
}

/// The fingerprint pinned for `authority`, if any.
pub fn pinned(authority: &str) -> Option<String> {
    PINS.lock()
        .expect("pins")
        .get(&authority.to_ascii_lowercase())
        .cloned()
}

/// `host:port` of an `https://` or `wss://` URL (`None` for anything else).
fn https_authority(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("wss://"))?;
    Some(rest.split(['/', '?', '#']).next().unwrap_or(rest))
}

/// The provider every config here uses: ring, like the agent and reqwest.
fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct PinVerifier {
    authority: String,
    /// `None`: accept any certificate, only recording it (a first look, before
    /// anything is pinned or any secret is sent).
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let presented = cert_sha256(end_entity.as_ref());
        *self.seen.lock().expect("seen") = Some(presented.clone());
        match &self.expected {
            Some(expected) if *expected != presented => {
                let changed = CertificateChanged {
                    authority: self.authority.clone(),
                    expected: expected.clone(),
                    presented,
                };
                *LAST_MISMATCH.lock().expect("mismatch") = Some(changed.clone());
                Err(rustls::Error::InvalidCertificate(CertificateError::Other(
                    OtherError(Arc::new(changed)),
                )))
            }
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// A rustls client config for `authority` that accepts only the certificate
/// whose SHA-256 is `expected` -- or, with `expected: None`, any certificate
/// (only to read what it is: see [`probe`]). The returned slot holds the
/// fingerprint of the certificate the server last presented.
pub fn tls_config(
    authority: &str,
    expected: Option<&str>,
) -> (ClientConfig, Arc<Mutex<Option<String>>>) {
    let provider = provider();
    let seen = Arc::new(Mutex::new(None));
    let verifier = PinVerifier {
        authority: authority.to_string(),
        expected: expected.map(str::to_ascii_lowercase),
        seen: seen.clone(),
        provider: provider.clone(),
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    (config, seen)
}

/// The HTTP client for a request to `url`: pinned when `url` is HTTPS to an
/// authority with a pin ([`pin`]); the ordinary client otherwise (plain HTTP
/// on this computer, or HTTPS to anything else, checked the usual way).
pub fn client_for(url: &str) -> reqwest::Client {
    let pinned = https_authority(url).and_then(|a| pinned(a).map(|fp| (a.to_string(), fp)));
    match pinned {
        Some((authority, fp)) => {
            let (config, _) = tls_config(&authority, Some(&fp));
            reqwest::Client::builder()
                .use_preconfigured_tls(config)
                .build()
                .unwrap_or_default()
        }
        None => reqwest::Client::new(),
    }
}

/// Take the most recent pin mismatch seen in this process, if any.
pub fn take_mismatch() -> Option<CertificateChanged> {
    LAST_MISMATCH.lock().expect("mismatch").take()
}

/// `err`, or -- when a connection failed because a box's certificate changed
/// -- that, said plainly. By the time a pin mismatch reaches a caller it is
/// buried inside reqwest's "error sending request", so the verifier records it
/// and this swaps it in.
pub fn explain(err: anyhow::Error) -> anyhow::Error {
    match take_mismatch() {
        Some(changed) => anyhow::Error::new(changed),
        None => err,
    }
}

/// What a box says about its HTTPS, checked against what it presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxCert {
    /// The HTTPS port that answered.
    pub port: u16,
    /// SHA-256 of the certificate the box presented (lowercase hex).
    pub sha256: String,
}

/// Connect to `https://host:port/tls`, accepting whatever certificate is
/// presented (nothing secret is sent), and return its fingerprint. The box's
/// own answer must agree with the certificate it presented -- if it names a
/// different one, something else is answering for it.
pub async fn probe(host: &str, port: u16) -> anyhow::Result<BoxCert> {
    let authority = format!("{}:{port}", url_host(host));
    let (config, seen) = tls_config(&authority, None);
    let client = reqwest::Client::builder()
        .use_preconfigured_tls(config)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!("https://{authority}/tls");
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("request to {url} failed: {e}"))?
        .json()
        .await?;
    let presented = seen
        .lock()
        .expect("seen")
        .clone()
        .ok_or_else(|| anyhow::anyhow!("{authority} didn't present a certificate"))?;
    if let Some(reported) = body["tls"]["sha256"].as_str() {
        if !reported.eq_ignore_ascii_case(&presented) {
            anyhow::bail!(
                "{authority} presented a certificate with SHA-256 {presented} but says its own is \
                 {reported}: something on the network is answering for the box. Not pairing."
            );
        }
    }
    Ok(BoxCert {
        port,
        sha256: presented,
    })
}

/// Find a box's HTTPS from the address someone typed: `port` itself (they gave
/// the TLS port), else `port + 1` (they gave the control port; HTTPS is the
/// next one up). See [`probe`].
pub async fn find_https(host: &str, port: u16) -> anyhow::Result<BoxCert> {
    let first = match probe(host, port).await {
        Ok(cert) => return Ok(cert),
        Err(e) => e,
    };
    let Some(next) = port.checked_add(1) else {
        return Err(first);
    };
    probe(host, next).await.map_err(|second| {
        anyhow::anyhow!(
            "couldn't reach {host} over HTTPS on port {port} ({first}) or {next} ({second}). \
             Is TT Station running there, and up to date?"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_normalize_and_match_by_prefix() {
        let full = "ab".repeat(32);
        assert_eq!(
            normalize_fingerprint(&"AB:".repeat(32)[..95]),
            Some(full.clone())
        );
        assert!(fingerprint_matches(&full, &full));
        assert!(fingerprint_matches("abababababababab", &full));
        assert!(
            !fingerprint_matches("abab", &full),
            "too short to mean anything"
        );
        assert!(!fingerprint_matches("cdababababababab", &full));
        assert!(!fingerprint_matches("not hex at all!!!", &full));
    }

    #[test]
    fn only_this_computer_uses_plain_http() {
        for h in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "[::1]"] {
            assert!(is_box_local(h), "{h}");
        }
        for h in [
            "127.0.0.2",
            "qb2.local",
            "192.168.1.20",
            "fe80::1",
            "localhost.evil",
        ] {
            assert!(!is_box_local(h), "{h}");
        }
    }

    #[test]
    fn pins_are_looked_up_by_https_authority() {
        pin("Pinned-Box.local:8766", "AA");
        assert_eq!(pinned("pinned-box.local:8766").as_deref(), Some("aa"));
        assert_eq!(
            https_authority("https://pinned-box.local:8766/status?x=1"),
            Some("pinned-box.local:8766")
        );
        assert_eq!(https_authority("wss://h:1/logs"), Some("h:1"));
        assert_eq!(https_authority("http://h:1/status"), None);
    }

    #[test]
    fn ipv6_hosts_are_bracketed_once() {
        assert_eq!(url_host("fe80::1"), "[fe80::1]");
        assert_eq!(url_host("[fe80::1]"), "[fe80::1]");
        assert_eq!(url_host("qb2.local"), "qb2.local");
    }
}
