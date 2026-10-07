//! Which host the runpy and docker backends poll for readiness, versus which host they advertise.
//!
//! When `serving_host` was not set by the operator (detected LAN address or hostname fallback),
//! `main` sets `probe_host` to loopback: readiness is polled there and the returned `Endpoint`
//! still carries the LAN address. With no `probe_host` (every operator-set host, and every
//! configuration from before smarter defaults) the poll goes to `serving_host` exactly as it
//! always has. The tt backend has the same tests in `src/serving/tt.rs`; `/serving`'s loopback
//! probe is covered in `src/serving/discovery.rs`.

use tt_station_agentd::serving::docker::{DockerBackend, DockerConfig};
use tt_station_agentd::serving::runpy::{RunPyBackend, RunPyConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

fn runpy_urls(host: &str, probe: Option<&str>) -> (Vec<String>, String) {
    let runner = FakeRunner::new(0);
    runner.set_http_get(r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#);
    let cfg = RunPyConfig {
        host: host.to_string(),
        probe_host: probe.map(String::from),
        service_port: 8080,
        ..Default::default()
    };
    let ep = RunPyBackend::new(cfg, Box::new(runner.clone()))
        .start("Qwen/Qwen3-32B")
        .unwrap();
    (runner.probed_urls(), ep.base_url)
}

fn docker_urls(host: &str, probe: Option<&str>) -> (Vec<String>, String) {
    let runner = FakeRunner::new(0);
    let cfg = DockerConfig {
        host: host.to_string(),
        probe_host: probe.map(String::from),
        host_port: 8080,
        ..Default::default()
    };
    let ep = DockerBackend::new(cfg, Box::new(runner.clone()))
        .start("Qwen/Qwen3-32B")
        .unwrap();
    (runner.probed_urls(), ep.base_url)
}

#[test]
fn runpy_with_a_defaulted_host_polls_loopback_and_advertises_the_lan_address() {
    let (urls, base) = runpy_urls("192.168.9.9", Some("127.0.0.1"));
    assert_eq!(
        urls,
        [
            "http://127.0.0.1:8080/health",
            "http://127.0.0.1:8080/v1/models"
        ]
    );
    assert_eq!(base, "http://192.168.9.9:8080/v1");
}

#[test]
fn runpy_with_an_explicit_host_polls_that_host_exactly_as_before() {
    let (urls, base) = runpy_urls("qb2-lab.local", None);
    assert_eq!(
        urls,
        [
            "http://qb2-lab.local:8080/health",
            "http://qb2-lab.local:8080/v1/models"
        ]
    );
    assert_eq!(base, "http://qb2-lab.local:8080/v1");
}

#[test]
fn docker_with_a_defaulted_host_polls_loopback_and_advertises_the_lan_address() {
    let (urls, base) = docker_urls("192.168.9.9", Some("127.0.0.1"));
    assert_eq!(urls, ["http://127.0.0.1:8080/health"]);
    assert_eq!(base, "http://192.168.9.9:8080/v1");
}

#[test]
fn docker_with_an_explicit_host_polls_that_host_exactly_as_before() {
    let (urls, base) = docker_urls("qb2-lab.local", None);
    assert_eq!(urls, ["http://qb2-lab.local:8080/health"]);
    assert_eq!(base, "http://qb2-lab.local:8080/v1");
}

/// Old flags-only configuration (a loopback host given by flag, no `probe_host`): the probe URLs
/// are the same strings the backends built before `probe_host` existed.
#[test]
fn old_flags_only_configuration_builds_the_same_probe_urls_as_before() {
    let (urls, _) = runpy_urls("127.0.0.1", None);
    assert_eq!(
        urls,
        [
            "http://127.0.0.1:8080/health",
            "http://127.0.0.1:8080/v1/models"
        ]
    );
    let (urls, _) = docker_urls("127.0.0.1", None);
    assert_eq!(urls, ["http://127.0.0.1:8080/health"]);
}
