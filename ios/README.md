# TT Station for iPhone (prototype)

Chat with your QuietBox from your phone. The app finds the box on your network,
pairs by scanning the QR TT Station shows on the box, and then talks to the agent
(only over HTTPS pinned to the box's certificate, on your home network) through its
`POST /assistant` route: the box's own served model answers, reads box
state through read-only tools, and asks before doing anything disruptive.

Text only for now; voice is planned on top of iOS's on-device speech APIs so the
box keeps serving a single chat model.

## Build and run (on a Mac)

```bash
brew install xcodegen
cd ios
xcodegen
open TTStationMobile.xcodeproj
```

Needs **Xcode 26** and an iPhone on **iOS 26+**: the UI is built on Liquid Glass
(`glassEffect`, the floating tab bar, `tabViewBottomAccessory`).

In Xcode: select the **TTStationMobile** target → **Signing & Capabilities** → choose
your team, then run on your iPhone. The first launch asks for **Local
Network** access; allow it, or discovery and pairing won't work.

## Using it

1. On the box, run `tt-station-agentd` (see the top-level README) and serve a chat
   model with good tool calling. Qwen3-32B on a P300x2 box was 10/10 on our command
   test; Llama-3.1-8B was not reliable enough.
2. The box appears under **Nearby** (keep the phone on the same network as the box).
3. Pair: open TT Station on the box (**Settings → Pairing and security**) and scan the
   pairing QR with the iPhone Camera app. The `ttstation://pair?...` link carries the
   box's HTTPS port (`tls`) and certificate fingerprint (`fp`); the app pairs over
   HTTPS pinned to that fingerprint and refuses links without them. There is no
   typed-code pairing (it couldn't pin the certificate). Boxes paired by an older
   version show "Pair again to connect securely" until they are re-paired. After
   pairing, the app sends the box this iPhone's own SSH public key (Settings → SSH key).
4. Chat. Read-only questions ("what's running, how hot are the chips?") answer
   directly. Disruptive actions (start/stop a model, reset chips) come back as a card
   with **Confirm** / **Cancel**.

More:

- **Voice.** With the composer empty, the send button is a microphone: speak, pause,
  and the answer is read aloud, then it listens again until you tap ✕. Recognition
  is on-device (Speech framework); only text goes to the box. Voice turns ask for
  short plain answers.
- **Documents.** The magnifier-on-page button in the Assistant tab uploads PDFs and
  text (extracted on the phone with PDFKit). Ask about them in chat; the assistant
  searches them on the box (`search_documents`) and names the source.
- **Chat and images together.** On a P300x2 box, the Create tab offers to run a
  chat model on one card and an image model on the other (e.g. Llama-3.1-8B +
  FLUX.1-schnell): ask the assistant things while images render. Starting any
  other model takes the whole box back.
- **Pictures in chat.** While an image model serves beside chat, ask "draw me…" and
  the picture appears in the conversation (and in your gallery).
- **Connect apps.** Box tab → Connect apps: mint a key per app and use the box as a
  private OpenAI API from Open WebUI, Continue, scripts (copy-ready setup inside).
- **Benchmark.** Box tab → "How fast is my box?": one-user and 8-user speed, image
  time, power and electricity cost per million tokens, as a shareable card.
- **Siri & Shortcuts.** "Ask TT Station…", "TT Station status", "Draw with TT
  Station" -- also in Spotlight, the Action button and Shortcuts automations.
- **Notifications.** Ordinary push notifications when downloads finish, models are ready
  or fail, the box comes online, or chips run hot; the app asks when you start a download or
  a model. The box sends them through APNs once it has the app's push key (see
  `docs/reference/alerts-and-power.md`); until then the app checks for them itself.
- **Power.** Box tab → Power: sleep, restart, shut down, and wake (Wake-on-LAN); the
  setup sheet lists the one-time commands to run on the box, and the smart-plug route
  for turning it on over Wi-Fi.
- **Jobs.** Box tab → Jobs: transcribe recordings of any length on the box, now or overnight; you're notified when it's done, and the transcript joins Documents for the assistant.
- **SSH.** Settings → SSH key: this iPhone's own Ed25519 key (made on the phone, kept
  in its Keychain, never synced). See its fingerprint and the `ssh` command, send it to
  or remove it from the box, or export the private key (OpenSSH format, after a warning)
  for an SSH app like Termius.

Starting a different model replaces the one that powers the assistant, so the
assistant is offline until the new model is ready; the status bar says so.

## Layout

| File | What |
|---|---|
| `Sources/TTStationMobileApp.swift` | App entry; onboarding until a box is paired, then the four tabs and the glass "now serving" accessory |
| `Sources/AppModel.swift` | App-wide state: paired boxes, discovery, the active box's session and telemetry |
| `Sources/ChatView.swift` | **Assistant** tab: chat bubbles, confirm cards, glass composer; explains/offers a switch when a non-chat model serves |
| `Sources/CreateView.swift` | **Create** tab: image/video studio for whatever serves, else a launcher (start or download image/video models); gallery + viewer (save to Photos, share, reuse) |
| `Sources/ImageStudio.swift` | Text-to-image: prompt, style chips, per-model step limits (FLUX schnell/dev), count, seed |
| `Sources/VideoStudio.swift` | Text-to-video jobs: submit, poll (survives relaunch), download, cancel |
| `Sources/Gallery.swift` | On-device store of creations (`Documents/Creations`) |
| `Sources/DashboardView.swift` | **Box** tab: serving hero, Happening now (Orchard run, transcriptions with progress), chip summary → Hardware page (chip cards, health, inference, energy, history, benchmark, log), Box Doctor, Explore grid (Orchard, Transcribe, Lab, Benchmark); stop/reset/power and style in the ⋯ menu; box switcher |
| `Sources/SettingsView.swift` | **Settings** tab: Boxes, Connect apps, Notifications, Power and wake, Box Doctor, Software, Memories, Documents, appearance (dashboard style, °C/°F), about |
| `Sources/SettingsPages.swift` | Settings pages: You and your household (people), Schedules, SSH key (this iPhone's key: fingerprint, send/remove, export), Energy (by day and month) |
| `Sources/VoiceMode.swift` | Hands-free voice on one always-running `AVAudioEngine` (`VoiceEngine`): on-device recognition with audio-level endpointing, the reply rendered with `AVSpeechSynthesizer.write` and queued gaplessly on one player node (first sentence alone, then batched), normal media volume (no `.voiceChat` processing), a level-driven Vesper orb, voice picker, haptics, route-change and interruption recovery |
| `Sources/LiveActivities.swift` | Lock Screen / Dynamic Island Live Activities for a model loading, an Orchard run and a transcription, updated while the app runs (no push) |
| `Shared/BoxActivity.swift`, `ActivityExtension/` | The Live Activity's attributes (shared) and the widget extension that draws it |
| `Resources/Fonts/` | Vesper's typefaces: Inter Tight (static cuts from the variable font) and IBM Plex Mono, OFL |
| `Sources/ToplikeView.swift` | tt-toplike style: mode bar, terminal header, Tensix glyph starfield, warp field, readouts, sparklines, scanlines |
| `Sources/ToplikeModes.swift` | tt-toplike's other views from the full telemetry frame: Insights (per-chip panels, DDR training, processes), Memory Castle, Memory Flow, Defrag, Arcade (with the `@` hero), Inference monitor (hungry snake / compile→load→ready / READY with tok/s) |
| `Sources/ModelsView.swift` | **Models** tab: the box's tt catalog — download (with progress, cancel), start, recommended picks |
| `Sources/BoxesView.swift` | First-run onboarding, and Settings → Boxes (`BoxesList`): paired boxes, nearby, how to pair (scan the box's QR) |
| `Sources/PairSheet.swift` | Pairing help: where to find the box's pairing QR (QR links are handled in `AppModel.handle(url:)`, over pinned HTTPS) |
| `Sources/SSHKey.swift` | This iPhone's SSH key: Ed25519 in the Keychain, OpenSSH public key and fingerprint, OpenSSH private-key export |
| `Sources/DocumentsView.swift` | Documents sheet: upload (PDFKit/RTF/HTML/text extraction), list, delete |
| `Sources/ConnectAppsView.swift` | Connect apps: the box's OpenAI endpoint, per-app keys (mint/revoke), copy-ready setup for Open WebUI, SDKs, curl, Continue |
| `Sources/BenchView.swift` | Benchmark card: one-tap speed/throughput/image/energy test, shareable as an image |
| `Sources/Intents.swift` | App Intents for Siri, Spotlight, Shortcuts and the Action button: Ask My Box, Box Status, Make an Image |
| `Sources/Alerts.swift` | App delegate (APNs token), alert center (registers with the box; fallback polling + local notifications), in-app banner, Notifications screen |
| `Sources/Power.swift` | Power card (sleep/restart/shut down/wake), Wake-on-LAN sender, remote power setup sheet |
| `Sources/BoxCare.swift` | Mode picker ("What do you want to do?"), Box Doctor (fixes, diagnostics share), Software |
| `Sources/LabView.swift` | Silicon check: tt-lab runs of gpt-oss on bare silicon -- prepare models, stream output (Harmony thinking/answer), bit-exact checks, per-stage profile chart, history |
| `Sources/JobsView.swift` | Jobs: upload recordings to transcribe (now / tonight), live progress, "switch now", transcript viewer (share text/SRT, ask the assistant) |
| `Sources/OrchardView.swift` | Orchard: tt-orchard bring-ups -- setup checklist, preflight, start (stop-the-model consent), runs with a Now card (step bar, time left, whole-run bar, what to do) and nine-stage progress, pause/resume/stop/retry, harvest-basket reports |
| `Sources/Recorder.swift` | Record on the phone for Transcribe, in place (the bottom bar becomes the recorder: timer, level, Stop and send / Discard). AAC 16 kHz mono into Documents/Recordings, kept until the box has it (unsent ones come back next time); keeps going with the screen locked |
| `Sources/Voice.swift` | Voice mode: on-device speech recognition with silence detection, spoken answers, the listening bar |
| `Sources/Markdown.swift` | Markdown renderer for answers (headings, lists, tables, code with copy) |
| `Sources/Conversations.swift` | Saved conversations per box |
| `Sources/DashboardCharts.swift` | Box tab charts: history (temp/power/throughput/clock), inference, energy, health |
| `Sources/BoxSession.swift` | Per-box state: status polling, conversation, confirmations, direct controls |
| `Sources/Telemetry.swift` | Live chip readings from the agent's `/telemetry?view=lite` WebSocket |
| `Sources/AgentAPI.swift` | Wire types + async `AgentClient` for the agent's HTTP API |
| `Sources/Discovery.swift` | `_tenstorrent._tcp` browsing via `NWBrowser`; IPv4 first, `<name>.local` for link-local |
| `Sources/BoxStore.swift` | Paired boxes (UserDefaults) and their bearer tokens (Keychain) |
| `Sources/Brand.swift` | Vesper design tokens (`Vesper`: stone/teal/brand colors, radii), `Brand` roles, the dot-grid backdrop, `Font.tt` / `Font.ttMono`, glass cards with hairline borders, Status Indicator |
| `Assets.xcassets` | App icon, vector wordmark, accent and launch colors |

`TTStationMobile.entitlements` turns on push (needs a paid Apple Developer team).
`project.yml` is the source of truth for the Xcode project; the generated
`TTStationMobile.xcodeproj` is git-ignored.

## Prototype caveats

- The agent speaks plain HTTP, so `Info.plist` allows arbitrary loads. Tighten this
  once the agent serves TLS.
- It doesn't share code with `macos/TTStation` yet: `TTStationKit` drives the
  `tt-station` CLI through `Process`, which doesn't exist on iOS. The pure wire
  types could move into a shared package later.
