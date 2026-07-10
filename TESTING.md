# Testing tt-station (first run)

How a collaborator takes a released build for a spin end-to-end: install the box
side from a `.deb`, install the Mac client from the `.dmg`, then discover → pair →
run → connect.

**Topology this assumes:** an **x86_64 QuietBox** running Ubuntu (the box) + an
**Apple-Silicon Mac** (the client), on the same LAN.

Everything below comes from the **`vX.Y.Z` release** on GitHub → Releases. A single
tag publishes both the `.deb`s (`release.yml`) and the macOS `.dmg`
(`macos-release.yml`) to the same release page.

---

## On the QuietBox (Ubuntu)

1. From the release, download the two `.deb`s matching your Ubuntu version —
   `_noble` for 24.04, `_jammy` for 22.04:
   - `tt-station_<ver>_amd64_<suite>.deb`
   - `tt-station-panel_<ver>_amd64_<suite>.deb`

2. Install both with `apt` (not `dpkg -i`) so dependencies resolve
   (`gir1.2-gtk-4.0`, `python3-gi`, `docker.io`, `xdg-utils`):

   ```bash
   sudo apt install ./tt-station_<ver>_amd64_<suite>.deb \
                    ./tt-station-panel_<ver>_amd64_<suite>.deb
   ```

3. Start the agent as your **user** service and make it survive logout/reboot:

   ```bash
   systemctl --user enable --now tt-station-agentd
   loginctl enable-linger "$USER"
   systemctl --user status tt-station-agentd   # expect: active (running)
   ```

4. Open the panel — “tt-station Panel” in the app menu, or `tt-station-panel` in a
   terminal. You should see the status pill, and a big 6-digit **pairing code**
   appears the moment a client tries to pair.

If the service won't stay up, the logs are:

```bash
journalctl --user -u tt-station-agentd -e --no-pager
```

## On the Mac (Apple Silicon)

1. Download `TTStation-*.dmg` from the release, open it, drag **TTStation** to
   Applications.
2. It's ad-hoc signed (not notarized), so clear the quarantine flag once, then
   launch:

   ```bash
   xattr -dr com.apple.quarantine /Applications/TTStation.app
   open /Applications/TTStation.app
   ```

   (First launch also installs a `~/.local/bin/tt` symlink so the CLI is on PATH.)

## The test loop

1. In the Mac app, the box should appear via LAN (mDNS) discovery. Click **Pair**
   and enter the 6-digit code shown on the box panel.
2. The app's device strip should stream live temp/power telemetry — this confirms
   the unauthenticated telemetry path even before anything is serving.
3. Run a model from the app's model browser (needs serving config on the box — see
   below).
4. Once it's serving, try **Connect → Open WebUI / opencode** — both from the Mac
   app *and* from the box panel's Connect row.

## Serving a model (needed for steps 3–4)

The packaged agent starts with no serving config, so **pairing, telemetry, and
status work out of the box**. Actually *running a model* additionally needs a
box-side config file at `~/.config/tt-station/agentd.toml` pointing at a
`tt-inference-server` checkout and a pinned serving image:

- Copy the starter: `box-panel/agentd.example.toml` → `~/.config/tt-station/agentd.toml`
- Reference: `docs/reference/agentd-config.md`
- After editing, `systemctl --user restart tt-station-agentd`.

## Hardware note

The `.deb`s are **amd64**; the `.dmg` is **arm64**. That matches an x86 QuietBox +
Apple-Silicon Mac. On other hardware you'll need a matching build.

---

## Maintainers: cutting a release

A single tag publishes everything (both `.deb` suites + the macOS DMG) to one
release page. The tag **must** match the version in `Cargo.toml`
(`[workspace.package]`), `debian/changelog`, `box-panel/tt-station-panel.py`
(`__version__`), and `macos/TTStation/AppShell/project.yml` (`MARKETING_VERSION`) —
`ci.yml` and `make-release.sh` assert this. Use `scripts/bump-version.sh <ver>` for
the first three; bump `MARKETING_VERSION` by hand.

```bash
git tag vX.Y.Z && git push origin vX.Y.Z
```

To build the `.deb`s **without** cutting a release (a test build), run the
**Release** workflow manually (Actions → Release → Run workflow); it uploads the
`.deb`s as downloadable Actions artifacts (`tt-station-debs-<suite>`) instead of
publishing a release.
