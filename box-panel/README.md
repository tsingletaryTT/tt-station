# tt-station box panel

A tiny GTK4 control surface that runs **on the QuietBox** — the physical box's
little face for tt-station. Not a dashboard (that's a different tool); just
enough to know "hey, it's working," pair a client, and start/stop the agent.

It shows:
- the **6-digit pairing code, big**, the instant a client pairs (read straight
  from the agent's own output — no log-scraping), with a TTL countdown
- a **status pill** + line: stopped / starting / idle / `serving:<model>`, chips,
  and the `/v1` endpoint when a model is up
- **Start · Stop · Restart** the `tt-station-agentd` daemon (the panel supervises
  it as a child, with the box-local config baked in — device auto-detected,
  token persistence on)
- **Reset** — return the box to a fresh state via `tt reset` (stop model, clear
  pairings, reset the board)

## Run

    python3 box-panel/tt-station-panel.py

Requires GTK4 + PyGObject (`python3-gi`, `gir1.2-gtk-4.0` — present on this box).
Run it from the repo root so the default binary paths (`./target/release/…`)
resolve, or set the env vars below.

## Config (env vars, sensible box defaults)

| Var | Default | Meaning |
|-----|---------|---------|
| `TTS_AGENT_BIN` | `./target/release/tt-station-agentd` | agent binary |
| `TTS_TT_BIN` | `./target/release/tt` | `tt` CLI (for Reset) |
| `TTS_NAME` | `qb2-lab` | `--name` |
| `TTS_CTRL_PORT` | `8765` | control-plane port |
| `TTS_SERVING_HOST` | `<name>.local` | endpoint host baked into `base_url` |
| `TTS_SERVING_PORT` | `8003` | serving port |
| `TTS_REPO` | `~/code/tt-inference-server` | `--tt-inference-repo` |
| `TTS_IMAGE` | *(unset)* | optional `--serving-image` override |
| `TTS_HF_ENV` | `<repo>/.env` | file to read `HF_TOKEN` from |

The panel deliberately carries the **box-local** knowledge (repo path, serving
host/port, HF token source) so the operator just clicks Start — the client side
stays zero-config. `--tt-device` and `--serving-image` are left to the agent's
auto-detect / pin logic unless overridden here.
