//! TOML config schema for `agentd.toml`, the loader that turns a path on
//! disk into a parsed [`AgentConfigFile`], and the pure precedence resolver
//! (explicit CLI flag > active profile / `[global]` > built-in defaults) that
//! turns all of that into a flat [`ResolvedConfig`]. `HF_TOKEN` is the sole
//! environment variable consulted, and for `hf_token` it is the lowest layer
//! (flag > profile > env).
//!
//! See `docs/superpowers/plans/2026-07-05-agentd-config-profiles.md` for the
//! full design (Task 1 added the parsed-file schema + `load_config`; Task 2
//! adds `expand_tilde`, the moved `default_*` helpers, and `resolve`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::bail;
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
    pub gozer_path: Option<String>,
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
    /// `runpy` backend launcher: `"runpy"` (python3 run.py) or `"tt"` (the
    /// official CLI's `tt serve`). Same as `--serve-with`.
    pub serve_with: Option<String>,
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
    // serving-scoped
    pub serve_with: Option<String>,
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
    /// `--chips`/`[global].chips` when the operator set one; `None` means
    /// "use what `tt-smi -s` finds at startup".
    pub chips: Option<String>,
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
    // active serving profile
    pub active_profile: Option<String>, // None = implicit default profile
    pub available_profiles: Vec<String>, // sorted; empty when no [profile.*]
    pub backend: String,                // "runpy" | "docker"
    pub serve_with: String,             // "runpy" | "tt" (runpy backend's launcher)
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

/// The control-plane port when neither `--ctrl-port` nor `[global].ctrl_port`
/// says otherwise (what the apps and docs assume).
pub const DEFAULT_CTRL_PORT: u16 = 8765;

/// Turn a hostname into a box name: the short form (up to the first `.`),
/// lowercased, with anything but ASCII letters, digits and `-` replaced by
/// `-` (an mDNS label and a `<name>.local` host both stay valid). Falls back
/// to `tt-station` when nothing usable is left.
pub fn sanitize_box_name(hostname: &str) -> String {
    let short = hostname.trim().split('.').next().unwrap_or_default();
    let mut out = String::new();
    for c in short.chars() {
        let c = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    let out: String = out.trim_matches('-').chars().take(63).collect();
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "tt-station".into()
    } else {
        out.to_string()
    }
}

pub fn default_host_hf_cache() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.cache/huggingface")
}

pub fn default_token_store() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.config/tt-station/agentd-tokens.json")
}

/// Resolve the effective config by layering, highest-precedence first:
/// explicit CLI flag > active profile (serving-scoped) or `[global]`
/// (global-scoped) > built-in default. `HF_TOKEN` is the sole environment
/// variable consulted, and for `hf_token` it is the lowest layer
/// (flag > profile > env).
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

    // --- identity: defaults so the packaged unit starts with no config ---
    // The .deb's systemd unit runs the binary bare, so `name` falls back to
    // this machine's hostname and `ctrl_port` to the documented 8765.
    let name = pick([&cli.name, &g.name, &None])
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| {
            sanitize_box_name(
                &std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default(),
            )
        });
    let ctrl_port = pick([&cli.ctrl_port, &g.ctrl_port, &None]).unwrap_or(DEFAULT_CTRL_PORT);

    // --- global-scoped with built-in defaults ---
    // An explicit override only: when absent, `main` derives the label from
    // the startup `tt-smi -s` scan (`device::chips_label`).
    let chips = pick([&cli.chips, &g.chips, &None]).filter(|c| !c.trim().is_empty());
    let apiver = pick([&cli.apiver, &g.apiver, &None]).unwrap_or(1);
    let telemetry_interval_ms =
        pick([&cli.telemetry_interval_ms, &g.telemetry_interval_ms, &None]).unwrap_or(1000);
    let tt_smi_bin =
        pick([&cli.tt_smi_bin, &g.tt_smi_bin, &None]).unwrap_or_else(|| "tt-smi".into());
    // No `unwrap_or_else` default string here (unlike `tt_smi_bin`): `None`
    // is a legitimate, common resolved value -- `gozer::probe` treats it as
    // "search $PATH", not "misconfigured". Only expand `~/` when a value was
    // actually given; `expand_tilde` on an absent value would just produce
    // an empty string to expand, which isn't the same as "not configured".
    let gozer_path = pick([&cli.gozer_path, &g.gozer_path, &None]).map(|p| expand_tilde(&p));

    let no_token_persistence = cli.no_token_persistence || g.no_token_persistence.unwrap_or(false);
    let token_store = if no_token_persistence {
        None
    } else {
        Some(PathBuf::from(expand_tilde(
            &pick([&cli.token_store, &g.token_store, &None]).unwrap_or_else(default_token_store),
        )))
    };

    // --- serving-scoped (profile layer instead of global) ---
    let backend = pick([&cli.backend, &prof.backend, &None]).unwrap_or_else(|| "runpy".into());
    let serve_with =
        pick([&cli.serve_with, &prof.serve_with, &None]).unwrap_or_else(|| "runpy".into());
    if serve_with != "runpy" && serve_with != "tt" {
        anyhow::bail!("serve_with must be \"runpy\" or \"tt\", got {serve_with:?}");
    }
    let serving_host =
        pick([&cli.serving_host, &prof.serving_host, &None]).unwrap_or_else(|| "127.0.0.1".into());
    let serving_port = pick([&cli.serving_port, &prof.serving_port, &None]).unwrap_or(8000);
    let serving_image = pick([&cli.serving_image, &prof.serving_image, &None]);
    let auto_image = cli.auto_image || prof.auto_image.unwrap_or(false);
    let tt_device = pick([&cli.tt_device, &prof.tt_device, &None]);
    // Serving through `tt serve` runs run.py from tt's own managed checkout,
    // which is where `workflow_logs/` land: default the log source there.
    let tt_inference_repo = expand_tilde(
        &pick([&cli.tt_inference_repo, &prof.tt_inference_repo, &None])
            .or_else(|| {
                (serve_with == "tt")
                    .then(crate::ttcli::managed_inference_checkout)
                    .flatten()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(default_tt_inference_repo),
    );
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
        active_profile,
        available_profiles,
        backend,
        serve_with,
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
    serve_with: None,
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
    fn serve_with_comes_from_the_profile_and_the_cli_wins() {
        let file = file_with("[profile.p]\nserve_with = \"tt\"\n");
        let r = resolve(base_cli(), None, Some(file), None).unwrap();
        assert_eq!(r.serve_with, "tt");

        let file = file_with("[profile.p]\nserve_with = \"tt\"\n");
        let cli = CliOverrides {
            serve_with: Some("runpy".into()),
            ..base_cli()
        };
        assert_eq!(
            resolve(cli, None, Some(file), None).unwrap().serve_with,
            "runpy"
        );

        assert_eq!(
            resolve(base_cli(), None, None, None).unwrap().serve_with,
            "runpy"
        );
        let bad = file_with("[profile.p]\nserve_with = \"docker\"\n");
        assert!(resolve(base_cli(), None, Some(bad), None).is_err());
    }

    #[test]
    fn implicit_default_profile_when_no_file() {
        let r = resolve(base_cli(), None, None, None).unwrap();
        assert_eq!(r.active_profile, None);
        assert!(r.available_profiles.is_empty());
        assert_eq!(r.backend, "runpy");
        assert_eq!(r.serving_port, 8000); // built-in default
        assert_eq!(r.chips, None); // detected at startup, not defaulted here
    }

    #[test]
    fn global_supplies_value_below_default() {
        let f = file_with("[global]\nchips = \"8xBH\"\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.chips.as_deref(), Some("8xBH"));
    }

    #[test]
    fn profile_overrides_global_and_default() {
        let f = file_with(
            "default_profile=\"p\"\n[global]\nchips=\"8xBH\"\n[profile.p]\nserving_port=8003\n",
        );
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!(r.active_profile.as_deref(), Some("p"));
        assert_eq!(r.serving_port, 8003); // from profile
        assert_eq!(r.chips.as_deref(), Some("8xBH")); // from global
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
    fn name_and_ctrl_port_default_when_absent() {
        // The packaged unit runs the binary with no flags and no config file.
        let r = resolve(CliOverrides::default(), None, None, None).unwrap();
        assert_eq!(r.ctrl_port, DEFAULT_CTRL_PORT);
        let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
        assert_eq!(r.name, sanitize_box_name(&host));
        assert!(!r.name.is_empty());

        // Explicit values still win: config file, then the flag above it.
        let f = file_with("[global]\nname = \"lab\"\nctrl_port = 9001\n");
        let r = resolve(CliOverrides::default(), None, Some(f), None).unwrap();
        assert_eq!((r.name.as_str(), r.ctrl_port), ("lab", 9001));
        let f = file_with("[global]\nname = \"lab\"\nctrl_port = 9001\n");
        let r = resolve(base_cli(), None, Some(f), None).unwrap();
        assert_eq!((r.name.as_str(), r.ctrl_port), ("box", 8765));
    }

    #[test]
    fn hostnames_become_box_names() {
        assert_eq!(sanitize_box_name("qb2-lab\n"), "qb2-lab");
        assert_eq!(sanitize_box_name("Galaxy01.corp.example.com"), "galaxy01");
        assert_eq!(sanitize_box_name("my_box  two"), "my-box-two");
        assert_eq!(sanitize_box_name("--"), "tt-station");
        assert_eq!(sanitize_box_name(""), "tt-station");
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
