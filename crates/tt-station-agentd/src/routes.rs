//! Where the HTTP API used to live in one file. It is now split up:
//!
//! - `crate::state` -- `AppState`, the shared state every handler reaches through
//! - `crate::auth` -- the bearer-token extractors and the auth layers
//! - `crate::error` -- the `{ "error": "..." }` body routes answer failures with
//! - `crate::router` -- the route table (`app()`, `local_http_app()`)
//! - `crate::handlers` -- the handlers, one module per area
//!
//! This module re-exports all of them so `crate::routes::X` paths keep
//! working. New code should import from the module that defines the item.

pub use crate::auth::*;
#[allow(unused_imports)]
pub(crate) use crate::error::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::assistant::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::devices::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::docs::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::logs::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::media::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::memories::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::pairing::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::power::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::security::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::serving::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::ssh::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::status::*;
#[allow(unused_imports)]
pub use crate::handlers::telemetry::*;
#[allow(unused_imports)]
pub(crate) use crate::handlers::ttcli::*;
pub use crate::router::*;
pub use crate::state::*;
