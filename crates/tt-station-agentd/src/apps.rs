// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Apps for the box: open-source tools that run in Docker next to the models,
//! each pre-wired to the box's own OpenAI-compatible endpoint (`/v1`, the
//! gateway) with its own app key, so they work the moment they start.
//!
//! Open WebUI and AnythingLLM come from the official tt CLI (`tt launch`, see
//! `tttools`); these are the rest. Containers are named `tt-app-<id>`, keep
//! their data in a `tt-app-<id>` volume and restart with the box.
//!
//! Every app listens only on the box itself (127.0.0.1) unless its owner turns
//! on **Open to the network** for it ([`Share`]): then it's published on every
//! address at `http://<box>:<port>`, with its own sign-in on (some apps have one
//! always; Langflow and LightRAG get theirs switched on; apps with no sign-in
//! are never shared). Another computer's
//! TT Station reaches a box-only app through its SSH tunnel; a plain browser
//! elsewhere can't. A container keeps the address it was started with: a change
//! takes effect the next time it's started from TT Station (which re-creates
//! it, keeping its data, password and key).
//!
//! - `GET /apps`: the catalogue and each app's state.
//! - `POST /apps/{id}/start`: install (in the background: pull, key, run) or start.
//! - `POST /apps/{id}/stop`; `DELETE /apps/{id}` (owners: the container and its
//!   network sharing; the data stays).
//! - `PUT /apps/{id}/network {open}`: Open to the network, or back to box-only.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::error::{err, ApiError};
use crate::routes::{AppState, BearerAuth};

pub struct AppDef {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    pub category: &'static str,
    /// `name:tag@sha256:…`: pulled by digest, so a moved tag can't change what
    /// runs (the tag is for people). Pinned 2026-10-15; bumping them:
    /// docs/reference/pinned-dependencies.md.
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
    /// Whether, and how, it may be opened to the network.
    pub share: Share,
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

/// Whether an app may be opened to the network, and on what terms. Each says
/// what someone who reaches it can do (for the owner's confirm), or why not.
pub enum Share {
    /// Its own sign-in is always on (the password or access code is on its tile).
    OwnLogin(&'static str),
    /// Its sign-in is turned on when it's shared: these on top of `env`.
    LoginWhenShared(&'static [(&'static str, &'static str)], &'static str),
    /// Never on the network.
    Never(&'static str),
}

pub const APPS: &[AppDef] = &[
    AppDef {
        id: "lobechat",
        name: "LobeChat",
        blurb: "A polished chat app with plugins, agents and file chat.",
        category: "Chat",
        image: "lobehub/lobe-chat:latest@sha256:b2d2454525523d9f0a19c79661f83ec45f13363dbadd5c1180887e77af35d872",
        port: 3020,
        container_port: 3210,
        data: "/app/data",
        env: &[
            ("OPENAI_API_KEY", "{key}"),
            ("OPENAI_PROXY_URL", "{base}"),
            ("ACCESS_CODE", "{token}"),
        ],
        command: None,
        share: Share::OwnLogin("chat with the box's model, after typing the access code"),
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
        image: "quay.io/jupyter/minimal-notebook:latest@sha256:877180014994396eb0857d3bad2dfedfb704fffccf54be4d1b7b50a43093f188",
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
            "pip install -q openai==3.26.1 && exec start-notebook.py",
        ]),
        share: Share::Never("It runs any code you type, as the box: it stays on the box. Other computers open it through TT Station (an SSH tunnel)."),
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
        id: "flowise",
        name: "Flowise",
        blurb: "Build AI agents and RAG flows visually, on the box's model.",
        category: "Build",
        image: "flowiseai/flowise:latest@sha256:cf880b17b1290fbaebebc76c33954bde67b3775aff95feec72a1d060cc81e43d",
        port: 3023,
        container_port: 3000,
        data: "/root/.flowise",
        env: &[
            ("FLOWISE_USERNAME", "admin"),
            ("FLOWISE_PASSWORD", "{token}"),
        ],
        command: None,
        share: Share::OwnLogin("sign in with the password shown here and build or run flows on the box's model"),
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
        blurb: "A private search engine on the box: it asks Google, Bing and others for you, with no account. The research apps here, and TT Station's own Research, search through it.",
        category: "Research",
        image: "searxng/searxng:latest@sha256:5903efa50bc6b99293fa059f7a0ff37eddd910210196af77d02e5536ccdfb387",
        port: 3024,
        container_port: 8080,
        data: "/var/cache/searxng",
        env: &[("SEARXNG_SECRET", "{token}")],
        command: None,
        share: Share::Never("It has no sign-in: anyone could use the box as their search proxy. Research apps reach it on the box."),
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
        image: "localdeepresearch/local-deep-research:latest@sha256:ce95481a35d3eebc2c1ae7bd34fad426108c3fb8d1a22f66c9e772276869c2f0",
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
        // It holds a key to the box's model and spends it for whoever reaches
        // it: this computer only (another computer's TT Station opens it
        // through its SSH tunnel).
        share: Share::Never("Anyone who reaches it can make an account and spend the box's model: it stays on the box. Other computers open it through TT Station (an SSH tunnel)."),
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
        image: "itzcrazykns1337/vane:latest@sha256:0b61bf0d4470e9f2a9c71f794b72fab6e5a612177172770684c158a461f6896e",
        port: 3026,
        container_port: 3000,
        data: "/home/vane/data",
        env: &[("OPENAI_BASE_URL", "{base}"), ("OPENAI_API_KEY", "{key}")],
        command: None,
        share: Share::Never("It has no sign-in of its own, and it spends the box's model for whoever reaches it."),
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
        id: "langflow",
        name: "Langflow",
        blurb: "Build agents and RAG pipelines visually, on the box's model, and serve them as APIs.",
        category: "Build",
        image: "langflowai/langflow:latest@sha256:a0396e089eb97fa471ef8affc492b8c56abb3758040a5ddaa80814b0abad15bf",
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
        share: Share::LoginWhenShared(&[("LANGFLOW_AUTO_LOGIN", "False")], "sign in with the password shown here and build, run and serve flows, which can run Python code on the box"),
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
        id: "openhands",
        name: "OpenHands",
        blurb: "An AI software engineer: give it a task in a project and it writes, runs and fixes the code, step by step, where you can watch.",
        category: "Agents",
        image: "ghcr.io/openhands/agent-canvas:latest@sha256:91b7a217dcbefe8c10562523b043458ed1dfaf8ba770194b319fe754cee03f23",
        port: 3031,
        container_port: 8000,
        data: "/home/openhands/.openhands",
        env: &[
            ("LOCAL_BACKEND_API_KEY", "{token}"),
            ("OH_SECRET_KEY", "{token}"),
            ("AGENT_CANVAS_DISABLE_TELEMETRY", "1"),
        ],
        command: None,
        share: Share::Never("It writes and runs code on the box: it stays on the box. Other computers open it through TT Station (an SSH tunnel)."),
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
        image: "openclaw/openclaw:2026.9.3@sha256:6cb72e1599b3b76e2ea3dcc2f4dd7f112247367fbecadfcdee8caa7423f4989b",
        port: 3032,
        container_port: 18789,
        data: "/home/node/.openclaw",
        env: &[("OPENCLAW_GATEWAY_TOKEN", "{token}"), ("TT_API_KEY", "{key}")],
        command: None,
        share: Share::OwnLogin("sign in to its control page with the access code and run your assistant, its channels and its tools"),
        url_suffix: "",
        homepage: "https://openclaw.ai",
        needs: &[],
        login_user: None,
        hint: "Sign in to its control page with the access code, then connect a chat app (WhatsApp, Telegram, Slack…) under Channels.",
        extra_args: &[],
        show_key: false,
        // The box as its model provider; memory search off (it would need embeddings the box doesn't serve).
        // `quietbox` is the gateway's catch-all model name (any name reaches the served model).
        files: &[("/home/node/.openclaw/openclaw.json", "{\n  \"gateway\": { \"mode\": \"local\" },\n  \"models\": { \"providers\": { \"tt\": { \"baseUrl\": \"{base}\", \"apiKey\": \"{key}\", \"api\": \"openai-completions\", \"models\": [ { \"id\": \"quietbox\", \"name\": \"TT Station\", \"contextWindow\": {context}, \"maxTokens\": 8192 } ] } } },\n  \"agents\": { \"defaults\": { \"model\": { \"primary\": \"tt/quietbox\" } } },\n  \"memory\": { \"search\": { \"provider\": \"none\" } }\n}\n")],
    },
    AppDef {
        id: "lightrag",
        name: "LightRAG",
        blurb: "Upload documents and ask questions: it maps how the facts in them connect (a knowledge graph) and answers from it, with the box's model and embeddings.",
        category: "Research",
        image: "ghcr.io/hkuds/lightrag:v1.5.7@sha256:5bdbd524931b011df246fe20888d110cef691e6804c12cde636a2b746d7de27e",
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
        share: Share::LoginWhenShared(&[("WHITELIST_PATHS", "/health")], "sign in with the password shown here, upload documents and ask questions with the box's model"),
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

// ---------------------------------------------------------------------
// Box-only, or open to the network
// ---------------------------------------------------------------------

/// Where Docker publishes the app's port (`-p <this><port>:<inside>`): the box
/// only, or every address once the owner has opened it to the network.
fn publish_host(def: &AppDef, open: bool) -> &'static str {
    if open && !matches!(def.share, Share::Never(_)) {
        ""
    } else {
        "127.0.0.1:"
    }
}

/// What the app gets on top of `env` when it's open to the network: its sign-in.
fn shared_env(def: &AppDef, open: bool) -> &'static [(&'static str, &'static str)] {
    match def.share {
        Share::LoginWhenShared(env, _) if open => env,
        _ => &[],
    }
}

fn network_file(state: &AppState) -> std::path::PathBuf {
    state.config_dir().join("apps").join("network.json")
}

/// The apps the owner has opened to the network.
fn opened(state: &AppState) -> Vec<String> {
    std::fs::read(network_file(state))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Whether the owner has opened `def` to the network (never for an app that can't be).
pub fn is_open(state: &AppState, def: &AppDef) -> bool {
    !matches!(def.share, Share::Never(_)) && opened(state).iter().any(|id| id == def.id)
}

fn set_open(state: &AppState, id: &str, open: bool) -> Result<(), String> {
    let mut ids = opened(state);
    ids.retain(|x| x != id);
    if open {
        ids.push(id.to_string());
    }
    let path = network_file(state);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, serde_json::to_vec(&ids).unwrap_or_default()).map_err(|e| e.to_string())
}

/// Whether `docker inspect`'s port bindings (`{{json .HostConfig.PortBindings}}`)
/// publish anywhere but the box's own loopback.
fn published_on_network(bindings: &str) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(bindings) else {
        return false;
    };
    v.as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, b)| b.as_array().into_iter().flatten())
        .any(|b| {
            let ip = b["HostIp"].as_str().unwrap_or("");
            !(ip.starts_with("127.") || ip == "::1" || ip == "localhost")
        })
}

/// A container as Docker has it: whether it's published on the network, and its environment.
fn inspect(id: &str) -> Option<(bool, Vec<String>)> {
    let o = Command::new("docker")
        .args([
            "inspect",
            "-f",
            "{{json .HostConfig.PortBindings}}\n{{range .Config.Env}}{{println .}}{{end}}",
            &container(id),
        ])
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    let mut lines = text.lines();
    let on_network = published_on_network(lines.next().unwrap_or(""));
    Some((on_network, lines.map(str::to_string).collect()))
}

/// Whether a container published as `on_network` with `env` is what the app's setting asks for now.
fn container_matches(def: &AppDef, open: bool, on_network: bool, env: &[String]) -> bool {
    on_network == publish_host(def, open).is_empty()
        && shared_env(def, open)
            .iter()
            .all(|(k, v)| env.iter().any(|e| *e == format!("{k}={v}")))
}

/// The value `placeholder` (`{token}`, `{key}`) had when the container was made,
/// read back out of its environment.
fn secret_in(def: &AppDef, env: &[String], placeholder: &str) -> Option<String> {
    def.env
        .iter()
        .filter(|(_, t)| t.contains(placeholder))
        .find_map(|(k, t)| {
            let (pre, post) = t.split_once(placeholder)?;
            let value = env.iter().find_map(|l| l.strip_prefix(&format!("{k}=")))?;
            value
                .strip_prefix(pre)?
                .strip_suffix(post)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        })
}

/// The box's address on its network, as another computer would type it.
fn lan_host(state: &AppState) -> String {
    crate::pairqr::lan_ipv4()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| format!("{}.local", state.name()))
}

/// What opening `def` to the network means, for the owner's confirm; or why it can't be.
fn network_terms(state: &AppState, def: &AppDef) -> Value {
    let reach = |what: &str| {
        format!(
            "Anyone who can reach this box on its network (the same Wi-Fi or office network, or a VPN into it) will be able to open http://{}:{} and {what}.",
            lan_host(state),
            def.port
        )
    };
    match def.share {
        Share::Never(why) => json!({ "can_share": false, "why_not": why }),
        Share::OwnLogin(what) => json!({ "can_share": true, "who": reach(what) }),
        Share::LoginWhenShared(_, what) => json!({ "can_share": true, "who": reach(what),
            "note": format!("{}'s own sign-in is turned on while it's open to the network.", def.name) }),
    }
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

/// What `crate::apps` keeps for the life of one agent (`crate::state::PerAgent`).
#[derive(Default)]
pub(crate) struct PerAgent {
    /// Installs in progress, and how far each has got.
    installs: Arc<Installs>,
}

fn mine(state: &AppState) -> &PerAgent {
    &state.per_agent().apps
}

/// Its address: on the network while it's open and published there; otherwise
/// the box's own (another computer's TT Station tunnels to it).
fn url_for(state: &AppState, def: &AppDef, token: Option<&str>, on_network: bool) -> String {
    let host = if on_network {
        lan_host(state)
    } else {
        "127.0.0.1".to_string()
    };
    format!(
        "http://{host}:{}{}",
        def.port,
        def.url_suffix.replace("{token}", token.unwrap_or(""))
    )
}

fn describe(state: &AppState, def: &AppDef) -> Value {
    let docker = docker_state(def.id);
    let install = mine(state).installs.get(def.id);
    // A container that keeps restarting is broken: say so, with what it last said.
    let crash = (docker == "restarting").then(|| {
        Command::new("docker")
            .args(["logs", "--tail", "5", &container(def.id)])
            .output()
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)
            })
            .and_then(|t| {
                t.lines().map(str::trim).rfind(|l| !l.is_empty()).map(|l| {
                    format!(
                        "It keeps stopping: {}",
                        l.chars().take(240).collect::<String>()
                    )
                })
            })
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
    let inspected = (docker != "missing").then(|| inspect(def.id)).flatten();
    let token = inspected
        .as_ref()
        .and_then(|(_, env)| secret_in(def, env, "{token}"));
    let open = is_open(state, def);
    let on_network = inspected.as_ref().is_some_and(|(n, _)| *n);
    // Containers keep the address they were made with until they're next started from here.
    let restart_to_apply = inspected
        .as_ref()
        .is_some_and(|(n, env)| !container_matches(def, open, *n, env));
    let mut network = network_terms(state, def);
    network["open"] = json!(open);
    network["on_network"] = json!(on_network);
    network["restart_to_apply"] = json!(restart_to_apply);
    json!({
        "id": def.id, "name": def.name, "blurb": def.blurb, "category": def.category,
        // Older clients label box-only apps with this.
        "port": def.port, "local_only": !(open && on_network), "homepage": def.homepage,
        "network": network,
        "needs": def.needs.iter().filter(|n| **n != EMBEDDINGS).filter_map(|n| app(n)).filter(|n| docker_state(n.id) == "missing").map(|n| n.name).collect::<Vec<_>>(),
        // Running apps that search (or otherwise lean) on this one.
        "used_by": APPS.iter().filter(|a| a.needs.contains(&def.id) && docker_state(a.id) == "running").map(|a| a.name).collect::<Vec<_>>(),
        "state": state_word,
        "step": install.as_ref().map(|(s, _)| s.clone()),
        "error": install.and_then(|(_, e)| e),
        "url": (state_word == "running").then(|| url_for(state, def, token.as_deref(), open && on_network)),
        // The app's own login, when it has one (shown so the owner can sign in).
        "login": token.filter(|_| def.login_user.is_some() || matches!(def.share, Share::OwnLogin(_)))
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
    let mut out = text
        .replace("{base}", base)
        .replace("{key}", key)
        .replace("{token}", token)
        .replace("{model}", model);
    for other in APPS {
        // On the apps' own network, by container name: reachable from the other apps, not from outside.
        out = out.replace(
            &format!("{{app:{}}}", other.id),
            &format!("http://{}:{}", container(other.id), other.container_port),
        );
    }
    out
}

fn install(state: AppState, def: &'static AppDef) {
    mine(&state).installs.set(def.id, "Downloading", None);
    std::thread::spawn(move || {
        // The box's embeddings service, when it needs one: on, and answering.
        if def.needs.contains(&EMBEDDINGS) {
            mine(&state).installs.set(
                def.id,
                "Setting up embeddings (downloading the model)",
                None,
            );
            crate::embeddings::ensure_embed(&state);
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            let ready = rt
                .map(|rt| {
                    rt.block_on(async {
                        for _ in 0..180 {
                            if crate::embeddings::ready(&state, &crate::embeddings::EMBED)
                                .await
                                .is_some()
                            {
                                return true;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                        false
                    })
                })
                .unwrap_or(false);
            if !ready {
                return mine(&state).installs.set(def.id, "Failed", Some("The embeddings service didn't come up. Check it in Models → Embeddings and reranking.".into()));
            }
        }
        // What it needs first, each the same way (installed, or started if it's stopped).
        for need in def.needs.iter().filter_map(|n| app(n)) {
            mine(&state)
                .installs
                .set(def.id, &format!("Setting up {}", need.name), None);
            let ready = match docker_state(need.id).as_str() {
                "missing" => install_blocking(
                    &state,
                    need,
                    &|step| mine(&state).installs.set(need.id, step, None),
                    None,
                )
                .map(|_| mine(&state).installs.clear(need.id)),
                "running" => Ok(()),
                _ => start_existing(&state, need),
            };
            if let Err(e) = ready {
                mine(&state)
                    .installs
                    .set(need.id, "Failed", Some(e.clone()));
                return mine(&state).installs.set(
                    def.id,
                    "Failed",
                    Some(format!("{} didn't start: {e}", need.name)),
                );
            }
        }
        match install_blocking(
            &state,
            def,
            &|step| mine(&state).installs.set(def.id, step, None),
            None,
        ) {
            Ok(()) => mine(&state).installs.clear(def.id),
            Err(e) => mine(&state).installs.set(def.id, "Failed", Some(e)),
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
                if mine(state)
                    .installs
                    .get(need.id)
                    .is_none_or(|(_, e)| e.is_some())
                {
                    install(state.clone(), need);
                }
            }
            _ => {
                let _ = start_existing(state, need);
            }
        }
    }
}

/// Start an installed app if it's stopped (research's private search). True when it's running.
pub fn ensure_running(id: &str) -> bool {
    match docker_state(id).as_str() {
        "running" => true,
        "missing" => false,
        _ => Command::new("docker")
            .args(["start", &container(id)])
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success()),
    }
}

fn run_ok(cmd: &mut Command) -> Result<(), String> {
    match cmd.stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr)
            .trim()
            .lines()
            .last()
            .unwrap_or("docker failed")
            .to_string()),
        Err(e) => Err(format!("docker isn't available: {e}")),
    }
}

/// What a re-created container keeps from the one it replaces: (its token, its app key).
type Kept = (Option<String>, Option<String>);

/// Pull, key, write its files, run. Blocking. `keep`: re-creating an installed
/// app (no pull, the same token and key).
fn install_blocking(
    state: &AppState,
    def: &'static AppDef,
    step: &dyn Fn(&str),
    keep: Option<Kept>,
) -> Result<(), String> {
    let id = def.id;
    if keep.is_none() {
        step("Downloading");
        run_ok(Command::new("docker").args(["pull", def.image]))?;
    }
    step("Starting");
    let (kept_token, kept_key) = keep.unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // An app key only for apps that call the box's model (SearXNG doesn't).
    let uses_key = def.show_key
        || def.env.iter().any(|(_, v)| v.contains("{key}"))
        || def.files.iter().any(|(_, c)| c.contains("{key}"));
    let key = match kept_key {
        Some(k) if uses_key => k,
        _ if uses_key => {
            state
                .app_keys()
                .create(&format!("App: {}", def.name), now)?
                .1
        }
        _ => String::new(),
    };
    let token = kept_token.unwrap_or_else(|| crate::pairing::issue_pair_id()[..16].to_string());
    let base = format!("http://host.docker.internal:{}/v1", state.ctrl_port());
    // The gateway sends any model name to the model that's serving; this one is for apps that list it.
    let model = state
        .endpoint()
        .map(|e| e.model)
        .unwrap_or_else(|| "quietbox".into());
    let dims = crate::embeddings::embed_dims(state).to_string();
    // The served model's context when it's been checked, else a common 128K.
    let context = crate::coding::known_context(&model)
        .unwrap_or(131072)
        .to_string();
    // The box only, unless its owner opened it to the network.
    let open = is_open(state, def);
    let bind = publish_host(def, open);
    // The apps share a private network, so one can reach another (a research app its search engine).
    let _ = Command::new("docker")
        .args(["network", "create", NETWORK])
        .stdin(Stdio::null())
        .output();
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
            let name = std::path::Path::new(inside)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("file");
            let host = dir.join(name);
            std::fs::write(
                &host,
                fill(contents, &base, &key, &token, &model)
                    .replace("{embed_dim}", &dims)
                    .replace("{context}", &context),
            )
            .map_err(|e| e.to_string())?;
            cmd.args(["-v", &format!("{}:{inside}", host.display())]);
        }
    }
    for (k, v) in def.env.iter().chain(shared_env(def, open)) {
        let value = fill(v, &base, &key, &token, &model).replace("{embed_dim}", &dims);
        if v.contains("{key}") || v.contains("{token}") {
            // A secret: docker takes it from its own environment, by name, so
            // it never shows in a process list.
            cmd.args(["-e", k]).env(k, value);
        } else {
            cmd.args(["-e", &format!("{k}={value}")]);
        }
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

type Reply = Result<Json<Value>, ApiError>;

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
    if mine(state)
        .installs
        .get(def.id)
        .is_some_and(|(_, e)| e.is_none())
    {
        return Ok("installing");
    }
    // A container stuck restarting is replaced with a fresh one (its data volume stays).
    if docker_state(def.id) == "restarting" {
        let _ = Command::new("docker")
            .args(["rm", "-f", &container(def.id)])
            .stdin(Stdio::null())
            .output();
    }
    if docker_state(def.id) != "missing" {
        start_needs(state, def);
    }
    match docker_state(def.id).as_str() {
        "missing" => {
            mine(state).installs.clear(def.id);
            install(state.clone(), def);
            Ok("installing")
        }
        "running" => Ok("running"),
        _ => start_existing(state, def).map(|_| "running"),
    }
}

/// Start a stopped container -- re-made first if the address (or sign-in) it
/// was made with isn't what its setting says now, keeping its data, token and key.
fn start_existing(state: &AppState, def: &'static AppDef) -> Result<(), String> {
    match inspect(def.id) {
        Some((on_network, env))
            if !container_matches(def, is_open(state, def), on_network, &env) =>
        {
            let token = secret_in(def, &env, "{token}");
            let key = secret_in(def, &env, "{key}").or_else(|| {
                std::fs::read_to_string(state.config_dir().join("apps").join(def.id).join("key"))
                    .ok()
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty())
            });
            run_ok(Command::new("docker").args(["rm", "-f", &container(def.id)]))?;
            install_blocking(state, def, &|_| {}, Some((token, key)))
        }
        _ => run_ok(Command::new("docker").args(["start", &container(def.id)])),
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

#[derive(Debug, serde::Deserialize)]
pub struct NetworkBody {
    pub open: bool,
}

/// `PUT /apps/{id}/network {open}`: Open to the network, or back to the box
/// only. Takes effect the next time it's started (a running app keeps its
/// address until then); the reply is the app as `GET /apps` shows it.
/// Owners only: it decides who on the network can reach the app.
pub(crate) async fn put_network(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: crate::routes::OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<NetworkBody>,
) -> Reply {
    let def = app(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such app."))?;
    let action = if body.open {
        "app_network_open"
    } else {
        "app_network_close"
    };
    let audit_state = state.clone();
    let done = tokio::task::spawn_blocking(move || {
        if body.open {
            match def.share {
                Share::Never(why) => {
                    return Err(err(
                        StatusCode::CONFLICT,
                        format!("{} stays on the box. {why}", def.name),
                    ))
                }
                Share::OwnLogin(_) | Share::LoginWhenShared(..) => {}
            }
        }
        set_open(&state, def.id, body.open)
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        Ok(Json(describe(&state, def)))
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    crate::audit::record(
        &audit_state,
        action,
        &id,
        if done.is_ok() { "ok" } else { "refused" },
    );
    done
}

/// `DELETE /apps/{id}`: remove the app's container, and its network sharing
/// with it (installed again, it's back on the box only until an owner opens
/// it). Owners only: removing an app takes it away from everyone.
pub(crate) async fn delete_app(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: crate::routes::OwnerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Reply {
    let def = app(&id).ok_or_else(|| err(StatusCode::NOT_FOUND, "No such app."))?;
    mine(&state).installs.clear(def.id);
    tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["rm", "-f", &container(def.id)])
            .output()
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| err(StatusCode::BAD_GATEWAY, e.to_string()))?;
    if opened(&state).iter().any(|x| x == def.id) {
        set_open(&state, def.id, false).map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        crate::audit::record(&state, "app_network_close", def.id, "ok");
    }
    crate::audit::record(&state, "app_delete", def.id, "ok");
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
                assert!(
                    *n == EMBEDDINGS || app(n).is_some(),
                    "{} needs {n}, which isn't in the catalogue",
                    a.id
                );
            }
        }
        assert!(
            matches!(app("jupyter").unwrap().share, Share::Never(_)),
            "Jupyter runs code: never on the network"
        );
        assert!(
            matches!(app("localdeepresearch").unwrap().share, Share::Never(_)),
            "it spends the box's model key for whoever reaches it: never on the network"
        );
    }

    #[test]
    fn every_image_is_pinned_by_digest() {
        let images = APPS.iter().map(|a| (a.id, a.image)).chain([
            ("voice-speak", crate::voice::SPEAK.image),
            ("voice-listen", crate::voice::LISTEN.image),
        ]);
        for (id, image) in images {
            let (name, digest) = image
                .split_once("@sha256:")
                .unwrap_or_else(|| panic!("{id}: {image} isn't pinned by digest"));
            assert!(
                digest.len() == 64
                    && digest
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{id}: {digest}"
            );
            assert!(
                name.rsplit('/').next().unwrap().contains(':'),
                "{id}: keep the tag for people reading it ({name})"
            );
        }
        // Jupyter's start command installs the OpenAI client: a fixed version.
        let jupyter = app("jupyter").unwrap().command.unwrap().join(" ");
        assert!(jupyter.contains("openai=="), "{jupyter}");
    }

    #[test]
    fn every_app_is_box_only_until_its_owner_opens_it() {
        for a in APPS {
            assert_eq!(
                publish_host(a, false),
                "127.0.0.1:",
                "{} is box-only by default",
                a.id
            );
            assert!(shared_env(a, false).is_empty(), "{}", a.id);
            match a.share {
                Share::Never(_) => assert_eq!(
                    publish_host(a, true),
                    "127.0.0.1:",
                    "{} never leaves the box",
                    a.id
                ),
                _ => assert_eq!(publish_host(a, true), "", "{} opened: every address", a.id),
            }
        }
        // Apps whose first visitor would own them, or that run code with no
        // sign-in, aren't published as-is.
        for id in [
            "jupyter",
            "searxng",
            "localdeepresearch",
            "vane",
            "openhands",
        ] {
            assert!(matches!(app(id).unwrap().share, Share::Never(_)), "{id}");
        }
        for id in ["lobechat", "flowise", "openclaw"] {
            let a = app(id).unwrap();
            assert!(matches!(a.share, Share::OwnLogin(_)), "{id}");
            // Its sign-in is always in its environment.
            assert!(
                a.env.iter().any(|(_, v)| v.contains("{token}")),
                "{id} has its own secret"
            );
        }
    }

    #[test]
    fn shared_apps_turn_their_sign_in_on() {
        let langflow = app("langflow").unwrap();
        assert_eq!(
            shared_env(langflow, true),
            &[("LANGFLOW_AUTO_LOGIN", "False")][..],
            "no auto-login as the superuser"
        );
        assert!(langflow
            .env
            .iter()
            .any(|(k, v)| *k == "LANGFLOW_SUPERUSER_PASSWORD" && v.contains("{token}")));
        let lightrag = app("lightrag").unwrap();
        assert_eq!(
            shared_env(lightrag, true),
            &[("WHITELIST_PATHS", "/health")][..],
            "its Ollama API behind the sign-in too"
        );
        assert!(lightrag
            .env
            .iter()
            .any(|(k, v)| *k == "AUTH_ACCOUNTS" && v.contains("{token}")));
    }

    #[test]
    fn a_container_made_for_another_setting_is_made_again() {
        let langflow = app("langflow").unwrap();
        let loopback = r#"{"7860/tcp":[{"HostIp":"127.0.0.1","HostPort":"3028"}]}"#;
        let everywhere = r#"{"7860/tcp":[{"HostIp":"","HostPort":"3028"}]}"#;
        let v6 = r#"{"7860/tcp":[{"HostIp":"0.0.0.0","HostPort":"3028"},{"HostIp":"::","HostPort":"3028"}]}"#;
        assert!(!published_on_network(loopback));
        assert!(published_on_network(everywhere) && published_on_network(v6));
        assert!(!published_on_network("null"));
        let plain = vec!["LANGFLOW_SUPERUSER=langflow".to_string()];
        let signed_in = vec![
            "LANGFLOW_SUPERUSER=langflow".to_string(),
            "LANGFLOW_AUTO_LOGIN=False".to_string(),
        ];
        assert!(container_matches(langflow, false, false, &plain));
        // Published everywhere before this setting existed: made again, box-only.
        assert!(!container_matches(langflow, false, true, &plain));
        // Opened: on the network, with its sign-in on.
        assert!(!container_matches(langflow, true, false, &plain));
        assert!(
            !container_matches(langflow, true, true, &plain),
            "an old open container without its sign-in"
        );
        assert!(container_matches(langflow, true, true, &signed_in));
    }

    #[test]
    fn a_remade_container_keeps_its_secrets() {
        let lightrag = app("lightrag").unwrap();
        let env = vec![
            "AUTH_ACCOUNTS=admin:s3cret".to_string(),
            "TOKEN_SECRET=s3cret".to_string(),
            "LLM_BINDING_API_KEY=tt-key".to_string(),
        ];
        assert_eq!(
            secret_in(lightrag, &env, "{token}").as_deref(),
            Some("s3cret"),
            "not admin:s3cret"
        );
        assert_eq!(
            secret_in(lightrag, &env, "{key}").as_deref(),
            Some("tt-key")
        );
        assert_eq!(secret_in(app("vane").unwrap(), &env, "{token}"), None);
    }
}
