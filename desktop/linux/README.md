# TT Station for Linux

TT Station on a Linux computer, connected over the network to any Tenstorrent
box running TT Station (a QuietBox, a Galaxy server, a workstation with
Tenstorrent cards): everything the box's own app has (Chat, Tori, Studio, Research,
Code, Library, Routines, Models, Hardware, Connect, Settings). The screens
come from the box itself, so they're always its current version.

## Install

Ubuntu 22.04 or later, or Debian 12 or later, on x86-64:

```sh
sudo apt install ./tt-station-desktop_<version>_amd64.deb
```

Then open **TT Station** from your apps. To remove it: `sudo apt remove tt-station-desktop`.

The package puts the app in `/opt/tt-station-desktop`, a `tt-station-desktop`
command on your path, a launcher and icons, and an AppArmor profile
(`/etc/apparmor.d/tt-station-desktop`, installed only where AppArmor 4 is)
that Ubuntu 24.04 needs before Chromium's sandbox can start. Licenses are in
`/usr/share/doc/tt-station-desktop/`.

## Connect a box

The computer must be on the same network as the box.

1. TT Station lists the boxes it finds (or type the box's address).
2. Pick yours. If this computer already SSHes into it with a key under your
   username here, you're in.
3. Otherwise sign in with your account on the box (its SSH username and
   password), or choose **Use a pairing code**: the box shows a code in TT
   Station on its screen and on any phone paired with it. A box that only
   accepts SSH keys doesn't take passwords, so the code is the way in for a
   new computer there.

Before signing in over SSH, TT Station asks the box for its SSH host keys over
its secure connection (pinned to the certificate it showed), and ssh accepts
only those. A box too old to say them can still let in an SSH key, but not a
password: use a pairing code there.

Signing in with the box's account makes this computer one of its owners;
pairing with a code makes it a member. An owner can change that in Settings →
Devices.

Tori's shortcut (`Ctrl+Alt+Space` unless changed) is off until you turn it on
in Settings → Assistant (Tori) and voice.

## Build it

`desktop/linux/build-deb.sh` (on x86-64 Linux, after `npm install` in `desktop/`)
→ `desktop/linux/dist/tt-station-desktop_<version>_amd64.deb`. It packages
this checkout's Electron (`node_modules/electron/dist`) with the shell
(`electron/`, `assets/`) and `bonjour-service` in `resources/app.asar`, and
flips Electron's fuses so the binary runs only that app. The box's own install is the
separate `tt-station` package (`build-deb.sh` at the repo root).
