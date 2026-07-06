// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The `tt_toplike.inference` telemetry enrichment: scrape a vLLM
//! Prometheus `/metrics` endpoint and fold it into the wire shape
//! `tt-toplike --remote`'s `[i]` view deserializes.
//!
//! Mirrors the metric names and rate/average math of tt-toplike's own
//! reference parser (`tt-toplike/src/workload/inference_server/metrics.rs`)
//! byte-for-byte, so a QuietBox watching itself locally (that reference
//! file) and a laptop watching it remotely (this module, over
//! `GET /telemetry`) compute identical numbers from the identical scrape.
//!
//! Three layers, same split as `procscan.rs`:
//!   - **Wire types** (`InferenceInfo`, `ServingInfo`, `Phase`) -- the
//!     `Serialize` shapes `tt-toplike` decodes. Field names/types and the
//!     `Phase` strings are load-bearing; see the module-level brief this was
//!     built from.
//!   - **Pure helpers** (`parse_vllm_metrics`, `extract_model_name`,
//!     `ServingInfo::fold`, `build_inference`) -- take already-fetched text
//!     or already-parsed counters, so they're unit-testable with canned
//!     `/metrics` bodies and no real HTTP.
//!   - **`InferenceSampler`** -- the stateful seam owned once per
//!     `/telemetry` connection (mirrors `procscan::ProcessSampler`), holding
//!     the previous tick's counters + timestamp so rates/deltas are
//!     computed from real elapsed wall-time rather than an assumed cadence.

use std::time::Instant;

use libttstation::model::ServingStatus;
use serde::Serialize;

/// Raw cumulative + gauge values from one `/metrics` scrape. Mirrors
/// tt-toplike's `VllmCounters` field-for-field -- see this module's doc
/// comment for why that parity matters.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct VllmCounters {
    pub generation_tokens_total: u64,
    pub prompt_tokens_total: u64,
    pub requests_succeeded_total: u64, // stop + length
    pub requests_errored_total: u64,   // error + abort
    pub requests_running: u32,
    pub requests_waiting: u32,
    pub kv_cache_usage: f32, // 0..1
    pub ttft_sum: f64,
    pub ttft_count: u64,
    pub queue_time_sum: f64,
    pub queue_time_count: u64,
    pub prefill_time_sum: f64,
    pub prefill_time_count: u64,
    pub decode_time_sum: f64,
    pub decode_time_count: u64,
    pub tpot_sum: f64,
    pub tpot_count: u64,
    pub prefix_queries_total: u64,
    pub prefix_hits_total: u64,
    pub preemptions_total: u64,
}

/// The numeric value at the end of a Prometheus sample line (after the last
/// space). vLLM formats integers as floats (`826.0`), so parse as f64.
fn line_value(line: &str) -> Option<f64> {
    line.rsplit(' ').next()?.trim().parse::<f64>().ok()
}

/// True if `line`'s metric name (before any `{labels}` or space) equals `name`.
fn is_metric(line: &str, name: &str) -> bool {
    let head = line.split(['{', ' ']).next().unwrap_or("");
    head == name
}

/// Parse the vLLM counters we render. `None` if the text carries no `vllm:`
/// metric lines at all (e.g. a non-vLLM server, or an unreachable/error body).
///
/// Deliberately identical to tt-toplike's `parse_vllm_metrics`: this doesn't
/// sum multiple `engine=`/`model_name=` labelled lines for the same metric
/// (later lines win, same as the reference), since the reference parser --
/// this module's mirror target -- doesn't either.
pub fn parse_vllm_metrics(text: &str) -> Option<VllmCounters> {
    let mut c = VllmCounters::default();
    let mut saw_vllm = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.starts_with("vllm:") {
            continue;
        }
        saw_vllm = true;
        let Some(v) = line_value(line) else { continue };
        if is_metric(line, "vllm:generation_tokens_total") {
            c.generation_tokens_total = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:prompt_tokens_total") {
            c.prompt_tokens_total = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:num_requests_running") {
            c.requests_running = v.max(0.0) as u32;
        } else if is_metric(line, "vllm:num_requests_waiting") {
            c.requests_waiting = v.max(0.0) as u32;
        } else if is_metric(line, "vllm:kv_cache_usage_perc") {
            c.kv_cache_usage = (v as f32).clamp(0.0, 1.0);
        } else if is_metric(line, "vllm:time_to_first_token_seconds_sum") {
            c.ttft_sum = v.max(0.0);
        } else if is_metric(line, "vllm:time_to_first_token_seconds_count") {
            c.ttft_count = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:request_queue_time_seconds_sum") {
            c.queue_time_sum = v.max(0.0);
        } else if is_metric(line, "vllm:request_queue_time_seconds_count") {
            c.queue_time_count = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:request_prefill_time_seconds_sum") {
            c.prefill_time_sum = v.max(0.0);
        } else if is_metric(line, "vllm:request_prefill_time_seconds_count") {
            c.prefill_time_count = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:request_decode_time_seconds_sum") {
            c.decode_time_sum = v.max(0.0);
        } else if is_metric(line, "vllm:request_decode_time_seconds_count") {
            c.decode_time_count = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:time_per_output_token_seconds_sum") {
            c.tpot_sum = v.max(0.0);
        } else if is_metric(line, "vllm:time_per_output_token_seconds_count") {
            c.tpot_count = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:prefix_cache_queries_total") {
            c.prefix_queries_total = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:prefix_cache_hits_total") {
            c.prefix_hits_total = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:num_preemptions_total") {
            c.preemptions_total = v.max(0.0) as u64;
        } else if is_metric(line, "vllm:request_success_total") {
            // Sum the labelled variants by finished_reason.
            let n = v.max(0.0) as u64;
            if line.contains("finished_reason=\"error\"")
                || line.contains("finished_reason=\"abort\"")
            {
                c.requests_errored_total += n;
            } else if line.contains("finished_reason=") {
                c.requests_succeeded_total += n; // stop, length, others
            }
        }
    }
    saw_vllm.then_some(c)
}

/// Pull the first `model_name="..."` label value out of a raw `/metrics`
/// body, for the one case that needs it: the box is `Idle` in agentd's own
/// bookkeeping but a scrape succeeds anyway (a model started out-of-band --
/// see `build_inference`), so there's no `ServingStatus::Serving(model)` to
/// borrow a name from. `None` when no line carries the label (unlabelled
/// vLLM builds, or a non-vLLM body).
pub fn extract_model_name(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(start) = line.find("model_name=\"") {
            let rest = &line[start + "model_name=\"".len()..];
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Display-ready serving stats folded from the previous tick's counters.
/// This is the `serving` object on the wire -- field names/types here are
/// load-bearing (see this module's doc comment).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ServingInfo {
    pub generation_tps: f32,
    pub prompt_tps: f32,
    pub requests_running: u32,
    pub requests_waiting: u32,
    pub kv_cache_usage: f32,
    pub ttft_avg_s: f32,
    pub queue_avg_s: f32,
    pub prefill_avg_s: f32,
    pub decode_avg_s: f32,
    pub tpot_avg_s: f32,
    pub completed_delta: u32,
    pub errored_delta: u32,
    pub prefix_hit_rate: f32,
    pub preemptions_delta: u32,
}

impl ServingInfo {
    /// Fold `cur` against the previous tick's counters over `elapsed_secs`
    /// (real measured wall-time between scrapes, from `InferenceSampler` --
    /// NOT a nominal cadence, since a WebSocket tick can be delayed by a slow
    /// client or a missed tick). A counter reset (`cur` < `prev`, e.g. the
    /// server restarted) clamps that rate/delta to 0. Without `prev`, rates
    /// and deltas are 0 but the instantaneous gauges still populate --
    /// exactly the "first tick has no previous" contract.
    ///
    /// Rate/average math mirrors tt-toplike's `ServingStats::fold`
    /// verbatim (see this module's doc comment for why parity matters),
    /// including `prefix_hit_rate` being the CUMULATIVE hits/queries ratio
    /// rather than a windowed delta -- the reference implementation this
    /// mirrors computes it that way (a per-tick delta would be noisy/`0/0`
    /// on ticks with no new prefix lookups).
    pub fn fold(prev: Option<&VllmCounters>, cur: &VllmCounters, elapsed_secs: f32) -> ServingInfo {
        // Guard against a near-zero (or first-tick, where this is unused
        // anyway) elapsed time producing a divide-by-near-zero spike.
        let secs = elapsed_secs.max(0.001);
        let rate = |c: u64, p: u64| -> f32 { c.saturating_sub(p) as f32 / secs };
        let delta = |c: u64, p: u64| -> u32 { c.saturating_sub(p) as u32 };
        let (gen_tps, prompt_tps, completed, errored) = match prev {
            Some(p) => (
                rate(cur.generation_tokens_total, p.generation_tokens_total),
                rate(cur.prompt_tokens_total, p.prompt_tokens_total),
                delta(cur.requests_succeeded_total, p.requests_succeeded_total),
                delta(cur.requests_errored_total, p.requests_errored_total),
            ),
            None => (0.0, 0.0, 0, 0),
        };
        // Windowed latency average: mean over just this tick's completed
        // requests (cur − prev), so a slow request actually moves the number
        // instead of being drowned by the lifetime mean of every request
        // since server start. Falls back to the lifetime mean when nothing
        // completed this window (idle) or on the first tick, so the display
        // holds the last steady value rather than dropping to 0.
        let wavg = |cur_sum: f64, cur_count: u64, prev_sum: f64, prev_count: u64| -> f32 {
            let d_count = cur_count.saturating_sub(prev_count);
            if d_count > 0 {
                ((cur_sum - prev_sum).max(0.0) / d_count as f64) as f32
            } else if cur_count > 0 {
                (cur_sum / cur_count as f64) as f32
            } else {
                0.0
            }
        };
        let avg = |cur_sum: f64,
                   cur_count: u64,
                   ps: fn(&VllmCounters) -> f64,
                   pc: fn(&VllmCounters) -> u64|
         -> f32 {
            wavg(cur_sum, cur_count, prev.map_or(0.0, ps), prev.map_or(0, pc))
        };
        let prefix_hit_rate = if cur.prefix_queries_total > 0 {
            (cur.prefix_hits_total as f64 / cur.prefix_queries_total as f64) as f32
        } else {
            0.0
        };
        let preemptions_delta = match prev {
            Some(p) => cur.preemptions_total.saturating_sub(p.preemptions_total) as u32,
            None => 0,
        };
        ServingInfo {
            generation_tps: gen_tps,
            prompt_tps,
            requests_running: cur.requests_running,
            requests_waiting: cur.requests_waiting,
            kv_cache_usage: cur.kv_cache_usage,
            ttft_avg_s: avg(
                cur.ttft_sum,
                cur.ttft_count,
                |c| c.ttft_sum,
                |c| c.ttft_count,
            ),
            queue_avg_s: avg(
                cur.queue_time_sum,
                cur.queue_time_count,
                |c| c.queue_time_sum,
                |c| c.queue_time_count,
            ),
            prefill_avg_s: avg(
                cur.prefill_time_sum,
                cur.prefill_time_count,
                |c| c.prefill_time_sum,
                |c| c.prefill_time_count,
            ),
            decode_avg_s: avg(
                cur.decode_time_sum,
                cur.decode_time_count,
                |c| c.decode_time_sum,
                |c| c.decode_time_count,
            ),
            tpot_avg_s: avg(
                cur.tpot_sum,
                cur.tpot_count,
                |c| c.tpot_sum,
                |c| c.tpot_count,
            ),
            completed_delta: completed,
            errored_delta: errored,
            prefix_hit_rate,
            preemptions_delta,
        }
    }
}

/// The `phase` enum on the wire. `#[serde(rename_all = "lowercase")]` yields
/// exactly `down`/`compiling`/`loading`/`ready`/`alarm` -- the byte-significant
/// strings `tt-toplike --remote` matches on. `Compiling`/`Down`/`Alarm` are
/// never produced by this module today (agentd has no signal for them yet --
/// see the module brief) but are included so the wire enum is complete and
/// future agentd work (e.g. surfacing a compiling container) doesn't need a
/// wire-format change, just a new `Phase::Compiling` call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Down,
    Compiling,
    Loading,
    Ready,
    Alarm,
}

/// One entry of the `tt_toplike.inference` array -- the wire shape
/// `tt-toplike --remote`'s `RemoteInference` decodes. Field names/types are
/// load-bearing; see this module's doc comment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InferenceInfo {
    pub key: String,
    pub label: String,
    pub phase: Phase,
    /// Always emitted (as JSON `null` when unknown) -- `RemoteInference` has
    /// no serde default for this field, so omitting the key would fail its
    /// decode. See the module brief's "None-vs-empty contract."
    pub progress: Option<f32>,
    /// Always emitted (as JSON `null` when absent) -- same reasoning as
    /// `progress`.
    pub serving: Option<ServingInfo>,
}

/// Strip an `org/` prefix off a model id for display, e.g.
/// `"meta-llama/Llama-3.1-8B-Instruct"` -> `"Llama-3.1-8B-Instruct"`. A model
/// id with no `/` (unusual, but not impossible for a locally-named model) is
/// returned unchanged.
fn display_label(model: &str) -> String {
    model.rsplit('/').next().unwrap_or(model).to_string()
}

/// Decide this tick's `InferenceInfo` (or `None`, meaning "agentd has no
/// authoritative opinion -- fall back to your local probe") from agentd's
/// in-memory `status`, whether the `/metrics` scrape produced vLLM counters,
/// and (only used when `status` is `Idle`) the `model_name` label pulled
/// from the scrape body.
///
/// Pure: takes already-parsed inputs, so it's unit-testable without any real
/// HTTP or a real `ServingBackend`. See the module brief's phase table --
/// this is a direct transcription of it:
///   - scrape succeeded (`parsed` is `Some`) -> `ready`, `serving: Some`,
///     regardless of `status` (a scrape success is authoritative even when
///     agentd's own bookkeeping says `Idle` -- a model started out-of-band).
///   - `status` is `Serving(model)` but the scrape failed -> `loading`
///     (the server process exists per agentd's own control routes, but
///     isn't answering `/metrics` yet -- still coming up).
///   - `status` is `Idle` and the scrape failed -> `None` (omit the key
///     entirely so the consumer falls back to its own local probe).
pub fn build_inference(
    status: &ServingStatus,
    parsed: Option<VllmCounters>,
    model_name_hint: Option<String>,
    prev: Option<&VllmCounters>,
    elapsed_secs: f32,
) -> Option<InferenceInfo> {
    match (status, parsed) {
        (_, Some(counters)) => {
            let model = match status {
                ServingStatus::Serving(model) => model.clone(),
                // Idle-but-scraping: the metrics body's own model_name label
                // is the only source of truth for what's being served.
                // Falling back to a literal "unknown" (rather than e.g.
                // silently dropping the entry) keeps the wire contract's
                // "Some means agentd knows a workload's state" honest even
                // when the vLLM build doesn't label its metrics.
                ServingStatus::Idle => model_name_hint.unwrap_or_else(|| "unknown".to_string()),
            };
            Some(InferenceInfo {
                label: display_label(&model),
                key: model,
                phase: Phase::Ready,
                progress: None,
                serving: Some(ServingInfo::fold(prev, &counters, elapsed_secs)),
            })
        }
        (ServingStatus::Serving(model), None) => Some(InferenceInfo {
            label: display_label(model),
            key: model.clone(),
            phase: Phase::Loading,
            progress: None,
            serving: None,
        }),
        (ServingStatus::Idle, None) => None,
    }
}

/// Stateful per-connection sampler: owns the previous tick's counters + the
/// `Instant` they were captured at, so `ServingInfo::fold`'s rates are
/// computed from real elapsed wall-time. Mirrors `procscan::ProcessSampler`'s
/// shape (a small struct created once per `GET /telemetry` connection and
/// `tick`-ed every push).
pub struct InferenceSampler {
    prev: Option<VllmCounters>,
    prev_at: Option<Instant>,
}

impl InferenceSampler {
    pub fn new() -> Self {
        Self {
            prev: None,
            prev_at: None,
        }
    }

    /// One tick. `scrape_body` is `Some(text)` when the `/metrics` HTTP GET
    /// returned 200 (whatever the body -- `parse_vllm_metrics` decides if
    /// it's usable), or `None` when the GET itself failed (connection
    /// refused, timeout, non-200 status -- see `routes.rs`'s scrape call
    /// site). Returns the `Option<InferenceInfo>` to set on this tick's
    /// `TtToplike`, and updates the held previous-counters state for the
    /// NEXT tick's rate math (only when this tick actually produced fresh
    /// counters -- a failed/non-vLLM scrape leaves the held state alone, so
    /// a single blip doesn't reset the rate baseline).
    pub fn tick(
        &mut self,
        status: &ServingStatus,
        scrape_body: Option<&str>,
    ) -> Option<InferenceInfo> {
        let now = Instant::now();
        let parsed = scrape_body.and_then(parse_vllm_metrics);
        let model_hint = scrape_body.and_then(extract_model_name);
        let elapsed_secs = self
            .prev_at
            .map(|at| now.duration_since(at).as_secs_f32())
            .unwrap_or(0.0);

        let info = build_inference(status, parsed, model_hint, self.prev.as_ref(), elapsed_secs);

        if let Some(counters) = parsed {
            self.prev = Some(counters);
            self.prev_at = Some(now);
        }

        info
    }
}

impl Default for InferenceSampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from a real vLLM /metrics scrape.
    const SAMPLE: &str = "\
# HELP vllm:num_requests_running Number of requests in model execution batches.
vllm:num_requests_running{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 6.0
vllm:num_requests_waiting{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 2.0
vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 0.42
vllm:prompt_tokens_total{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 343.0
vllm:generation_tokens_total{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 826.0
vllm:request_success_total{engine=\"0\",finished_reason=\"stop\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 3.0
vllm:request_success_total{engine=\"0\",finished_reason=\"length\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 1.0
vllm:request_success_total{engine=\"0\",finished_reason=\"error\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 0.0
vllm:time_to_first_token_seconds_count{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 4.0
vllm:time_to_first_token_seconds_sum{engine=\"0\",model_name=\"meta-llama/Llama-3.1-8B-Instruct\"} 0.44
vllm:request_queue_time_seconds_sum{engine=\"0\",model_name=\"M\"} 0.10
vllm:request_queue_time_seconds_count{engine=\"0\",model_name=\"M\"} 5.0
vllm:request_prefill_time_seconds_sum{engine=\"0\",model_name=\"M\"} 0.25
vllm:request_prefill_time_seconds_count{engine=\"0\",model_name=\"M\"} 5.0
vllm:request_decode_time_seconds_sum{engine=\"0\",model_name=\"M\"} 0.15
vllm:request_decode_time_seconds_count{engine=\"0\",model_name=\"M\"} 5.0
vllm:time_per_output_token_seconds_sum{engine=\"0\",model_name=\"M\"} 0.05
vllm:time_per_output_token_seconds_count{engine=\"0\",model_name=\"M\"} 5.0
vllm:prefix_cache_queries_total{engine=\"0\",model_name=\"M\"} 200.0
vllm:prefix_cache_hits_total{engine=\"0\",model_name=\"M\"} 150.0
vllm:num_preemptions_total{engine=\"0\",model_name=\"M\"} 3.0
";

    // (a) parse canned vLLM /metrics text -> expected counters.
    #[test]
    fn parses_the_vllm_counters() {
        let c = parse_vllm_metrics(SAMPLE).expect("has vllm metrics");
        assert_eq!(c.generation_tokens_total, 826);
        assert_eq!(c.prompt_tokens_total, 343);
        assert_eq!(c.requests_succeeded_total, 4); // stop(3)+length(1)
        assert_eq!(c.requests_errored_total, 0);
        assert_eq!(c.requests_running, 6);
        assert_eq!(c.requests_waiting, 2);
        assert!((c.kv_cache_usage - 0.42).abs() < 1e-6);
        assert_eq!(c.prefix_queries_total, 200);
        assert_eq!(c.prefix_hits_total, 150);
        assert_eq!(c.preemptions_total, 3);
    }

    #[test]
    fn none_when_no_vllm_metrics() {
        assert!(parse_vllm_metrics("# nothing here\nother_metric 5\n").is_none());
        assert!(parse_vllm_metrics("").is_none());
    }

    #[test]
    fn extracts_first_model_name_label() {
        assert_eq!(
            extract_model_name(SAMPLE).as_deref(),
            Some("meta-llama/Llama-3.1-8B-Instruct")
        );
        assert_eq!(extract_model_name("vllm:x 1.0\n"), None);
    }

    // (b) two counter samples + elapsed -> expected rates/deltas/avgs,
    // including the delta==0 -> 0 guards and first-sample -> 0.
    #[test]
    fn fold_computes_rates_from_deltas_over_elapsed_time() {
        let prev = VllmCounters {
            generation_tokens_total: 826,
            prompt_tokens_total: 343,
            requests_succeeded_total: 4,
            ..Default::default()
        };
        let cur = VllmCounters {
            generation_tokens_total: 826 + 4210,
            prompt_tokens_total: 343 + 600,
            requests_succeeded_total: 6,
            requests_running: 6,
            requests_waiting: 2,
            kv_cache_usage: 0.42,
            ..Default::default()
        };
        let s = ServingInfo::fold(Some(&prev), &cur, 5.0);
        assert!(
            (s.generation_tps - 842.0).abs() < 0.5,
            "4210 gen tokens / 5s ≈ 842, got {}",
            s.generation_tps
        );
        assert!((s.prompt_tps - 120.0).abs() < 0.5);
        assert_eq!(s.completed_delta, 2); // 6-4
        assert_eq!(s.errored_delta, 0);
        assert_eq!(s.requests_running, 6);
        assert_eq!(s.requests_waiting, 2);
        assert!((s.kv_cache_usage - 0.42).abs() < 1e-6);
    }

    #[test]
    fn fold_without_prev_is_zero_rates_but_keeps_gauges() {
        let cur = VllmCounters {
            requests_running: 3,
            kv_cache_usage: 0.5,
            ttft_sum: 2.0,
            ttft_count: 4,
            ..Default::default()
        };
        let s = ServingInfo::fold(None, &cur, 5.0);
        assert_eq!(s.generation_tps, 0.0);
        assert_eq!(s.completed_delta, 0);
        assert_eq!(s.requests_running, 3);
        assert!((s.kv_cache_usage - 0.5).abs() < 1e-6);
        assert!((s.ttft_avg_s - 0.5).abs() < 1e-4); // 2.0/4, lifetime fallback
    }

    #[test]
    fn fold_delta_count_zero_guards_avg_to_lifetime_or_zero() {
        // No new samples this window (cur == prev): falls back to lifetime mean.
        let counters = VllmCounters {
            ttft_sum: 3.0,
            ttft_count: 11,
            ..Default::default()
        };
        let idle = ServingInfo::fold(Some(&counters), &counters, 5.0);
        assert!((idle.ttft_avg_s - (3.0 / 11.0) as f32).abs() < 1e-4);

        // No samples EVER (count 0 both sides): 0, not NaN/panic.
        let empty = VllmCounters::default();
        let s = ServingInfo::fold(Some(&empty), &empty, 5.0);
        assert_eq!(s.ttft_avg_s, 0.0);
        assert_eq!(s.prefix_hit_rate, 0.0); // prefix_queries_total == 0 guard
    }

    #[test]
    fn fold_clamps_counter_reset_to_zero() {
        let prev = VllmCounters {
            generation_tokens_total: 9000,
            requests_succeeded_total: 50,
            ..Default::default()
        };
        let cur = VllmCounters {
            generation_tokens_total: 10,
            requests_succeeded_total: 1,
            ..Default::default()
        };
        let s = ServingInfo::fold(Some(&prev), &cur, 5.0);
        assert_eq!(s.generation_tps, 0.0);
        assert_eq!(s.completed_delta, 0);
    }

    // (c) phase logic.
    #[test]
    fn serving_plus_scrape_is_ready_with_serving_stats() {
        let status = ServingStatus::Serving("meta-llama/Llama-3.1-8B-Instruct".to_string());
        let parsed = parse_vllm_metrics(SAMPLE);
        let info = build_inference(&status, parsed, None, None, 0.0).expect("Some");
        assert_eq!(info.phase, Phase::Ready);
        assert_eq!(info.key, "meta-llama/Llama-3.1-8B-Instruct");
        assert_eq!(info.label, "Llama-3.1-8B-Instruct");
        assert_eq!(info.progress, None);
        assert!(info.serving.is_some());
    }

    #[test]
    fn serving_plus_failed_scrape_is_loading_with_no_serving_stats() {
        let status = ServingStatus::Serving("Qwen/Qwen3-32B".to_string());
        let info = build_inference(&status, None, None, None, 0.0).expect("Some");
        assert_eq!(info.phase, Phase::Loading);
        assert_eq!(info.key, "Qwen/Qwen3-32B");
        assert_eq!(info.label, "Qwen3-32B");
        assert_eq!(info.progress, None);
        assert_eq!(info.serving, None);
    }

    #[test]
    fn idle_plus_failed_scrape_omits_the_entry() {
        let info = build_inference(&ServingStatus::Idle, None, None, None, 0.0);
        assert_eq!(info, None);
    }

    #[test]
    fn idle_plus_successful_scrape_is_still_authoritative_ready() {
        // A model started out-of-band (not via this agent's own /run): status
        // is Idle in agentd's own bookkeeping, but the live scrape wins.
        let parsed = parse_vllm_metrics(SAMPLE);
        let hint = extract_model_name(SAMPLE);
        let info = build_inference(&ServingStatus::Idle, parsed, hint, None, 0.0).expect("Some");
        assert_eq!(info.phase, Phase::Ready);
        assert_eq!(info.key, "meta-llama/Llama-3.1-8B-Instruct");
        assert!(info.serving.is_some());
    }

    #[test]
    fn idle_plus_scrape_with_no_model_name_label_falls_back_to_unknown() {
        let unlabelled = "vllm:num_requests_running 0.0\n";
        let parsed = parse_vllm_metrics(unlabelled);
        let info = build_inference(&ServingStatus::Idle, parsed, None, None, 0.0).expect("Some");
        assert_eq!(info.key, "unknown");
        assert_eq!(info.label, "unknown");
    }

    // Phase/field serialize with the exact byte-significant strings/shape.
    #[test]
    fn phase_serializes_to_lowercase_wire_strings() {
        assert_eq!(serde_json::to_string(&Phase::Down).unwrap(), "\"down\"");
        assert_eq!(
            serde_json::to_string(&Phase::Compiling).unwrap(),
            "\"compiling\""
        );
        assert_eq!(
            serde_json::to_string(&Phase::Loading).unwrap(),
            "\"loading\""
        );
        assert_eq!(serde_json::to_string(&Phase::Ready).unwrap(), "\"ready\"");
        assert_eq!(serde_json::to_string(&Phase::Alarm).unwrap(), "\"alarm\"");
    }

    #[test]
    fn inference_info_serializes_progress_and_serving_as_explicit_null() {
        let info = InferenceInfo {
            key: "m".into(),
            label: "m".into(),
            phase: Phase::Loading,
            progress: None,
            serving: None,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"progress\":null"), "{json}");
        assert!(json.contains("\"serving\":null"), "{json}");
    }

    // InferenceSampler: state carried across ticks, plus a failed scrape
    // leaving the held rate-baseline alone.
    #[test]
    fn sampler_first_tick_has_zero_rates_second_tick_has_real_rates() {
        let mut sampler = InferenceSampler::new();
        let status = ServingStatus::Serving("meta-llama/Llama-3.1-8B-Instruct".to_string());

        let first = sampler.tick(&status, Some(SAMPLE)).expect("Some");
        let serving = first.serving.expect("serving stats present");
        assert_eq!(serving.generation_tps, 0.0); // no previous sample yet
        assert_eq!(serving.requests_running, 6); // gauge still populated

        // Second scrape, more tokens generated. Rate math needs a
        // measurable elapsed duration; the sampler uses a real Instant, so
        // simulate a slightly-elapsed second tick immediately after -- the
        // rate just needs to be > 0, not an exact value (real wall-clock
        // elapsed time in a unit test is not deterministic to the ms).
        let sample2 = SAMPLE.replace("826.0", "5036.0"); // +4210 tokens
        let second = sampler.tick(&status, Some(&sample2)).expect("Some");
        let serving2 = second.serving.expect("serving stats present");
        assert!(
            serving2.generation_tps > 0.0,
            "expected a positive rate on the second tick, got {}",
            serving2.generation_tps
        );
    }

    #[test]
    fn sampler_failed_scrape_does_not_reset_held_baseline() {
        let mut sampler = InferenceSampler::new();
        let status = ServingStatus::Serving("meta-llama/Llama-3.1-8B-Instruct".to_string());

        let first = sampler.tick(&status, Some(SAMPLE)).expect("Some");
        assert_eq!(first.phase, Phase::Ready);

        // A blip: this tick's scrape fails outright.
        let blip = sampler.tick(&status, None).expect("Some");
        assert_eq!(blip.phase, Phase::Loading);
        assert_eq!(blip.serving, None);

        // Next successful scrape should still compute a rate against the
        // FIRST tick's counters (held baseline survived the blip), not
        // reset to "no previous."
        let sample2 = SAMPLE.replace("826.0", "5036.0");
        let third = sampler.tick(&status, Some(&sample2)).expect("Some");
        let serving = third.serving.expect("serving stats present");
        assert!(serving.generation_tps > 0.0);
    }
}
