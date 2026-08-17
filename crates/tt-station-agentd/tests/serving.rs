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
    assert_eq!(run_cmd[0], "docker");
    assert_eq!(run_cmd[1], "run");

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

    // Gap flagged in review: the shared-memory, hugepages-mount, and
    // cache-volume flags were built by `DockerBackend::start` but never
    // actually asserted on here -- easy for a future edit to silently break
    // any of the three without a single test noticing. Pin down the EXACT
    // values (not just "some --ipc flag exists"), matching
    // docs/reference/tt-inference-server-docker.md's canonical invocation.
    assert!(
        run_cmd
            .windows(2)
            .any(|w| w[0] == "--ipc" && w[1] == "host"),
        "docker run args should carry --ipc host: {run_cmd:?}"
    );
    assert!(
        run_cmd
            .iter()
            .any(|a| a == "type=bind,src=/dev/hugepages-1G,dst=/dev/hugepages-1G"),
        "docker run args should carry the exact hugepages --mount spec: {run_cmd:?}"
    );
    assert!(
        run_cmd
            .iter()
            .any(|a| a == "tt-station-cache:/home/container_app_user/cache_root"),
        "docker run args should carry the exact cache --volume spec: {run_cmd:?}"
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
    assert_eq!(commands[0][0], "docker");
    assert_eq!(commands[0][1], "stop");
    assert!(
        commands[0].iter().any(|a| a.contains("llama3")),
        "docker stop args should mention the model: {:?}",
        commands[0]
    );

    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}

/// `stop` must be idempotent even when the underlying `docker stop` command
/// fails -- exactly what happens on a real box when `docker stop` targets an
/// already-stopped or missing container (it exits non-zero). This is a
/// documented contract on `ServingBackend::stop` (see its trait doc in
/// `serving/mod.rs`): "docker stop on an already-stopped/missing container
/// is not treated as an error by `DockerBackend`". `routes.rs::stop_model`
/// now calls `backend.stop` UNCONDITIONALLY (even while idle, so a `/stop`
/// can cancel an in-flight `/run`) -- if `stop` propagated a `docker stop`
/// failure, an operator hitting Stop while idle (or a client retrying after
/// a timeout) would see a 500 instead of a harmless no-op.
#[test]
fn docker_stop_is_idempotent_when_docker_stop_fails() {
    let runner = FakeRunner::new(0);
    runner.fail_run(
        "docker stop",
        "Error: No such container: tt-inference-llama3",
    );
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend
        .stop("llama3")
        .expect("stop must be idempotent: a failing docker stop is not an error");

    assert_eq!(
        backend.status().unwrap(),
        ServingStatus::Idle,
        "status should still flip to Idle even when docker stop fails"
    );
}

// ---------------------------------------------------------------------
// gozer leasing (Task 3): `DockerBackend::start` takes a chip lease before
// it touches any chips, pins the container to the granted devices with one
// `--device /dev/tenstorrent/<n>` per chip, and releases on EVERY exit --
// stopping the container it launched FIRST, because `gozer release` resets
// the released chips. With no gozer capability attached (the default, and
// what a box without gozer installed gets), every path must behave exactly
// as it did before leasing existed. See
// `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`.
// ---------------------------------------------------------------------

/// A `gozer acquire --json` grant payload, shaped exactly like gozer's own
/// `cmd_acquire` emits it: two chips of one expanded board, with BDFs in
/// `chips` and the `/dev/tenstorrent/<n>` device indices in `dev_indices`.
/// Deliberately the SAME fixture `tests/runpy.rs` uses, so both backends are
/// proven against one grant shape rather than two hand-tuned ones.
const GRANT_JSON: &str = r#"{"granted":true,"lease_id":"lease-abc123",
    "units":["0100014311601055"],
    "chips":["0000:01:00.0","0000:02:00.0"],
    "dev_indices":[2,3],
    "expanded":true,"requested":1,"neighbours":[],"owner_pid":4242}"#;

/// A `gozer status --json` payload with one board fully CLAIMED by another
/// tenant -- what agentd cross-references to name the holder when `acquire`
/// comes back unavailable (that payload names nobody itself).
const STATUS_JSON_HELD: &str = r#"{"grain":"board","chips":[
    {"dev_index":0,"bdf":"0000:01:00.0","board":"0100014311601055","card":"p300",
     "state":"CLAIMED","who":"claude:ttm-optimize","pid":9001,
     "reason":"bringup","pids_holding":[9001],"overstayed":false}],
    "queue":[]}"#;

/// `tt-smi -s` JSON fixture for THIS box: 4x `p300c` boards (one BDF per
/// ASIC, two per physical p300 card), verified on real hardware to map to
/// `p300x2`. Two of them -- what `GRANT_JSON` grants -- is one board,
/// `p300`. Same fixture `tests/runpy.rs` uses.
const TT_SMI_FOUR_P300C: &str = r#"{
    "device_info": [
        {"board_info": {"board_type": "p300c"}},
        {"board_info": {"board_type": "p300c"}},
        {"board_info": {"board_type": "p300c"}},
        {"board_info": {"board_type": "p300c"}}
    ]
}"#;

/// The capability a successful startup probe would have produced (see
/// `gozer::probe`). Handed to `DockerBackend::with_gozer` directly -- the
/// probe itself is covered in `tests/gozer.rs`, and the PRODUCTION wiring
/// that carries a probed capability into the backend `main.rs` actually
/// builds is covered by `make_backend`'s own tests plus `main.rs`'s
/// `build_serving_backend` test.
fn gozer_capability() -> tt_station_agentd::gozer::Capability {
    tt_station_agentd::gozer::Capability {
        path: "gozer".to_string(),
        version: "gozer 0.1.0".to_string(),
    }
}

/// A `FakeRunner` whose `gozer` verbs all answer successfully: `acquire`
/// grants `GRANT_JSON`, `release` reports released. NOTHING here spawns a
/// real gozer -- the whole point of the `CommandRunner` seam.
fn leasing_runner(health_calls_before_ok: u32) -> FakeRunner {
    let runner = FakeRunner::new(health_calls_before_ok);
    // A leased serve DERIVES `--tt-device` from the grant against this box's
    // real four-`p300c` snapshot, and fails closed without it (see
    // `leased_tt_device`). The 2-chip `GRANT_JSON` against a 4-chip box
    // therefore yields `p300` -- one board.
    runner.set_run_output("tt-smi -s", TT_SMI_FOUR_P300C);
    runner.set_run_capturing("gozer acquire", 0, GRANT_JSON, "");
    runner.set_run_capturing(
        "gozer release",
        0,
        r#"{"released":true,"message":"released lease-abc123"}"#,
        "",
    );
    runner
}

/// Position of the first `gozer <verb>` invocation among `commands`.
fn gozer_index(commands: &[Vec<String>], verb: &str) -> Option<usize> {
    commands.iter().position(|cmd| {
        cmd.first().map(String::as_str) == Some("gozer")
            && cmd.get(1).map(String::as_str) == Some(verb)
    })
}

/// How many `gozer <verb>` invocations are among `commands`.
fn gozer_count(commands: &[Vec<String>], verb: &str) -> usize {
    commands
        .iter()
        .filter(|cmd| {
            cmd.first().map(String::as_str) == Some("gozer")
                && cmd.get(1).map(String::as_str) == Some(verb)
        })
        .count()
}

/// Position of the first `docker <subcommand>` invocation among `commands`.
fn docker_index(commands: &[Vec<String>], subcommand: &str) -> Option<usize> {
    commands.iter().position(|cmd| {
        cmd.first().map(String::as_str) == Some("docker")
            && cmd.get(1).map(String::as_str) == Some(subcommand)
    })
}

/// How many `docker <subcommand>` invocations are among `commands`. A COUNT,
/// not just a presence check: `start`'s error paths and `stop` both emit a
/// byte-identical `docker stop <name>`, so "a stop happened" cannot
/// distinguish one from two.
fn docker_count(commands: &[Vec<String>], subcommand: &str) -> usize {
    commands
        .iter()
        .filter(|cmd| {
            cmd.first().map(String::as_str) == Some("docker")
                && cmd.get(1).map(String::as_str) == Some(subcommand)
        })
        .count()
}

/// The `docker run` argv a DEFAULT, UNLEASED `DockerBackend::start` builds --
/// pinned byte for byte, in order.
///
/// gozer is optional, and "optional" has to mean *nothing changes* on a box
/// without it: not the flags, not their values, not their order. The
/// assertions further up this file each check one flag; this one is the
/// whole vector at once, so a leasing change that quietly inserts, drops, or
/// reorders an argument on the no-gozer path fails here even if every
/// individual flag assertion still passes.
fn unleased_docker_run_argv(image: &str, host_port: u16, model: &str) -> Vec<String> {
    let defaults = DockerConfig::default();
    [
        "docker",
        "run",
        "-d",
        "--rm",
        "--name",
        &format!("tt-inference-{model}"),
        "--ipc",
        "host",
        "--device",
        "/dev/tenstorrent",
        "--mount",
        "type=bind,src=/dev/hugepages-1G,dst=/dev/hugepages-1G",
        "--volume",
        "tt-station-cache:/home/container_app_user/cache_root",
        "--publish",
        &format!("{host_port}:8000"),
        image,
        "--model",
        model,
        "--tt-device",
        &defaults.tt_device,
        "--no-auth",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// With NO gozer capability (the default), the `docker run` argv must be
/// byte-identical to what it has always been, and not a single `gozer`
/// subprocess may be spawned. This is the contract that makes gozer
/// genuinely optional.
#[test]
fn docker_start_unleased_argv_is_byte_identical() {
    let runner = FakeRunner::new(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    assert_eq!(
        commands.len(),
        1,
        "an unleased start must issue exactly one command: {commands:?}"
    );
    assert_eq!(
        commands[0],
        unleased_docker_run_argv("some/image:tag", 8080, "llama3"),
        "the unleased docker run argv must not change at all"
    );
    assert!(
        !commands.iter().any(|cmd| cmd[0] == "gozer"),
        "no gozer subprocess may be spawned without a capability: {commands:?}"
    );
}

/// With a lease, the container must be pinned to the granted chips: ONE
/// `--device /dev/tenstorrent/<n>` per `dev_index`, and never the
/// whole-directory `/dev/tenstorrent` the unleased default passes (which
/// would hand the container every chip on the box, including the
/// neighbour's).
#[test]
fn docker_start_emits_one_device_flag_per_leased_chip() {
    let runner = leasing_runner(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let run_cmd = &commands[docker_index(&commands, "run").expect("expected a docker run")];

    let device_values: Vec<&String> = run_cmd
        .windows(2)
        .filter(|w| w[0] == "--device")
        .map(|w| &w[1])
        .collect();
    assert_eq!(
        device_values,
        vec!["/dev/tenstorrent/2", "/dev/tenstorrent/3"],
        "a leased container must get one --device per granted chip, in grant \
         order: {run_cmd:?}"
    );
    // A COUNT, not just "the right values appear": a stray extra --device
    // (e.g. the configured whole-directory one left in alongside the leased
    // ones) would still satisfy a contains-check.
    assert_eq!(
        run_cmd.iter().filter(|a| *a == "--device").count(),
        2,
        "exactly two --device flags, one per granted chip: {run_cmd:?}"
    );
    assert!(
        !run_cmd.iter().any(|a| a == "/dev/tenstorrent"),
        "the whole-directory device path must NOT survive a lease -- it would \
         hand the container every chip on the box: {run_cmd:?}"
    );
}

/// The lease must be taken BEFORE `docker run`: everything after the acquire
/// touches chips, and a container started before the gate was asked is a
/// container running on chips nobody leased.
#[test]
fn docker_start_acquires_lease_before_docker_run() {
    let runner = leasing_runner(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let acquire_index = gozer_index(&commands, "acquire")
        .unwrap_or_else(|| panic!("expected a gozer acquire invocation: {commands:?}"));
    let run_index = docker_index(&commands, "run")
        .unwrap_or_else(|| panic!("expected a docker run invocation: {commands:?}"));
    assert_eq!(
        acquire_index, 0,
        "acquire must be the FIRST thing start does: {commands:?}"
    );
    assert!(
        acquire_index < run_index,
        "acquire ({acquire_index}) must precede docker run ({run_index}): {commands:?}"
    );

    let acquire = &commands[acquire_index];
    let pid = std::process::id().to_string();
    assert!(
        acquire
            .windows(2)
            .any(|w| w[0] == "--owner-pid" && w[1] == pid),
        "acquire must pass --owner-pid <agentd's own pid>, or gozer reaps the \
         lease out from under a live serve: {acquire:?}"
    );
    let who = acquire
        .windows(2)
        .find(|w| w[0] == "--who")
        .map(|w| w[1].clone())
        .expect("acquire must identify the lease holder with --who");
    // The `who` FORMAT is a shared contract with `RunPyBackend`, not a
    // per-backend detail: the design doc's startup sweep filters leases on
    // the `tt-station:` prefix and then extracts the SERVICE PORT from the
    // next field. A docker-specific shape (the container name, which this
    // backend does know at acquire time, unlike runpy) would make that one
    // rule two and leave crashed-agent leases unreclaimable.
    assert_eq!(
        who, "tt-station:8080:llama3",
        "--who must be tt-station:<published-port>:<model>, the same key the \
         startup sweep extracts a port from for either backend"
    );
    // A successful serve KEEPS its lease -- the chips stay in use until stop.
    assert!(
        gozer_index(&commands, "release").is_none(),
        "a healthy serve must NOT release its lease: {commands:?}"
    );
}

/// When the chips are held by somebody else, `start` must fail BEFORE it
/// runs a container, and must name the holder (`acquire`'s own payload names
/// nobody, so the holder comes from `gozer status --json`).
#[test]
fn docker_start_fails_without_running_a_container_when_chips_are_unavailable() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "gozer acquire",
        12,
        r#"{"granted":false,"queued":false}"#,
        "",
    );
    runner.set_run_capturing("gozer status", 0, STATUS_JSON_HELD, "");
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    let err = backend
        .start("llama3")
        .expect_err("start must fail when no chips are available");
    assert!(
        err.to_string().contains("claude:ttm-optimize"),
        "the error should name who holds the chips: {err}"
    );

    let commands = runner.commands();
    assert!(
        docker_index(&commands, "run").is_none(),
        "no container may be started when the lease was refused: {commands:?}"
    );
}

/// THE DANGEROUS ONE. When the health poll times out, the container this
/// `start` launched is still alive -- and returning drops the lease guard,
/// whose `gozer release` RESETS the released chips. Stopping the container
/// must therefore happen BEFORE the release, or a real `tt-smi -r` lands on
/// a live workload and those chips are then advertised FREE to the next
/// tenant.
#[test]
fn docker_start_stops_the_container_before_releasing_on_health_timeout() {
    let runner = leasing_runner(u32::MAX); // never reports healthy
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()))
    .with_health_poll(2, Duration::from_millis(1));

    backend
        .start("llama3")
        .expect_err("start should time out when never healthy");

    let commands = runner.commands();
    let stop_index = docker_index(&commands, "stop")
        .unwrap_or_else(|| panic!("expected a docker stop before the release: {commands:?}"));
    let release_index = gozer_index(&commands, "release")
        .unwrap_or_else(|| panic!("expected the lease to be released: {commands:?}"));
    assert!(
        stop_index < release_index,
        "the container stop ({stop_index}) must precede the release \
         ({release_index}), which resets the chips: {commands:?}"
    );
    assert_eq!(
        commands[stop_index][2], "tt-inference-llama3",
        "the stop must name the container this start launched: {commands:?}"
    );
    assert_eq!(
        docker_count(&commands, "stop"),
        1,
        "exactly one container stop on this path: {commands:?}"
    );
}

/// Same hazard, earlier exit: `docker run` itself failing can still leave a
/// container behind (it may have been created and then failed to stay up),
/// and this exit also drops the lease guard. Stop first, release second.
#[test]
fn docker_start_stops_the_container_before_releasing_when_docker_run_fails() {
    let runner = leasing_runner(0);
    runner.fail_run("docker run", "Error response from daemon: boom");
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend
        .start("llama3")
        .expect_err("start should fail when docker run fails");

    let commands = runner.commands();
    let stop_index = docker_index(&commands, "stop")
        .unwrap_or_else(|| panic!("expected a docker stop before the release: {commands:?}"));
    let release_index = gozer_index(&commands, "release")
        .unwrap_or_else(|| panic!("expected the lease to be released: {commands:?}"));
    assert!(
        stop_index < release_index,
        "the container stop ({stop_index}) must precede the release \
         ({release_index}), which resets the chips: {commands:?}"
    );
    // A COUNT, like its sibling above: this path and `stop` emit a
    // byte-identical `docker stop <name>`, so "a stop happened" cannot tell
    // one from two.
    assert_eq!(
        docker_count(&commands, "stop"),
        1,
        "exactly one container stop on this path: {commands:?}"
    );
}

/// The stop above is gated on actually HOLDING a lease, not on gozer merely
/// being installed: it exists to protect the release, so it depends on the
/// thing it protects. Without a lease the unleased behaviour must stay
/// byte-identical to what it has always been -- a timed-out bring-up leaves
/// its container for the operator to inspect, and no `docker stop` is
/// issued. (Whether the unleased path SHOULD stop it too is a separate
/// question, not a side effect of this change.)
#[test]
fn docker_start_unleased_health_timeout_still_issues_no_stop() {
    let runner = FakeRunner::new(u32::MAX);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_health_poll(2, Duration::from_millis(1));

    backend.start("llama3").expect_err("start should time out");

    let commands = runner.commands();
    assert_eq!(
        commands.len(),
        1,
        "an unleased timeout must still issue exactly the one docker run: {commands:?}"
    );
    assert_eq!(docker_count(&commands, "stop"), 0);
}

/// `stop` releases the lease -- but only AFTER the container is actually
/// stopped, for the same reset-under-a-live-container reason. (This is also
/// why `POST /stop` needs no separate reset verb: the release does it.)
#[test]
fn docker_stop_releases_the_lease_after_stopping_the_container() {
    let runner = leasing_runner(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend.start("llama3").expect("start should succeed");
    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    let stop_index = docker_index(&commands, "stop").expect("expected a docker stop");
    let release_index = gozer_index(&commands, "release").expect("expected a gozer release");
    assert!(
        stop_index < release_index,
        "the container stop ({stop_index}) must precede the release \
         ({release_index}): {commands:?}"
    );
    assert_eq!(
        commands[release_index][2], "lease-abc123",
        "the release must name the lease the grant handed back: {commands:?}"
    );
    // Idempotent: a second stop has nothing left to release, and must not
    // re-release a lease id that is already gone (gozer would answer exit 13
    // "no such lease", and a lease id can be REUSED by another tenant's
    // acquire in between). A COUNT, because both stops emit a
    // byte-identical `docker stop`, so "a release happened" cannot tell one
    // from two.
    backend.stop("llama3").expect("stop should be idempotent");
    let after = runner.commands();
    assert_eq!(
        gozer_count(&after, "release"),
        1,
        "exactly one release across two stops: {after:?}"
    );
    assert_eq!(
        docker_count(&after, "stop"),
        2,
        "both stops must still stop the container: {after:?}"
    );
}

/// THE DANGEROUS ONE, PART TWO. `stop` must stop the container the LEASE
/// BELONGS TO -- not whatever container name its `model` argument happens to
/// derive -- before releasing.
///
/// `routes.rs::stop_model` passes `state.current_model().unwrap_or_default()`,
/// i.e. **the empty string** whenever `AppState` has no recorded model. That
/// is reachable with no race at all: `POST /run llama3` (container up, lease
/// held), then `POST /reset` (which does NOT stop a `DockerBackend`
/// container -- the trait's default `reset` is a no-op -- but DOES
/// `set_idle()`), then `POST /stop`. With the model-derived name, `docker
/// stop tt-inference-` fails, is swallowed as "nothing to stop", and the
/// release then resets the chips `tt-inference-llama3` is still driving,
/// advertising them FREE to the next tenant while an orphaned root-owned
/// container keeps using them -- which gozer's `still_open` check cannot
/// see.
///
/// `RunPyBackend` is immune by construction: its `stop` ignores the model
/// and sweeps by published port, so its stop cannot miss. This backend has
/// to record what it launched.
#[test]
fn docker_stop_stops_the_leases_own_container_even_when_given_no_model() {
    let runner = leasing_runner(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend.start("llama3").expect("start should succeed");
    // Exactly what routes.rs passes after a /reset cleared the model.
    backend.stop("").expect("stop should succeed");

    let commands = runner.commands();
    let stop_index = commands
        .iter()
        .position(|cmd| {
            cmd.first().map(String::as_str) == Some("docker")
                && cmd.get(1).map(String::as_str) == Some("stop")
                && cmd.get(2).map(String::as_str) == Some("tt-inference-llama3")
        })
        .unwrap_or_else(|| {
            panic!(
                "stop must stop the container the lease belongs to \
                 (tt-inference-llama3), whatever model argument it was given: {commands:?}"
            )
        });
    let release_index = gozer_index(&commands, "release")
        .unwrap_or_else(|| panic!("expected the lease to be released: {commands:?}"));
    assert!(
        stop_index < release_index,
        "the lease's own container must be stopped ({stop_index}) before the \
         release ({release_index}) resets its chips: {commands:?}"
    );
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.get(2).map(String::as_str) == Some("tt-inference-")),
        "the empty model must not produce a bogus container name to stop: {commands:?}"
    );
}

/// A leased serve must name the mesh it actually LEASED, not the whole box.
///
/// `DockerConfig::tt_device` is a plain `String` with a production default of
/// `p300x2` (two boards) while `DEFAULT_LEASE_CHIPS` is `"1"` (one board), so
/// passing it verbatim under a lease describes a mesh wider than the lease on
/// every real box. The cgroup means the container physically cannot open the
/// neighbour's board, so this is not a cross-tenant bug -- it is a serve that
/// asks for hardware it was not given and then fails to come up, looking like
/// a flaky bring-up rather than a misconfiguration.
#[test]
fn docker_start_tt_device_describes_the_lease_not_the_box() {
    let runner = leasing_runner(0);
    let mut cfg = config("some/image:tag", "127.0.0.1", 8080);
    // A configured whole-box mesh that must LOSE to the leased shape.
    cfg.tt_device = "p300x2".to_string();
    let backend =
        DockerBackend::new(cfg, Box::new(runner.clone())).with_gozer(Some(gozer_capability()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let run_cmd = &commands[docker_index(&commands, "run").expect("expected a docker run")];
    assert!(
        run_cmd
            .windows(2)
            .any(|w| w[0] == "--tt-device" && w[1] == "p300"),
        "a 2-chip grant on a 4x p300c box is ONE board (p300), not the \
         configured whole-box p300x2: {run_cmd:?}"
    );
    assert!(
        !run_cmd.iter().any(|a| a == "p300x2"),
        "the configured whole-box mesh must not survive a lease: {run_cmd:?}"
    );
}

/// Fails CLOSED, like `leased_device_paths` and `Grant::reset_target`: if the
/// leased mesh can't be derived (unreadable `tt-smi -s`), refuse the serve
/// rather than fall back to the configured whole-box value. Omitting the flag
/// isn't a safe fallback either -- the container's own detection looks at
/// what it can see, and naming a mesh wider than the lease is the thing being
/// avoided.
#[test]
fn docker_start_refuses_to_serve_when_the_leased_mesh_cannot_be_derived() {
    let runner = leasing_runner(0);
    runner.fail_run("tt-smi -s", "tt-smi: command not found");
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend
        .start("llama3")
        .expect_err("an underivable leased mesh must not serve");

    let commands = runner.commands();
    assert!(
        docker_index(&commands, "run").is_none(),
        "no container may be started when the leased mesh is unknown: {commands:?}"
    );
    assert!(
        gozer_index(&commands, "release").is_some(),
        "the unusable lease must be handed straight back: {commands:?}"
    );
}

/// With NO gozer capability, `stop` must behave exactly as before: one
/// `docker stop`, no gozer subprocess.
#[test]
fn docker_stop_unleased_issues_no_gozer_call() {
    let runner = FakeRunner::new(0);
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    );

    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert!(!commands.iter().any(|cmd| cmd[0] == "gozer"));
}

/// A grant that names no device indices must REFUSE the serve rather than
/// fall back to the configured whole-directory `--device /dev/tenstorrent`.
/// Same fail-closed reasoning as `Grant::reset_target`'s empty-grant refusal:
/// silently degrading to "every chip on the box" is the worst possible
/// interpretation of "run only on what I leased".
#[test]
fn docker_start_refuses_a_grant_with_no_device_indices() {
    let runner = FakeRunner::new(0);
    runner.set_run_capturing(
        "gozer acquire",
        0,
        r#"{"granted":true,"lease_id":"lease-empty","chips":["0000:01:00.0"],"dev_indices":[]}"#,
        "",
    );
    runner.set_run_capturing("gozer release", 0, r#"{"released":true}"#, "");
    let backend = DockerBackend::new(
        config("some/image:tag", "127.0.0.1", 8080),
        Box::new(runner.clone()),
    )
    .with_gozer(Some(gozer_capability()));

    backend
        .start("llama3")
        .expect_err("a grant with no device indices must not serve");

    let commands = runner.commands();
    assert!(
        docker_index(&commands, "run").is_none(),
        "no container may be started from an unusable grant: {commands:?}"
    );
    assert!(
        gozer_index(&commands, "release").is_some(),
        "the unusable lease must be handed straight back: {commands:?}"
    );
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

/// Leasing is an explicit NO-OP on the dstack stub, and `make_backend` must
/// not pretend otherwise: handing it a gozer capability changes nothing.
///
/// This is a real assertion, not a formality. `make_backend` now threads a
/// probed capability into every backend it can build; if dstack ever grew a
/// silent `with_gozer` that acquired a lease, the stub would take chips it
/// can't use and never release them (its `start` fails before anything runs,
/// and it has no `CommandRunner` to release through). See the "leasing is a
/// deliberate no-op" note in `serving/dstack.rs`.
#[test]
fn dstack_ignores_a_gozer_capability() {
    let backend = tt_station_agentd::serving::make_backend(
        "dstack",
        DockerConfig::default(),
        tt_station_agentd::serving::runpy::RunPyConfig::default(),
        Some(gozer_capability()),
    )
    .expect("dstack backend should construct");

    assert_eq!(
        backend.gozer_capability(),
        None,
        "the dstack stub must hold no capability: it can neither use nor \
         release a lease"
    );
    assert!(backend.start("llama3").is_err());
    assert!(backend.stop("llama3").is_ok());
}
