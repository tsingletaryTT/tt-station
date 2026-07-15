//! Box power actions surfaced by `POST /power` (see `routes::power`).
//!
//! `reset-chips` is a board reset (`tt-smi -r`) that KEEPS pairing — distinct
//! from `POST /reset`, which unpairs. `suspend`/`reboot`/`shutdown` take the
//! whole machine down and are the "machine ops": they best-effort stop any
//! serving container first (see `AppState::run_power_command`).

/// A power action requested over `POST /power`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    /// `tt-smi -r` board reset; preserves pairing (unlike `/reset`).
    ResetChips,
    Suspend,
    Reboot,
    Shutdown,
}

impl PowerAction {
    /// Parse the wire value from `POST /power`'s `{"action": ...}` body.
    pub fn parse(s: &str) -> Option<PowerAction> {
        match s {
            "reset-chips" => Some(PowerAction::ResetChips),
            "suspend" => Some(PowerAction::Suspend),
            "reboot" => Some(PowerAction::Reboot),
            "shutdown" => Some(PowerAction::Shutdown),
            _ => None,
        }
    }

    /// True for the ops that take the whole box down (everything but a chip
    /// reset). Machine ops best-effort stop serving before running.
    pub fn is_machine_op(&self) -> bool {
        !matches!(self, PowerAction::ResetChips)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_four_actions() {
        assert_eq!(PowerAction::parse("reset-chips"), Some(PowerAction::ResetChips));
        assert_eq!(PowerAction::parse("suspend"), Some(PowerAction::Suspend));
        assert_eq!(PowerAction::parse("reboot"), Some(PowerAction::Reboot));
        assert_eq!(PowerAction::parse("shutdown"), Some(PowerAction::Shutdown));
        assert_eq!(PowerAction::parse("halt"), None);
        assert_eq!(PowerAction::parse(""), None);
    }

    #[test]
    fn only_reset_chips_is_not_a_machine_op() {
        assert!(!PowerAction::ResetChips.is_machine_op());
        assert!(PowerAction::Suspend.is_machine_op());
        assert!(PowerAction::Reboot.is_machine_op());
        assert!(PowerAction::Shutdown.is_machine_op());
    }
}
