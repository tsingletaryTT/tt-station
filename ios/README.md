# TT Station for iPhone

TT Station on your iPhone (and iPad), connected to your own Tenstorrent box (a QuietBox,
a Galaxy server, or a workstation with Tenstorrent cards running TT Station). Chat with
the model the box serves, by text or voice; make images; transcribe
recordings; see the chips' temperature, power and load; start, stop and download models;
put the box to sleep and wake it. The app talks only to your box, over HTTPS pinned to the
box's own certificate, on your network.

## Requirements

- An iPhone or iPad on **iOS 26** or later (the app is built on Liquid Glass).
- A Mac with **Xcode 26** and [XcodeGen](https://github.com/yonaskolb/XcodeGen).
- An Apple developer team to sign the app: a free personal team works, with the limits
  below.
- A box running TT Station on the same network (see the
  [top-level README](../README.md)).

## Build and install

```bash
brew install xcodegen
cd ios
xcodegen                      # makes TTStationMobile.xcodeproj from project.yml
open TTStationMobile.xcodeproj
```

In Xcode, select the **TTStationMobile** target → **Signing & Capabilities** and choose
your team (do the same for **TTStationActivity**),
then run the **TTStationMobile** scheme on your iPhone. If Xcode says a bundle identifier
isn't available, change the `com.tenstorrent.ttstation.mobile…` identifiers in
`project.yml` to your own prefix and run `xcodegen` again.

`project.yml` is the source of truth for the Xcode project; the generated
`TTStationMobile.xcodeproj` is git-ignored.

- **Free (personal) team:** the app runs on your own devices, and stops opening after
  7 days until you build and run it again from Xcode.
- **Paid team:** needed for TestFlight and the App Store.
- **Notifications** need no push setup: the app checks the box's event feed while it's
  open and from background refresh, and shows each alert as a local notification.

## Pair with your box

1. Keep the iPhone on the same network as the box. The box appears under **Found nearby**.
2. Open TT Station on the box (**Settings → Devices and pairing**) and point the iPhone's
   Camera app at the pairing QR code, then tap the link. No screen on the box? Run
   `tt-station-agentd pair` in a terminal on it: it prints the QR code there.
3. The code carries the box's HTTPS port and certificate fingerprint; the app pairs over
   HTTPS pinned to that certificate and refuses links without them. There's no typed-code
   pairing, because a typed code couldn't pin the certificate.

After pairing, the app sends the box this iPhone's own SSH public key (Settings → SSH
key). Owners can see paired devices and change their roles in Settings → Devices and
security. What the phone can and can't do compared with the desktop app:
[mobile-parity.md](../docs/reference/mobile-parity.md).

## What it asks for, and why

| Permission | Why |
|---|---|
| Local Network | Find the box on your network and talk to it. Without it, discovery and pairing don't work. |
| Microphone | Voice chat with the box, and recording meetings or notes for it to transcribe. |
| Speech Recognition | Turn your speech into text on the iPhone (on-device where supported); only the text goes to the box. |
| Notifications | Tell you when a download finishes, a model is ready or fails, the box comes online, or chips run hot. Asked when you start a download or a model. |
| Photos (add only) | Save the images your box makes. |

The app also uses background refresh (to check for alerts) and background audio (so a
voice conversation or a recording keeps going with the screen locked). It has no tracking
and collects nothing: see `PrivacyInfo.xcprivacy`.

## Using it

- **Chat.** Ask the box's served model anything. Questions about the box ("what's
  running, how hot are the chips?") are answered directly; anything disruptive (start or
  stop a model, reset the chips) comes back as a card to **Confirm** or **Cancel**.
  Starting a different model replaces the one behind the chat until it's ready.
- **Voice.** With the composer empty, the send button is a microphone: speak, pause, and
  the answer is read aloud, then it listens again until you tap ✕.
- **Documents.** Upload PDFs and text from the Chat tab; the assistant searches them on
  the box and names its source.
- **Create.** Images from whatever image model the box serves, saved to an on-device
  gallery.
- **Box.** Live chip readings, history and energy, Box Doctor, Jobs (transcribe long
  recordings now or overnight), Code, Research, Routines, a shareable benchmark ("How fast is my
  box?"), and sleep and power (⋯ → Sleep and power; Wake-on-LAN where it can work, see
  [sleep-and-wake.md](../docs/reference/sleep-and-wake.md)).
- **Models.** The box's catalog: download (with progress), start, recommended picks.
- **Connect apps** (Settings). Make a key per app and use the box as a private OpenAI
  API from Open WebUI, Continue or scripts, with copy-ready setup.
- **Siri and Shortcuts.** "Ask TT Station…", "TT Station status", "Draw with TT
  Station", also in Spotlight, the Action button and Shortcuts automations.
- **SSH.** Settings → SSH key: this iPhone's own Ed25519 key, made on the phone and kept
  in its Keychain (never synced). Send it to or remove it from the box, or export the
  private key (OpenSSH format, after a warning) for an SSH app.

## Source layout

| File | What |
|---|---|
| `Sources/TTStationMobileApp.swift` | App entry; onboarding until a box is paired, then the five tabs (Chat, Create, Box, Models, Settings) and the glass "now serving" accessory |
| `Sources/AppModel.swift` | App-wide state: paired boxes, discovery, the active box's session and telemetry |
| `Sources/ChatView.swift` | **Chat** tab: chat bubbles, confirm cards, glass composer; explains/offers a switch when a non-chat model serves |
| `Sources/CreateView.swift` | **Create** tab: the image studio while an image model serves, else a launcher (start or download image models); gallery + viewer (save to Photos, share, reuse) |
| `Sources/ImageStudio.swift` | Text-to-image: prompt, style chips, per-model step limits (FLUX schnell/dev), count, seed |
| `Sources/Gallery.swift` | On-device store of creations (`Documents/Creations`) |
| `Sources/DashboardView.swift` | **Box** tab: serving hero, Happening now, chip summary → Hardware page, Box Doctor, Explore grid (Code, Research, Routines, Transcribe, Benchmark); stop/reset/power in the ⋯ menu; box switcher |
| `Sources/DashboardCharts.swift` | Box tab charts: history (temp/power/throughput/clock), inference, energy, health |
| `Sources/ChipStyle.swift` | Chip colors and load for the Box tab's charts |
| `Sources/GlanceViews.swift` | The Box tab's "right now" card, the unreachable banner (Retry, Wake), the update-needed banner |
| `Sources/ModelsView.swift` | **Models** tab: the box's catalog — download (with progress, cancel), start, recommended picks |
| `Sources/ServeAnyView.swift` | Everything the box can serve, with load progress and the model-control rule |
| `Sources/SettingsView.swift` | **Settings** tab: Boxes, household, schedules, SSH key, energy, Devices and security, Connect apps, Notifications, Sleep and wake, Box Doctor, Software, Updates, Memories, Documents, appearance, about |
| `Sources/SettingsPages.swift` | Settings pages: You and your household, Schedules, SSH key, Energy |
| `Sources/BoxesView.swift` | First-run onboarding, and Settings → Boxes: paired boxes, nearby, how to pair |
| `Sources/PairSheet.swift` | Pairing help: where to find the box's pairing QR |
| `Sources/PairLink.swift` | Parses a scanned `ttstation://pair` link (tested in `Tests/PairingTests.swift`); `AppModel.handle(url:)` pairs |
| `Sources/Discovery.swift` | `_tenstorrent._tcp` browsing via `NWBrowser`; IPv4 first, `<name>.local` for link-local |
| `Sources/BoxStore.swift` | Paired boxes (UserDefaults) and their tokens (Keychain) |
| `Sources/BoxSession.swift` | Per-box state: status polling, conversation, confirmations, direct controls |
| `Sources/AgentAPI.swift` | Wire types + async `AgentClient` for the box's HTTP API (a refused token offers "Pair again") |
| `Sources/Telemetry.swift` | Live chip readings from the box's `/telemetry?view=lite` WebSocket |
| `Sources/BoxControlModels.swift`, `Sources/BoxControlLogic.swift` | Foundation-only wire types and decisions (versions, wake planning, grouping), tested in `Tests/BoxControlTests.swift` |
| `Sources/BoxControlAPI.swift` | The box routes behind those controls |
| `Sources/BoxCare.swift` | Mode picker ("What do you want to do?"), Box Doctor, Software |
| `Sources/DevicesViews.swift` | Devices and security (roles, remove, sign out, model-control rule), activity log, updates |
| `Sources/Power.swift` | The whole box's power (sleep/restart/shut down, owners), Wake-on-LAN, remote power setup |
| `Sources/SleepViews.swift` | Model sleep: sleep now, wake, the idle-sleep rule |
| `Sources/Alerts.swift` | App delegate (notification actions), alert center (polls the box's `/events`, posts local notifications), in-app banner, Notifications screen |
| `Sources/LiveActivities.swift` | Lock Screen / Dynamic Island Live Activities for a model loading and a transcription |
| `Shared/BoxActivity.swift`, `ActivityExtension/` | The Live Activity's attributes (shared) and the widget extension that draws it |
| `Sources/WorkViews.swift` | "Waiting for you": everything on the box that needs your OK (a Code session's command or edit, a routine's action) |
| `Sources/CodeNewSession.swift` | New sandboxed Code session: recent folders, or browse the box |
| `Sources/VoiceMode.swift` | Hands-free voice: on-device recognition with endpointing, spoken replies, voice picker, the orb |
| `Sources/Voice.swift` | Voice: on-device speech recognition with silence detection, spoken answers, the listening bar |
| `Sources/Recorder.swift` | Record on the phone for Transcribe (AAC 16 kHz mono into `Documents/Recordings`, kept until the box has it); keeps going with the screen locked |
| `Sources/JobsView.swift` | Jobs: upload recordings to transcribe (now / tonight), progress, transcript viewer |
| `Sources/ResearchViews.swift` | Research: start, follow, read with sources, share, stop |
| `Sources/DocumentsView.swift` | Documents: upload (PDF/RTF/HTML/text extraction), list, delete |
| `Sources/MemoriesSheet.swift` | What the assistant remembers about you, kept on your box; delete any of it |
| `Sources/Conversations.swift` | Saved conversations per box |
| `Sources/Markdown.swift` | Markdown renderer for answers (headings, lists, tables, code with copy) |
| `Sources/ConnectAppsView.swift` | Connect apps: the box's OpenAI endpoint, per-app keys, copy-ready setup for Open WebUI, SDKs, curl, Continue |
| `Sources/BenchView.swift` | Benchmark card: speed, throughput, image and energy test, shareable as an image |
| `Sources/Intents.swift` | App Intents for Siri, Spotlight, Shortcuts and the Action button |
| `Sources/SSHKey.swift` | This iPhone's SSH key: Ed25519 in the Keychain, OpenSSH public key and fingerprint, private-key export |
| `Sources/KeyboardDone.swift` | A Done button above the keyboard for multi-line text boxes |
| `Sources/Brand.swift` | Vesper design tokens, `Brand` roles, the dot-grid backdrop, `Font.tt` / `Font.ttMono`, glass cards |
| `Resources/Fonts/` | Vesper's typefaces: Inter Tight and IBM Plex Mono (OFL, license texts beside them) |
| `Assets.xcassets` | App icon, wordmark, accent and launch colors |
| `PrivacyInfo.xcprivacy` | The app's privacy manifest |
| `Tests/` | Unit tests for pairing links, certificate pins and box-control logic |
