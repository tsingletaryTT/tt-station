//! Serving-backend abstraction: the seam between `tt-station-agentd` and
//! whatever actually runs model-serving containers/VMs on a box.
//!
//! Docker proves the end-to-end story today (Task 9); `dstack` (a
//! confidential-VM orchestrator) takes over the same role in M4. Both live
//! behind the one `ServingBackend` trait so nothing above this module --
//! the agent's control routes (Task 10), the Mac-side `AgentClient` (Task
//! 11), or the `tt` CLI (Task 12) -- ever has to know or care which backend
//! is actually running. Swapping Docker for dstack later should be a
//! one-line change at whatever call site constructs the backend, not a
//! rewrite of everything that talks to it.

pub mod docker;
pub mod dstack;

use anyhow::Result;
use libttstation::model::{Endpoint, ServingStatus};

/// Starts, stops, and reports on model-serving for a box.
///
/// Deliberately synchronous (no `async fn`): implementations are expected
/// to block for as long as it genuinely takes to start/stop serving (docker
/// pulling an image, dstack spinning up a VM, ...), and a plain sync trait
/// is trivial to fake in tests with no async-trait machinery. A caller that
/// invokes this from an async context (e.g. an axum handler, arriving in
/// Task 10) is expected to hop off the async runtime first -- e.g. via
/// `tokio::task::spawn_blocking` -- rather than this trait growing async
/// just to accommodate one caller.
pub trait ServingBackend: Send + Sync {
    /// Start serving `model`, blocking until it's confirmed healthy (or the
    /// implementation gives up and returns an error). On success, returns
    /// the `Endpoint` clients should send inference requests to.
    fn start(&self, model: &str) -> Result<Endpoint>;

    /// Stop serving `model`. Idempotent where the underlying tooling allows
    /// it -- e.g. `docker stop` on an already-stopped/missing container is
    /// not treated as an error by `DockerBackend`.
    fn stop(&self, model: &str) -> Result<()>;

    /// Current serving status, independent of any particular `start`/`stop`
    /// call in this process -- e.g. so `/status` can report reality even
    /// after the agent itself restarted.
    fn status(&self) -> Result<ServingStatus>;
}

/// Construct a `ServingBackend` for the given `--backend` CLI choice.
///
/// `"docker"` and `"dstack"` are the only recognized kinds; anything else is
/// an error rather than a silent fallback, since a typo'd backend name
/// should fail loudly at startup rather than quietly serving nothing.
///
/// `host`/`host_port`/`image` are only meaningful for the Docker backend
/// today (dstack's stub needs none of them); they're threaded through here
/// rather than hardcoded so the eventual CLI wiring (Task 10) has a single
/// function to call regardless of which backend was chosen.
pub fn make_backend(
    kind: &str,
    host: &str,
    host_port: u16,
    image: &str,
) -> Result<Box<dyn ServingBackend>> {
    match kind {
        "docker" => Ok(Box::new(docker::DockerBackend::new(
            image.to_string(),
            host.to_string(),
            host_port,
            Box::new(docker::RealCommandRunner),
        ))),
        "dstack" => Ok(Box::new(dstack::DstackBackend)),
        other => Err(anyhow::anyhow!("unknown serving backend: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_backend_constructs_docker_and_dstack() {
        assert!(make_backend("docker", "127.0.0.1", 8080, "some/image:latest").is_ok());
        assert!(make_backend("dstack", "127.0.0.1", 8080, "some/image:latest").is_ok());
    }

    #[test]
    fn make_backend_rejects_unknown_kind() {
        // `Box<dyn ServingBackend>` isn't `Debug`, so `unwrap_err` (which
        // requires the `Ok` side to be `Debug` for its panic message)
        // doesn't work here -- match instead.
        match make_backend("bogus", "127.0.0.1", 8080, "some/image:latest") {
            Err(err) => assert!(err.to_string().contains("bogus")),
            Ok(_) => panic!("expected an error for an unknown backend kind"),
        }
    }
}
