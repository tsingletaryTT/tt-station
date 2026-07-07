# macOS brief: Connect → Open WebUI fails (IPv6 link-local + SSH key not offered)

**Audience:** the Claude on `macos/TTStation`. **From:** box-side session. **Priority:** user-facing (Taylor hit it).
**Box side is healthy — this is entirely in `LaunchController.openWebUI` / `runSSHCommand`.**

## What actually happens (evidence from the box)

- The box is **serving** (`Llama-3.3-70B`), the Mac is **paired**, and **Open WebUI is already running and
  healthy**: container `ttstation-openwebui` Up, `:3000` published on all interfaces, and
  `http://192.168.5.119:3000/health → 200` **and** `http://127.0.0.1:3000/health → 200`.
- The Mac's key **is** authorized on the box (`~/.ssh/authorized_keys`, perms `700`/`600`, `PubkeyAuthentication yes`,
  label `ttstation:Taylor-Singletar's-Mac:2026-07-06`).
- Yet the app's launch **SSHed** (so the health short-circuit didn't fire) and the SSH **failed**: box `sshd` logged
  `Failed password for ttuser from fe80::…%enp9s0` ×2, then `Connection closed [preauth]`. Note the source is an
  **IPv6 link-local** address.

## Root cause (two Mac-side defects, compounding)

1. **`openWebUI(endpoint:)` uses the raw `.local` host for health + SSH — no `resolveIPv4`.**
   It derives `host` from `endpoint.baseURL` (`qb2-lab.local`) and passes it straight to `healthURL(host:)` and
   `sshTarget(host:)`. macOS resolves `.local` **IPv6-first → link-local `fe80::…`** (exactly the mDNS issue you
   already fixed for tt-toplike in `2fd6ef2`). A link-local addr:
   - breaks `isHealthy(http://…:3000/health)` (URLSession can't use a zoned `fe80::` addr) → the "already healthy →
     open the browser" branch **never fires**, even though Open WebUI is up → falls through to SSH;
   - is what the box saw as the SSH source.
   **Fix:** run the endpoint host through `resolveIPv4(...)` (the helper already in `LaunchController`) before building
   `healthURL`/`webURL` and before `sshTarget`, same as the tt-toplike launcher. With this alone, the app would find
   Open WebUI healthy and just open `http://<ipv4>:3000` — **no SSH needed** in the common (already-running) case.

2. **`runSSHCommand` doesn't offer the authorized key and can't fail fast.**
   It runs `ssh -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10 ttuser@host <cmd>` with **no `-i <key>`** and
   **no `BatchMode`**. The box authorizes the key `tt ssh-authorize` installs (prefers `~/.ssh/id_ed25519.pub`), but
   plain `ssh` relies on default identity/agent resolution — and when pubkey isn't accepted it **falls to password**
   (no TTY → the `Failed password` we see) instead of erroring clearly.
   **Fix:** add `-o BatchMode=yes` (never prompt/fall to password — fail fast with a real error), `-o PreferredAuthentications=publickey`,
   and `-i ~/.ssh/id_ed25519` (offer exactly the key that was authorized). This makes first-run launch deterministic and
   turns the confusing "password failure" into a clear "publickey rejected" if the wrong key is ever authorized.

## Immediate workaround (no code needed)

Open WebUI is already up. Point a browser at the box's **IPv4** (or Tailscale) address, not the IPv6-resolving `.local`:
`http://192.168.5.119:3000` (LAN) or `http://100.125.193.124:3000` (tailnet). It's serving and wired to the box's vLLM.

## Note

With fix #1, the app almost never needs SSH for Open WebUI (it's a persistent container — health-check hits, browser
opens). SSH is only the first-run launch path, which fix #2 makes reliable. Box side needs no change.
