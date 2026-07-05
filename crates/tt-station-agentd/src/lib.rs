//! Library surface for `tt-station-agentd`, split out from `main.rs` so
//! integration tests (`tests/status.rs`) can build the real `Router` against
//! an in-process `AppState` without booting mDNS or binding a real port.

pub mod config;
pub mod pairing;
pub mod routes;
pub mod serving;
pub mod telemetry;
