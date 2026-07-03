//! Integration test for `MdnsProvider` against a real, running `mock-box`.
//!
//! This is NOT run as part of the normal `cargo test -p libttstation` suite
//! (it's `#[ignore]`d) because it needs a `mock-box advertise` process
//! actually up and advertising on the LAN via mDNS. To run it:
//!
//! ```bash
//! cargo run -p mock-box -- advertise --name qb2-it --ctrl-port 8765 &
//! TT_MOCK_NAME=qb2-it cargo test -p libttstation --test mdns_integration -- --ignored --nocapture
//! kill %1
//! ```
//!
//! The test reads the expected box name from the `TT_MOCK_NAME` env var
//! (rather than hardcoding it) so the caller's `mock-box advertise --name`
//! and this assertion can't silently drift apart.

use libttstation::discovery::mdns::MdnsProvider;
use libttstation::discovery::DiscoveryProvider;
use std::time::Duration;

#[test]
#[ignore = "needs a running `mock-box advertise` process on the LAN"]
fn discovers_running_mock_box_by_name() {
    let expected_name = std::env::var("TT_MOCK_NAME")
        .expect("set TT_MOCK_NAME to the --name a mock-box was advertised with");

    let provider = MdnsProvider;
    let found = provider
        .discover(Duration::from_secs(2))
        .expect("mDNS discovery should not error");

    assert!(
        found.iter().any(|rec| rec.name == expected_name),
        "expected to find a box named {expected_name:?} among discovered records: {found:?}"
    );
}
