# Keyless SSH provisioning during pairing

**Date:** 2026-07-05
**Status:** approved design (concept approved in-session), ready for implementation plan
**Scope:** `crates/tt-station-agentd` (new authed route + authorized_keys writer),
`crates/libttstation` (client call), `crates/tt` (CLI command + Mac-side key read/gen +
default SSH user), `macos/TTStation` (opt-in toggle + default SSH user), `crates/mock-box`.

---

## Problem

The workbench launchers (Terminal, tt-toplike, VS Code Remote-SSH) SSH into the box, but
nothing sets up SSH auth — so they fail on a fresh box unless the user manually installs a
key and knows the right username. Two concrete gaps observed:

1. **No keyless auth.** SSH to the box is denied (`publickey,password`) until the user hand-
   installs their key. VS Code Remote-SSH in particular needs key auth.
2. **Wrong default user.** The launchers default the SSH user to the *Mac* login name
   (`NSUserName()`), but the QuietBox 2 default user is **`ttuser`** — so even a working key
   would target the wrong account.

Meanwhile, pairing already establishes exactly the trust needed to fix this: entering the
console-displayed PIN proves authorized physical access to the box, and pair-complete issues
a persistent bearer token. That's the right moment to also provision keyless SSH.

## Goals

- After a successful pair (opt-in), the Mac's SSH **public** key is installed on the box so
  Terminal / tt-toplike / VS Code Remote-SSH work with no further setup.
- **`ttuser` is the default SSH user** everywhere (QuietBox 2 default), overridable.
- The installed key is **identifiable and revocable** (tagged comment; a revoke path).
- Only the public key ever leaves the Mac; provisioning is authed, opt-in, idempotent.

## Non-goals

- No private key ever transmitted or stored off the Mac.
- No arbitrary-user provisioning: the agent writes exactly one account's `authorized_keys`
  (its own run-user, `ttuser` on QB2) — not any user the client names.
- No change to the control-plane pairing token semantics.
- `~/.ssh/config` Host-block management is out of scope (default key offering + explicit
  `ttuser@host` in the launch args is enough).

---

## Design

### Trust model & the user alignment

Keyless SSH works only when three users line up:
- the **agent's run-user** on the box (whose `authorized_keys` gets the key),
- the **SSH target user** the launchers connect as,
- and the account the user actually wants a shell in.

On QuietBox 2 all three are **`ttuser`** (the box's default login, which the panel-launched
agent also runs as). So the agent writes **its own** `$HOME/.ssh/authorized_keys` (never an
arbitrary user's — that would need root and is a security footgun), and the client defaults
its SSH target user to `ttuser`. Both are overridable for non-default setups; the spec
documents that they must match.

### Component 1 — agent: `POST /ssh/authorize` (authed) + authorized_keys writer

- New **authed** route (bearer token from pairing required — only a proven-paired client can
  call it). Body: `{ "public_key": "<ssh-ed25519 AAAA… comment>", "label": "<identifier>" }`.
- The agent validates the value **looks like an SSH public key** (known key-type prefix:
  `ssh-ed25519` / `ssh-rsa` / `ecdsa-sha2-*` / `sk-*`; single line; no shell metacharacters /
  newlines beyond the trailing one) and **rejects anything resembling a private key**.
- Appends it to the agent run-user's `~/.ssh/authorized_keys`, **idempotently**:
  - Create `~/.ssh` (0700) and `authorized_keys` (0600) if absent.
  - Dedupe on the key body (the base64 blob), so re-pairing doesn't stack duplicates.
  - Append with a trailing marker comment `ttstation:<label>` so it's greppable for revoke.
- Returns `{ "authorized": true, "ssh_user": "<run-user>", "already_present": <bool> }` so the
  client can tell the user exactly which account to connect as.
- **`DELETE /ssh/authorize`** (authed), body `{ "public_key" | "label" }`: removes matching
  line(s) from `authorized_keys` (revocation). Idempotent (absent → ok).
- The target file is derived from the agent's own `$HOME` (its run-user). An advanced
  `--ssh-user <name>` agent flag can override the account whose home is written, but the
  default is the run-user — no privilege escalation in the common case.
- Failures (no `$HOME`, unwritable `.ssh`, malformed key) are non-fatal to the agent and
  returned as a clear error; they never crash it.

### Component 2 — libttstation: client call

- `ssh_authorize(base, token, public_key, label) -> Result<SshAuthorizeResult>` and
  `ssh_revoke(base, token, ...)` — authed POST/DELETE to the route above, mirroring the
  existing `agent_client` call style. `SshAuthorizeResult { authorized, ssh_user, already_present }`.

### Component 3 — tt CLI

- **Default SSH user → `ttuser`.** Introduce a single source of truth
  `DEFAULT_SSH_USER = "ttuser"`. Any place the CLI resolves an SSH user uses
  `override ?? DEFAULT_SSH_USER` (was `?? current_login`).
- **Mac-side key handling** (the CLI runs on the Mac, keeping the app a veneer):
  - Prefer an existing public key in this order: `~/.ssh/id_ed25519.pub`, `~/.ssh/id_rsa.pub`.
  - If none exists, **generate** an ed25519 keypair non-interactively:
    `ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519 -C "ttstation:<mac-hostname>"`.
  - Read the `.pub` and send it; never read/transmit the private key.
- **New command `tt ssh-authorize --host <h>`** (authed): resolves/gens the key, computes a
  label `ttstation:<mac-hostname>:<YYYY-MM-DD>` (date passed in / stamped by caller since the
  binary has no wall-clock in tests), calls the route, and prints the `ssh_user` to connect as.
  `--revoke` flag calls the delete route.
- **`tt pair … --enable-ssh`**: after a successful `pair-complete`, runs the ssh-authorize flow
  (opt-in; the app drives this).
- `--json` output includes `{ authorized, ssh_user, already_present, public_key_path }`.

### Component 4 — macOS app

- **Default SSH user → `ttuser`.** `SSHTarget.resolve` and the `LaunchController` sshTarget
  helper default to `ttuser` when `tt.sshUser` UserDefaults is unset (was `NSUserName()`).
  This aligns the workbench launchers with where the key is installed and fixes the earlier
  "VS Code opens but SSH auth fails" symptom.
- **Opt-in toggle in the pair flow.** After the code-entry step, a checkbox **"Also enable
  Terminal / SSH access (installs this Mac's key as `ttuser`)"**, default **on**. On a
  successful pair, if enabled, the app calls `tt ssh-authorize --host <h> --json` and surfaces
  a one-line result ("SSH enabled as ttuser") or a non-fatal error. Pairing itself succeeds
  regardless of the SSH step.
- Pure, testable: the "which key file / generate?" decision and the label format live in
  tested helpers; the actual `ssh-keygen`/route call is owner-verified I/O.

### Component 5 — mock-box

- Implement `POST /ssh/authorize` + `DELETE /ssh/authorize` against a **temp dir**
  `authorized_keys` (never the real `~/.ssh`), returning `ssh_user: "ttuser"`, so the CLI/app
  flow is verifiable end-to-end with no hardware and no risk to the dev machine's SSH.

---

## Security

- **Public-key only.** The route rejects anything that isn't a well-formed single-line SSH
  public key; explicitly rejects private-key material. The private key never leaves the Mac.
- **Authed.** Requires the pairing bearer token — the PIN handshake is the trust anchor.
- **Idempotent + tagged.** Dedupe on the key blob; every installed line carries a
  `ttstation:<label>` marker for audit and one-command revoke.
- **No privilege escalation.** The agent writes only its own run-user's `authorized_keys`; it
  does not accept an arbitrary target account.
- **Opt-in + honest.** The toggle names exactly what it does (installs a key as `ttuser`);
  revocation is available (`tt ssh-authorize --revoke`, and optionally on unpair).

## Testing

- **Rust (agent), TDD:** key-shape validation (accepts ed25519/rsa/ecdsa/sk; rejects private
  keys, multi-line, injection); idempotent append (no duplicate on re-add); dedupe-by-blob;
  file/dir creation with correct perms (0700/0600); revoke removes only the matching line;
  writes to a temp `$HOME` (never the real one) in tests.
- **Rust (CLI), TDD:** `DEFAULT_SSH_USER == "ttuser"`; key-file selection order
  (id_ed25519.pub before id_rsa.pub; generate when absent — the generate branch is
  owner-verified, the selection logic is pure); label format.
- **Swift, TDD:** `SSHTarget.resolve` defaults to `ttuser` (override still wins); the
  key/label helpers are pure and tested.
- **No-hardware e2e:** `mock-box` route exercised by the CLI against a temp authorized_keys.
- **Live:** against QB2 — pair with `--enable-ssh`, confirm `ttuser@qb2-lab.local` accepts the
  key and VS Code / Terminal connect without a password (requires the redeployed agent with
  this route).

## Versioning & docs

- Bump the app (0.3.x) and note the new pairing option + `ttuser` default.
- `macos/README.md` + `CLAUDE.md`: document the keyless-SSH-on-pair flow, the `ttuser`
  default (overridable via `tt.sshUser`), and revocation.

## Risks / open questions

- **Agent run-user ≠ ttuser.** If an operator runs the agent as a non-`ttuser` account, the
  key lands in the wrong `authorized_keys`; the `ssh_user` field in the response surfaces the
  actual account, and `--ssh-user` overrides it. Documented, not silently wrong.
- **Host-key trust.** Terminal uses `accept-new`; VS Code prompts once. Unchanged.
- **Multiple Macs.** Each appends its own key (dedup is per-blob), so several Macs can pair.
