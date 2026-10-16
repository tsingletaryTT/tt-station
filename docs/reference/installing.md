# Installing the agent: the .deb, and running from source (reference)

*Covers `debian/`, `deploy/tt-station-agentd.package.service`,
`deploy/tt-station-agentd.service` and the updater
(`crates/tt-station-agentd/src/updates.rs`). Signing and channels:
[releases.md](releases.md).*

## New box

For a box that has never run TT Station. Run everything as the account that
will own the box (not root), except where `sudo` is shown.

### 1. Install the package

The `tt-station` package comes with the release (or from whoever sent you TT
Station). On the box:

```bash
sudo apt install ./tt-station_0.12.0_amd64.deb
systemctl --user enable --now tt-station-agentd   # TT Station for this account
sudo loginctl enable-linger "$USER"               # keeps it running with nobody logged in
curl -fsS http://127.0.0.1:8765/healthz; echo     # it answers
```

It needs no configuration: the box is named after its hostname and answers on
port 8765 (HTTPS on 8766, which is what other computers and phones use).
Optional settings (a name, a Hugging Face token) go in
`~/.config/tt-station/agentd.toml`: [agentd-config.md](agentd-config.md).

### 2. Get ready to serve models

TT Station serves models with Tenstorrent's tt-inference-server, in Docker:

- **Tenstorrent software.** The driver, firmware, 1 GB hugepages and `tt-smi`,
  which [tt-installer](https://github.com/tenstorrent/tt-installer) sets up.
  `tt-smi` should list the box's chips before you go on.
- **Docker, and your account in the `docker` group:**

  ```bash
  sudo apt install docker.io                # if Docker isn't installed yet
  sudo usermod -aG docker "$USER"
  sudo reboot                               # so the agent's service gets the new group
  ```

  With lingering on, logging out doesn't restart the agent's service, so it
  keeps the old groups until the box restarts.
- **tt-inference-server**, where the agent looks for it by default:

  ```bash
  git clone https://github.com/tenstorrent/tt-inference-server ~/code/tt-inference-server
  ```

  The agent runs its `run.py`, which starts each model's container; pick a
  different checkout with `tt_inference_repo` in `agentd.toml`. If
  Tenstorrent's `tt` CLI is installed, the Models page also uses it to list and
  download the models this box can run ([tt-cli.md](tt-cli.md)).
- **A Hugging Face token** for gated models (Llama and others): set `hf_token`
  in `agentd.toml` (see [agentd-config.md](agentd-config.md)) and restart the
  agent (`systemctl --user restart tt-station-agentd`).

### 3. Pair the first owner

Install the desktop app on a Mac or Linux computer on the same network
([desktop/mac/README.md](../../desktop/mac/README.md),
[desktop/linux/README.md](../../desktop/linux/README.md)), pick the box it
finds, and sign in with your account on the box (the one you'd SSH in with).
The app checks the box's SSH host key over its pinned HTTPS first, and the
computer becomes an owner. Others join with a pairing code from TT Station on
the box's screen or a paired phone. On a box with no screen and no paired
phone, `tt-station-agentd pair` in a terminal on the box prints a pairing QR
code and link. The iPhone app pairs by scanning that QR
([ios/README.md](../../ios/README.md)).

## Existing boxes: moving from source to the .deb

### The supported install is the .deb

The `tt-station` package puts the agent at `/usr/bin/tt-station-agentd` and
its systemd user unit at `/usr/lib/systemd/user/tt-station-agentd.service`
(installed, not enabled: `systemctl --user enable --now tt-station-agentd`).
Only that install gets:

- **Signed updates, once releases are signed.** `POST /updates/install`
  takes a release only if its package is signed by a key the box trusts.
  This build has no release signing key yet, so it installs no updates and
  the app says so ("Updates aren't available for this build yet"); until
  then, update by installing the new package with `apt` as above. A source
  build is refused (`409`, "update it with git and a rebuild"): it updates to
  whatever the checkout holds, with nothing checking who wrote it.
- **The rollback watchdog.** Every switch (update or undo) restarts the agent
  under a separate watchdog unit that waits up to two minutes for the new
  version to be the running process *and* answer `GET /status` three times in
  a row; otherwise it puts the previous version back and records
  `rolled_back`. A source build has no watchdog: a build that panics at
  start just restarts every two seconds, which is how a crash-looping build
  (`Overlapping method route`) ran 23 times on a box.
- **A gate before it ships.** Every release's package is installed in a clean
  Ubuntu container and its agent must answer `GET /healthz` and `GET /status`
  before anything is published (`scripts/smoke-deb.sh`).

### Switching a box from source to the .deb

Written for a QuietBox running the agent from a checkout (e.g.
`~/code/tt-station/target/release/tt-station-agentd` through
`~/.config/systemd/user/tt-station-agentd.service`, written by
`tt-station console --install-service`). Nothing below touches the checkout,
so the source build stays available to go back to. Run as the agent's user,
not root, except where `sudo` is shown.

#### 0. Pick the package

Use a package **at least as new as the source build, with the features you
rely on**. A checkout ahead of the last release (`git describe --tags` says
`v0.11.0-105-g…`) would lose 105 commits of features on the 0.11.0 release;
for that box build a package from the same commit instead:

```bash
sudo apt install devscripts debhelper      # once
# In a separate worktree: build-deb.sh vendors crates and writes a
# .cargo/config.toml that would make the checkout's own builds offline.
git -C ~/code/tt-station worktree add --detach ~/code/tt-station-deb HEAD
cd ~/code/tt-station-deb && ./build-deb.sh # writes ~/code/tt-station_<v>_amd64.deb
```

Then smoke-test the exact binary before going further:

```bash
scripts/smoke-agent.sh debian/target/release/tt-station-agentd   # port 18765; the live agent is untouched
```

The package ships the app the agent serves at `/app` (in
`/usr/share/tt-station/app`); `build-deb.sh` builds it first, so building a
package needs Node 22 as well as Rust.

#### 1. Back up

Run the rest in one shell: later steps use `$B`.

```bash
B=~/tt-station-switch-$(date +%Y%m%d-%H%M); mkdir -m 700 "$B"
cp -a ~/.config/systemd/user/tt-station-agentd.service "$B"/
cp -a ~/.config/systemd/user/tt-station-agentd.service.d "$B"/ 2>/dev/null || true
(umask 077; tar -C ~ -czf "$B/config-tt-station.tgz" .config/tt-station)
[ -d ~/.local/share/tt-station ] && (umask 077; tar -C ~ -czf "$B/share-tt-station.tgz" .local/share/tt-station)
git -C ~/code/tt-station rev-parse HEAD > "$B/source-commit"
```

`~/.config/tt-station` holds pairing tokens, people, app keys and the TLS
certificate paired phones pin: the packaged agent uses the same directory,
so devices stay paired. The backup holds secrets; keep it `0700` and delete it
once you're settled.

#### 2. Install the package

Pick a quiet moment (no model loading; a running model keeps serving across
the restart). Installing doesn't start or restart anything:

```bash
sudo apt install ./tt-station_<v>_amd64.deb
```

#### 3. Point the service at the package

The unit in `~/.config/systemd/user/` overrides the package's, and the
`default.target.wants` link points at that file, so disable first, then move
it aside:

```bash
systemctl --user disable tt-station-agentd
mv ~/.config/systemd/user/tt-station-agentd.service "$B"/source-unit.moved
```

The package's unit has a short PATH (~/.local/bin, /usr/local/bin,
/usr/bin, /bin). Keep the PATH the source unit had (it finds `tt-smi`, the
tt-inference-server venv, `tt`), and anything else the box needs, in a drop-in
of your own (the updater only ever writes `50-version.conf`):

```bash
mkdir -p ~/.config/systemd/user/tt-station-agentd.service.d
{ echo '[Service]'; grep '^Environment=PATH=' "$B/tt-station-agentd.service"
} > ~/.config/systemd/user/tt-station-agentd.service.d/10-local.conf
systemctl --user daemon-reload
systemctl --user cat tt-station-agentd    # ExecStart=/usr/bin/tt-station-agentd, your PATH
```

#### 4. Start it and check

```bash
systemctl --user enable tt-station-agentd
systemctl --user restart tt-station-agentd
curl -fsS http://127.0.0.1:8765/healthz; echo
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:8765/status
readlink /proc/$(systemctl --user show -p MainPID --value tt-station-agentd)/exe   # /usr/bin/tt-station-agentd
journalctl --user -u tt-station-agentd -n 50 --no-pager
```

Then open the app on a paired phone: it should connect without re-pairing, and
the updates screen (`GET /updates`) should show the install as `package` and
the update channel with its trusted keys.

Also check: `ls /etc/polkit-1/rules.d/ | grep tt-station` shows one power rule
(the package installs `49-tt-station-power.rules`; remove any older copy a
hardening script put there), and `type -a tt-station` finds `/usr/bin/tt-station`
first or only.

#### Rolling back to source

```bash
systemctl --user disable tt-station-agentd
rm -f ~/.config/systemd/user/tt-station-agentd.service.d/50-version.conf  # an update's selection, if any
rm -f ~/.config/systemd/user/tt-station-agentd.service.d/10-local.conf
cp -a "$B/tt-station-agentd.service" ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable tt-station-agentd
systemctl --user restart tt-station-agentd
readlink /proc/$(systemctl --user show -p MainPID --value tt-station-agentd)/exe   # …/target/release/tt-station-agentd
```

Optionally `sudo apt remove tt-station` afterwards (it never touches
`~/.config/tt-station`). Only if the newer agent changed something in the
config directory the source build can't read: stop the agent, move
`~/.config/tt-station` aside, `tar -C ~ -xzf "$B/config-tt-station.tgz"`, start
it.

## Running from source on purpose

Developers can keep a box on a checkout; the agent says so
(`GET /updates`: install `source`, with the branch and uncommitted file count)
and refuses release installs on it. What you take on instead:

- **You are the rollback.** Before restarting onto a new build, smoke-test it
  on a spare port, and keep the last good binary:

  ```bash
  cargo build --release -p tt-station-agentd
  scripts/smoke-agent.sh target/release/tt-station-agentd   # must say OK
  cp ~/.local/bin/tt-station-agentd.last-good{,.prev} 2>/dev/null || true
  systemctl --user restart tt-station-agentd && sleep 5 \
    && curl -fsS http://127.0.0.1:8765/healthz \
    && cp target/release/tt-station-agentd ~/.local/bin/tt-station-agentd.last-good
  ```

  If it doesn't come up, `journalctl --user -u tt-station-agentd` shows why;
  `git checkout <previous commit>` and rebuild, or point the unit's
  `ExecStart` at `~/.local/bin/tt-station-agentd.last-good` for the moment.
- **You are the signature.** Nothing checks who wrote the commit you built;
  build only from branches you've reviewed.
- **Re-install the unit after moving the checkout:**
  `tt-station console --install-service` rewrites
  `~/.config/systemd/user/tt-station-agentd.service` for the binary and PATH
  you run it with.

Going back to the package later is the switch above.
