# The box's voice, and opening apps on its screen

## Voice (`voice.rs`)

Speaking and listening run on the QuietBox's CPU in two containers bound to
127.0.0.1, so voice works while the chips serve the chat model:

| Container | Image | Port | What |
|---|---|---|---|
| `tt-voice-kokoro` | `ghcr.io/remsky/kokoro-fastapi-cpu` | 8880 | Kokoro-82M text to speech |
| `tt-voice-ears` | `ghcr.io/speaches-ai/speaches:latest-cpu` | 8881 | faster-whisper speech to text (`WHISPER__CPU_THREADS=8`) |

Models: `Systran/faster-distil-whisper-small.en` for spoken requests
(~0.1 s per second of speech), `Systran/faster-distil-whisper-large-v3` for
recordings (~2.5× real time on the Ryzen 7 9700X). Both live in the
`tt-voice-hf` volume.

- `GET /voice`, `PUT /voice {voice?, speed?}` (saved in `voice.json`),
  `POST /voice/setup`, `DELETE /voice`.
- `POST /voice/speak {text, voice?, speed?, format?}`: `pcm` is 24 kHz
  16-bit mono (gapless playback on the phone and desktop); default `mp3`.
- `POST /voice/listen` (body: audio): `{text}`.

Clients: desktop voice mode (Chat → mic button, `Ctrl+Alt+V`, tray "Talk to
the box"); the phone's voice mode defaults to the box's voice when it's set up
(and falls back to the phone's own voice if the box can't be reached).

The job queue transcribes on the CPU Whisper whenever the chips are busy
(serving, Lab, Orchard) and the owner hasn't asked for the chips ("Run now"),
so recordings never stop the chat model.

## Opening apps (`desktop.rs`)

The assistant's `open_app {app?, url?}` tool (read-only kind: it runs without
confirmation) and `POST /desktop/open` open an installed desktop app, or a web
address, on the box's logged-in GNOME session. Apps come from the XDG
`.desktop` entries (`GET /desktop/apps`); spoken names are fuzzy-matched
("VS Code" → Visual Studio Code). Launching uses `gtk-launch`, URLs use
`xdg-open` and must be http(s). No shell, no arbitrary commands.

## Tori, the assistant

Chat (the sidebar page, the phone's Chat tab) and Tori are separate. Requests
with `"persona": "tori"` get Tori's name, a "be quick" prompt and the
computer tools; Chat doesn't get the computer tools and points people to Tori.
The phone's voice mode is Tori.

**Instant commands** (`tori.rs`): plain commands -- "open Claude and VS Code",
"open YouTube", "open my Downloads folder", "volume to 30", "mute", "pause",
"next song", "lock the screen", "dark mode" -- are parsed and done without a
model (~20 ms), before `prepare_chat`, so they work even with no model at all.
Anything not fully understood goes to the model. If the model claims it did
something without calling a tool, the stream sends `reset` and it's told to
call the tool.

**Summoning**: GNOME custom shortcut Ctrl+Alt+Space runs
`echo talk | nc -NU $XDG_RUNTIME_DIR/tt-station-tori.sock` (the running app
listens there; ~8 ms) and falls back to launching the app with `--tori`. The
Tori window is preloaded hidden (`/app/tori?preload`) so it appears at once,
already listening; typing stops listening. Spoken questions get spoken
answers and a follow-up listen; after opening an app or folder Tori hides so
the new window is in front.


The assistant is called Tori. When the chips aren't serving a chat model (or
nothing is serving), she thinks on the CPU with Ollama's `qwen3-coder:30b`
(`TT_STATION_CPU_MODEL`, `off` to disable; the agent opts in with
`AppState::with_cpu_brain(true)`, tests don't). `GET /assistant/brain` says where:
`{"where": "chips" | "cpu" | null, "model"}`. About 18 tokens/s; the first
question after a while waits ~20 s for Ollama to load the model (kept for 2 h).

More computer tools: `control_computer` (volume via `wpctl`, media via MPRIS
over `gdbus`, `loginctl lock-session`, GNOME dark mode and do-not-disturb via
`gsettings`), `open_folder` (`gio open`, paths inside home only) and
`find_files` (name search in home, newest first).

Desktop: Tori has her own floating window (`/app/tori`, `--tori` to type,
`--talk` to talk; also the launcher's right-click actions and the tray).
Electron's global shortcuts don't work on Wayland, so Settings → This app can
add Ctrl+Alt+Space / Ctrl+Alt+V to GNOME's custom keyboard shortcuts.
Notifications are closed when the main window gets focus, which clears the
dock's unread badge.

### Tori's top-bar dropdown (GNOME extension)

`desktop/gnome-extension/tori@tenstorrent.com` (install with
`desktop/gnome-extension/install.sh`; GNOME on Wayland loads it at the next
login). A real shell dropdown under Tori's top-bar icon: opens listening, type
instead, hold Space to talk (release to send; a tap listens hands-free).
Because the shell has no browser, the agent does the audio:
`POST /voice/hear {patience_s?, ptt?}` records the box's mic until speech ends
(or `POST /voice/hear/finish` for push-to-talk) and transcribes it;
`DELETE /voice/hear` cancels; `GET /voice/hear` gives the live level;
`POST /voice/say {text}` speaks through the box's speakers (`pw-play`).
The extension exports `org.tenstorrent.Tori` (`Open(mode)`, `Ping`) at
`/org/tenstorrent/Tori` on the shell's bus; the GNOME shortcut tries it first,
then the app's socket, then starts the app. When the extension runs, the app
drops its own tray icon. The shortcut is changed in Settings → This app
(saved in `~/.config/tt-station/desktop.json`).

Tori's instant commands now cover TT Station too (`tori::CAPABILITIES`):
start/stop models and reset chips (as confirmations), switch modes, make
images (offering to switch to images if no image model runs), record
meetings, box status, TT Station pages (`open_station`, via the app's socket
`nav /route`), "yes"/"no" for the newest pending confirmation, and "what can
you do?".

## Model preferences (`prefs.rs`)

`GET/PUT /preferences/models` (`model-prefs.json`): the owner's chat, image,
video and speech-to-text models, and the CPU model Tori/Chat use while the
chips are busy. Modes start the preferred model when this box can run it
(even if it still needs downloading), else their built-in best. Settings →
Models shows them with download state and a "can't use tools" warning for
chat models without a tool-call parser; Voice there is the Kokoro voice.
Chat shows "Chat is on the CPU right now… Load Qwen3-32B" when
`/assistant/brain` says `cpu`, and Tori's window has a "Use the chips" link.

## tt-toplike in TT Station (`toplike_term.rs`)

The Hardware page (desktop) and the Box tab's tt-toplike mode (phone) show the
real tt-toplike, not a re-drawing: `GET /toplike/term` (WebSocket, `?token=`)
runs `tt-toplike --remote 127.0.0.1:<agent port> --mode … -q` in a private
pty and streams the screen; the desktop draws it with xterm.js, the phone with
SwiftTerm. Keys go back to tt-toplike (`v`, `i`, `l`, `/explain`), and
resizes follow the view. Views: insights (`normal`), starfield, castle, flow,
arcade, defrag (arcade + `v`), inference (`i`), hivemind, training. At most 4
sessions; `GET /toplike` says whether it's installed. The desktop can also
open `tt-toplike-app` (its native window) on the same stream.

The desktop app reloads itself when the agent serves a new build (it checks
`/app/` every 20 s and reloads when the window isn't in front).
