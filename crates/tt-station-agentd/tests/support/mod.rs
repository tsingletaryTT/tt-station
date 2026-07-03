//! Shared test-only helpers for `tt-station-agentd`'s integration tests.
//!
//! `tests/*.rs` files are each compiled as their own separate binary crate,
//! so a helper defined in one (e.g. the original `FakeRunner` in
//! `tests/serving.rs`) is invisible to any other. Rather than duplicating it,
//! this lives in `tests/support/mod.rs` -- a subdirectory module, not a
//! top-level `tests/*.rs` file, so Cargo doesn't treat it as its own test
//! target -- and gets pulled into whichever integration test needs it via
//! `mod support;`.
//!
//! `#[allow(dead_code)]` throughout: different consumers (`tests/serving.rs`,
//! `tests/control.rs`) exercise different subsets of this API, and each
//! integration test file is its own compilation unit, so "unused" is
//! per-target rather than a real dead-code signal.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tt_station_agentd::serving::docker::CommandRunner;

/// A fake `CommandRunner` that records every command it's asked to `run`
/// (so tests can assert on the exact argv `DockerBackend` builds) and
/// reports `health_ok` as healthy either immediately or after a configured
/// number of prior probes (so `DockerBackend`'s poll-until-healthy loop is
/// exercised for real, not just its "healthy on the first try" path).
///
/// Cheap to `Clone`: all state lives behind `Arc<Mutex<_>>`, so a test can
/// hand one clone to `DockerBackend` (which needs to own a
/// `Box<dyn CommandRunner>`) while keeping another clone around to inspect
/// what happened after the call returns.
#[derive(Clone)]
pub struct FakeRunner {
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    health_calls_before_ok: u32,
    health_calls_seen: Arc<Mutex<u32>>,
    /// Canned stdout for `run` calls whose argv (space-joined) CONTAINS a
    /// registered substring -- e.g. `"docker ps"` -- so `RunPyBackend::stop`
    /// (which parses `docker ps`'s stdout for container ids) can be
    /// exercised without a real `docker` binary. Checked in insertion order;
    /// first match wins. Calls that match nothing get `""`, same as before
    /// this field existed.
    run_outputs: Arc<Mutex<Vec<(String, String)>>>,
    /// Canned failures for `run` calls whose argv (space-joined) CONTAINS a
    /// registered substring -- e.g. `"tt-smi -r"` -- so tests can exercise
    /// what happens when a specific command (like the pre-serve board
    /// reset) fails, without making every `run` call fail. Checked in
    /// insertion order, same as `run_outputs`; a match short-circuits
    /// `run` with `Err` before it ever records success or consults
    /// `run_outputs`.
    run_failures: Arc<Mutex<Vec<(String, String)>>>,
    /// Canned response body every `http_get` call returns (regardless of
    /// `url`) once `set_http_get` is called -- e.g. `RunPyBackend::start`'s
    /// `GET /v1/models` readiness poll. `None` (the default, unset) means
    /// `http_get` returns `DEFAULT_HTTP_GET_BODY` (see below), NOT an error.
    http_get_response: Arc<Mutex<Option<String>>>,
    /// Canned SEQUENCE of `http_get` responses, consumed one per call and
    /// STICKING at the last entry once exhausted, so a test can model
    /// `/v1/models` erroring/empty for the first few polls and then coming
    /// up populated (exactly what `RunPyBackend::start`'s readiness poll
    /// waits for). Each entry is `Some(body)` (returned `Ok`) or `None`
    /// (returned `Err`). Takes precedence over `http_get_response` whenever
    /// it's non-empty. Mirrors `set_run_output`/`set_http_get`.
    http_get_sequence: Arc<Mutex<Vec<Option<String>>>>,
    /// How many `http_get` calls have been seen -- indexes into
    /// `http_get_sequence`.
    http_get_calls_seen: Arc<Mutex<usize>>,
}

/// Default `http_get` body when nothing is configured: a non-empty `data`
/// array (so `RunPyBackend::start`'s `/v1/models` readiness gate is
/// satisfied and `start` succeeds) whose single entry carries NO `id` (so
/// `Endpoint.model` falls back to the caller's original `model` argument).
/// This lets the many argv-focused tests build a bare `FakeRunner::new(..)`
/// and still have `start` succeed with `endpoint.model == <arg>`, without
/// each having to stub `/v1/models` by hand.
const DEFAULT_HTTP_GET_BODY: &str = r#"{"data":[{}]}"#;

impl FakeRunner {
    #[allow(dead_code)]
    pub fn new(health_calls_before_ok: u32) -> Self {
        FakeRunner {
            commands: Arc::new(Mutex::new(Vec::new())),
            health_calls_before_ok,
            health_calls_seen: Arc::new(Mutex::new(0)),
            run_outputs: Arc::new(Mutex::new(Vec::new())),
            run_failures: Arc::new(Mutex::new(Vec::new())),
            http_get_response: Arc::new(Mutex::new(None)),
            http_get_sequence: Arc::new(Mutex::new(Vec::new())),
            http_get_calls_seen: Arc::new(Mutex::new(0)),
        }
    }

    #[allow(dead_code)]
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .clone()
    }

    /// Register canned stdout `output` for the next (and any subsequent)
    /// `run` call whose space-joined argv contains `matcher`. See the
    /// `run_outputs` field doc for why this exists (`docker ps` parsing in
    /// `RunPyBackend::stop`).
    #[allow(dead_code)]
    pub fn set_run_output(&self, matcher: &str, output: &str) {
        self.run_outputs
            .lock()
            .expect("run_outputs mutex poisoned")
            .push((matcher.to_string(), output.to_string()));
    }

    /// Make any future `run` call whose space-joined argv contains `matcher`
    /// return `Err` with `message` instead of succeeding -- e.g.
    /// `fail_run("tt-smi -r", "board reset timed out")` to exercise a
    /// failing pre-serve board reset without a real `tt-smi` binary.
    #[allow(dead_code)]
    pub fn fail_run(&self, matcher: &str, message: &str) {
        self.run_failures
            .lock()
            .expect("run_failures mutex poisoned")
            .push((matcher.to_string(), message.to_string()));
    }

    /// Set the canned body every future `http_get` call returns, regardless
    /// of `url` -- e.g. `set_http_get(r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#)`
    /// to exercise `RunPyBackend::start`'s `/v1/models` readiness poll.
    /// Left unset (the default), `http_get` returns `DEFAULT_HTTP_GET_BODY`.
    #[allow(dead_code)]
    pub fn set_http_get(&self, body: &str) {
        *self
            .http_get_response
            .lock()
            .expect("http_get_response mutex poisoned") = Some(body.to_string());
    }

    /// Set a SEQUENCE of `http_get` responses, consumed one per call and
    /// sticking at the last once exhausted -- each entry `Some(body)` returns
    /// `Ok(body)`, `None` returns `Err`. Lets a test model `/v1/models`
    /// erroring/empty at first and then coming up populated, driving
    /// `RunPyBackend::start`'s readiness poll through more than one round.
    /// Takes precedence over `set_http_get` while non-empty.
    #[allow(dead_code)]
    pub fn set_http_get_sequence(&self, bodies: &[Option<&str>]) {
        *self
            .http_get_sequence
            .lock()
            .expect("http_get_sequence mutex poisoned") =
            bodies.iter().map(|b| b.map(str::to_string)).collect();
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        let joined = args.join(" ");

        if let Some((_, message)) = self
            .run_failures
            .lock()
            .expect("run_failures mutex poisoned")
            .iter()
            .find(|(matcher, _)| joined.contains(matcher.as_str()))
        {
            return Err(anyhow::anyhow!(message.clone()));
        }

        let output = self
            .run_outputs
            .lock()
            .expect("run_outputs mutex poisoned")
            .iter()
            .find(|(matcher, _)| joined.contains(matcher.as_str()))
            .map(|(_, output)| output.clone())
            .unwrap_or_default();
        Ok(output)
    }

    fn health_ok(&self, _url: &str) -> bool {
        let mut seen = self
            .health_calls_seen
            .lock()
            .expect("health mutex poisoned");
        *seen += 1;
        *seen > self.health_calls_before_ok
    }

    fn http_get(&self, _url: &str) -> Result<String> {
        // A configured sequence wins: consume one entry per call, sticking at
        // the last once exhausted (so a "ready" tail keeps answering ready).
        {
            let sequence = self
                .http_get_sequence
                .lock()
                .expect("http_get_sequence mutex poisoned");
            if !sequence.is_empty() {
                let mut seen = self
                    .http_get_calls_seen
                    .lock()
                    .expect("http_get_calls_seen mutex poisoned");
                let idx = (*seen).min(sequence.len() - 1);
                *seen += 1;
                return match &sequence[idx] {
                    Some(body) => Ok(body.clone()),
                    None => Err(anyhow::anyhow!("FakeRunner: sequenced http_get error")),
                };
            }
        }

        // Otherwise: an explicitly-set single body, else the default "ready
        // but idless" body (see `DEFAULT_HTTP_GET_BODY`).
        Ok(self
            .http_get_response
            .lock()
            .expect("http_get_response mutex poisoned")
            .clone()
            .unwrap_or_else(|| DEFAULT_HTTP_GET_BODY.to_string()))
    }
}
