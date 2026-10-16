# Releases: signing, keys and the update channel (reference)

*Documents `crates/tt-station-agentd/src/updates.rs`, `release_keys.rs`,
`deploy/release-keys.pub`, `scripts/sign-release.sh` and
`.github/workflows/release.yml`.*

A box updates itself (`GET /updates`, `POST /updates/install`,
`POST /updates/undo`) from the GitHub releases of its **update channel**. Before
a package is unpacked, the agent checks, in order:

1. the tag is a plain version and the package is named for it
   (`tt-station_<version>_amd64_<suite>.deb`);
2. GitHub lists a sha256 for the package, and the download matches it
   (this only catches a broken download: the digest comes from the same place);
3. **the package's `.sig` is a valid Ed25519 signature from a key the box
   trusts** (this is what shows who made it);
4. it isn't older than the running version (versions compare as semver,
   pre-releases included), unless an owner allows a downgrade.

A version already unpacked on the box (from an earlier install) is used again
only if it was unpacked from this release's package (its sha256, noted beside
it) and that package's signature still checks out against today's keys;
otherwise it's downloaded again.

Anything missing or wrong refuses the install with a message naming the
problem (for a bad signature, the keys the box tried, by name and
fingerprint). Updates only apply to `.deb` installs; see
[installing.md](installing.md).

## Signature format

Each package `P` is published with `P.sig`: one line, the base64 of a 64-byte
Ed25519 signature over this statement (each line ending in `\n`):

```text
tt-station-release-v1
file=tt-station_0.12.0_amd64_noble.deb
sha256=<lowercase hex sha256 of the file>
```

Why this and not minisign: minisign's current format signs a BLAKE2b-512
prehash, which `ring` (the agent's only crypto library) doesn't provide, and
its legacy format signs the whole file, which needs either the package in
memory or a second pass. Signing a statement over the sha256 reuses the hash
the download already computes, needs no new dependency, and can be produced and
checked with stock `openssl` (3.0+) and coreutils on any runner or laptop.
Putting the file name in the statement binds the signature to the version in
it, so an old signed package can't be re-published under a newer tag.

Verify a package by hand:

```bash
scripts/sign-release.sh verify tt-station_0.12.0_amd64_noble.deb   # against deploy/release-keys.pub
KEYS=my-keys.pub scripts/sign-release.sh verify P.deb              # against another list
```

or with nothing but openssl (`KEY` is the base64 public key):

```bash
f=tt-station_0.12.0_amd64_noble.deb
printf 'tt-station-release-v1\nfile=%s\nsha256=%s\n' "$f" "$(sha256sum "$f" | cut -d' ' -f1)" > msg
{ printf '\x30\x2a\x30\x05\x06\x03\x2b\x65\x70\x03\x21\x00'; echo "$KEY" | base64 -d; } \
  | openssl pkey -pubin -inform DER -out pub.pem
base64 -d "$f.sig" > sig
openssl pkeyutl -verify -pubin -inkey pub.pem -rawin -in msg -sigfile sig
```

## Trusted keys

The agent trusts:

- **Compiled-in keys**: `deploy/release-keys.pub`, one `<name> <base64 32-byte
  Ed25519 public key>` per line. **Empty today**, until it's decided who holds
  the release key; until then a box installs only releases signed by a key its
  owner added to the channel file.
- **Channel keys**: any listed in the owner's `update-channel.toml` (below)
  **and approved by an owner** with `POST /updates/channel/approve
  {fingerprint}`, in addition to the compiled-in ones. A key in the file that
  no owner approved is listed under `pending_keys` and not trusted. Approval
  only accepts a key already written in the file on the box (never one sent
  in the request), is kept in `update-keys-approved.json` (0600), and is
  recorded in the activity log (`update_key_approve`). At every start the
  agent names each extra key it trusts, and each one waiting, in the journal.

`GET /updates` shows the channel: `repo`, where it came from (`default`,
`channel file`, `environment`), every trusted key's name, origin and
fingerprint, and `unsigned_allowed`.

### Escape hatch (developers only)

`TT_STATION_ALLOW_UNSIGNED_UPDATES=1` **in the agent service's environment**
(a drop-in, `Environment=`) lets a package with a missing or bad signature
through. There is no request field for it. While it's set the agent logs an
error at every start, logs an error for every unverified package it installs,
and the activity log records `update_verified` with `UNVERIFIED (...)`. Never
set it on a box someone else depends on.

## Key management

**Generating** (on an offline or otherwise trusted machine):

```bash
umask 077
openssl genpkey -algorithm ed25519 -out tt-station-2026a.pem
scripts/sign-release.sh pubkey tt-station-2026a.pem    # prints "<name> <base64>"
```

Put that line (with the real name) in `deploy/release-keys.pub` through a
normal reviewed PR.

**Storing**:

- *Signing copy*: the PEM, verbatim, as the secret
  `TT_STATION_RELEASE_SIGNING_KEY` of the GitHub environment **`release`**.
  Restrict that environment to `v*` tags (Settings → Environments → Deployment
  branches and tags) and, ideally, require a reviewer. Only the
  `sign-and-publish` job uses it; the build jobs, which run third-party build
  scripts, never see it.
- *Backup*: the same PEM, offline, in two places the key holders control
  (e.g. an encrypted USB key in a safe and the team's password manager's
  vault). Delete every other copy, including the one on the machine that made
  it.

**Releasing**: pushing a `v*` tag builds each suite's packages, installs each
in a clean Ubuntu container and smoke-tests the agent, then `sign-and-publish`
signs them, **verifies the signatures against `deploy/release-keys.pub` at that
tag**, and uploads `.deb` + `.deb.sig`. No secret, or a key that isn't pinned,
means no release. Manual (`workflow_dispatch`) runs leave unsigned test
packages as Actions artifacts; boxes won't self-update to them.

### Rotating a key (planned)

Boxes only learn keys from releases, so a new key has to reach them before it
signs anything:

1. Generate the new key; add its line to `deploy/release-keys.pub` next to the
   old one.
2. Release **signed with the old key**. Boxes that update to it now trust both.
3. Overlap: keep signing with the old key for at least one more release, or
   until telemetry/support says the fleet is on a release from step 2.
4. Switch the `release` environment's secret to the new key.
5. A release or two later, remove the old key's line. Destroy its backups.

A box that skipped the overlap releases can't take a new-key release directly;
its owner adds the new key to the channel file (or installs the `.deb` by hand)
once.

### Revoking a key (compromise)

1. Remove the compromised key from `deploy/release-keys.pub` and add a new one
   (generate as above); replace the secret.
2. Cut a release **signed with the new key**. Boxes that still trust only the
   compromised key can't take it, which is the point: their owners need to be
   told to install that release's `.deb` by hand (or add the new key to their
   channel file, then update).
3. Delete any release assets signed only by the compromised key, and announce
   the fingerprint that was revoked.

There is no online revocation list; a key stays trusted on a box until the box
runs a release that drops it. Keep the release key offline and the overlap
short.

## The update channel

`update-channel.toml` in the agent's config directory
(`$TT_CONFIG_DIR`, else `~/.config/tt-station/`), written by the box's owner
on the box itself (it is deliberately not settable over the API; a key in it
also needs an owner's approval, above):

```toml
# Follow a company-run release channel.
repo = "example-org/tt-station"

# Trust this key as well as the compiled-in ones. Any number.
[[keys]]
name = "example-2026a"
key = "<base64 Ed25519 public key>"
```

Both entries are optional. Where releases come from, highest first:
`TT_STATION_UPDATE_REPO` in the service's environment, the channel file's
`repo`, then the built-in default (`DEFAULT_REPO` in `updates.rs`). A channel
file that doesn't parse (unknown fields, a repo that isn't `owner/name`, a key
that isn't 32 bytes) stops updates with that error rather than falling back to
the default channel. A running company channel needs its own `release`
environment and key, and its key in every box's channel file (or compiled in,
for a company build).

### Moving the release channel

`DEFAULT_REPO` is a personal repository today and **must move to an org-owned
repository before company-wide rollout** (which one is still being decided).
To move it:

1. Create the org repo with branch protection on the default branch and on
   `v*` tags, the `release` environment and its secret.
2. In this repo, change `DEFAULT_REPO` to the new `owner/name` and release
   from **the old repo** (signed). Boxes that update to it look in the new
   place from then on.
3. Publish the same and later releases from the new repo. Keep the old repo's
   releases (and don't delete the repo) until the fleet has moved: boxes that
   haven't updated still look there.
4. Boxes that need to move sooner: their owner sets `repo` in the channel file.
