# `tt-station-agentd` config file + named profiles (reference)

*Documents the shipped behavior of `crates/tt-station-agentd/src/config.rs`,
`main.rs`, and `routes.rs` as of the config-file + named-profiles feature.
Design history: `docs/superpowers/specs/2026-07-05-agentd-config-profiles-design.md`.*

The agent can be configured two ways, and they compose: **CLI flags** (as
before) and an optional **`agentd.toml`** file with **named serving
profiles** (e.g. a `stable` checkout/image pinned for daily use, and a
`bleeding` one for testing a newer `tt-inference-server`). Neither is
required — an agent with no config file and no flags behaves exactly as it
always has (built-in defaults).

## File location

Default path: `$TT_CONFIG_DIR/agentd.toml` if `TT_CONFIG_DIR` is set in the
environment, else `$HOME/.config/tt-station/agentd.toml` — the same
`TT_CONFIG_DIR` convention the `tt` CLI already uses.

Override with `--config <PATH>`:

```
tt-station-agentd --config /etc/tt-station/agentd.toml ...
```

**Absence at the default path is fine** — the agent falls back to flags/env/
built-in defaults (the "implicit default profile," i.e. today's behavior).
**Absence at an explicit `--config` path is an error** — a path the operator
named but that isn't there is treated as a mistake, not "use defaults."

## Full schema

Format is TOML. All fields in every section are optional; a field absent
everywhere falls through to the precedence chain below and, ultimately, a
built-in default (or, for `name`/`ctrl_port`, a hard error if never set at
all — see "Precedence").

### Top level

| Key | Type | Meaning |
|---|---|---|
| `default_profile` | string | Name of the `[profile.*]` to activate when `--profile` isn't passed on the CLI. See "Active profile selection." |

### `[global]` — box identity, shared across all profiles

| Key | Type | Meaning |
|---|---|---|
| `name` | string | Box name; the mDNS instance name and the `name` TXT/JSON key. Required overall (via this, `--name`, or nothing — see "Precedence"). |
| `ctrl_port` | integer (u16) | Control-plane HTTP port to bind/advertise. Required overall (via this, `--ctrl-port`, or nothing). |
| `chips` | string | Chip inventory string advertised in the `chips` TXT key / `/status`. Default `"4xBH"`. |
| `apiver` | integer (u8) | API version advertised in the `apiver` TXT key. Default `1`. |
| `token_store` | string (path) | Where to persist issued bearer tokens. Default `~/.config/tt-station/agentd-tokens.json`. Ignored when `no_token_persistence` is set. |
| `no_token_persistence` | bool | Opt out of persisting bearer tokens across restarts (in-memory only). Default `false`. |
| `telemetry_interval_ms` | integer (u64) | Interval between `tt-smi -s` snapshots pushed on `GET /telemetry`. Default `1000`. |
| `tt_smi_bin` | string | `tt-smi` binary the telemetry stream runs. Default `"tt-smi"`. |

### `[profile.<name>]` — how a profile serves

A file may define zero, one, or many `[profile.<name>]` tables. Each is a
complete, independently-selectable serving configuration.

| Key | Type | Meaning |
|---|---|---|
| `backend` | string | `"runpy"` \| `"docker"` \| `"dstack"`. Default `"runpy"`. |
| `tt_inference_repo` | string (path) | Local `tt-inference-server` checkout (the `runpy` backend). Default: `./vendor/tt-inference-server` if present, else `$HOME/code/tt-inference-server`. |
| `serving_image` | string | Container image to serve (`run.py --override-docker-image`, or `docker run <image>` for the `docker` backend). No default for `runpy` (left unset so `run.py` can self-resolve via `model_spec.json`); the `docker` backend falls back to a hardcoded example tag if unset — pin this per box. |
| `auto_image` | bool | `runpy` only: opt in to auto-picking the newest locally-present release image when `serving_image` is unset. Default `false` — image↔run.py compatibility is a curated matrix, so pinning is safer. |
| `tt_device` | string | `--tt-device` value, e.g. `p300x2`, `n300`, `p150x4`. Optional — when unset, the `runpy` backend auto-detects from `tt-smi`; the `docker` backend falls back to `"p300x2"`. |
| `serving_host` | string | Host the serving container/VM is reachable on. Default `"127.0.0.1"`. |
| `serving_port` | integer (u16) | Host port the serving `/v1` is published on. Default `8000`. |
| `host_hf_cache` | string (path) | Host path bind-mounted for the Hugging Face weights cache (`runpy`'s `--host-hf-cache`). Default `$HOME/.cache/huggingface`. |
| `hf_token` | string (secret) | Hugging Face token for gated model repos. Falls back to the `HF_TOKEN` env var if unset here and on the CLI. **Never** exposed via `--print-config` or `GET /config`. |
| `no_device_reset` | bool | `runpy` only: skip the `tt-smi -r` board reset before serving. Default `false` (reset runs by default). |

Fields **not** part of a profile — always flag/env/built-in-default driven,
not configurable per profile (see `CliOverrides`' "runpy-only extras" in
`config.rs`): `--cache-volume`, `--require-auth`, `--device-path`,
`--hugepages-src`, `--engine`, `--impl`, `--device-id`, `--model-source`,
`--model-spec`.

A serving-scoped key placed under `[global]` (or a global-scoped key placed
under `[profile.*]`) is an **unknown key** there and is rejected at parse
time — see "Errors" below.

### Example (mirrors `box-panel/agentd.example.toml`)

```toml
default_profile = "stable"

[global]
name = "qb2-lab"
ctrl_port = 8765
chips = "4xBH"
token_store = "~/.config/tt-station/agentd-tokens.json"
telemetry_interval_ms = 1000
tt_smi_bin = "tt-smi"

[profile.stable]
backend = "runpy"
tt_inference_repo = "~/code/tt-inference-server"
serving_image = "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0-80180b9-7678b70"
tt_device = "p300x2"
serving_host = "qb2-lab.local"
serving_port = 8003
auto_image = false
host_hf_cache = "~/.cache/huggingface"

[profile.bleeding]
backend = "runpy"
tt_inference_repo = "~/code/tt-inference-server-next"
serving_image = "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.17.0-<sha>"
serving_port = 8004
```

## `~/` expansion

The **only** path expansion supported: a leading `~/` on any path-valued
field (`token_store`, `tt_inference_repo`, `host_hf_cache`) expands to
`$HOME/`. `~user/...`, `$VAR`, and a bare `~` are left untouched — write an
absolute or `$HOME`-relative path if you need something else.

## Precedence

For every setting, highest wins:

```
explicit CLI flag  >  environment variable  >  active profile  >  [global]  >  built-in default
```

- Every overridable CLI flag is `Option<T>` with **no clap default** — clap
  reports `None` when the operator didn't pass it, so "not passed" can be
  told apart from "passed the same value as the default." The hardcoded
  defaults (`"4xBH"`, `8000`, `"runpy"`, etc.) live in the resolver
  (`config::resolve`), applied only after flag/env/profile/global all miss.
- The only environment variables in the chain are the ones that already
  existed: `HF_TOKEN` (for `hf_token`) and `TT_CONFIG_DIR` (for locating the
  config file itself, not a per-setting override). No new per-setting env
  vars were added — the config file is the mechanism for anything beyond
  one-off flags now.
- `name` and `ctrl_port` have **no built-in default** — one of `--name`/
  `[global].name` and `--ctrl-port`/`[global].ctrl_port` must resolve to a
  value or the agent hard-errors at startup.
- `[global]` only supplies global-scoped settings; the active profile only
  supplies serving-scoped settings — there is no cross-scope fallback.

## Active profile selection

1. `--profile <name>` on the CLI, if given.
2. else `default_profile` in the file, if given.
3. else, if the file defines **exactly one** `[profile.*]`, that one is used
   automatically.
4. else the **implicit default profile** — no `[profile.*]` needed at all;
   serving settings come purely from flags/env/`[global]`/built-in defaults
   (today's pre-feature behavior, unchanged).

`ResolvedConfig::active_profile` is `None` in case 4, `Some(name)` otherwise;
`available_profiles` is the sorted list of every `[profile.*]` table name
defined in the file (empty if none).

## `--print-config`

Resolves the full config (flags + file + precedence), prints the redacted
`ConfigSummary` (see below) as pretty JSON to stdout, and **exits 0 without
binding the control port or starting mDNS**. A debugging aid for checking
what a given combination of flags/env/file/`--profile` actually resolves to,
without standing up the daemon:

```
tt-station-agentd --config ~/.config/tt-station/agentd.toml --profile stable --print-config
```

## `GET /config` and `tt config`

`GET /config` is **unauthed** (like `/status`, `/models`, `/serving`) and
returns the same redacted summary as `--print-config`, reflecting whatever
was actually resolved at boot:

```jsonc
{
  "active_profile": "stable",
  "available_profiles": ["bleeding", "stable"],
  "backend": "runpy",
  "serving_host": "qb2-lab.local",
  "serving_port": 8003,
  "serving_image": "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0-80180b9-7678b70",
  "tt_inference_repo": "/home/operator/code/tt-inference-server",
  "tt_device": "p300x2"
}
```

On an agent booted with no config file, this is
`{"active_profile": null, "available_profiles": [], ...}` with the
flag/env/default-resolved serving values.

`ConfigSummary` (`crates/libttstation/src/model.rs`) has **no `hf_token` or
token-store field by construction** — there is nothing for the route to
redact at request time; secrets never make it into the struct in the first
place.

The `tt` CLI exposes the same data:

```
tt config --host <host:port>            # human-readable: active profile, available, backend, serving host:port
tt config --host <host:port> --json     # pretty-printed ConfigSummary
```

`tt config` is unauthed, same as `tt status`/`tt models`/`tt serving` — it
works against a box you've never paired with.

The GTK box panel (`box-panel/tt-station-panel.py`) reads the profile list
directly from the box-local TOML to populate a profile dropdown *before* the
agent is started, passes `--profile <selected>` on Start/Restart, and shows
the active profile from the running agent's own `GET /config` once it's up
(falling back to the dropdown pick while starting). See
`box-panel/agentd.example.toml` for a copy-paste starting file.

## Errors

All of the following are hard errors **at startup, before the control port
is bound** — a misconfigured box fails fast and visibly rather than
half-serving:

| Situation | Behavior |
|---|---|
| No config file at the default path, no `--config` | Not an error — implicit default profile (flags/env/built-in defaults). |
| `--config PATH` given but the file is missing/unreadable | Hard error. |
| Malformed TOML, or an unknown key anywhere in the file | Hard error including the parse message. |
| `--profile X` (or `default_profile = "X"`) names a profile absent from the file | Hard error listing the available profile names. |
| `--profile` given but the file defines no `[profile.*]` at all | Hard error. |
| A serving-scoped key under `[global]`, or a global-scoped key under `[profile.*]` | Unknown-key rejection at parse (same as any other typo). |
| `name`/`ctrl_port` unresolved after flag/`[global]` | Hard error (no built-in default for either). |
| `GET /config` on an agent with no config file | Not an error — returns the implicit-default summary (`active_profile: null`, `available_profiles: []`). |
