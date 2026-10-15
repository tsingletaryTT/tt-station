// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Anthropic's Messages API on the box (`POST /v1/messages`), so tools built
//! for it -- Claude Code above all -- run on the model the box serves:
//! `ANTHROPIC_BASE_URL=http://<box>:8765 ANTHROPIC_AUTH_TOKEN=<app key> claude`.
//!
//! Each request is translated to OpenAI chat completions for the served model
//! (system prompt, text, images, tool definitions, tool calls and their
//! results), and the answer translated back, streamed or not: text blocks,
//! `tool_use` blocks with their JSON input, the stop reason and token usage.
//! Whatever model name a client asks for (`claude-sonnet-…`), the served
//! model answers, as with the rest of the gateway.
//!
//! - `POST /v1/messages`
//! - `POST /v1/messages/count_tokens`: an estimate (about 4 characters a token).

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::gateway::GatewayAuth;
use crate::routes::AppState;

fn error(status: StatusCode, kind: &str, msg: &str) -> Response {
    (
        status,
        Json(json!({ "type": "error", "error": { "type": kind, "message": msg } })),
    )
        .into_response()
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// An Anthropic request as an OpenAI chat-completions request for `model`.
pub fn to_openai(req: &Value, model: &str) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let system = text_of(&req["system"]);
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for m in req["messages"].as_array().into_iter().flatten() {
        let role = m["role"].as_str().unwrap_or("user");
        let content = &m["content"];
        let blocks: Vec<Value> = match content {
            Value::String(s) => vec![json!({ "type": "text", "text": s })],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if role == "assistant" {
            let text: Vec<String> = blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str().map(str::to_string))
                .collect();
            let calls: Vec<Value> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .map(|b| json!({
                    "id": b["id"], "type": "function",
                    "function": { "name": b["name"], "arguments": serde_json::to_string(&b["input"]).unwrap_or_else(|_| "{}".into()) },
                }))
                .collect();
            let mut msg = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text.join("\n")) } });
            if !calls.is_empty() {
                msg["tool_calls"] = json!(calls);
            }
            messages.push(msg);
            continue;
        }
        // A user turn: tool results become `tool` messages (first, as OpenAI wants
        // them right after the call), the rest one user message.
        let mut parts: Vec<Value> = Vec::new();
        for b in &blocks {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let mut out = text_of(&b["content"]);
                    if b["is_error"] == true {
                        out = format!("Error: {out}");
                    }
                    messages.push(
                        json!({ "role": "tool", "tool_call_id": b["tool_use_id"], "content": out }),
                    );
                }
                Some("text") => parts.push(json!({ "type": "text", "text": b["text"] })),
                Some("image") => {
                    let src = &b["source"];
                    let url = if src["type"] == "base64" {
                        format!(
                            "data:{};base64,{}",
                            src["media_type"].as_str().unwrap_or("image/png"),
                            src["data"].as_str().unwrap_or("")
                        )
                    } else {
                        src["url"].as_str().unwrap_or("").to_string()
                    };
                    parts.push(json!({ "type": "image_url", "image_url": { "url": url } }));
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            let only_text = parts.iter().all(|p| p["type"] == "text");
            let content = if only_text {
                json!(parts
                    .iter()
                    .filter_map(|p| p["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"))
            } else {
                json!(parts)
            };
            messages.push(json!({ "role": "user", "content": content }));
        }
    }
    let mut out = json!({
        "model": model,
        "messages": messages,
        "max_tokens": req["max_tokens"].as_u64().unwrap_or(4096),
        "stream": req["stream"] == true,
    });
    if out["stream"] == true {
        out["stream_options"] = json!({ "include_usage": true });
    }
    // Think only when the client asks to (Claude Code's thinking setting): an
    // agent's many short steps go much faster without it.
    let think = req["thinking"]["type"] == "enabled";
    out["chat_template_kwargs"] = json!({ "enable_thinking": think });
    if think {
        let budget = req["thinking"]["budget_tokens"].as_u64().unwrap_or(4096);
        out["max_tokens"] = json!(out["max_tokens"]
            .as_u64()
            .unwrap_or(4096)
            .max(budget + 4096));
    }
    for (from, to) in [
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop_sequences", "stop"),
    ] {
        if !req[from].is_null() {
            out[to] = req[from].clone();
        }
    }
    let tools: Vec<Value> = req["tools"]
        .as_array()
        .into_iter()
        .flatten()
        // Anthropic's server tools (web search…) have no schema to give a local model.
        .filter(|t| t["input_schema"].is_object())
        .map(|t| json!({ "type": "function", "function": { "name": t["name"], "description": t["description"], "parameters": t["input_schema"] } }))
        .collect();
    if !tools.is_empty() {
        out["tools"] = json!(tools);
        out["tool_choice"] = match req["tool_choice"]["type"].as_str() {
            Some("any") => json!("required"),
            Some("none") => json!("none"),
            Some("tool") => {
                json!({ "type": "function", "function": { "name": req["tool_choice"]["name"] } })
            }
            _ => json!("auto"),
        };
    }
    out
}

/// Room to answer within the model's context: prompt plus max_tokens past it is a
/// 400 from some servers (Claude Code asks for large budgets by default).
pub(crate) fn fit_output(req: &Value, body: &mut Value, ctx: u64) {
    let prompt = (req["system"].to_string().len()
        + req["messages"].to_string().len()
        + req["tools"].to_string().len()) as u64
        / 4;
    let room = ctx.saturating_sub(prompt + 256).max(1024);
    if body["max_tokens"].as_u64().is_some_and(|m| m > room) {
        body["max_tokens"] = json!(room);
    }
}

fn stop_reason(finish: Option<&str>, has_tools: bool) -> &'static str {
    match finish {
        // vLLM can say `tool_calls` with no call in the reply (gpt-oss declining
        // under tool_choice "required"); "tool_use" with nothing to run would
        // leave the client waiting on a call that isn't there.
        Some("tool_calls") if has_tools => "tool_use",
        Some("length") => "max_tokens",
        Some("stop") if has_tools => "tool_use",
        _ => "end_turn",
    }
}

fn message_id() -> String {
    format!("msg_{}", &crate::pairing::issue_pair_id()[..24])
}

/// Parse a tool call's arguments; a model that wrote broken JSON still gets its text through.
fn parse_args(args: &str, finished: bool) -> Value {
    let fixed = if finished {
        complete_json(args)
    } else {
        repair_json(args)
    };
    fixed.unwrap_or_else(|| {
        if args.trim().is_empty() {
            json!({})
        } else {
            json!({ "_unparsed_arguments": args })
        }
    })
}

/// An OpenAI chat completion as an Anthropic message.
pub fn from_openai(resp: &Value, model: &str) -> Value {
    from_openai_checked(resp, model, &ToolCheck::default())
}

/// [`from_openai`], with its tool calls checked against the request's tools.
pub fn from_openai_checked(resp: &Value, model: &str, check: &ToolCheck) -> Value {
    let choice = &resp["choices"][0];
    let msg = &choice["message"];
    let mut content: Vec<Value> = Vec::new();
    if let Some(r) = msg["reasoning_content"]
        .as_str()
        .or(msg["reasoning"].as_str())
        .filter(|r| !r.trim().is_empty())
    {
        content.push(json!({ "type": "thinking", "thinking": r, "signature": "" }));
    }
    let mut text = crate::assistant::strip_think(msg["content"].as_str().unwrap_or(""));
    let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
    let mut leaked = None;
    if calls.is_empty() && choice["finish_reason"] != "length" {
        if let Some((p, call)) =
            leak_start(&text).and_then(|p| leaked_call(&text[p..]).map(|c| (p, c)))
        {
            text = text[..p].trim_end().to_string();
            leaked = Some(call);
        }
    }
    if !text.trim().is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    for c in &calls {
        let input = parse_args(
            c["function"]["arguments"].as_str().unwrap_or("{}"),
            choice["finish_reason"] != "length",
        );
        let (name, input, _) =
            check.call(c["function"]["name"].as_str().unwrap_or(""), input, false);
        content.push(json!({
            "type": "tool_use",
            "id": c["id"].as_str().map(str::to_string).unwrap_or_else(|| format!("toolu_{}", &crate::pairing::issue_pair_id()[..20])),
            "name": name,
            "input": input,
        }));
    }
    let has_tools = !calls.is_empty() || leaked.is_some();
    if let Some((name, input)) = leaked {
        let (name, input, _) = check.call(&name, input, true);
        content.push(json!({ "type": "tool_use", "id": format!("toolu_{}", &crate::pairing::issue_pair_id()[..20]), "name": name, "input": input }));
    }
    json!({
        "id": message_id(),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason(choice["finish_reason"].as_str(), has_tools),
        "stop_sequence": null,
        "usage": {
            "input_tokens": resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            "output_tokens": resp["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        },
    })
}

/// Make a model's tool-call arguments into a JSON object if it can be done
/// safely: raw newlines and tabs inside strings escaped, trailing commas
/// dropped, Python's True/False/None. Anything else stays as it was (the
/// client then tells the model, which writes the call again).
pub fn repair_json(raw: &str) -> Option<Value> {
    repair(raw, false)
}

/// As `repair_json`, and also closes what's left open (a string, braces) for a
/// call the model finished: some tool parsers drop the last `"}` of a call that
/// was otherwise whole. Never for a reply cut off at the token limit (that's
/// half a file's content), nor a stream that dropped (that's an error instead).
pub fn complete_json(raw: &str) -> Option<Value> {
    repair(raw, true)
}

fn repair(raw: &str, close: bool) -> Option<Value> {
    let t = raw.trim();
    if let Ok(v) = serde_json::from_str::<Value>(t) {
        return v.is_object().then_some(v);
    }
    let mut out = String::with_capacity(t.len() + 16);
    let (mut in_str, mut esc) = (false, false);
    for c in t.chars() {
        if in_str {
            match c {
                _ if esc => {
                    out.push(c);
                    esc = false;
                }
                '\\' => {
                    out.push(c);
                    esc = true;
                }
                '"' => {
                    out.push(c);
                    in_str = false;
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                _ => out.push(c),
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    static TRAILING: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r",\s*([}\]])").expect("valid regex"));
    static PY: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(:\s*)(True|False|None)\b").expect("valid regex")
    });
    let fixed = TRAILING.replace_all(&out, "$1");
    let fixed = PY.replace_all(&fixed, |c: &regex::Captures| {
        format!(
            "{}{}",
            &c[1],
            match &c[2] {
                "True" => "true",
                "False" => "false",
                _ => "null",
            }
        )
    });
    let mut fixed = fixed.to_string();
    if close && serde_json::from_str::<Value>(&fixed).is_err() && fixed.starts_with('{') {
        let opens = fixed
            .matches('{')
            .count()
            .saturating_sub(fixed.matches('}').count());
        if in_str {
            fixed.push('"');
        }
        for _ in 0..opens {
            fixed.push('}');
        }
    }
    serde_json::from_str::<Value>(&fixed)
        .ok()
        .filter(Value::is_object)
}

/// How gpt-oss's own tool-call syntax starts when it leaks into the reply text.
/// Deep into a long session it sometimes writes a call as text, its harmony
/// tokens stripped (`assistantcommentary to=functions.Read json{"file_path": …}`),
/// and the turn would end there with nothing run.
const LEAK_MARKERS: [&str; 4] = [
    "assistantcommentary",
    "assistantanalysis",
    "commentary to=",
    "to=functions.",
];

/// Where a leaked tool call starts in the text, if one does.
pub(crate) fn leak_start(text: &str) -> Option<usize> {
    LEAK_MARKERS.iter().filter_map(|m| text.find(m)).min()
}

/// The longest ending of `text` that could be the start of a marker split across chunks.
fn marker_prefix_len(text: &str) -> usize {
    LEAK_MARKERS
        .iter()
        .flat_map(|m| (1..m.len()).filter(|&n| text.ends_with(&m[..n])))
        .max()
        .unwrap_or(0)
}

/// The last whole tool call in leaked harmony text: its name and arguments.
pub(crate) fn leaked_call(text: &str) -> Option<(String, Value)> {
    text.rmatch_indices("to=functions.").find_map(|(i, m)| {
        let rest = &text[i + m.len()..];
        let mut name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let after = &rest[name.len()..];
        // With its tokens stripped, the `json` before the arguments runs into the name.
        if name.len() > 4 && name.ends_with("json") && after.starts_with('{') {
            name.truncate(name.len() - 4);
        }
        // The arguments follow the name closely (`json{`, `<|constrain|>json{`); a
        // brace further on is something else.
        let open = after.find('{').filter(|&o| o <= 40 && !name.is_empty())?;
        complete_json(balanced_object(&after[open..])).map(|v| (name, v))
    })
}

/// A JSON object from its opening brace up to the brace that closes it (all of `s` if none does).
fn balanced_object(s: &str) -> &str {
    let (mut depth, mut in_str, mut esc) = (0usize, false, false);
    for (i, c) in s.char_indices() {
        if in_str {
            match c {
                _ if esc => esc = false,
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &s[..=i];
                }
            }
            _ => {}
        }
    }
    s
}

// ---------------------------------------------------------------- tool calls checked against their schemas
//
// A local model sometimes calls a tool in a way that doesn't fit the schema the
// client sent: a name that isn't one of the tools, a parameter the tool doesn't
// have, a number written as a string. Claude Code rejects such a call and the
// model tries again, a whole round trip later. So each call is checked against
// the request's tools here, and mended only where the mend can't change what
// the model meant:
//
// - a tool name that matches exactly one real tool when case is ignored
//   (`read` for `Read`): there is no other tool it could have meant;
// - a string holding a clean JSON number or boolean where the schema wants
//   that type (`"50"` for a number): the same value, quoted;
// - a string holding JSON where the schema wants an object or array: the same
//   value, encoded once too often.
//
// Nothing is guessed. A parameter the tool doesn't have (`line_start` where
// Read has `offset`) is left alone, never renamed or dropped: dropping it would
// quietly read the whole file instead. Such a call goes to the client as the
// model wrote it, and the client's error tells the model what was wrong, which
// is the best way it has to learn. Every call is counted, per served model, for
// `tool_call_health`.

/// How a tool call compared with its tool's schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    Valid,
    Repaired,
    Invalid,
}

/// Tool-call counts for one served model since the agent started.
#[derive(Clone, Copy, Default)]
struct CallCounts {
    valid: u64,
    repaired: u64,
    invalid: u64,
    /// Calls the model wrote as text, salvaged (also counted in the three above).
    leaked: u64,
}

/// Keyed by the served model's id, which comes from the box, not the client,
/// so it stays a handful of entries. Locked once per tool call, briefly.
static CALL_COUNTS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, CallCounts>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// How well each served model's `/v1/messages` tool calls have fit their
/// schemas since the agent started: `[{model, calls, valid, repaired, invalid,
/// leaked_as_text}]`. `calls = valid + repaired + invalid`; `leaked_as_text`
/// are calls the model wrote as text that became calls, counted in those too.
pub fn tool_call_health() -> Vec<Value> {
    let counts = CALL_COUNTS.lock().expect("call counts").clone();
    let mut out: Vec<Value> = counts
        .into_iter()
        .map(|(model, c)| json!({
            "model": model, "calls": c.valid + c.repaired + c.invalid,
            "valid": c.valid, "repaired": c.repaired, "invalid": c.invalid, "leaked_as_text": c.leaked,
        }))
        .collect();
    out.sort_by(|a, b| a["model"].as_str().cmp(&b["model"].as_str()));
    out
}

/// The tools of one request, to check that request's calls against.
#[derive(Default)]
pub struct ToolCheck {
    /// The served model the counts go under; `None` checks nothing.
    model: Option<String>,
    /// Tool name → its input schema.
    schemas: HashMap<String, Value>,
    /// Stream only: block index → (name as the model wrote it, written as text).
    open: HashMap<usize, (String, bool)>,
    /// Off for a check that isn't a real client's call (the agent reliability
    /// eval): it mends the same, but isn't counted or logged.
    uncounted: bool,
}

impl ToolCheck {
    /// Check calls against an Anthropic request's `tools`, counting them under `served_model`.
    pub fn new(tools: &Value, served_model: &str) -> Self {
        let schemas = tools
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["input_schema"].is_object())
            .filter_map(|t| Some((t["name"].as_str()?.to_string(), t["input_schema"].clone())))
            .collect();
        ToolCheck {
            model: Some(served_model.to_string()),
            schemas,
            open: HashMap::new(),
            uncounted: false,
        }
    }

    /// As [`ToolCheck::new`], for calls that aren't a client's: checked and
    /// mended the same, left out of [`tool_call_health`] and the log.
    pub fn uncounted(tools: &Value, served_model: &str) -> Self {
        ToolCheck {
            uncounted: true,
            ..Self::new(tools, served_model)
        }
    }

    /// The tool `name` means: itself, or the one tool it matches ignoring case,
    /// once gpt-oss's own syntax is taken off it. With its tokens stripped, a
    /// name can arrive as `Read<|channel|>commentary`, `functions.Read` or
    /// `Readjson` (the agent reliability eval saw these at every context
    /// length). Only a name that then is a real tool counts.
    fn resolve(&self, name: &str) -> Option<String> {
        let bare = name.split("<|").next().unwrap_or(name).trim();
        let bare = bare.strip_prefix("functions.").unwrap_or(bare);
        let mut tries = vec![name, bare];
        if let Some(b) = bare.strip_suffix("json").filter(|b| !b.is_empty()) {
            tries.push(b);
        }
        for t in tries {
            if self.schemas.contains_key(t) {
                return Some(t.to_string());
            }
            let mut alike = self.schemas.keys().filter(|k| k.eq_ignore_ascii_case(t));
            if let (Some(only), None) = (alike.next(), alike.next()) {
                return Some(only.clone());
            }
        }
        None
    }

    /// The name to show a client at the start of a call, before its input is in.
    pub fn name(&self, name: &str) -> String {
        match &self.model {
            Some(_) => self.resolve(name).unwrap_or_else(|| name.to_string()),
            None => name.to_string(),
        }
    }

    /// Check one finished call, mend what can be mended safely, and count it.
    pub fn call(&self, name: &str, input: Value, leaked: bool) -> (String, Value, CallOutcome) {
        let Some(model) = &self.model else {
            return (name.to_string(), input, CallOutcome::Valid);
        };
        let (fixed_name, input, outcome, problems) = self.check(name, input);
        if self.uncounted {
            return (fixed_name, input, outcome);
        }
        if outcome == CallOutcome::Invalid {
            // What was wrong, never the arguments themselves (they can hold a file's content).
            eprintln!(
                "tt-station-agentd: {model} called {name} in a way it doesn't take: {}",
                problems.join(", ")
            );
        }
        let mut counts = CALL_COUNTS.lock().expect("call counts");
        let c = counts.entry(model.clone()).or_default();
        match outcome {
            CallOutcome::Valid => c.valid += 1,
            CallOutcome::Repaired => c.repaired += 1,
            CallOutcome::Invalid => c.invalid += 1,
        }
        if leaked {
            c.leaked += 1;
        }
        (fixed_name, input, outcome)
    }

    /// Stream: a call starts at block `index`; the name to show for it.
    fn open_call(&mut self, index: usize, name: &str, leaked: bool) -> String {
        self.open.insert(index, (name.to_string(), leaked));
        self.name(name)
    }

    /// Stream: the call at block `index` is whole; its input, mended if it can be.
    fn close_call(&mut self, index: usize, input: Value) -> Value {
        match self.open.remove(&index) {
            Some((name, leaked)) => self.call(&name, input, leaked).1,
            None => input,
        }
    }

    /// The call checked, without counting: (name, input, outcome, what was wrong).
    fn check(&self, name: &str, mut input: Value) -> (String, Value, CallOutcome, Vec<String>) {
        let mut problems = Vec::new();
        let mut repaired = false;
        let Some(real) = self.resolve(name) else {
            return (
                name.to_string(),
                input,
                CallOutcome::Invalid,
                vec![format!("no tool is named {name}")],
            );
        };
        if real != name {
            repaired = true;
        }
        let schema = &self.schemas[&real];
        let Some(args) = input.as_object_mut() else {
            return (
                real,
                input,
                CallOutcome::Invalid,
                vec!["its input isn't an object".into()],
            );
        };
        if args.contains_key("_unparsed_arguments") {
            return (
                real,
                input,
                CallOutcome::Invalid,
                vec!["its input isn't JSON".into()],
            );
        }
        let props = schema["properties"].as_object();
        for key in schema["required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !args.contains_key(key) {
                problems.push(format!("missing {key}"));
            }
        }
        for (key, value) in args.iter_mut() {
            let Some(prop) = props.and_then(|p| p.get(key)) else {
                if schema["additionalProperties"] == false {
                    problems.push(format!("unexpected {key}"));
                }
                continue;
            };
            let types = schema_types(prop);
            if types.is_empty() || types.iter().any(|t| has_type(value, t)) {
                // Right type; an enum that leaves it out is still wrong.
            } else if let Some(v) = value.as_str().and_then(|s| unquote(s, &types)) {
                *value = v;
                repaired = true;
            } else {
                problems.push(format!("{key} should be {}", types.join(" or ")));
                continue;
            }
            if let Some(allowed) = prop["enum"].as_array() {
                if !allowed.contains(value) {
                    problems.push(format!("{key} isn't one of its choices"));
                }
            }
        }
        let outcome = match (problems.is_empty(), repaired) {
            (false, _) => CallOutcome::Invalid,
            (true, true) => CallOutcome::Repaired,
            (true, false) => CallOutcome::Valid,
        };
        (real, input, outcome, problems)
    }
}

/// A property schema's `type`, as a list (`"string"` or `["string", "null"]`);
/// empty when it has none (`anyOf` and the like, which aren't checked).
fn schema_types(prop: &Value) -> Vec<&str> {
    match &prop["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    }
}

/// Whether `v` is of JSON Schema type `t` (only the top level; an object's
/// insides aren't looked into).
fn has_type(v: &Value, t: &str) -> bool {
    match t {
        "string" => v.is_string(),
        // `5.0` is an integer to JSON Schema, and to JavaScript.
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// A string that is plainly a value of one of `types`, as that value: `"50"`
/// for a number, `"true"` for a boolean, `"[1, 2]"` for an array. JSON's own
/// grammar decides what's clean, so `"5px"`, `"0x10"`, `"NaN"` and `"1,000"`
/// stay strings (and the call stays invalid).
fn unquote(s: &str, types: &[&str]) -> Option<Value> {
    let t = s.trim();
    if types.contains(&"boolean") {
        if t.eq_ignore_ascii_case("true") {
            return Some(Value::Bool(true));
        }
        if t.eq_ignore_ascii_case("false") {
            return Some(Value::Bool(false));
        }
    }
    let parsed: Value = serde_json::from_str(t).ok()?;
    let fits = match &parsed {
        Value::Number(n) => {
            types.contains(&"number") || (types.contains(&"integer") && (n.is_i64() || n.is_u64()))
        }
        Value::Object(_) => types.contains(&"object"),
        Value::Array(_) => types.contains(&"array"),
        _ => false,
    };
    fits.then_some(parsed)
}

// ---------------------------------------------------------------- what the model is doing

/// One generation in flight, for the Code window ("thinking", "writing an edit"...).
#[derive(Clone)]
struct Live {
    /// The Code session it's for (the `x-tt-session` header Claude Code sends there).
    session: Option<String>,
    started: std::time::Instant,
    chars: usize,
    phase: &'static str,
    tool: Option<String>,
    input_tokens: u64,
    /// When its first token came out (thinking, text or a tool call).
    first_token: Option<std::time::Instant>,
    /// The model that answered (`served_by`), for the box's speed book.
    served: Option<String>,
}

static LIVE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Live>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// The `/v1/messages` generations in flight right now.
pub fn activity() -> Vec<Value> {
    LIVE.lock().expect("live").values().map(|l| {
        let secs = l.started.elapsed().as_secs_f64();
        // Its speed from the first token on: reading the prompt isn't writing.
        let writing = l.first_token.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        json!({
            "session": l.session, "seconds": secs.round(), "phase": l.phase, "tool": l.tool, "context_tokens": l.input_tokens,
            "tokens": l.chars / 4, "tokens_per_s": if writing > 1.0 { ((l.chars as f64 / 4.0) / writing).round() } else { 0.0 },
            "ttft_s": l.first_token.map(|t| (t.duration_since(l.started).as_secs_f64() * 10.0).round() / 10.0),
        })
    }).collect()
}

/// OpenAI stream chunks in, Anthropic stream events out.
pub struct Stream {
    id: String,
    model: String,
    started: bool,
    /// The open block: (index, kind).
    open: Option<(usize, &'static str)>,
    next: usize,
    /// OpenAI tool-call index → (block index, gathered arguments). Arguments go
    /// out whole, checked, when the block closes: a client can't take back a
    /// half-written call that turns out not to be JSON.
    tools: HashMap<u64, (usize, String)>,
    tool_order: Vec<u64>,
    finish: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    /// Prompt tokens the server found in its prefix cache, when it says
    /// (`usage.prompt_tokens_details.cached_tokens`).
    cached_tokens: Option<u64>,
    /// Qwen-style `<think>` text that leaked into content: dropped until it closes.
    in_think: bool,
    show_thinking: bool,
    /// An ending that may be the start of a leaked tool call, held until the next chunk shows.
    tail: String,
    /// A tool call gpt-oss wrote as text: held from where it starts, made a call at the end.
    leak: Option<String>,
    /// The request's tools, each call checked against them as its block closes.
    check: ToolCheck,
}

fn event(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

impl Stream {
    pub fn new(model: &str) -> Self {
        Self::with_thinking(model, true)
    }

    pub fn with_thinking(model: &str, show_thinking: bool) -> Self {
        let id = crate::pairing::issue_pair_id();
        LIVE.lock().expect("live").insert(
            id.clone(),
            Live {
                session: None,
                started: std::time::Instant::now(),
                chars: 0,
                phase: "reading",
                tool: None,
                input_tokens: 0,
                first_token: None,
                served: None,
            },
        );
        Stream {
            id,
            model: model.into(),
            started: false,
            open: None,
            next: 0,
            tools: HashMap::new(),
            tool_order: Vec::new(),
            finish: None,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: None,
            in_think: false,
            show_thinking,
            tail: String::new(),
            leak: None,
            check: ToolCheck::default(),
        }
    }

    /// Which Code session this generation is for.
    pub fn for_session(self, session: Option<String>) -> Self {
        if let Some(l) = LIVE.lock().expect("live").get_mut(&self.id) {
            l.session = session;
        }
        self
    }

    /// Check tool calls against the request's tools (see [`ToolCheck`]).
    pub fn checking(mut self, check: ToolCheck) -> Self {
        self.check = check;
        self
    }

    /// Which model answers it (the served id), for the box's speed book.
    pub fn served_by(self, model: &str) -> Self {
        if let Some(l) = LIVE.lock().expect("live").get_mut(&self.id) {
            l.served = Some(model.to_string());
        }
        self
    }

    /// Whether the model said it was done (a finish reason): a stream that ends
    /// without one was cut off.
    pub fn finished(&self) -> bool {
        self.finish.is_some()
    }

    fn live(&self, phase: &'static str, tool: Option<String>, chars: usize) {
        if let Some(l) = LIVE.lock().expect("live").get_mut(&self.id) {
            l.phase = phase;
            if tool.is_some() {
                l.tool = tool;
            }
            l.chars += chars;
            l.input_tokens = self.input_tokens;
            l.first_token.get_or_insert_with(std::time::Instant::now);
        }
    }

    fn start(&mut self, out: &mut Vec<String>) {
        if !self.started {
            self.started = true;
            out.push(event("message_start", json!({ "type": "message_start", "message": {
                "id": message_id(), "type": "message", "role": "assistant", "model": self.model, "content": [],
                "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": self.input_tokens, "output_tokens": 0 } } })));
        }
    }

    fn close_open(&mut self, out: &mut Vec<String>) {
        let Some((i, kind)) = self.open.take() else {
            return;
        };
        if kind == "thinking" {
            out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "signature_delta", "signature": "" } })));
        }
        if kind == "tool_use" {
            let args = self
                .tools
                .values()
                .find(|(bi, _)| *bi == i)
                .map(|(_, a)| a.clone())
                .unwrap_or_default();
            // Closed up only when the model ended the call itself, not when the
            // token limit cut it off.
            let input = parse_args(&args, self.finish.as_deref() != Some("length"));
            let input = self.check.close_call(i, input);
            out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "input_json_delta", "partial_json": input.to_string() } })));
        }
        out.push(event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": i }),
        ));
    }

    fn open_block(&mut self, out: &mut Vec<String>, kind: &'static str, block: Value) -> usize {
        self.close_open(out);
        let i = self.next;
        self.next += 1;
        self.open = Some((i, kind));
        out.push(event(
            "content_block_start",
            json!({ "type": "content_block_start", "index": i, "content_block": block }),
        ));
        i
    }

    /// One OpenAI chunk (`data:` JSON).
    pub fn chunk(&mut self, c: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(u) = c["usage"].as_object() {
            self.input_tokens = u
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.input_tokens);
            self.output_tokens = u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(self.output_tokens);
            self.cached_tokens = u
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64)
                .or(self.cached_tokens);
        }
        self.start(&mut out);
        let choice = &c["choices"][0];
        let delta = &choice["delta"];
        // Thinking (vLLM's reasoning parser puts it beside the content).
        if let Some(t) = delta["reasoning_content"]
            .as_str()
            .or(delta["reasoning"].as_str())
            .filter(|t| !t.is_empty())
        {
            self.live("thinking", None, t.len());
            if self.show_thinking {
                let i = match self.open {
                    Some((i, "thinking")) => i,
                    _ => self.open_block(
                        &mut out,
                        "thinking",
                        json!({ "type": "thinking", "thinking": "", "signature": "" }),
                    ),
                };
                out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "thinking_delta", "thinking": t } })));
            }
        }
        if let Some(raw) = delta["content"].as_str() {
            // Text outside <think>…</think>, which may open and close anywhere in a chunk.
            let mut t = String::new();
            let mut rest = raw;
            loop {
                if self.in_think {
                    match rest.split_once("</think>") {
                        Some((thought, after)) => {
                            self.live("thinking", None, thought.len());
                            self.in_think = false;
                            rest = after;
                        }
                        None => {
                            self.live("thinking", None, rest.len());
                            break;
                        }
                    }
                } else {
                    match rest.split_once("<think>") {
                        Some((before, after)) => {
                            t.push_str(before);
                            self.in_think = true;
                            rest = after;
                        }
                        None => {
                            t.push_str(rest);
                            break;
                        }
                    }
                }
            }
            if !t.is_empty() {
                self.live("writing", None, t.len());
                self.text(&mut out, &t);
            }
        }
        for tc in delta["tool_calls"].as_array().into_iter().flatten() {
            let k = tc["index"].as_u64().unwrap_or(0);
            if !self.tools.contains_key(&k) {
                let id = tc["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{}", &crate::pairing::issue_pair_id()[..20]));
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                // `open_block` gives the call the next index.
                let shown = self.check.open_call(self.next, &name, false);
                let i = self.open_block(
                    &mut out,
                    "tool_use",
                    json!({ "type": "tool_use", "id": id, "name": shown, "input": {} }),
                );
                self.tools.insert(k, (i, String::new()));
                self.tool_order.push(k);
                self.live("calling", Some(name), 0);
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                if let Some((_, a)) = self.tools.get_mut(&k) {
                    a.push_str(args);
                }
                self.live("calling", None, args.len());
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(f.to_string());
        }
        out
    }

    /// Reply text, except a tool call gpt-oss wrote as text: from where that
    /// starts it's held back, and `finish` makes it a call.
    fn text(&mut self, out: &mut Vec<String>, t: &str) {
        if let Some(l) = self.leak.as_mut() {
            l.push_str(t);
            return;
        }
        let mut s = std::mem::take(&mut self.tail);
        s.push_str(t);
        if let Some(p) = leak_start(&s) {
            self.leak = Some(s[p..].to_string());
            s.truncate(p);
        } else {
            let keep = marker_prefix_len(&s);
            self.tail = s.split_off(s.len() - keep);
        }
        self.write_text(out, &s);
    }

    fn write_text(&mut self, out: &mut Vec<String>, t: &str) {
        if t.is_empty() {
            return;
        }
        let i = match self.open {
            Some((i, "text")) => i,
            _ => self.open_block(out, "text", json!({ "type": "text", "text": "" })),
        };
        out.push(event("content_block_delta", json!({ "type": "content_block_delta", "index": i, "delta": { "type": "text_delta", "text": t } })));
    }

    /// Tokens in and out so far, from the model server's usage.
    pub fn tokens(&self) -> (u64, u64) {
        (self.input_tokens, self.output_tokens)
    }

    /// The end of the stream.
    pub fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        self.start(&mut out);
        let tail = std::mem::take(&mut self.tail);
        self.write_text(&mut out, &tail);
        if let Some(leak) = self.leak.take() {
            // Only for a reply that ended by itself with no real call in it.
            let call = if self.tools.is_empty() && self.finish.as_deref() == Some("stop") {
                leaked_call(&leak)
            } else {
                None
            };
            match call {
                Some((name, input)) => {
                    let id = format!("toolu_{}", &crate::pairing::issue_pair_id()[..20]);
                    // `open_block` gives the call the next index.
                    let shown = self.check.open_call(self.next, &name, true);
                    let i = self.open_block(
                        &mut out,
                        "tool_use",
                        json!({ "type": "tool_use", "id": id, "name": shown, "input": {} }),
                    );
                    self.tools.insert(u64::MAX, (i, input.to_string()));
                    self.tool_order.push(u64::MAX);
                }
                None => self.write_text(&mut out, &leak),
            }
        }
        self.close_open(&mut out);
        let reason = stop_reason(self.finish.as_deref(), !self.tools.is_empty());
        out.push(event("message_delta", json!({ "type": "message_delta", "delta": { "stop_reason": reason, "stop_sequence": null },
            "usage": { "input_tokens": self.input_tokens, "output_tokens": self.output_tokens } })));
        out.push(event("message_stop", json!({ "type": "message_stop" })));
        out
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let live = LIVE.lock().expect("live").remove(&self.id);
        // A Code step that ran to its end goes in that session's speed record.
        if let Some(l) = live.filter(|_| self.finished()) {
            if let Some(session) = &l.session {
                let step = crate::codespeed::Step::measured(
                    l.served.clone(),
                    l.started,
                    l.first_token,
                    std::time::Instant::now(),
                    self.input_tokens,
                    self.cached_tokens,
                    self.output_tokens,
                );
                crate::codespeed::record(session, step);
            }
        }
    }
}

/// `POST /v1/messages`.
pub(crate) async fn v1_messages(
    State(state): State<AppState>,
    auth: GatewayAuth,
    headers: axum::http::HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let session = headers
        .get("x-tt-session")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.chars().take(40).collect::<String>());
    let asked_model = req["model"].as_str().map(str::to_string);
    let Some(endpoint) =
        crate::gateway::llm_endpoint(&state, asked_model.as_deref(), session.is_some()).await
    else {
        // Asleep: wake it. "Overloaded" makes Claude Code wait and retry.
        if let Some(waking) = crate::idle::wake_for_request(&state).await {
            return error(
                StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
                "overloaded_error",
                &waking,
            );
        }
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded_error",
            "No chat model is serving on the box right now. Start one in TT Station.",
        );
    };
    let asked = req["model"].as_str().unwrap_or("quietbox").to_string();
    let thinking = req["thinking"]["type"] == "enabled";
    let mut body = to_openai(&req, &endpoint.model);
    if let Some(ctx) = crate::coding::known_context(&endpoint.model) {
        fit_output(&req, &mut body, ctx);
    }
    let url = crate::assistant::chat_completions_url(&endpoint.base_url);
    let upstream = match crate::localhttp::local()
        .post(&url)
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("Couldn't reach the model on the box: {e}"),
            )
        }
    };
    if !upstream.status().is_success() {
        let status = upstream.status().as_u16();
        let v: Value = upstream.json().await.unwrap_or(Value::Null);
        let msg = v["error"]["message"]
            .as_str()
            .or(v["message"].as_str())
            .or(v["detail"].as_str())
            .unwrap_or("the model refused the request")
            .to_string();
        return upstream_error(status, &msg);
    }
    if req["stream"] != true {
        let v: Value = match upstream.json().await {
            Ok(v) => v,
            Err(e) => return error(StatusCode::BAD_GATEWAY, "api_error", &e.to_string()),
        };
        auth.record(
            &state,
            v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
        );
        let check = ToolCheck::new(&req["tools"], &endpoint.model);
        return Json(from_openai_checked(&v, &asked, &check)).into_response();
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let check = ToolCheck::new(&req["tools"], &endpoint.model);
    let served = endpoint.model.clone();
    tokio::spawn(async move {
        let mut s = Stream::with_thinking(&asked, thinking)
            .for_session(session)
            .checking(check)
            .served_by(&served);
        relay(&mut s, upstream, &tx).await;
        let (input, output) = s.tokens();
        auth.record(&state, input, output);
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                &e.to_string(),
            )
        })
}

/// The model server's stream, translated, to the client.
async fn relay(s: &mut Stream, upstream: reqwest::Response, tx: &tokio::sync::mpsc::Sender<Bytes>) {
    {
        let mut bytes = upstream.bytes_stream();
        // Bytes until a whole line is in: a character split across two network
        // chunks must not turn into two replacement characters.
        let mut buf: Vec<u8> = Vec::new();
        let mut done = false;
        'read: while let Some(chunk) = bytes.next().await {
            let Ok(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=end).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data == "[DONE]" {
                    done = true;
                    break 'read;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    for e in s.chunk(&v) {
                        if tx.send(Bytes::from(e)).await.is_err() {
                            return; // the client went away
                        }
                    }
                }
            }
        }
        // Cut off (the server died or dropped the connection mid-reply): an error,
        // never a normal end, so a half-written tool call isn't applied.
        if !done && !s.finished() {
            let _ = tx.send(Bytes::from(event("error", json!({ "type": "error", "error": { "type": "api_error",
                "message": "The model's reply was cut off (the model server stopped answering mid-reply)." } })))).await;
            return;
        }
        for e in s.finish() {
            let _ = tx.send(Bytes::from(e)).await;
        }
    }
}

/// An error from the model server, as Anthropic clients understand it: only a
/// prompt too long for the context reads "prompt is too long" (Claude Code
/// compacts on exactly that); a server that's busy or restarting is "overloaded"
/// (Claude Code retries); anything else passes through as it is.
fn upstream_error(status: u16, msg: &str) -> Response {
    let low = msg.to_lowercase();
    let too_long = low.contains("maximum context length")
        || low.contains("context length")
        || low.contains("too long")
        || (low.contains("exceed") && (low.contains("token") || low.contains("context")));
    if too_long {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("prompt is too long: {msg}"),
        );
    }
    if status == 429 || status >= 500 {
        return error(
            StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE),
            "overloaded_error",
            &format!("The model server is busy or restarting: {msg}"),
        );
    }
    error(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        "invalid_request_error",
        msg,
    )
}

/// `POST /v1/messages/count_tokens`: an estimate, about four characters a token.
pub(crate) async fn count_tokens(_auth: GatewayAuth, Json(req): Json<Value>) -> Json<Value> {
    let chars = req["system"].to_string().len()
        + req["messages"].to_string().len()
        + req["tools"].to_string().len();
    Json(json!({ "input_tokens": chars / 4 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_a_tool_round_trip() {
        let req = json!({
            "model": "claude-sonnet-4", "max_tokens": 100, "system": [{ "type": "text", "text": "Be brief." }],
            "messages": [
                { "role": "user", "content": "List files" },
                { "role": "assistant", "content": [{ "type": "text", "text": "Sure." }, { "type": "tool_use", "id": "t1", "name": "Bash", "input": { "command": "ls" } }] },
                { "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": [{ "type": "text", "text": "a.rs" }] }, { "type": "text", "text": "thanks" }] }
            ],
            "tools": [{ "name": "Bash", "description": "run", "input_schema": { "type": "object" } }, { "type": "web_search_20250305", "name": "web_search" }]
        });
        let o = to_openai(&req, "Qwen/Qwen3");
        assert_eq!(o["model"], "Qwen/Qwen3");
        assert_eq!(o["messages"][0]["role"], "system");
        assert_eq!(
            o["messages"][2]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls\"}"
        );
        assert_eq!(o["messages"][3]["role"], "tool");
        assert_eq!(o["messages"][3]["content"], "a.rs");
        assert_eq!(o["messages"][4]["content"], "thanks");
        assert_eq!(
            o["tools"].as_array().unwrap().len(),
            1,
            "server tools are dropped"
        );

        let resp = json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "content": "<think>hm</think>Running it.",
            "tool_calls": [{ "id": "c1", "function": { "name": "Bash", "arguments": "{\"command\":\"ls\"}" } }] } }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 5 } });
        let m = from_openai(&resp, "claude-sonnet-4");
        assert_eq!(m["content"][0]["text"], "Running it.");
        assert_eq!(m["content"][1]["input"]["command"], "ls");
        assert_eq!(m["stop_reason"], "tool_use");
    }

    #[test]
    fn drops_think_tags_wherever_they_fall_in_a_chunk() {
        let mut s = Stream::new("m");
        let text = |out: Vec<String>| {
            out.iter()
                .filter(|e| e.contains("text_delta"))
                .map(|e| {
                    let data: Value =
                        serde_json::from_str(e.split("data: ").nth(1).unwrap().trim()).unwrap();
                    data["delta"]["text"].as_str().unwrap().to_string()
                })
                .collect::<String>()
        };
        let c = |t: &str| json!({ "choices": [{ "delta": { "content": t } }] });
        assert_eq!(text(s.chunk(&c("Hi <think>hmm</think>there"))), "Hi there");
        assert_eq!(text(s.chunk(&c(" and <think>still"))), " and ");
        assert_eq!(text(s.chunk(&c(" thinking</think> done"))), " done");
    }

    #[test]
    fn a_tool_calls_finish_without_a_call_ends_the_turn() {
        let resp = json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "content": "I can't do that with these tools." } }], "usage": {} });
        assert_eq!(from_openai(&resp, "m")["stop_reason"], "end_turn");
        let mut s = Stream::new("m");
        s.chunk(&json!({ "choices": [{ "delta": { "content": "I can't." }, "finish_reason": "tool_calls" }] }));
        assert!(s.finish().join("").contains("\"stop_reason\":\"end_turn\""));
    }

    #[test]
    fn says_prompt_too_long_only_when_it_is() {
        let kind = |status: u16, msg: &str| {
            let r = upstream_error(status, msg);
            (r.status().as_u16(), msg.to_string())
        };
        assert_eq!(
            kind(400, "This model's maximum context length is 32768 tokens").0,
            400
        );
        assert_eq!(kind(503, "Service Unavailable").0, 529);
        assert_eq!(kind(404, "The model `x` does not exist.").0, 404);
    }

    #[test]
    fn a_finished_code_step_leaves_its_usage_and_timing() {
        let mut s = Stream::new("m")
            .for_session(Some("test-anthropic-step".into()))
            .served_by("test-anthropic-model");
        s.chunk(&json!({ "choices": [{ "delta": { "content": "Done." } }] }));
        s.chunk(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }));
        s.chunk(&json!({ "choices": [], "usage": { "prompt_tokens": 41000, "completion_tokens": 12, "prompt_tokens_details": { "cached_tokens": 40960 } } }));
        drop(s);
        let steps = crate::codespeed::steps("test-anthropic-step");
        assert_eq!(steps.len(), 1);
        assert_eq!(
            (
                steps[0].prompt_tokens,
                steps[0].cached_tokens,
                steps[0].output_tokens
            ),
            (41000, Some(40960), 12)
        );
        assert_eq!(steps[0].model.as_deref(), Some("test-anthropic-model"));
        assert!(steps[0].ttft_ms.is_some());
        // Cut off before a finish reason: not a step to measure.
        let mut cut = Stream::new("m").for_session(Some("test-anthropic-cut".into()));
        cut.chunk(&json!({ "choices": [{ "delta": { "content": "Half" } }] }));
        drop(cut);
        assert!(crate::codespeed::steps("test-anthropic-cut").is_empty());
    }

    #[test]
    fn a_stream_without_a_finish_reason_was_cut_off() {
        let mut s = Stream::new("m");
        s.chunk(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "t", "function": { "name": "Write", "arguments": "{\"file_path\": \"a\", \"content\": \"half" } }] } }] }));
        assert!(!s.finished());
        s.chunk(&json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }));
        assert!(s.finished());
    }

    #[test]
    fn tool_names_lose_leftover_harmony_syntax() {
        let tools = json!([
            { "name": "Read", "input_schema": { "type": "object" } },
            { "name": "Grep", "input_schema": { "type": "object" } },
        ]);
        let check = ToolCheck::new(&tools, "m");
        assert_eq!(
            check.resolve("Read<|channel|>commentary").as_deref(),
            Some("Read")
        );
        assert_eq!(check.resolve("functions.Grep").as_deref(), Some("Grep"));
        assert_eq!(check.resolve("Readjson").as_deref(), Some("Read"));
        assert_eq!(check.resolve("read").as_deref(), Some("Read"));
        // Never a guess: what's left must be a real tool.
        assert_eq!(check.resolve("Search<|channel|>commentary"), None);
        assert_eq!(check.resolve("json"), None);
    }

    #[test]
    fn repairs_what_models_get_wrong_in_tool_calls() {
        assert_eq!(
            repair_json("{\"command\": \"ls\nwc -l\"}").unwrap()["command"],
            "ls\nwc -l"
        );
        assert_eq!(repair_json("{\"a\": 1,}").unwrap()["a"], 1);
        assert_eq!(
            repair_json("{\"ok\": True, \"x\": None}").unwrap()["ok"],
            true
        );
        // Cut off midway: never "closed" into a valid call with half its content...
        assert!(repair_json("{\"path\": \"a.py\", \"content\": \"cut off").is_none());
        // ...but a call the model finished, missing only its last `"}` (a tool
        // parser's slip), is closed up.
        let done = complete_json(
            "{\"command\": \"ls -la && ls crates/\", \"description\": \"List repo root and crates",
        )
        .unwrap();
        assert_eq!(done["command"], "ls -la && ls crates/");
        assert_eq!(parse_args("{\"command\": \"ls", true)["command"], "ls");
        assert!(parse_args("{\"command\": \"ls", false)
            .get("_unparsed_arguments")
            .is_some());
        assert!(repair_json("not json at all").is_none());
    }

    // What gpt-oss-20b wrote at 87K tokens into a Code session, its harmony tokens stripped.
    const LEAKED: &str = "Let's view that section around 470-500 lines.\n\nassistantcommentary to=functions.Read rename code snippet?assistantcommentary to=functions.Readjson{\"file_path\":\"desktop/src/pages/Code.tsx\",\"offset\":470,\"limit\":50}";

    #[test]
    fn a_tool_call_written_as_text_becomes_a_call() {
        let resp = json!({ "choices": [{ "finish_reason": "stop", "message": { "content": LEAKED } }], "usage": {} });
        let m = from_openai(&resp, "m");
        assert_eq!(
            m["content"][0]["text"],
            "Let's view that section around 470-500 lines."
        );
        assert_eq!(m["content"][1]["name"], "Read");
        assert_eq!(m["content"][1]["input"]["offset"], 470);
        assert_eq!(m["stop_reason"], "tool_use");

        // Streamed in small pieces, markers split across chunks.
        let mut s = Stream::new("m");
        let mut all = Vec::new();
        let chars: Vec<char> = LEAKED.chars().collect();
        for piece in chars.chunks(7) {
            all.extend(s.chunk(&json!({ "choices": [{ "delta": { "content": piece.iter().collect::<String>() } }] })));
        }
        all.extend(s.chunk(&json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] })));
        all.extend(s.finish());
        let text = all.join("");
        assert!(
            !text.contains("commentary"),
            "the leaked syntax never reaches the client"
        );
        assert!(text.contains("\"name\":\"Read\""));
        assert!(text.contains("\\\"offset\\\":470"));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));

        // Ordinary text that merely mentions the words goes through whole.
        let mut s = Stream::new("m");
        let mut all = s.chunk(&json!({ "choices": [{ "delta": { "content": "The commentary track and the assistant" }, "finish_reason": "stop" }] }));
        all.extend(s.finish());
        let said: String = all
            .iter()
            .filter(|e| e.contains("text_delta"))
            .map(|e| {
                let data: Value =
                    serde_json::from_str(e.split("data: ").nth(1).unwrap().trim()).unwrap();
                data["delta"]["text"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(said, "The commentary track and the assistant");
        assert!(all.join("").contains("\"stop_reason\":\"end_turn\""));
    }

    /// Tools as Claude Code describes them (trimmed).
    fn code_tools() -> Value {
        let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false });
        json!([
            { "name": "Read", "input_schema": obj(json!({ "file_path": { "type": "string" }, "offset": { "type": "number" }, "limit": { "type": "integer" } }), json!(["file_path"])) },
            { "name": "Grep", "input_schema": obj(json!({ "pattern": { "type": "string" }, "-i": { "type": "boolean" }, "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"] } }), json!(["pattern"])) },
            { "name": "TodoWrite", "input_schema": obj(json!({ "todos": { "type": "array" } }), json!(["todos"])) },
            { "name": "Task", "input_schema": obj(json!({ "options": { "type": "object" }, "note": { "type": ["string", "null"] } }), json!([])) },
            { "name": "Loose", "input_schema": { "type": "object", "properties": { "n": { "type": "number" } } } },
        ])
    }

    fn checked(name: &str, input: Value) -> (String, Value, CallOutcome) {
        let (name, input, outcome, _) = ToolCheck::new(&code_tools(), "unit").check(name, input);
        (name, input, outcome)
    }

    #[test]
    fn a_call_that_fits_is_left_alone() {
        let input = json!({ "file_path": "a.rs", "offset": 10, "limit": 5 });
        assert_eq!(
            checked("Read", input.clone()),
            ("Read".into(), input, CallOutcome::Valid)
        );
        // 5.0 is an integer, as far as JSON Schema and JavaScript go.
        assert_eq!(
            checked("Read", json!({ "file_path": "a", "limit": 5.0 })).2,
            CallOutcome::Valid
        );
        // Properties a schema doesn't close off may be anything.
        assert_eq!(
            checked("Loose", json!({ "n": 1, "extra": "x" })).2,
            CallOutcome::Valid
        );
    }

    #[test]
    fn a_tool_name_in_the_wrong_case_is_the_one_tool_it_matches() {
        assert_eq!(checked("read", json!({ "file_path": "a" })).0, "Read");
        assert_eq!(
            checked("GREP", json!({ "pattern": "x" })).2,
            CallOutcome::Repaired
        );
        // Two tools that differ only in case: no telling which was meant.
        let tools = json!([{ "name": "Read", "input_schema": { "type": "object" } }, { "name": "READ", "input_schema": { "type": "object" } }]);
        let (name, _, outcome, _) = ToolCheck::new(&tools, "unit").check("read", json!({}));
        assert_eq!((name.as_str(), outcome), ("read", CallOutcome::Invalid));
    }

    #[test]
    fn an_invented_tool_is_not_mapped_to_a_real_one() {
        let (name, input, outcome) = checked("Search", json!({ "pattern": "x" }));
        assert_eq!((name.as_str(), outcome), ("Search", CallOutcome::Invalid));
        assert_eq!(input, json!({ "pattern": "x" }));
    }

    #[test]
    fn an_invented_parameter_is_never_renamed_or_dropped() {
        let input = json!({ "file_path": "a.rs", "line_start": 10, "line_end": 20 });
        let (_, out, outcome, problems) =
            ToolCheck::new(&code_tools(), "unit").check("Read", input.clone());
        assert_eq!(outcome, CallOutcome::Invalid);
        assert_eq!(
            out, input,
            "line_start stays line_start: dropping it would read the whole file"
        );
        assert_eq!(problems, ["unexpected line_end", "unexpected line_start"]);
        assert_eq!(
            checked("Read", json!({ "offset": 1 })).2,
            CallOutcome::Invalid,
            "file_path is required"
        );
    }

    #[test]
    fn a_clean_number_or_boolean_in_a_string_is_unquoted() {
        let (_, input, outcome) = checked(
            "Read",
            json!({ "file_path": "a", "offset": "470", "limit": " 50 " }),
        );
        assert_eq!(outcome, CallOutcome::Repaired);
        assert_eq!(
            input,
            json!({ "file_path": "a", "offset": 470, "limit": 50 })
        );
        assert_eq!(
            checked("Read", json!({ "file_path": "a", "offset": "-1.5" })).1["offset"],
            -1.5
        );
        assert_eq!(
            checked("Grep", json!({ "pattern": "x", "-i": "True" })).1["-i"],
            true
        );
        // A string where the schema takes a string is never touched.
        assert_eq!(
            checked("Read", json!({ "file_path": "123" })).1["file_path"],
            "123"
        );
        assert_eq!(
            checked("Task", json!({ "note": "42" })).2,
            CallOutcome::Valid
        );
    }

    #[test]
    fn a_number_that_isnt_clean_stays_a_string() {
        for bad in ["5px", "0x10", "NaN", "1,000", "", "ten", "1e400"] {
            let (_, input, outcome) = checked("Read", json!({ "file_path": "a", "offset": bad }));
            assert_eq!(
                (input["offset"].as_str(), outcome),
                (Some(bad), CallOutcome::Invalid),
                "{bad:?}"
            );
        }
        // Not an integer, so not a limit.
        assert_eq!(
            checked("Read", json!({ "file_path": "a", "limit": "5.5" })).2,
            CallOutcome::Invalid
        );
        assert_eq!(
            checked("Grep", json!({ "pattern": "x", "-i": "yes" })).2,
            CallOutcome::Invalid
        );
        // A number where a string goes isn't turned into one: not a repair this makes.
        assert_eq!(
            checked("Read", json!({ "file_path": 7 })).2,
            CallOutcome::Invalid
        );
    }

    #[test]
    fn json_encoded_once_too_often_is_decoded() {
        let (_, input, outcome) =
            checked("TodoWrite", json!({ "todos": "[{\"content\": \"a\"}]" }));
        assert_eq!(outcome, CallOutcome::Repaired);
        assert_eq!(input["todos"][0]["content"], "a");
        assert_eq!(
            checked("Task", json!({ "options": "{\"x\": 1}" })).1["options"]["x"],
            1
        );
        // The wrong shape, or not JSON at all, stays as it was.
        assert_eq!(
            checked("TodoWrite", json!({ "todos": "{\"content\": \"a\"}" })).2,
            CallOutcome::Invalid
        );
        assert_eq!(
            checked("TodoWrite", json!({ "todos": "buy milk" })).2,
            CallOutcome::Invalid
        );
    }

    #[test]
    fn enums_and_unparsed_arguments_are_invalid() {
        assert_eq!(
            checked("Grep", json!({ "pattern": "x", "output_mode": "content" })).2,
            CallOutcome::Valid
        );
        assert_eq!(
            checked("Grep", json!({ "pattern": "x", "output_mode": "lines" })).2,
            CallOutcome::Invalid
        );
        assert_eq!(
            checked("Read", json!({ "_unparsed_arguments": "{\"file" })).2,
            CallOutcome::Invalid
        );
    }

    fn health(model: &str) -> Value {
        tool_call_health()
            .into_iter()
            .find(|h| h["model"] == model)
            .unwrap_or(Value::Null)
    }

    #[test]
    fn checked_and_counted_without_streaming() {
        let resp = json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "content": "", "tool_calls": [
            { "id": "c1", "function": { "name": "read", "arguments": "{\"file_path\":\"a\",\"offset\":\"5\"}" } },
            { "id": "c2", "function": { "name": "Read", "arguments": "{\"file_path\":\"a\"}" } },
            { "id": "c3", "function": { "name": "Search", "arguments": "{}" } },
        ] } }], "usage": {} });
        let m = from_openai_checked(
            &resp,
            "claude",
            &ToolCheck::new(&code_tools(), "unit-plain"),
        );
        assert_eq!(m["content"][0]["name"], "Read");
        assert_eq!(m["content"][0]["input"]["offset"], 5);
        assert_eq!(m["content"][2]["name"], "Search");
        let h = health("unit-plain");
        assert_eq!(
            (
                h["calls"].as_u64(),
                h["valid"].as_u64(),
                h["repaired"].as_u64(),
                h["invalid"].as_u64()
            ),
            (Some(3), Some(1), Some(1), Some(1))
        );

        // A call written as text is checked too, and counted as leaked.
        let resp = json!({ "choices": [{ "finish_reason": "stop", "message": { "content": LEAKED } }], "usage": {} });
        let m = from_openai_checked(&resp, "claude", &ToolCheck::new(&code_tools(), "unit-leak"));
        assert_eq!(m["content"][1]["name"], "Read");
        let h = health("unit-leak");
        assert_eq!(
            (h["valid"].as_u64(), h["leaked_as_text"].as_u64()),
            (Some(1), Some(1))
        );

        // Without the request's tools nothing is checked or counted.
        let before = tool_call_health().len();
        let m = from_openai(
            &json!({ "choices": [{ "finish_reason": "tool_calls", "message": { "tool_calls": [
            { "id": "c", "function": { "name": "read", "arguments": "{\"offset\":\"5\"}" } }] } }] }),
            "claude",
        );
        assert_eq!(
            (
                m["content"][0]["name"].as_str(),
                m["content"][0]["input"]["offset"].as_str()
            ),
            (Some("read"), Some("5"))
        );
        assert_eq!(tool_call_health().len(), before);
    }

    #[test]
    fn checked_and_counted_while_streaming() {
        let mut s = Stream::new("m").checking(ToolCheck::new(&code_tools(), "unit-stream"));
        let mut all = Vec::new();
        let call = |i: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            let mut f = json!({ "arguments": args });
            if let Some(n) = name {
                f["name"] = json!(n);
            }
            json!({ "choices": [{ "delta": { "tool_calls": [{ "index": i, "id": id, "function": f }] } }] })
        };
        all.extend(s.chunk(&call(
            0,
            Some("c1"),
            Some("read"),
            "{\"file_path\": \"a\", ",
        )));
        all.extend(s.chunk(&call(0, None, None, "\"limit\": \"20\"}")));
        all.extend(s.chunk(&call(
            1,
            Some("c2"),
            Some("Read"),
            "{\"file_path\": \"a\", \"line_start\": 3}",
        )));
        all.extend(
            s.chunk(&json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] })),
        );
        all.extend(s.finish());
        let events: Vec<Value> = all
            .iter()
            .map(|e| serde_json::from_str(e.split("data: ").nth(1).unwrap().trim()).unwrap())
            .collect();
        let starts: Vec<&Value> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .collect();
        assert_eq!(
            starts[0]["content_block"]["name"], "Read",
            "the name is mended before the block starts"
        );
        let inputs: Vec<Value> = events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .map(|e| serde_json::from_str(e["delta"]["partial_json"].as_str().unwrap()).unwrap())
            .collect();
        assert_eq!(
            inputs,
            [
                json!({ "file_path": "a", "limit": 20 }),
                json!({ "file_path": "a", "line_start": 3 })
            ]
        );
        // Input still goes out once per call, whole, when its block closes.
        assert_eq!(inputs.len(), 2);
        let h = health("unit-stream");
        assert_eq!(
            (
                h["calls"].as_u64(),
                h["repaired"].as_u64(),
                h["invalid"].as_u64()
            ),
            (Some(2), Some(1), Some(1))
        );

        // A call gpt-oss wrote as text, streamed.
        let mut s = Stream::new("m").checking(ToolCheck::new(&code_tools(), "unit-stream-leak"));
        s.chunk(&json!({ "choices": [{ "delta": { "content": LEAKED.replace("functions.Readjson", "functions.READjson") }, "finish_reason": "stop" }] }));
        let text = s.finish().join("");
        assert!(text.contains("\"name\":\"Read\""));
        let h = health("unit-stream-leak");
        assert_eq!(
            (h["repaired"].as_u64(), h["leaked_as_text"].as_u64()),
            (Some(1), Some(1))
        );
    }

    #[test]
    fn streams_thinking_then_text() {
        let mut s = Stream::with_thinking("m", true);
        let mut all = Vec::new();
        all.extend(
            s.chunk(&json!({ "choices": [{ "delta": { "reasoning_content": "Let me see." } }] })),
        );
        all.extend(s.chunk(
            &json!({ "choices": [{ "delta": { "content": "Done." }, "finish_reason": "stop" }] }),
        ));
        all.extend(s.finish());
        let text = all.join("");
        assert!(text.contains("\"thinking_delta\""));
        assert!(text.find("thinking_delta").unwrap() < text.find("text_delta").unwrap());
        assert!(text.contains("\"stop_reason\":\"end_turn\""));
    }

    #[test]
    fn streams_text_then_a_tool_call() {
        let mut s = Stream::new("m");
        let mut all = Vec::new();
        all.extend(s.chunk(&json!({ "choices": [{ "delta": { "content": "Hi" } }] })));
        all.extend(s.chunk(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "id": "c1", "function": { "name": "Bash", "arguments": "{\"co" } }] } }] })));
        all.extend(s.chunk(&json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "mmand\":\"ls\"}" } }] }, "finish_reason": "tool_calls" }] })));
        all.extend(s.finish());
        let text = all.join("");
        assert!(text.starts_with("event: message_start"));
        assert!(text.contains("\"text_delta\""));
        assert!(text.contains("\"type\":\"tool_use\""));
        assert!(text.contains("input_json_delta"));
        assert!(text.contains("\"stop_reason\":\"tool_use\""));
        assert!(text
            .trim_end()
            .ends_with("data: {\"type\":\"message_stop\"}"));
        // Blocks are closed in order: text (0) before the tool (1).
        let stop0 = text
            .find("\"content_block_stop\",\"index\":0")
            .or_else(|| text.find("\"index\":0,\"type\":\"content_block_stop\""));
        assert!(stop0.is_some());
    }
}
