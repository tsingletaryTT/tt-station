//! HTTP handlers, one module per area of the API. `crate::router` wires
//! them to paths.

pub mod devices;
pub mod pairing;
pub mod power;
pub mod serving;
pub mod status;
