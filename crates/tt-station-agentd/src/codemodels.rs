// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The models Code can run on: the `tt` catalog's (`crate::ttcli`), started
//! with `POST /serve`, plus whatever is serving now by another path.
//!
//! There's no default to pick here: people choose any model that would work.
//! One is listed when it would work for Code: a chat (instruct) LLM, not a
//! base checkpoint, a media model or an embedder; its weights on the box; and
//! tool calling on its serving path.
//!
//! Each choice carries what's known about it here: window, chips, prefix
//! caching, the speed Code measured (`crate::codespeed`'s book), the latest
//! check-up's agent reliability (`crate::evals`), and what choosing it does.
//! Switching is the client calling the serve route (`serve` in each choice),
//! so the roles, settings, leases and refusals of that route apply exactly as
//! they do from Models.
//!
//! - `GET /code/models` (authed).

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use crate::codespeed::{same_model, switch_now, Cache, Candidate, Choice, PrefixCache, Serving};
use crate::routes::{AppState, BearerAuth, Phase};

// ---------------------------------------------------------------- one choice

/// Where a choice is served from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Catalog,
    /// Serving now by some path this list doesn't know (shown so Code's model is there).
    Running,
}

/// Whether the model makes tool calls on this serving path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tools {
    pub verified: bool,
    /// How we know, or why we don't: "tool parser openai".
    pub how: String,
}

/// The route and body that start a choice.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ServeCall {
    pub route: &'static str,
    pub body: Value,
}

/// One model (in one setup) the picker offers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Opt {
    /// Unique in the list: `catalog:<name>`, or `running:<model>`.
    pub key: String,
    /// The model it serves, the same for all its setups (the picker groups by it).
    pub group: String,
    /// The model's name to show.
    pub name: String,
    /// The served model (its Hugging Face repo, or the catalog name).
    pub model: String,
    pub source: Source,
    /// Where it comes from, to show: "tt catalog", "serving now".
    pub origin: String,
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
pub fn serve_call(source: Source, id: &str) -> Option<ServeCall> {
    match source {
        Source::Catalog => Some(ServeCall {
            route: "/serve",
            body: json!({ "model": id, "force": true }),
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
    o.serve = serve_call(Source::Catalog, &m.name);
    Some(o)
}

// ---------------------------------------------------------------- the list

/// Which choice Code runs on now: the first serving that model.
pub fn in_use_index(opts: &[Opt], code_model: Option<&str>) -> Option<usize> {
    let model = code_model?;
    opts.iter().position(|o| same_model(&o.model, model))
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
        _ => 1,
    };
    opts.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a["group"].as_str().cmp(&b["group"].as_str()))
            .then_with(|| src(a).cmp(&src(b)))
            .then_with(|| a["origin"].as_str().cmp(&b["origin"].as_str()))
            .then_with(|| a["chips"].as_u64().cmp(&b["chips"].as_u64()))
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

/// Every catalog choice (slow-ish: reads the Hugging Face cache).
fn gather(
    state: &AppState,
    catalog: Option<&crate::ttcli::Catalog>,
    tt_hf: Option<&Path>,
) -> Vec<Opt> {
    let mut out = Vec::new();
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
    let endpoint = crate::gateway::chat_endpoint(&state).await;
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
    };
    let (opts, evals) = {
        let st = state.clone();
        let tt_hf = state.tt_cli().map(|t| t.hf_cache().to_path_buf());
        let cat = catalog.as_ref().ok().cloned();
        tokio::task::spawn_blocking(move || {
            (
                gather(&st, cat.as_ref(), tt_hf.as_deref()),
                crate::evals::all(&st),
            )
        })
        .await
        .unwrap_or_default()
    };
    let in_use = in_use_index(&opts, code_model.as_deref());

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
        models.push(describe(&state, &o, Choice::InUse, &evals, Some(e)).await);
    }
    for (i, o) in opts.iter().enumerate() {
        let live = (Some(i) == in_use).then_some(()).and(endpoint.as_ref());
        let choice = if live.is_some() {
            Choice::InUse
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

    const OK: fn(&str) -> Result<(), &'static str> = |_| Ok(());

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
    fn the_model_serving_now_is_marked() {
        let opts = vec![
            opt("catalog:Qwen3-32B", Source::Catalog, "Qwen3-32B"),
            opt("catalog:gpt-oss-20b", Source::Catalog, "gpt-oss-20b"),
        ];
        assert_eq!(in_use_index(&opts, Some("openai/gpt-oss-20b")), Some(1));
        assert_eq!(in_use_index(&opts, Some("Llama-3.1-8B")), None);
        assert_eq!(in_use_index(&opts, None), None);
    }

    #[test]
    fn in_use_comes_first_then_verified_then_unverified() {
        let mk = |key: &str, group: &str, action: &str, verified: bool, chips: u64| {
            json!({ "key": key, "group": group, "source": "catalog", "origin": "o", "chips": chips,
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
