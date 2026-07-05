//! Single source of truth for the project's CLI tool + service names, so a
//! future rename (`tt` → `tt-cli`, etc.) is a one-place change. Every
//! systemctl/journalctl/unit-template reference resolves names from here.

// `tt` is a bin crate, so clippy's dead-code lint fires on pub items that
// aren't yet called from `main.rs` — expected for this task, which only
// establishes the source of truth. Later tasks (lifecycle state machine,
// actions, TUI) wire this in; drop this allow once something calls it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolNames {
    pub tt_bin: String,
    pub agent_bin: String,
    pub service_name: String,
}

// Pulled out of `from_env` so the defaulting/override logic can be
// unit-tested as a pure function — no `std::env` mutation, hence no races
// with sibling tests under the default parallel `cargo test` harness.
#[allow(dead_code)]
fn resolve(tt_bin: Option<String>, agent_bin: Option<String>, service_name: Option<String>) -> ToolNames {
    fn or_default(v: Option<String>, default: &str) -> String {
        v.filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    }
    ToolNames {
        tt_bin: or_default(tt_bin, "tt"),
        agent_bin: or_default(agent_bin, "tt-station-agentd"),
        service_name: or_default(service_name, "tt-station-agentd.service"),
    }
}

#[allow(dead_code)]
impl ToolNames {
    pub fn from_env() -> Self {
        resolve(
            std::env::var("TTS_TT_BIN").ok(),
            std::env::var("TTS_AGENT_BIN").ok(),
            std::env::var("TTS_SERVICE_NAME").ok(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These exercise the pure `resolve()` helper directly — no `std::env`
    // mutation, so they're race-free under the default parallel test
    // harness (unlike testing via `from_env()`, which reads process-global
    // env vars that sibling tests could be mutating concurrently).
    #[test]
    fn defaults_when_unset() {
        let n = resolve(None, None, None);
        assert_eq!(n.tt_bin, "tt");
        assert_eq!(n.agent_bin, "tt-station-agentd");
        assert_eq!(n.service_name, "tt-station-agentd.service");
    }

    #[test]
    fn env_overrides_win() {
        let n = resolve(
            Some("tt-cli".to_string()),
            None,
            Some("quietbox-agent.service".to_string()),
        );
        assert_eq!(n.tt_bin, "tt-cli");
        assert_eq!(n.agent_bin, "tt-station-agentd");
        assert_eq!(n.service_name, "quietbox-agent.service");
    }

    #[test]
    fn empty_string_falls_back_to_default() {
        let n = resolve(Some(String::new()), Some(String::new()), Some(String::new()));
        assert_eq!(n.tt_bin, "tt");
        assert_eq!(n.agent_bin, "tt-station-agentd");
        assert_eq!(n.service_name, "tt-station-agentd.service");
    }
}
