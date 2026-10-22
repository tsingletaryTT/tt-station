# Voice, Tori and computer tools

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
(serving) and the owner hasn't asked for the chips ("Run now"),
so recordings never stop the chat model.

## Tori, the assistant

Chat (the sidebar page, the phone's Chat tab) and Tori are separate. Requests
with `"persona": "tori"` get Tori's name, a "be quick" prompt and, from the
desktop app (`"client"`, on the stream), the computer tools; Chat gets only
recording from them and points people to Tori. The phone's voice mode is Tori.

**Instant commands** (`tori.rs`): plain commands -- "open YouTube", "open my
Downloads folder", "volume to 30", "mute", "pause", "next song", "lock the
screen", "dark mode", "start Qwen3-32B" -- are parsed and done without a model
(~20 ms), before `prepare_chat`, so they work even with no model at all. Steps
on a computer are instant only from the desktop app, which runs them as they
stream; elsewhere the model answers. App names ("open VS Code") go to the
model, which calls `open_app` for the desktop app to find the app.
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

**Computer tools run in the desktop app, which asks first.** Tori's computer tools
(`open_app`, `control_computer`, `open_folder`, `find_files`, `open_station`,
`start_recording`/`stop_recording`; `assistant::CLIENT_TOOLS`) run on the computer the
desktop app runs on, never by the agent on the box (`status: "on_client"`,
`desktop/electron/main.cjs` `computerTool`). Before `open_app` opens a web or mail
link or launches an app, the app shows a native dialog: which box asked (name and
address), exactly what will open (the full, normalized link; or the app, as found on
this computer), and **Allow once** / **Always allow <site or app> from <box>** /
**Don't allow** (default and cancel). One dialog at a time. "Always" choices are kept
per box in `desktop.json` (`allowedActions: {<box id>: {name, sites, apps}}`; a site
is a link's origin, a mail link its address, an app its lower-cased name), listed and
cleared in Settings → General → This app, and dropped when the box is removed.
Links with control or bidi characters, credentials, more than 2000 characters or any
scheme but `http`/`https`/`mailto` never open. `open_folder` there opens folders only
(never a file or app bundle). The decisions are a pure module,
`desktop/electron/clientactions.cjs`, tested by `desktop/test/clientactions.test.cjs`.

More computer tools, as the desktop app runs them: `control_computer` (volume,
media, lock screen and dark mode through AppleScript; macOS only for now),
`open_folder` (folders inside home only) and `find_files` (name search in home,
with `mdfind` on a Mac and `find` elsewhere). Without the desktop app (the
phone, a browser, the plain `POST /assistant`), these tools aren't offered.

Desktop: Tori has her own floating window (`/app/tori`, `--tori` to type,
`--talk` to talk; also the launcher's right-click actions and the tray).
Electron's global shortcuts don't work on Wayland, so Settings → This app can
add Ctrl+Alt+Space / Ctrl+Alt+V to GNOME's custom keyboard shortcuts.
Notifications are closed when the main window gets focus, which clears the
dock's unread badge.

Tori's instant commands now cover TT Station too (`tori::CAPABILITIES`):
start/stop models and reset chips (as confirmations), switch modes, make
images (offering to switch to images if no image model runs), record
meetings (on this computer, uploaded to `/jobs/transcribe`), box status, TT Station pages (`open_station`, via the app's socket
`nav /route`), "yes"/"no" for the newest pending confirmation, and "what can
you do?".

## Model preferences (`prefs.rs`)

`GET/PUT /preferences/models` (`model-prefs.json`): the owner's chat, image
and speech-to-text models, and the CPU model Tori/Chat use while the
chips are busy. Modes start the preferred model when this box can run it
(even if it still needs downloading), else their built-in best. Settings →
Models shows them with download state and a "can't use tools" warning for
chat models without a tool-call parser; Voice there is the Kokoro voice.
Chat shows "Chat is on the CPU right now… Load Qwen3-32B" when
`/assistant/brain` says `cpu`, and Tori's window has a "Use the chips" link.

The desktop app reloads itself when the agent serves a new build (it checks
`/app/` every 20 s and reloads when the window isn't in front).
