# `tt-station` CLI: HTTPS and certificate pinning (reference)

*Documents `crates/libttstation/src/transport.rs`, `crates/tt-station/src/remote.rs`
and the pairing commands in `crates/tt-station/src/main.rs`.*

## Why

A box serves its control plane over plain HTTP **only to itself**; every other
computer must use HTTPS on the TLS port (the control port + 1, `8766` by default;
mDNS TXT `tls`). The box's certificate is its own (self-signed, made once and
kept), so trust comes from **pinning** its SHA-256 fingerprint, not from a
certificate authority -- the same model as the desktop app and the phone.

## How the CLI reaches a box

| Address you give (`--host` / `pair <host:port>`) | Connection |
|---|---|
| `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>` | plain HTTP on that port (this computer is the box) |
| anything else (`qb2.local:8765`, `192.168.1.20:8765`, even `127.0.0.2`) | HTTPS on the stored TLS port, accepting only the pinned certificate |

You keep typing the address you paired with (usually the control port, `:8765`);
the stored pairing knows the HTTPS port. Every request -- `run`, `stop`,
`status`, `logs --follow` (`wss://`), `discover --host`, `reset --host`,
`power` -- checks the certificate before anything is sent. The handshake
signature is still verified, so the server must hold the pinned certificate's key.

A box that presents a different certificate is refused:

```
Error: certificate changed: qb2.local:8766 presented a certificate with SHA-256 19e8…7a0f,
but this computer paired with 27cb…2da5. Nothing was sent. Either the box was reinstalled
(its certificate is made once and kept) or something on the network is answering for it.
If you're sure it's your box, check the fingerprint on the box (Settings → Pairing and
security) and pair again.
```

The pin isn't replaced automatically: pairing again (`tt-station pair <host:port>`)
is how you accept a new certificate.

## Pairing

```
tt-station pair <host:port> [--code <code>] [--fp <sha256>] [--enable-ssh]
tt-station pair --link 'ttstation://pair?...' [--enable-ssh]
tt-station pair-init <host:port> [--fp <sha256>]
tt-station pair-complete <host:port> --pair-id <id> --code <code> [--fp <sha256>] [--enable-ssh]
```

Before anything is sent, `pair` finds the box's HTTPS (`GET /tls` on the port you
gave, else the one above it), checks that the certificate it presented is the one
the box reports, and prints the fingerprint on stderr:

```
qb2.local:8765: HTTPS on port 8766, certificate SHA-256
  27cb59aadcad9af36c82f4927cdf398030ead4eea04b68f3bc562c56f7bf2da5
  (27cb 59aa dcad 9af3 -- the box shows this in TT Station → Settings → Security and activity → Encryption; check it matches)
```

- `--fp` checks it for you: the full 64 hex digits or the first 16 or more (what the
  box's screen shows; `:` and spaces are fine). A mismatch stops pairing with
  "certificate mismatch" and nothing is sent.
- `--link` takes the pairing link from TT Station on the box (Settings → Pairing and
  security → Pair another computer). It carries the address, the HTTPS port, the code
  and the certificate fingerprint, so no code is typed and the certificate is checked
  against the link. Links without `tls`/`fp` (a box too old to pair securely) are refused.
- `pair-init --json` returns `tls: {port, sha256}`; pass `sha256` to `pair-complete --fp`
  so both halves talk to the same certificate.

The token and the pin are stored together under the address you typed, in the CLI's
secret store (macOS Keychain, or `~/.config/tt-station/secrets.json`, `0600`), as one
JSON value: `{"token": "...", "tls_port": 8766, "sha256": "..."}`. `--json` output of
`pair`/`pair-complete` gains `tls` (`null` on the box itself).

## Pairings from before HTTPS (migration)

Pairings made by older CLIs stored only the token. The first command that uses one
looks up the box's HTTPS as above, pins the certificate it presents (**trust on first
use**), stores it with the token, and says so once on stderr:

```
note: qb2.local:8765 was paired before TT Station used HTTPS. Its certificate is now pinned
(trust on first use) and stored with your pairing:
  SHA-256 27cb59aa…
  HTTPS port 8766
From now on this computer talks to it only over HTTPS and only with that certificate. …
```

If the box's HTTPS can't be reached, the command fails and the old pairing is left as
it was. To verify the pin instead of trusting it, run `tt-station pair <host:port> --fp
<what the box shows>` again. (An older CLI can't read the new stored value; downgrading
means pairing again.)

## Commands that work unpaired

`status`, `models`, `serving`, `catalog` and `discover --host` work against a box you
haven't paired with. From another computer they still use HTTPS: the certificate the
box presents is accepted for that one run (no token or other secret is sent) and
nothing is stored.

## Testing

- `cargo test -p libttstation --test transport`: pin match, mismatch ("certificate
  changed"), a box whose `/tls` disagrees with its certificate, the port fallback, and
  that an unpinned self-signed box isn't trusted.
- `remote.rs` unit tests: migration (stored, noted, registered), pinned pairings skip
  the probe, the box itself stays plain HTTP, unpaired boxes store nothing, a failed
  migration keeps the old pairing, pairing links.
- `cargo test -p tt-station --test tls_pinning_mock -- --ignored`: the real binary
  against `mock-box serve --tls-port` on `127.0.0.2` -- pair, run, `logs --follow`,
  `--fp` and `--link` checks, migration, and a restarted box with a new certificate
  being refused.

`mock-box serve --ctrl-port P --tls-port Q [--tls-dir D]` serves the same API over
HTTPS (certificate from the agent's own `tls::identity`, kept in `D`, else a fresh one
each run) plus `GET /tls`.
