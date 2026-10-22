// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! HTTPS for the control plane. The agent makes itself a certificate once
//! (`tls-cert.pem` / `tls-key.pem`, 0600, in its config dir) and serves the same
//! API over TLS on the control port + 1, beside plain HTTP.
//!
//! Trust comes from **pinning**, not a certificate authority: pairing hands the
//! phone the certificate's SHA-256 fingerprint -- in the QR code
//! (`tt-station-agentd pair`, from the box's own terminal) and in the
//! `/pair/complete` answer -- and from then on the phone talks to the box only
//! over HTTPS, accepting only that certificate. That keeps the bearer token off
//! the wire in the clear, on the LAN and beyond.

use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TlsInfo {
    /// The HTTPS port.
    pub port: u16,
    /// SHA-256 of the certificate (DER), lowercase hex: what clients pin.
    pub sha256: String,
}

/// A certificate and key, PEM.
pub struct Identity {
    pub cert_pem: String,
    pub key_pem: String,
    pub sha256: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The DER inside one PEM block.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| format!("the certificate isn't PEM: {e}"))
}

/// SHA-256 of the first certificate in `cert_pem`.
pub fn fingerprint(cert_pem: &str) -> Result<String, String> {
    let der = pem_to_der(
        cert_pem
            .split("-----END CERTIFICATE-----")
            .next()
            .unwrap_or(cert_pem),
    )?;
    Ok(hex(
        ring::digest::digest(&ring::digest::SHA256, &der).as_ref()
    ))
}

/// The box's TLS identity: loaded from `config_dir`, or made (and saved) the
/// first time, valid for `names` (host names and IP addresses).
pub fn identity(config_dir: &Path, names: Vec<String>) -> Result<Identity, String> {
    let cert_path = config_dir.join("tls-cert.pem");
    let key_path = config_dir.join("tls-key.pem");
    if let (Ok(cert_pem), Ok(key_pem)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        let sha256 = fingerprint(&cert_pem)?;
        return Ok(Identity {
            cert_pem,
            key_pem,
            sha256,
        });
    }
    let certified = rcgen::generate_simple_self_signed(names)
        .map_err(|e| format!("couldn't make a certificate: {e}"))?;
    let cert_pem = certified.cert.pem();
    let key_pem = certified.signing_key.serialize_pem();
    let write = |path: &Path, text: &str| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(config_dir)?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())
    };
    write(&cert_path, &cert_pem).map_err(|e| e.to_string())?;
    write(&key_path, &key_pem).map_err(|e| e.to_string())?;
    let sha256 = fingerprint(&cert_pem)?;
    Ok(Identity {
        cert_pem,
        key_pem,
        sha256,
    })
}

/// The names the certificate covers: localhost, the Bonjour name, and the box's
/// current LAN and Tailscale addresses.
pub fn names(box_name: &str) -> Vec<String> {
    let mut names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        format!("{box_name}.local"),
    ];
    names.extend(crate::pairqr::lan_ipv4().map(|ip| ip.to_string()));
    names
}

/// `GET /tls` (unauthed): where HTTPS is and the certificate's fingerprint, so a
/// phone paired before HTTPS existed can move to it.
pub(crate) async fn get_tls(
    axum::extract::State(state): axum::extract::State<crate::routes::AppState>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "tls": state.tls() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_once_then_reuses_the_same_certificate() {
        let dir = std::env::temp_dir().join(format!("tt-tls-{}", std::process::id()));
        let first = identity(&dir, vec!["qb2.local".into(), "10.0.0.5".into()]).unwrap();
        assert_eq!(first.sha256.len(), 64);
        assert!(first.cert_pem.contains("BEGIN CERTIFICATE"));
        let second = identity(&dir, vec!["other".into()]).unwrap();
        assert_eq!(second.sha256, first.sha256, "reused, so pins stay valid");
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join("tls-key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fingerprint(&first.cert_pem).unwrap(), first.sha256);
        let _ = std::fs::remove_dir_all(dir);
    }
}
