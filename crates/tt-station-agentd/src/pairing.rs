//! Randomness for the pairing flow (Task 7): the 6-digit code a human reads
//! off the box and types into the client, the opaque `pair_id` that
//! correlates a `/pair/init` call with the matching `/pair/complete` call,
//! and the bearer token minted once pairing succeeds.
//!
//! Kept in its own module (rather than inline in `routes.rs`) so all the
//! actual randomness for the agent lives in one place that's easy to audit
//! -- and easy to swap later (e.g. if `rand`'s OS-backed default RNG ever
//! needs to change) without touching handler logic.

use rand::Rng;

/// Generate a 6-digit numeric pairing code, e.g. `"042817"`.
///
/// Zero-padded via the `{:06}` format so the result is always exactly six
/// ASCII digits -- callers (and the box's own display) never have to handle
/// a shorter string for small values.
pub fn issue_code() -> String {
    let code: u32 = rand::rng().random_range(0..1_000_000);
    format!("{code:06}")
}

/// Generate an opaque `pair_id` used only to correlate a `/pair/init` call
/// with the `/pair/complete` call that follows it. It doesn't need to be a
/// secret (the code is what proves the human read the box's screen) -- just
/// unique enough that concurrent pairing attempts don't collide.
///
/// 16 random bytes hex-encoded (128 bits) makes collisions practically
/// impossible without pulling in a `uuid` dependency for what's really just
/// a random correlation key.
pub fn issue_pair_id() -> String {
    hex_encode(rand::rng().random::<[u8; 16]>())
}

/// Generate a URL-safe bearer token: 32 random bytes (256 bits) hex-encoded.
/// Hex is already URL-safe (alphabet is `[0-9a-f]`) and needs no extra
/// dependency the way base64url would.
pub fn issue_token() -> String {
    hex_encode(rand::rng().random::<[u8; 32]>())
}

/// Lower-case hex encoding, shared by `issue_pair_id` and `issue_token` so
/// there's exactly one place that decides what "URL-safe random string"
/// looks like on the wire.
fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_code_is_always_six_digits() {
        // Run a bunch of times so a low-value code (which needs the
        // zero-padding to still be 6 characters) is virtually guaranteed to
        // show up.
        for _ in 0..1000 {
            let code = issue_code();
            assert_eq!(code.len(), 6, "code {code:?} was not 6 characters");
            assert!(
                code.chars().all(|c| c.is_ascii_digit()),
                "code {code:?} had a non-digit"
            );
        }
    }

    #[test]
    fn issue_pair_id_and_issue_token_are_hex_and_differ_per_call() {
        let a = issue_pair_id();
        let b = issue_pair_id();
        assert_ne!(a, b, "two calls produced the same pair_id");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a.len(), 32); // 16 bytes -> 32 hex chars

        let t = issue_token();
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(t.len(), 64); // 32 bytes -> 64 hex chars
    }
}
