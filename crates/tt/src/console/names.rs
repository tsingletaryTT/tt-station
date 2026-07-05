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

#[allow(dead_code)]
impl ToolNames {
    pub fn from_env() -> Self {
        fn env_or(key: &str, default: &str) -> String {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        }
        ToolNames {
            tt_bin: env_or("TTS_TT_BIN", "tt"),
            agent_bin: env_or("TTS_AGENT_BIN", "tt-station-agentd"),
            service_name: env_or("TTS_SERVICE_NAME", "tt-station-agentd.service"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // NB: env is process-global; set+remove within each test, don't run in parallel-hostile ways.
    #[test]
    fn defaults_when_unset() {
        std::env::remove_var("TTS_TT_BIN");
        std::env::remove_var("TTS_AGENT_BIN");
        std::env::remove_var("TTS_SERVICE_NAME");
        let n = ToolNames::from_env();
        assert_eq!(n.tt_bin, "tt");
        assert_eq!(n.agent_bin, "tt-station-agentd");
        assert_eq!(n.service_name, "tt-station-agentd.service");
    }
    #[test]
    fn env_overrides_win() {
        std::env::set_var("TTS_TT_BIN", "tt-cli");
        std::env::set_var("TTS_SERVICE_NAME", "quietbox-agent.service");
        let n = ToolNames::from_env();
        assert_eq!(n.tt_bin, "tt-cli");
        assert_eq!(n.service_name, "quietbox-agent.service");
        std::env::remove_var("TTS_TT_BIN");
        std::env::remove_var("TTS_SERVICE_NAME");
    }
}
