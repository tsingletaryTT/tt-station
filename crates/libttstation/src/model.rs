use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum ServingStatus {
    Idle,
    Serving(String),
}

/// Hand-written rather than `#[derive(Serialize)]`: serde's default enum
/// encoding would emit `"Idle"` / `{"Serving":"llama3"}`, which diverges
/// from the `idle` / `serving:<model>` STRING form every HTTP route and the
/// mDNS TXT record actually use (see [`ServingStatus::to_txt`]). Anything
/// that serializes a `ServingStatus` -- directly, or nested in a
/// `BoxRecord` -- gets the canonical wire form for free instead of having to
/// re-encode it by hand (see the removed `DiscoveredBox` workaround in
/// `crates/tt/src/main.rs`, which existed only because this wasn't true).
impl Serialize for ServingStatus {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_txt())
    }
}

/// Counterpart to the `Serialize` impl above: parse the same `idle` /
/// `serving:<model>` string form via [`ServingStatus::from_txt`], so a
/// `ServingStatus` round-trips through `serde_json` byte-for-byte with the
/// txt encoding used everywhere else.
impl<'de> Deserialize<'de> for ServingStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ServingStatus::from_txt(&s).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxRecord {
    pub name: String,
    pub host: String,
    pub ctrl_port: u16,
    pub chips: String,
    pub status: ServingStatus,
    pub apiver: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub base_url: String,
    pub model: String,
    pub requires_key: bool,
}

/// One live `tt-inference-server` `/v1` serving endpoint discovered on a box,
/// regardless of who launched the container (the agent's own `/run`,
/// tt-studio's FastAPI, or a manual operator run). Populated by
/// `tt-station-agentd`'s `GET /serving` route (see
/// `serving::discovery::discover_serving`) and consumed by
/// `agent_client::list_serving` / `tt serving` / the macOS toolbar.
///
/// Shared here (rather than defined privately in the agent) so the agent, the
/// `tt` CLI, and any future GUI all decode the exact same wire shape -- same
/// reasoning as [`Endpoint`]/[`ModelInfo`] living in this crate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServingEntry {
    /// The served model id, read from the endpoint's own `/v1/models`
    /// `data[0].id` -- the authoritative id the OpenAI-compatible server
    /// reports it is actually serving (e.g. `meta-llama/Llama-3.3-70B-Instruct`).
    pub model: String,
    /// OpenAI-compatible base URL a client should point at, built from the
    /// agent's configured serving host and the container's published host
    /// port, e.g. `http://127.0.0.1:8003/v1`.
    pub base_url: String,
    /// The host port the serving container publishes its `/v1` server on.
    pub host_port: u16,
    /// The Docker container name backing this endpoint (from `docker ps`).
    pub container: String,
    /// `"agent"` when this endpoint is the one the agent itself launched (its
    /// configured serving port, and its in-memory status says it's serving
    /// this model), otherwise `"external"` (e.g. launched by tt-studio or a
    /// manual `run.py`).
    pub source: String,
}

/// `GET /serving`'s response body: every live `tt-inference-server` `/v1`
/// endpoint on the box (see [`ServingEntry`]), sorted by `host_port`. Empty
/// `serving` on a clean box (no docker, or nothing serving) -- never an error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServingList {
    pub serving: Vec<ServingEntry>,
}

/// One model a box's serving backend can run, per its `model_spec.json` --
/// the model id (the top-level key under `model_specs`) plus the device
/// meshes it supports (that entry's own keys, e.g. `GALAXY`, `T3K`,
/// `P300X2`). See `ServingBackend::list_models`
/// (`tt-station-agentd/src/serving/mod.rs`) for how this is populated and
/// `agent_client::list_models`/`tt models` for how a client consumes it --
/// the point of enumerating this at all is so a caller never has to guess
/// or hardcode which models a given box can serve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub devices: Vec<String>,
}

/// `GET /models`'s response body: every model a box can serve (see
/// `ModelInfo`) plus the `model_spec.json` release version they were read
/// from, if the backend has one to report (the default, empty
/// `ServingBackend::list_models` impl -- used by backends with no model
/// catalog of their own, e.g. `DstackBackend` -- reports `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsResponse {
    pub release_version: Option<String>,
    pub models: Vec<ModelInfo>,
}

impl ServingStatus {
    pub fn to_txt(&self) -> String {
        match self {
            ServingStatus::Idle => "idle".to_string(),
            ServingStatus::Serving(model) => format!("serving:{}", model),
        }
    }

    pub fn from_txt(s: &str) -> anyhow::Result<Self> {
        if s == "idle" {
            Ok(ServingStatus::Idle)
        } else if let Some(model) = s.strip_prefix("serving:") {
            Ok(ServingStatus::Serving(model.to_string()))
        } else {
            Err(anyhow::anyhow!("Invalid ServingStatus format: {}", s))
        }
    }
}

pub fn txt_encode(rec: &BoxRecord) -> Vec<(String, String)> {
    vec![
        ("name".to_string(), rec.name.clone()),
        ("apiver".to_string(), rec.apiver.to_string()),
        ("chips".to_string(), rec.chips.clone()),
        ("status".to_string(), rec.status.to_txt()),
        ("ctrl".to_string(), rec.ctrl_port.to_string()),
    ]
}

pub fn txt_decode(
    _name: &str,
    host: &str,
    _port: u16,
    txt: &HashMap<String, String>,
) -> anyhow::Result<BoxRecord> {
    let name_val = txt
        .get("name")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: name"))?
        .clone();

    let chips = txt
        .get("chips")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: chips"))?
        .clone();

    let status_str = txt
        .get("status")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: status"))?;
    let status = ServingStatus::from_txt(status_str)?;

    let ctrl_port = txt
        .get("ctrl")
        .ok_or_else(|| anyhow::anyhow!("Missing required key: ctrl"))?
        .parse::<u16>()?;

    let apiver: u8 = txt
        .get("apiver")
        .map(|s| s.parse::<u8>())
        .transpose()?
        .unwrap_or(1);

    Ok(BoxRecord {
        name: name_val,
        host: host.to_string(),
        ctrl_port,
        chips,
        status,
        apiver,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrips_through_txt_string() {
        assert_eq!(ServingStatus::Idle.to_txt(), "idle");
        assert_eq!(
            ServingStatus::Serving("llama3".into()).to_txt(),
            "serving:llama3"
        );
        assert_eq!(
            ServingStatus::from_txt("idle").unwrap(),
            ServingStatus::Idle
        );
        assert_eq!(
            ServingStatus::from_txt("serving:llama3").unwrap(),
            ServingStatus::Serving("llama3".into())
        );
    }

    /// `ServingStatus` must serde-round-trip through the same txt form
    /// `to_txt`/`from_txt` use, NOT serde's default derived enum shape
    /// (`"Idle"` / `{"Serving":"llama3"}`) -- see the hand-written
    /// `Serialize`/`Deserialize` impls above.
    #[test]
    fn serving_status_roundtrips_through_serde_json_txt_form() {
        for status in [
            ServingStatus::Idle,
            ServingStatus::Serving("llama3".to_string()),
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, format!("{:?}", status.to_txt())); // both are just a quoted string
            let round_tripped: ServingStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(round_tripped, status);
        }
    }

    /// Wire-compat guard: `BoxRecord` (as serialized by `tt --json discover`,
    /// decoded by the macOS app) must emit `status` as the plain
    /// `idle`/`serving:<model>` STRING, not the derived-enum shape. This is
    /// exactly the shape `DiscoveredBox` in `crates/tt/src/main.rs` used to
    /// hand-roll before `ServingStatus` grew its own `Serialize` impl.
    #[test]
    fn boxrecord_serializes_status_as_canonical_txt_string() {
        let rec = BoxRecord {
            name: "qb2-lab".to_string(),
            host: "127.0.0.1".to_string(),
            ctrl_port: 8899,
            chips: "4xBH".to_string(),
            status: ServingStatus::Serving("llama3".to_string()),
            apiver: 1,
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains(r#""status":"serving:llama3""#),
            "expected canonical txt-string status, got: {json}"
        );
    }

    #[test]
    fn txt_decode_builds_boxrecord() {
        let mut txt = std::collections::HashMap::new();
        txt.insert("name".into(), "qb2-lab".into());
        txt.insert("apiver".into(), "1".into());
        txt.insert("chips".into(), "4xBH".into());
        txt.insert("status".into(), "idle".into());
        txt.insert("ctrl".into(), "8765".into());
        let rec = txt_decode("qb2-lab", "qb2-lab.local", 8765, &txt).unwrap();
        assert_eq!(rec.name, "qb2-lab");
        assert_eq!(rec.chips, "4xBH");
        assert_eq!(rec.ctrl_port, 8765);
        assert_eq!(rec.status, ServingStatus::Idle);
    }
}
