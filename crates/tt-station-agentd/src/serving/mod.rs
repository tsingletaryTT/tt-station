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
    ///
    /// NOTE: this is a deliberate, currently-unused seam. `AppState` tracks
    /// its own `status` (updated by `routes.rs`'s `set_serving`/`set_idle`
    /// on every successful `/run`/`/stop`), and `GET /status` reads that,
    /// not this method -- nothing in `tt-station-agentd` calls
    /// `ServingBackend::status()` today. It's here ahead of the dstack
    /// backend (M4), where a backend that can lose track of what it's
    /// serving across an agent restart will need `/status` to ask it
    /// directly instead of trusting in-process state. Don't assume `GET
    /// /status`'s response reflects this method's return value -- as of
    /// this PoC, it doesn't.
    fn status(&self) -> Result<ServingStatus>;
}

/// Construct a `ServingBackend` for the given `--backend` CLI choice.
///
/// `"docker"` and `"dstack"` are the only recognized kinds; anything else is
/// an error rather than a silent fallback, since a typo'd backend name
/// should fail loudly at startup rather than quietly serving nothing.
///
/// `docker_config` is only meaningful for the Docker backend today (dstack's
/// stub needs none of it); it's threaded through here rather than the
/// individual fields being hardcoded so the CLI wiring in `main.rs` has a
/// single function to call regardless of which backend was chosen, and so
/// adding a new Docker-only knob doesn't mean touching this function's
/// signature again.
pub fn make_backend(
    kind: &str,
    docker_config: docker::DockerConfig,
) -> Result<Box<dyn ServingBackend>> {
    match kind {
        "docker" => Ok(Box::new(docker::DockerBackend::new(
            docker_config,
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
        assert!(make_backend("docker", docker::DockerConfig::default()).is_ok());
        assert!(make_backend("dstack", docker::DockerConfig::default()).is_ok());
    }

    #[test]
    fn make_backend_rejects_unknown_kind() {
        // `Box<dyn ServingBackend>` isn't `Debug`, so `unwrap_err` (which
        // requires the `Ok` side to be `Debug` for its panic message)
        // doesn't work here -- match instead.
        match make_backend("bogus", docker::DockerConfig::default()) {
            Err(err) => assert!(err.to_string().contains("bogus")),
            Ok(_) => panic!("expected an error for an unknown backend kind"),
        }
    }
}
