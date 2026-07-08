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
    /// This box's detected device-mesh label (`"p300x2"`, `"n300x4"`, ...),
    /// passed through from the agent's `/status` `device_mesh` field (Task 2
    /// -- see `tt-station-agentd::routes::StatusResponse`). Only populated
    /// when the record actually came from a live `/status` probe (today,
    /// `ManualProvider`'s manual-host path via `manual_status_fetch` in the
    /// `tt` CLI); mDNS-discovered records (`txt_decode`, below) are always
    /// `None` here because the mDNS TXT advertisement doesn't carry this key
    /// -- see `txt_decode`'s doc comment. Task 3's `tt --json discover`
    /// output surfaces whatever this field ends up holding either way.
    pub device_mesh: Option<String>,
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
    /// Whether this model's weights are already downloaded on the box (so a
    /// serve starts fast rather than triggering a large first-run download).
    /// Best-effort, detected box-side by scanning the HF cache -- see
    /// `RunPyBackend::list_models`. `#[serde(default)]` keeps the wire
    /// backward-compatible: an older agent that doesn't report this field
    /// decodes as `false` (unknown → treat as not-downloaded).
    #[serde(default)]
    pub downloaded: bool,
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

/// `GET /status`'s response body, as decoded by
/// [`crate::agent_client::get_status`] and printed by `tt --json status`
/// (Task 3). `status` is the already-parsed [`ServingStatus`] (via
/// [`ServingStatus::from_txt`]) rather than the raw `idle`/`serving:<model>`
/// string the agent sends over the wire -- `get_status` does that parsing so
/// every caller gets a `ServingStatus` for free. Because `ServingStatus` has
/// its own hand-written `Serialize` impl (the canonical txt-string form, not
/// serde's derived enum shape), this struct's derived `Serialize` still
/// round-trips correctly when a caller re-serializes it for `--json` output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub status: ServingStatus,
    /// This box's detected device-mesh label, or `None` when the agent's own
    /// detection failed/didn't run -- passed through verbatim from the
    /// agent's `/status` `device_mesh` field (Task 2). See the identically-
    /// named field on [`BoxRecord`] for the same concept surfaced via
    /// discovery instead of a direct status probe.
    pub device_mesh: Option<String>,
}

/// `GET /config`'s response body (see `tt-station-agentd::routes::get_config`,
/// Task 5): a REDACTED view of the agent's fully-resolved serving config
/// (`tt-station-agentd::config::ResolvedConfig`, Task 1-3) -- just enough for
/// the GTK panel, the `tt config` CLI (Task 6), and the Mac app to render
/// "what am I actually about to serve with," without ever exposing secrets.
/// There is deliberately no `hf_token` (or any token-store) field here: the
/// struct's shape enforces the "secrets never leave the box" constraint by
/// construction rather than by convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSummary {
    pub active_profile: Option<String>,
    pub available_profiles: Vec<String>,
    pub backend: String,
    pub serving_host: String,
    pub serving_port: u16,
    pub serving_image: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub tt_device: Option<String>, // None = auto-detected
}

/// `GET /logs`'s response body (Task 2's `tt-station-agentd::routes::LogsResponse`,
/// which this mirrors field-for-field), as decoded by
/// [`crate::agent_client::get_logs`] and printed by `tt logs` (Task 4).
/// `source` echoes back which log stream was requested (`"container"` or
/// `"run"`); `origin` is the box-side path/identifier the lines were read
/// from (e.g. a container id or log file path), or `None` when there's
/// nothing to tail yet (no container running, no log file written); `lines`
/// is the trailing-`N`-lines snapshot (or, over `/logs/stream`, the replay
/// portion) as plain strings -- no per-line structure imposed, since the
/// underlying sources (`docker logs`, `run.py`'s stdout) are themselves
/// unstructured text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogsInfo {
    pub source: String,
    pub origin: Option<String>,
    pub lines: Vec<String>,
}

/// A box's operator-facing lifecycle unit state, roughly mirroring
/// systemd's own unit-state vocabulary (the agent runs as a systemd-managed
/// service on the box) -- `Active`/`Inactive`/`Activating`/`Deactivating`/
/// `Failed`, plus `Unknown` for "couldn't determine" (e.g. the box is
/// unreachable). `tt console`'s collector reads this from the box; the TUI,
/// `tt console --snapshot` JSON, and the GTK panel all render off the same
/// values. `#[serde(rename_all = "snake_case")]` so the wire/JSON form reads
/// naturally (`"inactive"`, not `"Inactive"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceState {
    Active,
    Inactive,
    Activating,
    Deactivating,
    Failed,
    Unknown,
}

/// A live pairing code as currently shown on the box (GTK panel) or
/// reported by the agent, plus its remaining TTL -- lets `tt console`
/// surface "pair with this box" affordances without a separate round trip
/// to fetch the code and its expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingState {
    pub code: String,
    pub expires_in_secs: u64,
}

/// The full operator-facing lifecycle snapshot for one box: is the agent
/// service up, is the box reachable at all, and (when known) its identity,
/// hardware, serving status, endpoint, live `/serving` entries, redacted
/// config, and any active pairing code. This is the single shared JSON
/// contract behind `tt console` (both the interactive TUI and its
/// `--snapshot` JSON output) and the GTK box panel -- one definition here
/// means all three always agree on the wire shape, rather than each
/// hand-rolling their own view of "what state is this box in." Later tasks
/// add the collector that assembles this from the various agent routes
/// (`/status`, `/serving`, `/config`, `/pair/...`) and the parsers that
/// consume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxLifecycleSnapshot {
    pub service: ServiceState,
    pub reachable: bool,
    pub name: Option<String>,
    pub chips: Option<String>,
    pub status: Option<ServingStatus>,
    pub endpoint: Option<Endpoint>,
    pub serving: Vec<ServingEntry>,
    pub config: Option<ConfigSummary>,
    pub pairing: Option<PairingState>,
    /// Trailing lines of the current/most-recent serving log, from `GET
    /// /logs?source=container&tail=<N>` (Task 6, dogfooding Task 2's route).
    /// `#[serde(default)]` so a snapshot recorded before this field existed
    /// still deserializes (the `--snapshot` JSON is a documented contract the
    /// GTK panel consumes) -- an old/absent value degrades to `vec![]`, the
    /// same "nothing to show yet" state as a box that hasn't served anything.
    #[serde(default)]
    pub logs: Vec<String>,
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
    let mut pairs = vec![
        ("name".to_string(), rec.name.clone()),
        ("apiver".to_string(), rec.apiver.to_string()),
        ("chips".to_string(), rec.chips.clone()),
        ("status".to_string(), rec.status.to_txt()),
        ("ctrl".to_string(), rec.ctrl_port.to_string()),
    ];
    // Only emit `device_mesh` when known -- an mDNS TXT record has no
    // concept of an explicit "empty" value, so absence of the key (not an
    // empty `device_mesh=` pair) is how `None` round-trips (Task 3.5).
    if let Some(mesh) = &rec.device_mesh {
        pairs.push(("device_mesh".to_string(), mesh.clone()));
    }
    pairs
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
        // `device_mesh` is optional in the TXT map (Task 3.5) -- older
        // agents, or advertisers that never learned their mesh, simply omit
        // the key, and that must decode to `None` rather than erroring.
        device_mesh: txt.get("device_mesh").cloned(),
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
            device_mesh: Some("p300x2".to_string()),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains(r#""status":"serving:llama3""#),
            "expected canonical txt-string status, got: {json}"
        );
    }

    /// `ConfigSummary` (the `GET /config` response body -- see Task 5) is a
    /// REDACTED view of the agent's resolved serving config: it must
    /// round-trip through serde_json byte-for-byte, and by construction can
    /// never carry `hf_token` or any other secret (there's no field for one).
    #[test]
    fn config_summary_round_trips_and_omits_secrets() {
        let s = ConfigSummary {
            active_profile: Some("stable".into()),
            available_profiles: vec!["stable".into(), "bleeding".into()],
            backend: "runpy".into(),
            serving_host: "qb2-lab.local".into(),
            serving_port: 8003,
            serving_image: Some("img:0.14.0".into()),
            tt_inference_repo: Some("/home/x/code/tt-inference-server".into()),
            tt_device: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hf_token"), "summary must not carry secrets");
        let back: ConfigSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
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
        // Back-compat: a TXT map WITHOUT a `device_mesh` key (older agents,
        // or advertisers that never learned their mesh) must still decode
        // cleanly to `None` rather than erroring or inventing a value.
        assert_eq!(rec.device_mesh, None);
    }

    /// `txt_encode` must emit a `device_mesh` pair when the record has one,
    /// so mDNS-discovered boxes carry the same hardware-aware signal as a
    /// direct `/status` probe (Task 3.5).
    #[test]
    fn txt_encode_includes_device_mesh_when_some() {
        let rec = BoxRecord {
            name: "qb2-lab".to_string(),
            host: "qb2-lab.local".to_string(),
            ctrl_port: 8765,
            chips: "4xBH".to_string(),
            status: ServingStatus::Idle,
            apiver: 1,
            device_mesh: Some("p300x2".to_string()),
        };
        let pairs = txt_encode(&rec);
        assert!(
            pairs.contains(&("device_mesh".to_string(), "p300x2".to_string())),
            "expected a device_mesh pair, got: {pairs:?}"
        );
    }

    /// `txt_encode` must NOT emit an empty `device_mesh` pair when the
    /// record has none -- absence, not `device_mesh=`, is how "unknown"
    /// round-trips through mDNS TXT.
    #[test]
    fn txt_encode_omits_device_mesh_when_none() {
        let rec = BoxRecord {
            name: "qb2-lab".to_string(),
            host: "qb2-lab.local".to_string(),
            ctrl_port: 8765,
            chips: "4xBH".to_string(),
            status: ServingStatus::Idle,
            apiver: 1,
            device_mesh: None,
        };
        let pairs = txt_encode(&rec);
        assert!(
            !pairs.iter().any(|(k, _)| k == "device_mesh"),
            "expected no device_mesh pair, got: {pairs:?}"
        );
    }

    /// `txt_decode` must read a present `device_mesh` key back into `Some`.
    #[test]
    fn txt_decode_reads_device_mesh_when_present() {
        let mut txt = std::collections::HashMap::new();
        txt.insert("name".into(), "qb2-lab".into());
        txt.insert("apiver".into(), "1".into());
        txt.insert("chips".into(), "4xBH".into());
        txt.insert("status".into(), "idle".into());
        txt.insert("ctrl".into(), "8765".into());
        txt.insert("device_mesh".into(), "p300x2".into());
        let rec = txt_decode("qb2-lab", "qb2-lab.local", 8765, &txt).unwrap();
        assert_eq!(rec.device_mesh, Some("p300x2".to_string()));
    }

    /// `BoxLifecycleSnapshot` is the one shared JSON contract for a box's
    /// operator-facing lifecycle state -- `tt console` (TUI + `--snapshot`
    /// JSON) and the GTK box panel both decode/encode this exact shape
    /// (Task 2). Round-trip through serde_json must be lossless, and
    /// `ServiceState` (mirroring systemd-ish unit states) must serialize as
    /// snake_case so it reads naturally in JSON output (`"inactive"`, not
    /// `"Inactive"`).
    #[test]
    fn lifecycle_snapshot_round_trips() {
        let s = BoxLifecycleSnapshot {
            service: ServiceState::Active,
            reachable: true,
            name: Some("qb2-lab".into()),
            chips: Some("4xBH".into()),
            status: None,
            endpoint: None,
            serving: vec![],
            config: None,
            pairing: Some(PairingState {
                code: "042817".into(),
                expires_in_secs: 107,
            }),
            logs: vec!["line one".into(), "line two".into()],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: BoxLifecycleSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        // ServiceState serializes snake_case
        assert!(serde_json::to_string(&ServiceState::Inactive)
            .unwrap()
            .contains("inactive"));
    }

    /// `#[serde(default)]` on `logs` is REQUIRED, not decorative: a
    /// `--snapshot` recorded before Task 6 (no `logs` key at all) must still
    /// deserialize -- the GTK panel polls this JSON as a documented contract
    /// and must not break against a slightly-older `tt console` binary.
    #[test]
    fn lifecycle_snapshot_deserializes_without_logs_field() {
        let json = r#"{
            "service": "active",
            "reachable": true,
            "name": null,
            "chips": null,
            "status": null,
            "endpoint": null,
            "serving": [],
            "config": null,
            "pairing": null
        }"#;
        let snap: BoxLifecycleSnapshot =
            serde_json::from_str(json).expect("must deserialize without a `logs` key");
        assert!(snap.logs.is_empty());
    }
}
