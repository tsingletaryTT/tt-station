// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The models Code can run on: one list from every way the box serves a model.
//!
//! - the `tt` catalog (`crate::ttcli`), started with `POST /serve`;
//! - installed tt-model community packages (`crate::community`), one choice
//!   per serve profile that fits this box, started with `POST /community/serve`
//!   and that profile;
//! - native vLLM bundles (`crate::native_vllm`), started with
//!   `POST /vllm/native/serve`.
//!
//! There's no default to pick here: people choose any model and profile that
//! would work. One is listed when it would work for Code: a chat (instruct)
//! LLM, not a base checkpoint, a media model or an embedder; its weights on
//! the box; fitting this box's chips; and tool calling on its serving path.
//! Where a source says nothing about tool calling, it's listed anyway, marked
//! unverified, rather than hidden.
//!
//! Each choice carries what's known about it here: window, chips, prefix
//! caching, the speed Code measured in that setup (`crate::codespeed`'s book),
//! the latest check-up's agent reliability (`crate::evals`), and what choosing
//! it does. Switching is the client calling the source's own serve route
//! (`serve` in each choice), so the roles, settings, leases and refusals of
//! that route apply exactly as they do from Models.
//!
//! - `GET /code/models` (authed).

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use crate::codespeed::{
    cache_from_flags, same_model, switch_now, Cache, Candidate, Choice, PrefixCache, Serving,
};
use crate::routes::{AppState, BearerAuth, Phase};

// ---------------------------------------------------------------- one choice

/// Where a choice is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Catalog,
    Community,
    Native,
    /// Serving now by some path this list doesn't know (shown so Code's model is there).
    Running,
}

/// Whether the model makes tool calls on this serving path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tools {
    pub verified: bool,
    /// How we know, or why we don't: "tool parser openai", "its package doesn't say".
    pub how: String,
}

/// The route and body that start a choice: the source's own serve route.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServeCall {
    pub route: &'static str,
    pub body: Value,
}

/// One model (in one setup) the picker offers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Opt {
    /// Unique in the list: `catalog:<name>`, `community:<id>#<profile>`, `native:<id>`.
    pub key: String,
    /// The model it serves, the same for all its setups (the picker groups by it).
    pub group: String,
    /// The model's name to show.
    pub name: String,
    /// The served model (its Hugging Face repo, or the catalog name).
    pub model: String,
    pub source: Source,
    /// Where it comes from, to show: "tt catalog", the package or bundle id.
    pub origin: String,
    pub profile: Option<String>,
    /// "4 chips · low latency".
    pub profile_label: Option<String>,
    /// The profile's own description, from its package.
    pub profile_note: Option<String>,
    pub chips: Option<u64>,
    pub context: Option<u64>,
    pub tools: Tools,
    pub prefix_cache: PrefixCache,
    pub params_b: Option<f64>,
    /// The book key's setup (`crate::codespeed::book_key`).
    #[serde(skip)]
    pub setup: Option<String>,
    pub serve: Option<ServeCall>,
}

impl Opt {
    fn base(source: Source, key: String, model: &str, origin: String) -> Opt {
        let name = short(model).to_string();
        Opt {
            key,
            group: name.to_lowercase(),
            name,
            model: model.to_string(),
            source,
            origin,
            profile: None,
            profile_label: None,
            profile_note: None,
            chips: None,
            context: None,
            tools: Tools {
                verified: false,
                how: String::new(),
            },
            prefix_cache: PrefixCache::new(Cache::Unknown, "not seen serving here yet"),
            params_b: None,
            setup: None,
            serve: None,
        }
    }
}

fn short(m: &str) -> &str {
    m.rsplit('/').next().unwrap_or(m)
}

/// The serve call for a choice: what Models would send for the same start.
pub fn serve_call(source: Source, id: &str, profile: Option<&str>) -> Option<ServeCall> {
    match source {
        Source::Catalog => Some(ServeCall {
            route: "/serve",
            body: json!({ "model": id, "force": true }),
        }),
        Source::Community => Some(ServeCall {
            route: "/community/serve",
            body: json!({ "id": id, "profile": profile, "stop_serving": true }),
        }),
        Source::Native => Some(ServeCall {
            route: "/vllm/native/serve",
            body: json!({ "dir": id, "stop_serving": true }),
        }),
        Source::Running => None,
    }
}

// ---------------------------------------------------------------- what counts as a chat model

/// Names that are never a chat model, whatever their config says.
fn not_chat_by_name(name: &str) -> Option<&'static str> {
    let n = name.to_lowercase();
    if n.contains("embed") || n.contains("rerank") {
        Some("an embedding model")
    } else if n.ends_with("-base") || n.contains("-base-") || n.ends_with("_base") {
        Some("a base checkpoint")
    } else {
        None
    }
}

/// [`chat_llm_in`]'s answer when there's nothing on the box to go by.
pub const NO_CONFIG: &str = "its config isn't on the box";

/// Is the model in these snapshot folders a chat LLM? Its `config.json` names
/// a causal-LM (or vision-language) architecture, and it has a chat template
/// (a base checkpoint has none). `Err` says why not.
pub fn chat_llm_in(snapshots: &[PathBuf]) -> Result<(), &'static str> {
    let mut arch_ok = None;
    let mut template = false;
    for dir in snapshots {
        if let Some(cfg) = std::fs::read(dir.join("config.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            let archs: Vec<&str> = cfg["architectures"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            arch_ok = Some(archs.iter().any(|a| {
                a.ends_with("ForCausalLM")
                    || a.ends_with("ForConditionalGeneration")
                    || a.ends_with("LMHeadModel")
            }));
        }
        template |= dir.join("chat_template.jinja").is_file()
            || dir.join("chat_template.json").is_file()
            || std::fs::read(dir.join("tokenizer_config.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|t| !t["chat_template"].is_null());
    }
    match arch_ok {
        None => Err(NO_CONFIG),
        Some(false) => Err("not a language model"),
        Some(true) if !template => Err("no chat template (a base checkpoint)"),
        Some(true) => Ok(()),
    }
}

fn snapshots(hub: &Path, repo: &str) -> Vec<PathBuf> {
    std::fs::read_dir(
        hub.join(format!("models--{}", repo.replace('/', "--")))
            .join("snapshots"),
    )
    .into_iter()
    .flatten()
    .flatten()
    .map(|e| e.path())
    .collect()
}

fn chat_llm(repo: &str) -> Result<(), &'static str> {
    chat_llm_at(&crate::download::hub_dir(), repo)
}

fn chat_llm_at(hub: &Path, repo: &str) -> Result<(), &'static str> {
    if let Some(why) = not_chat_by_name(repo) {
        return Err(why);
    }
    chat_llm_in(&snapshots(hub, repo))
}

// ---------------------------------------------------------------- chips

/// Chips a board layout uses: `p150` 1, `p300` 2, `p150x4` 4, `p300x2` 4, a QuietBox 2 4.
pub fn chips_for(hardware: &str) -> Option<u64> {
    let h = hardware.trim().to_lowercase();
    if matches!(h.as_str(), "qb2" | "quietbox2" | "blackholex4") {
        return Some(4);
    }
    let (board, count) = h.split_once('x').unwrap_or((h.as_str(), "1"));
    let per = match board {
        "p100" | "p100a" | "p150" | "p150a" | "p150b" | "blackhole" => 1,
        "p300" | "p300a" | "p300c" => 2,
        _ => return None,
    };
    count.parse::<u64>().ok().map(|n| n * per)
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let mut found = None;
    for (i, a) in args.iter().enumerate() {
        if a == flag {
            found = args.get(i + 1).cloned();
        } else if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            found = Some(v.to_string());
        }
    }
    found
}

/// A manifest's `args`, flattened: `[["--x", "1"], "--y"]` → `--x 1 --y`.
fn flat_args(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for a in v.as_array().into_iter().flatten() {
        match a {
            Value::String(s) => out.push(s.clone()),
            Value::Array(parts) => out.extend(parts.iter().filter_map(|p| match p {
                Value::String(s) => Some(s.clone()),
                Value::Null => None,
                other => Some(other.to_string()),
            })),
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------- community packages

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// tt-model's index of installed packages.
fn tt_model_db() -> Value {
    std::fs::read(home().join(".cache/tt-model/installed.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn read_manifest(entry: &Value) -> Value {
    entry["manifest"]
        .as_str()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

/// An installed package's model (its weights repo) and default profile.
pub fn package_model(id: &str) -> Option<(String, Option<String>)> {
    let db = tt_model_db();
    let entry = db.get(id)?;
    let m = read_manifest(entry);
    let weights = m["weights"]["repo_id"].as_str()?.to_string();
    let default = m["container"]["default_profile"]
        .as_str()
        .or(entry["profile"].as_str())
        .map(str::to_string);
    Some((weights, default))
}

/// One serve profile of a package, as this box would run it.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub hardware: String,
    pub chips: Option<u64>,
    pub fits: bool,
    pub context: Option<u64>,
    pub tool_parser: Option<String>,
    pub args: Vec<String>,
    pub description: Option<String>,
}

/// A package's profiles, each resolved against the package's own settings.
/// A manifest without `serve_profiles` has one: its default.
pub fn profiles(manifest: &Value, entry: &Value) -> Vec<Profile> {
    let serve = &manifest["container"]["serve"];
    let pkg_hw = serve["hardware"].as_str().unwrap_or("");
    let pkg_args = flat_args(&serve["args"]);
    let pkg_parser = serve["capabilities"]["tool_parser"].as_str();
    let resolve = |p: &Value, name: String| {
        let hw = p["hardware"].as_str().unwrap_or(pkg_hw).to_string();
        let visible = p["env"]
            .as_object()
            .and_then(|e| {
                e.iter()
                    .find(|(k, _)| k.ends_with("VISIBLE_DEVICES"))
                    .and_then(|(_, v)| v.as_str())
            })
            .map(|v| v.split(',').filter(|x| !x.trim().is_empty()).count() as u64);
        let chips = chips_for(&hw)
            .or(visible)
            .or(manifest["device_count"].as_u64());
        let (fits, _) = crate::community::fits(chips, &hw);
        let mut args = pkg_args.clone();
        args.extend(flat_args(&p["args"]));
        let context = p["max_model_len"]
            .as_u64()
            .or_else(|| flag_value(&args, "--max-model-len").and_then(|v| v.parse().ok()))
            .or(serve["max_model_len"].as_u64());
        let tool_parser = p["capabilities"]["tool_parser"]
            .as_str()
            .or(pkg_parser)
            .map(str::to_string)
            .or_else(|| flag_value(&args, "--tool-call-parser"));
        Profile {
            name,
            hardware: hw,
            chips,
            fits,
            context,
            tool_parser,
            args,
            description: p["description"].as_str().map(str::to_string),
        }
    };
    let listed: Vec<Profile> = manifest["container"]["serve_profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some(resolve(p, p["name"].as_str()?.to_string())))
        .collect();
    if !listed.is_empty() {
        return listed;
    }
    let name = manifest["container"]["default_profile"]
        .as_str()
        .or(entry["profile"].as_str())
        .unwrap_or("default")
        .to_string();
    vec![resolve(&Value::Null, name)]
}

/// Can `profile` of the installed package `id` be started here? For
/// `POST /community/serve`'s `profile`: one of its own, and fitting this box.
pub fn profile_ok(id: &str, profile: &str) -> Result<(), String> {
    let db = tt_model_db();
    let entry = db.get(id).ok_or("Download it first.")?;
    let all = profiles(&read_manifest(entry), entry);
    match all.iter().find(|p| p.name == profile) {
        None => Err(format!("{} has no profile called {profile}.", short(id))),
        Some(p) if !p.fits => Err(format!(
            "Its {profile} profile doesn't fit this box ({}).",
            crate::community::fits(p.chips, &p.hardware).1
        )),
        Some(_) => Ok(()),
    }
}

/// "4 chips · low latency": the chips, and what the profile is for.
pub fn profile_label(p: &Profile, siblings: &[Profile]) -> String {
    let kind = if p.name.ends_with("-throughput") {
        "throughput".to_string()
    } else if siblings
        .iter()
        .any(|s| s.name == format!("{}-throughput", p.name))
    {
        "low latency".to_string()
    } else {
        p.name.clone()
    };
    match p.chips {
        Some(1) => format!("1 chip · {kind}"),
        Some(n) => format!("{n} chips · {kind}"),
        None => kind,
    }
}

/// Prefix caching, as far as a profile's launch says (a DFlash profile, by its name).
fn profile_cache(p: &Profile) -> PrefixCache {
    let flags = cache_from_flags(&p.args);
    if flags.state == Cache::Off {
        return flags;
    }
    if p.name.to_lowercase().contains("dflash") {
        return PrefixCache::new(
            Cache::Off,
            "its DFlash speculative decoding can't use a prefix cache",
        );
    }
    flags
}

/// A package's choices: one per profile that fits, if it's a chat model with
/// its weights here. `chat` and `weights` say that for its weights repo.
pub fn community_options(
    id: &str,
    entry: &Value,
    manifest: &Value,
    chat: impl Fn(&str) -> Result<(), &'static str>,
    weights: impl Fn(&str) -> bool,
) -> Vec<Opt> {
    let Some(repo) = manifest["weights"]["repo_id"].as_str() else {
        return vec![];
    };
    if chat(repo).is_err() || !weights(repo) {
        return vec![];
    }
    let kind = manifest["container"]["kind"].as_str().unwrap_or("");
    let all = profiles(manifest, entry);
    all.iter()
        .filter(|p| p.fits)
        .map(|p| {
            let mut o = Opt::base(
                Source::Community,
                format!("community:{id}#{}", p.name),
                repo,
                id.to_string(),
            );
            o.profile = Some(p.name.clone());
            o.profile_label = Some(profile_label(p, &all));
            o.profile_note = p.description.clone();
            o.chips = p.chips;
            o.context = p.context;
            o.tools = match &p.tool_parser {
                Some(t) => Tools {
                    verified: true,
                    how: format!("tool parser {t}"),
                },
                None if kind == "vllm-plugin" => Tools {
                    verified: false,
                    how: "its package doesn't name a tool parser".into(),
                },
                None => Tools {
                    verified: false,
                    how: format!(
                        "its package doesn't say (a {} server)",
                        if kind.is_empty() { "custom" } else { kind }
                    ),
                },
            };
            o.prefix_cache = profile_cache(p);
            o.setup = Some(format!("{id}#{}", p.name));
            o.serve = serve_call(Source::Community, id, Some(&p.name));
            o
        })
        .collect()
}

// ---------------------------------------------------------------- native vLLM bundles

/// A bundle's choice (`bundle` as `native_vllm::bundles` lists it, `meta` its
/// `vllm_metadata.json`), if it's ready to serve a chat model whose weights are here.
pub fn native_option(
    bundle: &Value,
    meta: &Value,
    chat: impl Fn(&str) -> Result<(), &'static str>,
    weights: impl Fn(&str) -> bool,
) -> Option<Opt> {
    if bundle["ready"] != true {
        return None;
    }
    let repo = bundle["weights"].as_str()?;
    if chat(repo).is_err() || !weights(repo) {
        return None;
    }
    let (argv, env) = crate::native_vllm::serve_argv(meta, 0)?;
    let id = bundle["id"].as_str().unwrap_or(repo);
    let dir = bundle["dir"].as_str()?;
    let model = bundle["model"].as_str().unwrap_or(repo);
    let mut o = Opt::base(
        Source::Native,
        format!("native:{id}"),
        model,
        id.to_string(),
    );
    o.group = short(repo).to_lowercase();
    o.chips = env
        .iter()
        .find(|(k, _)| k == "MESH_DEVICE")
        .and_then(|(_, v)| chips_for(v));
    o.context = flag_value(&argv, "--max-model-len").and_then(|v| v.parse().ok());
    o.tools = match flag_value(&argv, "--tool-call-parser") {
        Some(t) => Tools {
            verified: true,
            how: format!("tool parser {t}"),
        },
        None => Tools {
            verified: false,
            how: "its launch command doesn't name a tool parser".into(),
        },
    };
    o.prefix_cache = cache_from_flags(&argv);
    o.serve = serve_call(Source::Native, dir, None);
    Some(o)
}

// ---------------------------------------------------------------- the tt catalog

/// A catalog model's choice: supported here, an LLM or VLM with weights on the
/// box and a tool-call parser (the catalog knows: none means none). `chat`
/// rules one out only from files on the box (a base checkpoint's missing chat
/// template), never for want of them.
pub fn catalog_option(
    m: &crate::ttcli::CatalogModel,
    mesh_chips: Option<u64>,
    chat: impl Fn(&str) -> Result<(), &'static str>,
) -> Option<Opt> {
    if !m.supported || !matches!(m.model_type.as_str(), "llm" | "vlm") || !m.weights.ready() {
        return None;
    }
    let parser = m.tool_call_parser.as_ref()?;
    if not_chat_by_name(&m.name).is_some() {
        return None;
    }
    if m.weights != crate::ttcli::Weights::Container {
        if let Some(repo) = &m.hf_repo {
            if chat(repo).is_err_and(|why| why != NO_CONFIG) {
                return None;
            }
        }
    }
    let mut o = Opt::base(
        Source::Catalog,
        format!("catalog:{}", m.name),
        &m.name,
        "tt catalog".into(),
    );
    o.group = short(m.hf_repo.as_deref().unwrap_or(&m.name)).to_lowercase();
    o.chips = mesh_chips;
    o.context = m.max_context;
    o.params_b = m.param_count;
    o.tools = Tools {
        verified: true,
        how: format!("tool parser {parser}"),
    };
    o.serve = serve_call(Source::Catalog, &m.name, None);
    Some(o)
}

// ---------------------------------------------------------------- the list

/// What's known of the community package serving now: (package, profile).
fn serving_community(state: &AppState, life: &crate::routes::Lifecycle) -> Option<String> {
    let id = crate::community::serving_package(state, life)?;
    let profile = crate::codespeed::tt_model_serve(&crate::codespeed::cmdlines())
        .filter(|(i, _)| *i == id)
        .and_then(|(_, p)| p)
        .or_else(|| package_model(&id).and_then(|(_, d)| d));
    Some(match profile {
        Some(p) => format!("community:{id}#{p}"),
        None => format!("community:{id}#"),
    })
}

/// Which choice Code runs on now: the exact setup when a community package
/// serves, else the first choice serving that model.
pub fn in_use_index(opts: &[Opt], code_model: Option<&str>, key: Option<&str>) -> Option<usize> {
    let model = code_model?;
    if let Some(k) = key {
        if let Some(i) = opts.iter().position(|o| o.key == k) {
            return Some(i);
        }
        // A package serving in a profile the list doesn't offer: none of its others.
        if k.starts_with("community:") {
            return None;
        }
    }
    opts.iter()
        .position(|o| o.source != Source::Community && same_model(&o.model, model))
}

/// Order: what's in use, then verified tool calling, then unverified; by model,
/// then source, then chips.
pub fn sort(opts: &mut [Value]) {
    let rank = |o: &Value| {
        if o["choice"]["action"] == "in_use" {
            0
        } else if o["tools"]["verified"] == true {
            1
        } else {
            2
        }
    };
    let src = |o: &Value| match o["source"].as_str() {
        Some("running") => 0,
        Some("community") => 1,
        Some("native") => 2,
        _ => 3,
    };
    opts.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a["group"].as_str().cmp(&b["group"].as_str()))
            .then_with(|| src(a).cmp(&src(b)))
            .then_with(|| a["origin"].as_str().cmp(&b["origin"].as_str()))
            .then_with(|| a["chips"].as_u64().cmp(&b["chips"].as_u64()))
            .then_with(|| a["profile"].as_str().cmp(&b["profile"].as_str()))
    });
}

/// The latest finished check-up's agent reliability for `model` in `setup`.
/// A check-up from before setups were recorded stands in for a setup with
/// none of its own, and says so.
pub fn reliability(
    evals: &[crate::evals::Eval],
    model: &str,
    setup: Option<&str>,
) -> Option<Value> {
    let done = |e: &&crate::evals::Eval| {
        e.status == "done" && e.reliability.is_some() && same_model(&e.model, model)
    };
    let pick = evals
        .iter()
        .filter(done)
        .filter(|e| e.setup_recorded && e.setup.as_deref() == setup)
        .max_by_key(|e| e.started)
        .or_else(|| {
            evals
                .iter()
                .filter(done)
                .filter(|e| !e.setup_recorded)
                .max_by_key(|e| e.started)
        })?;
    let r = pick.reliability.as_ref()?;
    Some(json!({
        "window": r.recommended_window,
        "valid": r.rows.iter().map(|x| x.within_1_retry).sum::<usize>(),
        "trials": r.rows.iter().map(|x| x.trials).sum::<usize>(),
        "at": pick.finished.unwrap_or(pick.started),
        "setup_unrecorded": !pick.setup_recorded,
    }))
}

/// Every choice from every source on the box (slow-ish: reads tt-model's
/// index and manifests, the bundles and the Hugging Face cache).
fn gather(
    state: &AppState,
    catalog: Option<&crate::ttcli::Catalog>,
    tt_hf: Option<&Path>,
) -> Vec<Opt> {
    let mut out = Vec::new();
    let weights = |r: &str| crate::native_vllm::weights_complete(r).0;
    let db = tt_model_db();
    for (id, entry) in db.as_object().into_iter().flatten() {
        out.extend(community_options(
            id,
            entry,
            &read_manifest(entry),
            chat_llm,
            weights,
        ));
    }
    for b in crate::native_vllm::bundles(state) {
        let meta: Value = b["dir"]
            .as_str()
            .and_then(|d| std::fs::read(Path::new(d).join("vllm_metadata.json")).ok())
            .and_then(|x| serde_json::from_slice(&x).ok())
            .unwrap_or(Value::Null);
        out.extend(native_option(&b, &meta, chat_llm, weights));
    }
    let mesh_chips = state.device_mesh().and_then(chips_for);
    // The catalog's weights are where tt keeps them.
    let hub = tt_hf
        .and_then(|h| {
            crate::ttcli::hf_repo_dir(h, "x/x")
                .parent()
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(crate::download::hub_dir);
    for m in catalog.map(|c| c.models.as_slice()).unwrap_or_default() {
        out.extend(catalog_option(m, mesh_chips, |r| chat_llm_at(&hub, r)));
    }
    out
}

/// `GET /code/models`.
pub(crate) async fn get_models(State(state): State<AppState>, _auth: BearerAuth) -> Json<Value> {
    crate::codespeed::attach(&state);
    let endpoint = crate::gateway::llm_endpoint(&state, None, true).await;
    let life = state.lifecycle();
    let device = state.device_mesh().map(str::to_string);
    let catalog = match state.tt_cli() {
        Some(tt) => tokio::task::spawn_blocking(move || {
            tt.catalog(device.as_deref(), false)
                .map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string())),
        None => Err("The box has no model catalog (the tt CLI isn't set up).".to_string()),
    };
    let code_model = endpoint.as_ref().map(|e| e.model.clone());
    let changing = matches!(life.phase, Phase::Starting | Phase::Stopping)
        .then(|| life.model.clone())
        .flatten();
    let serving = Serving {
        code_model: code_model.as_deref(),
        phase: life.phase,
        changing: changing.as_deref(),
        holder: state.chips_holder(),
    };
    let (opts, in_use_key, evals) = {
        let st = state.clone();
        let tt_hf = state.tt_cli().map(|t| t.hf_cache().to_path_buf());
        let cat = catalog.as_ref().ok().cloned();
        let life = life.clone();
        tokio::task::spawn_blocking(move || {
            (
                gather(&st, cat.as_ref(), tt_hf.as_deref()),
                serving_community(&st, &life),
                crate::evals::all(&st),
            )
        })
        .await
        .unwrap_or_default()
    };
    let in_use = in_use_index(&opts, code_model.as_deref(), in_use_key.as_deref());
    let tt_model = home().join(".local/bin/tt-model").is_file();

    let mut models: Vec<Value> = Vec::new();
    // Code's model by a path the list doesn't know: still shown, from the server itself.
    if let (Some(e), None) = (&endpoint, in_use) {
        let mut o = Opt::base(
            Source::Running,
            format!("running:{}", e.model),
            &e.model,
            "serving now".into(),
        );
        o.tools = Tools {
            verified: false,
            how: "it isn't one of the box's listed models".into(),
        };
        o.setup = crate::codespeed::current_setup(&e.model);
        models.push(describe(&state, &o, Choice::InUse, &evals, Some(e)).await);
    }
    for (i, o) in opts.iter().enumerate() {
        let live = (Some(i) == in_use).then_some(()).and(endpoint.as_ref());
        let choice = if live.is_some() {
            Choice::InUse
        } else if o.source == Source::Community && !tt_model {
            Choice::Unavailable("tt-model (tt-model-manager) isn't installed on this box.".into())
        } else {
            switch_now(
                &Candidate {
                    name: &o.model,
                    on_box: true,
                    tools: true,
                },
                &Serving {
                    code_model: None,
                    ..serving
                },
            )
        };
        models.push(describe(&state, o, choice, &evals, live).await);
    }
    sort(&mut models);
    Json(json!({
        "code_model": code_model,
        "serve": { "phase": life.phase, "model": life.model, "since": life.since, "error": life.error,
                   "eta_s": (life.phase == Phase::Starting).then(|| life.model.as_deref().and_then(|m| state.load_times().estimate(m))).flatten() },
        "catalog_error": catalog.err(),
        "models": models,
    }))
}

/// A choice as the picker shows it: what the box knows of it, and what choosing it does.
async fn describe(
    state: &AppState,
    o: &Opt,
    choice: Choice,
    evals: &[crate::evals::Eval],
    live: Option<&libttstation::model::Endpoint>,
) -> Value {
    let setup = o.setup.as_deref();
    let (context, cache) = match live {
        Some(e) => (
            crate::codespeed::served_context(&e.model, &e.base_url)
                .await
                .or(o.context),
            crate::codespeed::prefix_cache(&e.model, &e.base_url).await,
        ),
        None => (
            o.context,
            match crate::codespeed::remembered_cache(&o.model, setup) {
                Some(c) => c,
                None => o.prefix_cache.clone(),
            },
        ),
    };
    let served = live
        .map(|e| e.model.clone())
        .unwrap_or_else(|| o.model.clone());
    let mut v = serde_json::to_value(o).unwrap_or(Value::Null);
    v["context"] = json!(context);
    v["prefix_cache"] = json!(cache);
    v["served_id"] = json!(live.map(|e| e.model.clone()));
    v["speed"] = json!(
        crate::codespeed::measured_in(&served, setup).map(|m| crate::codespeed::speed_json(&m))
    );
    v["reliability"] = reliability(evals, &served, setup).unwrap_or(Value::Null);
    v["eta_s"] = json!(state.load_times().estimate(&o.model));
    v["choice"] = json!(choice);
    if matches!(choice, Choice::InUse) {
        v["serve"] = Value::Null;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpt_oss_manifest() -> Value {
        // The gpt-oss-20b package on this box, trimmed to what's read.
        let p = |name: &str, hw: &str, mesh: &str, dev: &str, what: &str| {
            json!({ "name": name, "hardware": hw, "mesh_device": mesh, "max_model_len": null,
                    "capabilities": null, "args": [], "env": { "TT_METAL_VISIBLE_DEVICES": dev },
                    "description": format!("{what} profile; full 131072-token context, up to 32 sequences.") })
        };
        json!({
            "device_count": 4,
            "weights": { "repo_id": "openai/gpt-oss-20b" },
            "container": {
                "kind": "vllm-plugin",
                "serve": { "hardware": null, "max_model_len": 131072,
                           "capabilities": { "tool_parser": "openai" }, "args": [] },
                "serve_profiles": [
                    p("p150", "p150", "P150", "0", "Interactive"),
                    p("p150-throughput", "p150", "P150", "0", "Throughput"),
                    p("p150x2", "p150x2", "P150x2", "0,1", "Interactive"),
                    p("p150x2-throughput", "p150x2", "P150x2", "0,1", "Throughput"),
                    p("p150x4", "p150x4", "P150x4", "0,1,2,3", "Interactive"),
                    p("p150x4-throughput", "p150x4", "P150x4", "0,1,2,3", "Throughput"),
                    p("p150x8", "p150x8", "P150x8", "0,1,2,3,4,5,6,7", "Too big"),
                ],
                "default_profile": "p150x2-throughput",
            },
        })
    }

    const OK: fn(&str) -> Result<(), &'static str> = |_| Ok(());
    const HERE: fn(&str) -> bool = |_| true;

    #[test]
    fn each_profile_that_fits_is_its_own_choice() {
        let m = gpt_oss_manifest();
        let opts = community_options("tt-hous/gpt-oss-20b-p150", &json!({}), &m, OK, HERE);
        let names: Vec<_> = opts.iter().map(|o| o.profile.clone().unwrap()).collect();
        assert_eq!(
            names,
            [
                "p150",
                "p150-throughput",
                "p150x2",
                "p150x2-throughput",
                "p150x4",
                "p150x4-throughput"
            ],
            "p150x8 needs 8 chips: left out"
        );
        let x4 = &opts[4];
        assert_eq!(x4.profile_label.as_deref(), Some("4 chips · low latency"));
        assert_eq!(
            opts[5].profile_label.as_deref(),
            Some("4 chips · throughput")
        );
        assert_eq!(
            opts[0].profile_label.as_deref(),
            Some("1 chip · low latency")
        );
        assert_eq!(x4.chips, Some(4));
        assert_eq!(x4.context, Some(131_072));
        assert_eq!(x4.name, "gpt-oss-20b");
        assert_eq!(x4.group, "gpt-oss-20b");
        assert!(x4.tools.verified);
        assert_eq!(x4.tools.how, "tool parser openai");
        assert_eq!(x4.key, "community:tt-hous/gpt-oss-20b-p150#p150x4");
        assert_eq!(x4.setup.as_deref(), Some("tt-hous/gpt-oss-20b-p150#p150x4"));
        // Switching: the package's own serve route, with that profile.
        assert_eq!(
            x4.serve,
            Some(ServeCall {
                route: "/community/serve",
                body: json!({ "id": "tt-hous/gpt-oss-20b-p150", "profile": "p150x4", "stop_serving": true }),
            })
        );
    }

    #[test]
    fn a_package_that_wouldnt_work_for_code_is_left_out() {
        let m = gpt_oss_manifest();
        let id = "tt-hous/gpt-oss-20b-p150";
        assert!(
            community_options(id, &json!({}), &m, |_| Err("no chat template"), HERE).is_empty()
        );
        assert!(
            community_options(id, &json!({}), &m, OK, |_| false).is_empty(),
            "weights not here"
        );
        assert!(
            community_options(id, &json!({}), &json!({}), OK, HERE).is_empty(),
            "no weights named"
        );
    }

    #[test]
    fn unknown_tool_calling_is_listed_but_unverified() {
        // DeAIafdeling's Flash Next package: no capabilities in its manifest.
        let m = json!({
            "device_count": 4, "weights": { "repo_id": "Qwen/Qwen3.8-Flash-Next" },
            "container": { "kind": "vllm-plugin", "serve": { "hardware": null, "capabilities": null,
                "args": ["--hf-overrides", "{}"] },
                "serve_profiles": [
                    { "name": "qb2-256k", "hardware": "p300x2", "max_model_len": 262080, "args": [] },
                    { "name": "qb2-131k", "hardware": "p300x2", "max_model_len": 131008, "args": [] },
                ] },
        });
        let opts = community_options(
            "DeAIafdeling/qwen38-flash-next-qb2-256k",
            &json!({}),
            &m,
            OK,
            HERE,
        );
        assert_eq!(opts.len(), 2);
        assert!(!opts[0].tools.verified);
        assert!(opts[0].tools.how.contains("doesn't name a tool parser"));
        assert_eq!(opts[0].context, Some(262_080));
        assert_eq!(opts[0].profile_label.as_deref(), Some("4 chips · qb2-256k"));
        // A non-vLLM server says what it is.
        let dit = json!({ "device_count": 4, "weights": { "repo_id": "Qwen/Qwen3.8-Flash-Next" },
            "container": { "kind": "tt-dit-server", "serve": { "hardware": "p150x4" } } });
        let o = community_options(
            "sjettTT/flash",
            &json!({ "profile": "c32k" }),
            &dit,
            OK,
            HERE,
        );
        assert_eq!(o.len(), 1);
        assert_eq!(
            o[0].profile.as_deref(),
            Some("c32k"),
            "no profiles listed: its default"
        );
        assert!(o[0].tools.how.contains("tt-dit-server"));
    }

    #[test]
    fn a_profiles_launch_says_whether_it_caches() {
        // changh95's Qwen3.8-27B: prefill/decode split turns caching off; DFlash can't use it.
        let m = json!({ "device_count": 2, "weights": { "repo_id": "Qwen/Qwen3.8-27B" },
            "container": { "kind": "vllm-plugin", "serve": { "hardware": "p150x2",
                "capabilities": { "tool_parser": "qwen3_coder" } },
                "serve_profiles": [
                    { "name": "tp2", "max_model_len": 65536, "args": [["--max-num-batched-tokens", "65536"]] },
                    { "name": "p1d1", "args": [["--max-num-batched-tokens", "65536"], "--no-enable-prefix-caching"] },
                    { "name": "tp2-dflash2", "args": ["--no-async-scheduling"] },
                ] } });
        let o = community_options("changh95/qwen3.8-27b-p150x2", &json!({}), &m, OK, HERE);
        assert_eq!(o[0].prefix_cache.state, Cache::Unknown);
        assert_eq!(o[0].chips, Some(2));
        assert_eq!(o[1].prefix_cache.state, Cache::Off);
        assert_eq!(o[2].prefix_cache.state, Cache::Off);
        assert!(o[2].prefix_cache.how.contains("DFlash"));
    }

    #[test]
    fn chips_follow_the_board_layout() {
        assert_eq!(chips_for("p150"), Some(1));
        assert_eq!(chips_for("P150x4"), Some(4));
        assert_eq!(chips_for("p300"), Some(2));
        assert_eq!(chips_for("p300x2"), Some(4));
        assert_eq!(chips_for("qb2"), Some(4));
        assert_eq!(chips_for("p150x8"), Some(8));
        assert_eq!(chips_for("galaxy"), None);
        assert_eq!(chips_for(""), None);
    }

    #[test]
    fn a_native_bundle_starts_through_its_own_route() {
        let bundle = json!({ "id": "mando2222/gemma-4-26b-a4b-it-QB2", "dir": "/b/mando2222__gemma",
            "model": "google/gemma-4-26B-A4B-it", "weights": "google/gemma-4-26B-A4B-it", "ready": true });
        let meta = json!({ "launch": { "blackhole": { "command": ["python", "-m", "vllm.entrypoints.openai.api_server",
            "--model", "google/gemma-4-26B-A4B-it", "--max-model-len", "65536",
            "--enable-auto-tool-choice", "--tool-call-parser", "gemma4", "--enable-prefix-caching"],
            "env": { "MESH_DEVICE": "P300x2" } } } });
        let o = native_option(&bundle, &meta, OK, HERE).unwrap();
        assert_eq!(o.source, Source::Native);
        assert_eq!(o.chips, Some(4));
        assert_eq!(o.context, Some(65_536));
        assert!(o.tools.verified);
        assert_eq!(o.prefix_cache.state, Cache::On);
        assert_eq!(o.group, "gemma-4-26b-a4b-it");
        assert_eq!(
            o.serve,
            Some(ServeCall {
                route: "/vllm/native/serve",
                body: json!({ "dir": "/b/mando2222__gemma", "stop_serving": true })
            })
        );
        // No parser named: still listed, unverified.
        let bare = json!({ "launch": { "default": { "command": ["vllm", "serve", "google/gemma-4-26B-A4B-it"] } } });
        assert!(
            !native_option(&bundle, &bare, OK, HERE)
                .unwrap()
                .tools
                .verified
        );
        // Not ready (no code, no launch for this box), not a chat model, or weights missing: left out.
        let not_ready = json!({ "id": "x", "dir": "/b/x", "weights": "g/m", "ready": false });
        assert!(native_option(&not_ready, &meta, OK, HERE).is_none());
        assert!(native_option(&bundle, &meta, |_| Err("base"), HERE).is_none());
        assert!(native_option(&bundle, &meta, OK, |_| false).is_none());
    }

    fn cat(name: &str, model_type: &str, parser: Option<&str>) -> crate::ttcli::CatalogModel {
        crate::ttcli::CatalogModel {
            name: name.into(),
            hf_repo: Some(format!("org/{name}")),
            model_type: model_type.into(),
            status: Some("COMPLETE".into()),
            supported: true,
            engines: vec!["vllm".into()],
            tool_call_parser: parser.map(str::to_string),
            cached: true,
            cache_size_bytes: 0,
            min_disk_gb: None,
            param_count: Some(32.0),
            max_context: Some(40_960),
            weights: crate::ttcli::Weights::Complete,
            downloaded_bytes: 0,
        }
    }

    #[test]
    fn a_catalog_model_starts_through_serve() {
        let o = catalog_option(&cat("Qwen3-32B", "llm", Some("hermes")), Some(4), OK).unwrap();
        assert_eq!(
            o.serve,
            Some(ServeCall {
                route: "/serve",
                body: json!({ "model": "Qwen3-32B", "force": true })
            })
        );
        assert_eq!(o.chips, Some(4));
        assert!(o.tools.verified);
        // The catalog knows tool calling: none named means none.
        assert!(catalog_option(&cat("Llama-3.1-8B", "llm", None), Some(4), OK).is_none());
        assert!(catalog_option(&cat("FLUX.1-dev", "image", Some("x")), Some(4), OK).is_none());
        assert!(
            catalog_option(&cat("Qwen3-Embedding-8B", "llm", Some("x")), Some(4), OK).is_none()
        );
        assert!(
            catalog_option(&cat("Llama-3.1-70B-Base", "llm", Some("x")), Some(4), OK).is_none()
        );
        assert!(
            catalog_option(&cat("Qwen3-32B", "llm", Some("x")), Some(4), |_| Err(
                "base"
            ))
            .is_none()
        );
        // Nothing on the box to judge by: the catalog's word stands.
        assert!(
            catalog_option(&cat("Qwen3-32B", "llm", Some("x")), Some(4), |_| Err(
                NO_CONFIG
            ))
            .is_some()
        );
        let mut partial = cat("Qwen3-32B", "llm", Some("x"));
        partial.weights = crate::ttcli::Weights::Partial;
        assert!(catalog_option(&partial, Some(4), OK).is_none());
    }

    #[test]
    fn chat_models_have_a_causal_lm_and_a_chat_template() {
        let d = tempfile::tempdir().unwrap();
        let snap = d.path().join("s1");
        std::fs::create_dir_all(&snap).unwrap();
        assert_eq!(
            chat_llm_in(std::slice::from_ref(&snap)),
            Err("its config isn't on the box")
        );
        std::fs::write(
            snap.join("config.json"),
            r#"{"architectures":["LayaTypedDecisions"]}"#,
        )
        .unwrap();
        assert_eq!(
            chat_llm_in(std::slice::from_ref(&snap)),
            Err("not a language model")
        );
        std::fs::write(
            snap.join("config.json"),
            r#"{"architectures":["GptOssForCausalLM"]}"#,
        )
        .unwrap();
        assert!(chat_llm_in(std::slice::from_ref(&snap))
            .unwrap_err()
            .contains("base checkpoint"));
        std::fs::write(
            snap.join("tokenizer_config.json"),
            r#"{"chat_template":"{{ x }}"}"#,
        )
        .unwrap();
        assert_eq!(chat_llm_in(std::slice::from_ref(&snap)), Ok(()));
        // The template in its own file, in another snapshot of the same repo.
        std::fs::remove_file(snap.join("tokenizer_config.json")).unwrap();
        let other = d.path().join("s2");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("chat_template.jinja"), "{{ x }}").unwrap();
        assert_eq!(chat_llm_in(&[snap, other]), Ok(()));
        assert_eq!(
            not_chat_by_name("Qwen/Qwen3-Embedding-8B"),
            Some("an embedding model")
        );
        assert_eq!(
            not_chat_by_name("org/Model-7B-Base"),
            Some("a base checkpoint")
        );
        assert_eq!(not_chat_by_name("openai/gpt-oss-20b"), None);
    }

    fn opt(key: &str, source: Source, model: &str) -> Opt {
        Opt::base(source, key.into(), model, String::new())
    }

    #[test]
    fn the_exact_setup_in_use_is_marked() {
        let opts = vec![
            opt(
                "community:tt-hous/gpt-oss-20b-p150#p150x2",
                Source::Community,
                "openai/gpt-oss-20b",
            ),
            opt(
                "community:tt-hous/gpt-oss-20b-p150#p150x2-throughput",
                Source::Community,
                "openai/gpt-oss-20b",
            ),
            opt("catalog:gpt-oss-20b", Source::Catalog, "gpt-oss-20b"),
        ];
        let key = Some("community:tt-hous/gpt-oss-20b-p150#p150x2-throughput");
        assert_eq!(
            in_use_index(&opts, Some("openai/gpt-oss-20b"), key),
            Some(1)
        );
        // Served some other way: the catalog's choice of that model, never a package profile.
        assert_eq!(
            in_use_index(&opts, Some("openai/gpt-oss-20b"), None),
            Some(2)
        );
        // A package profile the list doesn't offer: nothing else is "in use".
        assert_eq!(
            in_use_index(&opts, Some("openai/gpt-oss-20b"), Some("community:x#y")),
            None
        );
        assert_eq!(in_use_index(&opts, None, key), None);
    }

    #[test]
    fn in_use_comes_first_then_verified_then_unverified() {
        let mk = |key: &str, group: &str, action: &str, verified: bool, chips: u64| {
            json!({ "key": key, "group": group, "source": "community", "origin": "o", "chips": chips,
                    "choice": { "action": action }, "tools": { "verified": verified } })
        };
        let mut v = vec![
            mk("a", "aaa", "switch", false, 4),
            mk("b", "zzz", "switch", true, 4),
            mk("c", "mmm", "in_use", true, 2),
            mk("d", "bbb", "switch", true, 4),
            mk("e", "bbb", "busy", true, 1),
        ];
        sort(&mut v);
        let keys: Vec<_> = v.iter().map(|o| o["key"].as_str().unwrap()).collect();
        assert_eq!(keys, ["c", "e", "d", "b", "a"]);
    }

    #[test]
    fn reliability_is_per_setup() {
        use crate::evals::Eval;
        let report = |window| crate::reliability::Report {
            model: "openai/gpt-oss-20b".into(),
            context: None,
            histories: 1,
            tasks: 1,
            seed: 1,
            rows: vec![],
            recommended_window: Some(window),
            rule: String::new(),
            retries: 1,
            raw_window: None,
            raw_rule: String::new(),
        };
        let e = |started, setup: Option<&str>, recorded, window| Eval {
            id: format!("e{started}"),
            model: "openai/gpt-oss-20b".into(),
            started,
            status: "done".into(),
            reliability: Some(report(window)),
            setup: setup.map(str::to_string),
            setup_recorded: recorded,
            ..Default::default()
        };
        let evals = vec![e(1, None, false, 65_536), e(2, Some("p#x4"), true, 98_304)];
        assert_eq!(
            reliability(&evals, "gpt-oss-20b", Some("p#x4")).unwrap()["window"],
            98_304
        );
        let other = reliability(&evals, "gpt-oss-20b", Some("p#x2")).unwrap();
        assert_eq!(
            other["window"], 65_536,
            "an older, unlabelled run stands in"
        );
        assert_eq!(other["setup_unrecorded"], true);
        assert!(reliability(&evals, "Qwen3-32B", None).is_none());
    }
}
