// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Agent reliability's measuring loop (`reliability::run`) against stub
//! models on 127.0.0.1 that misbehave in known ways: what a retry with
//! Claude Code's validation error recovers and what it doesn't, failed
//! requests left out of the grade, sizes that don't fit the context skipped,
//! and stopping between requests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{extract::Json as AxJson, http::header, response::IntoResponse, routing, Router};
use serde_json::{json, Value};
use tt_station_agentd::reliability::{self, Options, Outcome, Server};

#[derive(Clone, Copy)]
enum Model {
    /// Leaves out Edit's required arguments, then gets it right once told why.
    FixesAfterTheError,
    /// Calls a tool that wasn't offered, every time.
    InventsTools,
    /// Fails every graded turn (the one-token size counts still answer).
    Broken,
}

fn sse(events: &[Value]) -> axum::response::Response {
    let mut body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    body.push_str("data: [DONE]\n\n");
    ([(header::CONTENT_TYPE, "text/event-stream")], body).into_response()
}

fn call(name: &str, args: Value) -> axum::response::Response {
    sse(&[
        json!({ "choices": [{ "index": 0, "delta": { "role": "assistant", "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": name, "arguments": args.to_string() } }] } }] }),
        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }], "usage": { "prompt_tokens": 8000, "completion_tokens": 10 } }),
    ])
}

/// The model at `/v1`, and how many graded (streamed) requests it answered.
async fn serve(model: Model) -> (String, Arc<AtomicUsize>) {
    let turns = Arc::new(AtomicUsize::new(0));
    let t = turns.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        routing::post(move |AxJson(body): AxJson<Value>| {
            let turns = t.clone();
            async move {
                if body["stream"] != true {
                    // Sizing a session: say it's exactly the size asked for, so it
                    // settles at once.
                    let tokens = (body.to_string().len() / 4) as u64;
                    return AxJson(json!({ "choices": [{ "message": { "content": "x" } }], "usage": { "prompt_tokens": tokens } })).into_response();
                }
                turns.fetch_add(1, Ordering::SeqCst);
                let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or_default();
                let told = last["role"] == "tool" && last.to_string().contains("tool_use_error");
                match model {
                    Model::FixesAfterTheError if told => call("Read", json!({ "file_path": "src/lib.rs" })),
                    Model::FixesAfterTheError => call("Edit", json!({ "file_path": "src/lib.rs" })),
                    Model::InventsTools => call("WebBrowse", json!({ "url": "https://example.com" })),
                    Model::Broken => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        AxJson(json!({ "error": { "message": "engine died" } })),
                    )
                        .into_response(),
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (base, turns)
}

fn server(base: String, context: Option<u64>) -> Server {
    Server {
        http: reqwest::Client::new(),
        base_url: base,
        model: "acme/stub-chat-8b".into(),
        context,
    }
}

/// One 8K session, four tasks, two retries: small and quick.
fn small() -> Options {
    Options {
        lengths: vec![8_192],
        histories: 1,
        tasks: 4,
        concurrency: 1,
        seed: 7,
        retries: 2,
    }
}

#[tokio::test]
async fn a_call_fixed_after_claude_codes_error_counts_as_recovered_not_as_valid() {
    let (base, turns) = serve(Model::FixesAfterTheError).await;
    let (report, trials) = reliability::run(&server(base, None), &small(), &|_| {}, &|| false)
        .await
        .unwrap();
    assert_eq!(trials.len(), 4);
    for t in &trials {
        assert_eq!(t.outcome, Outcome::InvalidArgs, "{t:?}");
        assert_eq!(t.valid_after, Some(1), "valid after one retry: {t:?}");
        assert_eq!(t.retries.len(), 1);
        assert!(
            t.retries[0].sent[0].contains("The required parameter `old_string` is missing"),
            "told what Claude Code would tell it: {:?}",
            t.retries[0].sent
        );
        assert!(t.retries[0].valid);
    }
    assert_eq!(turns.load(Ordering::SeqCst), 8, "one retry each, no more");
    let row = &report.rows[0];
    assert_eq!((row.trials, row.valid, row.invalid_args), (4, 0, 4));
    assert_eq!((row.repaired, row.within_1_retry), (0, 4));
    assert_eq!(row.recovered_rate, 1.0);
    assert_eq!(row.valid_rate, 0.0);
    assert_eq!(
        report.recommended_window, None,
        "4 trials are too few for a recommendation, however good"
    );
}

#[tokio::test]
async fn an_invented_tool_is_retried_up_to_the_limit_then_left_failing() {
    let (base, turns) = serve(Model::InventsTools).await;
    let (report, trials) = reliability::run(&server(base, None), &small(), &|_| {}, &|| false)
        .await
        .unwrap();
    for t in &trials {
        assert_eq!(t.outcome, Outcome::Invented, "{t:?}");
        assert_eq!(t.valid_after, None);
        assert_eq!(t.failed_as.as_deref(), Some("still invalid"));
        assert_eq!(t.retries.len(), 2, "Options::retries, no more");
        assert!(t.retries[0].sent[0].contains("No such tool available: WebBrowse"));
    }
    assert_eq!(turns.load(Ordering::SeqCst), 4 * 3);
    let row = &report.rows[0];
    assert_eq!(
        (row.invented, row.still_failing, row.within_2_retries),
        (4, 4, 0)
    );
}

#[tokio::test]
async fn failed_requests_are_left_out_of_the_grade() {
    let (base, _) = serve(Model::Broken).await;
    let (report, trials) = reliability::run(&server(base, None), &small(), &|_| {}, &|| false)
        .await
        .unwrap();
    assert!(trials.iter().all(|t| t.outcome == Outcome::Error));
    assert!(
        trials[0].detail.contains("engine died"),
        "{}",
        trials[0].detail
    );
    let row = &report.rows[0];
    assert_eq!(
        (row.trials, row.errors),
        (0, 4),
        "an error isn't a failed call"
    );
    assert_eq!(row.valid_rate, 0.0);
    assert_eq!(report.recommended_window, None);
}

#[tokio::test]
async fn only_sizes_that_leave_room_to_answer_are_tried() {
    let (base, _) = serve(Model::FixesAfterTheError).await;
    let opts = Options {
        lengths: vec![8_192, 16_384, 32_768],
        tasks: 1,
        ..small()
    };
    let heard = std::sync::Mutex::new(Vec::new());
    // 20K of context: 16K + 4K for the answer doesn't fit.
    let (report, _) = reliability::run(
        &server(base, Some(20_000)),
        &opts,
        &|what| heard.lock().unwrap().push(what),
        &|| false,
    )
    .await
    .unwrap();
    let targets: Vec<u64> = report.rows.iter().map(|r| r.target).collect();
    assert_eq!(targets, [8_192]);
    assert_eq!(report.context, Some(20_000));
    assert_eq!(*heard.lock().unwrap(), ["8K-token sessions (1 of 1)"]);
}

#[tokio::test]
async fn a_stop_ends_the_run_between_requests() {
    let (base, turns) = serve(Model::FixesAfterTheError).await;
    let asked = AtomicUsize::new(0);
    // Stop once it has started: the next check between requests ends it.
    let stop = || asked.fetch_add(1, Ordering::SeqCst) >= 2;
    let out = reliability::run(&server(base, None), &small(), &|_| {}, &stop).await;
    assert_eq!(out.err().as_deref(), Some("Stopped."));
    assert!(
        turns.load(Ordering::SeqCst) < 8,
        "it didn't carry on to the end"
    );
}
