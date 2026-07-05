# Keyless SSH Provisioning During Pairing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** During (opt-in) pairing, install the Mac's SSH public key on the box so Terminal / tt-toplike / VS Code Remote-SSH work with no manual setup, defaulting the SSH user to `ttuser`.

**Architecture:** A new **authed** agent route appends a client-supplied SSH *public* key to the agent run-user's `~/.ssh/authorized_keys` (idempotent, tagged, revocable). The `tt` CLI reads/generates the Mac keypair and calls the route; the app drives it as an opt-in step after pair-complete. The SSH-user default moves from the Mac login name to `ttuser` across CLI + app. Logic stays in Rust; the app is a veneer.

**Tech Stack:** Rust (axum agent, clap CLI, mock-box, libttstation), Swift 5 / SwiftUI, `ssh-keygen`, `swift test`, `cargo test`.

## Global Constraints

- **Veneer rule:** control + key handling live in Rust (`tt`/agent); the app shells out to `tt --json`. No new HTTP in Swift.
- **Public key only:** the private key NEVER leaves the Mac; the agent route rejects private-key material and anything not a well-formed single-line SSH public key.
- **Default SSH user = `ttuser`** (QuietBox 2 default), overridable via the app's `tt.sshUser` UserDefaults and the CLI's user override. Single source of truth constant `DEFAULT_SSH_USER = "ttuser"`.
- **Authed:** `/ssh/authorize` (POST + DELETE) requires the pairing bearer token, same auth as `/run`/`/stop`.
- **Agent writes its OWN run-user's `authorized_keys`** (`$HOME/.ssh/authorized_keys`), never an arbitrary named account. Advanced `--ssh-user` agent flag overrides the account whose home is targeted; default is the run-user.
- **Idempotent + tagged:** dedupe on the base64 key blob; each installed line ends with a `ttstation:<label>` marker. Perms: `~/.ssh` 0700, `authorized_keys` 0600.
- **Tests never touch the real `~/.ssh`** — Rust tests use a temp `$HOME`/path; mock-box uses a temp dir.
- Pure logic is TDD (RED→GREEN→commit). Process/socket/SwiftUI I/O is owner-verified, matching repo convention.
- App version bump on completion.

---

## Task 1: Agent `authorized_keys` writer (Rust, pure)

**Files:**
- Create: `crates/tt-station-agentd/src/authkeys.rs`
- Modify: `crates/tt-station-agentd/src/lib.rs` (`pub mod authkeys;`)

**Interfaces:**
- Produces:
  - `pub fn validate_public_key(s: &str) -> Result<&str, AuthKeyError>` — accepts a single-line SSH public key (`ssh-ed25519`/`ssh-rsa`/`ecdsa-sha2-*`/`sk-ssh-*`/`sk-ecdsa-*`), returns the trimmed line; errors on private-key material, multi-line, empty, or unknown type.
  - `pub fn key_blob(pubkey: &str) -> Option<&str>` — the base64 middle field (dedupe identity).
  - `pub fn authorize(path: &Path, pubkey: &str, label: &str) -> Result<AuthorizeOutcome>` — idempotent append to `authorized_keys` at `path` (create dir 0700 / file 0600), returns `{ already_present: bool }`; dedupes on `key_blob`; appended line is `"<pubkey> ttstation:<label>\n"` (only add the marker if the key had no comment collision — always append the marker as a trailing token).
  - `pub fn revoke(path: &Path, pubkey_or_label: &Revoke) -> Result<()>` — remove matching line(s) by key blob or by `ttstation:<label>` marker; absent → Ok.
  - `pub enum AuthorizeOutcome { Added, AlreadyPresent }`, `pub enum Revoke { Blob(String), Label(String) }`, `pub struct AuthKeyError(String)`.

- [ ] **Step 1: Write failing tests** in `authkeys.rs` (use `tempfile`-style temp dir — check the crate's dev-deps; if none, use `std::env::temp_dir()` + a unique subdir keyed off a passed-in name, and clean up):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ttauthkeys-{name}"));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.join("authorized_keys")
    }

    #[test]
    fn accepts_ed25519_and_rejects_private_key() {
        assert!(validate_public_key("ssh-ed25519 AAAAC3Nz... user@host").is_ok());
        assert!(validate_public_key("ssh-rsa AAAAB3Nz... x").is_ok());
        assert!(validate_public_key("-----BEGIN OPENSSH PRIVATE KEY-----").is_err());
        assert!(validate_public_key("not a key").is_err());
        assert!(validate_public_key("ssh-ed25519 AAAA\nssh-ed25519 BBBB").is_err()); // multi-line
        assert!(validate_public_key("").is_err());
    }

    #[test]
    fn authorize_creates_and_is_idempotent() {
        let p = tmp("idem");
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1 alice@mac";
        assert!(matches!(authorize(&p, key, "mac:2026-07-05").unwrap(), AuthorizeOutcome::Added));
        // same key blob again -> AlreadyPresent, no duplicate line
        assert!(matches!(authorize(&p, key, "mac:2026-07-05").unwrap(), AuthorizeOutcome::AlreadyPresent));
        let body = fs::read_to_string(&p).unwrap();
        assert_eq!(body.matches("AAAAC3NzaC1lZDI1").count(), 1);
        assert!(body.contains("ttstation:mac:2026-07-05"));
        // perms 0600 on file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn revoke_removes_only_matching_line() {
        let p = tmp("revoke");
        authorize(&p, "ssh-ed25519 AAAAKEEP keep@mac", "keep").unwrap();
        authorize(&p, "ssh-ed25519 AAAADROP drop@mac", "drop").unwrap();
        revoke(&p, &Revoke::Label("drop".into())).unwrap();
        let body = fs::read_to_string(&p).unwrap();
        assert!(body.contains("AAAAKEEP"));
        assert!(!body.contains("AAAADROP"));
        // revoking absent is ok
        assert!(revoke(&p, &Revoke::Label("nope".into())).is_ok());
    }
}
```

- [ ] **Step 2: Register module** — add `pub mod authkeys;` to `crates/tt-station-agentd/src/lib.rs`.

- [ ] **Step 3: Run, expect FAIL** — `cargo test -p tt-station-agentd authkeys`.

- [ ] **Step 4: Implement `authkeys.rs`** — `validate_public_key` checks the first token is a known key-type and there's exactly one line with a base64 second field and no `BEGIN ... PRIVATE KEY`; `key_blob` returns the 2nd whitespace field; `authorize` reads existing lines, returns `AlreadyPresent` if any line's blob matches, else creates `~/.ssh` (0700) + file (0600) and appends `"<pubkey> ttstation:<label>\n"`; `revoke` rewrites the file without lines matching the blob or trailing `ttstation:<label>` marker. Use `std::os::unix::fs::PermissionsExt` for perms (guard `#[cfg(unix)]`).

- [ ] **Step 5: Run, expect PASS** — `cargo test -p tt-station-agentd authkeys`.

- [ ] **Step 6: Commit** — `git commit -m "feat(agent): authorized_keys writer (validate/authorize/revoke, idempotent)"`

---

## Task 2: Agent routes `POST`/`DELETE /ssh/authorize` (authed)

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (routes + AppState ssh-target path)
- Modify: `crates/tt-station-agentd/src/main.rs` (`--ssh-user` flag → resolved target home; default = run-user `$HOME`)

**Interfaces:**
- Consumes: `authkeys::{validate_public_key, authorize, revoke}` (Task 1).
- Produces: authed `POST /ssh/authorize {public_key,label}` → `{authorized:bool, ssh_user:String, already_present:bool}`; authed `DELETE /ssh/authorize {label?|public_key?}` → `{revoked:bool}`. `AppState` gains an `ssh_authorized_keys_path: PathBuf` + `ssh_user: String` (default from `$HOME`/run-user).

- [ ] **Step 1: Study the existing authed-route pattern.** In `routes.rs`, read how `/run`/`/stop`/`/reset` are bearer-gated (the auth extractor/middleware) and how `AppState` builder fields are added (`with_*` + `Arc::get_mut`). Replicate for the new field(s): `with_ssh_target(path: PathBuf, user: String)`.

- [ ] **Step 2: Write route tests** (temp path, in `tests/` or the routes test module): a `POST /ssh/authorize` with a valid key + valid bearer → 200, body `authorized:true`, file at the temp path contains the key; without/invalid bearer → 401/403; a private-key body → 400. A `DELETE` with the label → 200 and the line gone. Build `AppState` with `.with_ssh_target(temp_authorized_keys, "ttuser".into())`.

- [ ] **Step 3: Run, expect FAIL.**

- [ ] **Step 4: Implement.** Add the two handlers behind the same auth as `/run`; POST validates via `authkeys::validate_public_key` (400 on error) then `authkeys::authorize(state.ssh_path(), key, label)`; response `ssh_user` = `state.ssh_user()`. DELETE calls `authkeys::revoke`. Register both routes in `app()`.

- [ ] **Step 5: Resolve the target in `main.rs`.** Default `ssh_user` = the agent's run-user (`$USER`/`whoami`), `authorized_keys_path` = `$HOME/.ssh/authorized_keys`. Add `--ssh-user <name>` to override the account (resolve its home via the passwd db / `/home/<name>`); chain `.with_ssh_target(path, user)`. Non-fatal if `$HOME` unresolved (route then returns a clear error).

- [ ] **Step 6: Run + build, expect PASS** — `cargo test -p tt-station-agentd && cargo build -p tt-station-agentd`.

- [ ] **Step 7: Commit** — `git commit -m "feat(agent): authed POST/DELETE /ssh/authorize (ttuser default target)"`

---

## Task 3: libttstation client — `ssh_authorize` / `ssh_revoke`

**Files:**
- Modify: `crates/libttstation/src/agent_client.rs` (+ a small result type, in `model.rs` or here)

**Interfaces:**
- Produces: `AgentClient::ssh_authorize(&self, public_key: &str, label: &str) -> Result<SshAuthorizeResult>` and `ssh_revoke(&self, by: SshRevokeBy) -> Result<()>`; `struct SshAuthorizeResult { authorized: bool, ssh_user: String, already_present: bool }`; `enum SshRevokeBy { Label(String), PublicKey(String) }`.

- [ ] **Step 1: Study `agent_client.rs`** — the authed request pattern (bearer header, JSON body, error mapping) used by `run`/`stop`.
- [ ] **Step 2: Add a client test** mirroring existing agent_client tests (against a stub/mock or the existing test harness) that asserts the request shape + decodes `SshAuthorizeResult`.
- [ ] **Step 3: Run, expect FAIL.**
- [ ] **Step 4: Implement** the two authed calls + result type, snake_case wire keys.
- [ ] **Step 5: Run + build, expect PASS** — `cargo test -p libttstation`.
- [ ] **Step 6: Commit** — `git commit -m "feat(lib): agent_client ssh_authorize/ssh_revoke"`

---

## Task 4: mock-box `/ssh/authorize` (temp dir)

**Files:**
- Modify: `crates/mock-box/src/main.rs`

**Interfaces:**
- Produces: `POST`/`DELETE /ssh/authorize` on mock-box, writing a temp-dir `authorized_keys` (NEVER `~/.ssh`), returning `ssh_user: "ttuser"`. Accepts any bearer (mock-box already fakes auth).

- [ ] **Step 1: Study mock-box's routes + how it fakes auth.** Add the two routes writing to a per-process temp `authorized_keys` (e.g. under `std::env::temp_dir()`); reuse `libttstation`/`tt-station-agentd::authkeys` if importable, else a trivial append.
- [ ] **Step 2: Implement + build** — `cargo build -p mock-box`.
- [ ] **Step 3: Manual smoke** — start mock-box, `curl -XPOST .../ssh/authorize` with a fake key → 200 `ssh_user:ttuser`.
- [ ] **Step 4: Commit** — `git commit -m "feat(mock-box): /ssh/authorize against a temp authorized_keys"`

---

## Task 5: tt CLI — default SSH user `ttuser`

**Files:**
- Modify: `crates/tt/src/main.rs` (or wherever SSH-user is resolved)

**Interfaces:**
- Produces: `const DEFAULT_SSH_USER: &str = "ttuser";` used wherever the CLI resolves an SSH user (`override.unwrap_or(DEFAULT_SSH_USER)`), replacing any `current_login` default.

- [ ] **Step 1: Grep** the CLI for existing SSH-user resolution / login-name use. If the CLI doesn't currently resolve an SSH user (it may not until Task 6/7), define `DEFAULT_SSH_USER` now for Task 6/7 to consume and add a unit test asserting its value.
- [ ] **Step 2: Test** — `#[test] fn default_ssh_user_is_ttuser() { assert_eq!(DEFAULT_SSH_USER, "ttuser"); }` (RED if the const doesn't exist).
- [ ] **Step 3: Implement the const + apply at any existing resolution site.**
- [ ] **Step 4: Run + build, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): DEFAULT_SSH_USER=ttuser"`

---

## Task 6: tt CLI — key selection/gen + `tt ssh-authorize [--revoke]`

**Files:**
- Modify: `crates/tt/src/main.rs` (+ a pure helper module for key-file selection/label)

**Interfaces:**
- Produces: pure `select_public_key_path(home: &Path) -> Option<PathBuf>` (prefers `id_ed25519.pub`, then `id_rsa.pub`); pure `ssh_label(host: &str, date: &str) -> String` → `"ttstation:<host>:<date>"`; `tt ssh-authorize --host <h> [--revoke] [--user <u>]` command that reads/gens the key (owner-verified gen) and calls `AgentClient::ssh_authorize`/`ssh_revoke`, printing `ssh_user`. `--json` → `{authorized, ssh_user, already_present, public_key_path}`.

- [ ] **Step 1: Write pure-helper tests** — `select_public_key_path` order (ed25519 before rsa; None when neither exists — use a temp home with fixture files); `ssh_label` format.
- [ ] **Step 2: Run, expect FAIL.**
- [ ] **Step 3: Implement helpers** + the `ssh-authorize` subcommand: resolve key via `select_public_key_path($HOME/.ssh)`; if `None`, run `ssh-keygen -t ed25519 -N "" -f $HOME/.ssh/id_ed25519 -C "ttstation:<mac-hostname>"` (owner-verified branch), then re-select; read the `.pub`, `validate` it locally too, call the client with `ssh_label(host, today)` (today passed by the caller/`chrono`-free: format via the CLI's existing time source or a `--date`); print/JSON the result. `--revoke` calls `ssh_revoke(Label(...))`.
- [ ] **Step 4: Run + build, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -m "feat(cli): tt ssh-authorize (read/gen key, --revoke, --json)"`

---

## Task 7: tt CLI — `tt pair … --enable-ssh`

**Files:**
- Modify: `crates/tt/src/main.rs` (pair command)

**Interfaces:**
- Produces: `--enable-ssh` flag on `tt pair`/`pair-complete`; after a successful pair, runs the Task 6 ssh-authorize flow and includes its result in the pair `--json` output (`ssh: {authorized, ssh_user, ...}` or `ssh: null` when the flag is off / it failed non-fatally).

- [ ] **Step 1: Study the pair/pair-complete command.** Add `--enable-ssh`; on success, call the shared ssh-authorize routine (extracted from Task 6 so it's reused, not duplicated). SSH failure is non-fatal — pair still reports success, with `ssh.error` set.
- [ ] **Step 2: Build + a test** (if the pair command has testable JSON assembly) asserting `ssh` present when `--enable-ssh`, absent/null otherwise.
- [ ] **Step 3: Run + build, expect PASS.**
- [ ] **Step 4: Commit** — `git commit -m "feat(cli): tt pair --enable-ssh installs the Mac key post-pair"`

---

## Task 8: Swift — default SSH user `ttuser`

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/WorkbenchLaunchers.swift` (`SSHTarget.resolve`)
- Test: `macos/TTStation/Tests/TTStationKitTests/WorkbenchLaunchersTests.swift`

**Interfaces:**
- Produces: `SSHTarget.resolve` defaults to `ttuser` when `overrideUser` is nil/empty (was `currentUser`). Signature stays `resolve(host:overrideUser:currentUser:)` but `currentUser` is now only a documented fallback-of-last-resort or dropped; default is the `ttuser` constant `SSHTarget.defaultUser`.

- [ ] **Step 1: Write failing tests:**

```swift
func testSSHTargetDefaultsToTtuser() {
    let t = SSHTarget.resolve(host: "qb2-lab.local", overrideUser: nil, currentUser: "tsingletary")
    XCTAssertEqual(t.user, "ttuser")   // NOT the Mac login
}
func testSSHTargetOverrideWins() {
    let t = SSHTarget.resolve(host: "qb2-lab.local", overrideUser: "someone", currentUser: "tsingletary")
    XCTAssertEqual(t.user, "someone")
}
```

- [ ] **Step 2: Run, expect FAIL** (currently returns `tsingletary`).
- [ ] **Step 3: Implement** — add `public static let defaultUser = "ttuser"`; change resolve so `user = (overrideUser.flatMap { $0.isEmpty ? nil : $0 }) ?? defaultUser`. Keep the `currentUser` param for source-compat but no longer use it as the default (or update call sites + delete it — verify `LaunchController.sshTarget`). Update the existing `SSHTarget.resolve` test that expected the login name.
- [ ] **Step 4: Run full `swift test`, expect PASS.**
- [ ] **Step 5: Commit** — `git commit -m "feat(macos): default SSH user ttuser (override via tt.sshUser)"`

---

## Task 9: Swift app — opt-in SSH toggle in the pair flow + version bump

**Files:**
- Modify: `macos/TTStation/Sources/TTStationKit/BoxViewModel.swift` (pair-complete → optional ssh-authorize call via `TTCommands`)
- Modify: `macos/TTStation/Sources/TTStationKit/TTClient.swift` + protocol (add `sshAuthorize(host:)`) + `FakeTTClient`
- Modify: `macos/TTStation/AppShell/Sources/BoxWorkspaceView.swift` + `BoxDetailView.swift` (the "Also enable Terminal/SSH access" toggle, default on)
- Modify: `macos/TTStation/AppShell/project.yml` (version bump)

**Interfaces:**
- Consumes: `tt ssh-authorize --host <h> --json` (Task 6).
- Produces: `TTCommands.sshAuthorize(host:) async throws -> SshAuthorizeInfo`; `BoxViewModel.enableSSH: Bool` + a post-pair `authorizeSSH()` that runs when the toggle is on; a one-line result/error surfaced in the pair UI.

- [ ] **Step 1: Add `sshAuthorize` to the `TTCommands` protocol + `TTClient`** (shells out to `tt --json ssh-authorize --host …`, decodes `{authorized, ssh_user, already_present}`) + `FakeTTClient` stub returning a canned success. Add a `BoxViewModel` test (using `FakeTTClient`) that `completePairing` with `enableSSH == true` triggers `sshAuthorize` and sets a success message; with `false` it does not.
- [ ] **Step 2: Run, expect FAIL; implement; expect PASS** (`swift test`).
- [ ] **Step 3: Wire the toggle** into the pair UI (`BoxWorkspaceView`/`BoxDetailView`): a `Toggle("Also enable Terminal / SSH access (installs this Mac's key as ttuser)", isOn:)` bound to `box.enableSSH` (default true), shown at the code-entry step. On successful pair with it on, show "SSH enabled as ttuser" or the non-fatal error.
- [ ] **Step 4: Bump `MARKETING_VERSION`** (e.g. 0.4.0).
- [ ] **Step 5: `xcodegen generate && xcodebuild … build` (BUILD SUCCEEDED) + `swift test`.**
- [ ] **Step 6: Commit** — `git commit -m "feat(macos): opt-in enable-SSH toggle on pair (ttuser), vX.Y.Z"`

---

## Task 10: Docs

**Files:** `macos/README.md`, `CLAUDE.md`

- [ ] **Step 1: Document** the keyless-SSH-on-pair flow, the `ttuser` default (override via `tt.sshUser` / `--user`), the authed `/ssh/authorize` route, and revocation (`tt ssh-authorize --revoke`). Note the agent-run-user alignment requirement.
- [ ] **Step 2: Commit** — `git commit -m "docs: keyless SSH on pairing + ttuser default"`

---

## Self-review notes

- **Spec coverage:** Component 1 → Tasks 1–2; Component 2 → Task 3; Component 3 (CLI) → Tasks 5–7; Component 4 (app) → Tasks 8–9; Component 5 (mock-box) → Task 4; security constraints → Tasks 1 (validation/idempotence/perms) + 2 (auth); docs/version → Tasks 9–10.
- **Type consistency:** `validate_public_key`/`authorize`/`revoke`/`AuthorizeOutcome`/`Revoke` (Task 1) consumed by Task 2; `SshAuthorizeResult{authorized,ssh_user,already_present}` (Task 3) mirrored by the CLI `--json` (Task 6) and Swift `SshAuthorizeInfo` (Task 9); `DEFAULT_SSH_USER`/`defaultUser = "ttuser"` (Tasks 5, 8).
- **Owner-verified vs TDD:** pure logic (Tasks 1, 5, 6-helpers, 8) is TDD; process/socket/SwiftUI (Task 6 keygen branch, 9 UI, 4 mock wiring) is owner-verified.
- **`ttuser` default** is the through-line: agent target (run-user, ttuser on QB2), CLI default, Swift default — all overridable.
```
