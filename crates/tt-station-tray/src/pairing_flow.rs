//! Pairing a box from the tray, and looking up the token afterwards.
//!
//! Nothing here invents a storage scheme. The handshake is `libttstation::pairing` and the
//! token goes into the same [`SecretStore`] the `tt-station` CLI uses
//! ([`libttstation::secrets::store_from_env`]), under the same key (`"<host>:<port>"`, which is
//! also the [`StationId`]). A box paired with `tt-station pair` is therefore already paired in
//! the tray, and a box paired in the tray works with `tt-station run`. The station registry
//! never holds a token.

use anyhow::{Context, Result};
use libttstation::pairing::{pair_complete, pair_init};
use libttstation::secrets::{store_from_env, SecretStore};

use crate::station::StationId;

/// The token store shared with the CLI: `$TT_CONFIG_DIR/secrets.json` when that variable is
/// set, otherwise the platform default. Open it per call; it is cheap and reads fresh state,
/// so a pairing done by the CLI a moment ago is seen.
pub fn open_store() -> Box<dyn SecretStore> {
    store_from_env()
}

/// The pairing token for `id`, if the box has been paired from this app or the CLI.
pub fn token_for(store: &dyn SecretStore, id: &StationId) -> Result<Option<String>> {
    store.get(&id.0)
}

/// Pair with the box whose agent is at `base` (`http://host:port`).
///
/// Steps: ask the box to mint a pairing (it shows a 6-digit code on its own screen), call
/// `ask_code` to get the code the person read off that screen, exchange the two for a token,
/// and only then store the token. If the box refuses the code (wrong, expired, locked out)
/// this returns the error and writes nothing to `store`.
///
/// `ask_code` runs after the box has been contacted, so it is never asked of a person when the
/// box is unreachable. It is a plain closure because the caller may block on a dialog.
pub async fn pair_station(
    base: &str,
    ask_code: impl FnOnce() -> String,
    store: &dyn SecretStore,
) -> Result<()> {
    let pair_id = pair_init(base).await.context("starting pairing")?;
    let code = ask_code();
    let token = pair_complete(base, &pair_id, code.trim())
        .await
        .context("the box did not accept that pairing code")?;
    // Store last: a refused code above has already returned.
    store
        .set(&StationId::from_base(base).0, &token)
        .context("saving the pairing token")?;
    Ok(())
}
