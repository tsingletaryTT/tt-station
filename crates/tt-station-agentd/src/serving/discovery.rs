//! Serving-endpoint discovery for the additive `GET /serving` route.
//!
//! Where `ServingBackend` (mod.rs) tracks only what the AGENT itself
//! launched, this module answers a different question: "what
//! `tt-inference-server` `/v1` endpoints are live on this box right now,
//! regardless of who started them?" -- the agent's own `/run`, tt-studio's
//! FastAPI, or a human running `run.py` by hand all end up as the same kind
//! of container publishing an OpenAI-compatible `/v1` server, so a single
//! `docker ps`-based scan surfaces all of them.
//!
//! The detection is factored into one pure-ish function,
//! [`discover_serving`], that reaches the outside world only through the
//! injected [`CommandRunner`] (for `docker ps` and the `/v1/models` probe) --
//! so it's unit-testable with canned `docker ps` output + canned
//! `/v1/models` responses, no real docker or HTTP required. The route handler
//! (`routes::get_serving`) is a thin wrapper that runs this on
//! `spawn_blocking` with a `RealCommandRunner`.

use libttstation::model::{ServingEntry, ServingStatus};

use super::docker::CommandRunner;

/// Substring an image name must contain (case-insensitive) to be considered a
/// serving candidate. Matches the release image repo
/// (`.../vllm-tt-metal-src-release-...`) as well as any `tt-inference-server`
/// tag, whether launched by the agent, tt-studio, or by hand.
const IMAGE_MARKER: &str = "tt-inference-server";

/// The `docker ps` format string used to enumerate candidate containers:
/// tab-separated id, image, ports, and name. Tab-separated (not the default
/// table layout) so [`discover_serving`] can split each row deterministically
/// without column-width heuristics.
const DOCKER_PS_FORMAT: &str = "{{.ID}}\t{{.Image}}\t{{.Ports}}\t{{.Names}}";

/// Discover every live `tt-inference-server` `/v1` endpoint on the box.
///
/// Steps (each container must pass ALL of them to appear in the result):
///   1. `docker ps` for id/image/ports/name. A failure here (docker missing,
///      daemon down) yields an EMPTY list -- never an error/panic, so a clean
///      box just reports nothing serving.
///   2. Keep rows whose image contains `tt-inference-server`
///      (case-insensitive) AND that publish a host port (parsed out of the
///      `Ports` column via [`parse_published_host_port`]).
///   3. Probe `GET http://127.0.0.1:<host_port>/v1/models`; keep only those
///      returning JSON with a non-empty `data[]`, and read the served model
///      id from `data[0].id` (the same readiness gate the agent's own serve
///      uses -- see `serving/runpy.rs`).
///
/// `serving_host` is the agent's configured serving host, baked into each
/// entry's `base_url` (the probe itself always uses loopback). `agent_port` /
/// `agent_status` classify `source`: an entry is `"agent"` when its
/// `host_port` matches the agent's own configured serving port AND the
/// agent's in-memory status is `Serving(<that model>)`; otherwise
/// `"external"`.
///
/// The result is sorted by `host_port` for a stable, deterministic order.
pub fn discover_serving(
    runner: &dyn CommandRunner,
    serving_host: &str,
    agent_port: u16,
    agent_status: &ServingStatus,
) -> Vec<ServingEntry> {
    // A `docker ps` failure (no docker binary, daemon not running, ...) is
    // "nothing serving," never an error -- a clean box must not make
    // `/serving` fail.
    let ps_output = match runner.run(&["docker", "ps", "--format", DOCKER_PS_FORMAT]) {
        Ok(out) => out,
        Err(_) => return Vec::new(),
    };

    let mut entries: Vec<ServingEntry> = Vec::new();

    for line in ps_output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Columns are exactly the four `DOCKER_PS_FORMAT` fields, tab-joined.
        let mut cols = line.split('\t');
        let _id = cols.next().unwrap_or("");
        let image = cols.next().unwrap_or("");
        let ports = cols.next().unwrap_or("");
        let name = cols.next().unwrap_or("");

        // Fast image filter first, then require a published host port --
        // a container with nothing published has no `/v1` to probe.
        if !image.to_lowercase().contains(IMAGE_MARKER) {
            continue;
        }
        let Some(host_port) = parse_published_host_port(ports) else {
            continue;
        };

        // Authoritative liveness gate: a non-empty `/v1/models` `data[]`
        // means the server has a model loaded and is answering.
        let probe_url = format!("http://127.0.0.1:{host_port}/v1/models");
        let Some(model) = probe_served_model(runner, &probe_url) else {
            continue;
        };

        let source = if host_port == agent_port
            && matches!(agent_status, ServingStatus::Serving(m) if *m == model)
        {
            "agent"
        } else {
            "external"
        };

        entries.push(ServingEntry {
            base_url: format!("http://{serving_host}:{host_port}/v1"),
            model,
            host_port,
            container: name.to_string(),
            source: source.to_string(),
        });
    }

    entries.sort_by_key(|e| e.host_port);
    entries
}

/// Reconcile the agent's in-memory serving status against docker reality
/// (the [`discover_serving`] entries), returning the status that should
/// actually be reported.
///
/// The agent's `status` is its last serving *intent*: it's flipped to
/// `Serving(model)` on a successful `/run` and back to `Idle` only on the
/// agent's own `/stop`. A model stopped OUT OF BAND -- a manual `docker stop`,
/// a crash, a host reboot of the container -- never runs through `/stop`, so
/// `status` gets stuck reporting `Serving` while nothing is actually up
/// (observed live: `/status` said `serving:Llama-3.3-70B-Instruct` while
/// `docker ps` was empty and `:8003` was dead). `/serving` didn't have this
/// problem because it probes docker; this brings `/status` to the same truth.
///
/// Rule: a `Serving(_)` status only stays `Serving` if `discover_serving`
/// found a live endpoint attributed to the agent (`source == "agent"` -- the
/// agent's own serving port answering `/v1/models` for that model). Otherwise
/// the agent's container is gone, so we report `Idle`. An already-`Idle`
/// status is returned unchanged (a serve started out of band shows up in
/// `/serving` as `external`; `/status` reflects the AGENT's serve only).
pub fn reconcile_status(status: &ServingStatus, entries: &[ServingEntry]) -> ServingStatus {
    match status {
        ServingStatus::Serving(_) if entries.iter().any(|e| e.source == "agent") => status.clone(),
        ServingStatus::Serving(_) => ServingStatus::Idle,
        ServingStatus::Idle => ServingStatus::Idle,
    }
}

/// Probe `GET {url}` (expected to be a `/v1/models` endpoint) and return the
/// served model id from `data[0].id`, or `None` if the request fails, the
/// body isn't JSON, `data[]` is empty/missing, or `data[0]` carries no string
/// `id`. `None` means "don't surface this container" -- the same
/// non-empty-`data[]` readiness signal `serving/runpy.rs` gates on.
fn probe_served_model(runner: &dyn CommandRunner, url: &str) -> Option<String> {
    let body = runner.http_get(url).ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let data = value.get("data")?.as_array()?;
    if data.is_empty() {
        return None;
    }
    data[0].get("id")?.as_str().map(str::to_string)
}

/// Parse the published HOST port out of a `docker ps` `Ports` column, or
/// `None` when the container publishes nothing to the host.
///
/// The column is a comma-separated list of mappings; a PUBLISHED mapping
/// contains `->` (`<host-side>-><container-side>`), e.g.
/// `0.0.0.0:8003->8003/tcp` or the IPv6 form `:::8003->8003/tcp` (also
/// `[::]:8003->8003/tcp`). An entry with no `->` (e.g. a bare `8000/tcp`) is
/// merely EXPOSED, not published, and is skipped. The host port is whatever
/// follows the last `:` on the host side, which handles the IPv4, `:::`, and
/// bracketed-IPv6 spellings uniformly. The first mapping that parses wins.
fn parse_published_host_port(ports: &str) -> Option<u16> {
    for part in ports.split(',') {
        let part = part.trim();
        // Only published mappings carry `->`; the host side is before it.
        let Some((host_side, _container_side)) = part.split_once("->") else {
            continue;
        };
        // Port is after the last ':' -- works for `0.0.0.0:8003`, `:::8003`,
        // and `[::]:8003` alike.
        let Some(port_str) = host_side.rsplit(':').next() else {
            continue;
        };
        if let Ok(port) = port_str.trim().parse::<u16>() {
            return Some(port);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    fn entry(source: &str, model: &str, host_port: u16) -> ServingEntry {
        ServingEntry {
            base_url: format!("http://h:{host_port}/v1"),
            model: model.to_string(),
            host_port,
            container: "c".to_string(),
            source: source.to_string(),
        }
    }

    #[test]
    fn reconcile_keeps_serving_when_agent_endpoint_is_live() {
        let status = ServingStatus::Serving("meta-llama/Llama-3.3-70B-Instruct".to_string());
        let entries = vec![entry("agent", "meta-llama/Llama-3.3-70B-Instruct", 8003)];
        assert_eq!(reconcile_status(&status, &entries), status);
    }

    #[test]
    fn reconcile_flips_to_idle_when_nothing_is_serving() {
        // The exact live bug: status says Serving, but docker reality is empty
        // (manual `docker stop`) -> report Idle instead of the stale model.
        let status = ServingStatus::Serving("meta-llama/Llama-3.3-70B-Instruct".to_string());
        assert_eq!(reconcile_status(&status, &[]), ServingStatus::Idle);
    }

    #[test]
    fn reconcile_flips_to_idle_when_only_external_serves() {
        // Something ELSE is serving on another port (source: external), but the
        // agent's own serve is gone -> the agent's /status must not claim it.
        let status = ServingStatus::Serving("meta-llama/Llama-3.3-70B-Instruct".to_string());
        let entries = vec![entry("external", "some/Other-Model", 9000)];
        assert_eq!(reconcile_status(&status, &entries), ServingStatus::Idle);
    }

    #[test]
    fn reconcile_leaves_idle_untouched() {
        assert_eq!(
            reconcile_status(&ServingStatus::Idle, &[entry("external", "x", 9000)]),
            ServingStatus::Idle
        );
    }

    /// A `CommandRunner` fake that returns canned `docker ps` stdout and maps
    /// a probe URL to a canned `/v1/models` body (or an error, for a port
    /// that isn't answering). Only `run` (for `docker ps`) and `http_get`
    /// (for the probe) are exercised by `discover_serving`.
    struct FakeProbe {
        /// Canned stdout for the `docker ps` call. `None` makes `run` return
        /// `Err`, modelling a box with no docker at all.
        ps_output: Option<String>,
        /// Maps a `host_port` to the `/v1/models` body its probe returns. A
        /// port absent from the map makes `http_get` return `Err` (nothing
        /// listening there).
        models_by_port: HashMap<u16, String>,
        /// Records the probe URLs `http_get` was asked for, so a test can
        /// assert the loopback-probe contract.
        probed: Mutex<Vec<String>>,
    }

    impl FakeProbe {
        fn new(ps_output: Option<&str>, models: &[(u16, &str)]) -> Self {
            FakeProbe {
                ps_output: ps_output.map(str::to_string),
                models_by_port: models.iter().map(|(p, b)| (*p, b.to_string())).collect(),
                probed: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for FakeProbe {
        fn run(&self, _args: &[&str]) -> anyhow::Result<String> {
            match &self.ps_output {
                Some(out) => Ok(out.clone()),
                None => Err(anyhow::anyhow!("docker: command not found")),
            }
        }

        fn health_ok(&self, _url: &str) -> bool {
            true
        }

        fn http_get(&self, url: &str) -> anyhow::Result<String> {
            self.probed
                .lock()
                .expect("probed mutex poisoned")
                .push(url.to_string());
            // The port is whatever sits between `127.0.0.1:` and `/v1/models`.
            let port: u16 = url
                .rsplit_once("127.0.0.1:")
                .and_then(|(_, rest)| rest.split('/').next())
                .and_then(|p| p.parse().ok())
                .expect("probe url should carry a loopback port");
            self.models_by_port
                .get(&port)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("nothing listening on {port}"))
        }
    }

    /// The happy path over a mixed `docker ps`: a matching container with a
    /// live, non-empty `/v1/models` is surfaced; a non-matching image is
    /// dropped on the image filter; a matching container whose `/v1/models`
    /// `data[]` is EMPTY is dropped on the liveness gate; and an
    /// IPv6-published (`:::`) matching container is surfaced with its host
    /// port parsed correctly. Result is sorted by `host_port`.
    #[test]
    fn discovers_only_confirmed_serving_endpoints_across_mixed_docker_ps() {
        // Port 8003: matching image, IPv4 publish, live with a real model.
        // Port 9999: NON-matching image -> dropped regardless of anything.
        // Port 8005: matching image, but empty `data[]` -> dropped.
        // Port 8009: matching image, IPv6 (`:::`) publish, live -> kept.
        let ps = "\
c1\tghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release:0.14\t0.0.0.0:8003->8000/tcp\ttt-agent-llama
c2\tnginx:latest\t0.0.0.0:9999->80/tcp\tsome-web
c3\tghcr.io/tenstorrent/tt-inference-server:empty\t0.0.0.0:8005->8000/tcp\ttt-studio-warming
c4\tghcr.io/tenstorrent/TT-Inference-Server:mixedcase\t:::8009->8000/tcp\ttt-manual-qwen";

        let runner = FakeProbe::new(
            Some(ps),
            &[
                (
                    8003,
                    r#"{"data":[{"id":"meta-llama/Llama-3.3-70B-Instruct"}]}"#,
                ),
                // 8005 answers but with an EMPTY data array -> not ready.
                (8005, r#"{"data":[]}"#),
                (8009, r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#),
            ],
        );

        let entries = discover_serving(&runner, "quietbox.local", 8000, &ServingStatus::Idle);

        assert_eq!(
            entries.len(),
            2,
            "only the two live tt-inference-server endpoints should surface, got: {entries:?}"
        );

        // Sorted by host_port: 8003 then 8009.
        assert_eq!(entries[0].host_port, 8003);
        assert_eq!(entries[0].model, "meta-llama/Llama-3.3-70B-Instruct");
        assert_eq!(entries[0].base_url, "http://quietbox.local:8003/v1");
        assert_eq!(entries[0].container, "tt-agent-llama");

        // The IPv6-published (`:::8009->`) container parsed its host port.
        assert_eq!(entries[1].host_port, 8009);
        assert_eq!(entries[1].model, "Qwen/Qwen3-32B");
        assert_eq!(entries[1].base_url, "http://quietbox.local:8009/v1");
        assert_eq!(entries[1].container, "tt-manual-qwen");
    }

    /// No docker at all (a `docker ps` failure) yields an empty list, never
    /// an error -- a clean box must not make `/serving` fail.
    #[test]
    fn missing_docker_yields_empty_list() {
        let runner = FakeProbe::new(None, &[]);
        let entries = discover_serving(&runner, "127.0.0.1", 8000, &ServingStatus::Idle);
        assert!(entries.is_empty());
    }

    /// Docker present but no containers running -> empty list.
    #[test]
    fn empty_docker_ps_yields_empty_list() {
        let runner = FakeProbe::new(Some(""), &[]);
        let entries = discover_serving(&runner, "127.0.0.1", 8000, &ServingStatus::Idle);
        assert!(entries.is_empty());
    }

    /// `source` classification: the endpoint on the agent's OWN configured
    /// serving port, whose model matches the agent's in-memory `Serving`
    /// status, is `"agent"`; a second live endpoint on a different port is
    /// `"external"`.
    #[test]
    fn classifies_agent_vs_external_by_port_and_status() {
        let ps = "\
c1\tghcr.io/tenstorrent/tt-inference-server:rel\t0.0.0.0:8000->8000/tcp\ttt-agent
c2\tghcr.io/tenstorrent/tt-inference-server:rel\t0.0.0.0:8003->8000/tcp\ttt-studio";

        let runner = FakeProbe::new(
            Some(ps),
            &[
                (8000, r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#),
                (
                    8003,
                    r#"{"data":[{"id":"meta-llama/Llama-3.3-70B-Instruct"}]}"#,
                ),
            ],
        );

        // Agent is configured to serve on 8000 and believes it's serving
        // exactly the model that port reports.
        let status = ServingStatus::Serving("Qwen/Qwen3-32B".to_string());
        let entries = discover_serving(&runner, "127.0.0.1", 8000, &status);

        assert_eq!(entries.len(), 2);
        let agent = &entries[0];
        assert_eq!(agent.host_port, 8000);
        assert_eq!(agent.source, "agent");

        let external = &entries[1];
        assert_eq!(external.host_port, 8003);
        assert_eq!(external.source, "external");
    }

    /// A container whose model matches a `Serving` status but is published on
    /// a DIFFERENT port than the agent's configured serving port is still
    /// `"external"` -- the port is part of the identity, not just the model.
    #[test]
    fn agent_port_mismatch_is_external_even_when_model_matches() {
        let ps = "c1\tghcr.io/tenstorrent/tt-inference-server:rel\t0.0.0.0:8003->8000/tcp\ttt-x";
        let runner = FakeProbe::new(Some(ps), &[(8003, r#"{"data":[{"id":"m"}]}"#)]);
        let status = ServingStatus::Serving("m".to_string());
        // Agent's configured port is 8000, but this endpoint is on 8003.
        let entries = discover_serving(&runner, "127.0.0.1", 8000, &status);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "external");
    }

    #[test]
    fn parse_published_host_port_handles_ipv4_ipv6_and_exposed_only() {
        assert_eq!(
            parse_published_host_port("0.0.0.0:8003->8003/tcp"),
            Some(8003)
        );
        assert_eq!(parse_published_host_port(":::8003->8003/tcp"), Some(8003));
        assert_eq!(parse_published_host_port("[::]:8003->8003/tcp"), Some(8003));
        // Exposed-but-not-published: no `->`, so nothing to probe.
        assert_eq!(parse_published_host_port("8000/tcp"), None);
        assert_eq!(parse_published_host_port(""), None);
        // Mixed list: the IPv4 publish wins (first parseable).
        assert_eq!(
            parse_published_host_port("0.0.0.0:7000->8000/tcp, :::7000->8000/tcp"),
            Some(7000)
        );
    }
}
