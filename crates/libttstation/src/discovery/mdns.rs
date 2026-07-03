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
    /// events.
    ///
    /// A single logical box can surface as *multiple* `ServiceResolved`
    /// events -- one per network interface / IP family the daemon sees it
    /// on -- so we dedup by the service instance's `fullname` and keep only
    /// the first resolution of each. `timeout` bounds the whole browse: the
    /// underlying `mdns-sd` browse channel has no deadline of its own, so
    /// without this a call here could hang forever if nothing (more)
    /// resolves.
    fn discover(&self, timeout: Duration) -> anyhow::Result<Vec<BoxRecord>> {
        let daemon = ServiceDaemon::new()?;
        let receiver = daemon.browse(SERVICE_TYPE)?;

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
        let _ = daemon.stop_browse(SERVICE_TYPE);
        let _ = daemon.shutdown();

        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_returns_empty_when_nothing_advertising() {
        // No mock-box is running in the unit-test environment, so a short
        // browse should time out cleanly with zero records rather than
        // erroring or hanging.
        let provider = MdnsProvider;
        let out = provider.discover(Duration::from_millis(200)).unwrap();
        assert!(out.is_empty());
    }
}
