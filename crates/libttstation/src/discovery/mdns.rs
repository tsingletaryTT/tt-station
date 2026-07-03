//! `MdnsProvider`: discovery via mDNS browsing of `_tenstorrent._tcp`.
//!
//! Real Tenstorrent boxes (and `mock-box`, our dev fixture) advertise
//! themselves as an mDNS service. This provider browses for that service
//! type, waits (up to `timeout`) for resolved instances, and decodes each
//! one's TXT record into a `BoxRecord` via the shared `txt_decode`.

use super::{DiscoveryProvider, SERVICE_TYPE};
use crate::model::{txt_decode, BoxRecord};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Discovers Tenstorrent boxes by browsing the `_tenstorrent._tcp` mDNS
/// service type.
pub struct MdnsProvider;

impl DiscoveryProvider for MdnsProvider {
    /// Browse `SERVICE_TYPE` for up to `timeout`, collecting `ServiceResolved`
    /// events. See [`discover_service_type`] for the actual browse loop --
    /// factored out so tests can point it at a service type other than the
    /// real `SERVICE_TYPE` (see `tests::discover_returns_empty_for_an_unadvertised_service_type`).
    fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<BoxRecord>> {
        discover_service_type(SERVICE_TYPE, timeout)
    }
}

/// Browse `service_type` for up to `timeout`, collecting `ServiceResolved`
/// events.
///
/// A single logical box can surface as *multiple* `ServiceResolved`
/// events -- one per network interface / IP family the daemon sees it
/// on -- so we dedup by the service instance's `fullname` and keep only
/// the first resolution of each. `timeout` bounds the whole browse: the
/// underlying `mdns-sd` browse channel has no deadline of its own, so
/// without this a call here could hang forever if nothing (more)
/// resolves.
fn discover_service_type(service_type: &str, timeout: Duration) -> anyhow::Result<Vec<BoxRecord>> {
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(service_type)?;

    let deadline = Instant::now() + timeout;
    let mut seen_fullnames = std::collections::HashSet::new();
    let mut records = Vec::new();

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(resolved)) => {
                // Dedup on the instance fullname: the same logical
                // service resolves once per interface/IP family, and we
                // only want one BoxRecord per box.
                if !seen_fullnames.insert(resolved.fullname.clone()) {
                    continue;
                }

                let txt: HashMap<String, String> = resolved
                    .txt_properties
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string()))
                    .collect();

                match txt_decode(&resolved.fullname, &resolved.host, resolved.port, &txt) {
                    Ok(rec) => records.push(rec),
                    Err(e) => {
                        eprintln!(
                            "mdns: skipping unparseable service {}: {e:#}",
                            resolved.fullname
                        );
                    }
                }
            }
            Ok(_other_event) => {
                // SearchStarted / ServiceFound / ServiceRemoved /
                // SearchStopped: not actionable here, keep waiting.
            }
            Err(_timeout_or_disconnect) => {
                // Either we hit `remaining`'s deadline or the daemon's
                // channel closed; either way, stop collecting.
                break;
            }
        }
    }

    // Best-effort: stop browsing and shut the daemon down so we don't
    // leak background threads/sockets across repeated `discover` calls.
    let _ = daemon.stop_browse(service_type);
    let _ = daemon.shutdown();

    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NOTE: this deliberately does NOT browse the real `SERVICE_TYPE`
    /// (`_tenstorrent._tcp.local.`) and assert zero results. This test runs
    /// on developer/CI machines that may be on the same LAN as a real
    /// `tt-station-agentd` (or `mock-box`) actively advertising that exact
    /// service -- including this repo's own dev box -- so asserting global
    /// silence on `_tenstorrent._tcp` is inherently flaky: it fails whenever
    /// discovery is doing its job correctly elsewhere on the network. This
    /// bit us for real (see `docs/superpowers/cleanup-analysis.md`'s
    /// "Quick wins" table).
    ///
    /// Instead, browse a randomized, definitely-unadvertised service type
    /// (nobody on the network is advertising a UUID we just generated) and
    /// assert THAT yields nothing within the timeout -- still a real
    /// assertion about `discover_service_type`'s emptiness/timeout behavior,
    /// just decoupled from ambient `_tenstorrent._tcp` traffic.
    #[test]
    fn discover_returns_empty_for_an_unadvertised_service_type() {
        let unique_service_type = format!("_tt-test-{}._tcp.local.", uuid_like_suffix());
        let out = discover_service_type(&unique_service_type, Duration::from_millis(200)).unwrap();
        assert!(out.is_empty());
    }

    /// Cheap process/time-based unique suffix -- no need to pull in a `uuid`
    /// crate dependency just for one test's random-enough service label.
    fn uuid_like_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock before UNIX epoch")
                .as_nanos()
        )
    }
}
