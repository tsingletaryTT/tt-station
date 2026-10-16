# TT Station for Mac

TT Station on your Mac, connected to your Tenstorrent devices (a QuietBox,
a Galaxy server, a workstation with Tenstorrent cards — anything running TT
Station). Everything the device's own app has is here: Chat, Tori, Studio,
Research, Code, Library, Routines, Models and check-ups, Hardware, Connect,
Settings. The screens come from the device itself, so they're always the
device's current version.

## Install

1. Unzip `TT-Station-<version>-mac-arm64.zip` (Apple Silicon: M1 and later)
   or `…-mac-x64.zip` (Intel Macs).
2. Drag **TT Station.app** to Applications.
3. The first time, macOS stops it: the app isn't notarized by Apple yet.
   Open it once, then go to **System Settings → Privacy & Security**, scroll
   down to *"TT Station" was blocked*, and click **Open Anyway**.
   (Or, in Terminal: `xattr -dr com.apple.quarantine "/Applications/TT Station.app"`.)
4. Allow it to **find devices on your local network** when macOS asks.

## Connect a device

The Mac must be on the same network as the device.

1. TT Station lists the devices it finds (or type the device's address).
2. Pick yours. If you already SSH into it with a key under your Mac's
   username, that's it: you're in.
3. Otherwise, sign in with your account on the device (the username and
   password you'd SSH in with). The password goes straight to the device and
   isn't kept.

How it works: TT Station first asks the device for its SSH identity (its host
keys) over its secure connection, pinned to the certificate it showed, so ssh
talks only to that device. Over SSH, the device's own TT Station then pairs
this Mac, and the Mac checks the device's certificate matches the one it sees
on the network. A device too old to say its SSH identity can still let in an
SSH key, but not a password: use a pairing code there.
It works the same on a QuietBox, a Galaxy server or a workstation with
Tenstorrent cards, with or without a screen.

No login for it? **Use a pairing code instead**: the device shows a code as
a notification in TT Station on its screen and on any phone paired with it.

Signing in with the device's account makes this Mac one of its owners; a
pairing code makes it a member (an owner can change that in Settings →
Devices).

More devices: click the device name at the top left → **Add another box…**
(or **Box → Add Another Box…** in the menu bar). Switch between them
from the same place.

## The command line

- **Open Terminal** (sidebar, or **Box → Open Terminal on Box**, ⇧⌘T):
  the device's shell in Terminal, over SSH. The first time, TT Station makes
  a key just for this (`~/.ssh/tt-station_ed25519`) and the device lets it
  in — no passwords.
- **Box → Open Box in VS Code**: VS Code's Remote-SSH, on the device.

## Tori's shortcut

Off until you turn it on in **Settings → Assistant (Tori) and voice**. Its
default, ⌃⌥Space, is also macOS's shortcut for switching input sources: pick
another there if you use that.

## On your Mac, not on the device

Some things act on the computer you're using:

- **Tori** opens apps and links, finds files (Spotlight), opens folders, and
  changes volume, media, dark mode and the screen lock *on your Mac*. Models,
  chips and everything about the device still act on the device.
- **Record a meeting** records your Mac's microphone and sends the recording
  to the device to transcribe. (macOS doesn't let apps record other apps'
  sound: on a call through the speakers it hears both sides, with headphones
  only you.)
- **Apps that only run on the device** (Jupyter, Flowise…) open through a
  private SSH tunnel, set up for you.

## Build it

`desktop/mac/build-mac.sh` (Linux or macOS) → `desktop/mac/dist/`. The zip
holds the app and its licenses (`Licenses/`, also inside the app's
`Contents/Resources/Licenses`).
