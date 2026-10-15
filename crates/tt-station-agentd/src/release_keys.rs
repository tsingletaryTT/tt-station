// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Release signatures: which keys the box trusts, and checking a package
//! against them before the updater ([`crate::updates`]) uses it.
//!
//! A release package `tt-station_<v>_amd64_<suite>.deb` is published with a
//! `<package>.sig` next to it: the base64 of a plain Ed25519 signature (64
//! bytes) over a three-line statement naming the file and its sha256 --
//!
//! ```text
//! tt-station-release-v1
//! file=tt-station_0.12.0_amd64_noble.deb
//! sha256=<64 lowercase hex>
//! ```
//!
//! -- each line ending in `\n`. Signing the digest rather than the file keeps
//! verification to one streamed hash (which the download already computes)
//! and makes the signature checkable with nothing but `sha256sum` and
//! `openssl pkeyutl -verify -rawin` (docs/reference/releases.md). Naming the
//! file binds the signature to the version in its name, so an old signed
//! package can't be re-published under a newer tag to slip under the
//! downgrade floor.
//!
//! Trusted keys are the ones compiled in from `deploy/release-keys.pub` plus
//! any the box's owner lists in the update channel file. Keys are raw 32-byte
//! Ed25519 public keys, base64, with a name for messages.

use base64::Engine as _;
use serde::Serialize;

/// First line of the signed statement; bump it if the statement changes.
pub const STATEMENT_TAG: &str = "tt-station-release-v1";

/// The keys every build trusts (`deploy/release-keys.pub`).
const BUILTIN: &str = include_str!("../../../deploy/release-keys.pub");

/// A public key the box accepts release signatures from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustedKey {
    pub name: String,
    /// `built in` or `channel`: where the box got it.
    pub origin: String,
    /// First 16 hex digits of the key's sha256, for logs and the app.
    pub fingerprint: String,
    #[serde(skip)]
    pub key: [u8; 32],
}

impl TrustedKey {
    /// `name` and the base64 of a 32-byte Ed25519 public key.
    pub fn new(name: &str, b64: &str, origin: &str) -> Result<Self, String> {
        let name = name.trim();
        if name.is_empty()
            || name.len() > 64
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(format!(
                "\"{name}\" isn't a usable key name (letters, digits, . _ - only)."
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .map_err(|_| format!("Key {name} isn't base64."))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| format!("Key {name} isn't a 32-byte Ed25519 public key."))?;
        let digest = ring::digest::digest(&ring::digest::SHA256, &key);
        Ok(Self {
            name: name.to_string(),
            origin: origin.to_string(),
            fingerprint: hex(&digest.as_ref()[..8]),
            key,
        })
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Keys from `<name> <base64>` lines; `#` comments and blank lines skipped.
pub fn parse_keys(text: &str, origin: &str) -> Result<Vec<TrustedKey>, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            [name, key] => TrustedKey::new(name, key, origin),
            _ => Err(format!("\"{l}\" isn't `<name> <base64 key>`.")),
        })
        .collect()
}

/// The keys compiled into this build.
pub fn builtin() -> Vec<TrustedKey> {
    parse_keys(BUILTIN, "built in").expect("deploy/release-keys.pub is malformed")
}

/// The exact bytes a release signature covers.
pub fn statement(file: &str, sha256_hex: &str) -> String {
    format!(
        "{STATEMENT_TAG}\nfile={file}\nsha256={}\n",
        sha256_hex.to_ascii_lowercase()
    )
}

/// Which trusted key signed `file` (whose sha256 is `sha256_hex`), given the
/// text of its `.sig`; or why it isn't acceptable.
pub fn verify<'k>(
    keys: &'k [TrustedKey],
    file: &str,
    sha256_hex: &str,
    sig_text: &str,
) -> Result<&'k TrustedKey, String> {
    if keys.is_empty() {
        return Err(format!(
            "This box trusts no release-signing keys yet, so {file} can't be checked and wasn't installed. An owner can add the release key to the update channel file (docs/reference/releases.md)."
        ));
    }
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_text.trim())
        .ok()
        .filter(|s| s.len() == 64)
        .ok_or_else(|| {
            format!("{file}'s signature file isn't an Ed25519 signature, so it wasn't installed.")
        })?;
    let msg = statement(file, sha256_hex);
    keys.iter()
        .find(|k| {
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &k.key)
                .verify(msg.as_bytes(), &sig)
                .is_ok()
        })
        .ok_or_else(|| {
            format!(
                "{file} isn't signed by a key this box trusts ({}), so it wasn't installed.",
                keys.iter()
                    .map(|k| format!("{} {}", k.name, k.fingerprint))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A throwaway keypair, made fresh for each test: no private key is ever
    /// committed, test or otherwise.
    fn keypair() -> Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    fn trusted(name: &str, kp: &Ed25519KeyPair) -> TrustedKey {
        let b64 = base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref());
        TrustedKey::new(name, &b64, "built in").unwrap()
    }

    fn sign(kp: &Ed25519KeyPair, file: &str, sha: &str) -> String {
        base64::engine::general_purpose::STANDARD
            .encode(kp.sign(statement(file, sha).as_bytes()).as_ref())
    }

    const FILE: &str = "tt-station_0.12.0_amd64_noble.deb";

    fn sha(fill: &str) -> String {
        fill.repeat(32)
    }

    #[test]
    fn a_valid_signature_from_a_pinned_key_is_accepted() {
        let kp = keypair();
        let keys = vec![trusted("tt-station-2026a", &kp)];
        let sig = sign(&kp, FILE, &sha("ab"));
        assert_eq!(
            verify(&keys, FILE, &sha("ab"), &sig).unwrap().name,
            "tt-station-2026a"
        );
        // Upper-case hex from a digest header is the same digest.
        assert!(verify(&keys, FILE, &sha("AB"), &format!("{sig}\n")).is_ok());
    }

    #[test]
    fn wrong_key_tampered_package_renamed_file_and_garbage_are_refused() {
        let (kp, other) = (keypair(), keypair());
        let keys = vec![trusted("tt-station-2026a", &kp)];
        let sig = sign(&kp, FILE, &sha("ab"));
        let e = verify(&keys, FILE, &sha("ab"), &sign(&other, FILE, &sha("ab"))).unwrap_err();
        assert!(e.contains("isn't signed by a key this box trusts"), "{e}");
        assert!(
            e.contains("tt-station-2026a"),
            "names the trusted keys: {e}"
        );
        // A different package (any byte changed changes its sha256).
        assert!(verify(&keys, FILE, &sha("ac"), &sig).is_err());
        // The same package published under another version's name.
        assert!(verify(&keys, "tt-station_0.13.0_amd64_noble.deb", &sha("ab"), &sig).is_err());
        for bad in ["", "not base64!", "AAAA"] {
            assert!(
                verify(&keys, FILE, &sha("ab"), bad)
                    .unwrap_err()
                    .contains("isn't an Ed25519 signature"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn no_trusted_keys_means_nothing_installs() {
        let kp = keypair();
        let e = verify(&[], FILE, &sha("ab"), &sign(&kp, FILE, &sha("ab"))).unwrap_err();
        assert!(e.contains("trusts no release-signing keys"), "{e}");
    }

    #[test]
    fn rotation_with_two_pinned_keys() {
        let (old, new) = (keypair(), keypair());
        let during = vec![
            trusted("tt-station-2026a", &old),
            trusted("tt-station-2027a", &new),
        ];
        // During the overlap both keys' releases install, and the signer is named.
        let by_old = sign(&old, FILE, &sha("ab"));
        let by_new = sign(&new, FILE, &sha("ab"));
        assert_eq!(
            verify(&during, FILE, &sha("ab"), &by_old).unwrap().name,
            "tt-station-2026a"
        );
        assert_eq!(
            verify(&during, FILE, &sha("ab"), &by_new).unwrap().name,
            "tt-station-2027a"
        );
        // After the old key is dropped (or revoked), its signatures stop counting.
        let after = vec![trusted("tt-station-2027a", &new)];
        assert!(verify(&after, FILE, &sha("ab"), &by_old).is_err());
        assert!(verify(&after, FILE, &sha("ab"), &by_new).is_ok());
    }

    #[test]
    fn key_lists_parse_and_reject_junk() {
        let kp = keypair();
        let b64 = base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref());
        let keys = parse_keys(&format!("# comment\n\n  a-1 {b64}\n"), "channel").unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].origin, "channel");
        assert_eq!(keys[0].fingerprint.len(), 16);
        assert!(
            parse_keys(&format!("{b64}\n"), "channel").is_err(),
            "no name"
        );
        assert!(parse_keys("a AAAA\n", "channel").is_err(), "too short");
        assert!(
            parse_keys(&format!("a;b {b64}\n"), "channel").is_err(),
            "bad name"
        );
        // The compiled-in list always parses (it may be empty).
        let _ = builtin();
    }

    /// The release workflow signs with `scripts/sign-release.sh` (openssl);
    /// this is a signature it made with a throwaway key whose private half
    /// was deleted, so shell signing and this check can't drift apart.
    #[test]
    fn verifies_a_signature_made_by_the_release_script() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release-sig");
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        let keys = parse_keys(&read("test-key.pub"), "built in").unwrap();
        let file = "tt-station_0.0.1_amd64_noble.deb";
        let bytes = std::fs::read(dir.join(file)).unwrap();
        let digest = hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
        assert!(verify(&keys, file, &digest, &read(&format!("{file}.sig"))).is_ok());
        let mut tampered = bytes.clone();
        tampered[0] ^= 1;
        let digest = hex(ring::digest::digest(&ring::digest::SHA256, &tampered).as_ref());
        assert!(verify(&keys, file, &digest, &read(&format!("{file}.sig"))).is_err());
    }
}
