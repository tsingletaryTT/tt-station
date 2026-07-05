# tt-station-agentd: config file + named profiles — design

**Date:** 2026-07-05
**Status:** Approved (self-approved per owner delegation — "approve your own spec and continue")
**Author:** Claude (brainstormed with Taylor Singletary)

## Goal

Replace the box's long CLI-flag / `TTS_*`-env launch dance with a **human-editable
config file** that supports **named, switchable profiles** — e.g. a `stable` profile
(tt-inference-server 0.14.0 checkout + confirmed image) and a `bleeding` profile
(0.17.0), selectable by name. The agent loads the file at startup; the GTK panel lets
the operator pick the active profile; both the panel (now) and the Mac app (later)
can read the resolved config over an HTTP route.

This is the first of three related "host configurability & observability" specs
(the others: connected-clients tracking, log viewer). It is foundational — the other
two build on how the box is configured.

## Decisions (settled during brainstorming)

1. **Config over flags** + **named profiles** — the two things "which inference server
   to use" actually meant.
2. **Profile = serving config; identity = global.** A profile owns *how the box serves*;
   box identity is shared across all profiles.
   - **Per-profile:** `backend`, `tt_inference_repo`, `serving_image`, `auto_image`,
     `tt_device` (optional → auto-detect), `serving_host`, `serving_port`,
     `host_hf_cache`, `hf_token` (optional, secret), `no_device_reset`.
   - **Global:** `name`, `ctrl_port`, `chips`, `apiver`, `token_store`,
     `no_token_persistence`, `telemetry_interval_ms`, `tt_smi_bin`.
3. **Architectural through-line** (shared by all three specs): new host state is exposed
   as an **agent HTTP route** so the panel and the Mac render the same data. Here that
   is `GET /config`.

## Non-goals (YAGNI / deferred)

- **Runtime hot-swap of the active profile while a model is serving.** Serving image /
  device / repo are launch-time concerns; switching profiles means (re)launching the
  agent. The panel already supervises the agent as a child with Start/Stop/Restart, so
  "switch profile" = restart the child with a different `--profile`. No `POST /profile`
  route in this spec.
- **New backend *types*** (Ollama/TGI/raw-vLLM). The `backend` field still only accepts
  the existing `runpy`/`docker`/`dstack` kinds; a profile just picks among them.
- **Swift/Mac wiring.** The Mac app is being iterated on another machine. We ship the
  `GET /config` route + a `libttstation` model so the Mac *can* consume it later, but we
  touch no Swift here.
- **Writing the config file from the panel.** The file is human-edited. The panel reads
  it and selects a profile via `--profile`; it does not mutate the TOML.

## Config file

**Format:** TOML (adds a `toml` crate dep; `serde` is already in the workspace).
**Location:** `~/.config/tt-station/agentd.toml`, overridable with `--config <path>`.
Respects `TT_CONFIG_DIR` if set (same convention the `tt` CLI already uses), i.e. the
default is `$TT_CONFIG_DIR/agentd.toml` when that env is set, else
`$HOME/.config/tt-station/agentd.toml`.

**Absence is fine.** With no config file (and no explicit `--config`), the agent behaves
**exactly as today**: built-in defaults + whatever flags/env were passed. This is the
"implicit default profile." Existing launch commands and the current panel keep working
unchanged.

### Schema (example)

```toml
# ~/.config/tt-station/agentd.toml
default_profile = "stable"          # optional; see "Active profile selection"

[global]
name = "qb2-lab"                    # optional; flag/default fills gaps
ctrl_port = 8765
chips = "4xBH"
token_store = "~/.config/tt-station/agentd-tokens.json"
telemetry_interval_ms = 1000
tt_smi_bin = "tt-smi"

[profile.stable]
backend = "runpy"
tt_inference_repo = "~/code/tt-inference-server"
serving_image = "ghcr.io/tenstorrent/tt-inference-server/vllm-tt-metal-src-release-ubuntu-22.04-amd64:0.14.0-80180b9-7678b70"
tt_device = "p300x2"               # optional — omit to auto-detect from tt-smi
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

- All fields are optional; a missing field falls through the precedence chain below.
- `~` at the start of any path field expands to `$HOME` (simple prefix expansion — no
  new dependency; documented as the only supported expansion).
- Unknown keys are **rejected** (serde `deny_unknown_fields`) so a typo fails loudly at
  startup rather than being silently ignored.

## Precedence

Highest wins. Resolving each setting:

```
explicit CLI flag  >  environment variable  >  active profile  >  [global]  >  built-in default
```

- **Back-compat requires detecting "was this flag explicitly set?"** Today every
  overridable flag has a clap `default_value`, so we can't tell "user passed 8765" from
  "defaulted to 8765." Fix: **overridable flags become `Option<T>` with no clap default.**
  `None` = "not set on the CLI," and resolution falls through to env → profile → global →
  the hardcoded default (moved out of clap into the resolver). This keeps flags
  authoritative for one-offs while letting the config file drive the common case.
- Env vars: only the ones that already exist stay wired (`HF_TOKEN`, `TT_CONFIG_DIR`).
  We do **not** add a new env var per setting — the config file is the mechanism now.
- `[global]` applies to global-scoped settings only; `[profile.X]` to serving-scoped
  only. A serving key placed in `[global]` (or vice-versa) is an unknown key there and
  is rejected.

## Active profile selection

1. `--profile <name>` on the CLI, if given.
2. else `default_profile` in the file, if given.
3. else, if the file defines **exactly one** profile, that one.
4. else the **implicit default profile** (no `[profile.*]` needed) — pure
   flags/env/global/defaults, i.e. today's behavior.

**Errors (all fail loudly at startup, before binding the port):**
- `--profile X` or `default_profile = "X"` naming a profile absent from the file →
  error listing available profile names.
- `--profile` given but the file defines no profiles → error.
- Malformed TOML / unknown key → error with the parse message.
- `--config PATH` given but the file is missing/unreadable → error (an *explicit* path
  that doesn't exist is a mistake; an absent *default* path is not).

## Components / file structure

### New: `crates/tt-station-agentd/src/config.rs`

The whole feature's testable core, split into parse and resolve so each is unit-testable
in isolation.

```rust
/// Parsed form of agentd.toml (serde, deny_unknown_fields on each struct).
pub struct AgentConfigFile {
    pub default_profile: Option<String>,
    pub global: GlobalSection,                 // all Option fields
    pub profiles: HashMap<String, ProfileSection>,  // #[serde(rename = "profile")]
}
pub struct GlobalSection { /* name, ctrl_port, chips, apiver, token_store,
                              no_token_persistence, telemetry_interval_ms, tt_smi_bin — all Option */ }
pub struct ProfileSection { /* backend, tt_inference_repo, serving_image, auto_image,
                               tt_device, serving_host, serving_port, host_hf_cache,
                               hf_token, no_device_reset — all Option */ }

/// CLI-supplied overrides (every overridable flag as Option<T>).
pub struct CliOverrides { /* mirrors the flags, all Option */ }

/// Fully-resolved, ready-to-use config. No Options for required settings —
/// every field has landed on a concrete value via the precedence chain.
pub struct ResolvedConfig {
    // global
    pub name: String, pub ctrl_port: u16, pub chips: String, pub apiver: String,
    pub token_store: Option<PathBuf>,        // None iff no_token_persistence
    pub telemetry_interval_ms: u64, pub tt_smi_bin: String,
    // serving (active profile)
    pub active_profile: Option<String>,      // None = implicit default profile
    pub backend: String,
    pub runpy: RunPyConfig, pub docker: DockerConfig,  // built from the same fields
    // for GET /config summary
    pub available_profiles: Vec<String>,     // sorted
}

/// Load + parse the file at `path`. Ok(None) if the default path is absent;
/// Err on malformed/unknown-key or an explicit-but-missing path.
pub fn load_config(path: &Path, explicit: bool) -> Result<Option<AgentConfigFile>>;

/// PURE precedence resolution — the primary unit-test target. No I/O.
pub fn resolve(
    cli: CliOverrides,
    env_hf_token: Option<String>,
    file: Option<AgentConfigFile>,
    requested_profile: Option<&str>,   // from --profile
) -> Result<ResolvedConfig>;

/// Expand a leading `~/` to $HOME. The only path expansion we support.
fn expand_tilde(s: &str) -> String;
```

### Modified: `crates/tt-station-agentd/src/main.rs`

- Overridable flags → `Option<T>`, clap defaults removed (defaults move into `resolve`).
- Add flags: `--config <PATH>`, `--profile <NAME>`, `--print-config`.
- Build `CliOverrides` from parsed args, call `load_config` then `resolve`, and
  construct the backend + `AppState` from the `ResolvedConfig` (replacing the current
  inline `DockerConfig`/`RunPyConfig` construction).
- `--print-config`: resolve, print the `ConfigSummary` (redacted — no `hf_token`) as
  pretty JSON to stdout, exit 0 without binding the port. A debugging aid for verifying
  precedence without starting the server.

### Modified: `crates/tt-station-agentd/src/routes.rs`

- `AppState` gains `active_profile: Option<String>` and `available_profiles: Vec<String>`
  (and enough of the resolved serving config to summarize).
- New `GET /config` (unauthed, like `/status` / `/models` / `/serving`): returns
  `ConfigSummary`. **Redacted** — never includes `hf_token` or `token_store` contents.

### Modified: `crates/libttstation/src/model.rs`

```rust
pub struct ConfigSummary {
    pub active_profile: Option<String>,
    pub available_profiles: Vec<String>,
    pub backend: String,
    pub serving_host: String,
    pub serving_port: u16,
    pub serving_image: Option<String>,     // None when auto-image/backend has none
    pub tt_inference_repo: Option<String>,
    pub tt_device: Option<String>,         // None = auto-detected
}
```

### Modified: `crates/libttstation/src/agent_client.rs` + `crates/tt/…`

- `get_config(host) -> Result<ConfigSummary>` free fn (unauthed GET, parallels
  `get_status`).
- `tt config` subcommand (global `--json`), parallels `tt status` — prints active
  profile, available profiles, and the serving summary.

### Modified: `box-panel/tt-station-panel.py`

- Read the profile list directly from the box-local TOML (`TTS_CONFIG` env or the
  default path) so the dropdown is populated **before** the agent is started.
- Add a **profile dropdown**; on Start/Restart pass `--profile <selected>`.
- Show the active profile in the status line (from `GET /config`, or the dropdown).
- Existing `TTS_*` env still works (maps to flags = highest precedence) for boxes with
  no config file.

### Docs

- `docs/reference/agentd-config.md` — the config file reference (schema, precedence,
  profile selection, examples).
- Ship `box-panel/agentd.example.toml` (or `docs/…`) as a copy-paste starting point.
- Update `CLAUDE.md`, `box-panel/README.md`, and `macos/README.md`'s `tt --json`
  contract table (add the `/config` + `tt config` rows).

## Error handling

| Situation | Behavior |
|---|---|
| No config file at default path, no `--config` | Use defaults/flags (implicit default profile). Normal. |
| `--config PATH` missing/unreadable | Hard error at startup. |
| Malformed TOML / unknown key | Hard error with parse message. |
| `--profile X` / `default_profile` not in file | Hard error listing available profiles. |
| `--profile` given, file has no profiles | Hard error. |
| Serving key in `[global]` or vice-versa | Unknown-key rejection at parse. |
| `GET /config` on an agent with no config file | Returns the implicit-default summary (`active_profile: null`, `available_profiles: []`). |

All startup errors happen **before** binding `ctrl_port`, so a misconfigured box fails
fast and visibly rather than half-serving.

## Testing

**Pure unit tests (the bulk — `config.rs`):**
- Precedence: for a representative setting, assert flag > env > profile > global >
  default at each layer (e.g. serving_port set only in `[global]`, only in profile,
  in both, plus a CLI override).
- Active-profile selection: `--profile`; `default_profile`; single-profile
  auto-select; implicit default when no profiles.
- Errors: unknown profile (message lists available), `--profile` with no profiles,
  serving-key-in-global rejection, unknown-key rejection.
- `expand_tilde`: `~/x` → `$HOME/x`; leaves `/abs`, `rel`, and `~user` untouched.
- Back-compat: `resolve` with `file = None` and today's flag values yields a
  `ResolvedConfig` identical to the pre-change construction.

**Parse tests:** valid full file; minimal file (one profile, no `[global]`); file with
only `[global]` and no profiles; malformed → Err.

**Route test (`routes.rs`):** `GET /config` returns the expected `ConfigSummary` and
**omits secrets** (assert no `hf_token`/token material in the serialized body).

**CLI:** extend the mock-box e2e so `tt config --json` round-trips a `ConfigSummary`
(add a `/config` handler to `mock-box`).

## Rollout / migration

Fully backward compatible. Order of adoption on the live box:
1. Land the code (implicit default profile = today's behavior; nothing breaks).
2. Drop an `agentd.toml` with `stable` (current pinned 0.14.0 config) at the default path.
3. Panel gains the dropdown; operator picks `stable`. Add `bleeding` when a 0.17.0-
   compatible checkout exists.
