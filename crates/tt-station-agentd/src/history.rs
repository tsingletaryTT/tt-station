// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What the box has been doing over time, for dashboards: per-chip temperature,
//! power and AI clock, plus the served model's throughput and latency, sampled
//! every few seconds by a background recorder (`routes::record_metrics_forever`)
//! whether or not anyone is watching -- so a phone opening the dashboard sees the
//! last hour, not an empty chart that starts filling now.
//!
//! Two rings: `fine` (one point per sample, the last hour) and `coarse` (one-minute
//! averages, the last day). Energy (Wh) and tokens generated are integrated as
//! samples arrive, with totals for "today" (the box's local day) and all time.
//! The rings and totals are saved to disk periodically so an agent restart
//! doesn't blank the history.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Points kept at full resolution: an hour at a 5 s cadence.
pub const FINE_LEN: usize = 720;
/// One-minute averages kept: a day.
pub const COARSE_LEN: usize = 1440;
/// A gap longer than this between samples (agent down, box asleep) isn't
/// integrated into energy/tokens -- we don't know what happened in it.
const MAX_GAP_S: u64 = 60;
/// Charts get at most this many points per range.
const MAX_POINTS: usize = 150;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChipPoint {
    pub temp_c: Option<f32>,
    pub power_w: Option<f32>,
    pub aiclk_mhz: Option<f32>,
}

/// One moment of the box.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Unix seconds.
    pub t: u64,
    pub chips: Vec<ChipPoint>,
    /// Output tokens/s across all requests (vLLM), when a chat model serves.
    pub gen_tps: Option<f32>,
    pub prompt_tps: Option<f32>,
    pub running: Option<u32>,
    pub waiting: Option<u32>,
    /// KV-cache usage, 0..1.
    pub kv_cache: Option<f32>,
    /// Mean time to first token / per output token over the last interval, seconds.
    pub ttft_s: Option<f32>,
    pub tpot_s: Option<f32>,
}

impl Point {
    pub fn total_power_w(&self) -> Option<f32> {
        let watts: Vec<f32> = self.chips.iter().filter_map(|c| c.power_w).collect();
        (!watts.is_empty()).then(|| watts.iter().sum())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Local calendar day (YYYY-MM-DD) the `*_today` fields belong to.
    pub day: String,
    pub energy_wh_today: f64,
    pub tokens_today: f64,
    pub requests_today: u64,
    pub energy_wh_all: f64,
    pub tokens_all: f64,
    pub requests_all: u64,
    /// Unix seconds the all-time totals started counting.
    pub since: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    FiveMinutes,
    Hour,
    Day,
}

impl Range {
    pub fn parse(s: &str) -> Option<Range> {
        match s {
            "5m" => Some(Range::FiveMinutes),
            "1h" => Some(Range::Hour),
            "24h" | "1d" => Some(Range::Day),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Inner {
    fine: VecDeque<Point>,
    coarse: VecDeque<Point>,
    /// Samples of the minute not yet rolled into `coarse`.
    minute: Vec<Point>,
    totals: Totals,
    last_t: Option<u64>,
    /// Per local day: energy, tokens and requests, for the monthly report.
    #[serde(default)]
    days: std::collections::BTreeMap<String, Day>,
}

/// One local day's totals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Day {
    pub energy_wh: f64,
    pub tokens: f64,
    pub requests: u64,
}

/// Days kept for the energy report (a bit over two years).
const DAYS_KEPT: usize = 800;

/// The recorded history. Cheap to share behind an `Arc`.
#[derive(Debug, Default)]
pub struct History {
    inner: Mutex<Inner>,
    /// The latest `inference::InferenceInfo` (phase, label, serving/media
    /// counters), as JSON -- live only, not saved.
    inference: Mutex<Option<serde_json::Value>>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a sample. `completed` is how many requests finished since the last
    /// one; `today` is the local date of `point.t` (passed in so tests don't
    /// depend on the timezone).
    pub fn record(&self, point: Point, completed: u32, today: &str) {
        let mut inner = self.inner.lock().expect("history mutex poisoned");
        if inner.totals.since == 0 {
            inner.totals.since = point.t;
        }
        if inner.totals.day != today {
            inner.totals.day = today.to_string();
            inner.totals.energy_wh_today = 0.0;
            inner.totals.tokens_today = 0.0;
            inner.totals.requests_today = 0;
        }
        let (mut wh, mut tokens) = (0.0, 0.0);
        if let Some(last) = inner.last_t {
            let dt = point.t.saturating_sub(last);
            if dt > 0 && dt <= MAX_GAP_S {
                let hours = dt as f64 / 3600.0;
                if let Some(w) = point.total_power_w() {
                    wh = w as f64 * hours;
                    inner.totals.energy_wh_today += wh;
                    inner.totals.energy_wh_all += wh;
                }
                if let Some(tps) = point.gen_tps {
                    tokens = tps as f64 * dt as f64;
                    inner.totals.tokens_today += tokens;
                    inner.totals.tokens_all += tokens;
                }
            }
        }
        inner.totals.requests_today += completed as u64;
        inner.totals.requests_all += completed as u64;
        let day = inner.days.entry(today.to_string()).or_default();
        day.energy_wh += wh;
        day.tokens += tokens;
        day.requests += completed as u64;
        while inner.days.len() > DAYS_KEPT {
            let oldest = inner.days.keys().next().cloned();
            if let Some(k) = oldest {
                inner.days.remove(&k);
            }
        }
        inner.last_t = Some(point.t);

        inner.fine.push_back(point.clone());
        while inner.fine.len() > FINE_LEN {
            inner.fine.pop_front();
        }
        // Roll a minute's samples into one coarse point.
        if let Some(first) = inner.minute.first() {
            if point.t / 60 != first.t / 60 {
                let avg = average(&inner.minute);
                inner.coarse.push_back(avg);
                while inner.coarse.len() > COARSE_LEN {
                    inner.coarse.pop_front();
                }
                inner.minute.clear();
            }
        }
        inner.minute.push(point);
    }

    /// Every recorded day, oldest first.
    pub fn days(&self) -> Vec<(String, Day)> {
        let inner = self.inner.lock().expect("history mutex poisoned");
        inner
            .days
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn set_inference(&self, info: Option<serde_json::Value>) {
        *self.inference.lock().expect("inference mutex poisoned") = info;
    }

    pub fn inference(&self) -> Option<serde_json::Value> {
        self.inference
            .lock()
            .expect("inference mutex poisoned")
            .clone()
    }

    /// The latest sample, if any.
    pub fn latest(&self) -> Option<Point> {
        self.inner
            .lock()
            .expect("history mutex poisoned")
            .fine
            .back()
            .cloned()
    }

    pub fn totals(&self) -> Totals {
        self.inner
            .lock()
            .expect("history mutex poisoned")
            .totals
            .clone()
    }

    /// Points for `range` ending at `now`, averaged down to at most
    /// [`MAX_POINTS`]; plus the seconds each returned point covers.
    pub fn range(&self, range: Range, now: u64) -> (u64, Vec<Point>) {
        let inner = self.inner.lock().expect("history mutex poisoned");
        let (source, span, step): (Vec<&Point>, u64, u64) = match range {
            Range::FiveMinutes => (inner.fine.iter().collect(), 300, 5),
            Range::Hour => (inner.fine.iter().collect(), 3600, 5),
            Range::Day => {
                // Coarse minutes, plus the minute in progress.
                let mut all: Vec<&Point> = inner.coarse.iter().collect();
                if !inner.minute.is_empty() {
                    all.extend(inner.fine.iter().rev().take(inner.minute.len()).rev());
                }
                (all, 86_400, 60)
            }
        };
        let start = now.saturating_sub(span);
        let window: Vec<Point> = source
            .into_iter()
            .filter(|p| p.t >= start)
            .cloned()
            .collect();
        if window.len() <= MAX_POINTS {
            return (step, window);
        }
        let per = window.len().div_ceil(MAX_POINTS);
        let points = window.chunks(per).map(average).collect();
        (step * per as u64, points)
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = {
            let inner = self.inner.lock().expect("history mutex poisoned");
            serde_json::to_vec(&*inner).map_err(std::io::Error::other)?
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)
    }

    /// Load a saved history; a missing or unreadable file is an empty history.
    pub fn load(path: &Path) -> Self {
        let inner = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Inner>(&b).ok())
            .unwrap_or_default();
        History {
            inner: Mutex::new(inner),
            inference: Mutex::new(None),
        }
    }
}

/// The average of some points: time is the first point's, each field the mean
/// of the values present.
fn average(points: &[Point]) -> Point {
    fn mean_f(values: impl Iterator<Item = Option<f32>>) -> Option<f32> {
        let v: Vec<f32> = values.flatten().collect();
        (!v.is_empty()).then(|| v.iter().sum::<f32>() / v.len() as f32)
    }
    fn mean_u(values: impl Iterator<Item = Option<u32>>) -> Option<u32> {
        let v: Vec<u32> = values.flatten().collect();
        (!v.is_empty()).then(|| (v.iter().sum::<u32>() as f32 / v.len() as f32).round() as u32)
    }
    let chips = points.iter().map(|p| p.chips.len()).max().unwrap_or(0);
    Point {
        t: points.first().map(|p| p.t).unwrap_or(0),
        chips: (0..chips)
            .map(|i| ChipPoint {
                temp_c: mean_f(points.iter().map(|p| p.chips.get(i).and_then(|c| c.temp_c))),
                power_w: mean_f(
                    points
                        .iter()
                        .map(|p| p.chips.get(i).and_then(|c| c.power_w)),
                ),
                aiclk_mhz: mean_f(
                    points
                        .iter()
                        .map(|p| p.chips.get(i).and_then(|c| c.aiclk_mhz)),
                ),
            })
            .collect(),
        gen_tps: mean_f(points.iter().map(|p| p.gen_tps)),
        prompt_tps: mean_f(points.iter().map(|p| p.prompt_tps)),
        running: mean_u(points.iter().map(|p| p.running)),
        waiting: mean_u(points.iter().map(|p| p.waiting)),
        kv_cache: mean_f(points.iter().map(|p| p.kv_cache)),
        ttft_s: mean_f(points.iter().map(|p| p.ttft_s)),
        tpot_s: mean_f(points.iter().map(|p| p.tpot_s)),
    }
}

/// Per-chip readings from a `tt-smi -s` snapshot (numbers arrive as strings).
pub fn chips_from_snapshot(tt_smi_json: &str) -> Vec<ChipPoint> {
    let Ok(lite) =
        serde_json::from_str::<serde_json::Value>(&crate::telemetry::lite_frame(tt_smi_json))
    else {
        return Vec::new();
    };
    let num = |v: Option<&serde_json::Value>| -> Option<f32> {
        match v? {
            serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
            serde_json::Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    };
    lite.get("device_info")
        .and_then(|d| d.as_array())
        .map(|devices| {
            devices
                .iter()
                .map(|d| ChipPoint {
                    temp_c: num(d.pointer("/telemetry/asic_temperature")),
                    power_w: num(d.pointer("/telemetry/power")),
                    aiclk_mhz: num(d.pointer("/telemetry/aiclk")),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The box's local calendar date for unix time `t`, as YYYY-MM-DD.
pub fn local_day(t: u64) -> String {
    // SAFETY: localtime_r writes only into the `tm` we own.
    unsafe {
        let secs = t as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&secs, &mut tm).is_null() {
            return format!("day-{}", t / 86_400);
        }
        format!(
            "{:04}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday
        )
    }
}

/// The energy report: per month (`YYYY-MM`) and the last 31 days, kWh,
/// tokens and requests, newest last.
pub fn energy_report(days: &[(String, Day)]) -> serde_json::Value {
    let mut months: std::collections::BTreeMap<String, (f64, f64, u64, u32)> = Default::default();
    for (day, d) in days {
        let m = months
            .entry(day.get(..7).unwrap_or(day).to_string())
            .or_default();
        m.0 += d.energy_wh;
        m.1 += d.tokens;
        m.2 += d.requests;
        m.3 += 1;
    }
    let round = |x: f64| (x * 1000.0).round() / 1000.0;
    serde_json::json!({
        "months": months.into_iter().map(|(month, (wh, tokens, requests, n))| serde_json::json!({
            "month": month, "energy_kwh": round(wh / 1000.0), "tokens": tokens.round(),
            "requests": requests, "days": n,
        })).collect::<Vec<_>>(),
        "days": days.iter().rev().take(31).rev().map(|(day, d)| serde_json::json!({
            "day": day, "energy_kwh": round(d.energy_wh / 1000.0), "tokens": d.tokens.round(), "requests": d.requests,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn days_add_up_into_months() {
        let h = History::new();
        let chip = |w: f32| ChipPoint {
            power_w: Some(w),
            ..Default::default()
        };
        let p = |t: u64| Point {
            t,
            chips: vec![chip(100.0), chip(100.0)],
            ..Default::default()
        };
        // An hour of 10 s samples on the 30th at 200 W: 200 Wh; then one on the 1st.
        for i in 0..=360 {
            h.record(p(i * 10), if i == 360 { 2 } else { 0 }, "2026-09-30");
        }
        h.record(p(3610), 1, "2026-10-01");
        let days = h.days();
        assert_eq!(days.len(), 2);
        assert!((days[0].1.energy_wh - 200.0).abs() < 1e-6);
        assert_eq!(days[0].1.requests, 2);
        let r = energy_report(&days);
        assert_eq!(r["months"][0]["month"], "2026-09");
        assert_eq!(r["months"][0]["energy_kwh"], 0.2);
        assert_eq!(r["months"][1]["month"], "2026-10");
        assert_eq!(r["days"].as_array().unwrap().len(), 2);
    }

    use super::*;

    fn point(t: u64, watts: f32, tps: Option<f32>) -> Point {
        Point {
            t,
            chips: vec![
                ChipPoint {
                    temp_c: Some(50.0),
                    power_w: Some(watts / 2.0),
                    aiclk_mhz: Some(1000.0),
                },
                ChipPoint {
                    temp_c: Some(60.0),
                    power_w: Some(watts / 2.0),
                    aiclk_mhz: Some(1000.0),
                },
            ],
            gen_tps: tps,
            ..Point::default()
        }
    }

    #[test]
    fn energy_and_tokens_integrate_over_time_and_skip_gaps() {
        let h = History::new();
        h.record(point(0, 360.0, Some(10.0)), 0, "d1");
        h.record(point(10, 360.0, Some(10.0)), 2, "d1"); // 10 s at 360 W = 1 Wh; 100 tokens
        h.record(point(1000, 360.0, Some(10.0)), 0, "d1"); // a gap: not integrated
        let t = h.totals();
        assert!((t.energy_wh_today - 1.0).abs() < 1e-9, "{t:?}");
        assert!((t.tokens_today - 100.0).abs() < 1e-9);
        assert_eq!(t.requests_today, 2);
    }

    #[test]
    fn a_new_day_resets_today_but_not_all_time() {
        let h = History::new();
        h.record(point(0, 360.0, None), 1, "d1");
        h.record(point(10, 360.0, None), 1, "d1");
        h.record(point(20, 360.0, None), 1, "d2");
        let t = h.totals();
        assert_eq!(t.day, "d2");
        assert_eq!(t.requests_today, 1);
        assert_eq!(t.requests_all, 3);
        assert!(t.energy_wh_today > 0.0 && t.energy_wh_all > t.energy_wh_today);
    }

    #[test]
    fn minutes_roll_into_coarse_points_and_ranges_downsample() {
        let h = History::new();
        for i in 0..(3 * 3600 / 5) {
            h.record(point(i * 5, 100.0 + (i % 12) as f32, Some(5.0)), 0, "d");
        }
        let now = 3 * 3600;
        let (step, five) = h.range(Range::FiveMinutes, now);
        assert_eq!(step, 5);
        assert!(five.len() <= 61 && five.len() >= 59, "{}", five.len());
        let (step, hour) = h.range(Range::Hour, now);
        assert!(
            hour.len() <= MAX_POINTS && step > 5,
            "{} points every {step}s",
            hour.len()
        );
        let (step, day) = h.range(Range::Day, now);
        // ~180 one-minute points, averaged down under the cap (two minutes each).
        assert!(
            day.len() <= MAX_POINTS && day.len() >= 85,
            "{} points",
            day.len()
        );
        assert_eq!(step, 120);
        // Averaging keeps values sensible.
        let p = &hour[0];
        assert!(p.chips[0].temp_c == Some(50.0) && p.gen_tps == Some(5.0));
    }

    #[test]
    fn history_survives_a_save_and_load() {
        let h = History::new();
        h.record(point(0, 200.0, None), 0, "d");
        h.record(point(5, 200.0, None), 0, "d");
        let path = std::env::temp_dir().join(format!("tt-history-{}.json", std::process::id()));
        h.save(&path).unwrap();
        let back = History::load(&path);
        assert_eq!(back.latest().map(|p| p.t), Some(5));
        assert_eq!(back.totals(), h.totals());
        let _ = std::fs::remove_file(path);
        assert!(History::load(Path::new("/nonexistent/x.json"))
            .latest()
            .is_none());
    }

    #[test]
    fn local_day_is_a_date() {
        let d = local_day(1_790_000_000);
        assert_eq!(d.len(), 10);
        assert_eq!(&d[4..5], "-");
    }
}
