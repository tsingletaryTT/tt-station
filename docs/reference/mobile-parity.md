# iPhone parity: controlling the box from the phone

What the desktop app (`desktop/src/pages`) lets you do to control or inspect a QuietBox,
and whether the iPhone app (`ios/`) can do it. "Before" is the iPhone app at `6976289`
(`jberkowitz/mobile-voice`), "After" is the branch `jberkowitz/mobile`. Routes are the
agent's (`crates/tt-station-agentd/src/router.rs`); "owner" means the box refuses
members (`OwnerAuth`), "model rule" means the owners' model-control setting applies.

**Has**: works. **Lacks**: not in the app. **Broken**: in the app but wrong. **Desktop
only**: deliberately left to a computer (needs a keyboard, a terminal or a file system).

## Requests and errors

| Behavior | Before | After |
|---|---|---|
| Token on every request (`AgentClient.send`/`raw`, WebSockets, the notification extension) | Has (verified: `send` adds `Authorization` whenever a token exists, `raw` and the streams require it) | Has |
| `401` → pair again | Broken: only chat, model starts, downloads and images set `needsRepair`; every other screen showed a raw error | Has: any refused token from any request posts `AgentClient.unauthorized`, and the session shows "Pair again" |
| `403` → plain "Only owners can …" | Broken: the box's text passed through, but members could tap owner-only buttons first | Has: the box's sentence (`OWNER_ONLY`, `model_control::OWNERS_ONLY`), and owner-only controls are disabled for members up front |
| Box unreachable | Broken: an "Unreachable" headline on the Box tab only | Has: a banner over every tab with Retry and Wake (or why waking can't work) |
| `/status` `api_version` / `min_client` | Lacks | Has: "update the app" (or "update the box") banner (`ClientCompat`) |

## Status and notifications

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Serving state, load progress | `GET /status`, `GET /serve` | Has | Has |
| Model not answering, restart it | `/status` `model_health`, `POST /doctor/fix/{fix}` | Lacks | Has (model rule) |
| Model asleep (idle sleep) | `GET /idle` | Lacks | Has |
| Chips and temperatures | `/telemetry` (WebSocket), `GET /metrics/now` | Has | Has |
| What's running and who started it | `GET /record`, `GET /code`, `GET /research`, `GET /routines` | Lacks | Has ("right now" card) |
| Code and routine approvals, Allow/Deny | `GET /approvals`, `POST /approvals/answer` | Has | Has |
| Bring-up approvals (owners) | `GET /bringup/codex/runs`, `POST /bringup/codex/runs/{id}/approvals/{aid}` | Has | Has |
| Pairing codes (owners) | `GET /events` (`pairing`) | Has (banner, notification) | Has |
| Push notifications | `/notify`, `POST /notify/devices` | Has | Has |

## Models

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Catalog, download, cancel, delete weights | `GET /tt/models`, `POST /tt/pull`, `POST /tt/pull/cancel`, `DELETE /tt/models/{name}` | Has | Has |
| Start or switch a catalog model, stop | `POST /serve`, `POST /stop` | Has | Has (model rule shown) |
| Modes, chat + images side by side | `GET /modes`, `POST /modes/{id}`, `/coserve` | Has | Has |
| Community packages and their profiles, native vLLM bundles | `GET /code/models` (each entry's own serve call: `POST /community/serve`, `POST /vllm/native/serve`) | Lacks | Has ("Everything it can serve") |
| Community downloads (pull, pause, cancel, delete) | `/community/pull`, `/community/pull/pause`, `/community/pull/cancel`, `DELETE /community/{ns}/{name}` | Lacks | Lacks: start what's on the box; downloading packages stays on the computer |
| The model-control rule | `GET /data/settings`, `PUT /data/settings` | Lacks | Has (owners change it; members see it) |
| Checkups (evals), CPU embeddings, model preferences | `/evals`, `/embeddings`, `/preferences/models` | Lacks | Lacks |

## Sleep, power, wake

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Put the model to sleep now, wake it | `POST /idle/sleep` (new), `POST /idle/wake` | Lacks | Has (model rule) |
| Idle sleep settings | `GET /idle`, `PUT /idle` | Lacks | Has (model rule) |
| Suspend, restart, shut down the box | `POST /power` | Broken: buttons enabled for members (refused by the box) and no word on whether the phone could wake it again | Has: owners only, with what it means for everyone and for waking |
| Reset chips | `POST /power` `reset-chips` | Has | Has (owners) |
| Wake a sleeping box (Wake-on-LAN) | `GET /power/info` `wake_on_lan` (new) | Broken: remembered a MAC and sent packets whether or not waking could work, with no reason when it couldn't | Has: only when it can work; says why not (Wi-Fi only, not set up, another network) and offers model sleep instead |
| Turn Wake-on-LAN on | `POST /power/wake-on-lan` (new) | Lacks | Has (owners; or the exact command) |
| One-time power setup steps | `GET /power/info` `setup` | Has | Has |
| Start the last model after a power cut | `PUT /data/settings` `restart_last_model` | Lacks | Lacks |

## Code

| Capability | Route(s) | Before | After |
|---|---|---|---|
| List sessions, follow output | `GET /code`, `GET /code/sessions/{id}` | Has | Has |
| Send a message, interrupt a turn | `POST /code/sessions/{id}/message`, `POST /code/sessions/{id}/interrupt` | Has | Has |
| Answer its questions | `POST /approvals/answer` | Has | Has |
| Stop a session's agent | `POST /code/sessions/{id}/stop` | Lacks | Has |
| Start a sandboxed session (recent folder, or browse the box) | `POST /code/sessions`, `GET /coding/folders` | Lacks | Has |
| Review changes, commit, discard; rename; outside-the-sandbox sessions | `GET /code/sessions/{id}/changes`, `POST /code/sessions/{id}/commit`, `POST /code/sessions/{id}/discard`, `POST /code/sessions/{id}/rename` | Lacks | Desktop only |

## Research, transcripts, recordings

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Start research, follow it | `POST /research`, `GET /research` | Lacks | Has |
| Read a report with its sources | `GET /research/{id}` | Lacks | Has |
| Share with everyone on the box, stop | `POST /research/{id}/share`, `POST /research/{id}/stop` | Lacks | Has (starter or owner) |
| Explain a passage, rename, delete | `POST /research/{id}/explain`, `POST /research/{id}/rename`, `DELETE /research/{id}` | Lacks | Lacks |
| Transcribe a recording or file; list, read, run now, delete | `POST /jobs/transcribe`, `GET /jobs`, `GET /jobs/{id}/result`, `POST /jobs/{id}/run`, `DELETE /jobs/{id}` | Has | Has |
| Record in the room on the box (start, stop, discard) | `GET /record`, `POST /record/start`, `POST /record/stop` | Lacks | Has (asks first; the room is told) |

## Routines

| Capability | Route(s) | Before | After |
|---|---|---|---|
| List, run now, follow a run, stop it | `GET /routines`, `POST /routines/{id}/run`, `GET /routines/runs/{run}`, `POST /routines/runs/{run}/stop` | Has | Has |
| Approve a queued step | `POST /approvals/answer` | Has | Has |
| Pause and resume | `PUT /routines/{id}` | Lacks | Has |
| Create, edit, delete | `POST /routines`, `PUT /routines/{id}`, `DELETE /routines/{id}` | Lacks | Desktop only |

## Devices and security

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Devices with roles | `GET /devices` | Lacks | Has |
| Promote, demote, remove (owners); sign this phone out | `POST /devices/{id}/owner`, `DELETE /devices/{id}` | Lacks | Has |
| Activity log (owners) | `GET /audit` | Lacks | Has |
| Assign a device to a person (owners) | `PUT /devices/{id}/person` | Lacks | Lacks (this phone picks its own person in Household) |
| App keys | `GET /keys`, `POST /keys`, `DELETE /keys/{id}` | Has | Has |
| SSH keys | `POST /ssh/authorize`, `GET /ssh/info` | Has | Has |
| Pair another device (QR) | `POST /pair/qr` | Desktop only (box-local) | Desktop only |

## Box care

| Capability | Route(s) | Before | After |
|---|---|---|---|
| Doctor checks and fixes | `GET /doctor`, `POST /doctor/fix/{fix}` | Broken: owner-only fixes offered to members | Has (owner-only fixes say so) |
| Diagnostics bundle (owners) | `POST /diagnostics`, `GET /diagnostics/{id}` | Has | Has (owners) |
| Software versions, update the tt CLI | `GET /software`, `POST /software/cli-update` | Has | Has |
| TT Station updates (signed): status, install | `GET /updates`, `POST /updates/install` | Lacks | Has (install: owners) |
| Undo an update | `POST /updates/undo` | Lacks | Lacks |
| Export and import | `POST /export`, `POST /import` | Lacks | Desktop only (files) |

## Also on the phone, unchanged

Chat and Tori (`/assistant/stream`), images and video (`/generate/*`), memories, documents,
household and schedules, energy, bring-ups (Orchard), Lab, Benchmark, tt-toplike, the
box's voice, browser apps (`/clients`), Live Activities, the widget and Siri intents.

Not on the phone: box apps from the store (`/apps`, box-only by design, with their own
network sharing), Library sources, Hermes, favorites and What's new.
