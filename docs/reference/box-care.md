# Modes, Box Doctor, software, household apps and HTTPS (reference)

## Modes (`GET /modes`, `POST /modes/{id}`)

Intent instead of model names: `chat` (Qwen3-32B → Llama-3.3-70B → gemma-4-31B-it →
Qwen3.6-27B → Llama-3.1-8B-Instruct), `images` (FLUX.1-schnell → FLUX.1-dev),
`chat_images` (co-serve: Llama-3.1-8B-Instruct + FLUX on one P300 each; carries the
half-box known-issue note), `video` (Wan2.2), `transcribe` (whisper-large-v3 →
distil-large-v3). Each mode picks a downloaded model before a better missing one, reports
`ready`/`needs_download`/`active` and `eta_s` -- the median of this box's own past loads,
recorded at every finished start in `$TT_CONFIG_DIR/load-times.json`. `GET /serve` (and its
`companion`) carry `eta_s` while loading. `POST /modes/{id}`: `202` started, `200` already
active, `409` with the reason (e.g. what to download). The assistant's confirm-gated
`switch_mode` tool uses the same path; `create_image` without an image model suggests it.

## Box Doctor (`GET /doctor`, `POST /doctor/fix/{fix}`, `POST /diagnostics`)

Pure checks over gathered facts (`doctor::checks`): chips found vs expected, cooling vs
each chip's trip limit, firmware consistency and driver, disk free, 1 GB hugepages, Docker,
the serving model actually answering (or loading > 45 min), stray inference containers,
failures in the last day, start-at-boot (linger), headless power, push, a newer `tt`, and
leftover old installs. Statuses `ok`/`info`/`warn`/`fail`; safe fixes the agent applies
itself: `restart_model`, `stop_stray_containers`, `reset_chips` (idle only); otherwise a
`command` to run on the box. Nightly at ~03:30 it runs and pushes "Box Doctor found N
problems". `POST /diagnostics` builds a `.tar.gz` (Doctor report, tt-smi, events,
versions, agent journal, serving logs; secrets redacted) under
`$TT_CONFIG_DIR/diagnostics/`; `GET /diagnostics/{id}` downloads it.

## Software (`GET /software`, `POST /software/cli-update`)

From the `tt` CLI: `tt self tools` (managed tools vs golden versions), `tt self
check-update`, and `tt update --dry-run` -- whose system stack is flagged
`downgrade: true` when it would install an older KMD than the box runs (seen: it targets
2.10.0 over an installed 2.11.0), so the app advises skipping it. Old installer copies
(`~/.local/lib/tt-inference-server`, `tt-studio`) are listed with their size.
`cli-update` runs `tt self update -y`. `DELETE /tt/models/{name}?weights=true` runs
`tt model rm --include-weights` (refused for a model in use).

## Household apps (`GET /clients`, `POST /clients/{tool}/start|stop`)

`tt launch openwebui|anythingllm` as a container wired to the served chat model
(`--url`), on host ports 3010/3011 (3000 is TT-Studio's). Anyone on the network opens the
address; the app shows it with a QR code.

## HTTPS (`crate::tls`, `GET /tls`)

The agent makes a self-signed certificate once (`tls-cert.pem`/`tls-key.pem`, 0600; SANs:
localhost, `<name>.local`, LAN and Tailscale IPs) and serves the same API over TLS on the
control port + 1 (8766) via axum-server/rustls (ring). Pairing hands out
`{port, sha256}`: in the QR code (`&tls=…&fp=…`, from the box's own terminal) and in
`/pair/complete`; the app pins that fingerprint (a `URLSessionDelegate` comparing the leaf
certificate's SHA-256) and then talks only HTTPS. QR pairing refuses if the network's
answer doesn't match the QR. Phones paired earlier upgrade once via `GET /tls` (trust on
first use, like pairing) after proving the pinned connection works. Plain HTTP stays for
discovery, telemetry and older clients.
