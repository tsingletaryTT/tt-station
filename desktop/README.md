# TT Station desktop

TT Station's desktop app, for any Tenstorrent box running TT Station: a native
window with its own dock icon, tray, notifications, an optional global shortcut
and start-at-login, built with Electron and
Tenstorrent's Vesper design system (`@tenstorrent/vesper`, React).

## How it fits together

- **The screens** (`src/`, React + Vite + Vesper) are built into `dist/` and
  served by the box's agent at `http://127.0.0.1:8765/app/` (`app_ui.rs`), the
  same origin as the API they call.
- **The shell** (`electron/main.cjs`) opens that page in a native window. It
  signs itself in with the agent's `local-secret` (readable only by the box's
  user) via `/pair/offer` + `/pair/complete`, keeps the token in
  `~/.config/tt-station/desktop-token` (0600), and hands it to the screens
  through `electron/preload.cjs`. If the agent isn't up, it shows
  `electron/offline.html` and retries.
- **Desktop integration:** tray (status, mode switches, stop, quit), system
  notifications from the box's events while the window isn't in front,
  a shortcut to ask the box from anywhere (off until turned on in Settings →
  Assistant; `Ctrl+Alt+Space` unless changed), start at login
  (`~/.config/autostart/tt-station.desktop`), and connected apps (Open WebUI,
  LobeChat, Jupyter, …) open in their own windows.

## Places

**Needs you** is the bell at the top right of the window, not a sidebar place:
a count when something is waiting for your OK (9+ past nine), and a dropdown of
it, newest first: Code's questions and routines' actions for everyone,
pairing requests for owners, each with Allow/Deny or
Open (keyboard: ↑ ↓ A D Enter; Escape closes). Its "See everything" opens the
full page (`/app/inbox`).

Home (what's serving, modes, happening now, chips, today's numbers with a link
to energy, Box Doctor) · Chat (streaming, Markdown, thinking, tool actions
with confirmation) · Studio (images, transcription with drag and drop, meeting recording on this
computer) ·
Routines (jobs the box does on its own, including switching modes at set times,
which used to be Settings → Schedules) · Models (catalogue, downloads, start,
remove) · Hardware (live chips, last hour, energy, serving log, and with
developer tools on, gozer leases) · Connect (apps, API addresses, keys, code) · Settings
(desktop, Tori and voice, models, power, people, devices and pairing, security
and activity, your data, software).

**One place each.** Pairing is Settings → Devices and pairing (`#devices`); the
box switcher and the connect screen on another computer point there. Energy is
Hardware → Energy (`/app/hardware#energy`). Mode switches on a timetable are
routines. Needs you reads the endpoints each feature already has (`GET
/approvals`, pairing events from `GET /events`) and answers through them
(`POST /approvals/answer`); see `src/inbox.ts`.

## Build and install (on the box)

Needs Node 22 (the box's system Node 18 is too old for Electron 44/Vite 8; a
user-local copy lives in `~/.local/share/node/current/bin`).

```sh
export PATH=$HOME/.local/share/node/current/bin:$PATH
npm ci
npm run build            # → dist/, served by the agent at /app
./install-desktop.sh     # icon + launcher for this user (pin it to the dock)
```

`npm run dev` serves the screens with hot reload at `localhost:5173` (proxied to
the agent); `npm start` opens the shell.

## On other computers

The same shell, packaged to connect to a box over the network: a Mac app
(`mac/build-mac.sh`, see [mac/README.md](mac/README.md)) and a Linux `.deb`
(`linux/build-deb.sh`, see [linux/README.md](linux/README.md)).
