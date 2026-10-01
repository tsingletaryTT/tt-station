//! THE `(board_type, count) -> device-mesh label` table, shared by every tt-station component
//! that has to name a device configuration:
//!
//! * the box agent (`tt-station-agentd::device`, re-exported there as `mesh_for`) — from
//!   `tt-smi -s` board types, to pick `run.py --tt-device` and report `/status.device_mesh`;
//! * the Mac-side local detection (`crate::local_device`, `tt-station local`) — from the PCI
//!   subsystem ID of a card attached over Thunderbolt, to ask the official `tt` CLI which
//!   models are right-sized for it (`tt model list --hw <mesh>`).
//!
//! The labels are the official tooling's device-config vocabulary (tt-inference-server's
//! `run.py --device` ids, which `tt model list --hw` also takes).

/// `(board_type, count) -> mesh label`. `count` counts ASICs (one `tt-smi -s` `device_info`
/// entry, or one PCI function, per ASIC), NOT physical cards — a p300 card presents two, which
/// is why four `p300c` entries are `p300x2` (two boards) and two are `p300` (one board). gozer's
/// grants count the same way (one BDF per ASIC), so a granted chip count can be passed straight in.
///
/// Board types are lowercase and may carry a revision suffix (`p100a`, `p150c`, `p300c`); only
/// suffixes seen in the field are listed rather than stripped generically.
///
/// `None` for any combination this codebase has no CONFIRMED mapping for. Callers must treat
/// that as "don't guess" — see the agent's `resolve_tt_device`.
///
/// `p100`/`p100a` → `p100` matches the official tt CLI, whose `infer_device_config` strips the
/// revision suffix and maps one `p100` board to the `p100` device config
/// (tenstorrent/tt-cli `backends/serving/inference_server.py`, `_BOARDS_TO_DEVICE`).
pub fn mesh_for(board_type: &str, count: usize) -> Option<&'static str> {
    let mesh = match (board_type, count) {
        ("p300c", 4) => "p300x2",
        ("p300c", 2) => "p300",
        ("p150" | "p150c", 1) => "p150",
        ("p150" | "p150c", 2) => "p150x2",
        ("p150" | "p150c", 3) => "p150x3",
        ("p150" | "p150c", 4) => "p150x4",
        ("p100" | "p100a", 1) => "p100",
        ("n300", 4) => "n300x4",
        ("n300", 1) => "n300",
        _ => return None,
    };
    Some(mesh)
}

#[cfg(test)]
mod tests {
    use super::mesh_for;

    #[test]
    fn single_p100a_is_p100() {
        // The owner's card: subsystem 0x43 → luwen "p100a" → tt CLI device config "p100".
        assert_eq!(mesh_for("p100a", 1), Some("p100"));
        assert_eq!(mesh_for("p100", 1), Some("p100"));
    }

    #[test]
    fn two_p100s_are_not_a_known_mesh() {
        // tt CLI has no multi-P100 config; refuse rather than invent "p100x2".
        assert_eq!(mesh_for("p100a", 2), None);
    }

    #[test]
    fn existing_rows_unchanged() {
        assert_eq!(mesh_for("p300c", 4), Some("p300x2"));
        assert_eq!(mesh_for("p300c", 2), Some("p300"));
        assert_eq!(mesh_for("p150", 4), Some("p150x4"));
        assert_eq!(mesh_for("n300", 1), Some("n300"));
        assert_eq!(mesh_for("p300c", 1), None);
    }
}
