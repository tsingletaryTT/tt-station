//! The tray's model of "somewhere a model can run".
//!
//! A [`station::Station`] is either this machine ([`local::LocalStation`], which drives the
//! official `tt` CLI) or a paired QuietBox ([`remote::RemoteStation`], which talks to the box's
//! `tt-station-agentd` over HTTP). The UI holds `Station` values and never branches on which
//! kind it has.

pub mod discover;
pub mod local;
pub mod menu_model;
pub mod pairing_flow;
pub mod registry;
pub mod remote;
pub mod sni_host;
pub mod station;
pub mod tray;
pub mod window;

pub use station::{fits, ModelRow, ServingNow, Station, StationId, StationKind, StationState};
