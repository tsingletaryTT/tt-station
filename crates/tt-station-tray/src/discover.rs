//! Finding boxes: mDNS on the local network plus the stations the person added by hand.
//!
//! This only wires up `libttstation::discovery`. [`ManualProvider`] probes each added host's
//! `/status` and [`MdnsProvider`] browses `_tenstorrent._tcp`; [`aggregate`] runs both and
//! removes duplicates by box name. The `tt-station discover` command uses the same pieces.

use std::time::Duration;

use libttstation::discovery::{
    aggregate,
    manual::{manual_status_fetch, ManualProvider},
    mdns::MdnsProvider,
    DiscoveryProvider,
};
use libttstation::model::BoxRecord;

use crate::registry::KnownStation;

/// Boxes found by mDNS plus every manual station that answers `GET /status` within `timeout`.
///
/// A manual station that does not answer is left out; it is not an error, because "the box is
/// off" is normal. The result is deduplicated by box name, so a box seen both ways appears
/// once. The providers block, so they run on a blocking thread.
pub async fn discover_all(manual: &[KnownStation], timeout: Duration) -> Vec<BoxRecord> {
    run(manual, timeout, true).await
}

/// Like [`discover_all`] but without mDNS: only the manual stations. Used by tests, which must
/// not depend on what else is on the network, and by a "refresh" that should not wait for a
/// full mDNS browse.
pub async fn discover_manual_only(manual: &[KnownStation], timeout: Duration) -> Vec<BoxRecord> {
    run(manual, timeout, false).await
}

async fn run(manual: &[KnownStation], timeout: Duration, mdns: bool) -> Vec<BoxRecord> {
    // The probe wants a host as it goes into a URL, so an IPv6 literal needs its brackets.
    let hosts: Vec<(String, u16)> = manual.iter().map(|k| (k.url_host(), k.port)).collect();
    tokio::task::spawn_blocking(move || {
        let mut providers: Vec<Box<dyn DiscoveryProvider>> = Vec::new();
        if !hosts.is_empty() {
            providers.push(Box::new(ManualProvider::new(hosts, move |host, port| {
                manual_status_fetch(host, port, timeout)
            })));
        }
        if mdns {
            providers.push(Box::new(MdnsProvider));
        }
        aggregate(&providers, timeout)
    })
    .await
    // A panic in a provider yields "found nothing", and the tray keeps running.
    .unwrap_or_default()
}
