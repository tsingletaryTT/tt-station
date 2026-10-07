//! TOML config schema for `agentd.toml`, the loader that turns a path on
//! disk into a parsed [`AgentConfigFile`], and the precedence resolver
//! (explicit CLI flag > active profile / `[global]` > detected value > built-in
//! default) that turns all of that into a flat [`ResolvedConfig`]. `HF_TOKEN` is
//! the sole environment variable consulted, and for `hf_token` it is the lowest
//! layer (flag > profile > env).
//!
//! Smarter defaults: a handful of settings (`name`, `ctrl_port`, `chips`,
//! `serving_host`, `serving_port`, and `backend = "auto"`) are filled in from the
//! machine when nothing else sets them. The detection goes through the
//! [`Detect`] trait, so [`resolve_with`] is testable without hardware, and every
//! resolved value remembers its [`Source`] so `--print-config` can say where it
//! came from.
//!
//! See `docs/superpowers/plans/2026-07-05-agentd-config-profiles.md` for the
//! full design (Task 1 added the parsed-file schema + `load_config`; Task 2
//! adds `expand_tilde`, the moved `default_*` helpers, and `resolve`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde::Deserialize;

use crate::detect::{Detect, NoDetect};

/// The port the control plane listens on when nothing sets one.
pub const DEFAULT_CTRL_PORT: u16 = 8765;
/// The `chips` string used when nothing sets one and detection finds nothing.
pub const FALLBACK_CHIPS: &str = "4xBH";
/// Serving port default for the `tt` backend (tt-model-manager's default).
pub const DEFAULT_TT_SERVING_PORT: u16 = 20000;
/// Serving port default for every other backend.
pub const DEFAULT_SERVING_PORT: u16 = 8000;

/// Which layer supplied a resolved value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A command-line flag.
    Flag,
    /// The active `[profile.*]`.
    Profile,
    /// The `[global]` section.
    Global,
    /// Read from this machine (hostname, LAN address, installed chips).
    Detected,
    /// `backend = "auto"` chose it.
    Auto,
    /// The built-in default.
    Default,
}

impl Source {
    /// The lowercase word `--print-config` and `GET /config` show.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::Profile => "profile",
            Source::Global => "global",
            Source::Detected => "detected",
            Source::Auto => "auto",
            Source::Default => "default",
        }
    }
}

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
    pub gozer_path: Option<String>,
    /// The official `tt` CLI the `tt` backend runs. See `ResolvedConfig::official_tt_bin`.
    pub official_tt_bin: Option<String>,
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
    pub no_token_persistence: bool, // clap SetTrue flag — false = "not set"
    pub telemetry_interval_ms: Option<u64>,
    pub tt_smi_bin: Option<String>,
    pub gozer_path: Option<String>,
    pub official_tt_bin: Option<String>,
    // serving-scoped
    pub backend: Option<String>,
    pub tt_inference_repo: Option<String>,
    pub serving_image: Option<String>,
    pub auto_image: bool, // clap SetTrue flag
    pub tt_device: Option<String>,
    pub serving_host: Option<String>,
    pub serving_port: Option<u16>,
    pub host_hf_cache: Option<String>,
    pub hf_token: Option<String>,
    pub no_device_reset: bool, // clap SetTrue flag
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
    pub token_store: Option<PathBuf>, // None iff persistence disabled
    pub telemetry_interval_ms: u64,
    pub tt_smi_bin: String,
    /// Resolved `gozer` binary path, tilde-expanded. `None` when neither
    /// `--gozer-path` nor `[global].gozer_path` was given -- unlike
    /// `tt_smi_bin`, there's no built-in default STRING here (`"gozer"`
    /// on `$PATH` isn't assumed at this layer); `gozer::probe` does its own
    /// `$PATH` search when this is `None`, so an unset value still resolves
    /// to a working binary if one is on `$PATH`. gozer is optional (see
    /// `gozer`'s module doc), so `None` all the way through to `probe`
    /// failing to find anything is a perfectly normal outcome, not an error.
    pub gozer_path: Option<String>,
    /// The official `tt` CLI for the `tt` backend, tilde-expanded, from
    /// `--official-tt-bin` or `[global].official_tt_bin`. `None` when neither is
    /// given; `main.rs` then searches with `libttstation::official_tt::resolve_bin`.
    /// No default string here, for the same reason as `gozer_path`: the search
    /// reads the environment and this function stays pure.
    pub official_tt_bin: Option<String>,
    // active serving profile
    pub active_profile: Option<String>, // None = implicit default profile
    pub available_profiles: Vec<String>, // sorted; empty when no [profile.*]
    pub backend: String, // RESOLVED: "runpy" | "docker" | "tt" | "dstack" (never "auto")
    /// What was asked for before resolution: the same as `backend`, or `"auto"`.
    pub backend_requested: String,
    /// When `backend_requested` is `"auto"`, one sentence saying what was found.
    pub backend_note: Option<String>,
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
    /// Where each of `name`, `ctrl_port`, `chips`, `backend`, `serving_host`, `serving_port`,
    /// `serving_image`, `tt_inference_repo` and `tt_device` came from. The last two and
    /// `serving_image` appear only when they have a value.
    pub sources: BTreeMap<&'static str, Source>,
}

impl ResolvedConfig {
    /// The redacted summary `--print-config` prints and `GET /config` serves. There is no field
    /// for `hf_token` or the token store, so no secret can reach it. The original keys keep
    /// their names and meaning; `name`, `ctrl_port`, `chips`, `backend_requested`,
    /// `backend_note` and `sources` are additive.
    pub fn summary(&self) -> libttstation::model::ConfigSummary {
        libttstation::model::ConfigSummary {
            active_profile: self.active_profile.clone(),
            available_profiles: self.available_profiles.clone(),
            backend: self.backend.clone(),
            serving_host: self.serving_host.clone(),
            serving_port: self.serving_port,
            serving_image: self.serving_image.clone(),
            tt_inference_repo: Some(self.tt_inference_repo.clone()),
            tt_device: self.tt_device.clone(),
            name: Some(self.name.clone()),
            ctrl_port: Some(self.ctrl_port),
            chips: Some(self.chips.clone()),
            backend_requested: Some(self.backend_requested.clone()),
            backend_note: self.backend_note.clone(),
            sources: self
                .sources
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.as_str().to_string()))
                .collect(),
        }
    }
}

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

/// Resolve the effective config with NO machine detection: nothing is read from the hostname,
/// the network, `tt-smi` or `PATH`. Behaves as the resolver did before smarter defaults, except
/// that `ctrl_port` now defaults to 8765. Production start-up uses [`resolve_with`] and the real
/// probes; this wrapper keeps the pure-precedence tests and any caller that wants no probing
/// hermetic.
pub fn resolve(
    cli: CliOverrides,
    env_hf_token: Option<String>,
    file: Option<AgentConfigFile>,
    requested_profile: Option<&str>,
) -> anyhow::Result<ResolvedConfig> {
    resolve_with(cli, env_hf_token, file, requested_profile, &NoDetect)
}

/// Resolve the effective config by layering, highest-precedence first:
/// explicit CLI flag > active profile (serving-scoped) or `[global]`
/// (global-scoped) > detected value > built-in default. `HF_TOKEN` is the sole
/// environment variable consulted, and for `hf_token` it is the lowest layer
/// (flag > profile > env).
///
/// `requested_profile` is `--profile`; when `None` the active profile is
/// `default_profile`, else the sole profile if exactly one is defined, else
/// the implicit default profile (no `[profile.*]` needed).
///
/// A probe on `detect` runs only when every layer above it is silent, so a box that sets a value
/// by flag, profile or `[global]` resolves exactly as it did before detection existed. The
/// defaults, with the probe behind each:
///
/// | setting | detected from | fallback |
/// |---|---|---|
/// | `name` | short hostname | error |
/// | `ctrl_port` | none | 8765 |
/// | `chips` | `tt-smi -s` mesh, via `device::chips_for_mesh` | `4xBH` |
/// | `serving_host` | primary LAN IPv4, then short hostname | `127.0.0.1` |
/// | `serving_port` | none | 20000 for `tt`, else 8000 |
/// | `backend` | none; `auto` asks for the official `tt` | `runpy` |
///
/// `backend = "auto"` (flag, profile) resolves to `tt` when the official `tt` CLI is found and
/// answers `tt --version` correctly, else to `runpy`. The default when nothing is set stays
/// `runpy`, and `auto` is never the default.
pub fn resolve_with(
    cli: CliOverrides,
    env_hf_token: Option<String>,
    file: Option<AgentConfigFile>,
    requested_profile: Option<&str>,
    detect: &dyn Detect,
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
    // Like `pick` over a flag and one file layer, and also reports which layer answered.
    fn pick_src<T: Clone>(
        flag: &Option<T>,
        file_layer: &Option<T>,
        file_source: Source,
    ) -> Option<(T, Source)> {
        flag.clone()
            .map(|v| (v, Source::Flag))
            .or_else(|| file_layer.clone().map(|v| (v, file_source)))
    }
    let mut sources: BTreeMap<&'static str, Source> = BTreeMap::new();

    // --- identity: name and ctrl_port ---
    let (name, name_src) = match pick_src(&cli.name, &g.name, Source::Global) {
        Some(found) => found,
        None => (
            detect.hostname().ok_or_else(|| {
                anyhow!(
                    "`name` is required: pass --name or set [global].name \
                     (the machine's hostname could not be used as a default)"
                )
            })?,
            Source::Detected,
        ),
    };
    sources.insert("name", name_src);
    let (ctrl_port, ctrl_src) = pick_src(&cli.ctrl_port, &g.ctrl_port, Source::Global)
        .unwrap_or((DEFAULT_CTRL_PORT, Source::Default));
    sources.insert("ctrl_port", ctrl_src);

    // --- global-scoped with built-in defaults ---
    let apiver = pick([&cli.apiver, &g.apiver, &None]).unwrap_or(1);
    let telemetry_interval_ms =
        pick([&cli.telemetry_interval_ms, &g.telemetry_interval_ms, &None]).unwrap_or(1000);
    let tt_smi_bin =
        pick([&cli.tt_smi_bin, &g.tt_smi_bin, &None]).unwrap_or_else(|| "tt-smi".into());
    // `chips` comes after `tt_smi_bin` because detection runs that binary.
    let (chips, chips_src) = match pick_src(&cli.chips, &g.chips, Source::Global) {
        Some(found) => found,
        None => match detect.chips(&tt_smi_bin) {
            Some(detected) => (detected, Source::Detected),
            None => (FALLBACK_CHIPS.to_string(), Source::Default),
        },
    };
    sources.insert("chips", chips_src);
    // No `unwrap_or_else` default string here (unlike `tt_smi_bin`): `None`
    // is a legitimate, common resolved value -- `gozer::probe` treats it as
    // "search $PATH", not "misconfigured". Only expand `~/` when a value was
    // actually given; `expand_tilde` on an absent value would just produce
    // an empty string to expand, which isn't the same as "not configured".
    let gozer_path = pick([&cli.gozer_path, &g.gozer_path, &None]).map(|p| expand_tilde(&p));
    let official_tt_bin =
        pick([&cli.official_tt_bin, &g.official_tt_bin, &None]).map(|p| expand_tilde(&p));

    let no_token_persistence = cli.no_token_persistence || g.no_token_persistence.unwrap_or(false);
    let token_store = if no_token_persistence {
        None
    } else {
        Some(PathBuf::from(expand_tilde(
            &pick([&cli.token_store, &g.token_store, &None]).unwrap_or_else(default_token_store),
        )))
    };

    // --- serving-scoped (profile layer instead of global) ---
    // The backend is resolved first: `auto` needs a probe, and the default port depends on it.
    let (backend_requested, requested_src) =
        pick_src(&cli.backend, &prof.backend, Source::Profile)
            .unwrap_or_else(|| ("runpy".to_string(), Source::Default));
    let (backend, backend_src, backend_note) = if backend_requested == "auto" {
        match detect.official_tt(official_tt_bin.as_deref()) {
            Some(tt) => (
                "tt".to_string(),
                Source::Auto,
                Some(format!(
                    "auto: found official tt {} at {}, so the tt backend is used",
                    tt.version, tt.bin
                )),
            ),
            None => (
                "runpy".to_string(),
                Source::Auto,
                Some(
                    "auto: no valid official tt found (it must answer `--version` with \
                     `tt <semver>`), so the runpy backend is used"
                        .to_string(),
                ),
            ),
        }
    } else {
        (backend_requested.clone(), requested_src, None)
    };
    sources.insert("backend", backend_src);

    let (serving_host, host_src) =
        match pick_src(&cli.serving_host, &prof.serving_host, Source::Profile) {
            Some(found) => found,
            None => match detect.primary_ipv4().or_else(|| detect.hostname()) {
                Some(detected) => (detected, Source::Detected),
                None => ("127.0.0.1".to_string(), Source::Default),
            },
        };
    sources.insert("serving_host", host_src);
    let (serving_port, port_src) = pick_src(&cli.serving_port, &prof.serving_port, Source::Profile)
        .unwrap_or((
            if backend == "tt" {
                DEFAULT_TT_SERVING_PORT
            } else {
                DEFAULT_SERVING_PORT
            },
            Source::Default,
        ));
    sources.insert("serving_port", port_src);

    let serving_image = pick_src(&cli.serving_image, &prof.serving_image, Source::Profile);
    if let Some((_, src)) = &serving_image {
        sources.insert("serving_image", *src);
    }
    let serving_image = serving_image.map(|(v, _)| v);
    let auto_image = cli.auto_image || prof.auto_image.unwrap_or(false);
    let tt_device = pick_src(&cli.tt_device, &prof.tt_device, Source::Profile);
    if let Some((_, src)) = &tt_device {
        sources.insert("tt_device", *src);
    }
    let tt_device = tt_device.map(|(v, _)| v);
    let (tt_inference_repo, repo_src) =
        match pick_src(&cli.tt_inference_repo, &prof.tt_inference_repo, Source::Profile) {
            Some(found) => found,
            None => (default_tt_inference_repo(), Source::Default),
        };
    sources.insert("tt_inference_repo", repo_src);
    let tt_inference_repo = expand_tilde(&tt_inference_repo);
    let host_hf_cache = expand_tilde(
        &pick([&cli.host_hf_cache, &prof.host_hf_cache, &None])
            .unwrap_or_else(default_host_hf_cache),
    );
    let hf_token = pick([&cli.hf_token, &prof.hf_token, &env_hf_token]).filter(|t| !t.is_empty());
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
        name,
        ctrl_port,
        chips,
        apiver,
        token_store,
        telemetry_interval_ms,
        tt_smi_bin,
        gozer_path,
        official_tt_bin,
        active_profile,
        available_profiles,
        backend,
        backend_requested,
        backend_note,
        serving_host,
        serving_port,
        serving_image,
        auto_image,
        tt_device,
        tt_inference_repo,
        host_hf_cache,
        hf_token,
        no_device_reset,
        cache_volume,
        require_auth: cli.require_auth,
        device_path,
        hugepages_src,
        engine: cli.engine,
        impl_name: cli.impl_name,
        device_id: cli.device_id,
        model_source,
        model_spec: cli.model_spec,
        sources,
    })
}

/// Shared empty profile for the implicit-default-profile case, so `resolve`
/// can borrow a `&ProfileSection` without an owned temporary.
static EMPTY_PROFILE: ProfileSection = ProfileSection {
    backend: None,
    tt_inference_repo: None,
    serving_image: None,
    auto_image: None,
    tt_device: None,
    serving_host: None,
    serving_port: None,
    host_hf_cache: None,
    hf_token: None,
    no_device_reset: None,
};

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
        assert!(
            err.contains("bogus") || err.contains("unknown"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(parse("this is not = = toml").is_err());
    }
}

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

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn base_cli() -> CliOverrides {
        // Minimum for a successful resolve: name + ctrl_port present.
        CliOverrides {
            name: Some("box".into()),
            ctrl_port: Some(8765),
            ..Default::default()
        }
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
        assert_eq!(r.serving_port, 8000); // built-in default
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
        assert_eq!(r.serving_port, 8003); // from profile
        assert_eq!(r.chips, "8xBH"); // from global
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
        let err = resolve(base_cli(), None, Some(f), Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("nope") && err.contains("a") && err.contains("b"),
            "got: {err}"
        );
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
    fn official_tt_bin_comes_from_flag_then_global_and_is_otherwise_unset() {
        std::env::set_var("HOME", "/home/tester");
        let r = resolve(base_cli(), None, None, None).unwrap();
        assert_eq!(r.official_tt_bin, None, "unset means: search at startup");

        let f = file_with("[global]\nofficial_tt_bin = \"~/.local/bin/tt\"\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.official_tt_bin.as_deref(), Some("/home/tester/.local/bin/tt"));

        let f = file_with("[global]\nofficial_tt_bin = \"/from/file\"\n");
        let mut cli = base_cli();
        cli.official_tt_bin = Some("/from/flag".into());
        let r = resolve(cli, None, Some(f), None).unwrap();
        assert_eq!(r.official_tt_bin.as_deref(), Some("/from/flag"));
    }

    #[test]
    fn backend_tt_is_accepted_from_a_profile() {
        let f = file_with("[profile.p]\nbackend = \"tt\"\nserving_port = 20000\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.backend, "tt");
        assert_eq!(r.serving_port, 20000);
    }

    #[test]
    fn tilde_expanded_in_resolved_paths() {
        std::env::set_var("HOME", "/home/tester");
        let f = file_with("[profile.p]\nhost_hf_cache=\"~/hf\"\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.host_hf_cache, "/home/tester/hf");
    }
}

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

/// Tests for the smarter defaults. Every detection goes through [`FakeDetect`], so nothing here
/// reads the real hostname, network, `tt-smi` output or `PATH`.
#[cfg(test)]
mod defaults_tests {
    use super::*;
    use crate::detect::{Detect, OfficialTt};
    use std::cell::RefCell;

    /// A scripted [`Detect`] that records which probes ran, so a test can assert that a probe
    /// did NOT run when a higher layer already supplied the value.
    #[derive(Default)]
    struct FakeDetect {
        host: Option<&'static str>,
        ip: Option<&'static str>,
        chips: Option<&'static str>,
        tt: Option<(&'static str, &'static str)>,
        calls: RefCell<Vec<String>>,
    }

    impl Detect for FakeDetect {
        fn hostname(&self) -> Option<String> {
            self.calls.borrow_mut().push("hostname".into());
            self.host.map(String::from)
        }
        fn primary_ipv4(&self) -> Option<String> {
            self.calls.borrow_mut().push("ip".into());
            self.ip.map(String::from)
        }
        fn chips(&self, tt_smi_bin: &str) -> Option<String> {
            self.calls.borrow_mut().push(format!("chips:{tt_smi_bin}"));
            self.chips.map(String::from)
        }
        fn official_tt(&self, explicit_bin: Option<&str>) -> Option<OfficialTt> {
            self.calls
                .borrow_mut()
                .push(format!("tt:{}", explicit_bin.unwrap_or("-")));
            self.tt.map(|(bin, version)| OfficialTt {
                bin: bin.into(),
                version: version.into(),
            })
        }
    }

    impl FakeDetect {
        fn called(&self, what: &str) -> bool {
            self.calls.borrow().iter().any(|c| c.starts_with(what))
        }
    }

    fn file_with(body: &str) -> AgentConfigFile {
        toml::from_str(body).unwrap()
    }

    fn run(
        cli: CliOverrides,
        file: Option<AgentConfigFile>,
        d: &FakeDetect,
    ) -> anyhow::Result<ResolvedConfig> {
        resolve_with(cli, None, file, None, d)
    }

    /// The old, fully explicit invocation: every identity and serving flag given.
    fn old_cli() -> CliOverrides {
        CliOverrides {
            name: Some("box".into()),
            ctrl_port: Some(9000),
            backend: Some("docker".into()),
            serving_host: Some("10.1.1.1".into()),
            serving_port: Some(9100),
            ..Default::default()
        }
    }

    // ---- compatibility pins -------------------------------------------------------------

    #[test]
    fn old_flags_only_invocation_resolves_as_before_without_probing() {
        let d = FakeDetect {
            host: Some("detected-host"),
            ip: Some("192.168.9.9"),
            chips: None, // detection finds nothing: `chips` keeps today's default
            tt: Some(("/x/tt", "1.0.1")),
            ..Default::default()
        };
        let r = run(old_cli(), None, &d).unwrap();
        assert_eq!(r.name, "box");
        assert_eq!(r.ctrl_port, 9000);
        assert_eq!(r.backend, "docker");
        assert_eq!(r.serving_host, "10.1.1.1");
        assert_eq!(r.serving_port, 9100);
        assert_eq!(r.chips, "4xBH");
        assert!(!d.called("hostname"), "name was given");
        assert!(!d.called("ip"), "serving_host was given");
        assert!(!d.called("tt:"), "backend was not auto");
    }

    #[test]
    fn old_flags_with_chips_probe_nothing_at_all() {
        let d = FakeDetect::default();
        let mut cli = old_cli();
        cli.chips = Some("8xBH".into());
        let r = run(cli, None, &d).unwrap();
        assert_eq!(r.chips, "8xBH");
        assert!(d.calls.borrow().is_empty(), "calls: {:?}", d.calls.borrow());
    }

    #[test]
    fn old_profile_driven_config_resolves_as_before_without_probing() {
        let f = file_with(
            r#"
            default_profile = "stable"
            [global]
            name = "qb2-lab"
            ctrl_port = 8765
            chips = "4xBH"
            [profile.stable]
            backend = "runpy"
            serving_host = "qb2-lab.local"
            serving_port = 8003
            "#,
        );
        let d = FakeDetect::default();
        let r = run(CliOverrides::default(), Some(f), &d).unwrap();
        assert_eq!(
            (r.name.as_str(), r.ctrl_port, r.chips.as_str()),
            ("qb2-lab", 8765, "4xBH")
        );
        assert_eq!(
            (r.backend.as_str(), r.serving_host.as_str(), r.serving_port),
            ("runpy", "qb2-lab.local", 8003)
        );
        assert_eq!(r.active_profile.as_deref(), Some("stable"));
        assert!(d.calls.borrow().is_empty(), "calls: {:?}", d.calls.borrow());
    }

    // ---- name -----------------------------------------------------------------------------

    #[test]
    fn name_precedence_flag_global_detected() {
        let f = || file_with("[global]\nname = \"from-global\"\n");
        // `ip` is set so the serving_host default does not fall back to the hostname probe.
        let d = FakeDetect {
            host: Some("from-host"),
            ip: Some("192.168.1.2"),
            ..Default::default()
        };
        let flag = CliOverrides {
            name: Some("from-flag".into()),
            ..Default::default()
        };
        assert_eq!(run(flag, Some(f()), &d).unwrap().name, "from-flag");
        let r = run(CliOverrides::default(), Some(f()), &d).unwrap();
        assert_eq!(r.name, "from-global");
        assert!(!d.called("hostname"));
        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.name, "from-host");
        assert_eq!(r.sources["name"], Source::Detected);
    }

    #[test]
    fn unusable_hostname_keeps_the_name_error() {
        let d = FakeDetect::default();
        let err = run(CliOverrides::default(), None, &d)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`name` is required"), "got: {err}");
        assert!(err.contains("--name"), "got: {err}");
    }

    // ---- ctrl_port ------------------------------------------------------------------------

    #[test]
    fn ctrl_port_precedence_flag_global_default() {
        let d = FakeDetect {
            host: Some("h"),
            ..Default::default()
        };
        let f = || file_with("[global]\nctrl_port = 9001\n");
        let flag = CliOverrides {
            ctrl_port: Some(9002),
            ..Default::default()
        };
        assert_eq!(run(flag, Some(f()), &d).unwrap().ctrl_port, 9002);
        assert_eq!(
            run(CliOverrides::default(), Some(f()), &d)
                .unwrap()
                .ctrl_port,
            9001
        );
        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.ctrl_port, 8765);
        assert_eq!(r.sources["ctrl_port"], Source::Default);
    }

    // ---- chips ----------------------------------------------------------------------------

    #[test]
    fn chips_precedence_flag_global_detected_fallback() {
        let d = FakeDetect {
            host: Some("h"),
            chips: Some("2xBH"),
            ..Default::default()
        };
        let f = || file_with("[global]\nchips = \"8xBH\"\n");
        let flag = CliOverrides {
            chips: Some("1xBH".into()),
            ..Default::default()
        };
        assert_eq!(run(flag, Some(f()), &d).unwrap().chips, "1xBH");
        assert_eq!(
            run(CliOverrides::default(), Some(f()), &d).unwrap().chips,
            "8xBH"
        );
        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.chips, "2xBH");
        assert_eq!(r.sources["chips"], Source::Detected);
        let none = FakeDetect {
            host: Some("h"),
            ..Default::default()
        };
        let r = run(CliOverrides::default(), None, &none).unwrap();
        assert_eq!(r.chips, "4xBH");
        assert_eq!(r.sources["chips"], Source::Default);
    }

    #[test]
    fn chips_detection_uses_the_resolved_tt_smi_bin() {
        let d = FakeDetect {
            host: Some("h"),
            chips: Some("4xBH"),
            ..Default::default()
        };
        let f = file_with("[global]\ntt_smi_bin = \"/opt/tt-smi\"\n");
        run(CliOverrides::default(), Some(f), &d).unwrap();
        assert!(d.called("chips:/opt/tt-smi"), "{:?}", d.calls.borrow());
    }

    // ---- serving_host ---------------------------------------------------------------------

    #[test]
    fn serving_host_precedence_flag_profile_ip_hostname_loopback() {
        let f = || file_with("[profile.p]\nserving_host = \"from-profile\"\n");
        let d = FakeDetect {
            host: Some("short"),
            ip: Some("192.168.1.50"),
            ..Default::default()
        };
        let flag = CliOverrides {
            serving_host: Some("from-flag".into()),
            ..Default::default()
        };
        assert_eq!(run(flag, Some(f()), &d).unwrap().serving_host, "from-flag");
        let r = run(CliOverrides::default(), Some(f()), &d).unwrap();
        assert_eq!(r.serving_host, "from-profile");
        assert_eq!(r.sources["serving_host"], Source::Profile);
        assert!(!d.called("ip"));

        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.serving_host, "192.168.1.50");
        assert_eq!(r.sources["serving_host"], Source::Detected);

        let no_ip = FakeDetect {
            host: Some("short"),
            ..Default::default()
        };
        let r = run(CliOverrides::default(), None, &no_ip).unwrap();
        assert_eq!(r.serving_host, "short");
        assert_eq!(r.sources["serving_host"], Source::Detected);

        // Hostname is only usable through `--name` here, so detection yields nothing.
        let nothing = FakeDetect::default();
        let cli = CliOverrides {
            name: Some("n".into()),
            ..Default::default()
        };
        let r = run(cli, None, &nothing).unwrap();
        assert_eq!(r.serving_host, "127.0.0.1");
        assert_eq!(r.sources["serving_host"], Source::Default);
    }

    // ---- serving_port and backend ---------------------------------------------------------

    #[test]
    fn serving_port_default_depends_on_the_resolved_backend() {
        let d = FakeDetect {
            host: Some("h"),
            ..Default::default()
        };
        let tt = CliOverrides {
            backend: Some("tt".into()),
            ..Default::default()
        };
        assert_eq!(run(tt, None, &d).unwrap().serving_port, 20000);
        for b in ["runpy", "docker", "dstack"] {
            let cli = CliOverrides {
                backend: Some(b.into()),
                ..Default::default()
            };
            assert_eq!(run(cli, None, &d).unwrap().serving_port, 8000, "{b}");
        }
        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.serving_port, 8000, "default backend is runpy");
        assert_eq!(r.sources["serving_port"], Source::Default);
        // An explicit port wins over the backend default.
        let cli = CliOverrides {
            backend: Some("tt".into()),
            serving_port: Some(1234),
            ..Default::default()
        };
        let r = run(cli, None, &d).unwrap();
        assert_eq!(r.serving_port, 1234);
        assert_eq!(r.sources["serving_port"], Source::Flag);
    }

    #[test]
    fn default_backend_stays_runpy_and_never_probes_for_tt() {
        let d = FakeDetect {
            host: Some("h"),
            tt: Some(("/x/tt", "1.0.1")),
            ..Default::default()
        };
        let r = run(CliOverrides::default(), None, &d).unwrap();
        assert_eq!(r.backend, "runpy");
        assert_eq!(r.sources["backend"], Source::Default);
        assert!(!d.called("tt:"));
    }

    #[test]
    fn auto_picks_tt_when_found_and_runpy_when_not() {
        let found = FakeDetect {
            host: Some("h"),
            tt: Some(("/home/u/.local/bin/tt", "1.0.1")),
            ..Default::default()
        };
        let auto = || CliOverrides {
            backend: Some("auto".into()),
            ..Default::default()
        };
        let r = run(auto(), None, &found).unwrap();
        assert_eq!(r.backend, "tt");
        assert_eq!(r.backend_requested, "auto");
        assert_eq!(r.sources["backend"], Source::Auto);
        assert_eq!(r.serving_port, 20000);
        let note = r.backend_note.unwrap();
        assert!(note.contains("1.0.1") && note.contains("/home/u/.local/bin/tt"), "{note}");

        let missing = FakeDetect {
            host: Some("h"),
            ..Default::default()
        };
        let r = run(auto(), None, &missing).unwrap();
        assert_eq!(r.backend, "runpy");
        assert_eq!(r.sources["backend"], Source::Auto);
        assert_eq!(r.serving_port, 8000);
        assert!(r.backend_note.unwrap().contains("no valid official"));
    }

    #[test]
    fn auto_works_from_a_profile_and_the_flag_beats_it() {
        let f = || file_with("[profile.p]\nbackend = \"auto\"\n");
        let found = FakeDetect {
            host: Some("h"),
            tt: Some(("/x/tt", "1.2.3")),
            ..Default::default()
        };
        let r = run(CliOverrides::default(), Some(f()), &found).unwrap();
        assert_eq!((r.backend.as_str(), r.serving_port), ("tt", 20000));
        let flag = CliOverrides {
            backend: Some("runpy".into()),
            ..Default::default()
        };
        let d = FakeDetect {
            host: Some("h"),
            tt: Some(("/x/tt", "1.2.3")),
            ..Default::default()
        };
        let r = run(flag, Some(f()), &d).unwrap();
        assert_eq!(r.backend, "runpy");
        assert_eq!(r.sources["backend"], Source::Flag);
        assert!(!d.called("tt:"));
    }

    #[test]
    fn auto_looks_for_the_configured_official_tt_bin() {
        let d = FakeDetect {
            host: Some("h"),
            ..Default::default()
        };
        let cli = CliOverrides {
            backend: Some("auto".into()),
            official_tt_bin: Some("/custom/tt".into()),
            ..Default::default()
        };
        run(cli, None, &d).unwrap();
        assert!(d.called("tt:/custom/tt"), "{:?}", d.calls.borrow());
    }

    // ---- sources and the summary ----------------------------------------------------------

    #[test]
    fn sources_label_flag_profile_global_detected_auto_default() {
        let f = file_with(
            "[global]\nctrl_port = 9001\n[profile.p]\nserving_host = \"ph\"\nbackend = \"auto\"\n",
        );
        let cli = CliOverrides {
            name: Some("flagged".into()),
            ..Default::default()
        };
        let d = FakeDetect {
            chips: Some("2xBH"),
            tt: Some(("/x/tt", "1.0.0")),
            ..Default::default()
        };
        let r = run(cli, Some(f), &d).unwrap();
        assert_eq!(r.sources["name"], Source::Flag);
        assert_eq!(r.sources["ctrl_port"], Source::Global);
        assert_eq!(r.sources["serving_host"], Source::Profile);
        assert_eq!(r.sources["chips"], Source::Detected);
        assert_eq!(r.sources["backend"], Source::Auto);
        assert_eq!(r.sources["serving_port"], Source::Default);
        assert_eq!(Source::Auto.as_str(), "auto");
        assert_eq!(Source::Detected.as_str(), "detected");
    }

    #[test]
    fn summary_reports_values_and_sources_and_no_secrets() {
        let f = file_with("[profile.p]\nbackend = \"auto\"\nhf_token = \"hf_SECRET123\"\n");
        let d = FakeDetect {
            host: Some("qb2"),
            ip: Some("192.168.1.50"),
            chips: Some("4xBH"),
            tt: Some(("/x/tt", "1.0.1")),
            ..Default::default()
        };
        let r = run(CliOverrides::default(), Some(f), &d).unwrap();
        let s = r.summary();
        assert_eq!(s.backend, "tt");
        assert_eq!(s.backend_requested.as_deref(), Some("auto"));
        assert_eq!(s.name.as_deref(), Some("qb2"));
        assert_eq!(s.ctrl_port, Some(8765));
        assert_eq!(s.chips.as_deref(), Some("4xBH"));
        assert_eq!(s.serving_host, "192.168.1.50");
        assert_eq!(s.serving_port, 20000);
        assert_eq!(s.sources["backend"], "auto");
        assert_eq!(s.sources["serving_host"], "detected");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("hf_SECRET123") && !json.contains("hf_token"), "{json}");
        // The pre-existing keys are all still there under their old names.
        for key in [
            "active_profile",
            "available_profiles",
            "backend",
            "serving_host",
            "serving_port",
            "serving_image",
            "tt_inference_repo",
            "tt_device",
        ] {
            assert!(json.contains(&format!("\"{key}\"")), "missing {key}: {json}");
        }
    }
}
