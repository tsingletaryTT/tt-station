# TT Station for Linux

TT Station on a Linux computer, connected to your Tenstorrent devices over the
network: everything the device's own app has (Chat, Tori, Studio, Research,
Code, Library, Routines, Models, Hardware, Connect, Settings). The screens
come from the device itself, so they're always its current version.

## Install

Ubuntu 22.04 or later, or Debian 12 or later, on x86-64:

```sh
sudo apt install ./tt-station-desktop_<version>_amd64.deb
```

Then open **TT Station** from your apps. To remove it: `sudo apt remove tt-station-desktop`.

The package puts the app in `/opt/tt-station-desktop`, a `tt-station-desktop`
command on your path, a launcher and icons, and an AppArmor profile
(`/etc/apparmor.d/tt-station-desktop`) that Ubuntu 24.04 needs before
Chromium's sandbox can start.

## Connect a device

The computer must be on the same network as the device.

1. TT Station lists the devices it finds (or type the device's address).
2. Pick yours. If this computer already SSHes into it with a key, you're in.
3. Otherwise choose **Use a pairing code**: the device shows a code in TT
   Station on its screen and on any phone paired with it. A QuietBox set up
   with `deploy/harden-box.sh` doesn't take SSH passwords, so the code is the
   way in for a new computer.

New computers join as members; an owner can change that in Settings → Devices.

## Build it

`desktop/linux/build-deb.sh` (on x86-64 Linux, after `npm install` in `desktop/`)
→ `desktop/linux/dist/tt-station-desktop_<version>_amd64.deb`. It packages
this checkout's Electron (`node_modules/electron/dist`) with the shell
(`electron/`, `assets/`) and `bonjour-service`. The box's own install is the
separate `tt-station` package (`build-deb.sh` at the repo root).
