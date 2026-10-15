// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Agent reliability: does the served model still make clean tool calls deep
//! into a Code session? Local models do well for the first stretch of a
//! session and then come apart (gpt-oss-20b at ~85K tokens wrote its calls as
//! text and invented tools), so this measures where.
//!
//! Each trial is a Claude Code-like conversation of a given size: a system
//! prompt and the six tools Claude Code offers (Bash, Read, Edit, Write, Grep,
//! Glob), a synthetic session of earlier calls and their results (generated
//! code, test runs, diffs, searches) padded to the size, and a last user turn
//! that needs a tool. It goes through [`crate::anthropic::to_openai`], exactly
//! as Claude Code's own requests do (streamed, the server's own sampling, no
//! thinking asked for), and the reply is graded:
//!
//! - **valid**: every call names a tool that was offered, with arguments that
//!   fit its schema (required ones there, no others, right types);
//! - **leaked**: no call, but the reply text holds one (`to=functions.`…, the
//!   harmony syntax with its tokens stripped);
//! - **invented**: a call to a tool that wasn't offered;
//! - **invalid args**: a known tool, arguments that don't fit;
//! - **no call**: it answered in text.
//!
//! That is the reply as the server returned it. A Code session doesn't stop
//! there, so each trial then goes on the way Claude Code would
//! ([`recover`]): the same stream through the translator's own streaming path
//! ([`crate::anthropic::Stream`], which salvages calls leaked as text and mends
//! tool names and arguments where that's safe), graded again; and while a call
//! is still invalid, the reply goes into the session with a tool result
//! carrying the error Claude Code gives for it ([`tool_use_error`]) and the
//! model is asked again, up to [`Options::retries`] times. A reply with no call
//! at all ends the turn, as it does in Claude Code: nothing to retry.
//!
//! The recommended compaction window ([`recommend_window`]) is the largest
//! tested size at which it, and every smaller tested size, had at least 95% of
//! trials with a valid call after the translator's repair and at most one
//! retry ([`RULE`]). The first-reply, as-returned rate is kept beside it
//! ([`recommend_window_raw`], [`RAW_RULE`]) to compare with earlier runs.
//!
//! The conversations are generated from a seed: nothing in them comes from a
//! real session.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ---------------------------------------------------------------- what's measured

/// The sizes tried, in prompt tokens.
pub const LENGTHS: [u64; 6] = [8_192, 16_384, 32_768, 65_536, 98_304, 122_880];
/// A size counts toward the recommended window at this share of valid calls...
pub const THRESHOLD: f64 = 0.95;
/// ...out of at least this many graded trials.
pub const MIN_TRIALS: usize = 20;

/// Retries counted by the recommendation: one costs a Code session a round
/// trip; a second, and the session is visibly struggling.
pub const RULE_RETRIES: usize = 1;

pub const RULE: &str = "The largest tested prompt size at which it, and every smaller tested size, had a valid tool call in at least 95% of trials after the translator's repair and at most one retry on Claude Code's validation error (at least 20 graded trials each).";

/// The rule before recovery was measured, kept to compare with earlier runs.
pub const RAW_RULE: &str = "The largest tested prompt size at which it, and every smaller tested size, had at least 95% valid tool calls (at least 20 graded trials each; calls as the server returned them, before any salvage of calls leaked as text).";

#[derive(Debug, Clone)]
pub struct Options {
    pub lengths: Vec<u64>,
    /// Different generated sessions per size.
    pub histories: usize,
    /// Final tasks asked of each session (each its own request).
    pub tasks: usize,
    /// Requests at once.
    pub concurrency: usize,
    /// Seeds the sessions: the same seed builds the same conversations.
    pub seed: u64,
    /// Times a still-invalid call is answered with its error and asked again
    /// (at least [`RULE_RETRIES`] for the recommendation to mean anything).
    pub retries: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            lengths: LENGTHS.to_vec(),
            histories: 4,
            tasks: 5,
            concurrency: 4,
            seed: 1,
            retries: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Valid,
    Leaked,
    Invented,
    InvalidArgs,
    NoCall,
    /// The request itself failed (not graded).
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trial {
    pub target: u64,
    pub history: usize,
    pub task: String,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub outcome: Outcome,
    /// How long the server took to read this session from an empty cache (its
    /// first, one-token request); the trials themselves find it cached, as a
    /// Code session's next step does.
    pub prefill_s: Option<f64>,
    /// A leaked call the translator would turn back into a valid one.
    pub salvaged: bool,
    /// Called a tool that suits the task.
    pub on_task: bool,
    pub calls: Vec<String>,
    pub detail: String,
    pub finish_reason: Option<String>,
    pub seconds: f64,
    pub ttft_s: Option<f64>,
    /// The first reply had a valid call once through the translator (salvaged,
    /// names and arguments mended).
    #[serde(default)]
    pub repaired: bool,
    /// Retries it took to get a valid call (0: none needed after the
    /// translator); `None` if it never did within the retries.
    #[serde(default)]
    pub valid_after: Option<usize>,
    /// Why it never did: "no call" (the turn ended in text), "still invalid",
    /// or a request that failed.
    #[serde(default)]
    pub failed_as: Option<String>,
    /// Each retry: what it was told and what it did.
    #[serde(default)]
    pub retries: Vec<Retry>,
}

/// One round of recovery: the error Claude Code would have answered with, and the reply after it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retry {
    /// The tool results sent back (errors, and stand-ins for calls that were valid).
    pub sent: Vec<String>,
    /// The new reply's calls, after the translator.
    pub calls: Vec<String>,
    pub valid: bool,
    pub detail: String,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Row {
    pub target: u64,
    /// Median of what the server counted.
    pub prompt_tokens: Option<u64>,
    /// Graded trials (errors left out).
    pub trials: usize,
    pub valid: usize,
    pub leaked: usize,
    pub invented: usize,
    pub invalid_args: usize,
    pub no_call: usize,
    pub errors: usize,
    pub salvaged: usize,
    pub on_task: usize,
    pub valid_rate: f64,
    /// Wilson 95% interval of `valid_rate`.
    pub ci_low: f64,
    pub ci_high: f64,
    /// Whole request, the session already in the server's prefix cache.
    pub median_s: Option<f64>,
    /// Reading the session from an empty cache (one per session).
    pub median_prefill_s: Option<f64>,
    pub median_ttft_s: Option<f64>,
    /// First replies valid after the translator's repair and salvage.
    #[serde(default)]
    pub repaired: usize,
    /// Valid within one retry (counts the repaired ones).
    #[serde(default)]
    pub within_1_retry: usize,
    /// Valid within two retries.
    #[serde(default)]
    pub within_2_retries: usize,
    /// Never valid within the retries.
    #[serde(default)]
    pub still_failing: usize,
    /// Of those, the turn ended in text with no call (nothing to retry).
    #[serde(default)]
    pub ended_without_call: usize,
    /// `within_1_retry / trials`: the rate [`RULE`] uses, and its Wilson 95% interval.
    #[serde(default)]
    pub recovered_rate: f64,
    #[serde(default)]
    pub recovered_ci_low: f64,
    #[serde(default)]
    pub recovered_ci_high: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Report {
    pub model: String,
    pub context: Option<u64>,
    pub histories: usize,
    pub tasks: usize,
    pub seed: u64,
    pub rows: Vec<Row>,
    pub recommended_window: Option<u64>,
    pub rule: String,
    /// Retries allowed per trial.
    #[serde(default)]
    pub retries: usize,
    /// The window by [`RAW_RULE`] (first replies as returned), for comparison.
    #[serde(default)]
    pub raw_window: Option<u64>,
    #[serde(default)]
    pub raw_rule: String,
}

/// Wilson score interval (95%) for `k` of `n`.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let (n, p, z) = (n as f64, k as f64 / n as f64, 1.96_f64);
    let mid = (p + z * z / (2.0 * n)) / (1.0 + z * z / n);
    let half = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / (1.0 + z * z / n);
    ((mid - half).max(0.0), (mid + half).min(1.0))
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    })
}

pub fn summarize(target: u64, trials: &[Trial]) -> Row {
    let of = |o: Outcome| trials.iter().filter(|t| t.outcome == o).count();
    let graded: Vec<&Trial> = trials
        .iter()
        .filter(|t| t.outcome != Outcome::Error)
        .collect();
    let n = graded.len();
    let valid = of(Outcome::Valid);
    let (ci_low, ci_high) = wilson(valid, n);
    let within = |k: usize| {
        graded
            .iter()
            .filter(|t| t.valid_after.is_some_and(|a| a <= k))
            .count()
    };
    let within_1_retry = within(1);
    let (recovered_ci_low, recovered_ci_high) = wilson(within_1_retry, n);
    let mut tokens: Vec<u64> = trials.iter().filter_map(|t| t.prompt_tokens).collect();
    tokens.sort();
    Row {
        target,
        prompt_tokens: tokens.get(tokens.len() / 2).copied(),
        trials: n,
        valid,
        leaked: of(Outcome::Leaked),
        invented: of(Outcome::Invented),
        invalid_args: of(Outcome::InvalidArgs),
        no_call: of(Outcome::NoCall),
        errors: of(Outcome::Error),
        salvaged: graded.iter().filter(|t| t.salvaged).count(),
        on_task: graded.iter().filter(|t| t.on_task).count(),
        valid_rate: if n == 0 { 0.0 } else { valid as f64 / n as f64 },
        ci_low,
        ci_high,
        median_s: median(graded.iter().map(|t| t.seconds).collect()),
        median_prefill_s: median({
            let mut seen = std::collections::BTreeMap::new();
            for t in trials {
                if let Some(p) = t.prefill_s {
                    seen.insert(t.history, p);
                }
            }
            seen.into_values().collect()
        }),
        median_ttft_s: median(graded.iter().filter_map(|t| t.ttft_s).collect()),
        repaired: within(0),
        within_1_retry,
        within_2_retries: within(2),
        still_failing: graded.iter().filter(|t| t.valid_after.is_none()).count(),
        ended_without_call: graded
            .iter()
            .filter(|t| t.valid_after.is_none() && t.failed_as.as_deref() == Some("no call"))
            .count(),
        recovered_rate: if n == 0 {
            0.0
        } else {
            within_1_retry as f64 / n as f64
        },
        recovered_ci_low,
        recovered_ci_high,
    }
}

fn largest_holding(rows: &[Row], rate: fn(&Row) -> f64) -> Option<u64> {
    let mut rows: Vec<&Row> = rows.iter().collect();
    rows.sort_by_key(|r| r.target);
    rows.iter()
        .take_while(|r| r.trials >= MIN_TRIALS && rate(r) >= THRESHOLD)
        .last()
        .map(|r| r.target)
}

/// See [`RULE`]. `None` when even the smallest size falls short.
pub fn recommend_window(rows: &[Row]) -> Option<u64> {
    largest_holding(rows, |r| r.recovered_rate)
}

/// See [`RAW_RULE`]: first replies as the server returned them.
pub fn recommend_window_raw(rows: &[Row]) -> Option<u64> {
    largest_holding(rows, |r| r.valid_rate)
}

// ---------------------------------------------------------------- a small, seeded random source

/// SplitMix64: the same seed gives the same sessions on any machine and version.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.next() % 10_000) as f64 / 10_000.0 < p
    }
}

fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

// ---------------------------------------------------------------- a made-up project

const NOUNS: &[&str] = &[
    "ledger", "batch", "token", "segment", "record", "cursor", "lease", "window", "shard", "frame",
    "bucket", "entry", "snapshot", "route", "budget", "queue", "span", "chunk", "schema",
    "manifest", "policy", "session", "metric", "index",
];
const VERBS: &[&str] = &[
    "load", "parse", "flush", "merge", "resolve", "compact", "encode", "decode", "validate",
    "apply", "collect", "refresh", "split", "drain", "render", "schedule", "retry", "prune",
    "rebuild", "lookup",
];
const FIELDS: &[&str] = &[
    "capacity",
    "offset",
    "limit",
    "entries",
    "deadline",
    "retries",
    "version",
    "pending",
    "weights",
    "checksum",
    "cursor",
    "labels",
    "owner",
    "budget",
    "interval",
    "generation",
];
const DOCS: &[&str] = &[
    "Returns early when there is nothing to do.",
    "Keeps the order of the input.",
    "Callers hold the lock while this runs.",
    "Counts only entries that are still live.",
    "The result is cached until the next refresh.",
    "Errors when the budget is spent.",
    "Never blocks; work that can't finish now is queued.",
    "Used by the scheduler on every tick.",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lang {
    Rust,
    Python,
    Ts,
}

struct File {
    path: String,
    lines: Vec<String>,
    /// Functions defined in it, with their (1-based) line.
    fns: Vec<(String, usize)>,
}

struct Project {
    lang: Lang,
    name: String,
    files: Vec<File>,
}

fn camel(s: &str) -> String {
    s.split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect()
}

fn ident(r: &mut Rng) -> String {
    format!("{}_{}", r.pick(VERBS), r.pick(NOUNS))
}

fn rust_file(r: &mut Rng, module: &str, others: &[&str]) -> (Vec<String>, Vec<(String, usize)>) {
    let mut l = vec![
        format!("//! The {module} stage: {}", r.pick(DOCS).to_lowercase()),
        String::new(),
        "use std::collections::HashMap;".into(),
        "use std::time::Duration;".into(),
    ];
    for o in others.iter().take(r.range(1, 3)) {
        l.push(format!("use crate::{o}::{};", camel(o)));
    }
    l.push("use crate::error::Error;".into());
    let mut fns = Vec::new();
    for _ in 0..r.range(1, 3) {
        let ty = camel(&format!("{}_{}", module, r.pick(NOUNS)));
        let fields: Vec<&str> = (0..r.range(3, 6)).map(|_| *r.pick(FIELDS)).collect();
        l.extend([
            String::new(),
            format!("/// {}", r.pick(DOCS)),
            "#[derive(Debug, Clone, Default)]".into(),
            format!("pub struct {ty} {{"),
        ]);
        for f in &fields {
            l.push(format!(
                "    pub {f}: {},",
                r.pick(&[
                    "u32",
                    "u64",
                    "usize",
                    "String",
                    "bool",
                    "Vec<u8>",
                    "Option<String>",
                    "Duration",
                    "HashMap<String, u64>"
                ])
            ));
        }
        l.extend(["}".into(), String::new(), format!("impl {ty} {{")]);
        for k in 0..r.range(3, 7) {
            let name = ident(r);
            let (a, f, g) = (*r.pick(NOUNS), *r.pick(&fields), *r.pick(&fields));
            if k > 0 {
                l.push(String::new());
            }
            l.push(format!("    /// {}", r.pick(DOCS)));
            fns.push((name.clone(), l.len() + 1));
            l.push(format!(
                "    pub fn {name}(&mut self, {a}: &str, limit: usize) -> Result<usize, Error> {{"
            ));
            for _ in 0..r.range(3, 12) {
                let n = r.range(2, 4096);
                match r.below(9) {
                    0 => l.push(format!("        let {a}_len = {a}.len().saturating_sub({n});")),
                    1 => l.extend([format!("        if self.{f}.len() > limit {{"), format!("            return Err(Error::Limit({n}));"), "        }".into()]),
                    2 => l.extend([format!("        for (i, b) in {a}.bytes().enumerate().take(limit) {{"), format!("            self.{g} = self.{g}.clone();"), format!("            if b == b'\\n' {{ tracing::trace!(\"{name}: line at {{i}}\"); }}"), "        }".into()]),
                    3 => l.push(format!("        tracing::debug!(\"{name} {a}={{}} {f}={{:?}}\", {a}, self.{f});")),
                    4 => l.push(format!("        let parts: Vec<&str> = {a}.split(',').filter(|p| !p.is_empty()).collect();")),
                    5 => l.extend([format!("        let {g}_ok = match {a}.parse::<u64>() {{"), "            Ok(v) => v > 0,".into(), format!("            Err(_) => return Err(Error::Parse(\"{a}\".into())),"), "        };".into()]),
                    6 => l.push(format!("        // {}", r.pick(DOCS))),
                    7 => l.push(format!("        let deadline = Duration::from_millis({n});")),
                    _ => l.push(format!("        self.{f} = Default::default();")),
                }
            }
            l.extend([format!("        Ok({a}.len().min(limit))"), "    }".into()]);
        }
        l.push("}".into());
    }
    l.extend([
        String::new(),
        "#[cfg(test)]".into(),
        "mod tests {".into(),
        "    use super::*;".into(),
    ]);
    for (name, _) in fns.clone().iter().take(r.range(2, 5)) {
        l.extend([
            String::new(),
            "    #[test]".into(),
            format!("    fn {name}_handles_empty_input() {{"),
            "        let mut s = Default::default();".into(),
            format!(
                "        assert_eq!(s.{name}(\"\", {}).unwrap(), 0);",
                r.range(1, 64)
            ),
            "    }".into(),
        ]);
    }
    l.push("}".into());
    (l, fns)
}

fn python_file(r: &mut Rng, module: &str, others: &[&str]) -> (Vec<String>, Vec<(String, usize)>) {
    let mut l = vec![
        format!(
            "\"\"\"The {module} stage: {}\"\"\"",
            r.pick(DOCS).to_lowercase()
        ),
        String::new(),
        "from __future__ import annotations".into(),
        String::new(),
        "import logging".into(),
        "from dataclasses import dataclass, field".into(),
    ];
    for o in others.iter().take(r.range(1, 3)) {
        l.push(format!("from .{o} import {}", camel(o)));
    }
    l.extend([String::new(), "log = logging.getLogger(__name__)".into()]);
    let mut fns = Vec::new();
    for _ in 0..r.range(1, 3) {
        let ty = camel(&format!("{}_{}", module, r.pick(NOUNS)));
        let fields: Vec<&str> = (0..r.range(3, 6)).map(|_| *r.pick(FIELDS)).collect();
        l.extend([
            String::new(),
            String::new(),
            "@dataclass".into(),
            format!("class {ty}:"),
            format!("    \"\"\"{}\"\"\"", r.pick(DOCS)),
            String::new(),
        ]);
        for f in &fields {
            l.push(format!(
                "    {f}: {} = {}",
                r.pick(&["int", "str", "list[str]", "dict[str, int]", "float", "bool"]),
                r.pick(&[
                    "0",
                    "\"\"",
                    "field(default_factory=list)",
                    "field(default_factory=dict)",
                    "0.0",
                    "False"
                ])
            ));
        }
        for _ in 0..r.range(3, 7) {
            let name = ident(r);
            let (a, f, g) = (*r.pick(NOUNS), *r.pick(&fields), *r.pick(&fields));
            l.push(String::new());
            fns.push((name.clone(), l.len() + 1));
            l.extend([
                format!(
                    "    def {name}(self, {a}: str, limit: int = {}) -> int:",
                    r.range(8, 512)
                ),
                format!("        \"\"\"{}\"\"\"", r.pick(DOCS)),
            ]);
            for _ in 0..r.range(3, 12) {
                let n = r.range(2, 4096);
                match r.below(8) {
                    0 => l.extend([
                        format!("        if len(self.{f}) > limit:"),
                        format!("            raise ValueError(f\"{name}: over {{limit}}\")"),
                    ]),
                    1 => l.extend([
                        format!("        for i, ch in enumerate({a}[:limit]):"),
                        "            if ch == \"\\n\":".into(),
                        format!("                log.debug(\"{name}: line at %d\", i)"),
                    ]),
                    2 => l.push(format!(
                        "        parts = [p for p in {a}.split(\",\") if p]"
                    )),
                    3 => l.push(format!("        log.info(\"{name} %s %s\", {a}, self.{g})")),
                    4 => l.extend([
                        "        try:".into(),
                        format!("            value = int({a})"),
                        "        except ValueError:".into(),
                        format!("            return {n}"),
                    ]),
                    5 => l.push(format!("        # {}", r.pick(DOCS))),
                    6 => l.push(format!("        self.{f} = type(self.{f})()")),
                    _ => l.push(format!("        deadline = {n} / 1000")),
                }
            }
            l.push(format!("        return min(len({a}), limit)"));
        }
    }
    (l, fns)
}

fn ts_file(r: &mut Rng, module: &str, others: &[&str]) -> (Vec<String>, Vec<(String, usize)>) {
    let mut l = vec![
        format!("// The {module} stage: {}", r.pick(DOCS).to_lowercase()),
        String::new(),
    ];
    for o in others.iter().take(r.range(1, 3)) {
        l.push(format!("import {{ {} }} from \"./{o}\";", camel(o)));
    }
    let mut fns = Vec::new();
    for _ in 0..r.range(1, 3) {
        let ty = camel(&format!("{}_{}", module, r.pick(NOUNS)));
        let fields: Vec<&str> = (0..r.range(3, 6)).map(|_| *r.pick(FIELDS)).collect();
        l.extend([
            String::new(),
            format!("/** {} */", r.pick(DOCS)),
            format!("export interface {ty} {{"),
        ]);
        for f in &fields {
            l.push(format!(
                "  {f}: {};",
                r.pick(&[
                    "number",
                    "string",
                    "boolean",
                    "string[]",
                    "Record<string, number>",
                    "Date | null"
                ])
            ));
        }
        l.push("}".into());
        for _ in 0..r.range(3, 7) {
            let name = {
                let i = ident(r);
                let c = camel(&i);
                c[..1].to_lowercase() + &c[1..]
            };
            let (a, f) = (*r.pick(NOUNS), *r.pick(&fields));
            l.extend([String::new(), format!("/** {} */", r.pick(DOCS))]);
            fns.push((name.clone(), l.len() + 1));
            l.push(format!(
                "export function {name}(state: {ty}, {a}: string, limit = {}): number {{",
                r.range(8, 512)
            ));
            for _ in 0..r.range(3, 12) {
                let n = r.range(2, 4096);
                match r.below(7) {
                    0 => l.extend([
                        format!("  if (Object.keys(state.{f} ?? {{}}).length > limit) {{"),
                        format!("    throw new Error(`{name}: over ${{limit}}`);"),
                        "  }".into(),
                    ]),
                    1 => l.push(format!("  const parts = {a}.split(\",\").filter(Boolean);")),
                    2 => l.push(format!("  console.debug(\"{name}\", {a}, state.{f});")),
                    3 => l.extend([
                        format!("  for (const [i, ch] of [...{a}].entries()) {{"),
                        "    if (i > limit || ch === \"\\n\") break;".into(),
                        "  }".into(),
                    ]),
                    4 => l.push(format!("  // {}", r.pick(DOCS))),
                    5 => l.push(format!("  const deadline = Date.now() + {n};")),
                    _ => l.push(format!("  const n = Number.parseInt({a}, 10) || {n};")),
                }
            }
            l.extend([format!("  return Math.min({a}.length, limit);"), "}".into()]);
        }
    }
    (l, fns)
}

const MODULES: &[&str] = &[
    "parser",
    "config",
    "storage",
    "scheduler",
    "http",
    "cache",
    "metrics",
    "retry",
    "auth",
    "queue",
    "index",
    "codec",
    "planner",
    "gateway",
    "journal",
    "sync",
    "limits",
    "export",
];
const PROJECTS: &[&str] = &[
    "ledgerd", "tidepool", "quarry", "fernlog", "packrat", "lanternd",
];

fn project(seed: u64) -> Project {
    let mut r = Rng::new(seed);
    let lang = *r.pick(&[Lang::Rust, Lang::Python, Lang::Ts]);
    let name = r.pick(PROJECTS).to_string();
    let mut mods: Vec<&str> = MODULES.to_vec();
    // A seeded shuffle; a dozen or so modules.
    for i in (1..mods.len()).rev() {
        let j = r.below(i + 1);
        mods.swap(i, j);
    }
    mods.truncate(r.range(10, 16));
    let files = mods
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let others: Vec<&str> = mods
                .iter()
                .filter(|o| *o != m)
                .skip(i % 3)
                .copied()
                .collect();
            let mut fr = Rng::new(seed ^ hash(m));
            let (dir, ext) = match lang {
                Lang::Rust => ("src", "rs"),
                Lang::Python => (
                    if i % 4 == 3 {
                        "src/{name}/io"
                    } else {
                        "src/{name}"
                    },
                    "py",
                ),
                Lang::Ts => (if i % 4 == 3 { "src/lib" } else { "src" }, "ts"),
            };
            let (lines, fns) = match lang {
                Lang::Rust => rust_file(&mut fr, m, &others),
                Lang::Python => python_file(&mut fr, m, &others),
                Lang::Ts => ts_file(&mut fr, m, &others),
            };
            File {
                path: format!("{}/{m}.{ext}", dir.replace("{name}", &name)),
                lines,
                fns,
            }
        })
        .collect();
    Project { lang, name, files }
}

impl Project {
    fn test_cmd(&self) -> &'static str {
        match self.lang {
            Lang::Rust => "cargo test",
            Lang::Python => "python -m pytest -q",
            Lang::Ts => "npm test",
        }
    }
    fn ext(&self) -> &'static str {
        match self.lang {
            Lang::Rust => "rs",
            Lang::Python => "py",
            Lang::Ts => "ts",
        }
    }
}

// ---------------------------------------------------------------- what the tools answer

fn numbered(f: &File, offset: usize, limit: usize) -> String {
    f.lines
        .iter()
        .enumerate()
        .skip(offset.saturating_sub(1))
        .take(limit)
        .map(|(i, l)| format!("{:>6}\t{l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn test_run(p: &Project, r: &mut Rng, fail: bool) -> String {
    let tests: Vec<(String, String)> = p
        .files
        .iter()
        .flat_map(|f| f.fns.iter().map(move |(n, _)| (f.path.clone(), n.clone())))
        .collect();
    let n = tests.len().min(r.range(12, 60));
    let start = r.below(tests.len().saturating_sub(n).max(1));
    let picked = &tests[start..start + n];
    let failed = if fail { Some(r.below(n)) } else { None };
    let module = |path: &str| {
        path.rsplit('/')
            .next()
            .unwrap_or(path)
            .split('.')
            .next()
            .unwrap_or("")
            .to_string()
    };
    match p.lang {
        Lang::Rust => {
            let mut o = vec![
                format!(
                    "   Compiling {} v0.{}.{} (/work)",
                    p.name,
                    r.range(1, 9),
                    r.range(0, 20)
                ),
                format!(
                    "    Finished `test` profile [unoptimized + debuginfo] target(s) in {}.{:02}s",
                    r.range(1, 40),
                    r.below(100)
                ),
                format!(
                    "     Running unittests src/lib.rs (target/debug/deps/{}-{:016x})",
                    p.name,
                    r.next()
                ),
                String::new(),
                format!("running {n} tests"),
            ];
            for (i, (path, t)) in picked.iter().enumerate() {
                o.push(format!(
                    "test {}::tests::{t}_handles_empty_input ... {}",
                    module(path),
                    if failed == Some(i) { "FAILED" } else { "ok" }
                ));
            }
            if let Some(i) = failed {
                let (path, t) = &picked[i];
                o.extend([
                    String::new(),
                    "failures:".into(),
                    String::new(),
                    format!(
                        "---- {}::tests::{t}_handles_empty_input stdout ----",
                        module(path)
                    ),
                    format!(
                        "thread '{}::tests::{t}_handles_empty_input' panicked at {path}:{}:{}:",
                        module(path),
                        r.range(20, 400),
                        r.range(5, 40)
                    ),
                    "assertion `left == right` failed".into(),
                    format!("  left: {}", r.range(1, 99)),
                    " right: 0".into(),
                    "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
                        .into(),
                ]);
            }
            o.extend([String::new(), format!("test result: {}. {} passed; {} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.{:02}s", if fail { "FAILED" } else { "ok" }, n - fail as usize, fail as usize, r.below(100))]);
            o.join("\n")
        }
        Lang::Python => {
            let dots: String = (0..n)
                .map(|i| if failed == Some(i) { 'F' } else { '.' })
                .collect();
            let mut o = vec![format!(
                "{dots}{:>w$}[100%]",
                "",
                w = 72usize.saturating_sub(n).max(1)
            )];
            if let Some(i) = failed {
                let (path, t) = &picked[i];
                o.extend(["=================================== FAILURES ===================================".into(), format!("_____________________________ test_{t}_empty _____________________________"), String::new(), format!("    def test_{t}_empty():"), format!(">       assert {}().{t}(\"\") == 0", camel(&module(path))), format!("E       AssertionError: assert {} == 0", r.range(1, 99)), String::new(), format!("tests/test_{}.py:{}: AssertionError", module(path), r.range(5, 200)), "=========================== short test summary info ============================".into(), format!("FAILED tests/test_{}.py::test_{t}_empty - AssertionError", module(path))]);
            }
            o.push(format!(
                "{} passed{} in {}.{:02}s",
                n - fail as usize,
                if fail { ", 1 failed" } else { "" },
                r.range(0, 9),
                r.below(100)
            ));
            o.join("\n")
        }
        Lang::Ts => {
            let mut o = vec![
                String::new(),
                format!("> {}@0.{}.0 test", p.name, r.range(1, 9)),
                "> vitest run".into(),
                String::new(),
                format!(" RUN  v2.1.{} /work", r.range(1, 9)),
                String::new(),
            ];
            for f in p.files.iter().take(r.range(4, 10)) {
                o.push(format!(
                    " ✓ {} ({} tests) {}ms",
                    f.path.replace(".ts", ".test.ts"),
                    f.fns.len(),
                    r.range(2, 90)
                ));
            }
            if let Some(i) = failed {
                let (path, t) = &picked[i];
                o.extend([
                    format!(" ❯ {} (1 failed)", path.replace(".ts", ".test.ts")),
                    format!("   × {t} handles empty input"),
                    format!("     → expected {} to be +0", r.range(1, 99)),
                ]);
            }
            o.extend([
                String::new(),
                format!(" Test Files  {} passed", r.range(4, 12)),
                format!(
                    "      Tests  {} passed{}",
                    n,
                    if fail { " | 1 failed" } else { "" }
                ),
                format!("   Duration  {}.{:02}s", r.range(0, 9), r.below(100)),
            ]);
            o.join("\n")
        }
    }
}

fn git_status(p: &Project, r: &mut Rng) -> String {
    let mut o = vec![
        "On branch fix/".to_string() + &ident(r).replace('_', "-"),
        "Changes not staged for commit:".into(),
        "  (use \"git add <file>...\" to update what will be committed)".into(),
        "  (use \"git restore <file>...\" to discard changes in working directory)".into(),
    ];
    for _ in 0..r.range(1, 4) {
        o.push(format!("\tmodified:   {}", r.pick(&p.files).path));
    }
    o.extend([
        String::new(),
        "no changes added to commit (use \"git add\" and/or \"git commit -a\")".into(),
    ]);
    o.join("\n")
}

fn git_diff(p: &Project, r: &mut Rng) -> String {
    let f = r.pick(&p.files);
    let at = r.range(1, f.lines.len().saturating_sub(12).max(1));
    let mut o = vec![
        format!("diff --git a/{0} b/{0}", f.path),
        format!(
            "index {:07x}..{:07x} 100644",
            r.next() & 0xFFFFFFF,
            r.next() & 0xFFFFFFF
        ),
        format!("--- a/{}", f.path),
        format!("+++ b/{}", f.path),
        format!("@@ -{at},10 +{at},11 @@"),
    ];
    for (k, l) in f.lines.iter().skip(at - 1).take(10).enumerate() {
        if k == 5 {
            o.push(format!("-{l}"));
            o.push(format!("+{}", l.replace("limit", "limit.max(1)")));
            o.push(format!(
                "+{}",
                if p.lang == Lang::Python {
                    "        # guard against a zero limit"
                } else {
                    "        // guard against a zero limit"
                }
            ));
        } else {
            o.push(format!(" {l}"));
        }
    }
    o.join("\n")
}

fn grep_hits(p: &Project, needle: &str) -> String {
    let hits: Vec<String> = p
        .files
        .iter()
        .flat_map(|f| {
            f.lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains(needle))
                .map(move |(i, l)| format!("{}:{}:{}", f.path, i + 1, l.trim()))
        })
        .take(40)
        .collect();
    if hits.is_empty() {
        "No matches found".into()
    } else {
        hits.join("\n")
    }
}

// ---------------------------------------------------------------- the session

/// The system prompt: written for this eval in the manner of a coding agent's.
fn system_prompt(p: &Project) -> String {
    format!(
        "You are an interactive coding agent working in the user's terminal, on the project in the current directory. You help with software engineering tasks: fixing bugs, adding features, refactoring, explaining code and running commands.

# How to work
- Use the tools to look at the code before you change it. Never guess what a file contains: read it.
- Read a file before you edit it. Edits replace an exact string, so copy the old text exactly, with its indentation.
- Search with Grep and find files with Glob rather than running grep or find through Bash.
- Large files: read the part you need with offset and limit instead of the whole file.
- After a change, run the project's tests ({test}) and fix what breaks.
- Keep changes small and in the style of the code around them. Don't add files unless they're needed.
- Several independent tool calls can be made in one reply.

# Tone
- Be brief. Say what you are about to do in a sentence, then do it. No preamble, no summaries of what you just did unless asked.
- When you reference code, give its path and line, like src/parser.rs:42.
- If a request is unclear, ask one short question instead of guessing.

# Safety
- Don't run commands that delete data or rewrite history (rm -rf, git reset --hard, git push --force) unless the user asks for exactly that.
- Never commit unless asked.

<env>
Working directory: /work
Is directory a git repo: Yes
Platform: linux
Shell: bash
Today's date: 2026-10-15
</env>

Project notes (from the repository's AGENTS.md):
- {name} is a {kind}. Source is under src/; tests sit next to the code.
- Build and test: {test}. Lint before finishing.
- Errors are returned, not panicked on; log with the module's logger.",
        test = p.test_cmd(),
        name = p.name,
        kind = match p.lang {
            Lang::Rust => "Rust service (edition 2021)",
            Lang::Python => "Python 3.12 package",
            Lang::Ts => "TypeScript library (Node 22, vitest)",
        },
    )
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#" })
}

/// The six tools, in Anthropic's form, as Claude Code offers them (descriptions in our own words).
pub fn tools() -> Value {
    json!([
        { "name": "Bash", "description": "Runs a bash command in a persistent shell, with an optional timeout, and returns its output. Use it for builds, tests, git and other terminal work -- not for reading, searching or editing files, which have their own tools. Quote paths that contain spaces. Output past 30000 characters is cut.",
          "input_schema": schema(json!({
            "command": { "type": "string", "description": "The command to run" },
            "description": { "type": "string", "description": "What the command does, in five to ten words" },
            "timeout": { "type": "number", "description": "Timeout in milliseconds (at most 600000)" },
            "run_in_background": { "type": "boolean", "description": "Run the command in the background and return at once" }
          }), &["command"]) },
        { "name": "Read", "description": "Reads a file from the local filesystem. file_path must be absolute or relative to the working directory. By default it reads up to 2000 lines from the start; for large files give offset (the line to start at) and limit (how many lines). Lines are returned numbered, cat -n style, starting at 1.",
          "input_schema": schema(json!({
            "file_path": { "type": "string", "description": "The path of the file to read" },
            "offset": { "type": "number", "description": "The line number to start reading from" },
            "limit": { "type": "number", "description": "The number of lines to read" }
          }), &["file_path"]) },
        { "name": "Edit", "description": "Replaces an exact string in a file. Read the file first. old_string must appear in the file exactly once (with its indentation) unless replace_all is true, and new_string must differ from it.",
          "input_schema": schema(json!({
            "file_path": { "type": "string", "description": "The path of the file to change" },
            "old_string": { "type": "string", "description": "The text to replace" },
            "new_string": { "type": "string", "description": "The text to put in its place" },
            "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)" }
          }), &["file_path", "old_string", "new_string"]) },
        { "name": "Write", "description": "Writes a file, replacing it if it exists. Read an existing file before overwriting it. Prefer Edit for changes to existing files.",
          "input_schema": schema(json!({
            "file_path": { "type": "string", "description": "The path of the file to write" },
            "content": { "type": "string", "description": "The full content of the file" }
          }), &["file_path", "content"]) },
        { "name": "Grep", "description": "Searches file contents with a regular expression (ripgrep syntax). Filter files with glob or type. output_mode is \"files_with_matches\" (default), \"content\" (matching lines; -n, -A, -B, -C apply) or \"count\".",
          "input_schema": schema(json!({
            "pattern": { "type": "string", "description": "The regular expression to search for" },
            "path": { "type": "string", "description": "File or directory to search (default: the working directory)" },
            "glob": { "type": "string", "description": "Only files matching this glob, e.g. \"*.rs\"" },
            "type": { "type": "string", "description": "Only files of this type, e.g. \"py\"" },
            "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "What to return" },
            "-i": { "type": "boolean", "description": "Case-insensitive" },
            "-n": { "type": "boolean", "description": "Line numbers (content mode)" },
            "-A": { "type": "number", "description": "Lines after each match (content mode)" },
            "-B": { "type": "number", "description": "Lines before each match (content mode)" },
            "-C": { "type": "number", "description": "Lines around each match (content mode)" },
            "head_limit": { "type": "number", "description": "Only the first N results" },
            "multiline": { "type": "boolean", "description": "Let patterns span lines" }
          }), &["pattern"]) },
        { "name": "Glob", "description": "Finds files by name with a glob pattern such as \"src/**/*.ts\", newest first.",
          "input_schema": schema(json!({
            "pattern": { "type": "string", "description": "The glob pattern" },
            "path": { "type": "string", "description": "The directory to search in (default: the working directory)" }
          }), &["pattern"]) }
    ])
}

/// What's wrong with `args` for the tool called `name` in `tools`, if anything.
pub fn check_call(tools: &Value, name: &str, args: &Value) -> Result<(), String> {
    let tool = tools
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == name)
        .ok_or_else(|| format!("no tool {name}"))?;
    let s = &tool["input_schema"];
    let obj = args.as_object().ok_or("arguments aren't a JSON object")?;
    for r in s["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !obj.contains_key(r) {
            return Err(format!("missing {r}"));
        }
    }
    for (k, v) in obj {
        let Some(p) = s["properties"].get(k) else {
            if s["additionalProperties"] == false {
                return Err(format!("unknown parameter {k}"));
            }
            continue;
        };
        let fits = match p["type"].as_str() {
            Some("string") => v.is_string(),
            Some("number") => v.is_number(),
            Some("integer") => v.as_i64().is_some() || v.as_u64().is_some(),
            Some("boolean") => v.is_boolean(),
            Some("array") => v.is_array(),
            Some("object") => v.is_object(),
            _ => true,
        };
        if !fits {
            return Err(format!(
                "{k} should be a {}, got {v}",
                p["type"].as_str().unwrap_or("?")
            ));
        }
        if let Some(allowed) = p["enum"].as_array() {
            if !allowed.contains(v) {
                return Err(format!(
                    "{k} = {v} isn't one of {}",
                    Value::Array(allowed.clone())
                ));
            }
        }
    }
    Ok(())
}

fn tool_id(r: &mut Rng) -> String {
    const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    "toolu_01".to_string()
        + &(0..22)
            .map(|_| B62[r.below(62)] as char)
            .collect::<String>()
}

/// One step of the session: what the assistant said and called, and what came back.
fn step(p: &Project, r: &mut Rng, out: &mut Vec<Value>) {
    let f = r.pick(&p.files);
    let fun = f
        .fns
        .first()
        .map(|x| x.0.clone())
        .unwrap_or_else(|| ident(r));
    let (fun, fun_line) = f
        .fns
        .get(r.below(f.fns.len().max(1)))
        .cloned()
        .unwrap_or((fun, 1));
    let roll = r.below(100);
    let (text, name, input, result) = if roll < 34 {
        // Read: a whole small file, or a window of a big one.
        let (offset, limit) = if f.lines.len() <= 250 || r.chance(0.3) {
            (None, None)
        } else {
            let o = fun_line.saturating_sub(r.range(5, 30)).max(1);
            (Some(o), Some(r.range(60, 220)))
        };
        let mut input = json!({ "file_path": format!("/work/{}", f.path) });
        if let (Some(o), Some(l)) = (offset, limit) {
            input["offset"] = json!(o);
            input["limit"] = json!(l);
        }
        let text = r
            .pick(&[
                "Let me look at `{p}`.",
                "I'll read `{f}` in `{p}`.",
                "Let me check how `{f}` handles an empty input.",
                "Reading the rest of `{p}`.",
                "",
            ])
            .replace("{p}", &f.path)
            .replace("{f}", &fun);
        (
            text,
            "Read",
            input,
            numbered(f, offset.unwrap_or(1), limit.unwrap_or(2000)),
        )
    } else if roll < 72 {
        let (cmd, desc, res) = match r.below(8) {
            0..=2 => {
                let fail = r.chance(0.35);
                (
                    p.test_cmd().to_string(),
                    "Run the test suite",
                    test_run(p, r, fail),
                )
            }
            3 => (
                "git status".into(),
                "Show working tree status",
                git_status(p, r),
            ),
            4 => (
                format!("git diff {}", f.path),
                "Show the change so far",
                git_diff(p, r),
            ),
            5 => (
                format!("wc -l src/*.{}", p.ext()),
                "Count lines per source file",
                p.files
                    .iter()
                    .map(|x| format!("{:>6} {}", x.lines.len(), x.path))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            6 => (
                format!("rg -n \"{fun}\" src"),
                "Find uses of the function",
                grep_hits(p, &fun),
            ),
            _ => (
                "ls -la src".into(),
                "List source files",
                p.files
                    .iter()
                    .map(|x| {
                        format!(
                            "-rw-r--r-- 1 dev dev {:>6} Oct 14 {:02}:{:02} {}",
                            x.lines.iter().map(|l| l.len() + 1).sum::<usize>(),
                            r.range(8, 19),
                            r.below(60),
                            x.path.rsplit('/').next().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        };
        let text = r
            .pick(&[
                "Now let me run the tests.",
                "Let me see where things stand.",
                "Checking the change.",
                "",
                "Let me verify that with a quick command.",
            ])
            .to_string();
        (
            text,
            "Bash",
            json!({ "command": cmd, "description": desc }),
            res,
        )
    } else if roll < 86 {
        let at = fun_line.clamp(1, f.lines.len());
        let old = f.lines[at.saturating_sub(1)..(at + 2).min(f.lines.len())].join("\n");
        let new = old.replace("limit", "limit.max(1)");
        let text = format!("The problem is in `{fun}`: a zero limit isn't handled. Fixing it.");
        let snippet = numbered(f, at.saturating_sub(4).max(1), 10);
        (text, "Edit", json!({ "file_path": format!("/work/{}", f.path), "old_string": old, "new_string": new, "replace_all": false }), format!("The file /work/{} has been updated. Here's the result of running `cat -n` on a snippet of the edited file:\n{snippet}", f.path))
    } else if roll < 95 {
        let text = format!("Let me find where `{fun}` is used.");
        let mode = if r.chance(0.5) {
            "content"
        } else {
            "files_with_matches"
        };
        let res = if mode == "content" {
            grep_hits(p, &fun)
        } else {
            p.files
                .iter()
                .filter(|x| x.lines.iter().any(|l| l.contains(&fun)))
                .map(|x| x.path.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut input = json!({ "pattern": fun, "path": "src", "output_mode": mode });
        if mode == "content" {
            input["-n"] = json!(true);
        }
        (text, "Grep", input, res)
    } else if roll < 98 {
        (
            String::new(),
            "Glob",
            json!({ "pattern": format!("src/**/*.{}", p.ext()) }),
            p.files
                .iter()
                .map(|x| format!("/work/{}", x.path))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    } else {
        let path = format!("notes/{}.md", r.pick(NOUNS));
        (
            "Writing down what I found so far.".into(),
            "Write",
            json!({ "file_path": format!("/work/{path}"), "content": format!("# Notes\n\n- `{fun}` in {} needs a non-zero limit.\n- Tests: `{}`.\n", f.path, p.test_cmd()) }),
            format!("File created successfully at: /work/{path}"),
        )
    };
    let id = tool_id(r);
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
    out.push(json!({ "role": "assistant", "content": content }));
    let result: String = result.chars().take(30_000).collect();
    out.push(json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": id, "content": result }] }));
}

fn opening(p: &Project, r: &mut Rng) -> String {
    let f = r.pick(&p.files);
    let fun = f.fns.first().map(|x| x.0.as_str()).unwrap_or("main");
    r.pick(&[
        "`{f}` in {p} returns the wrong count when the input is empty. Find out why and fix it, then make sure the tests pass.",
        "The tests for {p} started failing after the last refactor. Can you get them green again without changing what `{f}` returns for normal input?",
        "Add a guard so `{f}` in {p} never runs with a zero limit, and add a test for it.",
        "Review {p} for places that can loop past the limit, starting with `{f}`, and fix them.",
    ])
    .replace("{f}", fun)
    .replace("{p}", &f.path)
}

/// A session of about `chars` characters: earlier work, then the assistant's last reply.
fn history(p: &Project, seed: u64, chars: usize) -> Vec<Value> {
    let mut r = Rng::new(seed);
    let mut out = vec![json!({ "role": "user", "content": opening(p, &mut r) })];
    let size = |m: &Vec<Value>| m.iter().map(|v| v.to_string().len()).sum::<usize>();
    let mut steps = 0;
    while size(&out) < chars {
        step(p, &mut r, &mut out);
        steps += 1;
        // Now and then the work moves on to a new request, as it does in a long session.
        if steps % 30 == 0 && size(&out) < chars {
            out.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "Done: the guard is in and the tests pass." }] }));
            out.push(json!({ "role": "user", "content": opening(p, &mut r) }));
        }
    }
    out.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": r.pick(&["Done. The tests pass now.", "That's fixed; all tests pass.", "The change is in and the suite is green."]).to_string() }] }));
    out
}

// ---------------------------------------------------------------- the last turn

pub struct Task {
    pub key: &'static str,
    /// Tools that suit it (any valid call still counts as valid).
    pub expect: &'static [&'static str],
    text: fn(&Project, &mut Rng) -> String,
}

fn some_file<'a>(p: &'a Project, r: &mut Rng) -> &'a File {
    r.pick(&p.files)
}

pub const TASKS: &[Task] = &[
    Task {
        key: "read_function",
        expect: &["Read", "Grep"],
        text: |p, r| {
            let f = some_file(p, r);
            format!(
                "Show me the `{}` function in {}.",
                f.fns.first().map_or("main", |x| x.0.as_str()),
                f.path
            )
        },
    },
    Task {
        key: "read_range",
        expect: &["Read"],
        text: |p, r| {
            let f = some_file(p, r);
            let a = r.range(10, f.lines.len().max(20));
            format!(
                "Read {} from line {a} to line {}.",
                f.path,
                a + r.range(20, 60)
            )
        },
    },
    Task {
        key: "run_tests",
        expect: &["Bash"],
        text: |p, _| {
            format!(
                "Run the whole test suite again ({}) and tell me what fails.",
                p.test_cmd()
            )
        },
    },
    Task {
        key: "find_definition",
        expect: &["Grep", "Bash"],
        text: |p, r| {
            let f = some_file(p, r);
            format!(
                "Where is `{}` defined? Search the whole repo.",
                f.fns.last().map_or("main", |x| x.0.as_str())
            )
        },
    },
    Task {
        key: "list_files",
        expect: &["Glob", "Bash"],
        text: |p, _| format!("List every .{} file in the project.", p.ext()),
    },
    Task {
        key: "git_status",
        expect: &["Bash"],
        text: |_, _| "Check git status and show me what's changed so far.".into(),
    },
    Task {
        key: "write_file",
        expect: &["Write"],
        text: |p, r| {
            let f = some_file(p, r);
            let m = f
                .path
                .rsplit('/')
                .next()
                .unwrap_or("")
                .split('.')
                .next()
                .unwrap_or("");
            format!("Create docs/{m}.md with a short paragraph describing what the {m} module of {} does.", p.name)
        },
    },
    Task {
        key: "rename",
        expect: &["Edit", "Read", "Grep"],
        text: |p, r| {
            let f = some_file(p, r);
            let n = f.fns.first().map_or("main", |x| x.0.as_str());
            format!(
                "In {}, rename the function `{n}` to `{n}_checked` (only its definition for now).",
                f.path
            )
        },
    },
    Task {
        key: "count_lines",
        expect: &["Bash"],
        text: |p, r| format!("How many lines is {}? Use wc.", some_file(p, r).path),
    },
    Task {
        key: "find_todos",
        expect: &["Grep", "Bash"],
        text: |_, _| "Find every TODO or FIXME comment under src/.".into(),
    },
    Task {
        key: "read_head",
        expect: &["Read"],
        text: |p, r| {
            format!(
                "Open {} and show me just its first 40 lines.",
                some_file(p, r).path
            )
        },
    },
    Task {
        key: "search_limit",
        expect: &["Grep", "Bash"],
        text: |p, r| {
            format!(
                "Search {} for every place that compares against `limit`.",
                some_file(p, r).path
            )
        },
    },
];

// ---------------------------------------------------------------- talking to the server

pub struct Server {
    pub http: reqwest::Client,
    /// The OpenAI base, ending in `/v1`.
    pub base_url: String,
    pub model: String,
    pub context: Option<u64>,
}

impl Server {
    fn chat_url(&self) -> String {
        crate::assistant::chat_completions_url(&self.base_url)
    }

    /// The prompt's size in tokens as the server counts it when it answers. A
    /// one-token request: `/tokenize` doesn't do (vLLM renders gpt-oss's chat for
    /// `/tokenize` with the Jinja template, ~10% longer than the harmony prompt
    /// it actually runs). It fills the server's prefix cache with the session.
    async fn count(&self, body: &Value) -> Option<u64> {
        let mut req = body.clone();
        req["stream"] = json!(false);
        req["max_tokens"] = json!(1);
        if let Some(o) = req.as_object_mut() {
            o.remove("stream_options");
        }
        let v: Value = self
            .http
            .post(self.chat_url())
            .json(&req)
            .timeout(Duration::from_secs(900))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        v["usage"]["prompt_tokens"].as_u64()
    }
}

/// The request Claude Code's version of this conversation becomes on its way to the model.
fn request(server: &Server, system: &str, messages: &[Value]) -> (Value, Value) {
    let req = json!({
        "model": "quietbox",
        "system": [{ "type": "text", "text": system }],
        "messages": messages,
        "tools": tools(),
        // What Claude Code sends on the box: a 16K answer budget, adaptive
        // thinking (so none asked of the model), streamed, no temperature.
        "max_tokens": 16_384,
        "thinking": { "type": "adaptive" },
        "stream": true,
    });
    let mut body = crate::anthropic::to_openai(&req, &server.model);
    if let Some(ctx) = server.context {
        crate::anthropic::fit_output(&req, &mut body, ctx);
    }
    (req, body)
}

#[derive(Default)]
struct Reply {
    content: String,
    /// (name, raw arguments)
    calls: Vec<(String, String)>,
    finish: Option<String>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    ttft: Option<f64>,
    /// Every chunk as it came, for the translator to read again.
    chunks: Vec<Value>,
}

async fn send(server: &Server, body: &Value) -> Result<Reply, String> {
    let started = Instant::now();
    let r = server
        .http
        .post(server.chat_url())
        .json(body)
        .send()
        .await
        .map_err(|e| format!("couldn't reach the model: {e}"))?;
    if !r.status().is_success() {
        let status = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        return Err(format!(
            "{status}: {}",
            v["error"]["message"]
                .as_str()
                .or(v["message"].as_str())
                .unwrap_or("")
        ));
    }
    let mut reply = Reply::default();
    let mut bytes = r.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut done = false;
    'read: while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(|e| format!("the reply was cut off: {e}"))?;
        buf.extend_from_slice(&chunk);
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&raw);
            let Some(data) = line.trim().strip_prefix("data:").map(str::trim) else {
                continue;
            };
            if data == "[DONE]" {
                done = true;
                break 'read;
            }
            let Ok(v) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            reply.chunks.push(v.clone());
            if let Some(u) = v["usage"].as_object() {
                reply.prompt_tokens = u
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .or(reply.prompt_tokens);
                reply.completion_tokens = u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .or(reply.completion_tokens);
            }
            let c = &v["choices"][0];
            let d = &c["delta"];
            let any = ["content", "reasoning_content", "reasoning", "tool_calls"]
                .iter()
                .any(|k| !d[*k].is_null());
            if any && reply.ttft.is_none() {
                reply.ttft = Some(started.elapsed().as_secs_f64());
            }
            if let Some(t) = d["content"].as_str() {
                reply.content.push_str(t);
            }
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let i = tc["index"]
                    .as_u64()
                    .unwrap_or(reply.calls.len().saturating_sub(1) as u64)
                    as usize;
                while reply.calls.len() <= i {
                    reply.calls.push(Default::default());
                }
                if let Some(n) = tc["function"]["name"].as_str() {
                    reply.calls[i].0.push_str(n);
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    reply.calls[i].1.push_str(a);
                }
            }
            if let Some(f) = c["finish_reason"].as_str() {
                reply.finish = Some(f.to_string());
            }
        }
    }
    if !done && reply.finish.is_none() {
        return Err("the reply was cut off".into());
    }
    Ok(reply)
}

fn clip(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

/// (outcome, salvaged, on task, the calls, what happened)
pub fn grade(
    tools: &Value,
    content: &str,
    calls: &[(String, String)],
    expect: &[&str],
) -> (Outcome, bool, bool, Vec<String>, String) {
    let shown: Vec<String> = calls
        .iter()
        .map(|(n, a)| clip(&format!("{n}({a})"), 200))
        .collect();
    if calls.is_empty() {
        let text = crate::assistant::strip_think(content);
        if let Some(at) = crate::anthropic::leak_start(&text) {
            let salvage = crate::anthropic::leaked_call(&text[at..]);
            let salvaged = salvage
                .as_ref()
                .is_some_and(|(n, a)| check_call(tools, n, a).is_ok());
            let on_task = salvaged
                && salvage
                    .as_ref()
                    .is_some_and(|(n, _)| expect.contains(&n.as_str()));
            return (
                Outcome::Leaked,
                salvaged,
                on_task,
                shown,
                clip(&text[at..], 200),
            );
        }
        return (Outcome::NoCall, false, false, shown, clip(&text, 200));
    }
    let names_ok = |n: &str| {
        tools
            .as_array()
            .into_iter()
            .flatten()
            .any(|t| t["name"] == n)
    };
    if let Some((n, _)) = calls.iter().find(|(n, _)| !names_ok(n)) {
        return (
            Outcome::Invented,
            false,
            false,
            shown,
            format!("called {n}, which wasn't offered"),
        );
    }
    for (n, a) in calls {
        let parsed: Value = match serde_json::from_str(a.trim()) {
            Ok(v) => v,
            Err(_) if a.trim().is_empty() => json!({}),
            Err(e) => {
                return (
                    Outcome::InvalidArgs,
                    false,
                    false,
                    shown,
                    format!("{n}: arguments aren't JSON ({e})"),
                )
            }
        };
        if let Err(why) = check_call(tools, n, &parsed) {
            return (
                Outcome::InvalidArgs,
                false,
                false,
                shown,
                format!("{n}: {why}"),
            );
        }
    }
    let on_task = calls.iter().any(|(n, _)| expect.contains(&n.as_str()));
    (
        Outcome::Valid,
        false,
        on_task,
        shown.clone(),
        shown.join(", "),
    )
}

// ---------------------------------------------------------------- what Claude Code gets, and does with it

/// The reply as Claude Code receives it: the server's stream through the
/// translator's own streaming path, as `/v1/messages` runs it (thinking not
/// asked for, so not shown), put back together into the message's text and
/// tool_use blocks. The check is the translator's, uncounted: the eval's calls
/// stay out of the box's tool-call health.
pub fn translate(tools: &Value, model: &str, chunks: &[Value]) -> Vec<Value> {
    let mut s = crate::anthropic::Stream::with_thinking("quietbox", false)
        .checking(crate::anthropic::ToolCheck::uncounted(tools, model));
    let mut events: Vec<String> = chunks.iter().flat_map(|c| s.chunk(c)).collect();
    events.extend(s.finish());
    assemble(&events)
}

/// Anthropic stream events, back into the content blocks they spell out.
fn assemble(events: &[String]) -> Vec<Value> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut inputs: Vec<String> = Vec::new();
    for e in events {
        let Some(v) = e
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .and_then(|d| serde_json::from_str::<Value>(d).ok())
        else {
            continue;
        };
        let i = v["index"].as_u64().unwrap_or(0) as usize;
        match v["type"].as_str() {
            Some("content_block_start") => {
                while blocks.len() <= i {
                    blocks.push(Value::Null);
                    inputs.push(String::new());
                }
                blocks[i] = v["content_block"].clone();
            }
            Some("content_block_delta") if i < blocks.len() => {
                let d = &v["delta"];
                match d["type"].as_str() {
                    Some("text_delta") => {
                        let t = format!(
                            "{}{}",
                            blocks[i]["text"].as_str().unwrap_or(""),
                            d["text"].as_str().unwrap_or("")
                        );
                        blocks[i]["text"] = json!(t);
                    }
                    Some("input_json_delta") => {
                        inputs[i].push_str(d["partial_json"].as_str().unwrap_or(""))
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") if i < blocks.len() && blocks[i]["type"] == "tool_use" => {
                // As a client takes a call's input: the JSON it was sent, `{}` for none.
                blocks[i]["input"] = if inputs[i].trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&inputs[i]).unwrap_or(Value::Null)
                };
            }
            _ => {}
        }
    }
    blocks.retain(|b| b["type"] == "text" || b["type"] == "tool_use");
    blocks
}

fn json_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What Claude Code answers a call it won't run with (the `tool_result`,
/// marked as an error), worded as Claude Code words it; `None` for a call it
/// would run. The unknown-tool, missing and unexpected parameter wording is
/// Claude Code's own, as it appears in Code sessions on the box (2.1.x); the
/// wrong-type and enum wording wasn't seen there and follows its formatter's
/// pattern.
pub fn tool_use_error(tools: &Value, name: &str, input: &Value) -> Option<String> {
    let Some(tool) = tools
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| t["name"] == name)
    else {
        return Some(format!(
            "<tool_use_error>Error: No such tool available: {name}</tool_use_error>"
        ));
    };
    let why = check_call(tools, name, input).err()?;
    let s = &tool["input_schema"];
    let empty = serde_json::Map::new();
    let obj = input.as_object().unwrap_or(&empty);
    let mut issues = Vec::new();
    for r in s["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|r| !obj.contains_key(*r))
    {
        issues.push(format!("The required parameter `{r}` is missing"));
    }
    let mut unexpected = Vec::new();
    for (k, v) in obj {
        let Some(p) = s["properties"].get(k) else {
            unexpected.push(format!("An unexpected parameter `{k}` was provided"));
            continue;
        };
        let want = p["type"].as_str().unwrap_or("");
        let fits = match want {
            "string" | "number" | "boolean" | "array" | "object" => json_type(v) == want,
            "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
            _ => true,
        };
        if !fits {
            issues.push(format!(
                "The parameter `{k}` type is expected as `{want}` but provided as `{}`",
                json_type(v)
            ));
        } else if let Some(allowed) = p["enum"].as_array().filter(|a| !a.contains(v)) {
            issues.push(format!(
                "The parameter `{k}` must be one of {}",
                allowed
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    issues.extend(unexpected);
    if issues.is_empty() {
        issues.push(why);
    }
    Some(format!(
        "<tool_use_error>InputValidationError: {name} failed due to the following {}:\n{}</tool_use_error>",
        if issues.len() == 1 { "issue" } else { "issues" },
        issues.join("\n")
    ))
}

/// What a client sends back for a call that was fine, beside another's error:
/// the eval runs nothing, so a neutral stand-in.
const STAND_IN: &str = "(no output)";

/// A translated reply judged as Claude Code would: every call runnable
/// (`Ok`), no call at all (`Err(None)`: the turn ends), or the tool results to
/// send back (`Err(Some(..))`).
fn judge(tools: &Value, content: &[Value]) -> Result<(), Option<Vec<Value>>> {
    let uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    if uses.is_empty() {
        return Err(None);
    }
    let errors: Vec<Option<String>> = uses
        .iter()
        .map(|b| tool_use_error(tools, b["name"].as_str().unwrap_or(""), &b["input"]))
        .collect();
    if errors.iter().all(Option::is_none) {
        return Ok(());
    }
    Err(Some(
        uses.iter()
            .zip(errors)
            .map(|(b, e)| match e {
                Some(e) => json!({ "type": "tool_result", "tool_use_id": b["id"], "content": e, "is_error": true }),
                None => json!({ "type": "tool_result", "tool_use_id": b["id"], "content": STAND_IN }),
            })
            .collect(),
    ))
}

fn shown_calls(content: &[Value]) -> Vec<String> {
    content
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| {
            clip(
                &format!("{}({})", b["name"].as_str().unwrap_or(""), b["input"]),
                200,
            )
        })
        .collect()
}

fn reply_text(content: &[Value]) -> String {
    clip(
        &content
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(" "),
        200,
    )
}

/// How a trial recovered.
struct Recovery {
    repaired: bool,
    valid_after: Option<usize>,
    failed_as: Option<String>,
    retries: Vec<Retry>,
}

/// The rest of the turn as Claude Code would have it: the first reply through
/// the translator and, while a call is still invalid, its errors sent back and
/// the model asked again, up to `retries` times. `messages` ends with the task.
async fn recover(
    server: &Server,
    tools: &Value,
    system: &str,
    mut messages: Vec<Value>,
    first: &Reply,
    retries: usize,
) -> Recovery {
    let mut content = translate(tools, &server.model, &first.chunks);
    let mut log = Vec::new();
    let done = |valid_after, failed_as: Option<&str>, log| Recovery {
        repaired: valid_after == Some(0),
        valid_after,
        failed_as: failed_as.map(str::to_string),
        retries: log,
    };
    for k in 0..=retries {
        let results = match judge(tools, &content) {
            Ok(()) => return done(Some(k), None, log),
            Err(None) => return done(None, Some("no call"), log),
            Err(Some(results)) => results,
        };
        if k == retries {
            break;
        }
        let sent = results
            .iter()
            .map(|r| r["content"].as_str().unwrap_or("").to_string())
            .collect();
        messages.push(json!({ "role": "assistant", "content": content }));
        messages.push(json!({ "role": "user", "content": results }));
        let (_, body) = request(server, system, &messages);
        let t = Instant::now();
        match send(server, &body).await {
            Ok(reply) => {
                content = translate(tools, &server.model, &reply.chunks);
                let valid = judge(tools, &content).is_ok();
                log.push(Retry {
                    sent,
                    calls: shown_calls(&content),
                    valid,
                    detail: reply_text(&content),
                    seconds: t.elapsed().as_secs_f64(),
                });
            }
            Err(e) => {
                log.push(Retry {
                    sent,
                    calls: vec![],
                    valid: false,
                    detail: e.clone(),
                    seconds: t.elapsed().as_secs_f64(),
                });
                return done(None, Some("request failed"), log);
            }
        }
    }
    done(None, Some("still invalid"), log)
}

/// One generated session at `target` tokens, and the last turns to ask of it.
struct Session {
    system: String,
    messages: Vec<Value>,
    tasks: Vec<(&'static Task, String)>,
    /// How long the server took to read it all, from an empty cache (with one token of answer).
    prefill_s: Option<f64>,
}

async fn session(
    server: &Server,
    seed: u64,
    target: u64,
    tasks: usize,
    ratio: &std::sync::Mutex<f64>,
) -> Session {
    let p = project(seed);
    let system = system_prompt(&p);
    let mut r = Rng::new(seed.wrapping_mul(31).wrapping_add(target));
    let mut order: Vec<usize> = (0..TASKS.len()).collect();
    for i in (1..order.len()).rev() {
        let j = r.below(i + 1);
        order.swap(i, j);
    }
    let tasks: Vec<(&Task, String)> = order
        .iter()
        .cycle()
        .take(tasks)
        .map(|&i| (&TASKS[i], (TASKS[i].text)(&p, &mut r)))
        .collect();
    // Grow the session until the prompt (with the longest last turn) is about the target.
    let longest = tasks.iter().map(|(_, t)| t.len()).max().unwrap_or(0);
    let mut chars = (target as f64 * *ratio.lock().expect("ratio")) as usize;
    let mut messages = Vec::new();
    let mut prefill_s = None;
    // A longer or shorter session from the same seed starts the same way, so after
    // the first count the server reads only what changed at the end.
    for _ in 0..6 {
        messages = history(&p, seed, chars);
        let mut probe = messages.clone();
        probe.push(json!({ "role": "user", "content": "x".repeat(longest) }));
        let (_, body) = request(server, &system, &probe);
        let t = Instant::now();
        let Some(n) = server.count(&body).await else {
            // Most likely past the model's context (the first guess at the size overshot): try smaller.
            chars = chars * 17 / 20;
            continue;
        };
        prefill_s = prefill_s.or(Some(t.elapsed().as_secs_f64()));
        let size = chars as f64 / n as f64;
        *ratio.lock().expect("ratio") = size;
        if (n as f64 - target as f64).abs() / (target as f64) < 0.02 {
            break;
        }
        // The fixed part (system prompt, tools) doesn't scale: correct by the difference.
        chars =
            (chars as i64 + ((target as i64 - n as i64) as f64 * size) as i64).max(1000) as usize;
    }
    Session {
        system,
        messages,
        tasks,
        prefill_s,
    }
}

/// Runs the trials for every size in `opts` that fits the model's context.
/// `progress` hears each size as it starts; `stop` is asked between requests.
pub async fn run(
    server: &Server,
    opts: &Options,
    progress: &(dyn Fn(String) + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(Report, Vec<Trial>), String> {
    let tools = tools();
    let ratio = std::sync::Mutex::new(3.6_f64);
    let mut trials = Vec::new();
    let mut rows = Vec::new();
    // Room for the reply: a size that leaves under 4K tokens for it isn't a session Code would have.
    let lengths: Vec<u64> = opts
        .lengths
        .iter()
        .copied()
        .filter(|l| server.context.is_none_or(|c| l + 4096 <= c))
        .collect();
    for (li, &target) in lengths.iter().enumerate() {
        if stop() {
            return Err("Stopped.".into());
        }
        progress(format!(
            "{}K-token sessions ({} of {})",
            target / 1024,
            li + 1,
            lengths.len()
        ));
        let here: Vec<Vec<Trial>> = futures_util::stream::iter(0..opts.histories)
            .map(|h| {
                let (tools, ratio) = (&tools, &ratio);
                async move {
                    let seed = opts
                        .seed
                        .wrapping_mul(1_000_003)
                        .wrapping_add(h as u64 * 7919 + target);
                    let s = session(server, seed, target, opts.tasks, ratio).await;
                    let mut out = Vec::new();
                    // One at a time within a session, each finding it in the server's prefix cache.
                    for (task, text) in &s.tasks {
                        if stop() {
                            break;
                        }
                        let mut messages = s.messages.clone();
                        messages.push(json!({ "role": "user", "content": text }));
                        let (_, body) = request(server, &s.system, &messages);
                        let t = Instant::now();
                        let trial = match send(server, &body).await {
                            Ok(reply) => {
                                let seconds = t.elapsed().as_secs_f64();
                                let (outcome, salvaged, on_task, calls, detail) =
                                    grade(tools, &reply.content, &reply.calls, task.expect);
                                let r = recover(
                                    server,
                                    tools,
                                    &s.system,
                                    messages,
                                    &reply,
                                    opts.retries,
                                )
                                .await;
                                Trial {
                                    target,
                                    history: h,
                                    task: task.key.into(),
                                    prompt_tokens: reply.prompt_tokens,
                                    completion_tokens: reply.completion_tokens,
                                    outcome,
                                    prefill_s: s.prefill_s,
                                    salvaged,
                                    on_task,
                                    calls,
                                    detail,
                                    finish_reason: reply.finish,
                                    seconds,
                                    ttft_s: reply.ttft,
                                    repaired: r.repaired,
                                    valid_after: r.valid_after,
                                    failed_as: r.failed_as,
                                    retries: r.retries,
                                }
                            }
                            Err(e) => Trial {
                                target,
                                history: h,
                                task: task.key.into(),
                                prompt_tokens: None,
                                completion_tokens: None,
                                outcome: Outcome::Error,
                                prefill_s: s.prefill_s,
                                salvaged: false,
                                on_task: false,
                                calls: vec![],
                                detail: e,
                                finish_reason: None,
                                seconds: t.elapsed().as_secs_f64(),
                                ttft_s: None,
                                repaired: false,
                                valid_after: None,
                                failed_as: None,
                                retries: vec![],
                            },
                        };
                        out.push(trial);
                    }
                    out
                }
            })
            .buffer_unordered(opts.concurrency.max(1))
            .collect()
            .await;
        let mut here: Vec<Trial> = here.into_iter().flatten().collect();
        here.sort_by_key(|t| t.history);
        rows.push(summarize(target, &here));
        trials.extend(here);
        if stop() {
            return Err("Stopped.".into());
        }
    }
    let recommended_window = recommend_window(&rows);
    let raw_window = recommend_window_raw(&rows);
    Ok((
        Report {
            model: server.model.clone(),
            context: server.context,
            histories: opts.histories,
            tasks: opts.tasks,
            seed: opts.seed,
            rows,
            recommended_window,
            rule: RULE.into(),
            retries: opts.retries,
            raw_window,
            raw_rule: RAW_RULE.into(),
        },
        trials,
    ))
}

/// The results as a Markdown table.
pub fn table(report: &Report) -> String {
    let pct = |k: usize, n: usize| {
        if n == 0 {
            "–".to_string()
        } else {
            format!("{:.0}%", 100.0 * k as f64 / n as f64)
        }
    };
    let secs = |s: Option<f64>| s.map_or("–".into(), |s| format!("{s:.1} s"));
    let mut out = String::from("| Target | Prompt tokens (median) | Trials | Valid | 95% CI | Leaked as text | Invented tool | Invalid args | No call | Salvageable leaks | Errors | Median latency (cached prefix) | Median prefill (cold) |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in &report.rows {
        out.push_str(&format!(
            "| {}K | {} | {} | {} | {:.0}–{:.0}% | {} | {} | {} | {} | {}/{} | {} | {} | {} |\n",
            r.target / 1024,
            r.prompt_tokens.map_or("–".into(), |t| t.to_string()),
            r.trials,
            pct(r.valid, r.trials),
            100.0 * r.ci_low,
            100.0 * r.ci_high,
            pct(r.leaked, r.trials),
            pct(r.invented, r.trials),
            pct(r.invalid_args, r.trials),
            pct(r.no_call, r.trials),
            r.salvaged,
            r.leaked,
            r.errors,
            secs(r.median_s),
            secs(r.median_prefill_s),
        ));
    }
    out
}

/// Recovery, as a Markdown table: valid calls as returned, after the
/// translator, and within one and two retries.
pub fn recovery_table(report: &Report) -> String {
    let pct = |k: usize, n: usize| {
        if n == 0 {
            "–".to_string()
        } else {
            format!("{:.0}% ({k}/{n})", 100.0 * k as f64 / n as f64)
        }
    };
    let mut out = String::from("| Target | Trials | First reply valid (raw) | After translator repair | Within 1 retry | 95% CI (1 retry) | Within 2 retries | Still failing | of which: no call |\n|---|---|---|---|---|---|---|---|---|\n");
    for r in &report.rows {
        out.push_str(&format!(
            "| {}K | {} | {} | {} | {} | {:.0}–{:.0}% | {} | {} | {} |\n",
            r.target / 1024,
            r.trials,
            pct(r.valid, r.trials),
            pct(r.repaired, r.trials),
            pct(r.within_1_retry, r.trials),
            100.0 * r.recovered_ci_low,
            100.0 * r.recovered_ci_high,
            pct(r.within_2_retries, r.trials),
            pct(r.still_failing, r.trials),
            r.ended_without_call,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grades_each_kind_of_reply() {
        let t = tools();
        let call = |n: &str, a: &str| vec![(n.to_string(), a.to_string())];
        assert_eq!(
            grade(
                &t,
                "",
                &call(
                    "Read",
                    r#"{"file_path":"/work/src/a.rs","offset":10,"limit":40}"#
                ),
                &["Read"]
            )
            .0,
            Outcome::Valid
        );
        assert!(
            grade(
                &t,
                "",
                &call("Bash", r#"{"command":"cargo test"}"#),
                &["Bash"]
            )
            .2,
            "on task"
        );
        let leaked = "Let me look.\n\nassistantcommentary to=functions.Read json{\"file_path\":\"/work/src/a.rs\",\"offset\":1,\"limit\":20}";
        let (o, salvaged, ..) = grade(&t, leaked, &[], &["Read"]);
        assert_eq!((o, salvaged), (Outcome::Leaked, true));
        let (o, salvaged, ..) = grade(
            &t,
            "assistantcommentary to=functions.Read json{\"file_path\":\"a\",\"line_start\":1}",
            &[],
            &["Read"],
        );
        assert_eq!(
            (o, salvaged),
            (Outcome::Leaked, false),
            "leaked with an invented parameter can't be salvaged as valid"
        );
        assert_eq!(
            grade(&t, "", &call("Search", r#"{"query":"x"}"#), &[]).0,
            Outcome::Invented
        );
        assert_eq!(
            grade(
                &t,
                "",
                &call("Read<|channel|>commentary", r#"{"file_path":"a"}"#),
                &[]
            )
            .0,
            Outcome::Invented
        );
        assert_eq!(
            grade(
                &t,
                "",
                &call("Read", r#"{"file_path":"a","line_start":1,"line_end":9}"#),
                &[]
            )
            .0,
            Outcome::InvalidArgs
        );
        assert_eq!(
            grade(
                &t,
                "",
                &call("Read", r#"{"file_path":"a","offset":"10"}"#),
                &[]
            )
            .0,
            Outcome::InvalidArgs
        );
        assert_eq!(
            grade(
                &t,
                "",
                &call("Edit", r#"{"file_path":"a","new_string":"b"}"#),
                &[]
            )
            .0,
            Outcome::InvalidArgs
        );
        assert_eq!(
            grade(
                &t,
                "",
                &call("Grep", r#"{"pattern":"x","output_mode":"lines"}"#),
                &[]
            )
            .0,
            Outcome::InvalidArgs
        );
        assert_eq!(
            grade(&t, "", &call("Bash", "{\"command\": \"ls"), &[]).0,
            Outcome::InvalidArgs
        );
        assert_eq!(
            grade(&t, "Sure, here's what I'd do.", &[], &["Bash"]).0,
            Outcome::NoCall
        );
        // Two calls: valid only if both are.
        let two = vec![
            ("Read".to_string(), r#"{"file_path":"a"}"#.to_string()),
            ("Bash".to_string(), "{}".to_string()),
        ];
        assert_eq!(grade(&t, "", &two, &[]).0, Outcome::InvalidArgs);
    }

    #[test]
    fn recommends_the_largest_size_that_holds_up_from_the_bottom() {
        let row = |target, valid, trials| Row {
            target,
            within_1_retry: valid,
            trials,
            recovered_rate: valid as f64 / trials as f64,
            ..Default::default()
        };
        assert_eq!(
            recommend_window(&[
                row(8192, 20, 20),
                row(16384, 20, 20),
                row(32768, 19, 20),
                row(65536, 18, 20),
                row(98304, 20, 20)
            ]),
            Some(32768),
            "a later lucky size doesn't count past a failing one"
        );
        assert_eq!(recommend_window(&[row(8192, 18, 20)]), None);
        assert_eq!(
            recommend_window(&[row(8192, 10, 10)]),
            None,
            "too few trials"
        );
        assert_eq!(
            recommend_window(&[row(65536, 40, 40), row(8192, 40, 40)]),
            Some(65536),
            "order doesn't matter"
        );
    }

    #[test]
    fn the_raw_and_recovered_windows_are_judged_apart() {
        // First replies at 86%, all but one fine within a retry.
        let row = |target| Row {
            target,
            trials: 50,
            valid: 43,
            valid_rate: 0.86,
            within_1_retry: 49,
            recovered_rate: 0.98,
            ..Default::default()
        };
        let rows = [row(8192), row(16384)];
        assert_eq!(recommend_window(&rows), Some(16384));
        assert_eq!(recommend_window_raw(&rows), None);
    }

    fn trial(outcome: Outcome, valid_after: Option<usize>, failed_as: Option<&str>) -> Trial {
        Trial {
            target: 8192,
            history: 0,
            task: "t".into(),
            prompt_tokens: Some(8000),
            completion_tokens: None,
            outcome,
            prefill_s: None,
            salvaged: false,
            on_task: false,
            calls: vec![],
            detail: String::new(),
            finish_reason: None,
            seconds: 1.0,
            ttft_s: None,
            repaired: valid_after == Some(0),
            valid_after,
            failed_as: failed_as.map(str::to_string),
            retries: vec![],
        }
    }

    #[test]
    fn summarizes_recovery_cumulatively() {
        let t = [
            trial(Outcome::Valid, Some(0), None),
            trial(Outcome::Invented, Some(0), None),
            trial(Outcome::InvalidArgs, Some(1), None),
            trial(Outcome::InvalidArgs, Some(2), None),
            trial(Outcome::InvalidArgs, None, Some("still invalid")),
            trial(Outcome::NoCall, None, Some("no call")),
            trial(Outcome::Error, None, None),
        ];
        let r = summarize(8192, &t);
        assert_eq!(
            (
                r.trials,
                r.valid,
                r.repaired,
                r.within_1_retry,
                r.within_2_retries,
                r.still_failing,
                r.ended_without_call
            ),
            (6, 1, 2, 3, 4, 2, 1)
        );
        assert!((r.recovered_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn errors_are_worded_as_claude_code_words_them() {
        let t = tools();
        assert_eq!(
            tool_use_error(&t, "Read", &json!({ "file_path": "a", "offset": 1 })),
            None
        );
        assert_eq!(tool_use_error(&t, "Read<|channel|>commentary", &json!({ "file_path": "a" })).unwrap(), "<tool_use_error>Error: No such tool available: Read<|channel|>commentary</tool_use_error>");
        assert_eq!(tool_use_error(&t, "Edit", &json!({ "file_path": "a", "new_string": "b" })).unwrap(), "<tool_use_error>InputValidationError: Edit failed due to the following issue:\nThe required parameter `old_string` is missing</tool_use_error>");
        assert_eq!(
            tool_use_error(&t, "Read", &json!({ "file_path": "a", "line_start": 1, "line_end": 9 })).unwrap(),
            "<tool_use_error>InputValidationError: Read failed due to the following issues:\nAn unexpected parameter `line_end` was provided\nAn unexpected parameter `line_start` was provided</tool_use_error>"
        );
        // Arguments that weren't JSON reach Claude Code as the translator passes them on.
        assert_eq!(
            tool_use_error(&t, "Bash", &json!({ "_unparsed_arguments": "{\"command\": \"ls" })).unwrap(),
            "<tool_use_error>InputValidationError: Bash failed due to the following issues:\nThe required parameter `command` is missing\nAn unexpected parameter `_unparsed_arguments` was provided</tool_use_error>"
        );
        assert!(
            tool_use_error(&t, "Read", &json!({ "file_path": "a", "offset": "ten" }))
                .unwrap()
                .contains(
                    "The parameter `offset` type is expected as `number` but provided as `string`"
                )
        );
        assert!(tool_use_error(
            &t,
            "Grep",
            &json!({ "pattern": "x", "output_mode": "lines" })
        )
        .unwrap()
        .contains("`output_mode` must be one of"));
        // An error exactly when the grader calls it invalid.
        for (n, a) in [
            ("Read", json!({})),
            ("Glob", json!({ "pattern": "*", "depth": 2 })),
            ("Bash", json!({ "command": "ls", "timeout": 5 })),
            ("Write", json!(null)),
        ] {
            assert_eq!(
                tool_use_error(&t, n, &a).is_some(),
                check_call(&t, n, &a).is_err(),
                "{n} {a}"
            );
        }
    }

    /// A streamed reply, as vLLM sends it: text, then calls, then the finish.
    fn chunks(text: &str, calls: &[(&str, &str)]) -> Vec<Value> {
        let mut out = vec![
            json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" } }] }),
        ];
        if !text.is_empty() {
            out.push(json!({ "choices": [{ "index": 0, "delta": { "content": text } }] }));
        }
        for (i, (n, a)) in calls.iter().enumerate() {
            out.push(json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": i, "id": format!("call_{i}"), "type": "function", "function": { "name": n, "arguments": "" } }] } }] }));
            out.push(json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": i, "function": { "arguments": a } }] } }] }));
        }
        out.push(json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": if calls.is_empty() { "stop" } else { "tool_calls" } }] }));
        out
    }

    #[test]
    fn the_translator_mends_what_it_safely_can_and_no_more() {
        let t = tools();
        let model = "eval-test/translate";
        let one = |text: &str, calls: &[(&str, &str)]| translate(&t, model, &chunks(text, calls));
        // Harmony residue on the name, a number written as a string: mended.
        let c = one(
            "",
            &[(
                "Read<|channel|>commentary",
                r#"{"file_path":"/work/a.rs","offset":"10","limit":40}"#,
            )],
        );
        assert_eq!(c.len(), 1);
        assert_eq!(
            (
                c[0]["type"].as_str(),
                c[0]["name"].as_str(),
                &c[0]["input"]["offset"]
            ),
            (Some("tool_use"), Some("Read"), &json!(10))
        );
        assert_eq!(judge(&t, &c), Ok(()));
        // A call leaked into the text: salvaged into a call, the text before it kept.
        let c = one("Let me look.\n\nassistantcommentary to=functions.Read json{\"file_path\":\"/work/a.rs\"}", &[]);
        assert_eq!(
            c.iter()
                .map(|b| b["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["text", "tool_use"]
        );
        assert_eq!(c[0]["text"].as_str().map(str::trim), Some("Let me look."));
        assert_eq!(judge(&t, &c), Ok(()));
        // An invented parameter: left alone, so Claude Code's error goes back.
        let c = one(
            "",
            &[("Read", r#"{"file_path":"a","line_start":1,"line_end":9}"#)],
        );
        let Err(Some(results)) = judge(&t, &c) else {
            panic!("should be sent back")
        };
        assert_eq!(
            (
                results.len(),
                &results[0]["is_error"],
                &results[0]["tool_use_id"]
            ),
            (1, &json!(true), &json!("call_0"))
        );
        assert!(results[0]["content"]
            .as_str()
            .unwrap()
            .contains("unexpected parameter `line_end`"));
        // Text alone ends the turn: nothing to retry.
        assert_eq!(judge(&t, &one("All done.", &[])), Err(None));
        // Two calls, one fine: it runs, the other gets its error.
        let c = one(
            "",
            &[
                ("Bash", r#"{"command":"ls"}"#),
                ("Search", r#"{"query":"x"}"#),
            ],
        );
        let Err(Some(results)) = judge(&t, &c) else {
            panic!("should be sent back")
        };
        assert_eq!(results[0]["content"], STAND_IN);
        assert!(results[0].get("is_error").is_none());
        assert_eq!(
            results[1]["content"],
            "<tool_use_error>Error: No such tool available: Search</tool_use_error>"
        );
        // None of it counts as a client's call.
        assert!(crate::anthropic::tool_call_health()
            .iter()
            .all(|h| h["model"] != model));
    }

    #[test]
    fn wilson_interval_is_sane() {
        let (lo, hi) = wilson(20, 20);
        assert!(lo > 0.83 && lo < 0.84 && hi == 1.0);
        let (lo, hi) = wilson(10, 20);
        assert!((lo - 0.299).abs() < 0.01 && (hi - 0.701).abs() < 0.01);
    }

    #[test]
    fn sessions_are_reproducible_sized_and_well_formed() {
        for seed in [1, 2, 3, 4] {
            let p = project(seed);
            assert!(p.files.len() >= 10 && p.files.iter().all(|f| !f.fns.is_empty()));
            let a = history(&p, seed, 60_000);
            let b = history(&p, seed, 60_000);
            assert_eq!(a, b);
            let chars: usize = a.iter().map(|m| m.to_string().len()).sum();
            assert!((60_000..120_000).contains(&chars), "{chars}");
            // Every call it made in the past was a valid one, and has its result.
            let t = tools();
            for (i, m) in a.iter().enumerate() {
                for b in m["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == "tool_use")
                {
                    check_call(&t, b["name"].as_str().unwrap(), &b["input"]).unwrap();
                    assert_eq!(a[i + 1]["content"][0]["tool_use_id"], b["id"]);
                }
            }
            assert_eq!(a.last().unwrap()["role"], "assistant");
        }
    }

    #[test]
    fn asks_the_way_the_translator_does() {
        let server = Server {
            http: reqwest::Client::new(),
            base_url: "http://localhost:8003/v1".into(),
            model: "openai/gpt-oss-20b".into(),
            context: Some(131_072),
        };
        let p = project(5);
        let mut m = history(&p, 5, 20_000);
        m.push(json!({ "role": "user", "content": "Run the tests." }));
        let (_, body) = request(&server, &system_prompt(&p), &m);
        assert_eq!(body["model"], "openai/gpt-oss-20b");
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert!(
            body.get("temperature").is_none(),
            "Claude Code sends none: the server's own sampling"
        );
        assert_eq!(body["tools"].as_array().unwrap().len(), 6);
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool"));
        assert_eq!(body["max_tokens"], 16_384);
    }
}
