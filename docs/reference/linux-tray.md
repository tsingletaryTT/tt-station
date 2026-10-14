# Linux tray (`tt-station-tray`) — reference

`tt-station-tray` is the Linux counterpart of the macOS menu-bar app. It puts an icon in the
system tray and opens a small GTK4 window. From either, you start, stop and connect to models
served by this machine and by paired QuietBoxes.

Status: built, unit-tested and smoke-checked headless. It has been run on a display only in part
(KDE Plasma on Wayland, 2026-10-14). See "What is verified" below before relying on any behavior
described here.

## Branch and review

The work lives on `feat/linux-tray`, which is stacked on `experiments/egpu` (the macOS Blackhole
experiment, open as PR #1 against `main`): `main` <- `experiments/egpu` <- `feat/linux-tray`.
Its pull request targets `experiments/egpu`, so the review shows only the Linux work. Merge
#1 first and delete its branch; GitHub then retargets the stacked PR to `main`. A build for another machine clones the
branch directly (`git clone -b feat/linux-tray git@github.com:tsingletaryTT/tt-station.git`).
The branch edits `.github/workflows/`, so pushing it needs a credential with the `workflow`
scope (an SSH key, or `gh auth refresh -h github.com -s workflow`). The owner pushed
`feat/linux-tray` on 2026-10-14: `origin/feat/linux-tray` is at `4596d11` (tray icon and
left-click fix), so everything up to and including the agent-defaults and docs commits is on
`origin`. Commits made after `4596d11` (the GtkPaned fixes, reference performance on the Models
page, and these docs) are local until the next push. The stacked PR is not opened yet; its
description is drafted in `.superpowers/sdd/2026-10-05-linux-tray/stacked-pr-body.md` on the box
that built it.

## Stations

A *station* is anything that can serve models. The tray treats two kinds the same way.
`Station` in `crates/tt-station-tray/src/station.rs` is an enum with two variants, one per
kind. (The spec proposed a trait object. There are exactly two fixed kinds, and the enum needs
no `async-trait` dependency.)

| Kind | Talks to | Pairing |
|------|----------|---------|
| `LocalStation` | This machine, through the official Tenstorrent CLI (`tt --json ...`) | None |
| `RemoteStation` | A QuietBox, through the `tt-station-agentd` HTTP API | One-time 6-digit code |

`LocalStation` refuses a `tt` binary that does not answer `--version` with `tt <semver>`, so a
stale or shadowing copy is reported and never run. `RemoteStation` uses the same endpoints as
`tt-station run|stop|status`.

Known stations are kept in a registry file in `$TT_CONFIG_DIR` (else
`$XDG_CONFIG_HOME/tt-station`, else `~/.config/tt-station`). Every write takes an advisory
`flock` on a sibling lock file, so two writes never interleave, and the tray's MAC updates
re-read the file under that lock. The lock does not merge stale copies: the window loads the
registry once and saves its whole copy later, so a window save can still overwrite a MAC the
tray recorded after the window loaded (see `Registry::update` in `registry.rs`). The registry
holds no tokens.

## Finding and pairing boxes

Discovery combines mDNS (`_tenstorrent._tcp`) with hosts added by hand (probed through their
`/status`). Tailscale auto-discovery is planned and not built.

Pairing uses the same handshake and token store as the CLI. A box paired with `tt-station pair`
is already paired in the tray, and the reverse. The agent shows its code on the box (panel or
`tt-station console`); enter it in the tray's pairing dialog.

## Tray and window

* **Tray.** Built on `ksni` (StatusNotifierItem over D-Bus). Left click opens the window and
  right click shows the menu (`ksni` 0.3.6 fixes `ItemIsMenu` to false). The icon is an embedded
  pixmap, so it does not depend on an installed icon file. KDE Plasma hosts StatusNotifierItem
  icons itself. XFCE needs a status-notifier or indicator panel plugin; the package name was not
  confirmed here (`xfce4-indicator-plugin` has an apt candidate on 24.04, but whether it hosts
  these icons was not checked). **GNOME does not host StatusNotifierItem icons by
  default**: install the AppIndicator extension (`gnome-shell-extension-appindicator`) and enable
  it. At start-up the tray checks whether a host owns `org.kde.StatusNotifierWatcher` and, if
  not, shows install hints for the detected distribution in the window.
* **Window.** Plain GTK4 with Stations, Models and Logs pages. libadwaita is not used: the
  development box has no `libadwaita-dev`, and plain GTK4 should also build against Ubuntu
  22.04's GTK 4.6 (not yet built there). A libadwaita restyle is a later swap.
* **Connect launchers.** Open WebUI (a local `docker run`, published on `127.0.0.1` only; a
  loopback model endpoint is rewritten to `host.docker.internal` for the container), opencode in
  a terminal, copy the `/v1` URL, open it in a browser. Missing docker, a terminal, `xdg-open`
  or `notify-send` is reported in the UI.
* **Single instance.** The GTK application id `com.tenstorrent.ttstation.tray` is unique on the
  session bus. A second launch activates the first.

Run modes: no argument (tray plus window host; used by the autostart entry), `--no-window` (tray
only), `--smoke`, `--version`. A `--no-window` instance does not take part in single-instance,
so the autostart entry uses the default mode.

## Reference performance on the Models page

For this machine, the Models page shows performance numbers that ship with tt-inference-server.
They are **reference targets from a file. They were not measured on this machine**, and the page
says so in the heading of every block.

* **Source file.** `model_performance_reference.json` in a tt-inference-server checkout:
  `reference_config/benchmarking/benchmark_targets/` (0.22.x, keys are Hugging Face ids) or
  `benchmarking/benchmark_targets/` (0.18.x, keys are short names). The tray takes the checkout
  from the `run.py` path in one `tt --json serve <catalog model> --dry-run` (read-only; nothing
  starts), so the numbers come from the version `tt serve` would run. If that fails it uses the
  newest `~/.local/share/tenstorrent/tools/tt-inference-server-v*/src` that has the file. If
  there is no file, the page shows no numbers and reports no error. The version in the label
  comes from the checkout's `VERSION` file, else from the `tt-inference-server-vX` directory name.
* **What is shown.** A list row gets a suffix such as `  ·  ~37 tok/s/user`. The detail pane
  gets a block such as:

  ```
  Reference performance on p300x2 (tt-inference-server 0.22.0 targets, not measured on this machine)
  1 user, 128 in / 128 out: 46 ms to first token, 37 tok/s per user (theoretical)
  32 users, 128 in / 128 out: 400 tok/s total (theoretical)
  "theoretical" is the ceiling the file sets for this hardware.
  ```

  The single-user line uses the 1-user configuration with the smallest input plus output
  length, which is closest to one chat turn. The total line uses the highest concurrency the
  file lists. Image, audio, video and embedding models get one line per configuration with the
  file's own metric names (`rtr`, `tput_prefill`, `e2el_ms`, ...) and no list suffix. A model
  with no entry for this mesh shows `No reference numbers for <mesh> in tt-inference-server
  <version>.`
* **Matching rules.** A model is found by its `hf_repo`, then its name: exact first, then
  ignoring case, then by the last path segment. Only the device entry equal to the station's
  mesh is used; numbers are never borrowed or scaled from another device. Entries that declare
  `data_parallel` are skipped, because tt-inference-server uses them only when the model spec
  sets `data_parallel_size`, which the tray cannot see. When a configuration has a `measured`
  target (only on such data-parallel entries in 0.22.0 so far) it is preferred over
  `theoretical`, and the label names whichever kind was used.
* **Local station only.** A paired box shows no numbers: the agent's `/models` does not carry
  them. The next step would be an additive `perf` field on the agent's `/models`, filled from
  the box's own checkout by the same `crate::perf` code.
* **Cost.** The first page load runs one extra `tt` dry run and parses the file (about 200 KB)
  on the tokio blocking pool, so the GTK thread does not wait for it. The parsed table is cached for the life of
  the tray, so later loads start no process for it.

Code: `crates/tt-station-tray/src/perf.rs` (pure parse, lookup and formatting, unit-tested
against small real snippets in `tests/fixtures/perf_reference_*.json`),
`view_model::model_line` / `model_detail` (what the page binds), and
`LocalStation::perf_reference`. `--smoke` runs the same path against the short-name fixture.

## Run options

`POST /run` on the agent (and `AgentClient::run_with_options`) accepts an optional `options`
object. The shared type is `libttstation::run_options::RunOptions`; the agent, CLI client and
tray all validate through it.

| Field | Meaning |
|-------|---------|
| `device` | Device name for the serve (for example `p300x2`) |
| `port` | Port to serve on |
| `profile` | tt-model bundle profile |
| `offline` | Serve without network access |
| `workflow` | `server`, the only value the schema offers. `benchmarks` and `evals` parse, but every agent backend and the tray's local station refuse them: they run in the foreground until they finish |
| `vllm_args` | Pass-through vLLM flags, allowlist only |

Unknown keys are rejected by name. Every string that reaches a command line is checked
character by character. vLLM flags are never free text. The allowlist is `max-model-len`,
`max-num-seqs`, `max-num-batched-tokens`, `block-size` (integers with ranges),
`enable-auto-tool-choice` (switch), and `tool-call-parser`, `reasoning-parser` (short
identifiers). See `VLLM_ALLOWLIST` in `crates/libttstation/src/run_options.rs`.

`GET /run/schema` (unauthed) returns a description of every option, generated from the same
tables that drive validation, so a UI can build its form from it. Only the `tt` backend applies
`options`. The `runpy`, `docker` and `dstack` backends refuse a request with non-empty
`options` with HTTP 400 (`OptionsUnsupported`, naming the backend); a request without
`options` is served as before.

## The `tt` backend

`tt-station-agentd --backend tt` (or `backend = "tt"` in a profile) serves through the official
`tt` CLI. Its steps, option restrictions and settings are in
`docs/reference/agentd-config.md` under "The `tt` backend". The mock box (`mock-box`) takes
matching flags so the tray and CLI can be tested without hardware.

To start the agent on a typical box, run `tt-station-agentd --backend auto`. It picks the `tt`
backend when the official `tt` CLI is installed and `runpy` otherwise, and takes the box name,
control port, chip count and serving address from the machine, so the box's identity needs no flags.
If `auto` falls back to `runpy`, pin the serving image (`--serving-image` or `serving_image` in a
profile) as before; `auto` does not choose an image.
`tt-station-agentd --backend auto --print-config` shows what it chose and why.

The agent's gozer lease does not restrict which chips the container opens. On hardware
(2026-10-05) tt-model ignored the grant's `TT_VISIBLE_DEVICES` and mounted `/dev/tenstorrent/0`
to `3` itself. That is harmless only while the lease covers every chip, which is the agent's
current setting.

## Running the headless check

```
tt-station-tray --smoke
```

It builds the window view models and the app wiring against a temporary registry, never
initialises GTK and never opens the session bus, so it works over SSH and in CI. It prints `ok`
and exits 0, or prints the failure and exits 1. Run it with no `DISPLAY` or
`WAYLAND_DISPLAY` set to confirm that.

## What is verified

Verified:
* Unit tests for the options allowlist, the `Station` implementations (against `mock-box` and a
  fake `tt`), the registry, pairing flow, menu model, window view models, SNI probe and
  launcher command lines.
* `tt-station-tray --smoke` runs headless.
* **First display run, KDE Plasma on Wayland (2026-10-14).** Over D-Bus on the live tray:
  `IconPixmap` carries two entries (48 and 128 px) and `IconName` is empty; `Activate(0, 0)`
  (left click) and the "Open window" dbusmenu event each made a window titled "TT Station"
  appear in KWin's windows runner (it listed none before), with no panic. That check looked at
  the log only right after opening; about 15 s later, when the model list loaded, GTK logged
  `GtkPaned ... reports a minimum height of 191, but minimum height for width of 1048576 is
  229`. This was driven by D-Bus calls, so a physical click and a screenshot of the drawn icon
  were not checked.
* **GtkPaned CRITICAL fixed and the Models page checked live (KDE Plasma, Wayland,
  2026-10-14).** The cause was the Models detail label, which wrapped directly inside the
  page's `gtk::Paned`, so its height depended on the width the Paned gave it. The label now
  sits in a vertical-only `ScrolledWindow`. The tray was relaunched, the window opened with
  `Activate`, and the Stations, Models and Logs pages and five model rows were selected through
  AT-SPI selection (no button was pressed). The previous build logged 21 CRITICAL lines during
  that walk; the fixed build logged none in about 70 s. Read back through AT-SPI on this box
  (`p300x2`, tt-inference-server 0.22.0 via `tt` 1.0.1): `gemma-4-31B-it` shows
  `~37 tok/s/user` and the 1-user and 32-user lines, `Llama-3.1-8B-Instruct` shows "No
  reference numbers for p300x2 in tt-inference-server 0.22.0.", `whisper-large-v3` shows
  `asr, 1 user: rtr 16, tput_user 62, ttft_ms 1900 (theoretical)`, and `bge-m3` shows its
  embedding line. A screenshot of the window showed the block wrapped inside the detail pane.
* `cargo clippy --workspace --all-targets -- -D warnings` is clean.

Not verified:
* **Mostly not run on a display**: only the icon properties, the two ways of opening the
  window and the Models page (see above) were observed. Still open: the drawn icon, the
  Stations and Logs page contents beyond their headings, a physical click, `--no-window`, the
  GApplication single-instance logic, ksni menu updates, and the real runner for docker,
  terminal, clipboard, `notify-send` and Wake-on-LAN.
* **The `tt` backend is live-verified for one tt-model bundle only.** Tasks 4 and 4c served
  `changh95/qwen3.8-27b-p300x2` on the development box (4x p300c, `tt` 1.0.1): 5 serves, about
  2 to 4 minutes to `/v1/models`, nothing downloaded. `/serving` listing and rediscovery after
  an agent restart were verified after the fixes in commit e7c4c2c. Still open:
  * catalog models were checked only with `tt serve --dry-run`;
  * `--offline` is untested;
  * the lease's `TT_VISIBLE_DEVICES` does not reach the container (see "The `tt` backend");
  * a serve on a non-default `options.port` is not rediscovered after an agent restart;
  * on the development box `/tmp/tt-gozer/history.jsonl` is owned by root, so a restarted agent
    cannot adopt its lease. The owner fixes that with
    `sudo chown ttuser /tmp/tt-gozer/history.jsonl`.
* **Reference performance** was not checked against a remote station (it shows none by design),
  against a machine with only a 0.18.x checkout (covered by unit tests only), or on GNOME/XFCE.
* **Nothing has been tested on `franklinstower`**, the intended second machine.
* **A `.deb` has not been built or installed** from these packaging files.
* **Ubuntu 22.04 (jammy) has not been built.** The crate enables no `gtk4` `v4_*` feature, so
  it should compile against the base GTK 4.0 API. That is the only evidence for jammy, and it
  is an inference.

## Open decision: leasing

`LocalStation::run` does not take a gozer lease. On a shared box a tray-started model can
collide with another tenant's chips. The agent's `tt` backend does lease through gozer. The
owner has to decide whether the local station should lease too.

## Known gaps against the spec

* The Logs page fetches on demand; there is no live stream.
* The detail pane shows the model row and does not call `tt model info`.
* No presets.
* A 409 (chips busy) shows an error and has no Queue button.
* GNOME needs the AppIndicator extension; the window shows the hint.

## Packaging

Package `tt-station-tray` (amd64) installs `/usr/bin/tt-station-tray`, the launcher
`com.tenstorrent.ttstation.tray.desktop`, an autostart entry in `/etc/xdg/autostart/` (it
starts the tray for every user who logs in to a desktop), and the panel's icons under the
tray's own icon name (the running tray draws its embedded icon pixmap, not this theme entry). It depends on `libgtk-4-1`, and recommends `tt-station (= same version)`
and the GNOME AppIndicator extension.

The `tt-station` package (CLI and box agent) is optional for a tray-only workstation. The tray
runs no `tt-station` binary: it talks to boxes over the agent's HTTP API and to this machine
through the official `tt` CLI. Installing `tt-station` also installs the agent's systemd user
unit and the polkit rule `/etc/polkit-1/rules.d/49-tt-station-power.rules`, which lets members
of the `sudo` group suspend, reboot or power off the machine from any program without a
password prompt. On a workstation that only needs the tray, install it with
`--no-install-recommends` to leave both out.
