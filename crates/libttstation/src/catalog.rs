//! Compatibility-catalog types for the public Tenstorrent model catalog
//! (`compatibility.json`, ~222 models, each with per-hardware `status` and
//! `software` tags) + a pure `hw_to_mesh` mapping from catalog hardware
//! names to device-mesh labels used elsewhere in `tt-station` (e.g. to match
//! a box's detected mesh against the catalog's compatibility entries).
//!
//! This module is deliberately I/O-free: fetching/caching the catalog JSON
//! is a separate concern (a later task). Everything here is pure
//! parsing/mapping logic so it can be unit-tested without a network or
//! filesystem.
//!
//! Parsing is intentionally *tolerant*: the upstream catalog is not under
//! our control, so an unrecognized `status` string becomes
//! `CompatStatus::Other(..)` instead of a hard parse error, and optional/new
//! fields default rather than requiring every entry to be fully populated.

use serde::{Deserialize, Deserializer};

/// Top-level shape of `compatibility.json`: a flat list of models.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CompatCatalog {
    pub models: Vec<CompatModel>,
}

/// A single model entry in the catalog, along with its per-hardware
/// compatibility rows.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CompatModel {
    pub id: String,
    pub display_name: String,
    pub family: String,
    #[serde(default)]
    pub tasks: Vec<String>,
    #[serde(default)]
    pub model_size: Option<String>,
    #[serde(default)]
    pub model_size_num: Option<f64>,
    #[serde(default)]
    pub model_description: Option<String>,
    pub compatibility: Vec<HardwareCompat>,
}

/// One row of a model's per-hardware compatibility: which hardware, on what
/// chip/family, at what support status, with which software stacks.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HardwareCompat {
    pub hardware: String,
    pub chip_set: String,
    pub hardware_family: String,
    pub status: CompatStatus,
    #[serde(default)]
    pub software: Vec<String>,
}

/// Support status for a model on a given piece of hardware.
///
/// Deserializes tolerantly from the catalog's free-text status strings: any
/// string not matching a known variant is preserved verbatim in `Other` so
/// an upstream schema tweak (new status label) doesn't break the parse.
#[derive(Debug, Clone, PartialEq)]
pub enum CompatStatus {
    Supported,
    Experimental,
    NotSupported,
    Other(String),
}

impl<'de> Deserialize<'de> for CompatStatus {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "Supported" => CompatStatus::Supported,
            "Experimental" => CompatStatus::Experimental,
            "Not Supported" => CompatStatus::NotSupported,
            _ => CompatStatus::Other(s),
        })
    }
}

/// Map a catalog `hardware` string to the device-mesh label used elsewhere
/// in `tt-station` (e.g. what a box's `tt-smi`-detected mesh looks like).
///
/// Matching is case-insensitive (`hardware.to_lowercase()`); anything not in
/// the table below passes through uppercased, so new/unknown hardware names
/// from an upstream catalog update still produce a sensible (if unmapped)
/// label rather than an error.
pub fn hw_to_mesh(hardware: &str) -> String {
    match hardware.to_lowercase().as_str() {
        "n150" => "N150".to_string(),
        "n300" => "N300".to_string(),
        "p100" => "P100".to_string(),
        "p150" => "P150".to_string(),
        "p300" => "P300".to_string(),
        "galaxy" => "T3K".to_string(),
        "quietbox" => "P150X4".to_string(),
        "quietbox 2" => "P300X2".to_string(),
        "loudbox" => "P300X2".to_string(),
        "2 x quietbox" => "P150X8".to_string(),
        "2 x galaxy" => "GALAXY".to_string(),
        "4 x galaxy" => "GALAXY".to_string(),
        "quad_galaxy" => "GALAXY".to_string(),
        _ => hardware.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_map_known_and_passthrough() {
        assert_eq!(hw_to_mesh("Quietbox 2"), "P300X2");
        assert_eq!(hw_to_mesh("quietbox"), "P150X4");
        assert_eq!(hw_to_mesh("Galaxy"), "T3K");
        assert_eq!(hw_to_mesh("p150"), "P150");
        assert_eq!(hw_to_mesh("p300"), "P300");
        assert_eq!(hw_to_mesh("2 x Quietbox"), "P150X8");
        assert_eq!(hw_to_mesh("something-new"), "SOMETHING-NEW"); // passthrough uppercased
    }

    #[test]
    fn status_parses_tolerantly() {
        let j = r#"{"hardware":"p150","chip_set":"Blackhole","hardware_family":"Card","status":"Experimental","software":["tt-forge"]}"#;
        let hc: HardwareCompat = serde_json::from_str(j).unwrap();
        assert_eq!(hc.status, CompatStatus::Experimental);
        let j2 =
            r#"{"hardware":"x","chip_set":"","hardware_family":"","status":"Weird","software":[]}"#;
        let hc2: HardwareCompat = serde_json::from_str(j2).unwrap();
        assert_eq!(hc2.status, CompatStatus::Other("Weird".to_string()));
    }

    #[test]
    fn catalog_parses_full_entry() {
        let j = r#"{"models":[{"id":"qwen3-8b","display_name":"Qwen3-8B","family":"Qwen","tasks":["Text Generation"],"model_size":"8B","compatibility":[{"hardware":"Quietbox 2","chip_set":"Blackhole","hardware_family":"Quietbox","status":"Supported","software":["tt-inference-server"]}]}]}"#;
        let c: CompatCatalog = serde_json::from_str(j).unwrap();
        assert_eq!(c.models.len(), 1);
        assert_eq!(c.models[0].display_name, "Qwen3-8B");
        assert_eq!(c.models[0].compatibility[0].status, CompatStatus::Supported);
    }
}
