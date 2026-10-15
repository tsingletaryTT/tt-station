//! HTTP handlers, one module per area of the API. `crate::router` wires
//! them to paths.

pub mod assistant;
pub mod devices;
pub mod logs;
pub mod media;
pub mod pairing;
pub mod power;
pub mod security;
pub mod serving;
pub mod ssh;
pub mod status;
pub mod ttcli;
