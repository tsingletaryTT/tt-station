// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Apps for the box: open-source tools that run in Docker next to the models,
//! each pre-wired to the box's own OpenAI-compatible endpoint (`/v1`, the
//! gateway) with its own app key, so they work the moment they start.
//!
//! Open WebUI and AnythingLLM come from the official tt CLI (`tt launch`, see
//! `tttools`); these are the rest. Containers are named `tt-app-<id>`, keep
//! their data in a `tt-app-<id>` volume, restart with the box, and are
//! reached at `http://<box>:<port>`. Apps with no login of their own listen
//! only on the box itself.
//!
//! - `GET /apps`: the catalogue and each app's state.
//! - `POST /apps/{id}/start`: install (in the background: pull, key, run) or start.
//! - `POST /apps/{id}/stop`; `DELETE /apps/{id}` (the container; the data stays).

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth};

pub struct AppDef {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub category: &'static str,
    pub image: &'static str,
    /// Host port → container port.
    pub port: u16,
    pub container_port: u16,
    /// Where the app keeps its data inside the container.
    pub data: &'static str,
    /// Environment, with `{base}` (the box's /v1 as the container sees it),
    /// `{key}` (its app key) and `{token}` (a secret for the app's own login).
    pub env: &'static [(&'static str, &'static str)],
    /// Run instead of the image's default command.
    pub command: Option<&'static [&'static str]>,
    /// Only reachable from the box itself (no login of its own).
    pub local_only: bool,
    /// Appended to the URL (e.g. Jupyter's `?token=`).
    pub url_suffix: &'static str,
    pub homepage: &'static str,
    /// Apps it needs running beside it (a research app needs SearXNG); they're
    /// set up first. `{app:<id>}` in `env` is that app's address from inside a container.
    /// `"embeddings"` is the box's embeddings service (`crate::embeddings`), turned
    /// on if it's off; `{embed_dim}` is its vector size.
    pub needs: &'static [&'static str],
    /// Its own login's user name (the password is its `{token}`), when it has one.
    pub login_user: Option<&'static str>,
    /// One thing to do in the app the first time, shown on its tile.
    pub hint: &'static str,
    /// More `docker run` options (e.g. `--shm-size=1g`).
    pub extra_args: &'static [&'static str],
    /// The app is told the box's address and key in its own settings screen,
    /// so its tile shows them (the key is kept for it, 0600).
    pub show_key: bool,
    /// Files written for it (under the agent's config dir, with the same
    /// placeholders as `env`) and mounted into the container: (path inside, contents).
    pub files: &'static [(&'static str, &'static str)],
}

pub const APPS: &[AppDef] = &[
    AppDef {
        id: "lobechat",
        name: "LobeChat",
        blurb: "A polished chat app with plugins, agents and file chat.",
        category: "Chat",
        image: "lobehub/lobe-chat:latest",
        port: 3020,
        container_port: 3210,
        data: "/app/data",
        env: &[
            ("OPENAI_API_KEY", "{key}"),
            ("OPENAI_PROXY_URL", "{base}"),
            ("ACCESS_CODE", "{token}"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://lobehub.com",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "jupyter",
        name: "Jupyter",
        blurb: "Notebooks with an OpenAI client already pointed at the box.",
        category: "Build",
        image: "quay.io/jupyter/minimal-notebook:latest",
        port: 3021,
        container_port: 8888,
        data: "/home/jovyan/work",
        env: &[
            ("OPENAI_BASE_URL", "{base}"),
            ("OPENAI_API_KEY", "{key}"),
            ("JUPYTER_TOKEN", "{token}"),
        ],
        command: Some(&[
            "bash",
            "-c",
            "pip install -q openai && exec start-notebook.py",
        ]),
        local_only: true,
        url_suffix: "/lab?token={token}",
        homepage: "https://jupyter.org",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "n8n",
        name: "n8n",
        blurb: "Automations: wire the box's model into email, calendars, webhooks and more.",
        category: "Automate",
        image: "docker.n8n.io/n8nio/n8n:latest",
        port: 3022,
        container_port: 5678,
        data: "/home/node/.n8n",
        env: &[
            ("N8N_SECURE_COOKIE", "false"),
            ("OPENAI_API_BASE", "{base}"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://n8n.io",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "flowise",
        name: "Flowise",
        blurb: "Build AI agents and RAG flows visually, on the box's model.",
        category: "Build",
        image: "flowiseai/flowise:latest",
        port: 3023,
        container_port: 3000,
        data: "/root/.flowise",
        env: &[
            ("FLOWISE_USERNAME", "admin"),
            ("FLOWISE_PASSWORD", "{token}"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://flowiseai.com",
        needs: &[],
        login_user: Some("admin"),
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "searxng",
        name: "SearXNG",
        blurb: "A private search engine on the box: it asks Google, Bing and others for you, with no account. Research apps search through it.",
        category: "Research",
        image: "searxng/searxng:latest",
        port: 3024,
        container_port: 8080,
        data: "/var/cache/searxng",
        env: &[("SEARXNG_SECRET", "{token}")],
        command: None,
        local_only: true,
        url_suffix: "",
        homepage: "https://docs.searxng.org",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &[],
        show_key: false,
        // Research apps ask for JSON, which SearXNG answers only when it's on.
        files: &[("/etc/searxng/settings.yml", "use_default_settings: true\nserver:\n  secret_key: \"{token}\"\n  limiter: false\n  image_proxy: true\nsearch:\n  formats:\n    - html\n    - json\n")],
    },
    AppDef {
        id: "localdeepresearch",
        name: "Local Deep Research",
        blurb: "Ask a hard question; it plans, searches the web, arXiv and PubMed, reads, and writes a cited report. All on the box.",
        category: "Research",
        image: "localdeepresearch/local-deep-research:latest",
        port: 3025,
        container_port: 5000,
        data: "/data",
        env: &[
            ("LDR_DATA_DIR", "/data"),
            ("LDR_LLM_PROVIDER", "openai_endpoint"),
            ("LDR_LLM_OPENAI_ENDPOINT_URL", "{base}"),
            ("LDR_LLM_OPENAI_ENDPOINT_API_KEY", "{key}"),
            ("LDR_LLM_MODEL", "{model}"),
            ("LDR_SEARCH_ENGINE_WEB_SEARXNG_DEFAULT_PARAMS_INSTANCE_URL", "{app:searxng}"),
            ("LDR_SEARCH_PRIVATE_ENGINE_URL_ALLOWLIST", "{app:searxng}"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://github.com/LearningCircuit/local-deep-research",
        needs: &["searxng"],
        login_user: None,
        hint: "The first time, create your account in it.",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "vane",
        name: "Vane",
        blurb: "An answer engine like Perplexity (formerly Perplexica): quick, cited answers from the web, with its own private search built in.",
        category: "Research",
        image: "itzcrazykns1337/vane:latest",
        port: 3026,
        container_port: 3000,
        data: "/home/vane/data",
        env: &[("OPENAI_BASE_URL", "{base}"), ("OPENAI_API_KEY", "{key}")],
        command: None,
        local_only: true,
        url_suffix: "",
        homepage: "https://github.com/ItzCrazyKns/Vane",
        needs: &[],
        login_user: None,
        hint: "In its first-run setup, choose the OpenAI provider with the address and key below, and add a chat model named {model}.",
        extra_args: &[],
        show_key: true,
        files: &[],
    },
    AppDef {
        id: "comfyui",
        name: "ComfyUI",
        blurb: "Node-based image and video workflows. It runs on the box's CPU (ComfyUI doesn't support Tenstorrent chips yet), so large models are slow; Studio makes FLUX images on the chips.",
        category: "Images",
        image: "yanwk/comfyui-boot:cpu",
        port: 3027,
        container_port: 8188,
        data: "/root",
        env: &[("CLI_ARGS", "--cpu")],
        command: None,
        local_only: true,
        url_suffix: "",
        homepage: "https://www.comfy.org",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "langflow",
        name: "Langflow",
        blurb: "Build agents and RAG pipelines visually, on the box's model, and serve them as APIs.",
        category: "Build",
        image: "langflowai/langflow:latest",
        port: 3028,
        container_port: 7860,
        data: "/app/langflow",
        env: &[
            ("LANGFLOW_CONFIG_DIR", "/app/langflow"),
            ("LANGFLOW_SUPERUSER", "langflow"),
            ("LANGFLOW_SUPERUSER_PASSWORD", "{token}"),
            ("OPENAI_COMPATIBLE_BASE_URL", "{base}"),
            ("OPENAI_COMPATIBLE_API_KEY", "{key}"),
            // It refuses private addresses by default; the box is one.
            ("LANGFLOW_SSRF_ALLOWED_HOSTS", "host.docker.internal,172.16.0.0/12"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://www.langflow.org",
        needs: &[],
        login_user: Some("langflow"),
        hint: "",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "crawl4ai",
        name: "Crawl4AI",
        blurb: "Turns any web page into clean text for AI, with a playground to try it and tools agents can call. Uses the box's model to pull out structured data.",
        category: "Research",
        image: "unclecode/crawl4ai:0.9.4",
        port: 3029,
        container_port: 11235,
        data: "/data",
        env: &[
            ("OPENAI_API_KEY", "{key}"),
            ("LLM_PROVIDER", "openai/{model}"),
            ("LLM_BASE_URL", "{base}"),
        ],
        command: None,
        local_only: true,
        url_suffix: "/playground",
        homepage: "https://docs.crawl4ai.com",
        needs: &[],
        login_user: None,
        hint: "",
        extra_args: &["--shm-size=1g"],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "scrapling",
        name: "Scrapling",
        blurb: "A stealthy web scraper that agents use as a tool (an MCP server): it gets past pages that block simple fetching.",
        category: "Automate",
        image: "pyd4vinci/scrapling:0.4.15",
        port: 3030,
        container_port: 8000,
        data: "/data",
        env: &[("SCRAPLING_MCP_AUTH_TOKEN", "{token}")],
        command: Some(&["mcp", "--http", "--host", "0.0.0.0"]),
        local_only: true,
        url_suffix: "/mcp",
        homepage: "https://github.com/D4Vinci/Scrapling",
        needs: &[],
        login_user: None,
        hint: "It's a tool for agents, not a page to browse: add it to an agent (Hermes, OpenCode, Claude) as an MCP server at the address above, with the access code as its bearer token.",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
    AppDef {
        id: "openhands",
        name: "OpenHands",
        blurb: "An AI software engineer: give it a task in a project and it writes, runs and fixes the code, step by step, where you can watch.",
        category: "Code",
        image: "ghcr.io/openhands/agent-canvas:latest",
        port: 3031,
        container_port: 8000,
        data: "/home/openhands/.openhands",
        env: &[
            ("LOCAL_BACKEND_API_KEY", "{token}"),
            ("OH_SECRET_KEY", "{token}"),
            ("AGENT_CANVAS_DISABLE_TELEMETRY", "1"),
        ],
        command: None,
        local_only: true,
        url_suffix: "/canvas",
        homepage: "https://openhands.dev",
        needs: &[],
        login_user: None,
        hint: "In Settings → LLM → Advanced, set the model to openai/{model} and paste the address and key below. It needs a model with a long memory and reliable tool calls.",
        extra_args: &[],
        show_key: true,
        files: &[],
    },
    AppDef {
        id: "openclaw",
        name: "OpenClaw",
        blurb: "Your own AI assistant that you message from WhatsApp, Telegram, Slack or Discord, running on the box's model.",
        category: "Agents",
        image: "openclaw/openclaw:2026.9.3",
        port: 3032,
        container_port: 18789,
        data: "/home/node/.openclaw",
        env: &[("OPENCLAW_GATEWAY_TOKEN", "{token}"), ("TT_API_KEY", "{key}")],
        command: None,
        local_only: true,
        url_suffix: "",
        homepage: "https://openclaw.ai",
        needs: &[],
        login_user: None,
        hint: "Sign in to its control page with the access code, then connect a chat app (WhatsApp, Telegram, Slack…) under Channels.",
        extra_args: &[],
        show_key: false,
        // The box as its model provider; memory search off (it would need embeddings the box doesn't serve).
        files: &[("/home/node/.openclaw/openclaw.json", "{\n  \"gateway\": { \"mode\": \"local\" },\n  \"models\": { \"providers\": { \"tt\": { \"baseUrl\": \"{base}\", \"apiKey\": \"{key}\", \"api\": \"openai-completions\", \"models\": [ { \"id\": \"quietbox\", \"name\": \"QuietBox\", \"contextWindow\": 131072, \"maxTokens\": 8192 } ] } } },\n  \"agents\": { \"defaults\": { \"model\": { \"primary\": \"tt/quietbox\" } } },\n  \"memory\": { \"search\": { \"provider\": \"none\" } }\n}\n")],
    },
    AppDef {
        id: "lightrag",
        name: "LightRAG",
        blurb: "Upload documents and ask questions: it maps how the facts in them connect (a knowledge graph) and answers from it, with the box's model and embeddings.",
        category: "Research",
        image: "ghcr.io/hkuds/lightrag:v1.5.7",
        port: 3033,
        container_port: 9621,
        data: "/app/data",
        env: &[
            ("LLM_BINDING", "openai"),
            ("LLM_BINDING_HOST", "{base}"),
            ("LLM_BINDING_API_KEY", "{key}"),
            ("LLM_MODEL", "{model}"),
            ("EMBEDDING_BINDING", "openai"),
            ("EMBEDDING_BINDING_HOST", "{base}"),
            ("EMBEDDING_BINDING_API_KEY", "{key}"),
            ("EMBEDDING_MODEL", "embeddings"),
            ("EMBEDDING_DIM", "{embed_dim}"),
            ("AUTH_ACCOUNTS", "admin:{token}"),
            ("TOKEN_SECRET", "{token}"),
        ],
        command: None,
        local_only: false,
        url_suffix: "",
        homepage: "https://github.com/HKUDS/LightRAG",
        needs: &[EMBEDDINGS],
        login_user: Some("admin"),
        hint: "Indexing a document makes many calls to the model, so the first upload takes a while.",
        extra_args: &[],
        show_key: false,
        files: &[],
    },
];

pub fn app(id: &str) -> Option<&'static AppDef> {
    APPS.iter().find(|a| a.id == id)
}

/// In `needs`: the box's embeddings service rather than an app.
const EMBEDDINGS: &str = "embeddings";

/// The Docker network every app joins.
const NETWORK: &str = "tt-apps";

fn container(id: &str) -> String {
    format!("tt-app-{id}")
}

/// The container's state: `missing`, or Docker's (`running`, `exited`, …).
fn docker_state(id: &str) -> String {
    Command::new("docker")
        .args(["inspect", "-f", "{{.State.Status}}", &container(id)])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "missing".into())
}

/// An install in progress (or failed) for each app id.
#[derive(Default)]
pub struct Installs {
    inner: Mutex<HashMap<String, (String, Option<String>)>>, // id → (step, error)
}

impl Installs {
    fn set(&self, id: &str, step: &str, error: Option<String>) {
        self.inner
            .lock()
            .expect("installs")
            .insert(id.into(), (step.into(), error));
    }
    fn clear(&self, id: &str) {
        self.inner.lock().expect("installs").remove(id);
    }
    fn get(&self, id: &str) -> Option<(String, Option<String>)> {
        self.inner.lock().expect("installs").get(id).cloned()
    }
}

static INSTALLS: std::sync::LazyLock<Arc<Installs>> =
    std::sync::LazyLock::new(|| Arc::new(Installs::default()));

/// The app's own secret (Jupyter token, Flowise password, LobeChat access code),
/// kept in its container's environment so it survives restarts.
fn app_token(id: &str) -> Option<String> {
    let out = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{range .Config.Env}}{{println .}}{{end}}",
            &container(id),
        ])
        .output()
        .ok()?;
    let env = String::from_utf8_lossy(&out.stdout).to_string();
    let def = app(id)?;
    let (key, _) = def.env.iter().find(|(_, v)| v.contains("{token}"))?;
    env.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
}

fn url_for(state: &AppState, def: &AppDef, token: Option<&str>) -> String {
    let host = if def.local_only {
        "127.0.0.1".to_string()
    } else {
        crate::pairqr::lan_ipv4()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| format!("{}.local", state.name()))
    };
    format!(
        "http://{host}:{}{}",
        def.port,
        def.url_suffix.replace("{token}", token.unwrap_or(""))
    )
}

fn describe(state: &AppState, def: &AppDef) -> Value {
    let docker = docker_state(def.id);
    let install = INSTALLS.get(def.id);
    // A container that keeps restarting is broken: say so, with what it last said.
    let crash = (docker == "restarting").then(|| {
        Command::new("docker").args(["logs", "--tail", "5", &container(def.id)]).output().ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr))
            .and_then(|t| t.lines().map(str::trim).filter(|l| !l.is_empty()).last().map(|l| format!("It keeps stopping: {}", l.chars().take(240).collect::<String>())))
            .unwrap_or_else(|| "It keeps stopping right after it starts.".into())
    });
    let install = install.or_else(|| crash.map(|c| ("Failed".to_string(), Some(c))));
    let state_word = match (&install, docker.as_str()) {
        (Some((_, Some(_))), _) => "failed",
        (Some(_), _) => "installing",
        (None, "running") => "running",
        (None, "missing") => "not_installed",
        _ => "stopped",
    };
    let token = (docker != "missing").then(|| app_token(def.id)).flatten();
    json!({
        "id": def.id, "name": def.name, "blurb": def.blurb, "category": def.category,
        "port": def.port, "local_only": def.local_only, "homepage": def.homepage,
        "needs": def.needs.iter().filter(|n| **n != EMBEDDINGS).filter_map(|n| app(n)).filter(|n| docker_state(n.id) == "missing").map(|n| n.name).collect::<Vec<_>>(),
        // Running apps that search (or otherwise lean) on this one.
        "used_by": APPS.iter().filter(|a| a.needs.contains(&def.id) && docker_state(a.id) == "running").map(|a| a.name).collect::<Vec<_>>(),
        "state": state_word,
        "step": install.as_ref().map(|(s, _)| s.clone()),
        "error": install.and_then(|(_, e)| e),
        "url": (state_word == "running").then(|| url_for(state, def, token.as_deref())),
        // The app's own login, when it has one (shown so the owner can sign in).
        "login": token.filter(|_| def.login_user.is_some() || def.id == "lobechat")
            .map(|t| match def.login_user { Some(u) => json!({ "user": u, "password": t }), None => json!({ "access_code": t }) }),
        // What to paste into its own settings: the box as it sees it, and its key.
        "connect": (def.show_key && state_word == "running").then(|| json!({
            "base_url": format!("http://host.docker.internal:{}/v1", state.ctrl_port()),
            "api_key": std::fs::read_to_string(state.config_dir().join("apps").join(def.id).join("key")).ok().map(|k| k.trim().to_string()),
            "model": state.endpoint().map(|e| e.model),
        })),
        "hint": (!def.hint.is_empty()).then(|| def.hint.replace("{model}", &state.endpoint().map(|e| e.model).unwrap_or_else(|| "quietbox".into()))),
    })
}

/// `{base}`, `{key}`, `{token}`, `{model}` and `{app:<id>}` filled in.
fn fill(text: &str, base: &str, key: &str, token: &str, model: &str) -> String {
    let mut out = text.replace("{base}", base).replace("{key}", key).replace("{token}", token).replace("{model}", model);
    for other in APPS {
        // On the apps' own network, by container name: reachable from the other apps, not from outside.
        out = out.replace(&format!("{{app:{}}}", other.id), &format!("http://{}:{}", container(other.id), other.container_port));
    }
    out
}

fn install(state: AppState, def: &'static AppDef) {
    INSTALLS.set(def.id, "Downloading", None);
    std::thread::spawn(move || {
        // The box's embeddings service, when it needs one: on, and answering.
        if def.needs.contains(&EMBEDDINGS) {
            INSTALLS.set(def.id, "Setting up embeddings (downloading the model)", None);
            crate::embeddings::ensure_embed(&state);
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let ready = rt.map(|rt| rt.block_on(async {
                for _ in 0..180 {
                    if crate::embeddings::ready(&state, &crate::embeddings::EMBED).await.is_some() {
                        return true;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
                false
            })).unwrap_or(false);
            if !ready {
                return INSTALLS.set(def.id, "Failed", Some("The embeddings service didn't come up. Check it in Models → Embeddings and reranking.".into()));
            }
        }
        // What it needs first, each the same way (installed, or started if it's stopped).
        for need in def.needs.iter().filter_map(|n| app(n)) {
            INSTALLS.set(def.id, &format!("Setting up {}", need.name), None);
            let ready = match docker_state(need.id).as_str() {
                "missing" => install_blocking(&state, need, &|step| INSTALLS.set(need.id, step, None)).map(|_| INSTALLS.clear(need.id)),
                "running" => Ok(()),
                _ => run_ok(Command::new("docker").args(["start", &container(need.id)])),
            };
            if let Err(e) = ready {
                INSTALLS.set(need.id, "Failed", Some(e.clone()));
                return INSTALLS.set(def.id, "Failed", Some(format!("{} didn't start: {e}", need.name)));
            }
        }
        match install_blocking(&state, def, &|step| INSTALLS.set(def.id, step, None)) {
            Ok(()) => INSTALLS.clear(def.id),
            Err(e) => INSTALLS.set(def.id, "Failed", Some(e)),
        }
    });
}

/// Start what `def` needs: stopped ones are started, removed ones installed again
/// (a research app's search engine), and the box's embeddings switched on.
fn start_needs(state: &AppState, def: &AppDef) {
    if def.needs.contains(&EMBEDDINGS) {
        crate::embeddings::ensure_embed(state);
    }
    for need in def.needs.iter().filter_map(|n| app(n)) {
        match docker_state(need.id).as_str() {
            "running" => {}
            "missing" => {
                if !INSTALLS.get(need.id).is_some_and(|(_, e)| e.is_none()) {
                    install(state.clone(), need);
                }
            }
            _ => {
                let _ = Command::new("docker").args(["start", &container(need.id)]).stdin(Stdio::null()).output();
            }
        }
    }
}

/// Start an installed app if it's stopped (research's private search). True when it's running.
pub fn ensure_running(id: &str) -> bool {
    match docker_state(id).as_str() {
        "running" => true,
        "missing" => false,
        _ => Command::new("docker").args(["start", &container(id)]).stdin(Stdio::null()).output().is_ok_and(|o| o.status.success()),
    }
}

fn run_ok(cmd: &mut Command) -> Result<(), String> {
    match cmd.stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().lines().last().unwrap_or("docker failed").to_string()),
        Err(e) => Err(format!("docker isn't available: {e}")),
    }
}

/// Pull, key, write its files, run. Blocking.
fn install_blocking(state: &AppState, def: &'static AppDef, step: &dyn Fn(&str)) -> Result<(), String> {
    let id = def.id;
    step("Downloading");
    run_ok(Command::new("docker").args(["pull", def.image]))?;
    step("Starting");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // An app key only for apps that call the box's model (SearXNG, ComfyUI… don't).
    let uses_key = def.show_key || def.env.iter().any(|(_, v)| v.contains("{key}")) || def.files.iter().any(|(_, c)| c.contains("{key}"));
    let key = if uses_key { state.app_keys().create(&format!("App: {}", def.name), now)?.1 } else { String::new() };
    let token = crate::pairing::issue_pair_id()[..16].to_string();
    let base = format!("http://host.docker.internal:{}/v1", state.ctrl_port());
    // The gateway sends any model name to the model that's serving; this one is for apps that list it.
    let model = state.endpoint().map(|e| e.model).unwrap_or_else(|| "quietbox".into());
    let dims = crate::embeddings::embed_dims(state).to_string();
    let bind = if def.local_only { "127.0.0.1:" } else { "" };
    // The apps share a private network, so one can reach another (a research app its search engine).
    let _ = Command::new("docker").args(["network", "create", NETWORK]).stdin(Stdio::null()).output();
    let mut cmd = Command::new("docker");
    cmd.args([
        "run",
        "-d",
        "--network",
        NETWORK,
        "--name",
        &container(id),
        "--label",
        &format!("tt-station.app={id}"),
        "--restart",
        "unless-stopped",
        "--add-host",
        "host.docker.internal:host-gateway",
        "-p",
        &format!("{bind}{}:{}", def.port, def.container_port),
        "-v",
        &format!("{}:{}", container(id), def.data),
    ]);
    if !def.files.is_empty() {
        let dir = state.config_dir().join("apps").join(id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for (inside, contents) in def.files {
            let name = std::path::Path::new(inside).file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let host = dir.join(name);
            std::fs::write(&host, fill(contents, &base, &key, &token, &model).replace("{embed_dim}", &dims)).map_err(|e| e.to_string())?;
            cmd.args(["-v", &format!("{}:{inside}", host.display())]);
        }
    }
    for (k, v) in def.env {
        cmd.args(["-e", &format!("{k}={}", fill(v, &base, &key, &token, &model).replace("{embed_dim}", &dims))]);
    }
    cmd.args(def.extra_args);
    if def.show_key {
        let dir = state.config_dir().join("apps").join(id);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("key");
        let _ = std::fs::write(&path, &key);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    cmd.arg(def.image);
    if let Some(command) = def.command {
        cmd.args(command);
    }
    run_ok(&mut cmd)
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": msg.into() })))
}

pub(crate) async fn get_apps(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let s = state.clone();
    let apps = tokio::task::spawn_blocking(move || {
        APPS.iter().map(|a| describe(&s, a)).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(json!({ "apps": apps }))
}

/// Start an app, installing it (and what it needs) first if it isn't there.
/// Blocking. `installing` or `running`.
pub fn start_or_install(state: &AppState, def: &'static AppDef) -> Result<&'static str, String> {
    if INSTALLS.get(def.id).is_some_and(|(_, e)| e.is_none()) {
        return Ok("installing");
    }
    // A container stuck restarting is replaced with a fresh one (its data volume stays).
    if docker_state(def.id) == "restarting" {
        let _ = Command::new("docker").args(["rm", "-f", &container(def.id)]).stdin(Stdio::null()).output();
    }
    if docker_state(def.id) != "missing" {
        start_needs(state, def);
    }
    match docker_state(def.id).as_str() {
        "missing" => {
            INSTALLS.clear(def.id);
            install(state.clone(), def);
            Ok("installing")
        }
        "running" => Ok("running"),
        _ => {
            let o = Command::new("docker").args(["start", &container(def.id)]).output().map_err(|e| e.to_string())?;
            if o.status.success() {
                Ok("running")
            } else {
                Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
            }
        }
    }
}

pub(crate) async fn post_start(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    let def = app(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such app."))?;
    let result = tokio::task::spawn_blocking(move || start_or_install(&state, def))
        .await
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({ "state": result })))
}

pub(crate) async fn post_stop(
    axum::extract::State(_state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    let def = app(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such app."))?;
    tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["stop", &container(def.id)])
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({ "state": "stopped" })))
}

pub(crate) async fn delete_app(
    axum::extract::State(_state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    let def = app(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such app."))?;
    INSTALLS.clear(def.id);
    tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["rm", "-f", &container(def.id)])
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(Json(json!({ "state": "not_installed" })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_is_consistent() {
        let mut ports: Vec<u16> = APPS.iter().map(|a| a.port).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), APPS.len(), "each app has its own port");
        for a in APPS {
            assert!(!(3010..=3011).contains(&a.port), "tt launch uses 3010-3011");
            assert_ne!(a.port, 3000, "TT-Studio uses 3000");
            if a.url_suffix.contains("{token}") {
                assert!(
                    a.env.iter().any(|(_, v)| v.contains("{token}")),
                    "{} needs its token in the env",
                    a.id
                );
            }
        }
        for a in APPS {
            for n in a.needs {
                assert!(*n == EMBEDDINGS || app(n).is_some(), "{} needs {n}, which isn't in the catalogue", a.id);
            }
        }
        assert!(
            app("jupyter").unwrap().local_only,
            "Jupyter runs code: never on the network"
        );
    }
}
