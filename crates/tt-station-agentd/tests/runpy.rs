//! Integration tests for `RunPyBackend` -- the default serving backend,
//! which launches LLMs the way the operator's PROVEN scripts do: via
//! `tt-inference-server/run.py`, not a hand-rolled `docker run`. See
//! `docs/reference/tt-inference-server-docker.md`'s "⭐ Ground truth: launch
//! via run.py" section for the validated invocation this mirrors.
//!
//! Like `tests/serving.rs`, everything here goes through the shared
//! `FakeRunner` test double (`tests/support/mod.rs`) -- no real `python3`,
//! `run.py`, or `docker` binary, and no real HTTP health probe.

use std::time::Duration;

use libttstation::model::ServingStatus;
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// Build a `RunPyConfig` with production-shaped defaults, overriding only
/// `image`/`host`/`service_port` -- the fields most tests in this file vary.
fn config(image: &str, host: &str, service_port: u16) -> RunPyConfig {
    RunPyConfig {
        image: image.to_string(),
        host: host.to_string(),
        service_port,
        ..Default::default()
    }
}

/// `start` should invoke `python3 run.py` (NOT a raw `docker run`) with the
/// full ground-truth argv from `docs/reference/tt-inference-server-docker.md`,
/// poll `/health` until OK, and return an `Endpoint` whose `base_url` ends in
/// `/v1`.
#[test]
fn runpy_start_issues_run_py_command_and_returns_endpoint() {
    let runner = FakeRunner::new(0); // healthy on the very first probe
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    let model = "Llama-3.1-8B-Instruct";
    let endpoint = backend.start(model).expect("start should succeed");

    assert_eq!(endpoint.model, model);
    assert_eq!(endpoint.base_url, "http://127.0.0.1:8080/v1");
    assert!(
        endpoint.base_url.ends_with("/v1"),
        "base_url should end in /v1: {}",
        endpoint.base_url
    );
    assert!(!endpoint.requires_key, "no_auth defaults to true");

    let commands = runner.commands();
    assert_eq!(commands.len(), 1, "expected exactly one run.py invocation");
    let cmd = &commands[0];

    assert_eq!(cmd[0], "python3", "argv[0] must be python3: {cmd:?}");
    assert_eq!(cmd[1], "run.py", "argv[1] must be run.py: {cmd:?}");

    assert!(
        cmd.windows(2).any(|w| w[0] == "--model" && w[1] == model),
        "argv should carry --model <RAW model id>: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--workflow" && w[1] == "server"),
        "argv should carry --workflow server: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--tt-device" && w[1] == "p300x2"),
        "argv should carry the default --tt-device p300x2: {cmd:?}"
    );
    assert!(
        cmd.windows(2).any(|w| w[0] == "--engine" && w[1] == "vllm"),
        "argv should carry the default --engine vllm: {cmd:?}"
    );
    assert!(
        cmd.iter().any(|a| a == "--docker-server"),
        "argv should carry --docker-server: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--override-docker-image" && w[1] == "some/image:tag"),
        "argv should carry --override-docker-image <image>: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--service-port" && w[1] == "8080"),
        "argv should carry --service-port <port>: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--host-hf-cache" && w[1] == "~/.cache/huggingface"),
        "argv should carry --host-hf-cache <cache> with the configured value: {cmd:?}"
    );
    assert!(
        cmd.iter().any(|a| a == "--no-auth"),
        "argv should carry --no-auth by default: {cmd:?}"
    );
}

/// When auth is required, `--no-auth` must be absent and the returned
/// `Endpoint` must say `requires_key: true`.
#[test]
fn runpy_start_omits_no_auth_and_requires_key_when_auth_required() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("some/image:tag", "127.0.0.1", 8080);
    cfg.no_auth = false;
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    let endpoint = backend.start("llama3").expect("start should succeed");
    assert!(
        endpoint.requires_key,
        "requires_key should be true when auth is required"
    );

    let commands = runner.commands();
    let cmd = &commands[0];
    assert!(
        !cmd.iter().any(|a| a == "--no-auth"),
        "argv should not carry --no-auth when auth is required: {cmd:?}"
    );
}

/// A configured `--device-id` should show up verbatim in the argv.
#[test]
fn runpy_start_includes_device_id_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("some/image:tag", "127.0.0.1", 8080);
    cfg.device_ids = Some("0,1".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = &commands[0];
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--device-id" && w[1] == "0,1"),
        "argv should carry --device-id 0,1 when configured: {cmd:?}"
    );
}

/// Without a configured `device_ids`, `--device-id` must not appear at all.
#[test]
fn runpy_start_omits_device_id_when_not_configured() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = &commands[0];
    assert!(
        !cmd.iter().any(|a| a == "--device-id"),
        "argv should not carry --device-id when unconfigured: {cmd:?}"
    );
}

/// The health poll should actually poll more than once when the first
/// probes report unhealthy.
#[test]
fn runpy_start_polls_health_until_ok() {
    let runner = FakeRunner::new(2); // unhealthy for the first two probes
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8081),
        Box::new(runner),
    )
    .with_health_poll(10, Duration::from_millis(1));

    backend
        .start("llama3")
        .expect("start should eventually succeed");
}

/// If health never comes up within the bounded number of attempts, `start`
/// must return an `Err` rather than hang or silently report success.
#[test]
fn runpy_start_times_out_when_never_healthy() {
    let runner = FakeRunner::new(u32::MAX); // never reports healthy
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8082),
        Box::new(runner),
    )
    .with_health_poll(3, Duration::from_millis(1));

    let err = backend.start("llama3").expect_err("start should time out");
    assert!(err.to_string().contains("llama3"));
}

/// `stop` should query `docker ps --filter publish=<port> -q` then `docker
/// stop <id>` for whatever comes back -- mirroring `start_artgen.sh --stop`
/// -- and reset status back to `Idle`.
#[test]
fn runpy_stop_queries_and_stops_by_publish_port() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "abc123\n");
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    assert_eq!(commands.len(), 2, "expected a ps query then a stop");

    let ps_cmd = &commands[0];
    assert_eq!(ps_cmd[0], "docker");
    assert_eq!(ps_cmd[1], "ps");
    assert!(
        ps_cmd.iter().any(|a| a == "--filter"),
        "docker ps should filter: {ps_cmd:?}"
    );
    assert!(
        ps_cmd.iter().any(|a| a == "publish=8080"),
        "docker ps should filter by publish=<port>: {ps_cmd:?}"
    );

    let stop_cmd = &commands[1];
    assert_eq!(stop_cmd[0], "docker");
    assert_eq!(stop_cmd[1], "stop");
    assert!(
        stop_cmd.iter().any(|a| a == "abc123"),
        "docker stop should target the id returned by docker ps: {stop_cmd:?}"
    );

    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}

/// `stop` when nothing is running (empty `docker ps` output) must not error
/// and must not issue a `docker stop` at all.
#[test]
fn runpy_stop_is_ok_when_nothing_running() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    assert_eq!(
        commands.len(),
        1,
        "expected only the ps query, no stop call: {commands:?}"
    );
    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}
