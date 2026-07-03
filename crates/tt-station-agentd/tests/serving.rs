//! Integration tests for the `ServingBackend` abstraction (Task 9).
//!
//! `DockerBackend` is exercised entirely through a `FakeRunner` -- no real
//! `docker` binary and no real HTTP server are touched. This proves out the
//! *shape* of the commands/health-probe `DockerBackend` issues (the seam
//! that lets a Mac-side test suite trust the Docker story without a GPU box
//! on hand) without making these tests flaky or slow.
//!
//! `DstackBackend` is exercised directly since it's a documented stub with
//! no external dependencies to fake.

use std::time::Duration;

use libttstation::model::{Endpoint, ServingStatus};
use tt_station_agentd::serving::docker::DockerBackend;
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// `start` should issue exactly one `docker run` command whose args contain
/// both the model name and the host port (the two things the brief calls
/// out as load-bearing: without the model, docker doesn't know what to
/// serve; without the port mapping, nothing on the host can reach it), poll
/// health until OK, flip internal status to `Serving`, and return the
/// expected `Endpoint`.
#[test]
fn docker_start_issues_run_command_and_returns_endpoint() {
    let runner = FakeRunner::new(0); // healthy on the very first probe
    let backend = DockerBackend::new(
        "tenstorrent/tt-inference-server:latest".to_string(),
        "127.0.0.1".to_string(),
        8080,
        Box::new(runner.clone()),
    );

    let endpoint = backend.start("llama3").expect("start should succeed");

    assert_eq!(
        endpoint,
        Endpoint {
            base_url: "http://127.0.0.1:8080/v1".to_string(),
            model: "llama3".to_string(),
            requires_key: false,
        }
    );

    let commands = runner.commands();
    assert_eq!(commands.len(), 1, "expected exactly one docker run command");
    let run_cmd = &commands[0];
    assert_eq!(run_cmd[0], "run");
    assert!(
        run_cmd.iter().any(|a| a.contains("llama3")),
        "docker run args should mention the model: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a.contains("8080")),
        "docker run args should mention the port: {run_cmd:?}"
    );

    assert_eq!(
        backend.status().unwrap(),
        ServingStatus::Serving("llama3".to_string())
    );
}

/// The health poll should actually poll more than once when the first
/// probes report unhealthy -- proving the loop isn't a single check in
/// disguise. Kept fast via `with_health_poll`'s test-only override so this
/// doesn't sleep at production intervals.
#[test]
fn docker_start_polls_health_until_ok() {
    let runner = FakeRunner::new(2); // unhealthy for the first two probes
    let backend = DockerBackend::new(
        "tenstorrent/tt-inference-server:latest".to_string(),
        "127.0.0.1".to_string(),
        8081,
        Box::new(runner.clone()),
    )
    .with_health_poll(10, Duration::from_millis(1));

    backend
        .start("llama3")
        .expect("start should eventually succeed");
}

/// If health never comes up within the bounded number of attempts, `start`
/// must return an `Err` rather than hang or silently report success.
#[test]
fn docker_start_times_out_when_never_healthy() {
    let runner = FakeRunner::new(u32::MAX); // never reports healthy
    let backend = DockerBackend::new(
        "tenstorrent/tt-inference-server:latest".to_string(),
        "127.0.0.1".to_string(),
        8082,
        Box::new(runner),
    )
    .with_health_poll(3, Duration::from_millis(1));

    let err = backend.start("llama3").expect_err("start should time out");
    assert!(err.to_string().contains("llama3"));
}

/// `stop` should issue a `docker stop` command naming the model's
/// container, and reset status back to `Idle`.
#[test]
fn docker_stop_issues_stop_command() {
    let runner = FakeRunner::new(0);
    let backend = DockerBackend::new(
        "tenstorrent/tt-inference-server:latest".to_string(),
        "127.0.0.1".to_string(),
        8080,
        Box::new(runner.clone()),
    );

    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    assert_eq!(
        commands.len(),
        1,
        "expected exactly one docker stop command"
    );
    assert_eq!(commands[0][0], "stop");
    assert!(
        commands[0].iter().any(|a| a.contains("llama3")),
        "docker stop args should mention the model: {:?}",
        commands[0]
    );

    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}

/// `DstackBackend` is an intentional stub ahead of M4: `start` must fail
/// loudly (never silently pretend to serve), naming dstack in the error so
/// a caller trying to debug "why didn't my model start" isn't left
/// guessing.
#[test]
fn dstack_start_returns_not_implemented_error() {
    let backend = DstackBackend;
    let err = backend
        .start("llama3")
        .expect_err("dstack start should fail");
    assert!(
        err.to_string().to_lowercase().contains("dstack"),
        "error should mention dstack: {err}"
    );
}

/// `stop`/`status` on the stub are harmless no-ops -- there's never
/// anything running to stop, so `stop` succeeds trivially and `status` is
/// always `Idle`.
#[test]
fn dstack_stop_is_ok_and_status_is_idle() {
    let backend = DstackBackend;
    assert!(backend.stop("llama3").is_ok());
    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}
