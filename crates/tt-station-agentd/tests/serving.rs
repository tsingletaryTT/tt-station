//! Integration tests for the `ServingBackend` abstraction (Task 9).
//!
//! `DockerBackend` is exercised entirely through a `FakeRunner` -- no real
//! `docker` binary and no real HTTP server are touched. This proves out the
//! *shape* of the commands/health-probe `DockerBackend` issues (the seam
//! that lets a Mac-side test suite trust the Docker story without a GPU box
//! on hand) without making these tests flaky or slow.
//!
//! The exact argv asserted on here mirrors
//! `docs/reference/tt-inference-server-docker.md` -- the researched, real
//! invocation of `tt-inference-server` -- not a guess.
//!
//! `DstackBackend` is exercised directly since it's a documented stub with
//! no external dependencies to fake.

use std::time::Duration;

use libttstation::model::{Endpoint, ServingStatus};
use tt_station_agentd::serving::docker::{DockerBackend, DockerConfig};
use tt_station_agentd::serving::dstack::DstackBackend;
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// Build a `DockerConfig` with production-shaped defaults, overriding only
/// `image`/`host`/`host_port` -- the three things every test in this file
/// varies. Centralizing this keeps each test focused on the one thing it's
/// actually asserting rather than repeating every config field.
fn config(image: &str, host: &str, host_port: u16) -> DockerConfig {
    DockerConfig {
        image: image.to_string(),
        host: host.to_string(),
        host_port,
        ..Default::default()
    }
}

/// `start` should issue exactly one `docker run` command carrying the real
/// `tt-inference-server` argv -- `--device`, `--tt-device`, `--publish
/// <host>:8000` (the container always listens on 8000, regardless of the
/// host port), the image, and `--model <model>` -- poll `/health` until OK,
/// flip internal status to `Serving`, and return the expected `Endpoint`.
#[test]
fn docker_start_issues_run_command_and_returns_endpoint() {
    let runner = FakeRunner::new(0); // healthy on the very first probe
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
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
        run_cmd.iter().any(|a| a == "--device"),
        "docker run args should carry --device: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "/dev/tenstorrent"),
        "docker run args should pass through the default device path: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "--tt-device"),
        "docker run args should carry --tt-device: {run_cmd:?}"
    );
    assert!(
        run_cmd
            .windows(2)
            .any(|w| w[0] == "--tt-device" && w[1] == DockerConfig::default().tt_device),
        "docker run args should carry the configured --tt-device value: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "--model"),
        "docker run args should carry --model: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "llama3"),
        "docker run args should mention the model: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "--publish"),
        "docker run args should carry --publish: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "8080:8000"),
        "docker run args should map the host port onto the container's fixed port 8000: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "some/image:tag"),
        "docker run args should carry the configured image: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "--no-auth"),
        "docker run args should carry --no-auth by default: {run_cmd:?}"
    );
}

/// A model id with a `/` (org/model-style Hugging Face ids) must still be
/// passed RAW to `--model` -- the server inside the container needs the
/// real model id to know what to load -- even though the derived `--name`
/// is sanitized to satisfy Docker's container-name character rules.
#[test]
fn docker_start_keeps_raw_model_in_argv_but_sanitizes_container_name() {
    let runner = FakeRunner::new(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    let model = "meta-llama/Llama-3.1-8B";
    backend.start(model).expect("start should succeed");

    let commands = runner.commands();
    let run_cmd = &commands[0];

    let name_idx = run_cmd
        .iter()
        .position(|a| a == "--name")
        .expect("--name flag should be present");
    let container_name = &run_cmd[name_idx + 1];
    assert!(
        !container_name.contains('/'),
        "container name must not contain '/': {container_name}"
    );

    let model_idx = run_cmd
        .iter()
        .position(|a| a == "--model")
        .expect("--model flag should be present");
    assert_eq!(
        run_cmd[model_idx + 1],
        model,
        "--model should carry the ORIGINAL, unsanitized model id"
    );
}

/// A configured HF token should show up as `--env HF_TOKEN=<token>` -- only
/// needed for gated Hugging Face repos, so it must not appear when unset
/// (see the sibling test below).
#[test]
fn docker_start_includes_hf_token_env_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("some/image:tag", "127.0.0.1", 8080);
    cfg.hf_token = Some("secret-token".to_string());
    let backend = DockerBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let run_cmd = &commands[0];
    assert!(
        run_cmd.iter().any(|a| a == "--env"),
        "docker run args should carry --env when a token is configured: {run_cmd:?}"
    );
    assert!(
        run_cmd.iter().any(|a| a == "HF_TOKEN=secret-token"),
        "docker run args should carry the HF_TOKEN value: {run_cmd:?}"
    );
}

/// Without a configured token, no `--env`/`HF_TOKEN` should appear at all --
/// the PoC shouldn't ship an empty/placeholder token into the container.
#[test]
fn docker_start_omits_hf_token_env_when_not_configured() {
    let runner = FakeRunner::new(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let run_cmd = &commands[0];
    assert!(
        !run_cmd.iter().any(|a| a.starts_with("HF_TOKEN=")),
        "docker run args should not carry HF_TOKEN when no token is configured: {run_cmd:?}"
    );
    assert!(
        !run_cmd.iter().any(|a| a == "--env"),
        "docker run args should not carry --env when no token is configured: {run_cmd:?}"
    );
}

/// When auth is required (`no_auth: false`), `--no-auth` must be absent from
/// the argv and the returned `Endpoint` must say `requires_key: true`.
#[test]
fn docker_start_requires_key_and_omits_no_auth_when_auth_required() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("some/image:tag", "127.0.0.1", 8080);
    cfg.no_auth = false;
    let backend = DockerBackend::new(cfg, Box::new(runner.clone()));

    let endpoint = backend.start("llama3").expect("start should succeed");
    assert!(
        endpoint.requires_key,
        "requires_key should be true when auth is required"
    );

    let commands = runner.commands();
    let run_cmd = &commands[0];
    assert!(
        !run_cmd.iter().any(|a| a == "--no-auth"),
        "docker run args should not carry --no-auth when auth is required: {run_cmd:?}"
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
fn docker_start_times_out_when_never_healthy() {
    let runner = FakeRunner::new(u32::MAX); // never reports healthy
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8082),
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
        config("some/image:tag", "127.0.0.1", 8080),
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
