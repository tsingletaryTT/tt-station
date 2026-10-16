# Bring-up containment (reference)

A bring-up (Codex or Claude Code driving tt-metal's fast-models-fast stages, see
[orchard.md](orchard.md#bring-up-engines-see-also-bringuprs)) runs commands for hours, as
the box's own account. The permission policy (`codex_bringup.py`, `permission_mcp.py`)
asks a person before risky commands, but it reads command text: it is a guard, not
containment. This page is what actually holds a run in, what doesn't yet, and what an
owner can add with root.

## What a run can and can't reach (no root needed)

Every stage (`tt-codex-bringup-<id>` / `tt-claude-bringup-<id>`, and the Package stage)
starts through three layers (`crate::sandbox`):

1. **Landlock** (`sandbox_exec.py`, the unit's first program). Unprivileged, so it works
   on the stock box (kernel Landlock ABI 8 here).
   - **Unreadable:** `~/.config/tt-station` (paired devices' token hashes, the local
     secret, app keys, secrets, approvals, other runs' records), `~/.ssh`, `~/.gnupg`,
     `~/.local/share/keyrings`, `~/.password-store`, `~/.config/gh`, `~/.git-credentials`,
     `~/.netrc`, `~/.aws`, `~/.kube`, browser profiles. Only the run's driver scripts
     under `~/.config/tt-station/bringup/` stay readable, file by file.
   - **Writable, and nothing else:** the run's own folder
     (`~/.config/tt-station/bringup/codex/<id>`), the tt-metal checkout, `~/.cache`
     (Hugging Face, pip, tt-metal's kernel cache), `~/.triton`, the engines' own state
     (`~/.codex`, `~/.claude`, `~/.claude.json`, `~/.local/share/claude`,
     `~/.local/state`), `~/.config/tenstorrent`, `~/.config/vllm`, `/tmp`, `/var/tmp`,
     `/dev` (the chips, `/dev/shm`), plus `TT_STATION_BRINGUP_RW` (colon-separated).
     Not the home folder, so no `~/.bashrc` or autostart entries.
   - Outside the sandbox it can't signal, ptrace, or read `/proc/<pid>/environ` of
     anything (the agent included), can't reach abstract Unix sockets, can't write
     cgroup files (so it can't move itself out of its unit), and can't hard-link a hidden
     file somewhere readable.
   - Folder *names* on the way to a hidden path stay listable (`ls ~` works).
   - `TT_STATION_BRINGUP_SANDBOX=off` in the agent's environment turns this layer off
     for a run that needs something it blocks; the other two stay.
2. **systemd unit properties**, only the ones a user manager applies on this box
   (systemd 255, Ubuntu 24.04):

   | Property | Here |
   |---|---|
   | `NoNewPrivileges=`, `RestrictSUIDSGID=`, `RestrictNamespaces=`, `LockPersonality=`, `RestrictRealtime=`, `SystemCallFilter=`, `SystemCallArchitectures=`, `RestrictAddressFamilies=`, `KeyringMode=`, `UMask=`, `UnsetEnvironment=`, `MemoryMax=`/`TasksMax=` | applied (seccomp, no_new_privs, cgroup) |
   | `PrivateUsers=` | applied (a user namespace), but useless alone |
   | `ProtectHome=`, `ProtectSystem=`, `InaccessiblePaths=`, `ReadWritePaths=`, `TemporaryFileSystem=`, `BindPaths=`, `PrivateTmp=`, `PrivateNetwork=`, `ProtectKernelTunables=`, `ProtectControlGroups=`, `ProtectProc=`, `ProtectHostname=` | **silently skipped**: AppArmor (`kernel.apparmor_restrict_unprivileged_userns=1`) denies CAP_SYS_ADMIN in the user namespace, and systemd in user mode ignores the failed mount setup |
   | `IPAddressDeny=`/`IPAddressAllow=` | silently skipped (BPF needs privileges) |
   | `ProtectClock=`, `ProtectKernelModules=`, `ProtectKernelLogs=`, `PrivateDevices=` | the unit fails to start (status 218, can't drop capabilities) |
   | `DeviceAllow=`/`DevicePolicy=` | root's (system units only) |

   The agent asks the user manager once at start (a throwaway unit hides a folder and
   says what it saw); if mount namespaces work (after `--userns` below), runs also get
   `PrivateUsers=yes`, `ProtectSystem=strict`, `ProtectProc=invisible`, an empty
   `TemporaryFileSystem=` over `~/.config/tt-station` with the run's folder and drivers
   bound back in, `InaccessiblePaths=` for the keys, and `ReadWritePaths=` for the rest.
3. **No local trust** (`crate::localpeer`). Plain HTTP from the agent's own account is
   normally the box itself (owner routes, no token). A connection whose socket is held
   by a process in a `tt-codex-bringup-*`, `tt-claude-bringup-*`, `tt-bringup-*`,
   `tt-orchard-*` or `tt-code-*` unit's cgroup isn't: it may use the model API (`/v1`,
   with an app key) and a bring-up driver's own two routes (with its run secret), and
   gets `403` for everything else.

## Approvals

- Raising and waiting (`POST /bringup/codex/runs/{id}/approvals`,
  `POST /bringup/codex/runs/{id}/approvals/{aid}/wait`) take the run's secret
  (`X-Run-Secret`), and only while the run is live: a stopped or finished run's secret is
  refused, and every launch (start, resume, package) gets a new one.
- Answering (`POST /bringup/codex/runs/{id}/approvals/{aid}`) takes an **owner's paired
  device token**. Local trust doesn't count, with or without the run secret; a member's
  token is `403`. Tori's "allow it" is the same: only from an owner's paired device.
- Answered requests are dropped 10 minutes after the answer (time enough for the driver
  to collect it), and every request of a run that is no longer live goes with it.

## What the same account still allows

These need a different Unix account to close, i.e. root:

- **The docker socket.** The box user is in the `docker` group; a run can start a
  container that mounts any path. Docker commands ask a person under `smart`, but that's
  the guard.
- **The user's systemd and D-Bus sockets** (`bus` and `systemd/private` in the user's
  runtime folder, `$XDG_RUNTIME_DIR`). `systemd-run --user …` from inside a run starts a
  process outside its unit and sandbox. The unit drops `XDG_RUNTIME_DIR` and
  `DBUS_SESSION_BUS_ADDRESS`, and the policy asks before `systemd-run`, `busctl`,
  `dbus-send` and `gdbus`, but a run can name the socket itself, and Landlock can't stop
  `connect()` to a named Unix socket.
- **The engines' own state.** `~/.codex` and `~/.claude` (sign-in tokens, settings,
  hooks) are writable, since the engines need them; a run could change the settings the
  owner's interactive Codex/Claude use.
- **The network.** Anything reachable from the box is reachable from a run.

## With root: `deploy/contain-bringup.sh`

An owner runs it; TT Station never does.

- `sudo deploy/contain-bringup.sh --userns` installs an AppArmor profile that lets
  `systemd-executor` (only it) keep its capabilities in the user namespace it creates.
  The user manager then applies the mount properties above; TT Station picks this up on
  its next start. The sysctl stays on for everything else.
- `sudo deploy/contain-bringup.sh --account --box-user <user>` sets up the design that
  closes the rest:
  - a system account `tt-bringup` (home `/var/lib/tt-bringup`, no login shell, no
    `docker` group, no sudo), with ACLs on only the tt-metal checkout and the Hugging Face
    cache;
  - `/usr/local/libexec/tt-station/bringup-run start|stop|is-active <unit> …`, a
    root-owned launcher the box user may run through sudo (and nothing else), which
    starts the run as a **system** unit under `tt-bringup` with every property a system
    manager applies (`ProtectHome=tmpfs` with only the checkout and cache bound in,
    `ProtectSystem=strict`, `DevicePolicy=closed` + `DeviceAllow=char-tenstorrent`,
    `IPAddressDeny=any` + `IPAddressAllow=localhost`, kernel protections);
  - egress only through tinyproxy on `127.0.0.1:8899`, allowlisting Hugging Face,
    GitHub, PyPI, PyTorch's index, the docs hosts and the engines' own APIs.

  Different uid means no local trust (`crate::localpeer` checks the socket's owner), no
  docker, no user bus, and nothing of the box user's home beyond the two shared folders.
  The engines are signed in once as that account. **Follow-up:** the agent side (launch
  through `bringup-run` when `TT_STATION_BRINGUP_AS=tt-bringup`, run folders under
  `/var/lib/tt-bringup/runs`, status through `bringup-run is-active`) isn't written yet.
