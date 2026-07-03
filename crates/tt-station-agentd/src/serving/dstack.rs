//! `dstack` serving backend -- **intentional stub**.
//!
//! `dstack` (a confidential-VM orchestrator) is the direction the project
//! is headed for M4: instead of a plain `docker run` on the box, model
//! serving would run inside an attested confidential VM. That work isn't
//! part of this PoC. This stub exists purely so the `ServingBackend` trait
//! has two implementations from day one -- proving out the abstraction
//! boundary now, so nothing above it (agent, CLI, Mac client) needs to
//! change shape when the real dstack backend eventually lands.

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
