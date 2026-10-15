// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The agent reliability eval (`tt_station_agentd::reliability`) against any
//! OpenAI-compatible server, without the agent: for a full run to report on.
//!
//! ```text
//! cargo run --release -p tt-station-agentd --example agent_reliability -- \
//!     --url http://localhost:8003/v1 --histories 8 --tasks 5 --out results.json
//! ```
//!
//! Prints the tables (first replies; recovery through the translator and
//! retries) and the recommended compaction window; `--out` keeps the
//! report and every graded request. `--save-eval DIR` also writes it as a
//! check-up into DIR (the agent's is `~/.config/tt-station/evals`), where Code
//! sessions on that model pick up its window.

use std::time::Duration;

use clap::Parser;
use serde_json::{json, Value};
use tt_station_agentd::{evals, reliability};

#[derive(Parser)]
struct Args {
    /// The server's OpenAI base URL.
    #[arg(long, default_value = "http://localhost:8003/v1")]
    url: String,
    /// The model (default: the first the server lists).
    #[arg(long)]
    model: Option<String>,
    /// Sizes in prompt tokens, comma-separated.
    #[arg(long, value_delimiter = ',')]
    lengths: Option<Vec<u64>>,
    /// Generated sessions per size.
    #[arg(long, default_value_t = 4)]
    histories: usize,
    /// Last turns asked of each session.
    #[arg(long, default_value_t = 5)]
    tasks: usize,
    /// Requests at once.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Times a still-invalid call gets Claude Code's error back and is asked again (1-3).
    #[arg(long, default_value_t = 2)]
    retries: usize,
    /// Where to write the report and the graded requests (JSON).
    #[arg(long)]
    out: Option<String>,
    /// Also save it as a check-up in this folder.
    #[arg(long)]
    save_eval: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let a = Args::parse();
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(900))
        .build()
        .map_err(|e| e.to_string())?;
    let models: Value = http
        .get(format!("{}/models", a.url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let listed = models["data"].as_array().cloned().unwrap_or_default();
    let model = a
        .model
        .clone()
        .or_else(|| {
            listed
                .first()
                .and_then(|m| m["id"].as_str())
                .map(str::to_string)
        })
        .ok_or("the server lists no model")?;
    let context = listed
        .iter()
        .find(|m| m["id"] == model.as_str())
        .and_then(|m| m["max_model_len"].as_u64());
    let server = reliability::Server {
        http,
        base_url: a.url.clone(),
        model: model.clone(),
        context,
    };
    let opts = reliability::Options {
        lengths: a
            .lengths
            .clone()
            .unwrap_or_else(|| reliability::LENGTHS.to_vec()),
        histories: a.histories,
        tasks: a.tasks,
        concurrency: a.concurrency.clamp(1, 8),
        seed: a.seed,
        retries: a.retries.clamp(1, 3),
    };
    eprintln!(
        "{model} ({} context): {} sessions x {} tasks per size, {} at once",
        context.map_or("unknown".into(), |c| format!("{}K", c / 1024)),
        opts.histories,
        opts.tasks,
        opts.concurrency
    );
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = std::time::Instant::now();
    let (report, trials) = reliability::run(
        &server,
        &opts,
        &|s| eprintln!("[{:>5.0} s] {s}", t.elapsed().as_secs_f64()),
        &|| false,
    )
    .await?;
    let window = |w: Option<u64>| {
        w.map_or("none (no tested size held up)".into(), |w| {
            format!("{w} tokens ({}K)", w / 1024)
        })
    };
    println!("{}", reliability::table(&report));
    println!("{}", reliability::recovery_table(&report));
    println!(
        "Recommended compaction window: {}",
        window(report.recommended_window)
    );
    println!("Rule: {}", report.rule);
    println!(
        "By the earlier first-reply rule: {}",
        window(report.raw_window)
    );
    eprintln!("{:.0} s in all", t.elapsed().as_secs_f64());
    if let Some(out) = &a.out {
        let v = json!({ "report": report, "trials": trials, "seconds": t.elapsed().as_secs_f64(), "started": started, "url": a.url });
        std::fs::write(
            out,
            serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some(d) = &a.save_eval {
        let id = evals::new_id();
        let e = evals::Eval {
            id: id.clone(),
            model,
            started,
            finished: Some(started + t.elapsed().as_secs()),
            status: "done".into(),
            context,
            categories: vec![evals::agent_category(&report)],
            reliability: Some(report),
            ..Default::default()
        };
        evals::save_in(std::path::Path::new(d), &e);
        evals::save_trials(std::path::Path::new(d), &id, &trials);
        eprintln!("saved as check-up {id} in {d}");
    }
    Ok(())
}
