//! Pure lifecycle logic for `tt console`: parse `systemctl show` output into
//! a [`ServiceState`], parse tailed journal lines into a [`PairingState`],
//! and derive an overall [`LifecycleState`] from a [`BoxLifecycleSnapshot`].
//!
//! Everything here is a pure function -- no I/O, no process spawning, no
//! wall-clock reads. The collector (a later task) is responsible for
//! actually running `systemctl show`/`journalctl` and calling `tt-station`
//! agent routes, then feeding the results through these functions. Keeping
//! the logic pure makes it exhaustively unit-testable without mocking a
//! shell or a systemd instance.

// `tt` is a bin crate; clippy's dead-code lint fires on pub items nothing
// calls yet from `main.rs`. This task only establishes the pure logic core --
// the collector/actions/TUI (later tasks) wire it in. Drop this allow once
// something calls it.
#![allow(dead_code)]

use libttstation::model::{BoxLifecycleSnapshot, PairingState, ServiceState, ServingStatus};

/// How long a freshly-issued pairing code stays valid, in seconds.
///
/// Must match the agent's own `PAIR_TTL` (`crates/tt-station-agentd/src/routes.rs`,
/// `Duration::from_secs(120)`) and the GTK box panel's `PAIR_TTL_SECS`
/// (`box-panel/tt-station-panel.py`) -- all three surfaces show a countdown
/// for the same underlying pairing attempt, so they must agree on its TTL.
pub const PAIRING_TTL_SECS: u64 = 120;

/// Parse the `ActiveState=` line out of `systemctl show <unit>` output into
/// a [`ServiceState`]. Only `ActiveState` is consulted (not `SubState`) --
/// `SubState` gives finer detail (e.g. `running` vs `start` vs `dead`) but
/// `ActiveState` alone is enough to distinguish the six states `tt console`
/// cares about. Unrecognized or missing `ActiveState` values (e.g. `systemctl
/// show` on a unit that doesn't exist, or output that isn't `systemctl show`
/// at all) map to [`ServiceState::Unknown`] rather than panicking -- the
/// collector always has *some* text to hand this, even when it's garbage.
pub fn parse_service_state(show_output: &str) -> ServiceState {
    let mut active = "";
    for line in show_output.lines() {
        if let Some(v) = line.strip_prefix("ActiveState=") {
            active = v.trim();
        }
    }
    match active {
        "active" => ServiceState::Active,
        "inactive" => ServiceState::Inactive,
        "activating" => ServiceState::Activating,
        "deactivating" => ServiceState::Deactivating,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Unknown,
    }
}

/// Find the most-recently-logged 6-digit pairing code in `lines` (oldest
/// first, as `journalctl` emits them) and report it with a full TTL.
///
/// The agent logs pairing codes as `tt-station-agentd: pairing code: NNNNNN`
/// (`crates/tt-station-agentd/src/routes.rs`, `init_pair`); the GTK panel
/// parses the same line with `CODE_RE = re.compile(r"pairing code:\s*(\d{6})")`
/// (`box-panel/tt-station-panel.py`). This mirrors that: any line mentioning
/// "pairing" or "code" (case-insensitively) that contains a standalone
/// 6-digit run is a candidate, and the most recent one wins.
///
/// `journalctl -o cat` (the format the collector tails) strips timestamps,
/// so there's no way to compute *how long ago* a line was logged -- only
/// that it was seen. A freshly-tailed sighting is therefore always reported
/// as full-TTL (`PAIRING_TTL_SECS`), exactly like the box panel, which
/// starts its own countdown from the moment it first sees the code rather
/// than from any timestamp. `now` is accepted (and threaded through by the
/// collector) for future use -- e.g. if a timestamped log source is added
/// later -- but today's `-o cat` journal gives this function nothing to
/// subtract `now` from.
pub fn parse_pairing(lines: &[String], _now: u64) -> Option<PairingState> {
    for line in lines.iter().rev() {
        let lower = line.to_lowercase();
        if lower.contains("pairing") || lower.contains("code") {
            if let Some(code) = find_six_digit_run(line) {
                return Some(PairingState {
                    code,
                    expires_in_secs: PAIRING_TTL_SECS,
                });
            }
        }
    }
    None
}

/// Find the first standalone run of exactly 6 ASCII digits in `s`. A run
/// longer than 6 digits (e.g. embedded in a longer number or a timestamp)
/// does not match -- this avoids misreading, say, a PID or a Unix timestamp
/// as a pairing code.
fn find_six_digit_run(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 6 {
                return Some(chars[start..i].iter().collect());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// The operator-facing lifecycle state `tt console` renders for one box --
/// a small, display-ready summary of the raw [`BoxLifecycleSnapshot`],
/// collapsing "which systemd state + is it reachable + what's it serving"
/// into the handful of states an operator actually needs to distinguish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleState {
    /// The agent service is not running (`ServiceState::Inactive` or
    /// `Unknown` -- an unrecognized/missing state is treated the same as
    /// "not running" rather than surfaced as its own ambiguous state).
    Inactive,
    /// The service is coming up (`Activating`), or is `Active` per systemd
    /// but not yet answering `/status` (`reachable == false`) -- e.g. the
    /// process has forked but hasn't bound its port yet.
    Starting,
    /// Reachable and active, with no model currently being served.
    Idle,
    /// Reachable and active, serving the named model.
    Serving(String),
    /// The service is shutting down (`Deactivating`).
    Stopping,
    /// The service is in systemd's `failed` state.
    Failed,
}

/// Derive a [`LifecycleState`] from a [`BoxLifecycleSnapshot`]. Pure
/// function of the snapshot's `service`/`reachable`/`status` fields --
/// see [`LifecycleState`]'s variants for the exact precedence:
/// `Failed`/`Deactivating`/`Activating` map directly; `Inactive`/`Unknown`
/// become `Inactive`; and only when the service is `Active` do
/// `reachable`/`status` get consulted at all (unreachable-but-active is
/// `Starting`, reachable falls through to `status`).
pub fn derive_state(snapshot: &BoxLifecycleSnapshot) -> LifecycleState {
    match snapshot.service {
        ServiceState::Failed => LifecycleState::Failed,
        ServiceState::Deactivating => LifecycleState::Stopping,
        ServiceState::Activating => LifecycleState::Starting,
        ServiceState::Inactive | ServiceState::Unknown => LifecycleState::Inactive,
        ServiceState::Active => {
            if !snapshot.reachable {
                LifecycleState::Starting
            } else {
                match &snapshot.status {
                    Some(ServingStatus::Serving(model)) => LifecycleState::Serving(model.clone()),
                    _ => LifecycleState::Idle,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::{BoxLifecycleSnapshot, ServiceState, ServingStatus};

    #[test]
    fn service_state_from_systemctl_show() {
        assert_eq!(
            parse_service_state("ActiveState=active\nSubState=running\n"),
            ServiceState::Active
        );
        assert_eq!(
            parse_service_state("ActiveState=inactive\nSubState=dead\n"),
            ServiceState::Inactive
        );
        assert_eq!(
            parse_service_state("ActiveState=activating\nSubState=start\n"),
            ServiceState::Activating
        );
        assert_eq!(
            parse_service_state("ActiveState=deactivating\nSubState=stop\n"),
            ServiceState::Deactivating
        );
        assert_eq!(
            parse_service_state("ActiveState=failed\nSubState=failed\n"),
            ServiceState::Failed
        );
        assert_eq!(parse_service_state("garbage"), ServiceState::Unknown);
    }

    #[test]
    fn pairing_from_journal_recent_code() {
        // Real wording from `crates/tt-station-agentd/src/routes.rs`'s
        // `println!("tt-station-agentd: pairing code: {code}");`, matched by
        // the box panel's `CODE_RE = re.compile(r"pairing code:\s*(\d{6})")`.
        let lines = vec![
            "agent started".to_string(),
            "tt-station-agentd: pairing code: 042817".to_string(),
        ];
        // journal has no timestamps in `-o cat`; TTL is computed from "seen now".
        let p = parse_pairing(&lines, 1_000).unwrap();
        assert_eq!(p.code, "042817");
        assert_eq!(p.expires_in_secs, PAIRING_TTL_SECS); // fresh sighting → full TTL
    }

    #[test]
    fn pairing_picks_most_recent_code_when_multiple() {
        let lines = vec![
            "tt-station-agentd: pairing code: 111111".to_string(),
            "tt-station-agentd: pairing code: 222222".to_string(),
        ];
        let p = parse_pairing(&lines, 1_000).unwrap();
        assert_eq!(p.code, "222222");
    }

    #[test]
    fn pairing_none_when_no_code() {
        assert!(parse_pairing(&["agent started".to_string()], 1_000).is_none());
    }

    #[test]
    fn pairing_ignores_six_digit_runs_outside_pairing_lines() {
        // A 6-digit number that shows up in an unrelated log line (e.g. a
        // PID) must not be mistaken for a pairing code.
        let lines = vec!["worker pid 123456 exited".to_string()];
        assert!(parse_pairing(&lines, 1_000).is_none());
    }

    fn snap(
        service: ServiceState,
        reachable: bool,
        status: Option<ServingStatus>,
    ) -> BoxLifecycleSnapshot {
        BoxLifecycleSnapshot {
            service,
            reachable,
            name: None,
            chips: None,
            status,
            endpoint: None,
            serving: vec![],
            config: None,
            pairing: None,
        }
    }

    #[test]
    fn derive_covers_states() {
        assert_eq!(
            derive_state(&snap(ServiceState::Inactive, false, None)),
            LifecycleState::Inactive
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Unknown, false, None)),
            LifecycleState::Inactive
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Activating, false, None)),
            LifecycleState::Starting
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Active, false, None)),
            LifecycleState::Starting // active but not yet reachable
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Active, true, Some(ServingStatus::Idle))),
            LifecycleState::Idle
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Active, true, None)),
            LifecycleState::Idle
        );
        assert_eq!(
            derive_state(&snap(
                ServiceState::Active,
                true,
                Some(ServingStatus::Serving("m".into()))
            )),
            LifecycleState::Serving("m".into())
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Deactivating, false, None)),
            LifecycleState::Stopping
        );
        assert_eq!(
            derive_state(&snap(ServiceState::Failed, false, None)),
            LifecycleState::Failed
        );
    }
}
