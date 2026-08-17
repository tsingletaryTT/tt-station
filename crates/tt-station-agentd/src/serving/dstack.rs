//! `dstack` serving backend -- **intentional stub**.
//!
//! `dstack` (a confidential-VM orchestrator) is the direction the project
//! is headed for M4: instead of a plain `docker run` on the box, model
//! serving would run inside an attested confidential VM. That work isn't
//! part of this PoC. This stub exists purely so the `ServingBackend` trait
//! has two implementations from day one -- proving out the abstraction
//! boundary now, so nothing above it (agent, CLI, Mac client) needs to
//! change shape when the real dstack backend eventually lands.
//!
//! ## gozer leasing is a deliberate no-op here
//!
//! `RunPyBackend` and `DockerBackend` both take a gozer chip lease before
//! they launch anything and release it when they stop (see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md`, whose
//! backend table lists dstack as "none -- no-op, leasing skipped"). This stub
//! deliberately does NOT, and `serving::make_backend` drops the capability on
//! its `dstack` arm rather than handing it over. Three reasons, in order of
//! severity:
//!
//! 1. **It could not release one.** This struct holds no `CommandRunner` --
//!    it has nothing to run and nothing to run it through -- so a lease taken
//!    here could never be handed back through `gozer release`. gozer would
//!    hold those chips against agentd's pid until the daemon exits.
//! 2. **It could not pin one.** `start` fails before anything runs, so there
//!    is no container, no argv, and no device selector for granted chips to
//!    be applied to. A lease with nothing to protect protects nothing.
//! 3. **It would take chips from a neighbour for no work.** Acquiring is not
//!    free: it removes a board from the pool other tenants can be granted.
//!    Doing that on a backend that serves nothing is pure denial of service.
//!
//! When the real dstack backend lands, leasing belongs in it -- confidential
//! VMs get device passthrough too, and whatever selector that uses is where a
//! grant's `dev_indices` apply. Until then this file must stay leasing-free,
//! and `tests/serving.rs`'s `dstack_ignores_a_gozer_capability` asserts it.

use anyhow::Result;
use libttstation::model::{Endpoint, ServingStatus};

use super::ServingBackend;

/// Placeholder `ServingBackend` for the dstack direction. Holds no state --
/// there's nothing to start yet.
pub struct DstackBackend;

impl ServingBackend for DstackBackend {
    /// Always fails: dstack integration isn't implemented yet. Fails loudly
    /// (rather than silently no-op'ing "success") so a caller can't
    /// mistake a stub for a working backend -- e.g. accidentally shipping
    /// `--backend dstack` and having it look like serving started when it
    /// didn't.
    fn start(&self, _model: &str) -> Result<Endpoint> {
        Err(anyhow::anyhow!("dstack backend not implemented (M4)"))
    }

    /// There is never anything running to stop, so this is a harmless
    /// no-op rather than an error -- callers that unconditionally call
    /// `stop` during cleanup shouldn't have to special-case dstack.
    fn stop(&self, _model: &str) -> Result<()> {
        Ok(())
    }

    /// Nothing can ever be serving via this stub, so status is always
    /// `Idle`.
    fn status(&self) -> Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
}
