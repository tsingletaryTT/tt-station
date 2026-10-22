//! Serving-backend abstraction: the seam between `tt-station-agentd` and
//! whatever actually runs model-serving containers/VMs on a box.
//!
//! `run.py` (the default) and plain Docker both live behind the one
//! `ServingBackend` trait so nothing above this module -- the agent's control
//! routes, the Mac-side `AgentClient`, or the `tt-station` CLI -- ever has to
//! know or care which backend is actually running. [`NullBackend`] serves
//! nothing, for an agent (or a test) that needs no serving at all.

pub mod discovery;
pub mod docker;
pub mod runpy;
pub mod secrets;

use std::time::Duration;

use anyhow::Result;
use libttstation::model::{Endpoint, ModelsResponse, ServingStatus};

/// Starts, stops, and reports on model-serving for a box.
///
/// Deliberately synchronous (no `async fn`): implementations are expected
/// to block for as long as it genuinely takes to start/stop serving (docker
/// pulling an image, `run.py` waiting for a model to load, ...), and a plain sync trait
/// is trivial to fake in tests with no async-trait machinery. A caller that
/// invokes this from an async context (e.g. an axum handler, arriving in
/// Task 10) is expected to hop off the async runtime first -- e.g. via
/// `tokio::task::spawn_blocking` -- rather than this trait growing async
/// just to accommodate one caller.
pub trait ServingBackend: Send + Sync {
    /// Start serving `model`, blocking until it's confirmed healthy (or the
    /// implementation gives up and returns an error). On success, returns
    /// the `Endpoint` clients should send inference requests to.
    ///
    /// `force` is the operator's `tt-station run --force`: it overrides a refusal
    /// caused by ANOTHER TENANT holding chips, or by gozer being unreadable.
    /// It never overrides anything protecting the caller from an accident
    /// (see `RunPyBackend::acquire_lease` for exactly what it does and does
    /// not license). A backend that does not lease ignores it.
    ///
    /// **THIS is the method a backend implements**; [`ServingBackend::start`]
    /// and [`ServingBackend::start_forced`] are thin provided wrappers so
    /// existing callers (and the argv pins that protect boxes without gozer)
    /// keep the one-argument form. Overriding either of those instead would
    /// silently discard `force`.
    fn start_forcing(&self, model: &str, force: bool) -> Result<Endpoint>;

    /// [`ServingBackend::start_forcing`] with no override -- the ordinary
    /// `POST /run`, and the form every pre-`--force` caller already uses.
    fn start(&self, model: &str) -> Result<Endpoint> {
        self.start_forcing(model, false)
    }

    /// [`ServingBackend::start_forcing`] WITH the override -- `POST /run
    /// {"force": true}`.
    fn start_forced(&self, model: &str) -> Result<Endpoint> {
        self.start_forcing(model, true)
    }

    /// Stop serving `model`. Idempotent where the underlying tooling allows
    /// it -- e.g. `docker stop` on an already-stopped/missing container is
    /// not treated as an error by `DockerBackend`.
    fn stop(&self, model: &str) -> Result<()>;

    /// Take ownership of a gozer lease this agent's PREVIOUS process left
    /// behind, so the next `stop` releases it. Returns whether it was
    /// adopted.
    ///
    /// **Why this exists.** The agent's unit carries `Restart=on-failure`.
    /// Restart agentd while a model is serving and the startup sweep
    /// correctly KEEPS that lease (something is still on its port) -- but the
    /// new process's backend starts with no lease recorded, so the next
    /// `/stop` stops the container and releases nothing. The lease then only
    /// ever goes away via gozer's reap, and **a reaped lease is not reset**
    /// (gozer's `--fresh` is the protected path). The next tenant gets
    /// un-reset silicon with wedged ethernet cores -- the exact failure
    /// `reset_before_serve` exists to prevent.
    ///
    /// `who` is the lease's `tt-station:<port>:<model>` identity, which
    /// `DockerBackend` needs to reconstruct the container name it must stop
    /// before releasing.
    ///
    /// A lease handed to its container (`gozer::hand_off`) is still owned
    /// by that container after the restart. One from before the hand-off
    /// existed names the DEAD agent as owner; the lease reconciler
    /// (`gozer::reconcile_own_leases`) re-attaches it to the container --
    /// WITHOUT a reset -- and updates this backend's id through
    /// `swap_held_lease`.
    ///
    /// Default: adopt nothing and say so, correct for a backend that does not
    /// lease (`NullBackend`). A caller that gets `false` for a lease on its
    /// own serving port must LOG IT LOUDLY, naming the id -- an unowned lease
    /// is an operator-visible problem and the log line is what explains it.
    fn adopt_lease(&self, lease_id: &str, who: &str) -> bool {
        let _ = (lease_id, who);
        false
    }

    /// The gozer lease id this backend currently tracks for a running serve.
    /// Default `None`: a backend that does not lease.
    fn held_lease(&self) -> Option<String> {
        None
    }

    /// Compare-and-swap the tracked lease: if it is `old`, replace it with
    /// `new` (a re-attached lease's new id) or forget it (`None`, released or
    /// lost). Returns whether `old` was the tracked lease. Used by the lease
    /// reconciler (`gozer::reconcile_own_leases`), which changes lease ids
    /// out from under the backend.
    fn swap_held_lease(&self, old: &str, new: Option<&str>) -> bool {
        let _ = (old, new);
        false
    }

    /// Current serving status, independent of any particular `start`/`stop`
    /// call in this process -- e.g. so `/status` can report reality even
    /// after the agent itself restarted.
    ///
    /// NOTE: this is a deliberate, currently-unused seam. `AppState` tracks
    /// its own `status` (updated by `state/serving.rs`'s `set_serving`/`set_idle`
    /// on every successful `/run`/`/stop`), and `GET /status` reads that,
    /// not this method -- nothing in `tt-station-agentd` calls
    /// `ServingBackend::status()` today. It's here for a backend that can
    /// lose track of what it's serving across an agent restart and so needs
    /// `/status` to ask it directly instead of trusting in-process state. Don't assume `GET
    /// /status`'s response reflects this method's return value -- as of
    /// this PoC, it doesn't.
    fn status(&self) -> Result<ServingStatus>;

    /// The `gozer` capability this backend leases chips through, or `None`
    /// when leasing is off (no gozer on the box, or a backend that doesn't
    /// lease at all, such as [`NullBackend`]).
    ///
    /// **Test-only, and it exists for exactly one assertion**: that the
    /// backend `main.rs` actually constructs is holding the capability
    /// `main.rs` actually probed for. Nothing in production reads this --
    /// each backend reads its own field directly -- but without it, that
    /// wiring is unobservable from outside, which is precisely how it came
    /// to be missing in the first place (every leasing test passed against
    /// backends built by hand in the test, while `make_backend` took no
    /// capability at all and production leased nothing).
    ///
    /// Default `None` covers any backend that doesn't lease. Gated the same
    /// way `DockerBackend::with_health_poll` is (this crate's own tests, or
    /// downstream integration tests via the `test-hooks` feature).
    #[cfg(any(test, feature = "test-hooks"))]
    fn gozer_capability(&self) -> Option<&crate::gozer::Capability> {
        None
    }

    /// The serving status RECONCILED against real serving state, self-correcting
    /// a stale in-memory `Serving` when the serve actually died out of band (a
    /// manual `docker stop`, a crash, a host reboot of the container -- none of
    /// which run through the agent's `/stop`).
    ///
    /// Default: return `status()` unchanged -- correct for backends whose
    /// in-memory status can't drift from reality (`NullBackend`) or where a
    /// hand-rolled `docker run` isn't reliably discoverable (`DockerBackend`,
    /// the best-effort fallback). The `runpy` backend -- the real one -- probes
    /// docker via its own `CommandRunner` and overrides this so `GET /status`
    /// reports Idle once the container it launched is gone.
    fn reconciled_status(&self) -> Result<ServingStatus> {
        self.status()
    }

    /// Return the box to a fresh state for a demo reset (`POST /reset`):
    /// stop whatever's serving and, on backends that manage board state,
    /// reset the board too.
    ///
    /// The default implementation is a no-op that succeeds -- the sane
    /// answer for backends with no serving container or board of their own
    /// to clear (`NullBackend`) and for `DockerBackend`
    /// (whose containers are addressed by model name, which a `reset` with
    /// no model in hand doesn't have; `/reset` still clears the agent's own
    /// tokens/status regardless). `RunPyBackend` is the one backend that
    /// overrides this, since it owns both a stale-container stop path
    /// (`stop_serving_containers`) and a board reset (`reset_cmd`, `tt-smi
    /// -r`) worth running on a reset.
    ///
    /// Sync like every other method on this trait (see the trait doc): a
    /// caller in async context must hop off the runtime (e.g.
    /// `tokio::task::spawn_blocking`) before calling it.
    ///
    /// `force` is the operator's `tt-station reset --force`: it overrides the
    /// foreign-lease refusal (and the "gozer could not be read" refusal) that
    /// `RunPyBackend::reset_forcing` applies by default. It overrides nothing
    /// else -- notably not the stop-before-release ordering, which exists to
    /// stop this tool resetting chips under its own live container.
    ///
    /// **THIS is the method a backend overrides**; [`ServingBackend::reset`]
    /// and [`ServingBackend::reset_forced`] are provided wrappers, so the
    /// zero-argument form every existing caller and argv pin uses still
    /// works. Overriding `reset` instead would silently discard `force`.
    fn reset_forcing(&self, force: bool) -> Result<()> {
        let _ = force;
        Ok(())
    }

    /// [`ServingBackend::reset_forcing`] with no override -- the ordinary
    /// `POST /reset`.
    fn reset(&self) -> Result<()> {
        self.reset_forcing(false)
    }

    /// [`ServingBackend::reset_forcing`] WITH the override -- `POST /reset
    /// {"force": true}`.
    fn reset_forced(&self) -> Result<()> {
        self.reset_forcing(true)
    }

    /// [`ServingBackend::reset_forcing`], also handing back what a `force`d
    /// reset overrode (another tenant's lease, or unreadable lease state) for
    /// the activity log -- `None` when nothing was. `POST /reset` calls this.
    /// Only a backend that runs a lease-checked reset overrides it.
    fn reset_noting(&self, force: bool) -> Result<Option<String>> {
        self.reset_forcing(force).map(|()| None)
    }

    /// Enumerate the models this backend can serve, so a caller (`GET
    /// /models`, `tt-station models`) never has to guess or hardcode a model id --
    /// see `libttstation::model::ModelsResponse`.
    ///
    /// Default implementation reports an empty catalog with no known
    /// release version -- correct for any backend with no model spec of its
    /// own to read (`DockerBackend`, whose image/model are supplied
    /// entirely via CLI flags with no catalog file, and `NullBackend`). `RunPyBackend` is the one backend that overrides
    /// this, since `run.py`'s `model_spec.json` is an actual catalog to
    /// read.
    fn list_models(&self) -> Result<ModelsResponse> {
        Ok(ModelsResponse {
            release_version: None,
            models: vec![],
        })
    }
}

/// Poll `runner.health_ok(url)` up to `attempts` times, sleeping `interval`
/// between attempts, returning `true` as soon as one probe succeeds (or
/// `false` if every attempt is exhausted).
///
/// Shared by every `ServingBackend` that starts a long-lived server process
/// out-of-band (a container, a `run.py` invocation, ...) and needs to block
/// `start` until it's actually answering requests -- `DockerBackend` and
/// `RunPyBackend` both call this rather than each rolling their own
/// poll loop, so the "bounded wait, sleep between attempts" policy lives in
/// exactly one place.
pub(crate) fn poll_until_healthy(
    runner: &dyn docker::CommandRunner,
    url: &str,
    attempts: u32,
    interval: Duration,
) -> bool {
    for _ in 0..attempts {
        if runner.health_ok(url) {
            return true;
        }
        std::thread::sleep(interval);
    }
    false
}

/// Whether the serving `host` (what an `Endpoint`'s `base_url` names) is this
/// box only. Then the model's port is published on loopback alone: it takes
/// no key (`--no-auth`), and everything that uses it -- the gateway, Chat,
/// Code's gate, the apps (via the gateway) -- reaches it as 127.0.0.1 or
/// through the gateway, which checks keys. A host configured as a network
/// address keeps the network publish that address needs.
pub fn loopback_only(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A backend that serves nothing: `start` fails, `stop` succeeds, status is
/// always idle. For an agent (or a test) that needs the API without serving.
/// It runs nothing, so it never takes a gozer lease either.
pub struct NullBackend;

impl ServingBackend for NullBackend {
    fn start_forcing(&self, _model: &str, _force: bool) -> Result<Endpoint> {
        Err(anyhow::anyhow!("this agent has no serving backend"))
    }

    fn stop(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    fn status(&self) -> Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
}

/// Construct a `ServingBackend` for the given `--backend` CLI choice.
///
/// `"runpy"` and `"docker"` are the only recognized kinds;
/// anything else is an error rather than a silent fallback, since a typo'd
/// backend name should fail loudly at startup rather than quietly serving
/// nothing.
///
/// `docker_config`/`runpy_config` are each only meaningful for their own
/// backend; both are threaded through here
/// rather than the individual fields being hardcoded so the CLI wiring in
/// `main.rs` has a single function to call regardless of which backend was
/// chosen, and so adding a new per-backend knob doesn't mean touching this
/// function's signature again.
///
/// ## `gozer`: the production leasing wiring
///
/// `gozer` is the capability the startup probe found (`gozer::probe`, via
/// `main.rs`'s `build_serving_backend`), or `None` on a box without gozer
/// installed -- the normal case, and the one where every backend behaves
/// exactly as it did before leasing existed.
///
/// It is a REQUIRED parameter, deliberately, rather than a `with_gozer`
/// builder call a caller may forget. That is not hypothetical: for the first
/// two tasks of this integration, `RunPyBackend::with_gozer` existed, was
/// thoroughly tested, and was called by nothing but tests -- `main.rs`
/// constructed its backend through this function, which took no capability,
/// and probed for gozer a hundred lines later. Every test passed and no real
/// box ever leased a chip. Making the capability part of the one signature
/// `main.rs` must call means a future backend can't be added to the match
/// below without confronting the question.
pub fn make_backend(
    kind: &str,
    docker_config: docker::DockerConfig,
    runpy_config: runpy::RunPyConfig,
    gozer: Option<crate::gozer::Capability>,
) -> Result<Box<dyn ServingBackend>> {
    match kind {
        "runpy" => Ok(Box::new(
            runpy::RunPyBackend::new(runpy_config, Box::new(docker::RealCommandRunner))
                .with_gozer(gozer),
        )),
        "docker" => Ok(Box::new(
            docker::DockerBackend::new(docker_config, Box::new(docker::RealCommandRunner))
                .with_gozer(gozer),
        )),
        other => Err(anyhow::anyhow!("unknown serving backend: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The capability a successful startup probe would have produced.
    fn capability() -> crate::gozer::Capability {
        crate::gozer::Capability {
            path: "/opt/gozer/bin/gozer".to_string(),
            version: "gozer 0.1.0".to_string(),
        }
    }

    #[test]
    fn make_backend_constructs_runpy_and_docker() {
        assert!(make_backend(
            "runpy",
            docker::DockerConfig::default(),
            runpy::RunPyConfig::default(),
            None
        )
        .is_ok());
        assert!(make_backend(
            "docker",
            docker::DockerConfig::default(),
            runpy::RunPyConfig::default(),
            None
        )
        .is_ok());
    }

    /// THE WIRING TEST. `make_backend` is the ONE function `main.rs` builds
    /// its serving backend through, so a capability that doesn't survive
    /// this call never reaches production no matter how thoroughly the
    /// backends' own leasing is tested.
    ///
    /// That was the actual state of the code before this task: every
    /// leasing test passed, every backend leased correctly when handed a
    /// capability by hand -- and `main.rs` probed for gozer AFTER it had
    /// already constructed the backend, through a `make_backend` that took
    /// no capability at all. The feature was inert on every real box.
    ///
    /// If the `.with_gozer(..)` call is removed from either arm below, this
    /// test fails immediately: the constructed backend reports `None` while
    /// the caller passed `Some`.
    #[test]
    fn make_backend_hands_the_capability_to_the_backend_it_builds() {
        for kind in ["runpy", "docker"] {
            let backend = make_backend(
                kind,
                docker::DockerConfig::default(),
                runpy::RunPyConfig::default(),
                Some(capability()),
            )
            .expect("backend should construct");
            assert_eq!(
                backend.gozer_capability(),
                Some(&capability()),
                "the {kind} backend main.rs builds must hold the probed gozer \
                 capability, or leasing is dead code in production"
            );
        }
    }

    /// The other half of the same contract: no capability (a box without
    /// gozer installed, which is the normal case) must leave every backend
    /// in its pre-leasing state.
    #[test]
    fn make_backend_without_a_capability_leaves_leasing_off() {
        for kind in ["runpy", "docker"] {
            let backend = make_backend(
                kind,
                docker::DockerConfig::default(),
                runpy::RunPyConfig::default(),
                None,
            )
            .expect("backend should construct");
            assert_eq!(
                backend.gozer_capability(),
                None,
                "the {kind} backend must not lease without a capability"
            );
        }
    }

    #[test]
    fn make_backend_rejects_unknown_kind() {
        // `Box<dyn ServingBackend>` isn't `Debug`, so `unwrap_err` (which
        // requires the `Ok` side to be `Debug` for its panic message)
        // doesn't work here -- match instead.
        match make_backend(
            "bogus",
            docker::DockerConfig::default(),
            runpy::RunPyConfig::default(),
            None,
        ) {
            Err(err) => assert!(err.to_string().contains("bogus")),
            Ok(_) => panic!("expected an error for an unknown backend kind"),
        }
    }
}
