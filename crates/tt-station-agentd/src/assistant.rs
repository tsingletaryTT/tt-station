// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The box's chat assistant: `POST /assistant` and `POST /assistant/confirm`.
//!
//! A phone app (or `tt-station ask`) sends a short chat history; the agent
//! asks **the model this box is currently serving** to answer it, offering
//! that model a handful of OpenAI-style tools that inspect or control the
//! box. The served model is the brain, the agent is the hands -- and the
//! agent, not the model, decides which hands are allowed to move unprompted.
//!
//! ## The confirmation model
//!
//! Tools come in two kinds (see [`tool_kind`]):
//!
//! * **Read-only** (`box_status`, `list_models`, `recent_logs`) run
//!   immediately. Their JSON result goes back to the model as a `role: tool`
//!   message and the model gets another turn, up to [`MAX_TOOL_ROUNDS`]
//!   rounds. Each run is reported to the client as an action with status
//!   `"done"`.
//! * **Disruptive** (`run_model`, `stop_model`, `reset_chips`) are NEVER run
//!   from the chat. A call is validated, parked in `AppState` as a
//!   [`PendingAction`] under a random id, reported to the client as
//!   `"needs_confirmation"`, and the loop stops. Only a human hitting
//!   `POST /assistant/confirm` with that id and `approve: true` runs it --
//!   through the very same `AppState::start_serving`/`stop_serving`/
//!   `run_power_command` bodies `/run`, `/stop` and `/power` use. Pending
//!   actions expire after [`PENDING_TTL`] and are consumed by the first
//!   confirm call, approved or not.
//!
//! The model can't be trusted to keep that line itself -- a 32B model asked
//! to "restart the model" will happily reply "Done!" -- so the line is drawn
//! in code, and the system prompt merely tells the model where it is.
//!
//! ## The assistant runs on the thing it controls
//!
//! Every disruptive action takes the served model down: `run_model` replaces
//! it (the new one takes minutes to load), `stop_model` removes it, and
//! `reset_chips` resets the silicon under it. Each pending action's summary
//! says so, so the human confirms knowing the assistant will go quiet. A
//! confirmed `run_model` therefore runs in the BACKGROUND (`/run` blocks
//! until the new model is ready -- minutes) and the confirm call returns
//! `"started"` immediately; the client should poll `GET /status` to learn
//! when the new model is up.
//!
//! ## Wire shapes
//!
//! See `docs/reference/assistant.md` for the full contract. In short:
//!
//! ```text
//! POST /assistant          { "messages": [ {"role": "user"|"assistant", "content": "..."} ] }
//!   -> 200 { "reply": "...", "actions": [ActionReport], "model": "<served id>" }
//! POST /assistant/confirm  { "action_id": "...", "approve": true|false }
//!   -> 200 { "action_id", "tool", "status": "cancelled"|"started"|"done", "summary" }
//! ```

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::routes::AppState;

/// How many of the client's most recent chat turns are forwarded to the
/// model. The client keeps the whole history; the server only ever sends the
/// tail, which bounds both the prompt size (a long voice session would
/// otherwise grow without limit) and the latency of every reply.
pub const MAX_HISTORY: usize = 20;

/// How many rounds of read-only tool calls one `POST /assistant` may run
/// before the model is made to answer in plain text (one final call with no
/// tools offered). Bounds the worst case -- a model that keeps calling
/// `box_status` forever -- to `MAX_TOOL_ROUNDS + 1` completions.
pub const MAX_TOOL_ROUNDS: usize = 4;

/// How long a disruptive action waits for `POST /assistant/confirm` before
/// it silently expires. Long enough for a human to read the prompt, think,
/// and tap; short enough that a stale "yes" from a forgotten conversation
/// can't fire an action the box's state has long since moved past.
pub const PENDING_TTL: Duration = Duration::from_secs(5 * 60);

/// Cap on simultaneously pending actions (see `AppState::insert_pending_action`,
/// which evicts the oldest past this).
pub const MAX_PENDING: usize = 16;

/// Ceiling on one `/chat/completions` round trip. A 32B+ model on a busy box
/// can take tens of seconds for a long answer; two minutes is generous
/// without letting a wedged server hang the phone forever.
pub const LLM_TIMEOUT: Duration = Duration::from_secs(120);

/// `max_tokens` for every completion. Replies are meant to be 1-3 spoken
/// sentences; this mostly bounds a model that ignores that instruction.
pub const MAX_TOKENS: u32 = 400;

/// Ceiling for one streamed round: long, detailed answers from a 70B model run
/// well past [`LLM_TIMEOUT`]. The client sees progress the whole time.
pub const STREAM_TIMEOUT: Duration = Duration::from_secs(600);

/// Default and maximum `lines` for the `recent_logs` tool. The maximum is
/// far below `GET /logs`'s own cap: every line goes into the model's context.
const DEFAULT_LOG_LINES: usize = 10;
const MAX_LOG_LINES: usize = 50;

/// Per-line and total caps applied by [`compact_log_lines`].
const MAX_LOG_LINE_CHARS: usize = 200;
const MAX_LOG_TOTAL_CHARS: usize = 4000;

/// How many model names `list_models` hands the model at most.
const MAX_LISTED_MODELS: usize = 40;

/// How old a cached `tt-smi` snapshot may be for `box_status` to reuse it
/// instead of taking a fresh one (see `AppState::peek_snapshot`).
const TELEMETRY_MAX_AGE: Duration = Duration::from_secs(30);

/// Upper bound on `box_status`'s fresh `tt-smi -s` when nothing recent is
/// cached (~0.2s measured on a P300x2 box with a model serving). A wedged
/// `tt-smi` degrades to "telemetry unavailable" instead of stalling the chat.
const TELEMETRY_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Names of the tools that run immediately.
pub const READ_ONLY_TOOLS: [&str; 20] = [
    "remember",
    "forget",
    "box_status",
    "list_models",
    "recent_logs",
    "search_documents",
    "create_image",
    "web_search",
    "read_webpage",
    "open_app",
    "control_computer",
    "open_folder",
    "find_files",
    "start_recording",
    "stop_recording",
    "open_station",
    "bringup_status",
    "bringup_decide",
    "research",
    "research_check",
];

/// Names of the tools that only ever become pending actions.
pub const DISRUPTIVE_TOOLS: [&str; 5] = [
    "run_model",
    "stop_model",
    "reset_chips",
    "switch_mode",
    "install_app",
];

/// Read-only tools that act -- on the box, the person's computer, a bring-up,
/// a recording or what Tori remembers -- rather than only look. Once text from
/// the web has come into a turn it may be what's steering the model, so for
/// the rest of that turn each of these waits for the person, like the
/// disruptive ones (see [`needs_confirmation`]).
pub const ACTING_TOOLS: [&str; 10] = [
    "remember",
    "forget",
    "open_app",
    "control_computer",
    "open_folder",
    "open_station",
    "start_recording",
    "stop_recording",
    "bringup_decide",
    "research",
];

/// Tools whose results carry text from the web.
pub const WEB_TOOLS: [&str; 2] = ["web_search", "read_webpage"];

/// Whether a model's call of `name` would fetch or open a web address the turn
/// didn't already know, after web text came in (see `crate::urltrust`): it
/// waits for the person too.
pub fn fetches_unknown_address(name: &str, args: &Value, turn: &crate::urltrust::Turn) -> bool {
    match name {
        "read_webpage" | "open_app" => args["url"]
            .as_str()
            .is_some_and(|u| !u.trim().is_empty() && !turn.may_fetch(u)),
        _ => false,
    }
}

/// Whether a model's call of a read-only tool must wait for the person.
/// Always: answering a bring-up (it runs a command on the box), starting a
/// recording, opening a web address. Once web text is in the turn (`tainted`):
/// any acting tool.
pub fn needs_confirmation(name: &str, args: &Value, tainted: bool) -> bool {
    match name {
        "bringup_decide" | "start_recording" => true,
        "open_app" if args["url"].as_str().is_some_and(|u| !u.trim().is_empty()) => true,
        _ => tainted && ACTING_TOOLS.contains(&name),
    }
}

// ---------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------

/// `POST /assistant`'s request body.
#[derive(Debug, Deserialize)]
pub struct AssistantRequest {
    pub messages: Vec<ChatTurn>,
    /// `"concise"` (spoken-length plain text -- the default for `/assistant`)
    /// or `"detailed"` (thorough, Markdown -- the default for
    /// `/assistant/stream`, which a screen reads).
    #[serde(default)]
    pub style: Option<String>,
    /// Let a reasoning model think before answering (Qwen3's thinking mode).
    /// Streamed separately as `reasoning` events; never part of the reply.
    #[serde(default)]
    pub think: bool,
    /// `"tori"`: Tori, the summon-anywhere assistant that also runs the box's
    /// computer (apps, files, volume, media). Absent: the box's chat.
    #[serde(default)]
    pub persona: Option<String>,
    /// Tori on another computer (`"mac"`, `"linux"`...): her computer tools
    /// act on that computer -- its app runs them and posts the result to
    /// `POST /assistant/client-result` -- not on the box. Streaming only.
    #[serde(default)]
    pub client: Option<String>,
}

impl AssistantRequest {
    pub fn options(&self, default_detailed: bool) -> ChatOptions {
        let mut opts =
            ChatOptions::from_request(self.style.as_deref(), self.think, default_detailed);
        opts.tori = self.persona.as_deref() == Some("tori");
        opts.client = self.client.as_deref().is_some_and(|c| !c.is_empty());
        opts
    }
}

/// How to answer: resolved from an [`AssistantRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatOptions {
    pub detailed: bool,
    pub think: bool,
    /// Tori: her name, and the computer tools.
    pub tori: bool,
    /// The computer tools run on the person's own computer (see [`CLIENT_TOOLS`]).
    pub client: bool,
}

impl ChatOptions {
    pub fn from_request(style: Option<&str>, think: bool, default_detailed: bool) -> Self {
        let detailed = match style {
            Some("concise") => false,
            Some("detailed") => true,
            _ => default_detailed,
        };
        ChatOptions {
            detailed,
            think,
            tori: false,
            client: false,
        }
    }

    /// Room to answer: a spoken reply is short; a written one, and especially
    /// one that reasons first, needs much more.
    fn max_tokens(self) -> u32 {
        match (self.detailed, self.think) {
            (false, false) => MAX_TOKENS,
            (false, true) => 2000,
            (true, false) => 1800,
            (true, true) => 4000,
        }
    }
}

/// One turn of the client-kept chat history. Only `"user"` and
/// `"assistant"` roles are accepted -- the system prompt is the agent's
/// alone, and tool messages exist only inside one request's loop.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChatTurn {
    pub role: String,
    pub content: String,
}

/// `POST /assistant`'s response body.
#[derive(Debug, Serialize)]
pub struct AssistantResponse {
    /// Plain text, meant to be shown and/or spoken.
    pub reply: String,
    /// Every tool the model called that the agent acted on, in order.
    pub actions: Vec<ActionReport>,
    /// The served model id that produced `reply`.
    pub model: String,
}

/// One tool call, as reported to the client.
#[derive(Debug, Clone, Serialize)]
pub struct ActionReport {
    /// Random id. Only meaningful for `needs_confirmation` actions, where it
    /// is what `POST /assistant/confirm` takes; every action gets one anyway
    /// so a client can key a list by it.
    pub id: String,
    pub tool: String,
    pub args: Value,
    pub status: ActionStatus,
    /// Human-readable one-liner.
    pub summary: String,
    /// Images this action made (`create_image`), fetched from
    /// `GET /creations/{id}`. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
}

/// An [`ActionReport`]'s status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// A read-only tool that ran.
    Done,
    /// A disruptive tool waiting on `POST /assistant/confirm`.
    NeedsConfirmation,
    /// A read-only tool that failed to run.
    Error,
    /// Running on the person's computer (Tori on another computer); a `done`
    /// or `error` report with the same id follows.
    OnClient,
}

/// `POST /assistant/confirm`'s request body.
#[derive(Debug, Deserialize)]
pub struct ConfirmRequest {
    pub action_id: String,
    pub approve: bool,
}

/// `POST /assistant/confirm`'s response body.
#[derive(Debug, Serialize)]
pub struct ConfirmResponse {
    pub action_id: String,
    pub tool: String,
    pub status: ConfirmStatus,
    pub summary: String,
    /// `{tool, args}` with [`ConfirmStatus::OnClient`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<Value>,
}

/// A [`ConfirmResponse`]'s status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmStatus {
    /// `approve: false` -- nothing ran.
    Cancelled,
    /// `run_model` approved and kicked off in the background.
    Started,
    /// `stop_model`/`reset_chips` approved and finished.
    Done,
    /// Approved, and for the person's computer to do: the app runs
    /// [`ConfirmResponse::run`] itself (Tori on another computer).
    OnClient,
}

/// A disruptive action parked until a human approves or denies it. Stored in
/// `AppState` (see `AppState::insert_pending_action`).
#[derive(Debug, Clone)]
pub struct PendingAction {
    pub tool: String,
    /// Validated, canonical args (e.g. `run_model`'s `model` already resolved
    /// to a name `GET /models` lists).
    pub args: Value,
    pub summary: String,
    pub created: Instant,
}

/// Everything that can make an assistant route fail. `routes.rs` maps each
/// variant to an HTTP status (see `assistant_error` there).
#[derive(Debug)]
pub enum AssistantError {
    /// No model is serving, so there's no brain to ask. `409`.
    NotServing,
    /// A non-chat model (image/video/speech) is serving. `409`.
    NotChat(String),
    /// The client sent something unusable (empty history, bad role). `400`.
    BadRequest(String),
    /// `POST /assistant/confirm` named no live pending action. `404`.
    UnknownAction,
    /// The served model couldn't be reached or answered garbage. `502`.
    Upstream(String),
    /// Running a confirmed action failed. Same mapping as `/run`/`/stop`
    /// (`409` for chip contention, else `500`).
    Backend(anyhow::Error),
}

// ---------------------------------------------------------------------
// Pure helpers (unit-tested below)
// ---------------------------------------------------------------------

/// Which kind of tool `name` is, or `None` for a name the agent doesn't
/// offer (models hallucinate tools; that's answered with an error message,
/// never a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnly,
    Disruptive,
}

pub fn tool_kind(name: &str) -> Option<ToolKind> {
    if READ_ONLY_TOOLS.contains(&name) {
        Some(ToolKind::ReadOnly)
    } else if DISRUPTIVE_TOOLS.contains(&name) {
        Some(ToolKind::Disruptive)
    } else {
        None
    }
}

/// The OpenAI `tools` array offered to the served model.
pub fn tool_definitions() -> Value {
    fn tool(name: &str, description: &str, parameters: Value) -> Value {
        json!({
            "type": "function",
            "function": { "name": name, "description": description, "parameters": parameters }
        })
    }
    let no_params = json!({ "type": "object", "properties": {} });
    json!([
        tool(
            "box_status",
            "Get this box's live status: which model is serving and where, the device mesh and chips, and chip temperatures/power when available.",
            no_params.clone(),
        ),
        tool(
            "list_models",
            "List the models this box can serve.",
            no_params.clone(),
        ),
        tool(
            "recent_logs",
            "Read the last lines of the serving container's log, e.g. to see why a model failed to start.",
            json!({
                "type": "object",
                "properties": {
                    "lines": { "type": "integer", "description": "How many lines (1-50, default 10)." }
                }
            }),
        ),
        tool(
            "search_documents",
            "Search the user's own uploaded documents and return the most relevant passages. Use it for any question about their files, then answer from the passages and name the document.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to look for, in keywords (e.g. \"pets deposit\")." },
                    "document": { "type": "string", "description": "Optional: only search documents whose name contains this." }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "remember",
            "Save a lasting fact or preference about the user for future conversations (e.g. their name, units they prefer, their projects). Only when they tell you something worth keeping or ask you to remember it.",
            json!({
                "type": "object",
                "properties": { "fact": { "type": "string", "description": "One short sentence, in the third person (\"Prefers metric units\")." } },
                "required": ["fact"]
            }),
        ),
        tool(
            "forget",
            "Forget saved facts about the user that contain these words, when they ask you to forget something.",
            json!({
                "type": "object",
                "properties": { "about": { "type": "string", "description": "Words in the fact(s) to forget." } },
                "required": ["about"]
            }),
        ),
        tool(
            "research",
            "Start a deep research job on the box: it searches the web, reads dozens of pages and writes a cited report in TT Station's Research page, over several minutes, and notifies the user when it's done. Use it when the user asks you to research, investigate or write a report on something, not for quick facts (use web_search for those).",
            json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The full research question, in the user's words plus any detail they gave." },
                    "depth": { "type": "string", "enum": ["quick", "standard", "deep"], "description": "quick (a few minutes), standard (default), or deep (the most sources)." }
                },
                "required": ["question"]
            }),
        ),
        tool(
            "research_check",
            "Check whether the box is set up for research: is a suitable model serving, and does search go through the box's private search engine (SearXNG) or the public web. Use it when the user asks to set up research, or before research if unsure; then offer the missing pieces (run_model for a model, install_app with app \"searxng\" for private search).",
            no_params.clone(),
        ),
        tool(
            "install_app",
            "Install (or start) an app from TT Station's app store on the box, in Docker, e.g. searxng (private search for research), localdeepresearch, lightrag, comfyui, langflow, crawl4ai, openhands. It downloads the app the first time. Needs the user's confirmation.",
            json!({
                "type": "object",
                "properties": { "app": { "type": "string", "enum": crate::apps::APPS.iter().map(|a| a.id).collect::<Vec<_>>(), "description": "The app's id." } },
                "required": ["app"]
            }),
        ),
        tool(
            "web_search",
            "Search the web. Use it for news, current events, prices, recent releases, or any fact you're not sure of. Returns titles, links and snippets; read a page with read_webpage for detail.",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "What to search for." } },
                "required": ["query"]
            }),
        ),
        tool(
            "open_app",
            "Open an app or a web page on the box's own screen (the QuietBox desktop), e.g. VS Code, Chrome, Files, Terminal, or YouTube. Use it whenever the user asks to open, launch or show something on the box. Give the app's name, a web address, or both (to open the address in that app).",
            json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string", "description": "The app's name as the user said it, e.g. \"VS Code\"." },
                    "url": { "type": "string", "description": "A full http(s) address to open, e.g. https://www.youtube.com." }
                }
            }),
        ),
        tool(
            "bringup_status",
            "How the current model bring-up (Codex or Claude Code bringing a new model up on the chips) is going, and whether it's waiting for the user.",
            no_params.clone(),
        ),
        tool(
            "bringup_decide",
            "Answer what a running bring-up is waiting on (a command it wants to run). Only when the user clearly says to allow or deny it.",
            json!({ "type": "object", "properties": { "decision": { "type": "string", "enum": ["accept", "decline"] } }, "required": ["decision"] }),
        ),
        tool(
            "start_recording",
            "Start recording a meeting on the box: its microphone and whatever it's playing. Remind the user to tell everyone.",
            json!({ "type": "object", "properties": { "title": { "type": "string" } } }),
        ),
        tool(
            "stop_recording",
            "Stop the meeting recording; the box transcribes it in the background.",
            no_params.clone(),
        ),
        tool(
            "open_station",
            "Open a page of TT Station, the box's desktop app: home, chat, studio, transcribe, models, bringup (bring up a new model), hardware, connect (apps), settings, research (research reports), news (what's new), code (coding agents working in projects).",
            json!({
                "type": "object",
                "properties": { "page": { "type": "string", "enum": ["home", "chat", "studio", "transcribe", "models", "bringup", "hardware", "connect", "settings", "research", "news", "code"] } },
                "required": ["page"]
            }),
        ),
        tool(
            "control_computer",
            "Control the box's desktop: speaker volume, mute, media playback (any app playing audio or video), lock the screen, dark or light mode, do not disturb.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": crate::desktop::CONTROLS },
                    "value": { "type": "number", "description": "Volume percent for volume_set (0-150), or the step for volume_up/volume_down (default 10)." }
                },
                "required": ["action"]
            }),
        ),
        tool(
            "open_folder",
            "Open a folder in Files, or a file in its default app, on the box's screen. Paths are in the user's home: Downloads, Documents, ~/code/project, or a full path from find_files.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        ),
        tool(
            "find_files",
            "Find files or folders in the user's home on the box by part of their name; newest first. Then open one with open_folder.",
            json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Part of the file or folder name." } },
                "required": ["name"]
            }),
        ),
        tool(
            "read_webpage",
            "Read a public web page's text, e.g. a link the user pasted or a search result.",
            json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "The page's full http(s) address." } },
                "required": ["url"]
            }),
        ),
        tool(
            "create_image",
            "Draw a picture with the box's image model and show it to the user in the chat. Use it whenever the user asks you to draw, make, generate, imagine or show an image. Write a vivid, specific prompt (subject, setting, style, lighting).",
            json!({
                "type": "object",
                "properties": {
                    "prompt": { "type": "string", "description": "A detailed description of the image." },
                    "count": { "type": "integer", "description": "How many variations (1-4, default 1)." }
                },
                "required": ["prompt"]
            }),
        ),
        tool(
            "switch_mode",
            "Switch what the box is set up to do: chat (best answers), images, chat_images (chat and drawing at once, smaller chat model), video, or transcribe. Use it when the user wants something the box isn't set up for, e.g. a picture while only chat is loaded. Needs the user's confirmation.",
            json!({
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": ["chat", "images", "chat_images", "video", "transcribe"],
                        "description": "The mode to switch to."
                    }
                },
                "required": ["mode"]
            }),
        ),
        tool(
            "run_model",
            "Start serving a different model. Replaces the current model; needs the user's confirmation.",
            json!({
                "type": "object",
                "properties": {
                    "model": { "type": "string", "description": "Model name, as list_models reports it." }
                },
                "required": ["model"]
            }),
        ),
        tool(
            "stop_model",
            "Stop serving the current model. Needs the user's confirmation.",
            no_params.clone(),
        ),
        tool(
            "reset_chips",
            "Reset the Tenstorrent chips (tt-smi -r). Stops serving. Needs the user's confirmation.",
            no_params,
        ),
    ])
}

/// The tools that can do something right now: no `create_image` without an
/// image model, no `search_documents` without documents, no web tools when web
/// access is off. Fewer schemas keep small models (Llama-3.1-8B on half a box)
/// on track -- with all of them offered it was seen to loop on tool names.
pub async fn offered_tools(state: &AppState, tori: bool) -> Value {
    let images = crate::gateway::image_target(state).await.is_some();
    let docs = state.docs().is_some_and(|d| !d.is_empty());
    let web = web_allowed().is_ok();
    let all = tool_definitions();
    let keep: Vec<Value> = all
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter(|t| match t["function"]["name"].as_str().unwrap_or("") {
                    "create_image" => images,
                    "search_documents" => docs,
                    "web_search" | "read_webpage" => web,
                    // The computer is Tori's; the box's chat sticks to the box.
                    "open_app" | "control_computer" | "open_folder" | "find_files"
                    | "open_station" => tori,
                    _ => true,
                })
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    Value::Array(keep)
}

/// Live box facts injected into the system prompt.
#[derive(Debug, Clone)]
pub struct BoxContext {
    pub name: String,
    pub chips: String,
    pub device_mesh: Option<String>,
    pub model: String,
    /// Names of the user's uploaded documents (`search_documents`).
    pub documents: Vec<String>,
    /// How the box is split between models right now, when it is.
    pub layout: Option<String>,
    /// What the assistant remembers about the user (a prompt section, or empty).
    pub memories: String,
}

/// The system prompt: who the model is, what box it's on, and the rules
/// that matter most for a spoken, action-taking assistant.
pub fn system_prompt(ctx: &BoxContext) -> String {
    system_prompt_styled(ctx, false)
}

/// [`system_prompt`] for a chosen answer style: `detailed` swaps the spoken
/// brevity rule for one that asks for thorough, well-formatted Markdown.
pub fn system_prompt_styled(ctx: &BoxContext, detailed: bool) -> String {
    let base = system_prompt_base(ctx);
    if !detailed {
        return base;
    }
    base.replace(
        "- Be brief: 1-3 short sentences. Replies may be spoken aloud, so use plain text with \
         no markdown, lists, headings, or code formatting.\n",
        "- Give genuinely helpful, accurate answers: complete but not padded, the most \
         important point first. The reply is read on a phone screen, so use Markdown when it \
         makes the answer clearer -- short headings, bullet or numbered lists, tables, and \
         fenced code blocks with a language tag -- and plain prose for simple answers.\n",
    )
}

fn system_prompt_base(ctx: &BoxContext) -> String {
    let mesh = ctx.device_mesh.as_deref().unwrap_or("unknown");
    format!(
        "You are the assistant for \"{name}\", a Tenstorrent QuietBox AI workstation \
         ({chips} chips, device mesh {mesh}). You control this box through the provided tools, \
         and you yourself run on the model the box is currently serving: {model}.\n\
         Rules:\n\
         - Be brief: 1-3 short sentences. Replies may be spoken aloud, so use plain text with \
         no markdown, lists, headings, or code formatting.\n\
         - Call a tool only for box actions or box information; answer anything else directly.\n\
         - Use box_status, list_models, and recent_logs to look things up instead of guessing.\n\
         - When the user asks for a picture, call create_image; it appears in the chat.\n\
         - box_status is only about THIS box right now (what's serving, temperatures, power). \
         Questions about products, companies, people, news, prices or anything you aren't \
         sure of -- including other Tenstorrent hardware -- are web questions: call web_search \
         (then read_webpage for detail) and say where the answer came from. When the user \
         says \"look it up\" or pastes a link, use the web tools.\n\
         - When the user asks to start a model, stop the model, or reset the chips, call \
         run_model, stop_model, or reset_chips right away. Do NOT ask \"are you sure?\" \
         first: calling the tool does not do it yet, it shows the user a Confirm button. \
         After calling one, say in one sentence what will happen if they confirm.\n\
         - Starting a different model, stopping the model, or resetting the chips takes you \
         offline until a model is serving again, because you run on the served model.\n\
         - NEVER say an action happened unless a tool result says it did.\n\
         - Never mention tool or function names to the user; if a model name doesn't \
         match, retry with one of the suggested names or call list_models yourself.{docs}{layout}{memories}{facts}",
        name = ctx.name,
        chips = ctx.chips,
        model = ctx.model,
        docs = documents_note(&ctx.documents),
        memories = ctx.memories,
        layout = ctx.layout.as_deref().map(|l| format!("\n{l}")).unwrap_or_default(),
        facts = hardware_facts(ctx.device_mesh.as_deref()),
    )
}

/// The prompt lines telling the model which documents it can search: questions
/// about "my lease" should go to `search_documents`, not to a guess.
fn documents_note(documents: &[String]) -> String {
    if documents.is_empty() {
        return String::new();
    }
    let shown: Vec<&str> = documents.iter().take(30).map(String::as_str).collect();
    let more = documents.len().saturating_sub(shown.len());
    format!(
        "\nThe user has uploaded these documents: {}{}. For any question about them (or \
         about \"my\" files, notes, contracts, papers...), call search_documents first and \
         answer only from the passages it returns, naming the document; if nothing \
         relevant comes back, say so instead of guessing.",
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

/// Verified facts about the box's silicon (Tenstorrent docs, cross-checked in
/// tt-station's CONTEXT.md), so the assistant answers questions about its own
/// hardware from facts instead of inventing specs -- seen live: an 8B model
/// claiming "6x AI processing units" per Tensix core. Blackhole meshes only;
/// other boards get no claims rather than wrong ones.
fn hardware_facts(mesh: Option<&str>) -> String {
    let mesh = mesh.unwrap_or_default().to_lowercase();
    if !(mesh.starts_with("p100") || mesh.starts_with("p150") || mesh.starts_with("p300")) {
        return String::new();
    }
    let boards = match mesh.as_str() {
        "p300x2" => {
            "- This box has two p300 cards of two chips each: four Blackhole chips, 128 GB of \
             GDDR6 in total.\n"
        }
        "p300" => "- This box has one p300 card: two Blackhole chips.\n",
        _ => "",
    };
    format!(
        "\nFacts about this hardware (use them; don't invent other specs -- say so if you \
         don't know):\n\
         - Tenstorrent Blackhole chip: 120 Tensix cores, 16 large SiFive X280 RISC-V cores, about \
         180 MB of on-chip SRAM, 32 GB of GDDR6 at 512 GB/s, up to 664 TFLOPS (BLOCKFP8), PCIe 5.0 \
         x16, and four QSFP-DD 800G Ethernet ports for chip-to-chip scale-out.\n\
         - A Tensix core has five small RISC-V cores that orchestrate data movement and compute, a \
         matrix engine (FPU) and a vector engine (SFPU), about 1.5 MB of local SRAM, and \
         network-on-chip routers.\n\
         {boards}\
         - Software: TT-Metalium (low-level kernels), TT-NN (operator library), and \
         tt-inference-server (vLLM-based serving), which this box uses to serve models."
    )
}

/// Remove `<think>...</think>` reasoning from a model reply, defensively --
/// thinking is turned off per request (`enable_thinking: false`), but a
/// model or template that ignores that must not have its scratchpad read
/// aloud. Handles every shape seen in practice:
///
/// * complete blocks anywhere in the text (all of them are removed);
/// * a dangling `</think>` with no opener (some chat templates put the
///   opening tag in the prompt, so only the close comes back) -- everything
///   up to and including it is dropped;
/// * an unterminated `<think>` (the reply was cut off by `max_tokens` mid-
///   thought) -- everything from it onward is dropped.
pub fn strip_think(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";

    let mut rest = text;
    // A close tag before any open tag: the opener lived in the prompt.
    if let Some(close) = rest.find(CLOSE) {
        if rest.find(OPEN).is_none_or(|open| open > close) {
            rest = &rest[close + CLOSE.len()..];
        }
    }

    let mut out = String::with_capacity(rest.len());
    while let Some(open) = rest.find(OPEN) {
        out.push_str(&rest[..open]);
        match rest[open..].find(CLOSE) {
            Some(close) => rest = &rest[open + close + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// [`strip_think`], then drop any chat-template control tokens (`<|im_start|>`,
/// `<|im_end|>`, `<|eot_id|>`, ...) that leaked into the text. A healthy reply
/// never contains them; a model that has started emitting them is
/// degenerating, and they must never reach a phone screen or a TTS voice.
pub fn clean_reply(text: &str) -> String {
    let text = strip_think(text)
        .replace("<tool_call>", "")
        .replace("</tool_call>", "");
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(open) = rest.find("<|") {
        match rest[open..].find("|>") {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &rest[open + close + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    tidy_whitespace(&out)
}

/// Tidy a reply's whitespace without flattening its structure: Markdown needs
/// its newlines (headings, lists, code) and code needs its spacing. Outside
/// fenced code blocks, runs of spaces inside a line become one (leading
/// indentation is kept, for nested lists); trailing spaces go; more than one
/// blank line in a row becomes one; and the whole reply is trimmed.
fn tidy_whitespace(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut blank_run = 0;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let tidied = if in_fence || line.trim_start().starts_with("```") {
            line.to_string()
        } else {
            let indent = line.len() - line.trim_start().len();
            let body = line.split_whitespace().collect::<Vec<_>>().join(" ");
            format!("{}{}", &line[..indent], body)
        };
        if tidied.trim().is_empty() && !in_fence {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
            lines.push(String::new());
        } else {
            blank_run = 0;
            lines.push(tidied);
        }
    }
    lines.join("\n").trim().to_string()
}

/// Whether a cleaned reply is degenerate output rather than an answer: empty,
/// or long and dominated by one repeated token (`! ! ! ! ...`). Seen live from
/// Qwen3-32B on a P300x2 box, intermittently, at a few thousand prompt tokens
/// -- a model-side fault the agent can't fix, only refuse to pass along.
pub fn is_degenerate(reply: &str) -> bool {
    let tokens: Vec<&str> = reply.split_whitespace().collect();
    if tokens.is_empty() {
        return true;
    }
    if is_char_loop(reply) {
        return true;
    }
    if tokens.len() < 12 {
        return false;
    }
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in &tokens {
        *counts.entry(t).or_default() += 1;
    }
    let top = counts.values().copied().max().unwrap_or(0);
    top * 2 > tokens.len()
}

/// Loops that glue into a few long "words" (`"create_image"create_image"...`,
/// seen live): most positions start a 12-character pattern that recurs 4+
/// times -- near 1 for a loop of any period, small for prose, tables and code.
fn is_char_loop(reply: &str) -> bool {
    const WINDOW: usize = 12;
    let chars: Vec<char> = reply.chars().filter(|c| !c.is_whitespace()).collect();
    if chars.len() < 120 {
        return false;
    }
    let mut windows: std::collections::HashMap<&[char], usize> = std::collections::HashMap::new();
    for w in chars.windows(WINDOW) {
        *windows.entry(w).or_default() += 1;
    }
    let positions = chars.len() - WINDOW + 1;
    let looping = chars
        .windows(WINDOW)
        .filter(|w| windows.get(w).copied().unwrap_or(0) >= 4)
        .count();
    looping * 10 > positions * 6
}

/// What the client sees when the model's output was garbage twice in a row.
pub const GARBLED_REPLY: &str =
    "Sorry, the model returned garbled output. Please try asking again.";

/// Shrink serving-log lines for the model: drop vLLM's `(EngineCore_DP0 pid=122) `
/// process prefix, cap each line at [`MAX_LOG_LINE_CHARS`], and keep only the
/// newest lines that fit in [`MAX_LOG_TOTAL_CHARS`]. Long tool results are the
/// biggest prompt-size lever this assistant has, and prompt size is where the
/// served model has been seen to degenerate (see [`is_degenerate`]).
pub fn compact_log_lines(lines: &[String]) -> Vec<String> {
    let compact: Vec<String> = lines
        .iter()
        .map(|line| {
            let line = match (line.starts_with('('), line.find(") ")) {
                (true, Some(end)) if line[..end].contains(" pid=") => &line[end + 2..],
                _ => line.as_str(),
            };
            if line.chars().count() > MAX_LOG_LINE_CHARS {
                let cut: String = line.chars().take(MAX_LOG_LINE_CHARS).collect();
                format!("{cut}…")
            } else {
                line.to_string()
            }
        })
        .collect();
    let mut total = 0;
    let mut kept: Vec<String> = compact
        .into_iter()
        .rev()
        .take_while(|l| {
            total += l.len() + 1;
            total <= MAX_LOG_TOTAL_CHARS
        })
        .collect();
    kept.reverse();
    kept
}

/// Validate the client's history and keep only its last [`MAX_HISTORY`]
/// turns.
pub fn prepare_history(turns: Vec<ChatTurn>) -> Result<Vec<ChatTurn>, AssistantError> {
    if turns.is_empty() {
        return Err(AssistantError::BadRequest(
            "messages must contain at least one turn".to_string(),
        ));
    }
    if let Some(bad) = turns
        .iter()
        .find(|t| t.role != "user" && t.role != "assistant")
    {
        return Err(AssistantError::BadRequest(format!(
            "unsupported role {:?}: only \"user\" and \"assistant\" turns are accepted",
            bad.role
        )));
    }
    let skip = turns.len().saturating_sub(MAX_HISTORY);
    Ok(turns.into_iter().skip(skip).collect())
}

/// The `/chat/completions` URL for an `Endpoint.base_url`, pointed at
/// loopback.
///
/// The served container runs on this box, but the stored `base_url` carries
/// the agent's configured `--serving-host` (e.g. `qb2-lab.local`), which is
/// meant for LAN clients and may not even resolve from the box itself. So --
/// exactly like the telemetry `/metrics` scrape (`resolve_metrics_port`) --
/// the host is replaced with `127.0.0.1`, keeping the port and path. A
/// `base_url` with no parseable port is used unchanged.
pub fn chat_completions_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let local = trimmed
        .split_once("://")
        .and_then(|(scheme, rest)| {
            let (host_port, path) = match rest.split_once('/') {
                Some((hp, p)) => (hp, format!("/{p}")),
                None => (rest, String::new()),
            };
            let port: u16 = host_port.rsplit_once(':')?.1.parse().ok()?;
            Some(format!("{scheme}://127.0.0.1:{port}{path}"))
        })
        .unwrap_or_else(|| trimmed.to_string());
    format!("{local}/chat/completions")
}

/// Resolve a model name the LLM produced (often shortened or differently
/// cased: `"qwen3-32b"`) to a name the box actually lists
/// (`"Qwen/Qwen3-32B"`). Tried in order, first unique hit wins: exact,
/// case-insensitive, case-insensitive on the part after the last `/`, then
/// case-insensitive substring. `Err` carries up to five candidates for the
/// model to choose from (possibly none).
///
/// An empty `known` list (a backend with no catalog) can't be checked
/// against, so the name is passed through unchanged.
pub fn resolve_model(requested: &str, known: &[String]) -> Result<String, Vec<String>> {
    let requested = requested.trim();
    if known.is_empty() {
        return Ok(requested.to_string());
    }
    if let Some(hit) = known.iter().find(|k| k.as_str() == requested) {
        return Ok(hit.clone());
    }
    let want = requested.to_lowercase();
    let want_short = want.rsplit('/').next().unwrap_or(&want).to_string();
    let short = |k: &String| k.rsplit('/').next().unwrap_or(k).to_lowercase();

    let unique = |hits: Vec<&String>| (hits.len() == 1).then(|| hits[0].clone());
    if let Some(hit) = unique(known.iter().filter(|k| k.to_lowercase() == want).collect()) {
        return Ok(hit);
    }
    if let Some(hit) = unique(known.iter().filter(|k| short(k) == want_short).collect()) {
        return Ok(hit);
    }
    let contains: Vec<&String> = known
        .iter()
        .filter(|k| k.to_lowercase().contains(&want_short))
        .collect();
    if let Some(hit) = unique(contains.clone()) {
        return Ok(hit);
    }
    if !contains.is_empty() {
        return Err(contains.into_iter().take(5).cloned().collect());
    }
    Err(closest_models(&want_short, known))
}

/// Up to five `known` names sharing the most alphanumeric words with
/// `requested` (at least two), best first. This is what turns a misheard
/// "Lama 3 8B instruct" -- no substring of any real id -- into a suggestion of
/// `meta-llama/Llama-3.1-8B-Instruct` the model can retry with.
fn closest_models(requested: &str, known: &[String]) -> Vec<String> {
    let words = |s: &str| -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    };
    let want = words(requested);
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .map(|k| {
            let short = k.rsplit('/').next().unwrap_or(k);
            (want.intersection(&words(short)).count(), k)
        })
        .filter(|(score, _)| *score >= 2)
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(5).map(|(_, k)| k.clone()).collect()
}

/// Model names from a `GET /models` response for the `list_models` tool:
/// only those that list this box's `device_mesh` (case-insensitively --
/// `/status` says `p300x2`, `model_spec.json` says `P300X2`), falling back
/// to every model when the mesh is unknown or matches nothing. Downloaded
/// models first, then alphabetical; capped at [`MAX_LISTED_MODELS`].
/// Returns `(names, downloaded_names, total_before_cap)`.
pub fn models_for_box(
    resp: &libttstation::model::ModelsResponse,
    device_mesh: Option<&str>,
) -> (Vec<String>, Vec<String>, usize) {
    let fits = |m: &&libttstation::model::ModelInfo| {
        device_mesh.is_some_and(|mesh| m.devices.iter().any(|d| d.eq_ignore_ascii_case(mesh)))
    };
    let mut models: Vec<&libttstation::model::ModelInfo> =
        resp.models.iter().filter(fits).collect();
    if models.is_empty() {
        models = resp.models.iter().collect();
    }
    models.sort_by(|a, b| b.downloaded.cmp(&a.downloaded).then(a.name.cmp(&b.name)));
    let total = models.len();
    let names: Vec<String> = models
        .iter()
        .take(MAX_LISTED_MODELS)
        .map(|m| m.name.clone())
        .collect();
    let downloaded = models
        .iter()
        .filter(|m| m.downloaded)
        .take(MAX_LISTED_MODELS)
        .map(|m| m.name.clone())
        .collect();
    (names, downloaded, total)
}

/// Per-device readings from a raw `tt-smi -s` frame, trimmed (via
/// `telemetry::lite_frame`) to board type, ASIC temperature (°C) and power
/// (W). Values are passed through as tt-smi reports them (it usually quotes
/// numbers). `None` when the frame has no devices.
pub fn device_readings(tt_smi_json: &str) -> Option<Vec<Value>> {
    let lite: Value = serde_json::from_str(&crate::telemetry::lite_frame(tt_smi_json)).ok()?;
    let devices = lite.get("device_info")?.as_array()?;
    if devices.is_empty() {
        return None;
    }
    Some(
        devices
            .iter()
            .enumerate()
            .map(|(i, d)| {
                json!({
                    "device": i,
                    "board_type": d.pointer("/board_info/board_type").cloned().unwrap_or(Value::Null),
                    "temp_c": d.pointer("/telemetry/asic_temperature").cloned().unwrap_or(Value::Null),
                    "power_w": d.pointer("/telemetry/power").cloned().unwrap_or(Value::Null),
                })
            })
            .collect(),
    )
}

/// The human summary for a disruptive action. Each one says the assistant
/// goes offline -- see the module doc.
pub fn pending_summary(tool: &str, args: &Value, current_model: &str) -> String {
    match tool {
        "run_model" => format!(
            "Start serving {} — this replaces {current_model}, and I'll be offline until the new model is ready (usually several minutes).",
            args.get("model").and_then(Value::as_str).unwrap_or("the model")
        ),
        "stop_model" => format!(
            "Stop serving {current_model} — I run on it, so I'll be offline until a model is started again."
        ),
        "reset_chips" => format!(
            "Reset the Tenstorrent chips — this stops serving {current_model}, and I'll be offline until a model is started again."
        ),
        "switch_mode" => {
            let title = args.get("title").and_then(Value::as_str).unwrap_or("that mode");
            let models = args
                .get("models")
                .and_then(Value::as_array)
                .map(|m| m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" + "))
                .unwrap_or_default();
            let wait = args
                .get("eta_s")
                .and_then(Value::as_u64)
                .map(|s| format!("about {} min", s.div_ceil(60)))
                .unwrap_or_else(|| "several minutes".to_string());
            format!(
                "Switch the box to {title} ({models}) — this replaces {current_model}, and I'll be offline for {wait} while it loads."
            )
        }
        "install_app" => format!(
            "Set up {} on the box: {} It downloads the first time and runs in Docker; the model keeps serving.",
            args.get("name").and_then(Value::as_str).unwrap_or("the app"),
            args.get("blurb").and_then(Value::as_str).unwrap_or("")
        ),
        "open_app" => match (args["app"].as_str(), args["url"].as_str()) {
            (Some(app), Some(url)) => format!("Open {url} in {app}."),
            (_, Some(url)) => format!("Open {url}."),
            (Some(app), _) => format!("Open {app}."),
            _ => "Open an app.".to_string(),
        },
        "bringup_decide" => format!(
            "{} what the bring-up is waiting on: {}",
            if args["decision"] == "accept" { "Allow" } else { "Deny" },
            crate::bringup::waiting_on().unwrap_or_else(|| "nothing right now".into())
        ),
        "start_recording" => "Start recording (the microphone, until you stop it).".to_string(),
        "remember" => format!("Remember: {}", args["fact"].as_str().unwrap_or("something")),
        "read_webpage" => format!(
            "Read {} (an address that came from neither you nor this turn's searches, after reading the web).",
            args["url"].as_str().unwrap_or("a web page")
        ),
        "research" => format!("Research: {}", args["question"].as_str().unwrap_or("that")),
        other => format!("Run {other} with {args}."),
    }
}

/// The reply used when the model proposed disruptive actions without saying
/// anything itself, e.g. "I can start Qwen/Qwen3-32B — this replaces the
/// current model and I'll be offline while it loads. Confirm?"
pub fn pending_reply(proposed: &[(String, Value)]) -> String {
    let sentences: Vec<String> = proposed
        .iter()
        .map(|(tool, args)| match tool.as_str() {
            "run_model" => format!(
                "I can start {} — this replaces the current model and I'll be offline while it loads.",
                args.get("model").and_then(Value::as_str).unwrap_or("that model")
            ),
            "stop_model" => {
                "I can stop the current model — I'll be offline until another one is started."
                    .to_string()
            }
            "reset_chips" => {
                "I can reset the chips — that stops the current model and I'll be offline."
                    .to_string()
            }
            "switch_mode" => format!(
                "I can switch the box to {} — I'll be offline while it loads.",
                args.get("title").and_then(Value::as_str).unwrap_or("that mode")
            ),
            "install_app" => format!(
                "I can set up {} on the box; it downloads the first time.",
                args.get("name").and_then(Value::as_str).unwrap_or("that app")
            ),
            "open_app" | "bringup_decide" | "start_recording" | "remember" | "research" | "read_webpage" => {
                format!("I'd like to: {}", pending_summary(tool, args, ""))
            }
            other => format!("I can run {other}."),
        })
        .collect();
    format!("{} Confirm?", sentences.join(" "))
}

/// One tool call parsed out of a completion's `message.tool_calls`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed arguments, or `Err(raw)` when the model emitted arguments that
    /// aren't a JSON object.
    pub args: Result<Value, String>,
}

/// Parse `message.tool_calls`. `function.arguments` is a JSON *string* per
/// the OpenAI spec, but some servers send an object; both are accepted, and
/// an empty string means `{}`. A call with no `id` gets a generated one so
/// its `role: tool` reply can still reference it.
pub fn parse_tool_calls(message: &Value) -> Vec<ToolCall> {
    let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
        return Vec::new();
    };
    calls
        .iter()
        .enumerate()
        .filter_map(|(i, call)| {
            let function = call.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{i}"));
            let args = match function.get("arguments") {
                None | Some(Value::Null) => Ok(json!({})),
                Some(Value::Object(o)) => Ok(Value::Object(o.clone())),
                Some(Value::String(s)) if s.trim().is_empty() => Ok(json!({})),
                Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                    Ok(v @ Value::Object(_)) => Ok(v),
                    _ => Err(s.clone()),
                },
                Some(other) => Err(other.to_string()),
            };
            Some(ToolCall { id, name, args })
        })
        .collect()
}

// ---------------------------------------------------------------------
// The LLM client
// ---------------------------------------------------------------------

/// A thin async client for the served model's `/chat/completions`.
struct LlmClient {
    http: reqwest::Client,
    url: String,
    model: String,
    /// The tools offered (see [`offered_tools`]).
    tools: Value,
}

impl LlmClient {
    fn new(endpoint: &libttstation::model::Endpoint) -> Result<Self, AssistantError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(LLM_TIMEOUT)
            .build()
            .map_err(|e| AssistantError::Upstream(format!("building HTTP client: {e}")))?;
        Ok(LlmClient {
            tools: tool_definitions(),
            http,
            url: chat_completions_url(&endpoint.base_url),
            model: endpoint.model.clone(),
        })
    }

    /// The request body every completion shares; `stream` toggles SSE.
    fn body(
        &self,
        messages: &[Value],
        offer_tools: bool,
        opts: ChatOptions,
        stream: bool,
    ) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            // Deterministic for spoken box control; a little warmth for writing.
            "temperature": if opts.detailed { json!(0.3) } else { json!(0) },
            "max_tokens": opts.max_tokens(),
            // Qwen3 thinks by default; only when asked. Other models'
            // templates ignore unknown kwargs.
            "chat_template_kwargs": { "enable_thinking": opts.think },
            "stream": stream,
        });
        if offer_tools {
            body["tools"] = self.tools.clone();
            body["tool_choice"] = json!("auto");
        }
        body
    }

    /// One non-streaming completion; returns `choices[0].message`.
    /// `offer_tools: false` is the final forced-text round (no `tools` sent
    /// at all, so the model can only answer in words).
    async fn complete_with(
        &self,
        messages: &[Value],
        offer_tools: bool,
        opts: ChatOptions,
    ) -> Result<Value, AssistantError> {
        let body = self.body(messages, offer_tools, opts, false);

        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                AssistantError::Upstream(format!(
                    "couldn't reach the served model at {}: {e}",
                    self.url
                ))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let snippet: String = text.chars().take(300).collect();
            return Err(AssistantError::Upstream(format!(
                "the served model answered {status}: {snippet}"
            )));
        }
        let parsed: Value = serde_json::from_str(&text).map_err(|e| {
            AssistantError::Upstream(format!("the served model returned invalid JSON: {e}"))
        })?;
        parsed
            .pointer("/choices/0/message")
            .cloned()
            .ok_or_else(|| {
                AssistantError::Upstream("the served model's reply had no choices".to_string())
            })
    }
}

/// One piece of a streamed answer, as it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    Text(String),
    Reasoning(String),
}

/// What a streamed round produced once the model stopped.
#[derive(Debug, Default)]
pub struct StreamedRound {
    pub content: String,
    /// `[{id, type, function: {name, arguments}}]`, reassembled from deltas.
    pub tool_calls: Vec<Value>,
}

/// Splits streamed text into answer and reasoning: `<think>...</think>` goes to
/// [`Piece::Reasoning`] even when a tag is split across deltas (for models served
/// without a reasoning parser), and chat-template control tokens (`<|...|>`) are
/// dropped. Holds back only as much as could be the start of a tag.
#[derive(Debug, Default)]
pub struct ThinkFilter {
    in_think: bool,
    pending: String,
}

impl ThinkFilter {
    pub fn push(&mut self, text: &str) -> Vec<Piece> {
        self.pending.push_str(text);
        let mut out = Vec::new();
        loop {
            let tag = if self.in_think { "</think>" } else { "<think>" };
            if let Some(i) = self.pending.find(tag) {
                let before: String = self.pending[..i].to_string();
                self.emit(&before, &mut out);
                self.pending.drain(..i + tag.len());
                self.in_think = !self.in_think;
                continue;
            }
            // Keep a tail that might be the start of a tag, or of a `<|...|>`
            // control token that hasn't closed yet (capped, so ordinary text
            // containing "<|" is only ever briefly held).
            let tag_tail = (1..tag.len())
                .rev()
                .find(|&n| self.pending.ends_with(&tag[..n]))
                .unwrap_or(0);
            let token_tail = match self.pending.rfind("<|") {
                Some(at) if !self.pending[at..].contains("|>") && self.pending.len() - at <= 32 => {
                    self.pending.len() - at
                }
                _ if self.pending.ends_with('<') => 1,
                _ => 0,
            };
            let keep = tag_tail.max(token_tail).min(self.pending.len());
            let cut = self.pending.len() - keep;
            let cut = (0..=cut)
                .rev()
                .find(|&c| self.pending.is_char_boundary(c))
                .unwrap_or(0);
            let ready: String = self.pending[..cut].to_string();
            self.emit(&ready, &mut out);
            self.pending.drain(..cut);
            return out;
        }
    }

    /// Whatever is still held back, at the end of the stream.
    pub fn finish(&mut self) -> Vec<Piece> {
        let rest = std::mem::take(&mut self.pending);
        let mut out = Vec::new();
        self.emit(&rest, &mut out);
        out
    }

    fn emit(&self, text: &str, out: &mut Vec<Piece>) {
        let text = strip_control_tokens(text);
        if text.is_empty() {
            return;
        }
        out.push(if self.in_think {
            Piece::Reasoning(text)
        } else {
            Piece::Text(text)
        });
    }
}

/// Drop complete `<|...|>` chat-template tokens from a fragment.
fn strip_control_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("<|") {
        match rest[open..].find("|>") {
            Some(close) => {
                out.push_str(&rest[..open]);
                rest = &rest[open + close + 2..];
            }
            None => break,
        }
    }
    out.push_str(rest);
    // Qwen3-Coder (via Ollama) sometimes leaks its tool-call wrapper as text.
    for tag in ["<tool_call>", "</tool_call>"] {
        out = out.replace(tag, "");
    }
    out
}

impl LlmClient {
    /// One streamed completion. Text and reasoning go to `sink` as they arrive;
    /// tool-call fragments (which vLLM streams in pieces keyed by `index`) are
    /// reassembled and returned with the full text when the model stops.
    async fn stream(
        &self,
        messages: &[Value],
        offer_tools: bool,
        opts: ChatOptions,
        sink: &mut (dyn FnMut(Piece) + Send),
    ) -> Result<StreamedRound, AssistantError> {
        let body = self.body(messages, offer_tools, opts, true);
        let mut resp = self
            .http
            .post(&self.url)
            .timeout(STREAM_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                AssistantError::Upstream(format!(
                    "couldn't reach the served model at {}: {e}",
                    self.url
                ))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let snippet: String = text.chars().take(300).collect();
            return Err(AssistantError::Upstream(format!(
                "the served model answered {status}: {snippet}"
            )));
        }

        let mut round = StreamedRound::default();
        let mut filter = ThinkFilter::default();
        let mut partial: Vec<(String, String, String)> = Vec::new(); // (id, name, arguments)
        let mut buf: Vec<u8> = Vec::new();
        let mut handle = |line: &str,
                          round: &mut StreamedRound,
                          filter: &mut ThinkFilter,
                          partial: &mut Vec<(String, String, String)>| {
            let Some(payload) = line.strip_prefix("data:").map(str::trim) else {
                return;
            };
            if payload == "[DONE]" || payload.is_empty() {
                return;
            }
            let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
                return;
            };
            let Some(delta) = chunk.pointer("/choices/0/delta") else {
                return;
            };
            for key in ["reasoning_content", "reasoning"] {
                if let Some(r) = delta
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|r| !r.is_empty())
                {
                    sink(Piece::Reasoning(r.to_string()));
                }
            }
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                for piece in filter.push(text) {
                    if let Piece::Text(t) = &piece {
                        round.content.push_str(t);
                    }
                    sink(piece);
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for (n, call) in calls.iter().enumerate() {
                    let index = call
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|i| i as usize)
                        .unwrap_or(n);
                    while partial.len() <= index {
                        partial.push((String::new(), String::new(), String::new()));
                    }
                    let slot = &mut partial[index];
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        slot.0 = id.to_string();
                    }
                    if let Some(f) = call.get("function") {
                        if let Some(name) = f.get("name").and_then(Value::as_str) {
                            slot.1.push_str(name);
                        }
                        if let Some(args) = f.get("arguments").and_then(Value::as_str) {
                            slot.2.push_str(args);
                        }
                    }
                }
            }
        };

        loop {
            let chunk = resp.chunk().await.map_err(|e| {
                AssistantError::Upstream(format!("the model's stream broke off: {e}"))
            })?;
            let Some(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                handle(
                    String::from_utf8_lossy(&line).trim(),
                    &mut round,
                    &mut filter,
                    &mut partial,
                );
            }
        }
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf).to_string();
            handle(line.trim(), &mut round, &mut filter, &mut partial);
        }
        for piece in filter.finish() {
            if let Piece::Text(t) = &piece {
                round.content.push_str(t);
            }
            sink(piece);
        }
        round.tool_calls = partial
            .into_iter()
            .enumerate()
            .filter(|(_, (_, name, _))| !name.is_empty())
            .map(|(i, (id, name, args))| {
                json!({
                    "id": if id.is_empty() { format!("call_{i}") } else { id },
                    "type": "function",
                    "function": { "name": name, "arguments": args },
                })
            })
            .collect();
        Ok(round)
    }
}

/// A streamed chat's events, in the order a client receives them. Serialized
/// as `{"type": "...", ...}` -- one per SSE `data:` line.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// More of the answer.
    Delta { text: String },
    /// More of the model's reasoning (only with `think`).
    Reasoning { text: String },
    /// A tool the assistant ran or wants confirmed.
    Action { action: ActionReport },
    /// Throw away the streamed answer so far (it was garbled; a retry follows).
    Reset,
    /// The authoritative final reply (cleaned); replaces the streamed text.
    Done { reply: String, model: String },
    /// Something failed mid-stream.
    Error { error: String },
}

// ---------------------------------------------------------------------
// The chat loop
// ---------------------------------------------------------------------

/// `POST /assistant`'s body: ask the served model, run read-only tools,
/// park disruptive ones. See the module doc.
pub(crate) async fn converse(
    state: &AppState,
    turns: Vec<ChatTurn>,
    opts: ChatOptions,
) -> Result<AssistantResponse, AssistantError> {
    let (llm, mut messages, model) = prepare_chat(state, turns, opts).await?;
    let respond = |reply: String, actions: Vec<ActionReport>| AssistantResponse {
        reply,
        actions,
        model: model.clone(),
    };
    let mut actions: Vec<ActionReport> = Vec::new();
    let endpoint_model = model.clone();
    converse_rounds(
        state,
        &llm,
        &mut messages,
        opts,
        &endpoint_model,
        &mut actions,
    )
    .await
    .map(|reply| respond(reply, actions))
}

/// Everything both chat routes do before the first model call: validate the
/// history, make sure a chat model is serving, and build the messages.
async fn prepare_chat(
    state: &AppState,
    turns: Vec<ChatTurn>,
    opts: ChatOptions,
) -> Result<(LlmClient, Vec<Value>, String), AssistantError> {
    let history = prepare_history(turns)?;

    // Reconcile first (like `GET /endpoint`): a model that died out of band
    // must read as "nothing serving", not as a dead URL to time out against.
    let _ = state.reconciled_status().await;
    // The assistant needs a chat model; a media server would answer 404.
    let served = match state.endpoint() {
        Some(endpoint) => {
            let probe = state.clone();
            let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
                .await
                .unwrap_or("chat");
            Some((endpoint, kind))
        }
        None => None,
    };
    let (endpoint, on_cpu) = match served {
        Some((endpoint, kind)) if kind == "chat" || kind == "other" => (endpoint, None),
        // The chips are busy with something else (or idle): Tori thinks on the CPU.
        other => match if state.cpu_brain_enabled() {
            cpu_brain(state).await
        } else {
            None
        } {
            Some(cpu) => {
                let why = match &other {
                    Some((ep, kind)) => format!(
                        "the chips are serving {} ({kind}), not a chat model",
                        ep.model
                    ),
                    None => "nothing is serving on the chips".to_string(),
                };
                (cpu, Some(why))
            }
            None => {
                if other.is_none() {
                    if let Some(waking) = crate::idle::wake_for_request(state).await {
                        return Err(AssistantError::NotChat(waking));
                    }
                }
                return Err(match other {
                    Some((ep, kind)) => AssistantError::NotChat(format!(
                        "The box is serving {} ({kind}), and Tori needs a chat model. Start one \
                         like Qwen3-32B to chat.",
                        ep.model
                    )),
                    None => AssistantError::NotServing,
                });
            }
        },
    };
    let mut llm = LlmClient::new(&endpoint)?;
    llm.tools = offered_tools(state, opts.tori).await;

    let ctx = BoxContext {
        name: state.name().to_string(),
        chips: state.chips().to_string(),
        device_mesh: state.device_mesh().map(str::to_string),
        model: endpoint.model.clone(),
        documents: state
            .docs()
            .map(|d| d.list().into_iter().map(|s| s.name).collect())
            .unwrap_or_default(),
        layout: split_note(state),
        memories: state
            .memories()
            .prompt_note(crate::people::current().as_ref()),
    };
    let mut prompt = system_prompt_styled(&ctx, opts.detailed);
    if opts.tori {
        prompt = prompt.replacen(
            "You are the assistant for",
            "You are Tori, the assistant for",
            1,
        );
        prompt.push_str(TORI_NOTE);
        if opts.client {
            prompt.push_str(CLIENT_NOTE);
        }
    } else {
        prompt.push_str(
            "\nThis chat can't open apps or change things on the box's own desktop; Tori, \
             the assistant summoned with Ctrl+Alt+Space, does that. Say so if asked.",
        );
    }
    if let Some(why) = &on_cpu {
        prompt.push_str(&format!(
            "\nRight now you are running on the box's CPU ({}), because {why}. Switching \
             modes or starting a model does not take you offline. When you need a tool, call \
             it straight away without writing anything first, then answer from its result.",
            endpoint.model
        ));
    }
    let mut messages = vec![json!({ "role": "system", "content": prompt })];
    messages.extend(
        history
            .into_iter()
            .map(|t| json!({ "role": t.role, "content": t.content })),
    );
    Ok((llm, messages, endpoint.model.clone()))
}

/// `GET /assistant/brain` (authed): where Tori thinks right now --
/// `{"where": "chips" | "cpu" | null, "model"}` -- so apps can offer her even
/// when the chips are busy with pictures or speech.
pub(crate) async fn get_brain(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: crate::routes::BearerAuth,
) -> axum::Json<Value> {
    if let Some(endpoint) = state.endpoint() {
        let probe = state.clone();
        let kind = tokio::task::spawn_blocking(move || probe.served_kind_blocking())
            .await
            .unwrap_or("chat");
        if kind == "chat" || kind == "other" {
            return axum::Json(json!({ "where": "chips", "model": endpoint.model }));
        }
    }
    if state.cpu_brain_enabled() {
        if let Some(cpu) = cpu_brain(&state).await {
            return axum::Json(json!({ "where": "cpu", "model": cpu.model }));
        }
    }
    axum::Json(json!({ "where": null, "model": null }))
}

/// What Tori knows about herself, on top of the box assistant's prompt.
const TORI_NOTE: &str =
    "\nYour name is Tori. You are summoned with a keyboard shortcut over whatever \
the user is doing, so be quick and brief. You run this computer too: open apps and web pages \
(open_app; call it once per app), open folders and files (open_folder, find_files), and change \
the volume, media playback, dark mode, do not disturb, or lock the screen (control_computer). \
When asked to do one of these, call the tool; never say something is open or done unless a \
tool result says so.";

/// Tori on another computer: which one "this computer" is.
const CLIENT_NOTE: &str = "\nThe person is using you from their own computer, not the box: \
open_app, open_folder, find_files, control_computer and recordings act on THEIR computer. \
Everything about models, chips and the box still acts on the box.";

/// Tools that act on "this computer": on the person's computer when Tori runs
/// there (`ChatOptions::client`).
pub const CLIENT_TOOLS: &[&str] = &[
    "open_app",
    "control_computer",
    "open_folder",
    "find_files",
    "open_station",
    "start_recording",
    "stop_recording",
];

static CLIENT_WAITS: LazyLock<Mutex<HashMap<String, tokio::sync::oneshot::Sender<Value>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Has the person's computer run `call` and waits for what happened (30 s at most).
async fn run_on_client(
    call: &ToolCall,
    on_action: &mut (dyn FnMut(&ActionReport) + Send),
) -> (Value, ActionReport) {
    let args = call.args.clone().unwrap_or_else(|_| json!({}));
    let id = new_action_id();
    let (tx, rx) = tokio::sync::oneshot::channel();
    CLIENT_WAITS
        .lock()
        .expect("client waits")
        .insert(id.clone(), tx);
    let mut report = ActionReport {
        id: id.clone(),
        tool: call.name.clone(),
        args,
        status: ActionStatus::OnClient,
        summary: "On your computer…".into(),
        images: vec![],
    };
    on_action(&report);
    let answer = tokio::time::timeout(Duration::from_secs(30), rx).await;
    CLIENT_WAITS.lock().expect("client waits").remove(&id);
    match answer {
        Ok(Ok(v)) if v["ok"] == true => {
            report.status = ActionStatus::Done;
            report.summary = v["summary"].as_str().unwrap_or("Done").to_string();
            (
                if v["result"].is_null() {
                    json!({ "done": report.summary })
                } else {
                    v["result"].clone()
                },
                report,
            )
        }
        Ok(Ok(v)) => {
            let e = v["error"].as_str().unwrap_or("it didn't work").to_string();
            report.status = ActionStatus::Error;
            report.summary = e.clone();
            (json!({ "error": e }), report)
        }
        _ => {
            report.status = ActionStatus::Error;
            report.summary = "Your computer didn't answer".into();
            (
                json!({ "error": "the person's computer didn't answer (TT Station may be closed there)" }),
                report,
            )
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ClientResult {
    pub id: String,
    #[serde(flatten)]
    pub outcome: Value,
}

/// `POST /assistant/client-result {id, ok, result?, summary?, error?}`: what a
/// computer tool did on the person's computer.
pub(crate) async fn post_client_result(
    _auth: crate::routes::BearerAuth,
    axum::Json(r): axum::Json<ClientResult>,
) -> axum::http::StatusCode {
    match CLIENT_WAITS.lock().expect("client waits").remove(&r.id) {
        Some(tx) => {
            let _ = tx.send(r.outcome);
            axum::http::StatusCode::NO_CONTENT
        }
        None => axum::http::StatusCode::NOT_FOUND,
    }
}

/// Did the reply claim to have done something on the computer?
fn claims_done(reply: &str) -> bool {
    let r = reply.to_lowercase();
    [
        "opened",
        "opening",
        "i've launched",
        "launched",
        "i've started",
        "is now open",
        "are now open",
        "turned the volume",
        "volume is now",
        "paused",
        "locked",
    ]
    .iter()
    .any(|w| r.contains(w))
}

/// Tori's instant commands (see `tori.rs`): done here without a model.
pub async fn quick(
    state: &AppState,
    turns: &[ChatTurn],
    opts: ChatOptions,
) -> Option<AssistantResponse> {
    if !opts.tori {
        return None;
    }
    let said = turns.last().filter(|t| t.role == "user")?.content.clone();
    let steps = tokio::task::spawn_blocking(move || {
        let apps = crate::desktop::apps();
        crate::tori::parse(&said, &apps)
    })
    .await
    .ok()??;
    use crate::tori::Step;
    // "yes" with nothing waiting for a yes is just conversation.
    let pending = state.latest_pending_action(Duration::from_secs(180));
    if let [Step::Confirm(yes)] = steps.as_slice() {
        if pending.is_none() {
            // Nothing of Tori's to confirm: maybe a bring-up is waiting on a yes.
            if crate::bringup::anything_waiting() {
                let step = Step::Tool(
                    "bringup_decide",
                    json!({ "decision": if *yes { "accept" } else { "decline" } }),
                );
                let call = ToolCall {
                    id: new_action_id(),
                    name: "bringup_decide".into(),
                    args: Ok(step.tool().1),
                };
                let (result, report) = run_read_only(state, &call).await;
                let reply = result["what"]
                    .as_str()
                    .map(|w| {
                        if *yes {
                            format!("Allowed. It's running `{w}` now.")
                        } else {
                            format!("Denied `{w}`. It'll find another way or stop.")
                        }
                    })
                    .unwrap_or_else(|| "There's nothing waiting for a yes.".into());
                return Some(AssistantResponse {
                    reply,
                    actions: vec![report],
                    model: "Tori".into(),
                });
            }
            return None;
        }
    }
    let current = state.endpoint().map(|e| e.model).unwrap_or_default();
    let mut actions = Vec::new();
    let mut done = Vec::new();
    let mut skip_rest = false;
    for step in steps {
        if skip_rest {
            break;
        }
        match &step {
            Step::Help => {
                done.push((step, Ok(json!({}))));
                continue;
            }
            Step::Confirm(approve) => {
                let Some(id) = pending.clone() else { continue };
                let outcome = confirm(
                    state,
                    ConfirmRequest {
                        action_id: id,
                        approve: *approve,
                    },
                )
                .await
                .map(|r| {
                    let summary = if *approve {
                        r.summary
                    } else {
                        "Okay, I won't.".to_string()
                    };
                    json!({ "summary": summary })
                })
                .map_err(|e| format!("{e:?}"));
                done.push((step, outcome));
                continue;
            }
            _ => {}
        }
        let (name, args) = step.tool();
        let call = ToolCall {
            id: new_action_id(),
            name: name.to_string(),
            args: Ok(args),
        };
        let (result, report) = if tool_kind(name) == Some(ToolKind::Disruptive) {
            match propose(state, &call, &current).await {
                Ok(pair) => pair,
                Err(e) => (
                    json!({ "error": e }),
                    ActionReport {
                        id: call.id.clone(),
                        tool: call.name.clone(),
                        args: call.args.clone().unwrap_or_default(),
                        status: ActionStatus::Error,
                        summary: e,
                        images: Vec::new(),
                    },
                ),
            }
        } else {
            run_read_only(state, &call).await
        };
        let outcome = match result.get("error").and_then(Value::as_str) {
            Some(e) => Err(e.to_string()),
            None => Ok(result),
        };
        // No image model: offer to switch the box so pictures work.
        if name == "create_image" {
            if let Err(e) = &outcome {
                if e.contains("no image model") {
                    skip_rest = true;
                    let offer = ToolCall {
                        id: new_action_id(),
                        name: "switch_mode".into(),
                        args: Ok(json!({ "mode": "chat_images" })),
                    };
                    let offered = match propose(state, &offer, &current).await {
                        Ok(pair) => Some(pair),
                        Err(_) => propose(
                            state,
                            &ToolCall {
                                args: Ok(json!({ "mode": "images" })),
                                ..offer
                            },
                            &current,
                        )
                        .await
                        .ok(),
                    };
                    if let Some((r, report)) = offered {
                        actions.push(report);
                        done.push((
                            Step::Tool("switch_mode", json!({})),
                            Ok(json!({
                                "status": "needs_confirmation",
                                "summary": format!(
                                    "No image model is running. {}",
                                    r["summary"].as_str().unwrap_or("")
                                ),
                            })),
                        ));
                        continue;
                    }
                }
            }
        }
        actions.push(report);
        done.push((step, outcome));
    }
    Some(AssistantResponse {
        reply: crate::tori::reply(&done),
        actions,
        model: "Tori".into(),
    })
}

/// The CPU model Tori falls back to when the chips aren't serving a chat
/// model: a local Ollama (`TT_STATION_CPU_MODEL`, default Qwen3-Coder 30B, a
/// mixture-of-experts that runs at ~18 tokens/s on the box's Ryzen).
pub const CPU_MODEL_DEFAULT: &str = "qwen3-coder:30b";
const OLLAMA: &str = "http://127.0.0.1:11434";

pub async fn cpu_brain(state: &AppState) -> Option<libttstation::model::Endpoint> {
    let model = crate::prefs::load(state)
        .cpu_chat
        .or_else(|| std::env::var("TT_STATION_CPU_MODEL").ok())
        .unwrap_or_else(|| CPU_MODEL_DEFAULT.into());
    if model.is_empty() || model == "off" {
        return None;
    }
    let tags: Value = reqwest::Client::new()
        .get(format!("{OLLAMA}/api/tags"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let installed = tags["models"].as_array()?.iter().any(|m| {
        m["name"].as_str() == Some(model.as_str()) || m["model"].as_str() == Some(model.as_str())
    });
    if installed {
        // Keep it loaded for a while, so the next question doesn't wait for a load.
        let warm = model.clone();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .post(format!("{OLLAMA}/api/generate"))
                .json(&json!({ "model": warm, "keep_alive": "2h" }))
                .timeout(Duration::from_secs(120))
                .send()
                .await;
        });
    }
    installed.then(|| libttstation::model::Endpoint {
        base_url: format!("{OLLAMA}/v1"),
        model,
        requires_key: false,
    })
}

/// The non-streamed tool loop; returns the final reply.
async fn converse_rounds(
    state: &AppState,
    llm: &LlmClient,
    messages: &mut Vec<Value>,
    opts: ChatOptions,
    served_model: &str,
    actions: &mut Vec<ActionReport>,
) -> Result<String, AssistantError> {
    // Rounds 0..MAX_TOOL_ROUNDS offer tools; the last round doesn't, which
    // forces a plain-text answer out of a model stuck calling tools.
    // Whether web text has come into this turn, and the addresses it knows
    // (see `needs_confirmation`, `fetches_unknown_address`).
    let mut turn = crate::urltrust::Turn::from_messages(messages);
    for round in 0..=MAX_TOOL_ROUNDS {
        let offer_tools = round < MAX_TOOL_ROUNDS;
        let read = |message: &Value| {
            let content = clean_reply(message.get("content").and_then(Value::as_str).unwrap_or(""));
            let calls = if offer_tools {
                parse_tool_calls(message)
            } else {
                Vec::new()
            };
            (content, calls)
        };
        let mut message = llm.complete_with(messages, offer_tools, opts).await?;
        let (mut content, mut calls) = read(&message);
        // The served model has been seen to emit garbage intermittently (see
        // `is_degenerate`); one retry of the same request usually recovers.
        if calls.is_empty() && is_degenerate(&content) {
            eprintln!("tt-station-agentd: assistant: degenerate model output, retrying once");
            message = llm.complete_with(messages, offer_tools, opts).await?;
            (content, calls) = read(&message);
        }

        if calls.is_empty() {
            let reply = if is_degenerate(&content) {
                eprintln!("tt-station-agentd: assistant: degenerate model output again, giving up");
                GARBLED_REPLY.to_string()
            } else {
                content
            };
            return Ok(reply);
        }

        let tool_calls = message.get("tool_calls").cloned().unwrap_or(json!([]));
        let proposed = run_tool_calls(
            state,
            messages,
            &content,
            tool_calls,
            &calls,
            served_model,
            actions,
            // Not streamed: nothing reaches the person's computer, so the box's own tools.
            false,
            &mut turn,
            &mut |_| {},
        )
        .await;
        // A disruptive proposal ends the turn: the human decides next.
        if !proposed.is_empty() {
            return Ok(if content.is_empty() {
                pending_reply(&proposed)
            } else {
                content
            });
        }
    }

    // Unreachable in practice (the final round offers no tools, so it always
    // returns above), but a plain answer beats a panic.
    Ok("Sorry, I couldn't finish that.".to_string())
}

/// Act on one round's tool calls: echo the assistant turn, run read-only tools,
/// park disruptive ones, and feed every result back as a `role: tool` message.
/// Each [`ActionReport`] also goes to `on_action` as it happens (the stream
/// sends it to the client). Returns the disruptive proposals, if any.
#[allow(clippy::too_many_arguments)]
async fn run_tool_calls(
    state: &AppState,
    messages: &mut Vec<Value>,
    content: &str,
    tool_calls: Value,
    calls: &[ToolCall],
    served_model: &str,
    actions: &mut Vec<ActionReport>,
    client: bool,
    turn: &mut crate::urltrust::Turn,
    on_action: &mut (dyn FnMut(&ActionReport) + Send),
) -> Vec<(String, Value)> {
    // Echo the assistant turn (with its tool calls) back so the next round's
    // `role: tool` messages have something to answer. Content is the
    // think-stripped text, and "" rather than null: some chat templates choke
    // on a null content.
    //
    // Several calls in one turn are echoed as consecutive single-call turns,
    // each followed by its result: Llama's chat template rejects an assistant
    // turn with more than one tool call ("This model only supports single
    // tool-calls at once!", seen live after a double `remember`), and the split
    // form means the same thing to every model.
    let raw: Vec<Value> = tool_calls.as_array().cloned().unwrap_or_default();
    let split = raw.len() > 1 && raw.len() == calls.len();
    if !split {
        messages.push(json!({ "role": "assistant", "content": content, "tool_calls": tool_calls }));
    }

    let mut proposed: Vec<(String, Value)> = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        if split {
            messages.push(json!({
                "role": "assistant",
                "content": if i == 0 { content } else { "" },
                "tool_calls": [raw[i].clone()],
            }));
        }
        let args = call.args.as_ref().unwrap_or(&Value::Null);
        let gated = tool_kind(&call.name) == Some(ToolKind::ReadOnly)
            && (needs_confirmation(&call.name, args, turn.tainted())
                || fetches_unknown_address(&call.name, args, turn));
        let result = match tool_kind(&call.name) {
            Some(ToolKind::ReadOnly) if gated => match park(
                state,
                call,
                client && CLIENT_TOOLS.contains(&call.name.as_str()),
            ) {
                Ok((pending, report)) => {
                    proposed.push((report.tool.clone(), report.args.clone()));
                    on_action(&report);
                    actions.push(report);
                    pending
                }
                Err(reason) => json!({ "error": reason }),
            },
            Some(ToolKind::ReadOnly) => {
                let (result, report) = if client && CLIENT_TOOLS.contains(&call.name.as_str()) {
                    run_on_client(call, on_action).await
                } else {
                    run_read_only(state, call).await
                };
                on_action(&report);
                actions.push(report);
                result
            }
            Some(ToolKind::Disruptive) => match propose(state, call, served_model).await {
                Ok((pending, report)) => {
                    proposed.push((report.tool.clone(), report.args.clone()));
                    on_action(&report);
                    actions.push(report);
                    pending
                }
                Err(reason) => json!({ "error": reason }),
            },
            None => json!({
                "error": format!(
                    "unknown tool '{}'; available tools: {}",
                    call.name,
                    READ_ONLY_TOOLS
                        .iter()
                        .chain(DISRUPTIVE_TOOLS.iter())
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
        };
        if WEB_TOOLS.contains(&call.name.as_str()) && !gated {
            turn.web_result(&result.to_string());
        }
        messages.push(json!({
            "role": "tool",
            "tool_call_id": call.id,
            "name": call.name,
            "content": result.to_string(),
        }));
    }
    proposed
}

/// Marks a parked action that the person's computer, not the box, runs once approved.
const ON_CLIENT: &str = "_on_client";

/// Parks a model's call of an acting tool until the person confirms it (see
/// [`needs_confirmation`]); `confirm` runs it, or hands it back to the
/// person's computer when it acts there.
fn park(
    state: &AppState,
    call: &ToolCall,
    on_client: bool,
) -> Result<(Value, ActionReport), String> {
    let args = call
        .args
        .clone()
        .map_err(|raw| format!("arguments were not a JSON object: {raw}"))?;
    let summary = pending_summary(&call.name, &args, "");
    let mut stored = args.clone();
    if call.name == "bringup_decide" {
        // The very request the person is shown, not whatever is waiting when they answer.
        stored["what"] =
            json!(crate::bringup::waiting_on().ok_or("the bring-up isn't waiting on anything")?);
    }
    if on_client {
        stored[ON_CLIENT] = json!(true);
    }
    let id = state.insert_pending_action(PendingAction {
        tool: call.name.clone(),
        args: stored,
        summary: summary.clone(),
        created: Instant::now(),
    });
    let result = json!({
        "status": "needs_confirmation",
        "summary": summary,
        "note": "Not done yet. Ask the user to confirm.",
    });
    let report = ActionReport {
        id,
        tool: call.name.clone(),
        args,
        status: ActionStatus::NeedsConfirmation,
        summary,
        images: Vec::new(),
    };
    Ok((result, report))
}

/// Everything `POST /assistant/stream` needs before streaming starts -- failures
/// here become plain HTTP errors (`409` nothing/non-chat serving, `400`).
pub(crate) struct PreparedChat {
    llm: LlmClient,
    messages: Vec<Value>,
    model: String,
    opts: ChatOptions,
}

pub(crate) async fn prepare_stream(
    state: &AppState,
    turns: Vec<ChatTurn>,
    opts: ChatOptions,
) -> Result<PreparedChat, AssistantError> {
    let (llm, messages, model) = prepare_chat(state, turns, opts).await?;
    Ok(PreparedChat {
        llm,
        messages,
        model,
        opts,
    })
}

/// The streamed tool loop: like `converse`, but every round streams, sending
/// [`StreamEvent`]s to `tx` as they happen. Always ends with `Done` or `Error`.
pub(crate) async fn converse_stream(
    state: AppState,
    chat: PreparedChat,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    let PreparedChat {
        llm,
        mut messages,
        model,
        opts,
    } = chat;
    let send = |e: StreamEvent| {
        let _ = tx.send(e);
    };
    let mut actions: Vec<ActionReport> = Vec::new();
    let mut streamed_any = false;
    // Whether web text has come into this turn, and the addresses it knows
    // (see `needs_confirmation`, `fetches_unknown_address`).
    let mut turn = crate::urltrust::Turn::from_messages(&messages);
    for round in 0..=MAX_TOOL_ROUNDS {
        let offer_tools = round < MAX_TOOL_ROUNDS;
        if streamed_any {
            // Separate a preamble ("Let me check...") from what follows.
            send(StreamEvent::Delta {
                text: "\n\n".to_string(),
            });
        }
        let mut sink = |piece: Piece| match piece {
            Piece::Text(text) => {
                streamed_any = true;
                let _ = tx.send(StreamEvent::Delta { text });
            }
            Piece::Reasoning(text) => {
                let _ = tx.send(StreamEvent::Reasoning { text });
            }
        };
        let result = llm.stream(&messages, offer_tools, opts, &mut sink).await;
        let streamed = match result {
            Ok(r) => r,
            Err(e) => {
                send(StreamEvent::Error {
                    error: assistant_error_text(&e),
                });
                return;
            }
        };
        let content = clean_reply(&streamed.content);
        let tool_calls = if offer_tools {
            json!(streamed.tool_calls)
        } else {
            json!([])
        };
        let calls = parse_tool_calls(&json!({ "tool_calls": tool_calls.clone() }));

        if calls.is_empty()
            && opts.tori
            && actions.is_empty()
            && offer_tools
            && round == 0
            && claims_done(&content)
        {
            // Said it did something without calling a tool: take that back
            // and make it actually do it.
            eprintln!(
                "tt-station-agentd: tori: reply claimed an action without a tool call; retrying"
            );
            send(StreamEvent::Reset);
            streamed_any = false;
            messages.push(json!({ "role": "assistant", "content": content }));
            messages.push(json!({
                "role": "user",
                "content": "You haven't actually done that: nothing was called. Call the tool now (once per app or action), then answer in one short sentence.",
            }));
            continue;
        }
        if calls.is_empty() {
            let reply = if is_degenerate(&content) {
                // Garbled: tell the client to drop what it showed, retry once
                // without streaming, and send the retry as one piece.
                eprintln!(
                    "tt-station-agentd: assistant: degenerate streamed output, retrying once"
                );
                send(StreamEvent::Reset);
                match llm.complete_with(&messages, false, opts).await {
                    Ok(message) => {
                        let retry = clean_reply(
                            message.get("content").and_then(Value::as_str).unwrap_or(""),
                        );
                        if is_degenerate(&retry) {
                            GARBLED_REPLY.to_string()
                        } else {
                            retry
                        }
                    }
                    Err(_) => GARBLED_REPLY.to_string(),
                }
            } else {
                content
            };
            send(StreamEvent::Done { reply, model });
            return;
        }

        // Some models (Qwen3-Coder on the CPU) write a whole guessed answer
        // before calling a tool. Keep a short preamble; retract a guess.
        if content.len() > 120 {
            send(StreamEvent::Reset);
            streamed_any = false;
        }
        let proposed = run_tool_calls(
            &state,
            &mut messages,
            &content,
            tool_calls,
            &calls,
            &model,
            &mut actions,
            opts.client,
            &mut turn,
            &mut |report| {
                let _ = tx.send(StreamEvent::Action {
                    action: report.clone(),
                });
            },
        )
        .await;
        if !proposed.is_empty() {
            let reply = if content.is_empty() {
                pending_reply(&proposed)
            } else {
                content
            };
            send(StreamEvent::Done { reply, model });
            return;
        }
    }
    send(StreamEvent::Done {
        reply: "Sorry, I couldn't finish that.".to_string(),
        model,
    });
}

/// A mid-stream failure, phrased for a person.
fn assistant_error_text(e: &AssistantError) -> String {
    match e {
        AssistantError::Upstream(msg) => msg.clone(),
        AssistantError::NotChat(msg) | AssistantError::BadRequest(msg) => msg.clone(),
        AssistantError::NotServing => "No model is serving — start one first".to_string(),
        other => format!("{other:?}"),
    }
}

/// A fresh random action id (same generator as `pair_id`).
fn new_action_id() -> String {
    crate::pairing::issue_pair_id()
}

/// Run a read-only tool. Returns the JSON result for the model and the
/// report for the client. Never fails outright: a failure becomes an
/// `{"error": ..}` result (so the model can explain it) and an `error`
/// report.
pub(crate) async fn run_read_only(state: &AppState, call: &ToolCall) -> (Value, ActionReport) {
    let args = call.args.clone().unwrap_or_else(|_| json!({}));
    let outcome: Result<(Value, String), String> = match call.name.as_str() {
        "box_status" => Ok(box_status(state).await),
        "list_models" => list_models(state).await,
        "recent_logs" => recent_logs(state, &args).await,
        "search_documents" => search_documents(state, &args).await,
        "create_image" => create_image(state, &args).await,
        "web_search" => web_search(&args).await,
        "research_check" => {
            let c = crate::research::check(state).await;
            let mut todo = Vec::new();
            if c["ok"] != true {
                todo.push(
                    c["why"]
                        .as_str()
                        .unwrap_or("a model must be serving")
                        .to_string(),
                );
            }
            if c["search"] != "searxng" {
                todo.push("search goes to DuckDuckGo; installing SearXNG (install_app searxng) makes it private".to_string());
            }
            let line = if todo.is_empty() {
                "Research is ready: a model is serving and search is private.".to_string()
            } else {
                format!("To do: {}", todo.join("; "))
            };
            Ok((
                json!({ "ready": c["ok"], "private_search": c["search"] == "searxng", "model": c["model"], "context": c["context"], "to_do": todo }),
                line,
            ))
        }
        "research" => {
            let depth = match args["depth"].as_str() {
                Some("quick") => crate::research::Depth::Quick,
                Some("deep") => crate::research::Depth::Deep,
                _ => crate::research::Depth::Standard,
            };
            let question = args["question"].as_str().unwrap_or("").to_string();
            match crate::research::start(
                state,
                crate::research::StartBody {
                    question,
                    depth: Some(depth),
                    parent: None,
                },
            )
            .await
            {
                Ok(job) => Ok((
                    json!({ "started": true, "id": job.id, "note": "It runs in the background; the user gets a notification when the report is ready, and can follow it in TT Station's Research page." }),
                    "Started researching it".to_string(),
                )),
                Err((_, e)) => Err(e.0["error"]
                    .as_str()
                    .unwrap_or("couldn't start the research")
                    .to_string()),
            }
        }
        "remember" => remember(state, &args),
        "forget" => forget(state, &args),
        "read_webpage" => read_webpage(&args).await,
        "open_app" => open_app(&args).await,
        "control_computer" => {
            let action = args["action"].as_str().unwrap_or("").to_string();
            let value = args["value"].as_f64();
            tokio::task::spawn_blocking(move || crate::desktop::control(&action, value))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map(|r| {
                    let s = format!("{} on the box", r["done"].as_str().unwrap_or("done"));
                    let s = s[..1].to_uppercase() + &s[1..];
                    (r, s)
                })
        }
        "open_folder" => {
            let path = args["path"].as_str().unwrap_or("").to_string();
            tokio::task::spawn_blocking(move || crate::desktop::open_path(&path))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map(|r| {
                    let s = format!("Opened {} on the box", r["opened"].as_str().unwrap_or("it"));
                    (r, s)
                })
        }
        "bringup_status" => {
            let st = state.clone();
            tokio::task::spawn_blocking(move || crate::bringup::status_line(&st))
                .await
                .map_err(|e| e.to_string())
                .map(|(v, line)| (json!({ "status": line, "detail": v["state"] }), line))
        }
        "bringup_decide" => {
            let decision = match args["decision"].as_str() {
                Some("accept") => "accept",
                _ => "decline",
            };
            match crate::bringup::decide_waiting(decision) {
                Some((d, what)) => {
                    let said = if d == "accept" {
                        format!("Allowed: {what}")
                    } else {
                        format!("Denied: {what}")
                    };
                    Ok((json!({ "decided": d, "what": what }), said))
                }
                None => Err("the bring-up isn't waiting on anything".into()),
            }
        }
        "start_recording" => {
            let title = args["title"].as_str().map(str::to_string);
            crate::recorder::start(
                state,
                crate::recorder::StartBody {
                    title,
                    ..Default::default()
                },
            )
            .await
            .map(|(_, r)| (r.0, "Started recording".to_string()))
            .map_err(|(_, e)| e.0.error)
        }
        "stop_recording" => crate::recorder::stop(state, false)
            .await
            .map(|r| (r.0, "Stopped recording; transcribing".to_string()))
            .map_err(|(_, e)| e.0.error),
        "open_station" => {
            let page = args["page"].as_str().unwrap_or("home").to_string();
            tokio::task::spawn_blocking(move || crate::desktop::open_station(&page))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map(|r| {
                    let s = format!(
                        "Opened {} in TT Station",
                        r["page"].as_str().unwrap_or("it")
                    );
                    (r, s)
                })
        }
        "find_files" => {
            let name = args["name"].as_str().unwrap_or("").to_string();
            tokio::task::spawn_blocking(move || crate::desktop::find_files(&name))
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r)
                .map(|r| {
                    let n = r["matches"].as_array().map_or(0, Vec::len);
                    (r, format!("Found {n} matching file(s)"))
                })
        }
        other => Err(format!("{other} is not a read-only tool")),
    };
    let (result, status, summary) = match outcome {
        Ok((result, summary)) => (result, ActionStatus::Done, summary),
        Err(e) => (
            json!({ "error": e }),
            ActionStatus::Error,
            format!("{} failed: {e}", call.name),
        ),
    };
    let report = ActionReport {
        id: new_action_id(),
        tool: call.name.clone(),
        images: result["image_ids"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        args,
        status,
        summary,
    };
    (result, report)
}

/// `open_app`: open an app or web page on the box's desktop.
async fn open_app(args: &Value) -> Result<(Value, String), String> {
    let app = args["app"].as_str().map(str::to_string);
    let url = args["url"].as_str().map(str::to_string);
    let result =
        tokio::task::spawn_blocking(move || crate::desktop::open(app.as_deref(), url.as_deref()))
            .await
            .map_err(|e| e.to_string())??;
    let summary = if result["ambiguous"] == true {
        "Found several matching apps".to_string()
    } else {
        format!(
            "Opened {} on the box",
            result["opened"].as_str().unwrap_or("it")
        )
    };
    Ok((result, summary))
}

/// `box_status`: cheap, in-memory facts plus per-device temperature/power.
/// Telemetry comes from a recent cached `tt-smi` snapshot when a `/telemetry`
/// viewer has already paid for one, else from one fresh, deduplicated
/// `tt-smi -s` (`AppState::cached_snapshot`) bounded by
/// [`TELEMETRY_FETCH_TIMEOUT`] -- "how hot are the chips?" is the most
/// natural thing to ask a phone, and answering "unavailable" whenever no Mac
/// happens to be watching would make the tool useless.
async fn box_status(state: &AppState) -> (Value, String) {
    let serving = state
        .endpoint()
        .map(|ep| json!({ "model": ep.model, "base_url": ep.base_url }));
    let mut result = json!({
        "box": state.name(),
        "chips": state.chips(),
        "device_mesh": state.device_mesh(),
        "serving": serving,
    });
    if let Some(layout) = split_layout(state) {
        result["split"] = layout;
    }
    let snapshot = match state.peek_snapshot(TELEMETRY_MAX_AGE) {
        Some(json) => Some(json),
        None => tokio::time::timeout(TELEMETRY_FETCH_TIMEOUT, state.cached_snapshot())
            .await
            .ok()
            .and_then(Result::ok),
    };
    match snapshot.as_deref().and_then(device_readings) {
        Some(devices) => result["devices"] = Value::Array(devices),
        None => {
            result["telemetry_note"] = json!(
                "Chip telemetry (tt-smi) couldn't be read just now, so temperatures and power \
                 are unavailable."
            )
        }
    }
    (result, "Checked the box status".to_string())
}

/// When the box is split (co-serving), which model runs on which chips.
fn split_layout(state: &AppState) -> Option<Value> {
    if !crate::coserve::is_split(state) {
        return None;
    }
    let companion = state.companion()?;
    let image = companion.lifecycle();
    Some(json!({
        "chat": { "model": state.current_model(), "chips": crate::coserve::CHAT_CHIPS },
        "image": {
            "model": image.model.or(image.served_id),
            "phase": image.phase,
            "chips": crate::coserve::MEDIA_CHIPS,
        },
        "note": "The box is split: one P300 card (two chips) per model, so chat and images run at the same time.",
    }))
}

/// The system-prompt line describing a split box, if it is split.
fn split_note(state: &AppState) -> Option<String> {
    let layout = split_layout(state)?;
    Some(format!(
        "Right now the box is split between two models: you run on chips {} (one P300 card), \
         and the image model {} runs on chips {} at the same time, for the app's Create tab. \
         Images are made there, not by you.",
        crate::coserve::CHAT_CHIPS,
        layout["image"]["model"].as_str().unwrap_or("(starting)"),
        crate::coserve::MEDIA_CHIPS,
    ))
}

/// `list_models`: `GET /models`'s backend call, narrowed to this box.
async fn list_models(state: &AppState) -> Result<(Value, String), String> {
    let backend = state.backend();
    let resp = tokio::task::spawn_blocking(move || backend.list_models())
        .await
        .map_err(|e| format!("list_models task panicked: {e}"))?
        .map_err(|e| e.to_string())?;
    let (names, downloaded, total) = models_for_box(&resp, state.device_mesh());
    let summary = format!("Listed {} models this box can serve", names.len());
    Ok((
        json!({
            "models": names,
            "downloaded": downloaded,
            "total": total,
            "truncated": total > names.len(),
        }),
        summary,
    ))
}

/// `search_documents`: the top passages for the query, trimmed so five of them
/// fit comfortably in an 8K context.
async fn search_documents(state: &AppState, args: &Value) -> Result<(Value, String), String> {
    let docs = state
        .docs()
        .ok_or_else(|| "this box has no document store".to_string())?;
    if docs.is_empty() {
        return Err(
            "the user hasn't uploaded any documents or added any Library folders yet".to_string(),
        );
    }
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| "search_documents needs a query".to_string())?;
    let document = args
        .get("document")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty());
    let hits = crate::library::search(state, query, 5, document).await;
    let summary = match hits.len() {
        0 => format!("No passages matched \"{query}\""),
        n => {
            let mut names: Vec<&str> = hits.iter().map(|h| h.document.as_str()).collect();
            names.dedup();
            format!("Found {n} passages in {}", names.join(", "))
        }
    };
    let passages: Vec<Value> = hits
        .iter()
        .map(|h| json!({ "document": h.document, "path": h.path, "text": h.text.chars().take(1000).collect::<String>() }))
        .collect();
    Ok((json!({ "query": query, "passages": passages }), summary))
}

/// `remember`: save a fact about the user.
fn remember(state: &AppState, args: &Value) -> Result<(Value, String), String> {
    let fact = args
        .get("fact")
        .and_then(Value::as_str)
        .ok_or_else(|| "remember needs a fact".to_string())?;
    let person = crate::people::current().map(|p| p.id);
    let memory = state.memories().add(fact, unix_secs(), person.as_deref())?;
    Ok((
        json!({ "remembered": memory.text }),
        format!("Remembered: {}", memory.text),
    ))
}

/// `forget`: drop saved facts containing some words.
fn forget(state: &AppState, args: &Value) -> Result<(Value, String), String> {
    let about = args
        .get("about")
        .and_then(Value::as_str)
        .ok_or_else(|| "forget needs words to look for".to_string())?;
    let person = crate::people::current().map(|p| p.id);
    let gone = state.memories().forget(about, person.as_deref());
    let texts: Vec<&str> = gone.iter().map(|m| m.text.as_str()).collect();
    let summary = match gone.len() {
        0 => format!("Nothing remembered about \"{about}\""),
        1 => format!("Forgot: {}", texts[0]),
        n => format!("Forgot {n} things"),
    };
    Ok((json!({ "forgotten": texts }), summary))
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Web access can be turned off for a box (`TT_STATION_NO_WEB=1`).
fn web_allowed() -> Result<(), String> {
    match std::env::var("TT_STATION_NO_WEB").as_deref() {
        Ok("1") | Ok("true") => Err("web access is turned off on this box".to_string()),
        _ => Ok(()),
    }
}

/// `web_search`: the top results for a query.
async fn web_search(args: &Value) -> Result<(Value, String), String> {
    web_allowed()?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .ok_or_else(|| "web_search needs a query".to_string())?;
    let hits = crate::web::search(query, 6).await?;
    let summary = format!("Searched the web for \"{query}\"");
    if hits.is_empty() {
        return Ok((
            json!({ "query": query, "results": [], "note": "No results." }),
            summary,
        ));
    }
    Ok((json!({ "query": query, "results": hits }), summary))
}

/// `read_webpage`: a public page's readable text.
async fn read_webpage(args: &Value) -> Result<(Value, String), String> {
    web_allowed()?;
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| "read_webpage needs a url".to_string())?;
    let (final_url, title, text) = crate::web::read(url).await?;
    let host = reqwest::Url::parse(&final_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let summary = format!("Read {}", title.clone().unwrap_or(host));
    Ok((
        json!({ "url": final_url, "title": title, "text": text }),
        summary,
    ))
}

/// `create_image`: render with the image model serving beside the chat model
/// (or alone), keep the pictures on the box, and hand the model only their
/// ids -- the client shows them.
async fn create_image(state: &AppState, args: &Value) -> Result<(Value, String), String> {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| "create_image needs a prompt".to_string())?
        .to_string();
    let Some((base, model)) = crate::gateway::image_target(state).await else {
        return Err(
            "no image model is serving next to you. Offer to switch the box: call switch_mode \
             with mode chat_images (chat and drawing at once) or images (pictures only)."
                .to_string(),
        );
    };
    let count = args.get("count").and_then(Value::as_u64).unwrap_or(1);
    let request =
        crate::gateway::media_image_request(&json!({ "prompt": prompt, "n": count }), &model);
    let api_key =
        std::env::var("TT_MEDIA_API_KEY").unwrap_or_else(|_| "your-secret-key".to_string());
    let started = Instant::now();
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/images/generations"))
        .bearer_auth(api_key)
        .timeout(Duration::from_secs(600))
        .json(&request)
        .send()
        .await
        .map_err(|e| format!("couldn't reach the image model: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!(
            "the image model answered {status}: {}",
            text.chars().take(200).collect::<String>()
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("the image model's answer wasn't JSON: {e}"))?;
    let creations = state.creations().clone();
    let images: Vec<String> = body["images"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let saved_prompt = prompt.clone();
    let ids = tokio::task::spawn_blocking(move || {
        images
            .iter()
            .filter_map(|b64| creations.save_b64(b64, &saved_prompt).ok())
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    if ids.is_empty() {
        return Err("the image model returned no image".to_string());
    }
    let secs = started.elapsed().as_secs_f32();
    let what = if ids.len() == 1 {
        "an image".to_string()
    } else {
        format!("{} images", ids.len())
    };
    Ok((
        json!({
            "image_ids": ids,
            "prompt": prompt,
            "seconds": (secs * 10.0).round() / 10.0,
            "note": "The image is already shown to the user in the chat. Say one short sentence about it; don't describe it in detail or include links.",
        }),
        format!("Drew {what} in {secs:.1}s"),
    ))
}

/// `recent_logs`: `GET /logs?source=container`'s file tail, capped at
/// [`MAX_LOG_LINES`].
async fn recent_logs(state: &AppState, args: &Value) -> Result<(Value, String), String> {
    let lines = args
        .get("lines")
        .and_then(Value::as_u64)
        .map(|n| (n as usize).clamp(1, MAX_LOG_LINES))
        .unwrap_or(DEFAULT_LOG_LINES);
    let repo = state
        .tt_inference_repo()
        .ok_or("logs are unavailable on this box (no tt-inference-server checkout configured)")?
        .to_path_buf();
    let (origin, tail) = tokio::task::spawn_blocking(move || {
        crate::routes::read_log_tail(&repo, crate::logs::LogSource::Container, lines)
    })
    .await
    .map_err(|e| format!("log read task panicked: {e}"))?
    .map_err(|e| format!("reading logs failed: {e}"))?;
    let tail = compact_log_lines(&tail);
    let summary = format!("Read the last {} lines of the serving log", tail.len());
    Ok((json!({ "origin": origin, "lines": tail }), summary))
}

/// Validate a disruptive call and park it as a pending action. `Ok` carries
/// the tool result for the model and the client report; `Err` is a reason
/// fed back to the model (bad/unknown model, already serving it) with no
/// pending action created.
pub(crate) async fn propose(
    state: &AppState,
    call: &ToolCall,
    current_model: &str,
) -> Result<(Value, ActionReport), String> {
    let raw_args = call
        .args
        .clone()
        .map_err(|raw| format!("arguments were not a JSON object: {raw}"))?;

    let args = match call.name.as_str() {
        "run_model" => {
            let requested = raw_args
                .get("model")
                .and_then(Value::as_str)
                .filter(|m| !m.trim().is_empty())
                .ok_or("run_model needs a \"model\" argument")?;
            let backend = state.backend();
            // A catalog that can't be read is treated as "can't check", not
            // as "no such model" -- the real `/run` will still refuse a bad id.
            let known: Vec<String> = tokio::task::spawn_blocking(move || backend.list_models())
                .await
                .ok()
                .and_then(Result::ok)
                .map(|r| r.models.into_iter().map(|m| m.name).collect())
                .unwrap_or_default();
            let model = resolve_model(requested, &known).map_err(|candidates| {
                if candidates.is_empty() {
                    format!("no model named '{requested}' is available; call list_models to see what is")
                } else {
                    format!(
                        "'{requested}' is ambiguous or unknown. If the user clearly meant one \
                         of these, call run_model again with that exact name; otherwise ask \
                         which they meant: {}",
                        candidates.join(", ")
                    )
                }
            })?;
            if model == current_model {
                return Err(format!("{model} is already being served; nothing to do"));
            }
            // Don't offer a confirm card for something that would only fail (or
            // silently download tens of GB inside the launch).
            let probe = state.clone();
            let wanted = model.clone();
            if let Some(reason) =
                tokio::task::spawn_blocking(move || probe.not_servable_reason_blocking(&wanted))
                    .await
                    .ok()
                    .flatten()
            {
                return Err(reason);
            }
            json!({ "model": model })
        }
        "switch_mode" => {
            let wanted = raw_args
                .get("mode")
                .and_then(Value::as_str)
                .ok_or("switch_mode needs a \"mode\" argument")?;
            let modes = crate::modes::modes(state).await;
            let mode = modes.iter().find(|m| m.id == wanted).ok_or_else(|| {
                format!(
                    "this box has no '{wanted}' mode; it has: {}",
                    modes.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
                )
            })?;
            if mode.active {
                return Err(format!(
                    "the box is already in {} mode; nothing to do",
                    mode.title
                ));
            }
            if !mode.ready {
                return Err(format!(
                    "{} mode needs {} downloaded first; tell the user to download it in the Models tab",
                    mode.title,
                    mode.needs_download.join(" and ")
                ));
            }
            json!({
                "mode": mode.id,
                "title": mode.title,
                "models": mode.models.iter().map(|m| crate::notify::display_model(m)).collect::<Vec<_>>(),
                "eta_s": mode.eta_s,
            })
        }
        "install_app" => {
            let id = raw_args
                .get("app")
                .and_then(Value::as_str)
                .ok_or("install_app needs an \"app\" argument")?;
            let def = crate::apps::app(id).ok_or_else(|| {
                format!(
                    "there's no app \"{id}\" in the store; the apps are: {}",
                    crate::apps::APPS
                        .iter()
                        .map(|a| a.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            json!({ "app": def.id, "name": def.name, "blurb": def.blurb })
        }
        _ => json!({}),
    };

    let summary = pending_summary(&call.name, &args, current_model);
    let id = state.insert_pending_action(PendingAction {
        tool: call.name.clone(),
        args: args.clone(),
        summary: summary.clone(),
        created: Instant::now(),
    });
    let result = json!({
        "status": "needs_confirmation",
        "summary": summary,
        "note": "Not done yet. Ask the user to confirm.",
    });
    let report = ActionReport {
        id,
        tool: call.name.clone(),
        args,
        status: ActionStatus::NeedsConfirmation,
        summary,
        images: Vec::new(),
    };
    Ok((result, report))
}

// ---------------------------------------------------------------------
// Confirmation
// ---------------------------------------------------------------------

/// `POST /assistant/confirm`'s body: consume the pending action and, if
/// approved, run it through the same code `/run`, `/stop` and `/power` use.
pub(crate) async fn confirm(
    state: &AppState,
    req: ConfirmRequest,
) -> Result<ConfirmResponse, AssistantError> {
    let action = state
        .take_pending_action(&req.action_id)
        .ok_or(AssistantError::UnknownAction)?;
    let respond = |status: ConfirmStatus, summary: String| ConfirmResponse {
        action_id: req.action_id.clone(),
        tool: action.tool.clone(),
        status,
        summary,
        run: None,
    };

    if !req.approve {
        return Ok(respond(
            ConfirmStatus::Cancelled,
            format!("Cancelled: {}", action.summary),
        ));
    }

    match action.tool.as_str() {
        "run_model" => {
            let model = action
                .args
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AssistantError::Backend(anyhow::anyhow!("pending run_model has no model"))
                })?
                .to_string();
            // `/run` blocks until the model is ready -- minutes -- and this
            // very model may be what's answering the phone. So: background
            // it, return now, and log the outcome the way the agent logs any
            // other failure. `start_serving` updates status/endpoint/mDNS
            // exactly as `/run` would when it finishes.
            let background = state.clone();
            let bg_model = model.clone();
            tokio::spawn(async move {
                eprintln!("tt-station-agentd: assistant: starting {bg_model} (confirmed)");
                match background.start_serving(&bg_model, false).await {
                    Ok(ep) => eprintln!(
                        "tt-station-agentd: assistant: {} is serving at {}",
                        ep.model, ep.base_url
                    ),
                    Err(e) => eprintln!(
                        "tt-station-agentd: assistant: confirmed run of {bg_model} failed: {e:#}"
                    ),
                }
            });
            Ok(respond(
                ConfirmStatus::Started,
                format!(
                    "Starting {model} — this takes a few minutes; I'll be offline until it's ready."
                ),
            ))
        }
        "install_app" => {
            let id = action
                .args
                .get("app")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = action
                .args
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("the app")
                .to_string();
            let def = crate::apps::app(&id)
                .ok_or_else(|| AssistantError::Backend(anyhow::anyhow!("no app {id}")))?;
            let st = state.clone();
            let done = tokio::task::spawn_blocking(move || crate::apps::start_or_install(&st, def))
                .await
                .map_err(|e| AssistantError::Backend(anyhow::anyhow!(e.to_string())))?
                .map_err(|e| AssistantError::Backend(anyhow::anyhow!(e)))?;
            Ok(respond(
                ConfirmStatus::Started,
                if done == "running" {
                    format!("{name} is running. It's in TT Station's Connect page.")
                } else {
                    format!("Setting up {name}. It takes a minute or two; it's in TT Station's Connect page.")
                },
            ))
        }
        "switch_mode" => {
            let mode = action
                .args
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let background = state.clone();
            let bg_mode = mode.clone();
            tokio::spawn(async move {
                if let Err(e) = crate::modes::start(&background, &bg_mode).await {
                    eprintln!("tt-station-agentd: assistant: switching to {bg_mode} failed: {e}");
                }
            });
            Ok(respond(
                ConfirmStatus::Started,
                format!("Switching to {mode} — I'll be offline until it's ready. You'll get a notification."),
            ))
        }
        "stop_model" => {
            let was = state.current_model();
            state
                .stop_serving()
                .await
                .map_err(AssistantError::Backend)?;
            Ok(respond(
                ConfirmStatus::Done,
                match was {
                    Some(m) => format!("Stopped serving {m}."),
                    None => "Stopped serving.".to_string(),
                },
            ))
        }
        "reset_chips" => {
            // Stop first so the board reset never pulls the silicon out from
            // under a live vLLM container (the same stop-then-`tt-smi -r`
            // order `RunPyBackend` uses before every serve), then run the
            // `POST /power reset-chips` body -- NOT `POST /reset`, which also
            // clears every pairing token and would unpair the very phone
            // that asked.
            state
                .stop_serving()
                .await
                .map_err(AssistantError::Backend)?;
            let s = state.clone();
            tokio::task::spawn_blocking(move || {
                s.run_power_command(crate::power::PowerAction::ResetChips, false)
            })
            .await
            .map_err(|e| AssistantError::Backend(anyhow::anyhow!("reset task panicked: {e}")))?
            .map_err(AssistantError::Backend)?;
            Ok(respond(
                ConfirmStatus::Done,
                "Reset the chips. Nothing is serving now.".to_string(),
            ))
        }
        // An acting tool that waited for the person (needs_confirmation).
        name if READ_ONLY_TOOLS.contains(&name) => {
            let mut args = action.args.clone();
            let on_client = args
                .as_object_mut()
                .and_then(|a| a.remove(ON_CLIENT))
                .is_some_and(|v| v == true);
            if on_client {
                return Ok(ConfirmResponse {
                    run: Some(json!({ "tool": name, "args": args })),
                    ..respond(ConfirmStatus::OnClient, action.summary.clone())
                });
            }
            if name == "bringup_decide"
                && crate::bringup::waiting_on().as_deref() != args["what"].as_str()
            {
                return Err(AssistantError::Backend(anyhow::anyhow!(
                    "the bring-up is no longer waiting on that"
                )));
            }
            let call = ToolCall {
                id: req.action_id.clone(),
                name: name.to_string(),
                args: Ok(args),
            };
            let (_, report) = run_read_only(state, &call).await;
            if report.status == ActionStatus::Error {
                return Err(AssistantError::Backend(anyhow::anyhow!(report.summary)));
            }
            Ok(respond(ConfirmStatus::Done, report.summary))
        }
        other => Err(AssistantError::Backend(anyhow::anyhow!(
            "pending action has unknown tool {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_waits_for_the_person() {
        let none = json!({});
        for t in ["bringup_decide", "start_recording"] {
            assert!(needs_confirmation(t, &none, false), "{t}");
        }
        assert!(needs_confirmation(
            "open_app",
            &json!({ "url": "https://x.example" }),
            false
        ));
        assert!(!needs_confirmation(
            "open_app",
            &json!({ "app": "VS Code" }),
            false
        ));
        assert!(!needs_confirmation(
            "remember",
            &json!({ "fact": "x" }),
            false
        ));
        // Web text in the turn: every acting tool, no looking tool.
        for t in ACTING_TOOLS {
            assert!(needs_confirmation(t, &none, true), "{t}");
            assert_eq!(tool_kind(t), Some(ToolKind::ReadOnly), "{t}");
        }
        for t in [
            "box_status",
            "web_search",
            "read_webpage",
            "search_documents",
            "find_files",
            "bringup_status",
            "research_check",
        ] {
            assert!(!needs_confirmation(t, &none, true), "{t}");
        }
    }

    #[test]
    fn after_web_text_only_known_addresses_are_fetched_unasked() {
        let mut turn = crate::urltrust::Turn::from_messages(&[
            json!({ "role": "user", "content": "read https://news.example/a" }),
        ]);
        let read = |u: &str| json!({ "url": u });
        // Before any web text, nothing about an address waits.
        assert!(!fetches_unknown_address(
            "read_webpage",
            &read("https://other.example/?q=x"),
            &turn
        ));
        turn.web_result(
            &json!({ "results": [{ "url": "https://found.example/page" }] }).to_string(),
        );
        assert!(!fetches_unknown_address(
            "read_webpage",
            &read("https://news.example/a"),
            &turn
        ));
        assert!(!fetches_unknown_address(
            "read_webpage",
            &read("https://found.example/page"),
            &turn
        ));
        assert!(fetches_unknown_address(
            "read_webpage",
            &read("https://found.example/page?leak=hello"),
            &turn
        ));
        assert!(fetches_unknown_address(
            "open_app",
            &read("https://other.example/"),
            &turn
        ));
        assert!(!fetches_unknown_address("box_status", &json!({}), &turn));
        assert!(
            pending_summary("read_webpage", &read("https://x.example/?q=1"), "")
                .contains("https://x.example/?q=1")
        );
    }

    #[tokio::test]
    async fn a_computer_step_runs_on_the_persons_computer_and_comes_back() {
        let call = ToolCall {
            id: "c1".into(),
            name: "find_files".into(),
            args: Ok(json!({ "name": "resume" })),
        };
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<ActionReport>();
        let task = tokio::spawn(async move {
            let mut on_action = move |r: &ActionReport| {
                let _ = seen_tx.send(r.clone());
            };
            run_on_client(&call, &mut on_action).await
        });
        // The step goes out to the person's computer…
        let out = seen_rx.recv().await.unwrap();
        assert_eq!(out.status, ActionStatus::OnClient);
        assert_eq!(out.args["name"], "resume");
        // …which posts what it found.
        let status = post_client_result(crate::routes::BearerAuth, axum::Json(ClientResult {
            id: out.id.clone(),
            outcome: json!({ "ok": true, "summary": "Found 1 matching file(s)", "result": { "matches": ["/Users/j/resume.pdf"] } }),
        })).await;
        assert_eq!(status, axum::http::StatusCode::NO_CONTENT);
        let (result, report) = task.await.unwrap();
        assert_eq!(report.id, out.id, "the same step, now done");
        assert_eq!(report.status, ActionStatus::Done);
        assert_eq!(
            result["matches"][0], "/Users/j/resume.pdf",
            "the model sees what the Mac found"
        );
        // An answer for a step nobody's waiting on goes nowhere.
        let late = post_client_result(
            crate::routes::BearerAuth,
            axum::Json(ClientResult {
                id: out.id,
                outcome: json!({ "ok": true }),
            }),
        )
        .await;
        assert_eq!(late, axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn tori_on_another_computer_knows_which_computer_is_hers() {
        let req: AssistantRequest = serde_json::from_value(json!({ "messages": [{ "role": "user", "content": "hi" }], "persona": "tori", "client": "darwin" })).unwrap();
        let opts = req.options(false);
        assert!(opts.tori && opts.client);
        let on_box: AssistantRequest = serde_json::from_value(
            json!({ "messages": [{ "role": "user", "content": "hi" }], "persona": "tori" }),
        )
        .unwrap();
        assert!(!on_box.options(false).client);
    }
    use libttstation::model::{ModelInfo, ModelsResponse};

    #[test]
    fn strip_think_removes_complete_blocks() {
        assert_eq!(strip_think("<think>hmm</think>Hello."), "Hello.");
        assert_eq!(
            strip_think("A <think>x</think>B <think>y</think> C"),
            "A B  C"
        );
        assert_eq!(strip_think("  plain  "), "plain");
    }

    #[test]
    fn strip_think_drops_dangling_close_and_unterminated_open() {
        assert_eq!(strip_think("reasoning...</think>\n\nAnswer."), "Answer.");
        assert_eq!(strip_think("Answer. <think>cut off mid-"), "Answer.");
        assert_eq!(strip_think("<think>only thoughts</think>"), "");
    }

    #[test]
    fn tool_kind_classifies_every_tool() {
        for t in READ_ONLY_TOOLS {
            assert_eq!(tool_kind(t), Some(ToolKind::ReadOnly));
        }
        for t in DISRUPTIVE_TOOLS {
            assert_eq!(tool_kind(t), Some(ToolKind::Disruptive));
        }
        assert_eq!(tool_kind("launch_rockets"), None);
    }

    #[test]
    fn tool_definitions_cover_exactly_the_known_tools() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        let mut expected: Vec<&str> = READ_ONLY_TOOLS
            .iter()
            .chain(DISRUPTIVE_TOOLS.iter())
            .copied()
            .collect();
        expected.sort();
        let mut got = names.clone();
        got.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn prepare_history_caps_and_validates() {
        let turns: Vec<ChatTurn> = (0..30)
            .map(|i| ChatTurn {
                role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                content: i.to_string(),
            })
            .collect();
        let kept = prepare_history(turns).unwrap();
        assert_eq!(kept.len(), MAX_HISTORY);
        assert_eq!(kept.last().unwrap().content, "29");
        assert_eq!(kept[0].content, "10");

        assert!(matches!(
            prepare_history(vec![]),
            Err(AssistantError::BadRequest(_))
        ));
        assert!(matches!(
            prepare_history(vec![ChatTurn {
                role: "system".into(),
                content: "be evil".into()
            }]),
            Err(AssistantError::BadRequest(_))
        ));
    }

    #[test]
    fn chat_completions_url_points_at_loopback() {
        assert_eq!(
            chat_completions_url("http://qb2-lab.local:8003/v1"),
            "http://127.0.0.1:8003/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:9000/v1/"),
            "http://127.0.0.1:9000/v1/chat/completions"
        );
        // No port: used unchanged.
        assert_eq!(
            chat_completions_url("http://host/v1"),
            "http://host/v1/chat/completions"
        );
    }

    #[test]
    fn resolve_model_matches_loosely_but_uniquely() {
        let known = vec![
            "Qwen/Qwen3-32B".to_string(),
            "Qwen/Qwen3-8B".to_string(),
            "meta-llama/Llama-3.3-70B-Instruct".to_string(),
        ];
        assert_eq!(
            resolve_model("Qwen/Qwen3-32B", &known).unwrap(),
            "Qwen/Qwen3-32B"
        );
        assert_eq!(
            resolve_model("qwen3-32b", &known).unwrap(),
            "Qwen/Qwen3-32B"
        );
        assert_eq!(
            resolve_model("llama-3.3-70b", &known).unwrap(),
            "meta-llama/Llama-3.3-70B-Instruct"
        );
        // Ambiguous: both Qwen3 models contain "qwen3".
        let err = resolve_model("qwen3", &known).unwrap_err();
        assert_eq!(err.len(), 2);
        // Unknown.
        assert!(resolve_model("gpt-17", &known).unwrap_err().is_empty());
        // No catalog: pass through.
        assert_eq!(resolve_model("anything", &[]).unwrap(), "anything");
    }

    #[test]
    fn models_for_box_filters_by_mesh_and_puts_downloaded_first() {
        let m = |name: &str, devices: &[&str], downloaded: bool| ModelInfo {
            name: name.to_string(),
            devices: devices.iter().map(|d| d.to_string()).collect(),
            downloaded,
        };
        let resp = ModelsResponse {
            release_version: None,
            models: vec![
                m("B", &["P300X2"], false),
                m("A", &["P300X2"], false),
                m("C", &["P300X2", "T3K"], true),
                m("Galaxy-only", &["GALAXY"], true),
            ],
        };
        let (names, downloaded, total) = models_for_box(&resp, Some("p300x2"));
        assert_eq!(names, vec!["C", "A", "B"]);
        assert_eq!(downloaded, vec!["C"]);
        assert_eq!(total, 3);

        // Unknown mesh: everything.
        let (names, _, total) = models_for_box(&resp, None);
        assert_eq!(total, 4);
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn device_readings_trims_tt_smi_frames() {
        let frame = r#"{"device_info":[{"board_info":{"board_type":"p300c"},"telemetry":{"asic_temperature":"61.4","power":"85.2","aiclk":"1000","voltage":"0.8"}}]}"#;
        let devices = device_readings(frame).unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["board_type"], "p300c");
        assert_eq!(devices[0]["temp_c"], "61.4");
        assert_eq!(devices[0]["power_w"], "85.2");
        assert!(device_readings(r#"{"device_info":[]}"#).is_none());
        assert!(device_readings("not json").is_none());
    }

    #[test]
    fn parse_tool_calls_handles_string_object_and_bad_arguments() {
        let msg = json!({
            "tool_calls": [
                { "id": "a", "type": "function", "function": { "name": "run_model", "arguments": "{\"model\":\"X\"}" } },
                { "type": "function", "function": { "name": "box_status", "arguments": "" } },
                { "id": "c", "function": { "name": "recent_logs", "arguments": { "lines": 5 } } },
                { "id": "d", "function": { "name": "run_model", "arguments": "not json" } },
                { "id": "e", "function": {} }
            ]
        });
        let calls = parse_tool_calls(&msg);
        assert_eq!(calls.len(), 4, "the call with no name is skipped");
        assert_eq!(calls[0].args, Ok(json!({ "model": "X" })));
        assert_eq!(calls[1].id, "call_1");
        assert_eq!(calls[1].args, Ok(json!({})));
        assert_eq!(calls[2].args, Ok(json!({ "lines": 5 })));
        assert!(calls[3].args.is_err());
        assert!(parse_tool_calls(&json!({ "content": "hi" })).is_empty());
    }

    #[test]
    fn pending_texts_say_the_assistant_goes_offline() {
        let args = json!({ "model": "Qwen/Qwen3-32B" });
        let s = pending_summary("run_model", &args, "meta-llama/Llama-3.3-70B-Instruct");
        assert!(s.contains("Qwen/Qwen3-32B") && s.contains("offline"));
        assert!(pending_summary("stop_model", &json!({}), "M").contains("offline"));
        assert!(pending_summary("reset_chips", &json!({}), "M").contains("offline"));

        let reply = pending_reply(&[("run_model".to_string(), args)]);
        assert_eq!(
            reply,
            "I can start Qwen/Qwen3-32B — this replaces the current model and I'll be offline while it loads. Confirm?"
        );
    }

    #[test]
    fn system_prompt_carries_box_context_and_rules() {
        let p = system_prompt(&BoxContext {
            name: "qb2-lab".into(),
            chips: "4xBH".into(),
            device_mesh: Some("p300x2".into()),
            model: "Qwen/Qwen3-32B".into(),
            documents: vec![],
            layout: None,
            memories: String::new(),
        });
        for needle in [
            "qb2-lab",
            "4xBH",
            "p300x2",
            "Qwen/Qwen3-32B",
            "NEVER",
            "no markdown",
            "Do NOT ask",
        ] {
            assert!(p.contains(needle), "system prompt missing {needle:?}");
        }
    }

    fn feed(chunks: &[&str]) -> (String, String) {
        let mut f = ThinkFilter::default();
        let mut pieces = Vec::new();
        for c in chunks {
            pieces.extend(f.push(c));
        }
        pieces.extend(f.finish());
        let (mut text, mut reasoning) = (String::new(), String::new());
        for p in pieces {
            match p {
                Piece::Text(t) => text.push_str(&t),
                Piece::Reasoning(r) => reasoning.push_str(&r),
            }
        }
        (text, reasoning)
    }

    #[test]
    fn think_filter_routes_tags_split_across_chunks() {
        assert_eq!(
            feed(&["Hi <th", "ink>plan", " this</th", "ink> there"]),
            ("Hi  there".to_string(), "plan this".to_string())
        );
        assert_eq!(
            feed(&["no tags at all"]),
            ("no tags at all".to_string(), String::new())
        );
        // A lone `<` that never becomes a tag is released at the end.
        assert_eq!(feed(&["a < b"]).0, "a < b");
        // Multi-byte text across chunk edges stays intact.
        assert_eq!(feed(&["caf", "é ", "naïve"]).0, "café naïve");
        // Control tokens vanish.
        assert_eq!(feed(&["Done.<|im_", "end|>"]).0, "Done.");
    }

    #[test]
    fn clean_reply_keeps_markdown_structure_and_code_spacing() {
        let md = "## Title\n\n- one\n  - nested   item\n\n\n\n```rust\nlet  x =  1;\n```\nDone.<|im_end|>";
        assert_eq!(
            clean_reply(md),
            "## Title\n\n- one\n  - nested item\n\n```rust\nlet  x =  1;\n```\nDone."
        );
    }

    #[test]
    fn detailed_style_swaps_brevity_for_markdown() {
        let ctx = BoxContext {
            name: "b".into(),
            chips: "4xBH".into(),
            device_mesh: None,
            model: "m".into(),
            documents: vec![],
            layout: None,
            memories: String::new(),
        };
        let concise = system_prompt_styled(&ctx, false);
        let detailed = system_prompt_styled(&ctx, true);
        assert!(concise.contains("Be brief") && !concise.contains("Markdown"));
        assert!(detailed.contains("Markdown") && !detailed.contains("Be brief"));
        assert!(
            detailed.contains("NEVER say an action happened"),
            "tool rules kept"
        );
        assert!(
            !detailed.contains("Tensix"),
            "no hardware claims for an unknown mesh"
        );
        let bh = BoxContext {
            device_mesh: Some("p300x2".into()),
            ..ctx
        };
        let facts = system_prompt_styled(&bh, true);
        assert!(facts.contains("120 Tensix cores") && facts.contains("four Blackhole chips"));
    }

    #[test]
    fn clean_reply_drops_control_tokens_and_collapses_whitespace() {
        assert_eq!(clean_reply("<|im_start|>\n\n<|im_start|>"), "");
        assert_eq!(clean_reply("Paris.<|im_end|>"), "Paris.");
        assert_eq!(
            clean_reply("<think>x</think> It is   <|eot_id|> fine."),
            "It is fine."
        );
        // A lone "<|" with no closer is ordinary text.
        assert_eq!(clean_reply("a <| b"), "a <| b");
    }

    #[test]
    fn is_degenerate_catches_glued_loops_but_not_tables_or_code() {
        // Seen live from Llama-3.1-8B with many tools offered.
        let glued = format!(
            "}}\n{{\n  \"create_image\" \"create_image\" {}crea",
            "\"create_image\"create_image".repeat(40)
        );
        assert!(is_degenerate(&glued));
        let table = "| Model | Chips | tok/s |\n|---|---|---|\n| Llama-3.1-8B | 2 | 36 |\n\
                     | Qwen3-32B | 4 | 21 |\n| FLUX.1-schnell | 2 | n/a |\n\n\
                     The 8B model is faster per user; the 32B model writes better answers.";
        assert!(!is_degenerate(table));
        let code = "```python\nfor i in range(10):\n    print(i)\nfor j in range(10):\n    print(j)\n```\n\
                    Both loops print the numbers zero through nine, one per line, so you see twenty lines.";
        assert!(!is_degenerate(code));
    }

    #[test]
    fn is_degenerate_flags_empty_and_repetitive_output_only() {
        assert!(is_degenerate(""));
        assert!(is_degenerate(&"! ".repeat(40)));
        assert!(!is_degenerate("The capital of France is Paris."));
        assert!(!is_degenerate("ok ok ok")); // too short to judge
        assert!(!is_degenerate(
            "Chip temperatures are 49.1, 53.5, 50.8 and 54.4 degrees, all well within range today."
        ));
    }

    #[test]
    fn compact_log_lines_strips_prefixes_and_caps_size() {
        let lines = vec![
            "(EngineCore_DP0 pid=122) 2026-10-06 | INFO | Prefilling User 1".to_string(),
            "(not a pid prefix) kept as is".to_string(),
            "x".repeat(500),
        ];
        let out = compact_log_lines(&lines);
        assert_eq!(out[0], "2026-10-06 | INFO | Prefilling User 1");
        assert_eq!(out[1], "(not a pid prefix) kept as is");
        assert_eq!(out[2].chars().count(), MAX_LOG_LINE_CHARS + 1); // + ellipsis

        let many: Vec<String> = (0..200)
            .map(|i| format!("{i:03} {}", "y".repeat(150)))
            .collect();
        let out = compact_log_lines(&many);
        let total: usize = out.iter().map(|l| l.len() + 1).sum();
        assert!(total <= MAX_LOG_TOTAL_CHARS);
        assert!(
            out.last().unwrap().starts_with("199 "),
            "keeps the newest lines"
        );
    }

    #[test]
    fn resolve_model_suggests_closest_names_for_a_misheard_request() {
        let known: Vec<String> = [
            "Qwen/Qwen3-32B",
            "meta-llama/Llama-3.1-8B-Instruct",
            "meta-llama/Llama-3.3-70B-Instruct",
            "Qwen/Qwen2.5-7B-Instruct",
        ]
        .map(String::from)
        .to_vec();
        let suggestions = resolve_model("Lama 3 8B instruct", &known).unwrap_err();
        assert_eq!(suggestions[0], "meta-llama/Llama-3.1-8B-Instruct");
        assert!(resolve_model("banana-9000", &known).unwrap_err().is_empty());
    }
}
