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

use serde::{Deserialize, Deserializer, Serialize};

use crate::model::ModelInfo;

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

/// True if a box whose detected mesh is `box_mesh` can run a model whose
/// catalog hardware maps to `catalog_mesh` (both already `hw_to_mesh`-mapped
/// labels, e.g. `"p300x2"` / `"P150"`). Case-insensitive.
///
/// An exact match is always compatible. Beyond that, a multi-card box mesh
/// (e.g. `"p150x2"`, detected by the agent for a 2-card P150 box) is also
/// compatible with its single-card family (`"P150"`) -- a model that runs on
/// one card of a family runs on a box that has several of that family. This
/// does NOT upgrade the other direction: a bare single-card mesh (`"p150"`)
/// is NOT compatible with a multi-card, box-level catalog requirement like
/// `"P150X4"` -- one card can't satisfy a four-card requirement.
pub fn mesh_compatible(box_mesh: &str, catalog_mesh: &str) -> bool {
    let box_mesh = box_mesh.to_lowercase();
    let catalog_mesh = catalog_mesh.to_lowercase();
    if box_mesh == catalog_mesh {
        return true;
    }
    // Strip a trailing "x<digits>" card-count suffix (e.g. "p150x2" ->
    // "p150") to get the box's single-card family base. A mesh with no such
    // suffix (e.g. a bare "p150") is already its own base.
    let box_base = match box_mesh.rfind('x') {
        Some(idx)
            if box_mesh[idx + 1..].chars().all(|c| c.is_ascii_digit())
                && idx + 1 < box_mesh.len() =>
        {
            &box_mesh[..idx]
        }
        _ => box_mesh.as_str(),
    };
    catalog_mesh == box_base
}

/// True if any entry in `software` names tt-inference-server (the engine
/// the box actually serves via run.py). Tolerant: lowercased, `_`→`-`,
/// matches "tt-inference-server" or any value containing "inference-server".
pub fn software_is_tis(software: &[String]) -> bool {
    software.iter().any(|s| {
        let f = s.to_lowercase().replace('_', "-");
        f == "tt-inference-server" || f.contains("inference-server")
    })
}

/// A single row in the merged, box-aware model catalog (see [`BoxCatalog`]).
/// This is the WIRE contract for `tt catalog`'s JSON output and what the
/// macOS app's model picker decodes -- it is deliberately flatter than
/// [`CompatModel`]/[`HardwareCompat`] (no per-hardware software split, no
/// `Other` status variant) because by the time a [`CatalogEntry`] exists,
/// [`classify`] has already resolved "does/doesn't this run on *this* box"
/// down to a handful of plain fields a UI can render without re-deriving
/// any catalog logic itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub display_name: String,
    pub family: String,
    pub size: Option<String>,
    pub software: Vec<String>,
    pub meshes: Vec<String>,
    pub needed_hardware: Vec<String>,
    pub available_now: bool,
    pub status_here: String,
}

/// `classify`'s full output: the compatibility catalog and a box's live
/// `/models` merged into three tiers relative to that box's detected mesh
/// (see [`classify`] for the merge rules). This is what `tt catalog`
/// prints and what the macOS app's model picker renders directly -- one
/// shared shape so both agree on what "runs here" / "experimental" /
/// "needs other hardware" mean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxCatalog {
    pub box_mesh: Option<String>,
    pub catalog_available: bool,
    pub catalog_stale: bool,
    pub runs_here: Vec<CatalogEntry>,
    pub experimental: Vec<CatalogEntry>,
    pub other_hardware: Vec<CatalogEntry>,
}

/// Normalize a model identifier (a catalog `id`/`display_name`, or a live
/// `ModelInfo::name`) into a comparison key so `classify` can match "the
/// same model" across the catalog and a box's live `/models` list even
/// though the two sources spell it differently -- e.g. the catalog's
/// `display_name: "Qwen3-8B"` vs. a live HF-style repo id
/// `"Qwen/Qwen3-8B"`. Steps: lowercase, keep only the substring after the
/// last `/` (drop any org/namespace prefix), fold `.`/`_`/` ` to `-`, then
/// collapse runs of `-` down to one (so `"bge_large en.v1.5"` and a
/// hypothetical `"bge-large--en.v1.5"` both key the same).
pub fn normalize_key(s: &str) -> String {
    let lower = s.to_lowercase();
    let after_slash = lower.rsplit('/').next().unwrap_or(&lower);
    let folded: String = after_slash
        .chars()
        .map(|c| match c {
            '.' | '_' | ' ' => '-',
            other => other,
        })
        .collect();
    let mut collapsed = String::with_capacity(folded.len());
    let mut prev_dash = false;
    for c in folded.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push(c);
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    collapsed
}

/// Build a [`CatalogEntry`] for a live-only model (no catalog match) --
/// used both when there is no catalog at all and when a live `/models`
/// entry doesn't match any catalog model by [`normalize_key`]. A live
/// model is, by definition, something the box can run right now, so it
/// always lands in `runs_here` with `available_now: true`.
///
/// `family` has no catalog data to draw on here, so it's just the model's
/// own name -- there's a fancier family-name split
/// (`ModelDefaults.familyName`) on the macOS/Swift side, but that lives in
/// a different (Swift) codebase entirely, and duplicating a heuristic
/// across a language boundary for a cosmetic-only field isn't worth the
/// coupling. Callers that want the nicer split already have `id` to
/// re-derive it themselves.
fn live_only_entry(m: &ModelInfo) -> CatalogEntry {
    CatalogEntry {
        id: m.name.clone(),
        display_name: m.name.clone(),
        family: m.name.clone(),
        size: None,
        software: Vec::new(),
        meshes: Vec::new(),
        needed_hardware: Vec::new(),
        available_now: true,
        status_here: "supported".to_string(),
    }
}

/// True if a catalog entry is the "clean" representation of a model: a real
/// family (not the catch-all `"Other"`) and no `org/` prefix in its `id`. The
/// upstream catalog ships two rows per model — a clean one
/// (`id: "gpt-oss-120b"`, `family: "GPT-OSS"`) and an org-prefixed twin
/// (`id: "openai/gpt-oss-120b"`, `family: "Other"`) — and when we collapse the
/// twins we keep the clean one's display metadata.
fn is_clean_entry(m: &CompatModel) -> bool {
    !m.id.contains('/') && !m.family.is_empty() && !m.family.eq_ignore_ascii_case("other")
}

/// Collapse catalog entries that [`normalize_key`]-match into one entry each,
/// so a model that ships as two upstream rows (a clean row + an org-prefixed
/// `"Other"` twin — see [`is_clean_entry`]) is not classified and rendered
/// twice. Without this, both twins match the same live model, both get
/// `available_now`, and both land in `runs_here` (the duplicate "70B" rows the
/// user reported, plus a spurious `"Other"` family group).
///
/// For each key group, in first-appearance order (so output ordering stays
/// deterministic): pick the display metadata (`id`/`display_name`/`family`)
/// from the clean twin if there is one, else the twin whose `id` has no `/`,
/// else the first; take `model_size` from whichever twin carries it; and keep
/// the UNION of every twin's `compatibility` rows (deduped) so no per-hardware
/// coverage is lost when the two rows differ.
fn dedupe_catalog_models(models: &[CompatModel]) -> Vec<CompatModel> {
    use std::collections::HashMap;
    let key_of = |m: &CompatModel| {
        normalize_key(if !m.id.is_empty() {
            &m.id
        } else {
            &m.display_name
        })
    };
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&CompatModel>> = HashMap::new();
    for m in models {
        let key = key_of(m);
        groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Vec::new()
        });
        groups.get_mut(&key).unwrap().push(m);
    }

    let mut out = Vec::with_capacity(order.len());
    for key in &order {
        let members = &groups[key];
        // Prefer the clean twin's metadata, then a no-slash id, then the first.
        let base = members
            .iter()
            .copied()
            .find(|m| is_clean_entry(m))
            .or_else(|| members.iter().copied().find(|m| !m.id.contains('/')))
            .unwrap_or(members[0]);

        let mut merged = base.clone();
        // Union all twins' compatibility rows (base's already present), deduped.
        for m in members {
            if std::ptr::eq(*m, base) {
                continue;
            }
            for hc in &m.compatibility {
                if !merged.compatibility.contains(hc) {
                    merged.compatibility.push(hc.clone());
                }
            }
        }
        // Fill a missing size from any twin that has one.
        if merged.model_size.is_none() {
            merged.model_size = members.iter().find_map(|m| m.model_size.clone());
        }
        out.push(merged);
    }
    out
}

/// Merge the public compatibility catalog with a box's live `/models` into
/// the three tiers the `tt catalog` command and the macOS model picker
/// render (see [`BoxCatalog`]):
///
/// - `runs_here`: models the box can serve right now via tt-inference-server
///   -- a live `/models` entry (always), or a catalog model `Supported` on
///   `box_mesh` whose on-mesh entry lists `tt-inference-server` in `software`.
/// - `experimental`: models flagged `Experimental` on `box_mesh`, OR
///   `Supported` on `box_mesh` but only via a non-tt-inference-server tool
///   (tt-forge/tt-metal, or no software listed) -- runnable "with the tools,"
///   not run-now. (In both cases, not already in `runs_here`.)
/// - `other_hardware`: models `Supported`/`Experimental` on *some* mesh,
///   but not `box_mesh` -- `needed_hardware` lists which mesh(es).
///
/// Models that are `Not Supported` (or unlisted) everywhere are omitted
/// entirely rather than surfaced as a dead end.
///
/// `catalog == None` (the catalog fetch failed/hasn't happened yet) is a
/// degenerate case: there is no compatibility data to classify against, so
/// every live model is trivially "runs here" and the other tiers are
/// empty. `catalog_available` tells the caller (the CLI/app) whether it's
/// looking at a real classification or this fallback.
///
/// `box_mesh == None` (the box's mesh couldn't be detected) similarly has
/// no "here" to test membership against, so this collapses to a flat
/// list: every catalog model with any `Supported`/`Experimental` entry
/// counts as `runs_here`, with no experimental/other-hardware split.
///
/// Regardless of the catalog shape, a live model always wins: if a catalog
/// model's `id`/`display_name` normalizes (see [`normalize_key`]) to the
/// same key as a live model's `name`, that model is `available_now: true`
/// and appears exactly once, in `runs_here` -- never duplicated into
/// `experimental`/`other_hardware` even if the catalog would otherwise
/// place it there (a live model is definitionally already running, so
/// "experimental"/"needs other hardware" no longer applies). Live models
/// with no catalog match at all are appended to `runs_here` verbatim (see
/// [`live_only_entry`]).
///
/// Ordering is deterministic: catalog order within each tier, then
/// unmatched live models appended (in their input order) to `runs_here` --
/// no sorting/scoring, so callers (and this module's own tests) can assert
/// on exact `Vec` contents.
pub fn classify(
    catalog: Option<&CompatCatalog>,
    live_models: &[ModelInfo],
    box_mesh: Option<&str>,
    catalog_stale: bool,
) -> BoxCatalog {
    // Degenerate case: no catalog data at all -- every live model is
    // trivially available, nothing to classify against.
    let Some(catalog) = catalog else {
        return BoxCatalog {
            box_mesh: box_mesh.map(str::to_string),
            catalog_available: false,
            catalog_stale,
            runs_here: live_models.iter().map(live_only_entry).collect(),
            experimental: Vec::new(),
            other_hardware: Vec::new(),
        };
    };

    // Live models, keyed by normalized name, so catalog models can look
    // themselves up in O(1) and we can track which live models were
    // "claimed" by a catalog match (the rest get appended verbatim).
    let mut live_by_key: std::collections::HashMap<String, &ModelInfo> =
        std::collections::HashMap::new();
    for m in live_models {
        live_by_key.insert(normalize_key(&m.name), m);
    }
    let mut claimed_live_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    let box_mesh_lower = box_mesh.map(str::to_lowercase);

    let mut runs_here = Vec::new();
    let mut experimental = Vec::new();
    let mut other_hardware = Vec::new();

    // Collapse the catalog's twin rows (clean + org-prefixed "Other") so each
    // model is classified once — see `dedupe_catalog_models`.
    let deduped = dedupe_catalog_models(&catalog.models);
    for model in &deduped {
        // Distinct mapped meshes this model has a Supported/Experimental
        // entry for, in catalog order, paired with the strongest status
        // seen for that mesh (Supported beats Experimental if a mesh
        // somehow appears twice with different statuses).
        let mut mesh_status: Vec<(String, CompatStatus)> = Vec::new();
        for hc in &model.compatibility {
            if !matches!(
                hc.status,
                CompatStatus::Supported | CompatStatus::Experimental
            ) {
                continue;
            }
            let mesh = hw_to_mesh(&hc.hardware);
            if let Some(existing) = mesh_status.iter_mut().find(|(m, _)| *m == mesh) {
                if matches!(hc.status, CompatStatus::Supported) {
                    existing.1 = CompatStatus::Supported;
                }
            } else {
                mesh_status.push((mesh, hc.status.clone()));
            }
        }

        let key = normalize_key(if !model.id.is_empty() {
            &model.id
        } else {
            &model.display_name
        });
        let available_now = live_by_key.contains_key(&key);

        let all_meshes: Vec<String> = mesh_status.iter().map(|(m, _)| m.clone()).collect();

        let make_entry = |status_here: &str, needed_hardware: Vec<String>| CatalogEntry {
            id: model.id.clone(),
            display_name: model.display_name.clone(),
            family: model.family.clone(),
            size: model.model_size.clone(),
            software: model
                .compatibility
                .iter()
                .flat_map(|hc| hc.software.clone())
                .fold(Vec::new(), |mut acc, s| {
                    if !acc.contains(&s) {
                        acc.push(s);
                    }
                    acc
                }),
            meshes: all_meshes.clone(),
            needed_hardware,
            available_now,
            // A live-model match always wins: the model is running right
            // now regardless of what the catalog would otherwise say.
            status_here: if available_now {
                "supported".to_string()
            } else {
                status_here.to_string()
            },
        };

        // A live match is a runs-here-forcing signal, full stop: the model
        // is demonstrably servable right now, so it belongs in `runs_here`
        // no matter what the catalog says about it -- including the "Not
        // Supported everywhere" case that would otherwise omit the entry
        // entirely. Handle this *before* any of the omit/experimental/other
        // branches below, and only mark the key "claimed" here, at the
        // point the entry is actually pushed -- never earlier. Otherwise a
        // catalog entry that goes on to be omitted (e.g. empty mesh_status)
        // would still have "claimed" the live key, and the live model would
        // vanish from every tier: neither emitted here nor picked up by the
        // trailing unmatched-live-models append below. See
        // classify_live_model_matching_unsupported_catalog_still_runs_here.
        if available_now {
            runs_here.push(make_entry("supported", Vec::new()));
            claimed_live_keys.insert(key.clone());
            continue;
        }

        if box_mesh_lower.is_none() {
            // No box mesh to test membership against -- flat list of
            // anything with any Supported/Experimental entry.
            if !mesh_status.is_empty() {
                runs_here.push(make_entry("supported", Vec::new()));
            }
            continue;
        }
        let box_mesh_lower = box_mesh_lower.as_deref().unwrap();

        // Compatible (not just exact-match) meshes for this box, via
        // `mesh_compatible` -- e.g. a "p150x2" box is compatible with a
        // catalog row mapped to "P150" (its single-card family), not just
        // an exact "P150X2" row. If more than one compatible mesh is
        // present, Supported beats Experimental (mirrors the aggregation
        // above when the same mesh appears twice).
        let status_on_box = if mesh_status.iter().any(|(m, s)| {
            mesh_compatible(box_mesh_lower, m) && matches!(s, CompatStatus::Supported)
        }) {
            Some(CompatStatus::Supported)
        } else if mesh_status.iter().any(|(m, s)| {
            mesh_compatible(box_mesh_lower, m) && matches!(s, CompatStatus::Experimental)
        }) {
            Some(CompatStatus::Experimental)
        } else {
            None
        };

        // `available_now` is unconditionally `false` from here on (the
        // live-match case above already `continue`d), so these branches
        // never need to special-case it.
        match status_on_box {
            Some(CompatStatus::Supported) => {
                // The box serves models via run.py = tt-inference-server, so
                // "Supported" on-mesh isn't enough on its own: only a
                // Supported, on-mesh entry whose software actually names
                // tt-inference-server is run-now. A model that's Supported
                // on this mesh but only via tt-forge/tt-metal/no listed
                // software is "supported with the tools," not run-now here
                // -- demote to experimental rather than falsely promising
                // runs_here.
                let tis_on_box = model.compatibility.iter().any(|hc| {
                    matches!(hc.status, CompatStatus::Supported)
                        && mesh_compatible(box_mesh_lower, &hw_to_mesh(&hc.hardware))
                        && software_is_tis(&hc.software)
                });
                if tis_on_box {
                    runs_here.push(make_entry("supported", Vec::new()));
                } else {
                    experimental.push(make_entry("experimental", Vec::new()));
                }
            }
            Some(CompatStatus::Experimental) => {
                experimental.push(make_entry("experimental", Vec::new()));
            }
            _ => {
                if !mesh_status.is_empty() {
                    // Supported/Experimental somewhere else -> needs other
                    // hardware; excludes any mesh the box is already
                    // `mesh_compatible` with (nothing the box can actually
                    // run should appear as "needed" -- by construction of
                    // this branch that's everything, since a compatible
                    // mesh would have taken the Supported/Experimental
                    // branch above instead).
                    let needed: Vec<String> = mesh_status
                        .iter()
                        .map(|(m, _)| m.clone())
                        .filter(|m| !mesh_compatible(box_mesh_lower, m))
                        .collect();
                    other_hardware.push(make_entry("unavailable", needed));
                }
                // else: Not Supported everywhere -> omit entirely.
            }
        }
    }

    // Any live model not claimed by a catalog match is appended verbatim,
    // in its original input order, so it's still surfaced as runnable.
    for m in live_models {
        let key = normalize_key(&m.name);
        if !claimed_live_keys.contains(&key) {
            runs_here.push(live_only_entry(m));
        }
    }

    BoxCatalog {
        box_mesh: box_mesh.map(str::to_string),
        catalog_available: true,
        catalog_stale,
        runs_here,
        experimental,
        other_hardware,
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

    #[test]
    fn normalize_key_forms() {
        assert_eq!(normalize_key("Qwen/Qwen3-8B"), "qwen3-8b");
        assert_eq!(normalize_key("Qwen3-8B"), "qwen3-8b");
        assert_eq!(normalize_key("bge_large en.v1.5"), "bge-large-en-v1-5");
    }

    #[test]
    fn classify_dedupes_catalog_entries_with_same_normalized_key() {
        // The upstream catalog ships two rows per model: a clean one
        // (id "llama-3-3-70b-instruct", family "Llama") and an org-prefixed
        // twin (id "meta-llama/llama-3-3-70b-instruct", family "Other") that
        // normalize_key-collapse to the same key. Both are Supported+TIS on
        // this box, so without dedupe BOTH land in runs_here — the duplicate
        // "70B" rows (and the bogus "Other" family group) the user saw.
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"llama-3-3-70b-instruct","display_name":"Llama-3.3-70B-Instruct","family":"Llama","tasks":[],"model_size":"70B","compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
          {"id":"meta-llama/llama-3-3-70b-instruct","display_name":"meta llama/Llama 3.3 70B Instruct","family":"Other","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], Some("p300x2"), false);
        // Exactly one runs_here entry, and it's the CLEAN one (real family,
        // no org/ prefix in the id), not the "Other" twin.
        assert_eq!(bc.runs_here.len(), 1, "duplicate not collapsed: {bc:?}");
        assert_eq!(bc.runs_here[0].id, "llama-3-3-70b-instruct");
        assert_eq!(bc.runs_here[0].family, "Llama");
        assert_eq!(bc.runs_here[0].display_name, "Llama-3.3-70B-Instruct");
        // Size is preserved from whichever twin carried it.
        assert_eq!(bc.runs_here[0].size.as_deref(), Some("70B"));
        assert!(bc.experimental.is_empty() && bc.other_hardware.is_empty());
    }

    #[test]
    fn classify_dedupe_unions_compatibility_across_twins() {
        // When collapsing twins, keep BOTH twins' compatibility rows so no
        // hardware coverage is lost: the clean twin is Supported+TIS only on
        // Galaxy (other hardware for a p300x2 box), while the "Other" twin
        // adds the Quietbox 2 Experimental row. The merged entry must reflect
        // the union — here that makes it Experimental on THIS box.
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"m","display_name":"M","family":"Fam","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
          {"id":"org/m","display_name":"org/M","family":"Other","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Experimental","software":["tt-inference-server"]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], Some("p300x2"), false);
        assert!(bc.runs_here.is_empty());
        assert_eq!(bc.experimental.len(), 1, "twins not merged: {bc:?}");
        assert_eq!(bc.experimental[0].id, "m");
        // Union of meshes across twins: Galaxy→T3K and Quietbox 2→P300X2.
        assert!(bc.experimental[0].meshes.contains(&"T3K".to_string()));
        assert!(bc.experimental[0].meshes.contains(&"P300X2".to_string()));
    }

    #[test]
    fn classify_tiers_by_box_mesh() {
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"a","display_name":"A","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
          {"id":"b","display_name":"B","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Experimental","software":["tt-forge"]}]},
          {"id":"c","display_name":"C","family":"F","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":["tt-metal"]}]},
          {"id":"d","display_name":"D","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Not Supported","software":[]}]}
        ]}"#).unwrap();
        let live = vec![]; // no live models
        let bc = classify(Some(&cat), &live, Some("p300x2"), false);
        assert_eq!(
            bc.runs_here
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert_eq!(
            bc.experimental
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(
            bc.other_hardware
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["c"]
        );
        // d is Not Supported everywhere -> omitted
        assert!(!bc
            .runs_here
            .iter()
            .chain(&bc.experimental)
            .chain(&bc.other_hardware)
            .any(|e| e.id == "d"));
        // c is annotated with the mesh it needs
        assert_eq!(bc.other_hardware[0].needed_hardware, vec!["T3K"]);
        assert!(bc.catalog_available);
    }

    #[test]
    fn classify_live_model_always_runs_here_and_marks_available() {
        use crate::model::ModelInfo;
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"qwen3-8b","display_name":"Qwen3-8B","family":"Qwen","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
        ]}"#).unwrap();
        let live = vec![ModelInfo {
            name: "Qwen/Qwen3-8B".into(),
            devices: vec!["P300X2".into()],
        }];
        let bc = classify(Some(&cat), &live, Some("p300x2"), false);
        // live model wins -> runs_here, available_now, deduped with the catalog entry (matched by normalize_key)
        assert_eq!(bc.runs_here.len(), 1);
        assert!(bc.runs_here[0].available_now);
        assert!(bc.other_hardware.is_empty()); // not double-listed
    }

    #[test]
    fn classify_live_model_matching_unsupported_catalog_still_runs_here() {
        use crate::model::ModelInfo;
        // Catalog says "d" is Not Supported on every hardware row (its only
        // row here is Quietbox 2 / Not Supported) -- normally that means
        // "omit entirely" (see classify_tiers_by_box_mesh's `d`). But a live
        // box is *actually serving* "d" right now via /models, so it must
        // still show up in runs_here: live status always wins over a stale
        // catalog. Regression for the data-loss bug where an all-unsupported
        // catalog match would both omit the entry AND "claim" the live
        // model's key, dropping it from every tier.
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"d","display_name":"D","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Not Supported","software":[]}]}
        ]}"#).unwrap();
        let live = vec![ModelInfo {
            name: "d".into(),
            devices: vec![],
        }];
        let bc = classify(Some(&cat), &live, Some("p300x2"), false);

        let in_runs_here = bc
            .runs_here
            .iter()
            .find(|e| e.id == "d" || e.display_name == "D");
        assert!(
            in_runs_here.is_some(),
            "live model 'd' must be in runs_here, got: {bc:?}"
        );
        assert!(in_runs_here.unwrap().available_now);
        assert!(!bc.experimental.iter().any(|e| e.id == "d"));
        assert!(!bc.other_hardware.iter().any(|e| e.id == "d"));
    }

    #[test]
    fn classify_unavailable_catalog_returns_live_only() {
        use crate::model::ModelInfo;
        let live = vec![ModelInfo {
            name: "X/Y".into(),
            devices: vec![],
        }];
        let bc = classify(None, &live, Some("p300x2"), false);
        assert!(!bc.catalog_available);
        assert_eq!(bc.runs_here.len(), 1);
        assert!(bc.experimental.is_empty() && bc.other_hardware.is_empty());
    }

    #[test]
    fn mesh_compatible_exact_match_case_insensitive() {
        assert!(mesh_compatible("p300x2", "P300X2"));
        assert!(mesh_compatible("P300X2", "p300x2"));
    }

    #[test]
    fn mesh_compatible_multi_card_box_runs_single_card_family() {
        assert!(mesh_compatible("p150x2", "P150"));
        assert!(mesh_compatible("p150x3", "P150"));
        assert!(mesh_compatible("p150x4", "P150"));
        assert!(mesh_compatible("p300x2", "P300"));
        assert!(mesh_compatible("n300x4", "N300"));
    }

    #[test]
    fn mesh_compatible_does_not_upgrade_single_card_to_box_level() {
        // A bare single "p150" card is NOT compatible with a "P150X4"
        // box-level requirement -- compatibility only flows the other way.
        assert!(!mesh_compatible("p150", "P150X4"));
    }

    #[test]
    fn mesh_compatible_unrelated_meshes_are_incompatible() {
        assert!(!mesh_compatible("p300x2", "T3K"));
    }

    #[test]
    fn classify_p150x2_box_demotes_no_software_single_card_p150_model() {
        // A p150x2 box (agent-detected 2-card P150 mesh) should be able to
        // run a model the catalog lists as Supported on single-card "P150"
        // -- not fall into other_hardware with a nonsensical
        // needed_hardware:["P150"] on a box that literally has P150 cards.
        //
        // NOTE (task 1, TIS-focus): this fixture's only compatibility entry
        // has `software: []` (no tt-inference-server), so under the new
        // runs_here rule it no longer qualifies for runs_here -- it demotes
        // to experimental ("supported with the tools," not run-now). The
        // mesh-compatibility behavior this test actually guards (a
        // p150x2 box's on-mesh Supported model must NOT be misclassified
        // into other_hardware with a nonsensical needed_hardware:["P150"])
        // still holds and is asserted below via `other_hardware.is_empty()`.
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"a","display_name":"A","family":"F","tasks":[],"compatibility":[{"hardware":"p150","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], Some("p150x2"), false);
        assert_eq!(
            bc.experimental
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["a"]
        );
        assert!(bc.runs_here.is_empty());
        assert!(bc.other_hardware.is_empty());
    }

    #[test]
    fn classify_p300x2_box_needed_hardware_excludes_compatible_meshes() {
        // A p300x2 box has a model Supported only on single-card "p150" --
        // that's genuinely other hardware, so it lands in other_hardware.
        // But needed_hardware must not list anything the box can already
        // run (nothing box-compatible should appear as "needed").
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"b","display_name":"B","family":"F","tasks":[],"compatibility":[{"hardware":"p150","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], Some("p300x2"), false);
        assert_eq!(
            bc.other_hardware
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["b"]
        );
        assert_eq!(bc.other_hardware[0].needed_hardware, vec!["P150"]);
    }

    #[test]
    fn classify_unknown_mesh_no_split() {
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"a","display_name":"A","family":"F","tasks":[],"compatibility":[{"hardware":"Galaxy","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], None, false);
        // no box mesh -> nothing goes to experimental/other; catalog models land in runs_here as a flat list
        assert!(bc.experimental.is_empty() && bc.other_hardware.is_empty());
        assert_eq!(bc.runs_here.len(), 1);
    }

    #[test]
    fn classify_runs_here_requires_tt_inference_server() {
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"tis","display_name":"TIS","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-inference-server"]}]},
          {"id":"forgeonly","display_name":"ForgeOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-forge"]}]},
          {"id":"metalonly","display_name":"MetalOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-metal"]}]},
          {"id":"nosoftware","display_name":"NoSoftware","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":[]}]}
        ]}"#).unwrap();
        let bc = classify(Some(&cat), &[], Some("p300x2"), false);
        // Only the tt-inference-server model runs here.
        assert_eq!(
            bc.runs_here
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["tis"]
        );
        // Supported-on-mesh-but-not-TIS demote to experimental.
        let exp: Vec<String> = bc.experimental.iter().map(|e| e.id.clone()).collect();
        assert!(exp.contains(&"forgeonly".to_string()));
        assert!(exp.contains(&"metalonly".to_string()));
        assert!(exp.contains(&"nosoftware".to_string()));
        // None of them wrongly landed in other_hardware.
        assert!(bc.other_hardware.is_empty());
    }

    #[test]
    fn classify_live_model_still_runs_here_regardless_of_software() {
        use crate::model::ModelInfo;
        let cat = serde_json::from_str::<CompatCatalog>(r#"{"models":[
          {"id":"forgeonly","display_name":"ForgeOnly","family":"F","tasks":[],"compatibility":[{"hardware":"Quietbox 2","chip_set":"","hardware_family":"","status":"Supported","software":["tt-forge"]}]}
        ]}"#).unwrap();
        // The box's live /models reports it → it IS tt-inference-server-servable now.
        let live = vec![ModelInfo {
            name: "forgeonly".into(),
            devices: vec!["P300X2".into()],
        }];
        let bc = classify(Some(&cat), &live, Some("p300x2"), false);
        assert_eq!(
            bc.runs_here
                .iter()
                .map(|e| e.id.clone())
                .collect::<Vec<_>>(),
            vec!["forgeonly"]
        );
        assert!(bc.experimental.is_empty());
    }

    #[test]
    fn software_is_tt_inference_server_matches_tolerantly() {
        assert!(software_is_tis(&["tt-inference-server".into()]));
        assert!(software_is_tis(&["TT-Inference-Server".into()]));
        assert!(software_is_tis(&["tt_inference_server".into()]));
        assert!(software_is_tis(&[
            "tt-forge".into(),
            "inference-server".into()
        ]));
        assert!(!software_is_tis(&["tt-forge".into()]));
        assert!(!software_is_tis(&[]));
    }
}
