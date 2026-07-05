//! TOML config schema for `agentd.toml`, plus the loader that turns a path
//! on disk into a parsed [`AgentConfigFile`].
//!
//! This module only covers the *parsed-file* shape (Task 1 of the
//! config-profiles plan; see
//! `docs/superpowers/plans/2026-07-05-agentd-config-profiles.md`). The
//! precedence resolver (CLI overrides > env > profile > global > built-in
//! defaults) that turns this into a `ResolvedConfig` lands in a later task.

use std::collections::BTreeMap;
use std::path::Path;

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
