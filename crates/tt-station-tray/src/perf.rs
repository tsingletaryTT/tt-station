//! Reference performance numbers from tt-inference-server's `model_performance_reference.json`.
//!
//! **What the numbers are.** tt-inference-server ships a table of performance *targets* per
//! model and device: time to first token, tokens per second per user, total tokens per second,
//! and other metrics for image, audio, video and embedding models. Its benchmark workflow
//! grades real runs against them. They are reference targets. They were not measured on the
//! machine running this tray, and every piece of text built here says so.
//!
//! **Where the file is.** Two layouts exist:
//!
//! * `<checkout>/benchmarking/benchmark_targets/model_performance_reference.json` (v0.18.x). Keys
//!   are short model names such as `gemma-4-31b-it`.
//! * `<checkout>/reference_config/benchmarking/benchmark_targets/model_performance_reference.json`
//!   (v0.22.x). Keys are mostly Hugging Face repo ids such as `google/gemma-4-31b-it`, with a few
//!   short names mixed in.
//!
//! [`reference_path`] checks both. The checkout itself comes from the `run.py` path in the argv of
//! `tt --json serve <model> --dry-run` ([`checkout_from_plan`]), so the numbers come from the same
//! tt-inference-server version `tt serve` would run. When that fails, [`newest_checkout`] picks the
//! newest `tt-inference-server-v*/src` under the official CLI's tools directory.
//!
//! **File shape.** `{ model: { device: [ config, ... ] } }`, where a config has `isl`, `osl`,
//! `max_concurrency`, `task_type` and `targets: { kind: { metric: number } }`. In the files seen
//! so far (0.18.0, 0.22.0) the kinds are `theoretical` (a ceiling for the hardware) and, in
//! 0.22.0 only, `measured` (acceptance floors taken from runs on Tenstorrent test machines). The
//! parser here is tolerant: unknown keys are ignored, malformed entries are skipped, and a file
//! that is not a JSON object gives no table at all. Nothing here panics or raises a dialog.
//!
//! Everything in this module is pure except [`load`], [`newest_checkout`] and
//! [`checkout_version`], which read the file system.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file name tt-inference-server uses for its reference table.
pub const REFERENCE_FILE: &str = "model_performance_reference.json";

/// The two places the reference file has lived, relative to a checkout root (newest first).
const LAYOUTS: [&str; 2] = [
    "reference_config/benchmarking/benchmark_targets",
    "benchmarking/benchmark_targets",
];

/// One benchmark configuration of one model on one device, as the file lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct BenchConfig {
    /// `text`, `vlm`, `image`, `asr`, `tts`, `video`, `embedding`, `cnn`, ... Older LLM entries
    /// have none.
    pub task_type: Option<String>,
    /// Input sequence length in tokens.
    pub isl: Option<u64>,
    /// Output sequence length in tokens.
    pub osl: Option<u64>,
    /// Number of concurrent users the benchmark drives.
    pub max_concurrency: Option<u64>,
    /// Whether the entry declares `data_parallel`. Such entries describe a data-parallel
    /// deployment (several model replicas on one device), and tt-inference-server uses them only
    /// when the model spec sets `data_parallel_size`. The tray cannot see the model spec, so it
    /// skips them.
    pub data_parallel: bool,
    /// Target kind (`theoretical`, `measured`, ...) to its numeric metrics. Null and non-numeric
    /// values are dropped at parse time.
    pub targets: BTreeMap<String, BTreeMap<String, f64>>,
}

/// The parsed reference table plus the tt-inference-server version it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfRef {
    /// tt-inference-server version (`0.22.0`), when known.
    pub version: Option<String>,
    /// Model key (as written in the file) to device key to configs.
    pub models: BTreeMap<String, BTreeMap<String, Vec<BenchConfig>>>,
}

/// Single-user figures for an LLM-like model.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleUser {
    pub isl: u64,
    pub osl: u64,
    pub ttft_ms: Option<f64>,
    pub tput_user: Option<f64>,
    /// The target kind the numbers come from, exactly as the file names it.
    pub kind: String,
}

/// Total throughput at the highest concurrency the file lists for an LLM-like model.
#[derive(Debug, Clone, PartialEq)]
pub struct Aggregate {
    pub users: u64,
    pub isl: u64,
    pub osl: u64,
    pub tput: f64,
    pub kind: String,
}

/// One config of a model that is not LLM-like, shown with the file's own metric names.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherLine {
    pub task_type: String,
    pub max_concurrency: Option<u64>,
    pub kind: String,
    /// Metric name to value, in the file's names (`ttft_ms`, `rtr`, `tput_prefill`, ...).
    pub metrics: Vec<(String, f64)>,
}

/// What the numbers describe.
#[derive(Debug, Clone, PartialEq)]
pub enum PerfBody {
    /// LLM-like (`text`, `vlm`, or no task type with `isl`/`osl`).
    Text {
        single: Option<SingleUser>,
        aggregate: Option<Aggregate>,
    },
    /// Anything else (image, audio, video, embedding, ...).
    Other(Vec<OtherLine>),
}

/// The reference numbers for one model on one mesh.
#[derive(Debug, Clone, PartialEq)]
pub struct PerfSummary {
    /// The device key used, equal to the station's mesh.
    pub mesh: String,
    pub version: Option<String>,
    pub body: PerfBody,
}

/// Metrics that are grading parameters, not performance figures. Never shown.
const NOT_METRICS: [&str; 1] = ["tolerance"];

/// Parse the reference file. `None` when `text` is not JSON or its top level is not an object.
/// Entries of the wrong shape are skipped one by one.
pub fn parse(text: &str, version: Option<&str>) -> Option<PerfRef> {
    let root: serde_json::Value = serde_json::from_str(text).ok()?;
    let root = root.as_object()?;
    let mut models = BTreeMap::new();
    for (model, devices) in root {
        let Some(devices) = devices.as_object() else {
            continue;
        };
        let mut by_device = BTreeMap::new();
        for (device, configs) in devices {
            let Some(configs) = configs.as_array() else {
                continue;
            };
            let parsed: Vec<BenchConfig> = configs.iter().filter_map(parse_config).collect();
            if !parsed.is_empty() {
                by_device.insert(device.clone(), parsed);
            }
        }
        if !by_device.is_empty() {
            models.insert(model.clone(), by_device);
        }
    }
    Some(PerfRef {
        version: version.map(str::to_string),
        models,
    })
}

/// One config object, or `None` when it is not an object or has no usable targets.
fn parse_config(v: &serde_json::Value) -> Option<BenchConfig> {
    let o = v.as_object()?;
    let uint = |k: &str| o.get(k).and_then(|x| x.as_u64());
    let mut targets = BTreeMap::new();
    for (kind, metrics) in o.get("targets")?.as_object()? {
        let Some(metrics) = metrics.as_object() else {
            continue;
        };
        let numeric: BTreeMap<String, f64> = metrics
            .iter()
            .filter(|(k, _)| !NOT_METRICS.contains(&k.as_str()))
            .filter_map(|(k, x)| x.as_f64().map(|f| (k.clone(), f)))
            .collect();
        if !numeric.is_empty() {
            targets.insert(kind.clone(), numeric);
        }
    }
    if targets.is_empty() {
        return None;
    }
    Some(BenchConfig {
        task_type: o
            .get("task_type")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        isl: uint("isl"),
        osl: uint("osl"),
        max_concurrency: uint("max_concurrency"),
        data_parallel: o.get("data_parallel").is_some_and(|x| !x.is_null()),
        targets,
    })
}

/// The last `/`-separated segment of a model id (`google/gemma-4-31b-it` -> `gemma-4-31b-it`).
fn last_segment(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The file's key for a model, tried in this order (first hit wins; within one step the keys
/// are compared in sorted order, because the parsed table does not keep the file's key order):
///
/// 1. a key equal to `hf_repo`;
/// 2. a key equal to `name`;
/// 3. the same two comparisons ignoring ASCII case;
/// 4. a key whose last path segment equals the last segment of `hf_repo` or of `name`,
///    ignoring case (this is what maps an HF id onto a 0.18-style short key).
///
/// Exact matches come before case-folded ones because the 0.22.0 file holds both
/// `google/gemma-4-31b-it` and `gemma-4-31B-it`, and the catalog's `gemma-4-31B-it` is the
/// second one exactly. tt-inference-server itself looks the table up by the exact HF repo.
fn find_model<'a>(perf: &'a PerfRef, name: &str, hf_repo: Option<&str>) -> Option<&'a str> {
    let keys: Vec<&str> = perf.models.keys().map(String::as_str).collect();
    let wanted: Vec<&str> = hf_repo.into_iter().chain(std::iter::once(name)).collect();
    for w in &wanted {
        if let Some(k) = keys.iter().find(|k| *k == w) {
            return Some(k);
        }
    }
    for w in &wanted {
        if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(w)) {
            return Some(k);
        }
    }
    for w in &wanted {
        let seg = last_segment(w);
        if let Some(k) = keys
            .iter()
            .find(|k| last_segment(k).eq_ignore_ascii_case(seg))
        {
            return Some(k);
        }
    }
    None
}

/// The target kind to show for one config: `measured` when the config has it, else
/// `theoretical`, else the first kind the config has (in name order). The kind's name is
/// carried into the text so the page says exactly what the file says.
fn pick_kind(c: &BenchConfig) -> Option<(&str, &BTreeMap<String, f64>)> {
    for preferred in ["measured", "theoretical"] {
        if let Some((k, m)) = c.targets.get_key_value(preferred) {
            return Some((k.as_str(), m));
        }
    }
    c.targets.iter().next().map(|(k, m)| (k.as_str(), m))
}

/// Whether a config describes an LLM-like benchmark (text in, tokens out).
fn is_text_like(c: &BenchConfig) -> bool {
    match c.task_type.as_deref() {
        Some("text") | Some("vlm") => c.isl.is_some() && c.osl.is_some(),
        None => c.isl.is_some() && c.osl.is_some(),
        Some(_) => false,
    }
}

/// The reference numbers for a model on `mesh`, or `None`.
///
/// `None` when the model is not in the file, when the file has no entry for exactly `mesh`
/// (no other device's numbers are borrowed or scaled), or when nothing usable is left after
/// skipping `data_parallel` entries.
///
/// For LLM-like models:
/// * **Single user**: among configs with `max_concurrency == 1`, the one with the smallest
///   `isl + osl` (ties: first in the file). A short prompt and a short answer is closest to one
///   chat turn, and every LLM entry seen so far has a 128 in / 128 out point. Shows `ttft_ms`
///   and `tput_user`.
/// * **Aggregate**: the config with the largest `max_concurrency` above 1 that has `tput`
///   (ties: smallest `isl + osl`, then first in the file). Shows `tput` and that concurrency.
///   A `tput` on a single-user config is never shown as an aggregate (`gemma-3-27b-it` on
///   `p300x2` lists `tput` 687 at one user next to `tput_user` 41, which cannot both be true).
pub fn summary(
    perf: &PerfRef,
    model_name: &str,
    hf_repo: Option<&str>,
    mesh: &str,
) -> Option<PerfSummary> {
    let key = find_model(perf, model_name, hf_repo)?;
    let configs: Vec<&BenchConfig> = perf
        .models
        .get(key)?
        .get(mesh)?
        .iter()
        .filter(|c| !c.data_parallel)
        .collect();
    let text: Vec<&BenchConfig> = configs
        .iter()
        .copied()
        .filter(|c| is_text_like(c))
        .collect();
    let body = if !text.is_empty() {
        let tokens = |c: &BenchConfig| c.isl.unwrap_or(0) + c.osl.unwrap_or(0);
        let single = text
            .iter()
            .filter(|c| c.max_concurrency == Some(1))
            .filter_map(|c| {
                let (kind, m) = pick_kind(c)?;
                let ttft = m.get("ttft_ms").copied();
                let tpu = m.get("tput_user").copied();
                (ttft.is_some() || tpu.is_some()).then(|| {
                    (
                        tokens(c),
                        SingleUser {
                            isl: c.isl.unwrap_or(0),
                            osl: c.osl.unwrap_or(0),
                            ttft_ms: ttft,
                            tput_user: tpu,
                            kind: kind.to_string(),
                        },
                    )
                })
            })
            // `min_by_key` keeps the first of equal keys, so file order breaks ties.
            .min_by_key(|(t, _)| *t)
            .map(|(_, s)| s);
        let aggregate = text
            .iter()
            .filter(|c| c.max_concurrency.is_some_and(|n| n > 1))
            .filter_map(|c| {
                let (kind, m) = pick_kind(c)?;
                let tput = *m.get("tput")?;
                let users = c.max_concurrency?;
                Some((
                    users,
                    tokens(c),
                    Aggregate {
                        users,
                        isl: c.isl.unwrap_or(0),
                        osl: c.osl.unwrap_or(0),
                        tput,
                        kind: kind.to_string(),
                    },
                ))
            })
            // Largest concurrency; among equals the smallest isl+osl; among those the first.
            .fold(None::<(u64, u64, Aggregate)>, |best, cur| match &best {
                Some((u, t, _)) if (cur.0 < *u) || (cur.0 == *u && cur.1 >= *t) => best,
                _ => Some(cur),
            })
            .map(|(_, _, a)| a);
        if single.is_none() && aggregate.is_none() {
            return None;
        }
        PerfBody::Text { single, aggregate }
    } else {
        let lines: Vec<OtherLine> = configs
            .iter()
            .filter_map(|c| {
                let (kind, m) = pick_kind(c)?;
                Some(OtherLine {
                    task_type: c
                        .task_type
                        .clone()
                        .unwrap_or_else(|| "(no task type)".to_string()),
                    max_concurrency: c.max_concurrency,
                    kind: kind.to_string(),
                    metrics: m.iter().map(|(k, v)| (k.clone(), *v)).collect(),
                })
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        PerfBody::Other(lines)
    };
    Some(PerfSummary {
        mesh: mesh.to_string(),
        version: perf.version.clone(),
        body,
    })
}

/// A number as the file wrote it: `37` for 37.0, `55.1`, `0.091`.
fn num(v: f64) -> String {
    format!("{v}")
}

/// `tt-inference-server 0.22.0`, or `tt-inference-server (version unknown)`.
fn source_name(version: Option<&str>) -> String {
    match version {
        Some(v) => format!("tt-inference-server {v}"),
        None => "tt-inference-server (version unknown)".to_string(),
    }
}

/// The short text appended to a model's list row, for LLM-like models with data:
/// `  ·  ~37 tok/s/user`, or `  ·  ~400 tok/s total at 32 users` when the file has no
/// single-user rate. `None` for other models. The separator matches the rest of the row.
pub fn row_suffix(s: &PerfSummary) -> Option<String> {
    let PerfBody::Text { single, aggregate } = &s.body else {
        return None;
    };
    if let Some(t) = single.as_ref().and_then(|s| s.tput_user) {
        return Some(format!("  ·  ~{} tok/s/user", num(t)));
    }
    aggregate
        .as_ref()
        .map(|a| format!("  ·  ~{} tok/s total at {} users", num(a.tput), a.users))
}

/// `1 user` or `32 users`.
fn users(n: u64) -> String {
    if n == 1 {
        "1 user".to_string()
    } else {
        format!("{n} users")
    }
}

/// The block for the detail pane: a heading that names the mesh, the tt-inference-server
/// version and the words "not measured on this machine", then one line per figure, then one
/// line explaining each target kind used.
pub fn detail_block(s: &PerfSummary) -> String {
    let mut out = vec![format!(
        "Reference performance on {} ({} targets, not measured on this machine)",
        s.mesh,
        source_name(s.version.as_deref())
    )];
    let mut kinds: Vec<&str> = Vec::new();
    match &s.body {
        PerfBody::Text { single, aggregate } => {
            if let Some(u) = single {
                let mut parts = Vec::new();
                if let Some(t) = u.ttft_ms {
                    parts.push(format!("{} ms to first token", num(t)));
                }
                if let Some(t) = u.tput_user {
                    parts.push(format!("{} tok/s per user", num(t)));
                }
                out.push(format!(
                    "1 user, {} in / {} out: {} ({})",
                    u.isl,
                    u.osl,
                    parts.join(", "),
                    u.kind
                ));
                kinds.push(&u.kind);
            }
            if let Some(a) = aggregate {
                out.push(format!(
                    "{}, {} in / {} out: {} tok/s total ({})",
                    users(a.users),
                    a.isl,
                    a.osl,
                    num(a.tput),
                    a.kind
                ));
                kinds.push(&a.kind);
            }
        }
        PerfBody::Other(lines) => {
            for l in lines {
                let metrics: Vec<String> = l
                    .metrics
                    .iter()
                    .map(|(k, v)| format!("{k} {}", num(*v)))
                    .collect();
                let who = l
                    .max_concurrency
                    .map(|n| format!(", {}", users(n)))
                    .unwrap_or_default();
                out.push(format!(
                    "{}{who}: {} ({})",
                    l.task_type,
                    metrics.join(", "),
                    l.kind
                ));
                kinds.push(&l.kind);
            }
        }
    }
    kinds.sort_unstable();
    kinds.dedup();
    for k in kinds {
        out.push(match k {
            "theoretical" => {
                "\"theoretical\" is the ceiling the file sets for this hardware.".to_string()
            }
            "measured" => "\"measured\" is a target the file took from runs on Tenstorrent test \
                           machines."
                .to_string(),
            other => format!("\"{other}\" is the file's own name for this target."),
        });
    }
    out.join("\n")
}

/// The detail pane's line for a model the file has no numbers for on this mesh.
pub fn no_data_line(mesh: &str, version: Option<&str>) -> String {
    format!(
        "No reference numbers for {mesh} in {}.",
        source_name(version)
    )
}

/// The tt-inference-server checkout named by a `tt --json serve <model> --dry-run` plan: the
/// directory holding the `run.py` in its `argv`. `None` when the plan has no such argument
/// (a tt-model bundle plan runs `tt-model`, not `run.py`).
pub fn checkout_from_plan(plan: &serde_json::Value) -> Option<PathBuf> {
    plan.get("argv")?
        .as_array()?
        .iter()
        .filter_map(|a| a.as_str())
        .map(Path::new)
        .find(|p| p.file_name().is_some_and(|f| f == "run.py"))
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
}

/// The reference file inside a checkout, in either layout, when it exists.
pub fn reference_path(checkout: &Path) -> Option<PathBuf> {
    LAYOUTS
        .iter()
        .map(|l| checkout.join(l).join(REFERENCE_FILE))
        .find(|p| p.is_file())
}

/// `0.22.0` from a `tt-inference-server-v0.22.0` directory name.
fn version_from_dir_name(name: &str) -> Option<&str> {
    name.strip_prefix("tt-inference-server-v")
        .filter(|v| !v.is_empty())
}

/// The checkout's version: its `VERSION` file, else the `tt-inference-server-vX` name of the
/// checkout directory or of its parent (the official CLI installs to
/// `tools/tt-inference-server-vX/src`).
pub fn checkout_version(checkout: &Path) -> Option<String> {
    if let Ok(v) = std::fs::read_to_string(checkout.join("VERSION")) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    [Some(checkout), checkout.parent()]
        .into_iter()
        .flatten()
        .filter_map(|d| d.file_name()?.to_str())
        .find_map(version_from_dir_name)
        .map(str::to_string)
}

/// Numeric parts of a version for ordering (`0.22.0` -> `[0, 22, 0]`); non-numeric parts end it.
fn version_key(v: &str) -> Vec<u64> {
    v.split('.').map_while(|p| p.parse().ok()).collect()
}

/// The newest `tt-inference-server-v<version>/src` under `tools_dir` that holds the reference
/// file, by numeric version. The official CLI installs checkouts there
/// (`~/.local/share/tenstorrent/tools`).
pub fn newest_checkout(tools_dir: &Path) -> Option<PathBuf> {
    let mut found: Vec<(Vec<u64>, PathBuf)> = std::fs::read_dir(tools_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let key = version_key(version_from_dir_name(&name)?);
            let src = e.path().join("src");
            reference_path(&src).map(|_| (key, src))
        })
        .collect();
    found.sort();
    found.pop().map(|(_, p)| p)
}

/// Read and parse the reference file of `checkout`. `None` when the file is missing,
/// unreadable or malformed. Blocking file I/O: call it off the GTK thread.
pub fn load(checkout: &Path) -> Option<PerfRef> {
    let path = reference_path(checkout)?;
    let text = std::fs::read_to_string(path).ok()?;
    parse(&text, checkout_version(checkout).as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: &str = include_str!("../tests/fixtures/perf_reference_short.json");
    const HF: &str = include_str!("../tests/fixtures/perf_reference_hf.json");

    fn short() -> PerfRef {
        parse(SHORT, Some("0.18.0")).expect("short fixture parses")
    }
    fn hf() -> PerfRef {
        parse(HF, Some("0.22.0")).expect("hf fixture parses")
    }

    #[test]
    fn parses_short_name_keys() {
        let p = short();
        assert_eq!(p.version.as_deref(), Some("0.18.0"));
        let c = &p.models["gemma-4-31b-it"]["p300x2"][0];
        assert_eq!(c.task_type.as_deref(), Some("text"));
        assert_eq!(
            (c.isl, c.osl, c.max_concurrency),
            (Some(128), Some(128), Some(1))
        );
        assert_eq!(c.targets["theoretical"]["ttft_ms"], 46.0);
        assert_eq!(c.targets["theoretical"]["tput_user"], 37.0);
        assert!(p.models["Llama-3.1-8B-Instruct"].contains_key("n150"));
    }

    #[test]
    fn parses_hf_id_keys_and_ignores_unknown_fields() {
        let p = hf();
        assert!(p.models.contains_key("google/gemma-4-31b-it"));
        let galaxy = &p.models["Qwen/Qwen3-32B"]["galaxy"];
        // The first galaxy entry carries `_comment` and `data_parallel`; both are tolerated.
        assert!(galaxy[0].data_parallel);
        assert_eq!(galaxy[0].targets["measured"]["tput_user"], 55.1);
        assert!(
            !galaxy[0].targets["measured"].contains_key("tolerance"),
            "tolerance is a grading parameter, not a metric"
        );
        assert!(!galaxy[2].data_parallel);
        // whisper's `ttft_streaming_ms: null` is dropped.
        let w = &p.models["openai/whisper-large-v3"]["p300x2"][0];
        assert!(!w.targets["theoretical"].contains_key("ttft_streaming_ms"));
        assert_eq!(w.targets["theoretical"]["rtr"], 16.0);
    }

    #[test]
    fn malformed_json_gives_none() {
        assert_eq!(parse("{not json", None), None);
        assert_eq!(parse("[1, 2]", None), None, "top level must be an object");
        assert_eq!(parse("", None), None);
        // Wrong shapes inside a valid object are skipped, never a panic.
        let p = parse(
            r#"{"a": 1, "b": {"p300x2": "x"}, "c": {"p300x2": [1, {"targets": 2}, {}]}}"#,
            None,
        )
        .unwrap();
        assert!(p.models.is_empty());
    }

    #[test]
    fn matches_by_hf_repo() {
        let s = summary(&hf(), "Qwen3-32B", Some("Qwen/Qwen3-32B"), "p300x2").unwrap();
        assert_eq!(s.mesh, "p300x2");
        let PerfBody::Text { single, .. } = s.body else {
            panic!()
        };
        assert_eq!(single.unwrap().tput_user, Some(37.0));
    }

    #[test]
    fn exact_name_beats_case_folded_hf_repo() {
        // Catalog: name gemma-4-31B-it, hf_repo google/gemma-4-31B-it. The 0.22.0 file has
        // `google/gemma-4-31b-it` (one config) and `gemma-4-31B-it` (two configs). The exact
        // key is the second, and it has the 32-user point.
        let s = summary(
            &hf(),
            "gemma-4-31B-it",
            Some("google/gemma-4-31B-it"),
            "p300x2",
        )
        .unwrap();
        let PerfBody::Text { aggregate, .. } = s.body else {
            panic!()
        };
        assert_eq!(aggregate.unwrap().users, 32);
    }

    #[test]
    fn matches_hf_repo_case_insensitively() {
        assert!(summary(&hf(), "x", Some("QWEN/qwen3-32b"), "p300x2").is_some());
        // Two keys share a last segment; the case-folded full id picks the right one before
        // the last-segment fallback could pick the first.
        let p = parse(
            r#"{"org-a/Model": {"p300x2": [{"isl": 1, "osl": 1, "max_concurrency": 1,
                  "task_type": "text", "targets": {"theoretical": {"tput_user": 1}}}]},
                "org-b/model": {"p300x2": [{"isl": 1, "osl": 1, "max_concurrency": 1,
                  "task_type": "text", "targets": {"theoretical": {"tput_user": 2}}}]}}"#,
            None,
        )
        .unwrap();
        let s = summary(&p, "x", Some("ORG-B/MODEL"), "p300x2").unwrap();
        let PerfBody::Text { single, .. } = s.body else {
            panic!()
        };
        assert_eq!(single.unwrap().tput_user, Some(2.0));
    }

    #[test]
    fn matches_by_short_name_case_insensitively() {
        let s = summary(&short(), "LLAMA-3.1-8B-INSTRUCT", None, "n150").unwrap();
        let PerfBody::Text { single, .. } = s.body else {
            panic!()
        };
        assert_eq!(single.unwrap().ttft_ms, Some(51.0));
    }

    #[test]
    fn matches_hf_repo_against_a_short_key_by_last_segment() {
        let s = summary(
            &short(),
            "gemma-4-31B-it",
            Some("google/gemma-4-31B-it"),
            "p300x2",
        )
        .unwrap();
        assert_eq!(s.version.as_deref(), Some("0.18.0"));
        // A name with an org prefix and no hf_repo also matches by its last segment.
        assert!(summary(&short(), "meta-llama/Llama-3.1-8B-Instruct", None, "p150").is_some());
    }

    #[test]
    fn no_match_gives_none() {
        assert_eq!(
            summary(&short(), "No-Such-Model", Some("x/No-Such-Model"), "p300x2"),
            None
        );
    }

    #[test]
    fn mesh_absent_gives_none_without_borrowing_another_device() {
        // Llama-3.1-8B-Instruct has p150 and n150 in the fixture, and no p300x2.
        assert_eq!(
            summary(&short(), "Llama-3.1-8B-Instruct", None, "p300x2"),
            None
        );
        assert_eq!(
            summary(&short(), "Llama-3.1-8B-Instruct", None, "P150"),
            None,
            "exact string"
        );
    }

    #[test]
    fn data_parallel_entries_are_skipped() {
        // Galaxy has dp=4 entries (conc 1 measured, conc 32 measured) and one plain conc-1
        // theoretical entry. Only the plain one is used.
        let s = summary(&hf(), "Qwen3-32B", Some("Qwen/Qwen3-32B"), "galaxy").unwrap();
        let PerfBody::Text { single, aggregate } = s.body else {
            panic!()
        };
        let single = single.unwrap();
        assert_eq!(single.kind, "theoretical");
        assert_eq!(single.tput_user, Some(156.0));
        assert_eq!(aggregate, None, "the 32-user point is data-parallel only");
    }

    /// A synthetic table (the real files only carry `measured` on data-parallel entries).
    fn synthetic() -> PerfRef {
        parse(
            r#"{"m": {"p300x2": [
                {"isl": 2048, "osl": 128, "max_concurrency": 1, "task_type": "text",
                 "targets": {"theoretical": {"ttft_ms": 400, "tput_user": 30, "tput": 30}}},
                {"isl": 128, "osl": 128, "max_concurrency": 1, "task_type": "text",
                 "targets": {"theoretical": {"ttft_ms": 46, "tput_user": 37, "tput": 37},
                             "measured": {"ttft_ms": 60, "tput_user": 33.5, "tput": 33.5}}},
                {"isl": 128, "osl": 128, "max_concurrency": 32, "task_type": "text",
                 "targets": {"theoretical": {"ttft_ms": 200, "tput_user": 20, "tput": 600}}},
                {"isl": 2048, "osl": 128, "max_concurrency": 32, "task_type": "text",
                 "targets": {"theoretical": {"tput": 500}}},
                {"isl": 128, "osl": 128, "max_concurrency": 8, "task_type": "text",
                 "targets": {"theoretical": {"tput": 250}}}
            ]}}"#,
            Some("9.9.9"),
        )
        .unwrap()
    }

    #[test]
    fn picks_the_smallest_single_user_config_and_prefers_measured() {
        let s = summary(&synthetic(), "m", None, "p300x2").unwrap();
        let PerfBody::Text { single, .. } = s.body else {
            panic!()
        };
        assert_eq!(
            single.unwrap(),
            SingleUser {
                isl: 128,
                osl: 128,
                ttft_ms: Some(60.0),
                tput_user: Some(33.5),
                kind: "measured".into()
            }
        );
    }

    #[test]
    fn picks_the_highest_concurrency_aggregate() {
        let s = summary(&synthetic(), "m", None, "p300x2").unwrap();
        let PerfBody::Text { aggregate, .. } = s.body else {
            panic!()
        };
        assert_eq!(
            aggregate.unwrap(),
            Aggregate {
                users: 32,
                isl: 128,
                osl: 128,
                tput: 600.0,
                kind: "theoretical".into()
            }
        );
    }

    #[test]
    fn a_single_user_tput_is_never_an_aggregate() {
        // gemma-4-31b-it on p300x2 (0.18.0) has only a 1-user point.
        let s = summary(&short(), "gemma-4-31b-it", None, "p300x2").unwrap();
        let PerfBody::Text { aggregate, .. } = s.body else {
            panic!()
        };
        assert_eq!(aggregate, None);
    }

    #[test]
    fn row_suffix_text() {
        let s = summary(&short(), "gemma-4-31b-it", None, "p300x2").unwrap();
        assert_eq!(row_suffix(&s).as_deref(), Some("  ·  ~37 tok/s/user"));
        let img = summary(&short(), "FLUX.1-dev", None, "p300x2").unwrap();
        assert_eq!(row_suffix(&img), None, "no suffix for non-LLM models");
    }

    #[test]
    fn detail_block_text_for_an_llm() {
        let s = summary(
            &hf(),
            "gemma-4-31B-it",
            Some("google/gemma-4-31B-it"),
            "p300x2",
        )
        .unwrap();
        assert_eq!(
            detail_block(&s),
            "Reference performance on p300x2 (tt-inference-server 0.22.0 targets, not measured \
             on this machine)\n\
             1 user, 128 in / 128 out: 46 ms to first token, 37 tok/s per user (theoretical)\n\
             32 users, 128 in / 128 out: 400 tok/s total (theoretical)\n\
             \"theoretical\" is the ceiling the file sets for this hardware."
        );
    }

    #[test]
    fn detail_block_uses_the_files_own_names_for_other_models() {
        let s = summary(
            &hf(),
            "whisper-large-v3",
            Some("openai/whisper-large-v3"),
            "p300x2",
        )
        .unwrap();
        assert_eq!(
            detail_block(&s),
            "Reference performance on p300x2 (tt-inference-server 0.22.0 targets, not measured \
             on this machine)\n\
             asr, 1 user: rtr 16, tput_user 62, ttft_ms 1900 (theoretical)\n\
             \"theoretical\" is the ceiling the file sets for this hardware."
        );
    }

    #[test]
    fn the_label_names_the_version_and_says_not_measured_here() {
        for s in [
            summary(&short(), "gemma-4-31b-it", None, "p300x2").unwrap(),
            summary(&hf(), "bge-m3", Some("BAAI/bge-m3"), "p300x2").unwrap(),
        ] {
            let b = detail_block(&s);
            assert!(b.contains("not measured on this machine"), "{b}");
            assert!(b.contains(&format!(
                "tt-inference-server {}",
                s.version.clone().unwrap()
            )));
        }
        let mut s = summary(&short(), "gemma-4-31b-it", None, "p300x2").unwrap();
        s.version = None;
        assert!(detail_block(&s).contains("tt-inference-server (version unknown) targets"));
    }

    #[test]
    fn no_data_line_text() {
        assert_eq!(
            no_data_line("p300x2", Some("0.22.0")),
            "No reference numbers for p300x2 in tt-inference-server 0.22.0."
        );
    }

    #[test]
    fn checkout_comes_from_the_run_py_argument() {
        let plan = serde_json::json!({"argv": [
            "/h/tools/tt-inference-server-v0.22.0/venv/bin/python",
            "/h/tools/tt-inference-server-v0.22.0/src/run.py",
            "--model", "gemma-4-31B-it"
        ]});
        assert_eq!(
            checkout_from_plan(&plan),
            Some(PathBuf::from("/h/tools/tt-inference-server-v0.22.0/src"))
        );
        let bundle = serde_json::json!({"argv": ["/h/bin/tt-model", "serve", "a/b"]});
        assert_eq!(checkout_from_plan(&bundle), None);
        assert_eq!(checkout_from_plan(&serde_json::json!({})), None);
    }

    #[test]
    fn finds_the_file_in_either_layout_and_the_newest_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let mk = |ver: &str, layout: &str| {
            let src = tmp.path().join(format!("tt-inference-server-v{ver}/src"));
            std::fs::create_dir_all(src.join(layout)).unwrap();
            std::fs::write(src.join(layout).join(REFERENCE_FILE), SHORT).unwrap();
            src
        };
        let old = mk("0.9.0", "benchmarking/benchmark_targets");
        let new = mk("0.18.0", "reference_config/benchmarking/benchmark_targets");
        // A newer checkout without the file is skipped.
        std::fs::create_dir_all(tmp.path().join("tt-inference-server-v0.30.0/src")).unwrap();
        assert!(reference_path(&old).is_some());
        assert!(reference_path(&new).is_some());
        assert_eq!(
            newest_checkout(tmp.path()),
            Some(new.clone()),
            "0.18 > 0.9 numerically"
        );
        // No VERSION file: the version comes from the directory name.
        assert_eq!(checkout_version(&new).as_deref(), Some("0.18.0"));
        std::fs::write(new.join("VERSION"), "0.18.1\n").unwrap();
        assert_eq!(load(&new).unwrap().version.as_deref(), Some("0.18.1"));
        assert_eq!(load(tmp.path()), None, "no file, no table");
    }
}
