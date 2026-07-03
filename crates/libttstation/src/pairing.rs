//! Client side of the pairing handshake (Task 8) -- the counterpart to the
//! `POST /pair/init` and `POST /pair/complete` routes `tt-station-agentd`
//! exposes (Task 7). Lives in `libttstation` rather than the `tt` CLI crate
//! (Task 12) so any future client (CLI, GUI, another service) can drive
//! pairing without reimplementing the HTTP calls.
//!
//! The flow, end to end:
//!   1. [`pair_init`] tells the agent "someone wants to pair"; the agent
//!      mints a `pair_id` and displays a short code on the box's own
//!      screen/log.
//!   2. A human reads that code and types it into whatever's calling this
//!      module.
//!   3. [`pair_complete`] sends the `pair_id` and typed `code` back; if they
//!      match what the agent is holding, it mints and returns a bearer
//!      token the caller can use for authenticated control-plane calls.

use serde::{Deserialize, Serialize};

/// Start a pairing attempt against the agent at `base` (a control-plane
/// base URL such as `http://host:port`, with or without a trailing slash --
/// see the module-level note on [`join`]).
///
/// Returns the `pair_id` the agent minted, to be echoed back in
/// [`pair_complete`] along with the code a human reads off the box.
pub async fn pair_init(base: &str) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct PairInitResponse {
        pair_id: String,
    }

    let resp = reqwest::Client::new()
        .post(join(base, "pair/init"))
        .send()
        .await?
        .error_for_status()
        .map_err(|e| anyhow::anyhow!("pairing init failed: {e}"))?;

    let body: PairInitResponse = resp.json().await?;
    Ok(body.pair_id)
}

/// Finish a pairing attempt: send the `pair_id` from [`pair_init`] together
/// with the `code` a human read off the box's screen. On success, returns
/// the bearer token the agent minted.
///
/// A non-2xx response (e.g. the agent's 401 for a wrong or expired code)
/// comes back as a plain, human-readable `Err` -- callers don't need to
/// inspect a status code to know pairing didn't work.
pub async fn pair_complete(base: &str, pair_id: &str, code: &str) -> anyhow::Result<String> {
    #[derive(Serialize)]
    struct PairCompleteRequest<'a> {
        pair_id: &'a str,
        code: &'a str,
    }

    #[derive(Deserialize)]
    struct PairCompleteResponse {
        token: String,
    }

    let resp = reqwest::Client::new()
        .post(join(base, "pair/complete"))
        .json(&PairCompleteRequest { pair_id, code })
        .send()
        .await?;

    if !resp.status().is_success() {
        // Deliberately vague (matches the agent's own 401 semantics): don't
        // distinguish "wrong code" from "unknown/expired pair_id" so a
        // caller can't use the error to narrow down a guess.
        anyhow::bail!("pairing failed: invalid or expired code");
    }

    let body: PairCompleteResponse = resp.json().await?;
    Ok(body.token)
}

/// Join a base URL and a path segment, tolerating a trailing slash on
/// `base` either way (`http://host:port` and `http://host:port/` both
/// produce `http://host:port/pair/init`) so callers don't have to think
/// about it.
///
/// `pub(crate)` rather than private: `agent_client` (Task 11) talks to the
/// same base-URL-shaped agent addresses and reuses this instead of
/// duplicating the trailing-slash handling.
pub(crate) fn join(base: &str, path: &str) -> String {
    format!("{}/{path}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_handles_trailing_slash_either_way() {
        assert_eq!(
            join("http://host:1234", "pair/init"),
            "http://host:1234/pair/init"
        );
        assert_eq!(
            join("http://host:1234/", "pair/init"),
            "http://host:1234/pair/init"
        );
    }
}
