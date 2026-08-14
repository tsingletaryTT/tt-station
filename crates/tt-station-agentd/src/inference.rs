// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The `tt_toplike.inference` telemetry enrichment: scrape an inference
//! server's Prometheus `/metrics` endpoint and fold it into the wire shape
//! `tt-toplike --remote`'s `[i]` view deserializes.
//!
//! Two server flavors share that one endpoint and are told apart by which
//! metric namespace the body carries (see `parse_scrape`):
//!   - **vLLM** (`vllm:*`) -- LLM serving; folds to the `serving` object.
//!   - **tt-media-inference-server** (`tt_media_server_*`) -- diffusion/video
//!     models like SkyReels, where tokens/sec is meaningless; folds to the
//!     `media` object. The two are mutually exclusive on the wire.
//!
//! Mirrors the metric names and rate/average math of tt-toplike's own
//! reference parser (`tt-toplike/src/workload/inference_server/metrics.rs`)
//! byte-for-byte, so a QuietBox watching itself locally (that reference
//! file) and a laptop watching it remotely (this module, over
//! `GET /telemetry`) compute identical numbers from the identical scrape.
//!
//! Three layers, same split as `procscan.rs`:
//!   - **Wire types** (`InferenceInfo`, `ServingInfo`, `MediaInfo`, `Phase`)
//!     -- the `Serialize` shapes `tt-toplike` decodes (`RemoteInference`,
//!     `RemoteServing`, `RemoteMedia`). Field names/types and the `Phase`
//!     strings are load-bearing; see the module-level brief this was
//!     built from.
//!   - **Pure helpers** (`parse_vllm_metrics`, `parse_media_metrics`,
//!     `parse_scrape`, `extract_model_name`, `ServingInfo::fold`,
//!     `MediaInfo::fold`, `build_inference`) -- take already-fetched text
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
            // `v.max(0.0)` first (like every sibling field): it floors at 0 AND
            // sanitizes NaN to 0.0 (f64::max returns the non-NaN operand) —
            // vLLM can emit NaN here (0/0 before any KV blocks). Then cap at 1.0.
            // Without the max(0.0), a NaN survives (f32::clamp passes NaN
            // through) and serializes to JSON `null` on this non-Option field.
            c.kv_cache_usage = (v.max(0.0) as f32).min(1.0);
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

// ── Media / diffusion server (tt-media-inference-server) ─────────────────
//
// Diffusion/video models (SkyReels, SDXL, z-image) run under
// tt-media-inference-server, which exposes a DIFFERENT Prometheus namespace --
// `tt_media_server_*` -- on the SAME `/metrics` endpoint agentd already
// scrapes. vLLM's token counters don't exist there (and tokens/sec is
// meaningless for image/video generation), so media servers get their own
// counters + parser + fold. Mirrors tt-toplike's `parse_media_metrics` /
// `MediaStats` (`src/workload/inference_server/metrics.rs`) so a box watching
// itself locally and a laptop watching it remotely compute identical numbers.

/// Raw cumulative counters + a gauge + histogram sums/counts from one
/// media-server `/metrics` scrape, summed across distinct label sets
/// (`model_type`, `device_id`) so the display sees one fleet figure.
///
/// Metric names mirror tt-toplike's, which verified them against a live
/// `tt-media-inference-server` 0.15.0 SkyReels scrape: the real signals are
/// `requests_base_total`, `jobs_in_progress`, the
/// `requests_base_duration_seconds_total` histogram, and `post_processing`.
/// The `pre_processing` / `model_inference` / `device_warmup` families aren't
/// emitted by that build but are parsed best-effort (other runners may emit
/// them) and simply stay 0.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MediaCounters {
    /// `tt_media_server_requests_base_total` -- completed generations (the
    /// duration histogram observes on completion, so this tracks finished work).
    pub requests_total: u64,
    /// `tt_media_server_requests_base_total{status="error"|"failed"|"failure"}`,
    /// if the server labels failures (0.15.0 does not, so it stays 0).
    pub errored_total: u64,
    /// `tt_media_server_jobs_in_progress` -- in-flight generations right now.
    pub jobs_in_progress: u32,
    /// `tt_media_server_requests_base_duration_seconds_total_{sum,count}` --
    /// end-to-end per-generation wall time (queue + compute + post).
    pub duration_sum: f64,
    pub duration_count: u64,
    /// `tt_media_server_post_processing_duration_seconds_{sum,count}`.
    pub post_sum: f64,
    pub post_count: u64,
    /// `tt_media_server_pre_processing_duration_seconds_{sum,count}`.
    pub pre_sum: f64,
    pub pre_count: u64,
    /// `tt_media_server_model_inference_duration_seconds_{sum,count}`.
    pub inference_sum: f64,
    pub inference_count: u64,
    /// `tt_media_server_device_warmup_duration_seconds_{sum,count}`.
    pub warmup_sum: f64,
    pub warmup_count: u64,
}

/// Parse the media-server counters we render. `None` if the text carries no
/// `tt_media_server_` metric lines at all (a vLLM or non-media server), which
/// folds to `media: None` downstream -- mirroring `parse_vllm_metrics`.
///
/// Unlike the vLLM parser (where later labelled lines win, matching its own
/// reference), this one SUMS across distinct label sets so a multi-device or
/// multi-model server reports a fleet total -- again matching its reference.
pub fn parse_media_metrics(text: &str) -> Option<MediaCounters> {
    let mut c = MediaCounters::default();
    let mut saw_media = false;
    // The live server (prometheus multiprocess mode) emits each series TWICE --
    // byte-identical `name{labels}` lines. Since we sum across label sets,
    // those duplicates would double every value (jobs_in_progress 2->4), so
    // count each distinct series (name + full label block) at most once.
    // Genuine multi-device series have different label blocks -> still summed.
    //
    // This keys on the exact `name{labels}` byte substring, so it assumes the
    // exporter emits a given series with a STABLE label order (true for the
    // prometheus client's multiprocess output). Labels re-emitted in a
    // different order would key differently and double-count -- not a concern
    // for this exporter, but noted since fleet totals rest on it.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.starts_with("tt_media_server_") {
            continue;
        }
        saw_media = true;
        let Some(v) = line_value(line) else { continue };
        // Series identity = everything up to the value: `name{labels}` when
        // labelled, else the bare metric name.
        let series_key = match line.rfind('}') {
            Some(i) => &line[..=i],
            None => line.split_whitespace().next().unwrap_or(line),
        };
        if !seen.insert(series_key) {
            continue;
        }
        // `is_metric` compares the exact metric name, so histogram `_bucket` /
        // `_created` companion lines never collide with the `_sum`/`_count`
        // or base-counter names below.
        if is_metric(line, "tt_media_server_requests_base_total") {
            let n = v.max(0.0) as u64;
            if line.contains("status=\"error\"")
                || line.contains("status=\"failed\"")
                || line.contains("status=\"failure\"")
            {
                c.errored_total += n;
            } else {
                c.requests_total += n;
            }
        } else if is_metric(line, "tt_media_server_jobs_in_progress") {
            // A gauge (not cumulative); summed across label sets for a fleet total.
            c.jobs_in_progress += v.max(0.0) as u32;
        } else if is_metric(
            line,
            "tt_media_server_requests_base_duration_seconds_total_sum",
        ) {
            c.duration_sum += v.max(0.0);
        } else if is_metric(
            line,
            "tt_media_server_requests_base_duration_seconds_total_count",
        ) {
            c.duration_count += v.max(0.0) as u64;
        } else if is_metric(line, "tt_media_server_post_processing_duration_seconds_sum") {
            c.post_sum += v.max(0.0);
        } else if is_metric(
            line,
            "tt_media_server_post_processing_duration_seconds_count",
        ) {
            c.post_count += v.max(0.0) as u64;
        } else if is_metric(line, "tt_media_server_pre_processing_duration_seconds_sum") {
            c.pre_sum += v.max(0.0);
        } else if is_metric(line, "tt_media_server_pre_processing_duration_seconds_count") {
            c.pre_count += v.max(0.0) as u64;
        } else if is_metric(line, "tt_media_server_model_inference_duration_seconds_sum") {
            c.inference_sum += v.max(0.0);
        } else if is_metric(
            line,
            "tt_media_server_model_inference_duration_seconds_count",
        ) {
            c.inference_count += v.max(0.0) as u64;
        } else if is_metric(line, "tt_media_server_device_warmup_duration_seconds_sum") {
            c.warmup_sum += v.max(0.0);
        } else if is_metric(line, "tt_media_server_device_warmup_duration_seconds_count") {
            c.warmup_count += v.max(0.0) as u64;
        }
    }
    saw_media.then_some(c)
}

/// Which flavor of inference server the one `/metrics` scrape turned out to
/// be. The two namespaces are mutually exclusive in practice (a given server
/// is either vLLM or tt-media-inference-server), and the wire shape mirrors
/// that: an `InferenceInfo` carries `serving` OR `media`, never both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrapeCounters {
    Vllm(VllmCounters),
    Media(MediaCounters),
}

/// Classify one `/metrics` body. vLLM is tried first: it's the primary path
/// agentd serves, and checking it first makes the precedence explicit rather
/// than incidental should a body ever somehow carry both namespaces.
/// `None` when the body is neither (an unreachable/error page, or a server
/// whose metrics we don't understand) -- which folds to "no authoritative
/// opinion" downstream.
pub fn parse_scrape(text: &str) -> Option<ScrapeCounters> {
    if let Some(v) = parse_vllm_metrics(text) {
        return Some(ScrapeCounters::Vllm(v));
    }
    parse_media_metrics(text).map(ScrapeCounters::Media)
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

/// Display-ready media/diffusion stats -- the `media` object on the wire, and
/// the counterpart to `ServingInfo` for servers where "tokens/sec" is
/// meaningless: throughput is generations, the live-work signal is
/// `jobs_in_progress`, and timing is the end-to-end per-generation duration
/// plus whichever pipeline stages the server exposes.
///
/// Field names/types mirror tt-toplike's `RemoteMedia` exactly (see this
/// module's doc comment). Note this carries the two CUMULATIVE totals
/// (`completed_total`/`errored_total`) alongside the per-tick deltas, because
/// the remote UI displays them directly as "N done" / "errors N" -- a remote
/// viewer can't reconstruct them from deltas it only started observing midway.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct MediaInfo {
    pub generations_per_min: f32,
    pub jobs_in_progress: u32,
    pub completed_total: u64,
    pub errored_total: u64,
    pub completed_delta: u32,
    pub errored_delta: u32,
    pub duration_avg_s: f32,
    pub post_avg_s: f32,
    pub pre_avg_s: f32,
    pub inference_avg_s: f32,
    pub warmup_avg_s: f32,
}

impl MediaInfo {
    /// Fold `cur` against the previous tick's counters over `elapsed_secs`
    /// (real measured wall-time from `InferenceSampler`, not a nominal
    /// cadence -- the same adaptation `ServingInfo::fold` makes to
    /// tt-toplike's `MediaStats::fold`, which assumes a fixed cadence).
    ///
    /// A counter reset (`cur` < `prev`, e.g. the server restarted) clamps
    /// that rate/delta to 0. Without `prev`, rates and deltas are 0 but the
    /// gauges and cumulative totals still populate.
    pub fn fold(prev: Option<&MediaCounters>, cur: &MediaCounters, elapsed_secs: f32) -> MediaInfo {
        // Guard a near-zero (or first-tick, where it's unused) elapsed time
        // from producing a divide-by-near-zero spike.
        let secs = elapsed_secs.max(0.001);
        let delta = |c: u64, p: u64| -> u32 { c.saturating_sub(p) as u32 };
        let (gen_per_min, completed, errored) = match prev {
            Some(p) => {
                let per_sec = cur.requests_total.saturating_sub(p.requests_total) as f32 / secs;
                (
                    per_sec * 60.0,
                    delta(cur.requests_total, p.requests_total),
                    delta(cur.errored_total, p.errored_total),
                )
            }
            None => (0.0, 0, 0),
        };
        // Windowed mean over just this tick's completed work (cur - prev), so
        // a slow generation moves the number instead of being drowned by the
        // lifetime mean; falls back to the lifetime mean when nothing
        // completed this window, so the display holds its last steady value
        // rather than dropping to 0. Identical rule to `ServingInfo::fold`.
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
                   ps: fn(&MediaCounters) -> f64,
                   pc: fn(&MediaCounters) -> u64|
         -> f32 { wavg(cur_sum, cur_count, prev.map_or(0.0, ps), prev.map_or(0, pc)) };
        MediaInfo {
            generations_per_min: gen_per_min.max(0.0),
            jobs_in_progress: cur.jobs_in_progress,
            completed_total: cur.requests_total,
            errored_total: cur.errored_total,
            completed_delta: completed,
            errored_delta: errored,
            duration_avg_s: avg(
                cur.duration_sum,
                cur.duration_count,
                |c| c.duration_sum,
                |c| c.duration_count,
            ),
            post_avg_s: avg(cur.post_sum, cur.post_count, |c| c.post_sum, |c| c.post_count),
            pre_avg_s: avg(cur.pre_sum, cur.pre_count, |c| c.pre_sum, |c| c.pre_count),
            inference_avg_s: avg(
                cur.inference_sum,
                cur.inference_count,
                |c| c.inference_sum,
                |c| c.inference_count,
            ),
            warmup_avg_s: avg(
                cur.warmup_sum,
                cur.warmup_count,
                |c| c.warmup_sum,
                |c| c.warmup_count,
            ),
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
    /// Live media/diffusion stats (tt-media-inference-server, e.g. SkyReels).
    /// Mutually exclusive with `serving`: a given server is one flavor or the
    /// other. Always emitted (as JSON `null` when absent) for consistency with
    /// its siblings -- tt-toplike's `RemoteMedia` field carries a serde
    /// default so either form decodes, and a tt-toplike predating the field
    /// ignores the unknown key rather than failing the whole extension.
    pub media: Option<MediaInfo>,
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
    parsed: Option<ScrapeCounters>,
    model_name_hint: Option<String>,
    prev: Option<&VllmCounters>,
    prev_media: Option<&MediaCounters>,
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
                // when the server build doesn't label its metrics.
                ServingStatus::Idle => model_name_hint.unwrap_or_else(|| "unknown".to_string()),
            };
            // Exactly one of `serving`/`media` is populated, per the flavor
            // the scrape identified.
            let (serving, media) = match counters {
                ScrapeCounters::Vllm(c) => {
                    (Some(ServingInfo::fold(prev, &c, elapsed_secs)), None)
                }
                ScrapeCounters::Media(c) => {
                    (None, Some(MediaInfo::fold(prev_media, &c, elapsed_secs)))
                }
            };
            Some(InferenceInfo {
                label: display_label(&model),
                key: model,
                phase: Phase::Ready,
                progress: None,
                serving,
                media,
            })
        }
        (ServingStatus::Serving(model), None) => Some(InferenceInfo {
            label: display_label(model),
            key: model.clone(),
            phase: Phase::Loading,
            progress: None,
            serving: None,
            media: None,
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
    /// Separate baseline for a media server. Kept independently of `prev`
    /// (rather than one enum-shaped slot) so the two flavors never
    /// cross-contaminate each other's rate math.
    prev_media: Option<MediaCounters>,
    prev_at: Option<Instant>,
}

impl InferenceSampler {
    pub fn new() -> Self {
        Self {
            prev: None,
            prev_media: None,
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
        let parsed = scrape_body.and_then(parse_scrape);
        let model_hint = scrape_body.and_then(extract_model_name);
        let elapsed_secs = self
            .prev_at
            .map(|at| now.duration_since(at).as_secs_f32())
            .unwrap_or(0.0);

        let info = build_inference(
            status,
            parsed,
            model_hint,
            self.prev.as_ref(),
            self.prev_media.as_ref(),
            elapsed_secs,
        );

        // Only the flavor we actually saw this tick updates its baseline; a
        // failed or unrecognized scrape leaves BOTH alone, so a single blip
        // doesn't reset the rate baseline.
        match parsed {
            Some(ScrapeCounters::Vllm(c)) => {
                self.prev = Some(c);
                self.prev_at = Some(now);
            }
            Some(ScrapeCounters::Media(c)) => {
                self.prev_media = Some(c);
                self.prev_at = Some(now);
            }
            None => {}
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

    // vLLM can legitimately emit `NaN` for kv_cache_usage_perc (e.g. 0/0
    // before any KV blocks are allocated). Every other numeric field is
    // sanitized via `v.max(0.0)` (which returns 0.0 for NaN); kv_cache_usage
    // must be too, or a NaN reaches the wire as JSON `null` on a non-Option
    // f32 field and breaks the consumer's decode. Assert it's sanitized to a
    // finite value in [0,1], never NaN.
    #[test]
    fn kv_cache_usage_nan_is_sanitized() {
        let sample = "vllm:kv_cache_usage_perc{engine=\"0\",model_name=\"M\"} NaN\n";
        let c = parse_vllm_metrics(sample).expect("has vllm metrics");
        assert!(c.kv_cache_usage.is_finite(), "kv_cache_usage must not be NaN");
        assert!((0.0..=1.0).contains(&c.kv_cache_usage));
    }

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
        let info = build_inference(
            &status,
            parsed.map(ScrapeCounters::Vllm),
            None,
            None,
            None,
            0.0,
        )
        .expect("Some");
        assert_eq!(info.phase, Phase::Ready);
        assert_eq!(info.key, "meta-llama/Llama-3.1-8B-Instruct");
        assert_eq!(info.label, "Llama-3.1-8B-Instruct");
        assert_eq!(info.progress, None);
        assert!(info.serving.is_some());
    }

    #[test]
    fn serving_plus_failed_scrape_is_loading_with_no_serving_stats() {
        let status = ServingStatus::Serving("Qwen/Qwen3-32B".to_string());
        let info = build_inference(&status, None, None, None, None, 0.0).expect("Some");
        assert_eq!(info.phase, Phase::Loading);
        assert_eq!(info.key, "Qwen/Qwen3-32B");
        assert_eq!(info.label, "Qwen3-32B");
        assert_eq!(info.progress, None);
        assert_eq!(info.serving, None);
    }

    #[test]
    fn idle_plus_failed_scrape_omits_the_entry() {
        let info = build_inference(&ServingStatus::Idle, None, None, None, None, 0.0);
        assert_eq!(info, None);
    }

    #[test]
    fn idle_plus_successful_scrape_is_still_authoritative_ready() {
        // A model started out-of-band (not via this agent's own /run): status
        // is Idle in agentd's own bookkeeping, but the live scrape wins.
        let parsed = parse_vllm_metrics(SAMPLE);
        let hint = extract_model_name(SAMPLE);
        let info = build_inference(
            &ServingStatus::Idle,
            parsed.map(ScrapeCounters::Vllm),
            hint,
            None,
            None,
            0.0,
        )
        .expect("Some");
        assert_eq!(info.phase, Phase::Ready);
        assert_eq!(info.key, "meta-llama/Llama-3.1-8B-Instruct");
        assert!(info.serving.is_some());
    }

    #[test]
    fn idle_plus_scrape_with_no_model_name_label_falls_back_to_unknown() {
        let unlabelled = "vllm:num_requests_running 0.0\n";
        let parsed = parse_vllm_metrics(unlabelled);
        let info = build_inference(
            &ServingStatus::Idle,
            parsed.map(ScrapeCounters::Vllm),
            None,
            None,
            None,
            0.0,
        )
        .expect("Some");
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
            media: None,
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

    // ── Media / diffusion server (tt-media-inference-server) ─────────────

    /// Shaped after a real `tt-media-inference-server` 0.15.0 SkyReels
    /// scrape: the `tt_media_server_*` namespace, and -- critically -- the
    /// prometheus multiprocess exporter emitting each series TWICE,
    /// byte-identically. A parser that naively sums would double every value.
    const MEDIA_SAMPLE: &str = "\
# HELP tt_media_server_requests_base_total Completed generations.
# TYPE tt_media_server_requests_base_total counter
tt_media_server_requests_base_total{device_id=\"0\",model_type=\"skyreels\"} 12.0
tt_media_server_requests_base_total{device_id=\"0\",model_type=\"skyreels\"} 12.0
tt_media_server_jobs_in_progress{model_type=\"skyreels\"} 2.0
tt_media_server_jobs_in_progress{model_type=\"skyreels\"} 2.0
tt_media_server_requests_base_duration_seconds_total_sum{model_type=\"skyreels\"} 240.0
tt_media_server_requests_base_duration_seconds_total_sum{model_type=\"skyreels\"} 240.0
tt_media_server_requests_base_duration_seconds_total_count{model_type=\"skyreels\"} 12.0
tt_media_server_requests_base_duration_seconds_total_count{model_type=\"skyreels\"} 12.0
tt_media_server_post_processing_duration_seconds_sum{model_type=\"skyreels\"} 6.0
tt_media_server_post_processing_duration_seconds_count{model_type=\"skyreels\"} 12.0
";

    #[test]
    fn parses_the_media_counters() {
        let c = parse_media_metrics(MEDIA_SAMPLE).expect("media metrics present");
        assert_eq!(c.requests_total, 12);
        assert_eq!(c.errored_total, 0);
        assert_eq!(c.jobs_in_progress, 2);
        assert_eq!(c.duration_sum, 240.0);
        assert_eq!(c.duration_count, 12);
        assert_eq!(c.post_sum, 6.0);
        assert_eq!(c.post_count, 12);
        // Families this build doesn't emit stay 0 (parsed best-effort).
        assert_eq!(c.pre_count, 0);
        assert_eq!(c.inference_count, 0);
        assert_eq!(c.warmup_count, 0);
    }

    #[test]
    fn media_duplicate_series_are_counted_once() {
        // The doubled lines in MEDIA_SAMPLE must not double the totals --
        // this is the whole reason the parser keys on `name{labels}`.
        let c = parse_media_metrics(MEDIA_SAMPLE).unwrap();
        assert_eq!(c.requests_total, 12, "duplicate series double-counted");
        assert_eq!(c.jobs_in_progress, 2, "duplicate gauge double-counted");
    }

    #[test]
    fn media_distinct_label_sets_are_summed() {
        // Genuinely different label blocks are separate series -> summed,
        // giving a fleet total across devices.
        let text = "\
tt_media_server_requests_base_total{device_id=\"0\",model_type=\"skyreels\"} 12.0
tt_media_server_requests_base_total{device_id=\"1\",model_type=\"skyreels\"} 5.0
tt_media_server_jobs_in_progress{device_id=\"0\"} 2.0
tt_media_server_jobs_in_progress{device_id=\"1\"} 1.0
";
        let c = parse_media_metrics(text).unwrap();
        assert_eq!(c.requests_total, 17);
        assert_eq!(c.jobs_in_progress, 3);
    }

    #[test]
    fn media_error_status_routes_to_errored_total() {
        let text = "\
tt_media_server_requests_base_total{model_type=\"skyreels\",status=\"ok\"} 12.0
tt_media_server_requests_base_total{model_type=\"skyreels\",status=\"error\"} 3.0
tt_media_server_requests_base_total{model_type=\"skyreels\",status=\"failed\"} 2.0
";
        let c = parse_media_metrics(text).unwrap();
        assert_eq!(c.requests_total, 12);
        assert_eq!(c.errored_total, 5);
    }

    #[test]
    fn none_when_no_media_metrics() {
        // A vLLM body carries no `tt_media_server_` lines -> not a media
        // server, so the media view must stay absent (mirrors
        // `none_when_no_vllm_metrics`).
        assert_eq!(parse_media_metrics(SAMPLE), None);
    }

    #[test]
    fn media_fold_computes_generations_per_min_and_deltas() {
        let prev = MediaCounters {
            requests_total: 12,
            errored_total: 1,
            duration_count: 12,
            duration_sum: 240.0,
            ..Default::default()
        };
        let cur = MediaCounters {
            requests_total: 18,
            errored_total: 3,
            jobs_in_progress: 2,
            duration_count: 18,
            duration_sum: 420.0,
            ..Default::default()
        };
        // 6 generations over 2s -> 3/s -> 180/min.
        let m = MediaInfo::fold(Some(&prev), &cur, 2.0);
        assert_eq!(m.generations_per_min, 180.0);
        assert_eq!(m.completed_delta, 6);
        assert_eq!(m.errored_delta, 2);
        assert_eq!(m.jobs_in_progress, 2);
        // Cumulative totals ride along so a remote viewer shows real
        // "N done" / "errors N" instead of reconstructing from deltas.
        assert_eq!(m.completed_total, 18);
        assert_eq!(m.errored_total, 3);
        // Windowed mean over just this tick: 180s / 6 completions = 30s.
        assert_eq!(m.duration_avg_s, 30.0);
    }

    #[test]
    fn media_fold_without_prev_is_zero_rates_but_keeps_gauges() {
        let cur = MediaCounters {
            requests_total: 12,
            jobs_in_progress: 3,
            duration_sum: 240.0,
            duration_count: 12,
            ..Default::default()
        };
        let m = MediaInfo::fold(None, &cur, 0.0);
        assert_eq!(m.generations_per_min, 0.0);
        assert_eq!(m.completed_delta, 0);
        assert_eq!(m.errored_delta, 0);
        // Gauge + cumulative totals still populate on the first tick.
        assert_eq!(m.jobs_in_progress, 3);
        assert_eq!(m.completed_total, 12);
        // No previous -> lifetime mean: 240/12 = 20s.
        assert_eq!(m.duration_avg_s, 20.0);
    }

    #[test]
    fn media_fold_clamps_counter_reset_to_zero() {
        // Server restarted: counters went backwards. Rates/deltas must clamp
        // to 0 rather than underflow into huge numbers.
        let prev = MediaCounters {
            requests_total: 500,
            errored_total: 10,
            ..Default::default()
        };
        let cur = MediaCounters {
            requests_total: 2,
            errored_total: 0,
            ..Default::default()
        };
        let m = MediaInfo::fold(Some(&prev), &cur, 1.0);
        assert_eq!(m.generations_per_min, 0.0);
        assert_eq!(m.completed_delta, 0);
        assert_eq!(m.errored_delta, 0);
    }

    #[test]
    fn media_fold_falls_back_to_lifetime_mean_when_nothing_completed() {
        // Nothing completed this window -> hold the lifetime mean rather than
        // dropping the displayed average to 0 (same rule as ServingInfo).
        let prev = MediaCounters {
            duration_sum: 240.0,
            duration_count: 12,
            post_sum: 6.0,
            post_count: 12,
            ..Default::default()
        };
        let cur = prev;
        let m = MediaInfo::fold(Some(&prev), &cur, 2.0);
        assert_eq!(m.duration_avg_s, 20.0); // 240/12
        assert_eq!(m.post_avg_s, 0.5); // 6/12
        // Never-observed stages stay 0 (callers hide a zero stage row).
        assert_eq!(m.pre_avg_s, 0.0);
        assert_eq!(m.inference_avg_s, 0.0);
        assert_eq!(m.warmup_avg_s, 0.0);
    }

    #[test]
    fn parse_scrape_picks_the_right_server_flavor() {
        // One endpoint, two possible namespaces -- the scrape decides which.
        assert!(matches!(
            parse_scrape(SAMPLE),
            Some(ScrapeCounters::Vllm(_))
        ));
        assert!(matches!(
            parse_scrape(MEDIA_SAMPLE),
            Some(ScrapeCounters::Media(_))
        ));
        assert!(parse_scrape("# nothing useful here\n").is_none());
    }

    #[test]
    fn media_scrape_is_ready_with_media_stats_and_no_serving() {
        // `serving` and `media` are mutually exclusive on the wire.
        let counters = parse_media_metrics(MEDIA_SAMPLE).unwrap();
        let info = build_inference(
            &ServingStatus::Serving("SkyReels-V1".into()),
            Some(ScrapeCounters::Media(counters)),
            None,
            None,
            None,
            1.0,
        )
        .expect("Some");
        assert_eq!(info.phase, Phase::Ready);
        assert!(info.serving.is_none(), "media workload has no vLLM serving");
        let media = info.media.expect("media stats present");
        assert_eq!(media.jobs_in_progress, 2);
        assert_eq!(media.completed_total, 12);
    }

    #[test]
    fn vllm_scrape_leaves_media_absent() {
        let counters = parse_vllm_metrics(SAMPLE).unwrap();
        let info = build_inference(
            &ServingStatus::Serving("M".into()),
            Some(ScrapeCounters::Vllm(counters)),
            None,
            None,
            None,
            1.0,
        )
        .expect("Some");
        assert!(info.serving.is_some());
        assert!(info.media.is_none(), "vLLM workload has no media stats");
    }

    #[test]
    fn media_info_serializes_with_the_remote_media_field_names() {
        // These key names are the wire contract with tt-toplike's
        // `RemoteMedia`; a rename here silently blanks the remote UI.
        let counters = parse_media_metrics(MEDIA_SAMPLE).unwrap();
        let info = build_inference(
            &ServingStatus::Serving("SkyReels-V1".into()),
            Some(ScrapeCounters::Media(counters)),
            None,
            None,
            None,
            1.0,
        )
        .unwrap();
        let v = serde_json::to_value(&info).unwrap();
        let media = v.get("media").expect("media key present");
        for key in [
            "generations_per_min",
            "jobs_in_progress",
            "completed_total",
            "errored_total",
            "completed_delta",
            "errored_delta",
            "duration_avg_s",
            "post_avg_s",
            "pre_avg_s",
            "inference_avg_s",
            "warmup_avg_s",
        ] {
            assert!(media.get(key).is_some(), "missing wire field `{key}`");
        }
    }

    #[test]
    fn inference_info_serializes_media_as_explicit_null_for_vllm() {
        // `media` is always emitted (null when absent), matching how
        // `progress`/`serving` are handled. tt-toplike's `RemoteMedia` field
        // has a serde default so either form decodes, and an OLDER
        // tt-toplike simply ignores the unknown key.
        let counters = parse_vllm_metrics(SAMPLE).unwrap();
        let info = build_inference(
            &ServingStatus::Serving("M".into()),
            Some(ScrapeCounters::Vllm(counters)),
            None,
            None,
            None,
            1.0,
        )
        .unwrap();
        let v = serde_json::to_value(&info).unwrap();
        assert!(v.get("media").is_some(), "media key emitted");
        assert!(v["media"].is_null(), "media is null for a vLLM workload");
    }

    #[test]
    fn sampler_media_first_tick_zero_rate_second_tick_real_rate() {
        let mut sampler = InferenceSampler::new();
        let status = ServingStatus::Serving("SkyReels-V1".into());

        let first = sampler.tick(&status, Some(MEDIA_SAMPLE)).expect("Some");
        let m1 = first.media.expect("media present");
        assert_eq!(m1.generations_per_min, 0.0, "no baseline on first tick");
        assert_eq!(m1.completed_total, 12);

        // More generations completed by the next scrape.
        let sample2 = MEDIA_SAMPLE.replace("} 12.0", "} 18.0");
        let second = sampler.tick(&status, Some(&sample2)).expect("Some");
        let m2 = second.media.expect("media present");
        assert!(
            m2.generations_per_min > 0.0,
            "second tick computes a real rate"
        );
        assert_eq!(m2.completed_delta, 6);
    }

    #[test]
    fn sampler_failed_scrape_does_not_reset_media_baseline() {
        let mut sampler = InferenceSampler::new();
        let status = ServingStatus::Serving("SkyReels-V1".into());
        sampler.tick(&status, Some(MEDIA_SAMPLE));

        // A blip: the scrape failed entirely -> loading, no stats.
        let blip = sampler.tick(&status, None).expect("Some");
        assert_eq!(blip.phase, Phase::Loading);
        assert!(blip.media.is_none());

        // The held media baseline must have survived the blip.
        let sample2 = MEDIA_SAMPLE.replace("} 12.0", "} 18.0");
        let third = sampler.tick(&status, Some(&sample2)).expect("Some");
        assert_eq!(third.media.expect("media").completed_delta, 6);
    }

    #[test]
    fn media_histogram_companion_lines_do_not_collide() {
        // `_bucket` / `_created` companions share a prefix with the
        // `_sum`/`_count` names; exact-name matching must ignore them.
        let text = "\
tt_media_server_requests_base_duration_seconds_total_bucket{le=\"5.0\"} 99.0
tt_media_server_requests_base_duration_seconds_total_created 1.7e9
tt_media_server_requests_base_duration_seconds_total_sum 240.0
tt_media_server_requests_base_duration_seconds_total_count 12.0
";
        let c = parse_media_metrics(text).unwrap();
        assert_eq!(c.duration_sum, 240.0);
        assert_eq!(c.duration_count, 12);
    }
}
