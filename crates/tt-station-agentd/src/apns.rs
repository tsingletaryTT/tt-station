// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Apple push notifications, sent by the box itself: the app registers its
//! device token with the agent (`POST /notify/devices`), and each alert goes to
//! Apple's push service (APNs) over HTTP/2 with a token signed by the app
//! developer's APNs key -- the same way any app's server does it.
//!
//! The key (`AuthKey_<KEYID>.p8`, from developer.apple.com → Keys) and its ids
//! live in `$TT_CONFIG_DIR/apns.toml`:
//!
//! ```toml
//! team_id = "ABCDE12345"
//! key_id = "XYZ9876543"
//! key_file = "AuthKey_XYZ9876543.p8"   # relative to the config dir
//! topic = "com.tenstorrent.ttstation.mobile"  # the app's bundle id
//! ```
//!
//! For a shipped product the key would stay on a vendor relay rather than on
//! every box; the app side is the same either way.
//!
//! What goes through Apple is generic text and an event id, never the event
//! itself (see `crate::notify::push_payload`): the app's notification service
//! extension fetches the event from the box and shows it.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use serde::Deserialize;
use serde_json::json;

const PRODUCTION: &str = "https://api.push.apple.com";
const SANDBOX: &str = "https://api.sandbox.push.apple.com";
/// Apple accepts a provider token for up to an hour and asks that it not be
/// refreshed more than every 20 minutes.
const TOKEN_LIFETIME: Duration = Duration::from_secs(45 * 60);

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ApnsConfig {
    pub team_id: String,
    pub key_id: String,
    pub key_file: String,
    pub topic: String,
    /// Tests point this at a stub instead of Apple.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// What a send to one device came to.
#[derive(Debug, PartialEq)]
pub enum Delivery {
    Sent,
    /// Apple says the token is dead (app deleted, notifications reset): forget it.
    Unregistered,
    /// The token belongs to the other APNs environment (a debug build's token
    /// sent to production, or the reverse).
    WrongEnvironment,
    Failed(String),
}

pub struct Apns {
    config: ApnsConfig,
    key: EcdsaKeyPair,
    rng: ring::rand::SystemRandom,
    client: reqwest::Client,
    token: Mutex<Option<(Instant, String)>>,
}

impl std::fmt::Debug for Apns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Apns")
            .field("team_id", &self.config.team_id)
            .field("key_id", &self.config.key_id)
            .field("topic", &self.config.topic)
            .finish()
    }
}

fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The DER inside a `.p8` (PEM "PRIVATE KEY").
pub fn pem_to_der(pem: &str) -> Result<Vec<u8>, String> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .map(str::trim)
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(body)
        .map_err(|e| format!("the key file isn't a PEM private key: {e}"))
}

impl Apns {
    /// Load `apns.toml` and its key from `config_dir`; `Ok(None)` when push
    /// isn't set up on this box.
    pub fn load(config_dir: &Path) -> Result<Option<Apns>, String> {
        let path = config_dir.join("apns.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        let config: ApnsConfig =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let key_path = config_dir.join(&config.key_file);
        let pem = std::fs::read_to_string(&key_path)
            .map_err(|e| format!("can't read {}: {e}", key_path.display()))?;
        Apns::new(config, &pem).map(Some)
    }

    pub fn new(config: ApnsConfig, pem: &str) -> Result<Apns, String> {
        let rng = ring::rand::SystemRandom::new();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pem_to_der(pem)?, &rng)
                .map_err(|e| format!("not a P-256 APNs key: {e}"))?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Apns {
            config,
            key,
            rng,
            client,
            token: Mutex::new(None),
        })
    }

    pub fn topic(&self) -> &str {
        &self.config.topic
    }

    /// The provider token: an ES256 JWT `{iss: team, iat}` with `kid` = key id,
    /// reused for [`TOKEN_LIFETIME`].
    pub fn provider_token(&self, now_unix: u64) -> Result<String, String> {
        let mut cached = self.token.lock().expect("apns mutex poisoned");
        if let Some((at, token)) = cached.as_ref() {
            if at.elapsed() < TOKEN_LIFETIME {
                return Ok(token.clone());
            }
        }
        let header = json!({ "alg": "ES256", "kid": self.config.key_id });
        let claims = json!({ "iss": self.config.team_id, "iat": now_unix });
        let signing_input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let signature = self
            .key
            .sign(&self.rng, signing_input.as_bytes())
            .map_err(|e| format!("signing failed: {e}"))?;
        let token = format!("{signing_input}.{}", b64url(signature.as_ref()));
        *cached = Some((Instant::now(), token.clone()));
        Ok(token)
    }

    /// Send one alert (`payload`, from `crate::notify::push_payload`) to one
    /// device token.
    pub async fn send(
        &self,
        device_token: &str,
        sandbox: bool,
        payload: &serde_json::Value,
    ) -> Delivery {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let token = match self.provider_token(now) {
            Ok(t) => t,
            Err(e) => return Delivery::Failed(e),
        };
        let base = self
            .config
            .endpoint
            .clone()
            .unwrap_or_else(|| if sandbox { SANDBOX } else { PRODUCTION }.to_string());
        let response = self
            .client
            .post(format!("{base}/3/device/{device_token}"))
            .bearer_auth(token)
            .header("apns-topic", &self.config.topic)
            .header("apns-push-type", "alert")
            .header("apns-priority", "10")
            .json(payload)
            .send()
            .await;
        let response = match response {
            Ok(r) => r,
            Err(e) => return Delivery::Failed(format!("couldn't reach Apple: {e}")),
        };
        let status = response.status().as_u16();
        if status == 200 {
            return Delivery::Sent;
        }
        let reason = response
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_string))
            .unwrap_or_default();
        match (status, reason.as_str()) {
            (410, _) | (400, "Unregistered") => Delivery::Unregistered,
            (400, "BadDeviceToken") => Delivery::WrongEnvironment,
            _ => Delivery::Failed(format!("Apple answered {status} {reason}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::KeyPair;

    pub fn test_key_pem() -> String {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref());
        format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
    }

    #[test]
    fn provider_token_is_a_verifiable_es256_jwt() {
        let pem = test_key_pem();
        let apns = Apns::new(
            ApnsConfig {
                team_id: "TEAM123456".into(),
                key_id: "KEY1234567".into(),
                key_file: "x".into(),
                topic: "com.example.app".into(),
                endpoint: None,
            },
            &pem,
        )
        .unwrap();
        let token = apns.provider_token(1_700_000_000).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let decode = |s: &str| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(s)
                .unwrap()
        };
        let header: serde_json::Value = serde_json::from_slice(&decode(parts[0])).unwrap();
        assert_eq!(header, json!({ "alg": "ES256", "kid": "KEY1234567" }));
        let claims: serde_json::Value = serde_json::from_slice(&decode(parts[1])).unwrap();
        assert_eq!(
            claims,
            json!({ "iss": "TEAM123456", "iat": 1_700_000_000u64 })
        );
        let public = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            apns.key.public_key().as_ref().to_vec(),
        );
        public
            .verify(
                format!("{}.{}", parts[0], parts[1]).as_bytes(),
                &decode(parts[2]),
            )
            .expect("signature verifies");
        assert_eq!(apns.provider_token(1_700_000_100).unwrap(), token, "cached");
    }

    #[test]
    fn bad_keys_and_missing_config_are_reported() {
        let dir = std::env::temp_dir().join(format!("tt-apns-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Apns::load(&dir).unwrap().is_none(), "not set up");
        std::fs::write(
            dir.join("apns.toml"),
            "team_id = \"T\"\nkey_id = \"K\"\nkey_file = \"AuthKey_K.p8\"\ntopic = \"t\"\n",
        )
        .unwrap();
        assert!(Apns::load(&dir).unwrap_err().contains("AuthKey_K.p8"));
        std::fs::write(
            dir.join("AuthKey_K.p8"),
            "-----BEGIN PRIVATE KEY-----\nnope\n",
        )
        .unwrap();
        assert!(Apns::load(&dir).is_err());
        std::fs::write(dir.join("AuthKey_K.p8"), test_key_pem()).unwrap();
        assert!(Apns::load(&dir).unwrap().is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
