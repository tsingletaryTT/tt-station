# agentd Config File + Named Profiles — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give `tt-station-agentd` a human-editable TOML config file with named, switchable serving profiles, replacing the long CLI-flag / `TTS_*`-env launch dance while staying fully backward compatible.

**Architecture:** A new `config.rs` module owns parsing (`AgentConfigFile`) and a pure precedence resolver (`resolve`) that layers `CLI flag > env > active profile > [global] > built-in default` into a flat `ResolvedConfig`. `main.rs` turns overridable flags into `Option<T>`, calls `resolve`, and builds the backend + `AppState` from the result. The resolved serving config is exposed (redacted) at a new unauthed `GET /config` route so the GTK panel and (later) the Mac app render the same data.

**Tech Stack:** Rust (clap derive, serde, new `toml` crate, axum), Python/GTK4 (panel), TOML.

## Global Constraints

- **Backward compatible:** with no config file and no `--config`, behavior is byte-identical to today (built-in defaults + flags/env). Existing launch commands and the current panel keep working.
- **Precedence (highest wins):** `explicit CLI flag > environment variable > active profile > [global] > built-in default`.
- **Profile = serving config; identity = global.** Per-profile: `backend`, `tt_inference_repo`, `serving_image`, `auto_image`, `tt_device`, `serving_host`, `serving_port`, `host_hf_cache`, `hf_token`, `no_device_reset`. Global: `name`, `ctrl_port`, `chips`, `apiver`, `token_store`, `no_token_persistence`, `telemetry_interval_ms`, `tt_smi_bin`.
- **Config path:** `$TT_CONFIG_DIR/agentd.toml` if `TT_CONFIG_DIR` set, else `$HOME/.config/tt-station/agentd.toml`; overridable with `--config <PATH>`.
- **Fail loud at startup, before binding `ctrl_port`:** malformed TOML, unknown TOML key (serde `deny_unknown_fields`), unknown/absent `--profile`/`default_profile`, explicit `--config` that is missing/unreadable.
- **Only path expansion supported:** a leading `~/` expands to `$HOME`. Nothing else (`~user`, `$VAR`) is expanded.
- **Secrets never leave the box:** `GET /config` / `ConfigSummary` / `--print-config` never include `hf_token` or token-store contents.
- **YAGNI:** no runtime hot-swap of the active profile (switching = relaunch with a different `--profile`); no new backend *types*; no Swift/Mac changes; the panel never writes the TOML.
- TDD, DRY, frequent commits. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` must pass.

## Shared Type Definitions (authoritative — every task uses these verbatim)

All in `crates/tt-station-agentd/src/config.rs` unless noted.

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use serde::Deserialize;

/// Parsed form of agentd.toml. `deny_unknown_fields` on every struct so a
/// typo fails loudly rather than being silently ignored.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentConfigFile {
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub global: GlobalSection,
    /// TOML `[profile.<name>]` tables. BTreeMap so `available_profiles` is
    /// deterministically sorted with no extra sort step.
    #[serde(default)]
    pub profile: BTreeMap<String, ProfileSection>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GlobalSection {
    pub name: Option<String>,
    pub ctrl_port: Option<u16>,
    pub chips: Option<String>,
    pub apiver: Option<u8>,
    pub token_store: Option<String>,
    pub no_token_persistence: Option<bool>,
    pub telemetry_interval_ms: Option<u64>,
    pub tt_smi_bin: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileSection {
    pub backend: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub serving_image: Option<String>,
    pub auto_image: Option<bool>,
    pub tt_device: Option<String>,
    pub serving_host: Option<String>,
    pub serving_port: Option<u16>,
    pub host_hf_cache: Option<String>,
    pub hf_token: Option<String>,
    pub no_device_reset: Option<bool>,
}

/// Every overridable flag as an Option — `None` means "not set on the CLI",
/// so resolution falls through to env → profile → global → default.
#[derive(Debug, Default)]
pub struct CliOverrides {
    // global-scoped
    pub name: Option<String>,
    pub ctrl_port: Option<u16>,
    pub chips: Option<String>,
    pub apiver: Option<u8>,
    pub token_store: Option<String>,
    pub no_token_persistence: bool,     // clap SetTrue flag — false = "not set"
    pub telemetry_interval_ms: Option<u64>,
    pub tt_smi_bin: Option<String>,
    // serving-scoped
    pub backend: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub serving_image: Option<String>,
    pub auto_image: bool,               // clap SetTrue flag
    pub tt_device: Option<String>,
    pub serving_host: Option<String>,
    pub serving_port: Option<u16>,
    pub host_hf_cache: Option<String>,
    pub hf_token: Option<String>,
    pub no_device_reset: bool,          // clap SetTrue flag
    // runpy-only extras that stay flag/default-driven (not in profiles)
    pub cache_volume: Option<String>,
    pub require_auth: bool,
    pub device_path: Option<String>,
    pub hugepages_src: Option<String>,
    pub engine: Option<String>,
    pub impl_name: Option<String>,
    pub device_id: Option<String>,
    pub model_source: Option<String>,
    pub model_spec: Option<String>,
}

/// Fully-resolved config. Required settings are concrete (no Option).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedConfig {
    // global
    pub name: String,
    pub ctrl_port: u16,
    pub chips: String,
    pub apiver: u8,
    pub token_store: Option<PathBuf>,   // None iff persistence disabled
    pub telemetry_interval_ms: u64,
    pub tt_smi_bin: String,
    // active serving profile
    pub active_profile: Option<String>, // None = implicit default profile
    pub available_profiles: Vec<String>,// sorted; empty when no [profile.*]
    pub backend: String,                // "runpy" | "docker" | "dstack"
    pub serving_host: String,
    pub serving_port: u16,
    pub serving_image: Option<String>,
    pub auto_image: bool,
    pub tt_device: Option<String>,
    pub tt_inference_repo: String,
    pub host_hf_cache: String,
    pub hf_token: Option<String>,
    pub no_device_reset: bool,
    // runpy extras
    pub cache_volume: String,
    pub require_auth: bool,
    pub device_path: String,
    pub hugepages_src: String,
    pub engine: Option<String>,
    pub impl_name: Option<String>,
    pub device_id: Option<String>,
    pub model_source: String,
    pub model_spec: Option<String>,
}
```

Built-in defaults (used by `resolve` when a setting is absent at every layer):
`ctrl_port` has NO default (must come from flag or `[global]`; error if absent — matches today's required flag). `name` likewise required. `chips="4xBH"`, `apiver=1`, `telemetry_interval_ms=1000`, `tt_smi_bin="tt-smi"`, `backend="runpy"`, `serving_host="127.0.0.1"`, `serving_port=8000`, `auto_image=false`, `no_device_reset=false`, `cache_volume="tt-station-cache"`, `require_auth=false`, `device_path="/dev/tenstorrent"`, `hugepages_src="/dev/hugepages-1G"`, `model_source="huggingface"`. `tt_inference_repo`/`host_hf_cache`/`token_store` use the existing `default_*` helpers (moved into `config.rs`).

`ConfigSummary` (in `crates/libttstation/src/model.rs`):

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSummary {
    pub active_profile: Option<String>,
    pub available_profiles: Vec<String>,
    pub backend: String,
    pub serving_host: String,
    pub serving_port: u16,
    pub serving_image: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub tt_device: Option<String>,   // None = auto-detected
}
```

---

### Task 1: `config.rs` — TOML schema + `load_config` + parse tests

**Files:**
- Create: `crates/tt-station-agentd/src/config.rs`
- Modify: `crates/tt-station-agentd/src/lib.rs` (add `pub mod config;`)
- Modify: `crates/tt-station-agentd/Cargo.toml` (add `toml`)
- Modify: `Cargo.toml` (workspace) (add `toml = "0.8"` to `[workspace.dependencies]`)

**Interfaces:**
- Produces: `AgentConfigFile`, `GlobalSection`, `ProfileSection` (see Shared Types); `pub fn load_config(path: &Path, explicit: bool) -> anyhow::Result<Option<AgentConfigFile>>`.

- [ ] **Step 1: Add the `toml` dependency**

In workspace `Cargo.toml` under `[workspace.dependencies]` add:
```toml
toml = "0.8"
```
In `crates/tt-station-agentd/Cargo.toml` under `[dependencies]` add:
```toml
toml = { workspace = true }
```

- [ ] **Step 2: Register the module**

In `crates/tt-station-agentd/src/lib.rs` add alongside the existing `pub mod`s:
```rust
pub mod config;
```

- [ ] **Step 3: Write the failing parse tests**

Create `crates/tt-station-agentd/src/config.rs` with the Shared-Types structs (`AgentConfigFile`, `GlobalSection`, `ProfileSection`) and this test module:
```rust
#[cfg(test)]
mod parse_tests {
    use super::*;

    fn parse(s: &str) -> anyhow::Result<AgentConfigFile> {
        Ok(toml::from_str(s)?)
    }

    #[test]
    fn parses_full_file() {
        let cfg = parse(
            r#"
            default_profile = "stable"
            [global]
            name = "qb2-lab"
            ctrl_port = 8765
            [profile.stable]
            backend = "runpy"
            serving_port = 8003
            [profile.bleeding]
            serving_port = 8004
            "#,
        )
        .unwrap();
        assert_eq!(cfg.default_profile.as_deref(), Some("stable"));
        assert_eq!(cfg.global.name.as_deref(), Some("qb2-lab"));
        assert_eq!(cfg.global.ctrl_port, Some(8765));
        assert_eq!(cfg.profile.len(), 2);
        assert_eq!(cfg.profile["stable"].serving_port, Some(8003));
    }

    #[test]
    fn parses_minimal_single_profile() {
        let cfg = parse("[profile.only]\nbackend = \"docker\"\n").unwrap();
        assert!(cfg.default_profile.is_none());
        assert_eq!(cfg.profile.len(), 1);
        assert_eq!(cfg.global, GlobalSection::default());
    }

    #[test]
    fn parses_global_only_no_profiles() {
        let cfg = parse("[global]\nchips = \"4xBH\"\n").unwrap();
        assert!(cfg.profile.is_empty());
        assert_eq!(cfg.global.chips.as_deref(), Some("4xBH"));
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        let err = parse("wat = 1\n").unwrap_err().to_string();
        assert!(err.contains("wat") || err.contains("unknown"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_profile_key() {
        let err = parse("[profile.x]\nbogus = 1\n").unwrap_err().to_string();
        assert!(err.contains("bogus") || err.contains("unknown"), "got: {err}");
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(parse("this is not = = toml").is_err());
    }
}
```

- [ ] **Step 4: Run tests to verify they fail (no `load_config` yet is fine; these test parsing only)**

Run: `cargo test -p tt-station-agentd --lib config::parse_tests`
Expected: compile error / FAIL until the structs compile with serde.

- [ ] **Step 5: Confirm the structs make the parse tests pass**

The Shared-Types structs already satisfy these tests. Adjust only if a test fails.

Run: `cargo test -p tt-station-agentd --lib config::parse_tests`
Expected: PASS (6 tests).

- [ ] **Step 6: Write the failing `load_config` tests**

Add to `config.rs`:
```rust
#[cfg(test)]
mod load_tests {
    use super::*;
    use std::io::Write;

    fn tmp_toml(body: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn absent_default_path_is_ok_none() {
        let missing = Path::new("/nonexistent/tt-station/agentd.toml");
        let got = load_config(missing, false).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn absent_explicit_path_is_err() {
        let missing = Path::new("/nonexistent/tt-station/agentd.toml");
        assert!(load_config(missing, true).is_err());
    }

    #[test]
    fn present_file_parses() {
        let f = tmp_toml("[profile.p]\nserving_port = 9001\n");
        let got = load_config(f.path(), true).unwrap().unwrap();
        assert_eq!(got.profile["p"].serving_port, Some(9001));
    }

    #[test]
    fn present_but_malformed_is_err() {
        let f = tmp_toml("nope = = =");
        assert!(load_config(f.path(), false).is_err());
    }
}
```
Add `tempfile = "3"` to `crates/tt-station-agentd/Cargo.toml` `[dev-dependencies]` (and `tempfile = "3"` to workspace `[workspace.dependencies]` if not present; reference as `tempfile = { workspace = true }`).

- [ ] **Step 7: Run to verify failure**

Run: `cargo test -p tt-station-agentd --lib config::load_tests`
Expected: FAIL — `load_config` not defined.

- [ ] **Step 8: Implement `load_config`**

```rust
/// Load + parse the config file at `path`.
///
/// - Absent file at a NON-explicit (default) path → `Ok(None)` (the implicit
///   default profile — today's behavior).
/// - Absent/unreadable file at an EXPLICIT `--config` path → `Err` (a path the
///   operator named but that isn't there is a mistake, not "use defaults").
/// - Malformed TOML / unknown key → `Err` with the parse message.
pub fn load_config(path: &Path, explicit: bool) -> anyhow::Result<Option<AgentConfigFile>> {
    use anyhow::Context;
    match std::fs::read_to_string(path) {
        Ok(body) => {
            let cfg: AgentConfigFile = toml::from_str(&body)
                .with_context(|| format!("failed to parse config file {}", path.display()))?;
            Ok(Some(cfg))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read config file {}", path.display()))
        }
    }
}
```

- [ ] **Step 9: Run to verify pass**

Run: `cargo test -p tt-station-agentd --lib config`
Expected: PASS (all parse + load tests).

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml crates/tt-station-agentd/Cargo.toml crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/src/config.rs
git commit -m "feat(agentd): config.rs TOML schema + load_config"
```

---

### Task 2: `expand_tilde`, default helpers, and the pure `resolve` precedence

**Files:**
- Modify: `crates/tt-station-agentd/src/config.rs`

**Interfaces:**
- Consumes: `AgentConfigFile`, `GlobalSection`, `ProfileSection` (Task 1); `CliOverrides`, `ResolvedConfig` (Shared Types).
- Produces:
  - `pub fn expand_tilde(s: &str) -> String`
  - `pub fn default_tt_inference_repo() -> String` / `default_host_hf_cache() -> String` / `default_token_store() -> String` (moved here from `main.rs`)
  - `pub fn resolve(cli: CliOverrides, env_hf_token: Option<String>, file: Option<AgentConfigFile>, requested_profile: Option<&str>) -> anyhow::Result<ResolvedConfig>`

- [ ] **Step 1: Write the failing `expand_tilde` test**

Add to `config.rs`:
```rust
#[cfg(test)]
mod tilde_tests {
    use super::*;
    #[test]
    fn expands_leading_tilde_slash() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde("~/x/y"), "/home/tester/x/y");
    }
    #[test]
    fn leaves_other_forms_untouched() {
        assert_eq!(expand_tilde("/abs"), "/abs");
        assert_eq!(expand_tilde("rel/path"), "rel/path");
        assert_eq!(expand_tilde("~user/x"), "~user/x");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-station-agentd --lib config::tilde_tests`
Expected: FAIL — `expand_tilde` not defined.

- [ ] **Step 3: Implement `expand_tilde` and move the default helpers into `config.rs`**

```rust
/// Expand a leading `~/` to `$HOME`. The ONLY path expansion this codebase
/// supports (documented in the config reference). `~user`, `$VAR`, and a bare
/// `~` are left untouched.
pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}/{rest}")
    } else {
        s.to_string()
    }
}

/// Prefer a vendored `./vendor/tt-inference-server` if present, else
/// `$HOME/code/tt-inference-server`. (Moved verbatim from main.rs.)
pub fn default_tt_inference_repo() -> String {
    let vendored = std::path::Path::new("./vendor/tt-inference-server");
    if vendored.exists() {
        return vendored.to_string_lossy().into_owned();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/code/tt-inference-server")
}

pub fn default_host_hf_cache() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.cache/huggingface")
}

pub fn default_token_store() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.config/tt-station/agentd-tokens.json")
}
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p tt-station-agentd --lib config::tilde_tests`
Expected: PASS.

- [ ] **Step 5: Write the failing `resolve` tests**

Add to `config.rs`:
```rust
#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn base_cli() -> CliOverrides {
        // Minimum for a successful resolve: name + ctrl_port present.
        CliOverrides { name: Some("box".into()), ctrl_port: Some(8765), ..Default::default() }
    }

    fn file_with(body: &str) -> AgentConfigFile {
        toml::from_str(body).unwrap()
    }

    #[test]
    fn implicit_default_profile_when_no_file() {
        let r = resolve(base_cli(), None, None, None).unwrap();
        assert_eq!(r.active_profile, None);
        assert!(r.available_profiles.is_empty());
        assert_eq!(r.backend, "runpy");
        assert_eq!(r.serving_port, 8000);      // built-in default
        assert_eq!(r.chips, "4xBH");
    }

    #[test]
    fn global_supplies_value_below_default() {
        let f = file_with("[global]\nchips = \"8xBH\"\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.chips, "8xBH");
    }

    #[test]
    fn profile_overrides_global_and_default() {
        let f = file_with(
            "default_profile=\"p\"\n[global]\nchips=\"8xBH\"\n[profile.p]\nserving_port=8003\n",
        );
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.active_profile.as_deref(), Some("p"));
        assert_eq!(r.serving_port, 8003);      // from profile
        assert_eq!(r.chips, "8xBH");           // from global
    }

    #[test]
    fn cli_flag_overrides_profile() {
        let f = file_with("default_profile=\"p\"\n[profile.p]\nserving_port=8003\n");
        let mut cli = base_cli();
        cli.serving_port = Some(9999);
        let r = resolve(cli, None, Some(f), None).unwrap();
        assert_eq!(r.serving_port, 9999);
    }

    #[test]
    fn requested_profile_overrides_default_profile() {
        let f = file_with(
            "default_profile=\"a\"\n[profile.a]\nserving_port=1\n[profile.b]\nserving_port=2\n",
        );
        let r = resolve(base_cli(), None, Some(f), Some("b")).unwrap();
        assert_eq!(r.active_profile.as_deref(), Some("b"));
        assert_eq!(r.serving_port, 2);
        assert_eq!(r.available_profiles, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn single_profile_auto_selected() {
        let f = file_with("[profile.only]\nserving_port=7\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.active_profile.as_deref(), Some("only"));
        assert_eq!(r.serving_port, 7);
    }

    #[test]
    fn unknown_requested_profile_errors_listing_available() {
        let f = file_with("[profile.a]\n[profile.b]\n");
        let err = resolve(base_cli(), None, Some(f), Some("nope")).unwrap_err().to_string();
        assert!(err.contains("nope") && err.contains("a") && err.contains("b"), "got: {err}");
    }

    #[test]
    fn requested_profile_with_no_profiles_errors() {
        let f = file_with("[global]\nchips=\"4xBH\"\n");
        assert!(resolve(base_cli(), None, Some(f), Some("x")).is_err());
    }

    #[test]
    fn missing_name_or_ctrl_port_errors() {
        let mut cli = base_cli();
        cli.name = None;
        assert!(resolve(cli, None, None, None).is_err());
    }

    #[test]
    fn env_hf_token_used_when_profile_and_flag_absent() {
        let r = resolve(base_cli(), Some("envtok".into()), None, None).unwrap();
        assert_eq!(r.hf_token.as_deref(), Some("envtok"));
    }

    #[test]
    fn empty_env_hf_token_is_dropped() {
        let r = resolve(base_cli(), Some(String::new()), None, None).unwrap();
        assert_eq!(r.hf_token, None);
    }

    #[test]
    fn no_token_persistence_yields_none_store() {
        let mut cli = base_cli();
        cli.no_token_persistence = true;
        let r = resolve(cli, None, None, None).unwrap();
        assert_eq!(r.token_store, None);
    }

    #[test]
    fn tilde_expanded_in_resolved_paths() {
        std::env::set_var("HOME", "/home/tester");
        let f = file_with("[profile.p]\nhost_hf_cache=\"~/hf\"\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.host_hf_cache, "/home/tester/hf");
    }
}
```

- [ ] **Step 6: Run to verify failure**

Run: `cargo test -p tt-station-agentd --lib config::resolve_tests`
Expected: FAIL — `resolve` not defined.

- [ ] **Step 7: Implement `resolve`**

```rust
use anyhow::{anyhow, bail};

/// Resolve the effective config by layering, highest-precedence first:
/// CLI flag > env > active profile > [global] > built-in default.
///
/// `requested_profile` is `--profile`; when `None` the active profile is
/// `default_profile`, else the sole profile if exactly one is defined, else
/// the implicit default profile (no `[profile.*]` needed).
pub fn resolve(
    cli: CliOverrides,
    env_hf_token: Option<String>,
    file: Option<AgentConfigFile>,
    requested_profile: Option<&str>,
) -> anyhow::Result<ResolvedConfig> {
    let file = file.unwrap_or_default();
    let available_profiles: Vec<String> = file.profile.keys().cloned().collect(); // BTreeMap → sorted

    // --- pick the active profile ---
    let active_profile: Option<String> = match requested_profile {
        Some(name) => {
            if !file.profile.contains_key(name) {
                bail!(
                    "profile {name:?} not found; available profiles: [{}]",
                    available_profiles.join(", ")
                );
            }
            Some(name.to_string())
        }
        None => match &file.default_profile {
            Some(name) => {
                if !file.profile.contains_key(name) {
                    bail!(
                        "default_profile {name:?} not found; available profiles: [{}]",
                        available_profiles.join(", ")
                    );
                }
                Some(name.clone())
            }
            None => match available_profiles.as_slice() {
                [only] => Some(only.clone()),
                _ => None, // zero, or several with no default → implicit default profile
            },
        },
    };
    let prof: &ProfileSection = active_profile
        .as_ref()
        .map(|n| &file.profile[n])
        .unwrap_or(&EMPTY_PROFILE);
    let g = &file.global;

    // helpers: first Some wins
    fn pick<T: Clone>(layers: [&Option<T>; 3]) -> Option<T> {
        layers.iter().find_map(|o| (*o).clone())
    }

    // --- required (no built-in default) ---
    let name = pick([&cli.name, &g.name, &None])
        .ok_or_else(|| anyhow!("`name` is required (pass --name or set [global].name)"))?;
    let ctrl_port = pick([&cli.ctrl_port, &g.ctrl_port, &None]).ok_or_else(|| {
        anyhow!("`ctrl_port` is required (pass --ctrl-port or set [global].ctrl_port)")
    })?;

    // --- global-scoped with built-in defaults ---
    let chips = pick([&cli.chips, &g.chips, &None]).unwrap_or_else(|| "4xBH".into());
    let apiver = pick([&cli.apiver, &g.apiver, &None]).unwrap_or(1);
    let telemetry_interval_ms =
        pick([&cli.telemetry_interval_ms, &g.telemetry_interval_ms, &None]).unwrap_or(1000);
    let tt_smi_bin = pick([&cli.tt_smi_bin, &g.tt_smi_bin, &None]).unwrap_or_else(|| "tt-smi".into());

    let no_token_persistence =
        cli.no_token_persistence || g.no_token_persistence.unwrap_or(false);
    let token_store = if no_token_persistence {
        None
    } else {
        Some(PathBuf::from(expand_tilde(
            &pick([&cli.token_store, &g.token_store, &None]).unwrap_or_else(default_token_store),
        )))
    };

    // --- serving-scoped (profile layer instead of global) ---
    let backend = pick([&cli.backend, &prof.backend, &None]).unwrap_or_else(|| "runpy".into());
    let serving_host =
        pick([&cli.serving_host, &prof.serving_host, &None]).unwrap_or_else(|| "127.0.0.1".into());
    let serving_port = pick([&cli.serving_port, &prof.serving_port, &None]).unwrap_or(8000);
    let serving_image = pick([&cli.serving_image, &prof.serving_image, &None]);
    let auto_image = cli.auto_image || prof.auto_image.unwrap_or(false);
    let tt_device = pick([&cli.tt_device, &prof.tt_device, &None]);
    let tt_inference_repo = expand_tilde(
        &pick([&cli.tt_inference_repo, &prof.tt_inference_repo, &None])
            .unwrap_or_else(default_tt_inference_repo),
    );
    let host_hf_cache = expand_tilde(
        &pick([&cli.host_hf_cache, &prof.host_hf_cache, &None])
            .unwrap_or_else(default_host_hf_cache),
    );
    let hf_token = pick([&cli.hf_token, &prof.hf_token, &env_hf_token])
        .filter(|t| !t.is_empty());
    let no_device_reset = cli.no_device_reset || prof.no_device_reset.unwrap_or(false);

    // --- runpy extras (flag/default only; not part of profiles) ---
    let cache_volume =
        pick([&cli.cache_volume, &None, &None]).unwrap_or_else(|| "tt-station-cache".into());
    let device_path =
        pick([&cli.device_path, &None, &None]).unwrap_or_else(|| "/dev/tenstorrent".into());
    let hugepages_src =
        pick([&cli.hugepages_src, &None, &None]).unwrap_or_else(|| "/dev/hugepages-1G".into());
    let model_source =
        pick([&cli.model_source, &None, &None]).unwrap_or_else(|| "huggingface".into());

    Ok(ResolvedConfig {
        name, ctrl_port, chips, apiver, token_store, telemetry_interval_ms, tt_smi_bin,
        active_profile, available_profiles, backend, serving_host, serving_port,
        serving_image, auto_image, tt_device, tt_inference_repo, host_hf_cache, hf_token,
        no_device_reset, cache_volume, require_auth: cli.require_auth, device_path,
        hugepages_src, engine: cli.engine, impl_name: cli.impl_name, device_id: cli.device_id,
        model_source, model_spec: cli.model_spec,
    })
}

/// Shared empty profile for the implicit-default-profile case, so `resolve`
/// can borrow a `&ProfileSection` without an owned temporary.
static EMPTY_PROFILE: ProfileSection = ProfileSection {
    backend: None, tt_inference_repo: None, serving_image: None, auto_image: None,
    tt_device: None, serving_host: None, serving_port: None, host_hf_cache: None,
    hf_token: None, no_device_reset: None,
};
```

Note: `hf_token` precedence puts `env_hf_token` in the lowest slot so an explicit flag/profile value wins over the env var, matching today's `cli.hf_token.or(env)` behavior.

- [ ] **Step 8: Run to verify pass**

Run: `cargo test -p tt-station-agentd --lib config::resolve_tests`
Expected: PASS (all resolve tests).

- [ ] **Step 9: Clippy + commit**

```bash
cargo clippy -p tt-station-agentd --all-targets -- -D warnings
git add crates/tt-station-agentd/src/config.rs
git commit -m "feat(agentd): expand_tilde + pure resolve precedence"
```

---

### Task 3: Wire `resolve` into `main.rs` (flags → Option, `--config`/`--profile`/`--print-config`)

**Files:**
- Modify: `crates/tt-station-agentd/src/main.rs`

**Interfaces:**
- Consumes: `config::{CliOverrides, ResolvedConfig, load_config, resolve, default_*}` (Tasks 1–2); `ResolvedConfig` fields (Shared Types).
- Produces: an agent whose config comes from `resolve`; `--config`, `--profile`, `--print-config` flags. `ResolvedConfig` is the single source `DockerConfig`/`RunPyConfig`/`AppState` are built from.

- [ ] **Step 1: Turn overridable flags into `Option`, drop clap defaults, add new flags**

In `Cli`: remove `default_value*` from `chips`, `apiver`, `serving_host`, `serving_port`, `cache_volume`, `device_path`, `hugepages_src`, `model_source`, `telemetry_interval_ms`, `tt_smi_bin`, and change each to `Option<T>`. Change `backend` from `Backend` to `Option<Backend>` (drop `default_value_t`). Make `name`/`ctrl_port` `Option` too (resolve enforces required-ness). Keep `auto_image`, `require_auth`, `no_device_reset`, `no_token_persistence` as `bool` SetTrue flags. Keep the already-`Option` fields as-is. Keep the `telemetry_interval_ms` range validator by moving it onto the `Option<u64>`: `#[arg(long = "telemetry-interval-ms", value_parser = clap::value_parser!(u64).range(1..))] telemetry_interval_ms: Option<u64>`.

Add:
```rust
    /// Path to agentd.toml. Defaults to `$TT_CONFIG_DIR/agentd.toml` if
    /// `TT_CONFIG_DIR` is set, else `$HOME/.config/tt-station/agentd.toml`.
    /// An explicit path that is missing/unreadable is a hard error.
    #[arg(long)]
    config: Option<String>,

    /// Name of the `[profile.<name>]` in the config file to activate.
    /// Overrides `default_profile`. Errors if the named profile is absent.
    #[arg(long)]
    profile: Option<String>,

    /// Resolve the config, print it (secrets redacted) as JSON, and exit
    /// without binding the control port. For verifying precedence/profiles.
    #[arg(long = "print-config", action = clap::ArgAction::SetTrue)]
    print_config: bool,
```

- [ ] **Step 2: Build `CliOverrides`, resolve, and build configs from `ResolvedConfig`**

Replace the top of `main()` (the `hf_token`/`docker_config`/`runpy_config`/backend construction through `AppState` creation) with:
```rust
    let cli = Cli::parse();

    // Config file path: explicit --config wins; else $TT_CONFIG_DIR/agentd.toml
    // if set; else $HOME/.config/tt-station/agentd.toml.
    let explicit = cli.config.is_some();
    let config_path = cli.config.clone().map(std::path::PathBuf::from).unwrap_or_else(|| {
        let dir = std::env::var("TT_CONFIG_DIR")
            .unwrap_or_else(|_| format!("{}/.config/tt-station", std::env::var("HOME").unwrap_or_default()));
        std::path::PathBuf::from(dir).join("agentd.toml")
    });
    let file = config::load_config(&config_path, explicit).context("failed to load config file")?;

    let env_hf_token = std::env::var("HF_TOKEN").ok();
    let overrides = config::CliOverrides {
        name: cli.name.clone(),
        ctrl_port: cli.ctrl_port,
        chips: cli.chips.clone(),
        apiver: cli.apiver,
        token_store: cli.token_store.clone(),
        no_token_persistence: cli.no_token_persistence,
        telemetry_interval_ms: cli.telemetry_interval_ms,
        tt_smi_bin: cli.tt_smi_bin.clone(),
        backend: cli.backend.map(|b| b.to_string()),
        tt_inference_repo: cli.tt_inference_repo.clone(),
        serving_image: cli.serving_image.clone(),
        auto_image: cli.auto_image,
        tt_device: cli.tt_device.clone(),
        serving_host: cli.serving_host.clone(),
        serving_port: cli.serving_port,
        host_hf_cache: cli.host_hf_cache.clone(),
        hf_token: cli.hf_token.clone(),
        no_device_reset: cli.no_device_reset,
        cache_volume: cli.cache_volume.clone(),
        require_auth: cli.require_auth,
        device_path: cli.device_path.clone(),
        hugepages_src: cli.hugepages_src.clone(),
        engine: cli.engine.clone(),
        impl_name: cli.impl_name.clone(),
        device_id: cli.device_id.clone(),
        model_source: cli.model_source.clone(),
        model_spec: cli.model_spec.clone(),
    };
    let rc = config::resolve(overrides, env_hf_token, file, cli.profile.as_deref())
        .context("failed to resolve configuration")?;

    if cli.print_config {
        let summary = config_summary(&rc);           // see Step 3
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }

    let docker_config = DockerConfig {
        image: rc.serving_image.clone().unwrap_or_else(|| DEFAULT_DOCKER_SERVING_IMAGE.to_string()),
        host: rc.serving_host.clone(),
        host_port: rc.serving_port,
        tt_device: rc.tt_device.clone().unwrap_or_else(|| DEFAULT_DOCKER_TT_DEVICE.to_string()),
        hf_token: rc.hf_token.clone(),
        cache_volume: rc.cache_volume.clone(),
        no_auth: !rc.require_auth,
        device_path: rc.device_path.clone(),
        hugepages_src: rc.hugepages_src.clone(),
    };
    let runpy_config = RunPyConfig {
        repo_dir: rc.tt_inference_repo.clone(),
        host: rc.serving_host.clone(),
        service_port: rc.serving_port,
        no_auth: !rc.require_auth,
        model_source: rc.model_source.clone(),
        host_hf_cache: Some(rc.host_hf_cache.clone()),
        tt_device: rc.tt_device.clone(),
        image: rc.serving_image.clone(),
        auto_image: rc.auto_image,
        engine: rc.engine.clone(),
        impl_name: rc.impl_name.clone(),
        device_id: rc.device_id.clone(),
        model_spec_path: rc.model_spec.clone(),
        reset_before_serve: !rc.no_device_reset,
        reset_cmd: vec!["tt-smi".to_string(), "-r".to_string()],
    };

    let backend = make_backend(&rc.backend, docker_config, runpy_config)
        .context("failed to construct serving backend")?;
    let backend: Arc<dyn tt_station_agentd::serving::ServingBackend> = Arc::from(backend);

    let state = match &rc.token_store {
        None => AppState::new(rc.name.clone(), rc.chips.clone(), backend),
        Some(path) => {
            println!("tt-station-agentd: persisting bearer tokens to {}", path.display());
            AppState::new_persisting(rc.name.clone(), rc.chips.clone(), backend, path.clone())
        }
    };
    let state = state.with_telemetry_config(rc.tt_smi_bin.clone(), rc.telemetry_interval_ms);
    let state = state.with_serving_config(rc.serving_host.clone(), rc.serving_port);
    let state = state.with_config_summary(config_summary(&rc));   // Task 5 builder
```

- [ ] **Step 3: Add a `config_summary` helper and delete the moved default helpers**

Delete `default_tt_inference_repo`/`default_host_hf_cache`/`default_token_store` from `main.rs` (now in `config.rs`). Add:
```rust
/// Build the redacted `ConfigSummary` (Task 4 type) from a `ResolvedConfig`.
/// NEVER includes `hf_token` or token-store contents.
fn config_summary(rc: &config::ResolvedConfig) -> libttstation::model::ConfigSummary {
    libttstation::model::ConfigSummary {
        active_profile: rc.active_profile.clone(),
        available_profiles: rc.available_profiles.clone(),
        backend: rc.backend.clone(),
        serving_host: rc.serving_host.clone(),
        serving_port: rc.serving_port,
        serving_image: rc.serving_image.clone(),
        tt_inference_repo: Some(rc.tt_inference_repo.clone()),
        tt_device: rc.tt_device.clone(),
    }
}
```
Update `advertise()` and `MdnsStatusAdvertiser` construction to take values from `rc`/explicit args instead of `cli` (they read `cli.name`, `cli.ctrl_port`, `cli.chips`, `cli.apiver` today). Change `advertise(&cli, ...)` to `advertise(&rc, ...)` and adjust the signature to `fn advertise(rc: &config::ResolvedConfig, status: ServingStatus)`; read `rc.name`/`rc.ctrl_port`/`rc.chips`/`rc.apiver`. Update the final `println!`/bind to use `rc.ctrl_port`, `rc.name`, `rc.backend`, `rc.chips`.

> This step depends on Task 4 (`ConfigSummary`) and Task 5 (`with_config_summary`). If executing strictly in order, implement Task 4 and Task 5 first, or stub `with_config_summary`/`ConfigSummary` minimally to compile and let Task 4/5 fill them in. Recommended execution order: **Task 1 → 2 → 4 → 5 → 3 → 6 → 7 → 8** (see note at end). The plan lists Task 3 here for narrative locality.

- [ ] **Step 4: Build + smoke-test `--print-config` and back-compat**

```bash
cargo build -p tt-station-agentd
# Back-compat: flags only, no config file
./target/debug/tt-station-agentd --name box --ctrl-port 8799 --print-config
```
Expected: JSON with `"active_profile": null`, `"available_profiles": []`, `"backend": "runpy"`, `"serving_port": 8000`, and NO `hf_token` field.

```bash
# Profile selection from a temp config
printf 'default_profile="stable"\n[profile.stable]\nserving_port=8003\n[profile.bleeding]\nserving_port=8004\n' > /tmp/agentd.toml
./target/debug/tt-station-agentd --name box --ctrl-port 8799 --config /tmp/agentd.toml --print-config
./target/debug/tt-station-agentd --name box --ctrl-port 8799 --config /tmp/agentd.toml --profile bleeding --print-config
```
Expected: first shows `active_profile: "stable"`, `serving_port: 8003`; second `bleeding`/`8004`.

- [ ] **Step 5: Run the workspace tests + clippy**

Run: `cargo test -p tt-station-agentd && cargo clippy -p tt-station-agentd --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/tt-station-agentd/src/main.rs
git commit -m "feat(agentd): resolve config via file+profiles in main; --config/--profile/--print-config"
```

---

### Task 4: `ConfigSummary` model + `get_config` client fn

**Files:**
- Modify: `crates/libttstation/src/model.rs`
- Modify: `crates/libttstation/src/agent_client.rs`

**Interfaces:**
- Produces: `libttstation::model::ConfigSummary` (Shared Types); `libttstation::agent_client::get_config(host: &str) -> anyhow::Result<ConfigSummary>` (unauthed GET, mirrors `get_status`).

- [ ] **Step 1: Write the failing serde round-trip test**

In `crates/libttstation/src/model.rs` tests:
```rust
#[test]
fn config_summary_round_trips_and_omits_secrets() {
    let s = ConfigSummary {
        active_profile: Some("stable".into()),
        available_profiles: vec!["stable".into(), "bleeding".into()],
        backend: "runpy".into(),
        serving_host: "qb2-lab.local".into(),
        serving_port: 8003,
        serving_image: Some("img:0.14.0".into()),
        tt_inference_repo: Some("/home/x/code/tt-inference-server".into()),
        tt_device: None,
    };
    let json = serde_json::to_string(&s).unwrap();
    assert!(!json.contains("hf_token"), "summary must not carry secrets");
    let back: ConfigSummary = serde_json::from_str(&json).unwrap();
    assert_eq!(s, back);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p libttstation config_summary`
Expected: FAIL — `ConfigSummary` not defined.

- [ ] **Step 3: Add the `ConfigSummary` struct**

Add the Shared-Types `ConfigSummary` to `model.rs` (with `use serde::{Serialize, Deserialize};` already present in that file).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p libttstation config_summary`
Expected: PASS.

- [ ] **Step 5: Add `get_config` free fn (follow `get_status`'s pattern exactly)**

Inspect the existing `get_status` in `crates/libttstation/src/agent_client.rs` and add a sibling:
```rust
/// GET /config (unauthed) — the box's resolved, redacted serving config.
pub fn get_config(host: &str) -> anyhow::Result<crate::model::ConfigSummary> {
    let url = format!("http://{host}/config");
    let resp = ureq_get(&url)?;              // use whatever helper get_status uses
    Ok(resp.into_json()?)                    // match get_status's deserialization style
}
```
Match the actual HTTP client and error handling `get_status` uses (the snippet above is a template — mirror the real one).

- [ ] **Step 6: Build + commit**

```bash
cargo build -p libttstation && cargo test -p libttstation
git add crates/libttstation/src/model.rs crates/libttstation/src/agent_client.rs
git commit -m "feat(lib): ConfigSummary model + get_config client fn"
```

---

### Task 5: `GET /config` route + `AppState` config fields

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs`

**Interfaces:**
- Consumes: `libttstation::model::ConfigSummary` (Task 4).
- Produces: `AppState::with_config_summary(self, summary: ConfigSummary) -> Self` (Arc::get_mut builder, same pattern as `with_telemetry_config`/`with_serving_config`); `GET /config` route returning the stored `ConfigSummary` as JSON, unauthed.

- [ ] **Step 1: Write the failing route test**

In `routes.rs` tests (follow the existing `/status` or `/serving` handler test for `app(state)` + `tower::ServiceExt::oneshot`):
```rust
#[tokio::test]
async fn get_config_returns_summary_without_secrets() {
    let summary = libttstation::model::ConfigSummary {
        active_profile: Some("stable".into()),
        available_profiles: vec!["stable".into()],
        backend: "runpy".into(),
        serving_host: "qb2-lab.local".into(),
        serving_port: 8003,
        serving_image: Some("img:0.14.0".into()),
        tt_inference_repo: Some("/home/x/tt-inference-server".into()),
        tt_device: None,
    };
    let state = test_state().with_config_summary(summary.clone()); // test_state(): existing helper
    let app = app(state);
    let resp = app
        .oneshot(Request::builder().uri("/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let got: libttstation::model::ConfigSummary = serde_json::from_slice(&body).unwrap();
    assert_eq!(got, summary);
    assert!(!String::from_utf8_lossy(&body).contains("hf_token"));
}
```
(Use whatever request/body helpers the other route tests in this file already use.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p tt-station-agentd --lib routes::tests::get_config_returns_summary_without_secrets`
Expected: FAIL — `with_config_summary` / `/config` not defined.

- [ ] **Step 3: Add the `config_summary` field to `Inner`, the builder, and the route**

In `Inner` add `config_summary: ConfigSummary` (default via a sensible empty summary in `new_inner`: `active_profile: None, available_profiles: vec![], backend: "runpy".into(), serving_host: "127.0.0.1".into(), serving_port: 8000, serving_image: None, tt_inference_repo: None, tt_device: None`). Add the builder (mirror `with_serving_config` exactly — `Arc::get_mut`):
```rust
pub fn with_config_summary(mut self, summary: libttstation::model::ConfigSummary) -> Self {
    Arc::get_mut(&mut self.inner)
        .expect("with_config_summary must be called before AppState is cloned")
        .config_summary = summary;
    self
}
fn config_summary(&self) -> libttstation::model::ConfigSummary {
    self.inner.config_summary.clone()
}
```
Add the handler and register it in `app()` next to `/status`:
```rust
async fn get_config(State(state): State<AppState>) -> Json<libttstation::model::ConfigSummary> {
    Json(state.config_summary())
}
// in app(): .route("/config", get(get_config))
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p tt-station-agentd --lib routes`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy -p tt-station-agentd --all-targets -- -D warnings
git add crates/tt-station-agentd/src/routes.rs
git commit -m "feat(agentd): GET /config route + AppState config summary"
```

---

### Task 6: `tt config` CLI subcommand + mock-box `/config` + e2e

**Files:**
- Modify: `crates/tt/src/main.rs` (or the CLI's command module — match its structure)
- Modify: `crates/mock-box/src/*` (add a `/config` handler to the fake control API)
- Modify/Test: `crates/tt/tests/e2e_mock.rs`

**Interfaces:**
- Consumes: `libttstation::agent_client::get_config` (Task 4); `ConfigSummary` (Task 4).
- Produces: `tt config` (respects global `--json`), mock-box `/config` returning a `ConfigSummary`.

- [ ] **Step 1: Add a `/config` handler to mock-box**

Find mock-box's control-API router (it already fakes `/status`, `/models`, `/serving`). Add a `/config` route returning a fixed `ConfigSummary` JSON, e.g. `active_profile: Some("mock"), available_profiles: ["mock"], backend: "runpy", serving_host: "127.0.0.1", serving_port: 8000, serving_image: None, tt_inference_repo: None, tt_device: None`.

- [ ] **Step 2: Add the `tt config` subcommand**

Mirror the existing `tt status` subcommand. Human output: print active profile, available profiles, backend, and `host:port`. With global `--json`: print `serde_json::to_string_pretty(&summary)`.
```rust
// subcommand enum:
/// Show the box's resolved serving config (active/available profiles, backend, endpoint).
Config,
// dispatch:
Command::Config => {
    let summary = libttstation::agent_client::get_config(&host)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!("active profile: {}", summary.active_profile.as_deref().unwrap_or("(implicit default)"));
        println!("available:      {}", if summary.available_profiles.is_empty() { "(none)".into() } else { summary.available_profiles.join(", ") });
        println!("backend:        {}", summary.backend);
        println!("serving:        {}:{}", summary.serving_host, summary.serving_port);
    }
}
```
Match the crate's actual host-resolution and `--json` plumbing.

- [ ] **Step 3: Write the failing e2e test**

Add to `crates/tt/tests/e2e_mock.rs` (mirror the existing `tt status`/`tt serving` e2e that spins up mock-box):
```rust
#[test]
#[ignore] // hardware-free but network/process — run with --ignored like the others
fn tt_config_json_round_trips_from_mock_box() {
    // start mock-box `serve`, then run `tt config --json --host 127.0.0.1:<port>`
    // assert the JSON parses to ConfigSummary and active_profile == "mock".
}
```
Fill in using the exact harness the other e2e tests in this file use.

- [ ] **Step 4: Run to verify failure, implement, verify pass**

Run: `cargo test -p tt --test e2e_mock -- --ignored tt_config_json_round_trips_from_mock_box`
Expected: FAIL → implement Steps 1–2 → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/tt crates/mock-box
git commit -m "feat(tt): tt config subcommand + mock-box /config + e2e"
```

---

### Task 7: GTK panel — profile dropdown + read TOML

**Files:**
- Modify: `box-panel/tt-station-panel.py`
- Modify: `box-panel/README.md`

**Interfaces:**
- Consumes: the box-local `agentd.toml` (profile names) and `GET /config` (active profile).
- Produces: a profile dropdown that passes `--profile <name>` when starting the agent; active-profile shown in the status line.

- [ ] **Step 1: Read profile names from the TOML for the dropdown**

Add a helper that reads `TTS_CONFIG` (or the default `$TT_CONFIG_DIR/agentd.toml` / `~/.config/tt-station/agentd.toml`) using Python's `tomllib` (stdlib 3.11+) and returns the sorted `[profile.*]` keys plus `default_profile`. On any error (missing file, parse error), return `([], None)` so the panel still works with no config file.
```python
import tomllib, os
def read_profiles():
    path = os.environ.get("TTS_CONFIG") or os.path.join(
        os.environ.get("TT_CONFIG_DIR", os.path.expanduser("~/.config/tt-station")),
        "agentd.toml")
    try:
        with open(path, "rb") as f:
            data = tomllib.load(f)
        return sorted(data.get("profile", {}).keys()), data.get("default_profile")
    except Exception:
        return [], None
```

- [ ] **Step 2: Add the dropdown and pass `--profile` on Start/Restart**

Add a `Gtk.DropDown` (or `Gtk.ComboBoxText`) populated from `read_profiles()`, defaulting to `default_profile` (or the first entry). When building the agent argv in the Start/Restart handler, append `--profile <selected>` only when a profile is selected (dropdown non-empty). Hide/disable the dropdown when `read_profiles()` returns `[]`.

- [ ] **Step 3: Show the active profile in the status line**

When polling `GET /config` (add the poll alongside the existing status poll), display `active profile: <name>` (or `(implicit default)`), so what's actually running is visible even if the dropdown changed since Start.

- [ ] **Step 4: Manual verification (panel is UI, not unit-tested — like the app's LaunchController)**

```bash
printf 'default_profile="stable"\n[profile.stable]\nserving_port=8003\n[profile.bleeding]\nserving_port=8004\n' > ~/.config/tt-station/agentd.toml
python3 box-panel/tt-station-panel.py
```
Expected: dropdown shows `stable`, `bleeding`, defaults to `stable`; Start launches the agent with `--profile stable`; status line shows the active profile. Remove the temp file after (or keep as the real config).

- [ ] **Step 5: Update the panel README + commit**

Document the dropdown and `TTS_CONFIG` in `box-panel/README.md`. Then:
```bash
git add box-panel/tt-station-panel.py box-panel/README.md
git commit -m "feat(panel): profile dropdown reading agentd.toml; show active profile"
```

---

### Task 8: Docs — config reference, example TOML, project docs

**Files:**
- Create: `docs/reference/agentd-config.md`
- Create: `box-panel/agentd.example.toml`
- Modify: `CLAUDE.md`, `macos/README.md`

**Interfaces:** none (docs only).

- [ ] **Step 1: Write `docs/reference/agentd-config.md`**

Document: file location + `--config`, the full schema (every `[global]` + `[profile.*]` key with type and meaning), the precedence chain (verbatim from Global Constraints), profile selection rules, `~/` expansion, `--print-config`, `GET /config`/`tt config`, and the error table. Use the spec's example TOML.

- [ ] **Step 2: Ship `box-panel/agentd.example.toml`**

A copy-paste starting point with a `stable` profile matching this box's confirmed config (0.14.0 image, `qb2-lab.local:8003`, repo `~/code/tt-inference-server`) and a commented-out `bleeding` example.

- [ ] **Step 3: Update `CLAUDE.md` and `macos/README.md`**

In `CLAUDE.md` "Current state": note the config file + profiles, `--config`/`--profile`/`--print-config`, and `GET /config` (unauthed) / `tt config`. In `macos/README.md`'s `tt --json` contract table add a `config` row (`tt config --json` → `ConfigSummary`).

- [ ] **Step 4: Commit**

```bash
git add docs/reference/agentd-config.md box-panel/agentd.example.toml CLAUDE.md macos/README.md
git commit -m "docs: agentd config file + profiles reference and project-doc updates"
```

---

## Recommended execution order

Because Task 3 (main wiring) references `ConfigSummary` (Task 4) and `with_config_summary` (Task 5), execute in dependency order: **1 → 2 → 4 → 5 → 3 → 6 → 7 → 8**. Tasks 1, 2 are the pure core; 4, 5 add the model + route; 3 wires everything through `main`; 6 exposes it via the CLI; 7 the panel; 8 docs.

## Self-review notes

- **Spec coverage:** config file (T1), precedence resolver (T2), profile selection + errors (T2), `--config`/`--profile`/`--print-config` (T3), back-compat implicit-default profile (T2/T3), `ConfigSummary` + secret redaction (T4/T5), `GET /config` (T5), `tt config` (T6), panel dropdown (T7), docs + example TOML (T8). All spec sections map to a task.
- **Back-compat** is enforced by `resolve`'s built-in defaults matching today's clap defaults exactly (see Shared Types default list) and verified by `implicit_default_profile_when_no_file` (T2) + the `--print-config` back-compat smoke (T3) + the existing e2e suite staying green.
- **Secrets:** `ConfigSummary` has no `hf_token` field by construction; asserted in T4 and T5.
