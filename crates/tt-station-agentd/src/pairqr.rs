// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Pair a phone by scanning a QR code: `tt-station-agentd pair` (run on the box)
//! asks the running agent for a fresh pairing code and prints a QR holding a
//! `ttstation://pair?...` link. The iPhone Camera app opens TT Station, which
//! completes pairing with no typing.
//!
//! Minting a code bypasses the "read it off the box's screen" step, so it is
//! gated by a secret only the box's own user can read: the agent writes a fresh
//! one to `local-secret` (mode 0600) in its config directory at startup, and
//! `POST /pair/offer` requires it in the [`SECRET_HEADER`].
//!
//! The link also carries the box's Tailscale address when it has one, so the
//! phone can reach the box away from home.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SECRET_HEADER: &str = "x-tt-local-secret";

/// What the agent writes to `local-secret`: the secret plus how to reach it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LocalSecret {
    pub secret: String,
    pub port: u16,
    pub name: String,
}

pub fn secret_path(config_dir: &Path) -> PathBuf {
    config_dir.join("local-secret")
}

/// Write a new secret, readable only by this user.
pub fn write_secret(config_dir: &Path, port: u16, name: &str) -> std::io::Result<LocalSecret> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;

    let secret = LocalSecret {
        secret: format!(
            "{}{}",
            crate::pairing::issue_pair_id(),
            crate::pairing::issue_pair_id()
        ),
        port,
        name: name.to_string(),
    };
    std::fs::create_dir_all(config_dir)?;
    let path = secret_path(config_dir);
    let _ = std::fs::remove_file(&path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec(&secret).map_err(std::io::Error::other)?)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(secret)
}

pub fn read_secret(config_dir: &Path) -> std::io::Result<LocalSecret> {
    let bytes = std::fs::read(secret_path(config_dir))?;
    serde_json::from_slice(&bytes).map_err(std::io::Error::other)
}

/// Equal-time comparison, so the secret can't be guessed byte by byte.
pub fn secrets_match(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// The box's LAN IPv4: the source address the kernel would use to reach the
/// internet (no packet is sent).
pub fn lan_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// The box's Tailscale IPv4 (100.64.0.0/10), if Tailscale is up.
pub fn tailscale_ipv4() -> Option<Ipv4Addr> {
    let out = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<Ipv4Addr>().ok())
        .find(is_tailscale)
}

pub fn is_tailscale(ip: &Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    a == 100 && (64..128).contains(&b)
}

/// Everything the phone needs to pair.
#[derive(Debug, Clone, PartialEq)]
pub struct PairLink {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub pair_id: String,
    pub code: String,
    pub remote_host: Option<String>,
    /// HTTPS port and the certificate to pin (`crate::tls`).
    pub tls_port: Option<u16>,
    pub cert_sha256: Option<String>,
}

impl PairLink {
    pub fn url(&self) -> String {
        let mut url = format!(
            "ttstation://pair?name={}&host={}&port={}&pair={}&code={}",
            encode(&self.name),
            encode(&self.host),
            self.port,
            encode(&self.pair_id),
            encode(&self.code)
        );
        if let Some(remote) = &self.remote_host {
            url.push_str(&format!("&remote={}", encode(remote)));
        }
        if let (Some(port), Some(sha)) = (self.tls_port, &self.cert_sha256) {
            url.push_str(&format!("&tls={port}&fp={}", encode(sha)));
        }
        url
    }
}

/// Percent-encode one URL query value.
pub fn encode_component(s: &str) -> String {
    encode(s)
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The QR as terminal text: two modules per character cell (half blocks), with
/// a quiet zone, light-on-dark so it scans from a dark terminal.
pub fn render_qr(data: &str) -> Result<String, qrcode::types::QrError> {
    let code = qrcode::QrCode::with_error_correction_level(data, qrcode::EcLevel::L)?;
    let width = code.width();
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < width
            && (y as usize) < width
            && colors[y as usize * width + x as usize] == qrcode::Color::Dark
    };
    let quiet = 2isize;
    let mut out = String::new();
    let mut y = -quiet;
    while y < width as isize + quiet {
        for x in -quiet..width as isize + quiet {
            // Light modules are drawn (█), dark ones left as the dark background.
            out.push(match (!dark(x, y), !dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_every_field_encoded() {
        let link = PairLink {
            name: "tt quietbox".into(),
            host: "10.0.0.122".into(),
            port: 8765,
            pair_id: "abc".into(),
            code: "012345".into(),
            remote_host: Some("100.101.1.2".into()),
            tls_port: Some(8766),
            cert_sha256: Some("ab12".into()),
        };
        assert_eq!(
            link.url(),
            "ttstation://pair?name=tt%20quietbox&host=10.0.0.122&port=8765&pair=abc&code=012345&remote=100.101.1.2&tls=8766&fp=ab12"
        );
    }

    #[test]
    fn qr_renders_square_with_quiet_zone() {
        let qr = render_qr("ttstation://pair?host=10.0.0.1&port=8765&pair=x&code=123456").unwrap();
        let lines: Vec<&str> = qr.lines().collect();
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
        assert!(
            lines[0].chars().all(|c| c == '█'),
            "top quiet zone is light"
        );
        assert!((w / 2).abs_diff(lines.len()) <= 2, "roughly square");
    }

    #[test]
    fn secret_file_is_private_and_round_trips() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tt-pairqr-{}", std::process::id()));
        let written = write_secret(&dir, 8765, "box").unwrap();
        let mode = std::fs::metadata(secret_path(&dir))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(read_secret(&dir).unwrap(), written);
        let again = write_secret(&dir, 8765, "box").unwrap();
        assert_ne!(again.secret, written.secret, "fresh per start");
        assert!(secrets_match(&again.secret, &again.secret.clone()));
        assert!(!secrets_match(&again.secret, &written.secret));
        assert!(!secrets_match("a", "ab"));
    }

    #[test]
    fn tailscale_range() {
        assert!(is_tailscale(&"100.64.0.1".parse().unwrap()));
        assert!(is_tailscale(&"100.127.255.1".parse().unwrap()));
        assert!(!is_tailscale(&"100.128.0.1".parse().unwrap()));
        assert!(!is_tailscale(&"10.0.0.1".parse().unwrap()));
    }
}
