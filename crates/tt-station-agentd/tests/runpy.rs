//! Integration tests for `RunPyBackend` -- the default serving backend,
//! which launches LLMs the way the operator's PROVEN scripts do: via
//! `tt-inference-server/run.py`, not a hand-rolled `docker run`. See
//! `docs/reference/tt-inference-server-docker.md`'s "⭐ Ground truth: launch
//! via run.py" section for the validated invocation this mirrors.
//!
//! The central behavior under test: `run.py` itself auto-resolves the
//! device mesh (`--tt-device` "Defaults to the largest supported device
//! available on the host"), the serving image (`--override-docker-image` is
//! an OVERRIDE, not a requirement), and `--impl`/`--engine` (default to the
//! model spec) -- so `RunPyBackend::start`'s DEFAULT invocation must NOT
//! guess values for any of them. Each is only appended when a caller
//! explicitly configures it (`Some(..)`), proven below by the
//! `*_overrides_when_configured` tests alongside the default-omission test.
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
/// `host`/`service_port` -- the two fields most tests in this file vary.
/// Every device/image/impl/engine field starts `None` (the real default --
/// see `RunPyConfig::default`'s doc comment).
fn config(host: &str, service_port: u16) -> RunPyConfig {
    RunPyConfig {
        host: host.to_string(),
        service_port,
        ..Default::default()
    }
}

/// Find the `python3 run.py ...` invocation among `commands` -- since
/// `RunPyConfig::reset_before_serve` defaults to `true`, `start` now issues
/// a `tt-smi -r` board reset (see `reset_before_serve_*` tests below)
/// BEFORE the run.py command, so tests that only care about the run.py
/// argv itself must look it up by content rather than assuming it's
/// `commands[0]`.
fn find_runpy_cmd(commands: &[Vec<String>]) -> &Vec<String> {
    commands
        .iter()
        .find(|cmd| cmd.first().map(String::as_str) == Some("python3"))
        .expect("expected a python3 run.py invocation among the recorded commands")
}

/// The DEFAULT `start` invocation -- no device/image/impl/engine override
/// configured -- must be the MINIMAL `run.py` command: `--model`,
/// `--workflow server`, `--docker-server`, `--service-port`, plus
/// `--no-auth` (default-on) and `--host-hf-cache` (the one non-hardware
/// default this codebase still sets, see `RunPyConfig::default`). It must
/// NOT carry `--tt-device`, `--override-docker-image`, `--impl`, or
/// `--engine` -- that's the whole point: `run.py` resolves all four itself
/// from `model_spec.json` and detected hardware, and hardcoding/guessing a
/// value here would just be a worse, staler copy of that resolution.
#[test]
fn runpy_start_default_omits_device_image_impl_engine() {
    let runner = FakeRunner::new(0); // healthy on the very first probe
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

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
    assert_eq!(
        commands.len(),
        3,
        "expected the stop-stale docker-ps query, the default board-reset \
         command, and the run.py invocation: {commands:?}"
    );
    let cmd = find_runpy_cmd(&commands);

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
        cmd.iter().any(|a| a == "--docker-server"),
        "argv should carry --docker-server: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--service-port" && w[1] == "8080"),
        "argv should carry --service-port <port>: {cmd:?}"
    );
    assert!(
        cmd.iter().any(|a| a == "--no-auth"),
        "argv should carry --no-auth by default: {cmd:?}"
    );
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--host-hf-cache" && w[1] == "~/.cache/huggingface"),
        "argv should carry --host-hf-cache <cache> with the configured value: {cmd:?}"
    );

    // The whole point of this change: none of these should be present.
    assert!(
        !cmd.iter().any(|a| a == "--tt-device"),
        "DEFAULT argv must NOT carry --tt-device -- run.py auto-detects the \
         largest supported device on the host: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--override-docker-image"),
        "DEFAULT argv must NOT carry --override-docker-image -- run.py \
         resolves the image from model_spec.json itself: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--impl"),
        "DEFAULT argv must NOT carry --impl -- run.py defaults it to the \
         model spec: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--engine"),
        "DEFAULT argv must NOT carry --engine -- run.py defaults it to the \
         model spec: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "--device-id"),
        "DEFAULT argv must NOT carry --device-id when unconfigured: {cmd:?}"
    );
}

/// Setting `tt_device` must make `--tt-device <value>` appear -- an
/// explicit OVERRIDE of run.py's own hardware auto-detection.
#[test]
fn runpy_start_includes_tt_device_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.tt_device = Some("p300x2".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--tt-device" && w[1] == "p300x2"),
        "argv should carry --tt-device p300x2 when configured: {cmd:?}"
    );
}

/// Setting `image` must make `--override-docker-image <value>` appear.
#[test]
fn runpy_start_includes_override_docker_image_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.image = Some("some/image:tag".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--override-docker-image" && w[1] == "some/image:tag"),
        "argv should carry --override-docker-image when configured: {cmd:?}"
    );
}

/// Setting `impl_name` must make `--impl <value>` appear.
#[test]
fn runpy_start_includes_impl_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.impl_name = Some("tt-transformers".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--impl" && w[1] == "tt-transformers"),
        "argv should carry --impl when configured: {cmd:?}"
    );
}

/// Setting `engine` must make `--engine <value>` appear.
#[test]
fn runpy_start_includes_engine_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.engine = Some("vllm".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2).any(|w| w[0] == "--engine" && w[1] == "vllm"),
        "argv should carry --engine when configured: {cmd:?}"
    );
}

/// When auth is required, `--no-auth` must be absent and the returned
/// `Endpoint` must say `requires_key: true`.
#[test]
fn runpy_start_omits_no_auth_and_requires_key_when_auth_required() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.no_auth = false;
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    let endpoint = backend.start("llama3").expect("start should succeed");
    assert!(
        endpoint.requires_key,
        "requires_key should be true when auth is required"
    );

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        !cmd.iter().any(|a| a == "--no-auth"),
        "argv should not carry --no-auth when auth is required: {cmd:?}"
    );
}

/// `--host-hf-cache` must not appear at all when unconfigured (`None`).
#[test]
fn runpy_start_omits_host_hf_cache_when_not_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.host_hf_cache = None;
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        !cmd.iter().any(|a| a == "--host-hf-cache"),
        "argv should not carry --host-hf-cache when unconfigured: {cmd:?}"
    );
}

/// A configured `device_id` should show up verbatim in the argv.
#[test]
fn runpy_start_includes_device_id_when_configured() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.device_id = Some("0,1".to_string());
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--device-id" && w[1] == "0,1"),
        "argv should carry --device-id 0,1 when configured: {cmd:?}"
    );
}

/// Without a configured `device_id`, `--device-id` must not appear at all.
#[test]
fn runpy_start_omits_device_id_when_not_configured() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        !cmd.iter().any(|a| a == "--device-id"),
        "argv should not carry --device-id when unconfigured: {cmd:?}"
    );
}

// ---------------------------------------------------------------------
// `reset_before_serve`: `tt-smi -r` clears wedged mesh ethernet cores left
// by a previously-stopped/crashed model BEFORE launching a new one. See
// the module doc in `src/serving/runpy.rs`.
// ---------------------------------------------------------------------

/// Default config (`reset_before_serve: true`) must issue the reset
/// command as the FIRST recorded `run` call, strictly before the `python3
/// run.py ...` invocation -- ordering matters here, not just presence,
/// since a reset that ran (say) after run.py would be useless.
#[test]
fn runpy_start_resets_board_before_launching_runpy_by_default() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    let reset_index = commands
        .iter()
        .position(|cmd| cmd.first().map(String::as_str) == Some("tt-smi"))
        .expect("expected a tt-smi reset command among the recorded commands");
    let runpy_index = commands
        .iter()
        .position(|cmd| cmd.first().map(String::as_str) == Some("python3"))
        .expect("expected a python3 run.py invocation among the recorded commands");

    let reset_cmd = &commands[reset_index];
    assert_eq!(
        reset_cmd,
        &vec!["tt-smi".to_string(), "-r".to_string()],
        "default reset command should be exactly `tt-smi -r`: {reset_cmd:?}"
    );
    assert!(
        reset_index < runpy_index,
        "reset ({reset_index}) must run before run.py ({runpy_index}): {commands:?}"
    );
}

/// `reset_before_serve = false` must skip the reset entirely -- but the
/// stop-stale-serving-container check still runs unconditionally (it's not
/// gated by `reset_before_serve` at all), so the first command is the
/// `docker ps` stale-container query and the second is the run.py
/// invocation itself.
#[test]
fn runpy_start_skips_reset_when_disabled() {
    let runner = FakeRunner::new(0);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.reset_before_serve = false;
    let backend = RunPyBackend::new(cfg, Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("tt-smi")),
        "no tt-smi/reset command should be issued when reset_before_serve is false: {commands:?}"
    );
    assert_eq!(
        commands[0][0], "docker",
        "stop-stale docker ps query should still run even when reset is disabled: {commands:?}"
    );
    assert_eq!(
        commands[1][0], "python3",
        "run.py should be the next command when reset is disabled: {commands:?}"
    );
}

/// A failing reset must fail `start` outright and must NEVER launch
/// run.py -- a wedged mesh would almost certainly make the serve attempt
/// fail anyway, so surface the reset failure instead of masking it.
#[test]
fn runpy_start_fails_and_skips_runpy_when_reset_fails() {
    let runner = FakeRunner::new(0);
    runner.fail_run("tt-smi -r", "board reset timed out");
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    let err = backend
        .start("llama3")
        .expect_err("start should fail when the board reset fails");
    assert!(
        err.to_string().contains("tt-smi -r"),
        "error should mention the failing reset command: {err}"
    );

    let commands = runner.commands();
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("python3")),
        "run.py must never be launched after a failed reset: {commands:?}"
    );
}

// ---------------------------------------------------------------------
// Stop stale serving containers BEFORE (reset+)launch: a leftover/crashed
// container still publishing the serving port holds the chips, so run.py's
// own container-start check times out on the next launch. See the module
// doc in `src/serving/runpy.rs`.
// ---------------------------------------------------------------------

/// When a stale container is still publishing the serving port, `start`
/// must stop it FIRST -- strictly before the board reset and before
/// launching run.py.
#[test]
fn runpy_start_stops_stale_serving_container_before_reset_and_launch() {
    let runner = FakeRunner::new(0);
    runner.set_run_output("docker ps", "stale123\n");
    let backend = RunPyBackend::new(config("127.0.0.1", 8003), Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();

    let stop_index = commands
        .iter()
        .position(|cmd| {
            cmd.first().map(String::as_str) == Some("docker")
                && cmd.get(1).map(String::as_str) == Some("stop")
        })
        .expect("expected a docker stop for the stale container");
    assert!(
        commands[stop_index].iter().any(|a| a == "stale123"),
        "docker stop should target the stale container id: {:?}",
        commands[stop_index]
    );

    let reset_index = commands
        .iter()
        .position(|cmd| cmd.first().map(String::as_str) == Some("tt-smi"))
        .expect("expected a tt-smi reset command among the recorded commands");
    let runpy_index = commands
        .iter()
        .position(|cmd| cmd.first().map(String::as_str) == Some("python3"))
        .expect("expected a python3 run.py invocation among the recorded commands");

    assert!(
        stop_index < reset_index,
        "stale-container stop ({stop_index}) must run before board reset \
         ({reset_index}): {commands:?}"
    );
    assert!(
        reset_index < runpy_index,
        "board reset ({reset_index}) must run before run.py ({runpy_index}): {commands:?}"
    );
}

/// When `docker ps` reports nothing publishing the port, `start` must not
/// issue a `docker stop` at all -- and should proceed normally.
#[test]
fn runpy_start_skips_docker_stop_when_no_stale_container() {
    let runner = FakeRunner::new(0); // docker ps defaults to empty output
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend.start("llama3").expect("start should succeed");

    let commands = runner.commands();
    assert!(
        !commands
            .iter()
            .any(|cmd| cmd.first().map(String::as_str) == Some("docker")
                && cmd.get(1).map(String::as_str) == Some("stop")),
        "no docker stop should be issued when docker ps returns nothing: {commands:?}"
    );
}

// ---------------------------------------------------------------------
// Model identifier: run.py wants the SHORT name; the served /v1 id is the
// authoritative HF id. See the module doc in `src/serving/runpy.rs`.
// ---------------------------------------------------------------------

/// `start` must strip any `org/` prefix before passing `--model` to run.py
/// -- `run.py` validates `--model` against `model_spec.json`'s SHORT model
/// names, but `tt models` (and callers generally) deal in HF ids.
#[test]
fn runpy_start_strips_org_prefix_for_runpy_model_flag() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend
        .start("Qwen/Qwen3-32B")
        .expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--model" && w[1] == "Qwen3-32B"),
        "argv should carry the STRIPPED short model name, not the HF id: {cmd:?}"
    );
    assert!(
        !cmd.iter().any(|a| a == "Qwen/Qwen3-32B"),
        "argv must not carry the full HF id anywhere: {cmd:?}"
    );
}

/// A model id with no `org/` prefix must pass through to `--model`
/// unchanged.
#[test]
fn runpy_start_passes_through_model_with_no_org_prefix() {
    let runner = FakeRunner::new(0);
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend.start("Qwen3-32B").expect("start should succeed");

    let commands = runner.commands();
    let cmd = find_runpy_cmd(&commands);
    assert!(
        cmd.windows(2)
            .any(|w| w[0] == "--model" && w[1] == "Qwen3-32B"),
        "argv should carry --model Qwen3-32B unchanged when there's no org prefix: {cmd:?}"
    );
}

/// The returned `Endpoint.model` must be the AUTHORITATIVE served id
/// fetched from `GET /v1/models`, not whatever form the caller passed to
/// `start` -- proven here by passing the short form while the fake
/// `/v1/models` response reports the full HF id.
#[test]
fn runpy_start_endpoint_model_is_served_id_from_v1_models() {
    let runner = FakeRunner::new(0);
    runner.set_http_get(r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#);
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    let endpoint = backend.start("Qwen3-32B").expect("start should succeed");

    assert_eq!(
        endpoint.model, "Qwen/Qwen3-32B",
        "Endpoint.model should be the served id from /v1/models, even \
         though --model got the short form and the caller passed the short \
         form too"
    );
}

/// If the `/v1/models` fetch fails (or can't be parsed), `start` must not
/// fail outright -- it should fall back to the original `model` argument
/// passed to `start`.
#[test]
fn runpy_start_endpoint_model_falls_back_when_http_get_fails() {
    let runner = FakeRunner::new(0); // http_get left unconfigured -> Err
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    let endpoint = backend
        .start("Qwen/Qwen3-32B")
        .expect("start should succeed even when the /v1/models fetch fails");

    assert_eq!(
        endpoint.model, "Qwen/Qwen3-32B",
        "when /v1/models can't be fetched, Endpoint.model should fall back \
         to the original start() argument"
    );
}

/// Malformed JSON from `/v1/models` must also fall back to the original
/// `model` argument rather than failing `start`.
#[test]
fn runpy_start_endpoint_model_falls_back_on_unparseable_response() {
    let runner = FakeRunner::new(0);
    runner.set_http_get("not json");
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    let endpoint = backend
        .start("Qwen/Qwen3-32B")
        .expect("start should succeed even when /v1/models returns garbage");

    assert_eq!(endpoint.model, "Qwen/Qwen3-32B");
}

/// The health poll should actually poll more than once when the first
/// probes report unhealthy.
#[test]
fn runpy_start_polls_health_until_ok() {
    let runner = FakeRunner::new(2); // unhealthy for the first two probes
    let backend = RunPyBackend::new(config("127.0.0.1", 8081), Box::new(runner))
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
    let backend = RunPyBackend::new(config("127.0.0.1", 8082), Box::new(runner))
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
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

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
    let backend = RunPyBackend::new(config("127.0.0.1", 8080), Box::new(runner.clone()));

    backend.stop("llama3").expect("stop should succeed");

    let commands = runner.commands();
    assert_eq!(
        commands.len(),
        1,
        "expected only the ps query, no stop call: {commands:?}"
    );
    assert_eq!(backend.status().unwrap(), ServingStatus::Idle);
}

// ---------------------------------------------------------------------
// `list_models`: enumerate model_spec.json's catalog.
// ---------------------------------------------------------------------

/// A scratch `model_spec.json` fixture, unique per test run and cleaned up
/// on drop -- same pattern as `crates/tt/tests/e2e_mock.rs`'s
/// `TempConfigDir`, kept local here since this is the only file that needs
/// it.
struct TempModelSpec(std::path::PathBuf);

impl TempModelSpec {
    fn write(contents: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "tt-station-model-spec-{}-{}.json",
            std::process::id(),
            std::time::Instant::now().elapsed().as_nanos()
        ));
        std::fs::write(&path, contents).expect("write temp model_spec.json fixture");
        TempModelSpec(path)
    }

    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempModelSpec {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const MODEL_SPEC_FIXTURE: &str = r#"{
    "release_version": "0.12.0",
    "model_specs": {
        "Qwen/Qwen3-32B": { "P300X2": {}, "T3K": {} },
        "Qwen/Qwen3-8B": { "P150X4": {} }
    }
}"#;

/// `list_models` should read `model_spec.json`, return every model with its
/// device meshes sorted, the whole list sorted by model name, and echo back
/// `release_version`.
#[test]
fn runpy_list_models_reads_and_sorts_model_spec() {
    let fixture = TempModelSpec::write(MODEL_SPEC_FIXTURE);
    let mut cfg = config("127.0.0.1", 8080);
    cfg.model_spec_path = Some(fixture.path());
    let backend = RunPyBackend::new(cfg, Box::new(FakeRunner::new(0)));

    let resp = backend
        .list_models()
        .expect("list_models should succeed against the fixture");

    assert_eq!(resp.release_version.as_deref(), Some("0.12.0"));
    assert_eq!(resp.models.len(), 2);

    // Sorted by name: "Qwen/Qwen3-32B" < "Qwen/Qwen3-8B" (ASCII '3' < '8').
    assert_eq!(resp.models[0].name, "Qwen/Qwen3-32B");
    assert_eq!(resp.models[0].devices, vec!["P300X2", "T3K"]);

    assert_eq!(resp.models[1].name, "Qwen/Qwen3-8B");
    assert_eq!(resp.models[1].devices, vec!["P150X4"]);
}

/// `model_spec_path` defaults to `<repo_dir>/model_spec.json` when
/// unconfigured -- proven by pointing `repo_dir` at a scratch directory
/// containing exactly that filename with no explicit `model_spec_path` set.
#[test]
fn runpy_list_models_defaults_path_to_repo_dir_slash_model_spec_json() {
    let dir = std::env::temp_dir().join(format!(
        "tt-station-repo-dir-{}-{}",
        std::process::id(),
        std::time::Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch repo dir");
    std::fs::write(dir.join("model_spec.json"), MODEL_SPEC_FIXTURE)
        .expect("write model_spec.json into scratch repo dir");

    let mut cfg = config("127.0.0.1", 8080);
    cfg.repo_dir = dir.to_string_lossy().into_owned();
    let backend = RunPyBackend::new(cfg, Box::new(FakeRunner::new(0)));

    let resp = backend
        .list_models()
        .expect("list_models should find model_spec.json under repo_dir");
    assert_eq!(resp.models.len(), 2);

    let _ = std::fs::remove_dir_all(&dir);
}
