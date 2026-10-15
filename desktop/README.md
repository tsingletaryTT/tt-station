# TT Station desktop

The QuietBox's own desktop app: a native window with its own dock icon, tray,
notifications, global shortcut and start-at-login, built with Electron and
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
  `Ctrl+Alt+Space` to ask the box from anywhere, start at login
  (`~/.config/autostart/tt-station.desktop`), and connected apps (Open WebUI,
  LobeChat, Jupyter, …) open in their own windows.

## Places

Home (what's serving, modes, happening now, chips, today, Box Doctor) · Chat
(streaming, Markdown, thinking, tool actions with confirmation) · Studio
(images, video, transcription with drag and drop) · Models (catalogue,
downloads, start, remove) · Bring-up (tt-orchard: preflight, stages, progress,
reports, serve) · Hardware (live chips, last hour, tt-toplike starfield, gozer
leases, serving log, Lab) · Connect (apps, API addresses, keys, code) ·
Settings (desktop, power, household, schedules, energy, remote access,
software).

## Build and install (on the box)

Needs Node 22 (the box's system Node 18 is too old for Electron 44/Vite 8; a
user-local copy lives in `~/.local/share/node/current/bin`).

```sh
export PATH=$HOME/.local/share/node/current/bin:$PATH
npm install
npm run build            # → dist/, served by the agent at /app
./install-desktop.sh     # icon + launcher for this user (pin it to the dock)
```

`npm run dev` serves the screens with hot reload at `localhost:5173` (proxied to
the agent); `npm start` opens the shell.
