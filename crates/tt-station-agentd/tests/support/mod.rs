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
}

impl FakeRunner {
    #[allow(dead_code)]
    pub fn new(health_calls_before_ok: u32) -> Self {
        FakeRunner {
            commands: Arc::new(Mutex::new(Vec::new())),
            health_calls_before_ok,
            health_calls_seen: Arc::new(Mutex::new(0)),
            run_outputs: Arc::new(Mutex::new(Vec::new())),
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
}

impl CommandRunner for FakeRunner {
    fn run(&self, args: &[&str]) -> Result<String> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        let joined = args.join(" ");
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
}
