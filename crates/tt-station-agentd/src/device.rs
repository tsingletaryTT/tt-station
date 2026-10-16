//! Pure mapping from a `tt-smi -s` snapshot to this box's device-mesh label.
//!
//! The single source of truth for `(board_type, count) -> mesh`. Both the
//! runpy backend (choosing `--tt-device`) and the `/status` route (reporting
//! the box's mesh so clients can rank models by hardware fit) call this, so the
//! table lives in exactly one place.

use anyhow::Result;
use serde_json::Value;

/// Map a verbatim `tt-smi -s` JSON snapshot to a device-mesh label
/// (`"p300x2"`, `"p150x4"`, …). Returns `None` when `device_info` is empty,
/// the fleet is mixed (boards of differing `board_type`), or the
/// (type, count) pair isn't a known mesh.
pub fn detect_device_mesh(tt_smi_json: &str) -> Option<String> {
    let board_types = board_types(tt_smi_json)?;
    let count = board_types.len();
    if count == 0 || !board_types.windows(2).all(|p| p[0] == p[1]) {
        return None;
    }
    mesh_for(&board_types[0], count).map(str::to_string)
}

/// The box's chip inventory label (`"4xBH"`, `"2xWH"`, `"32xWH"`) from a
/// verbatim `tt-smi -s` snapshot: one count per ASIC (`device_info` entry,
/// the same unit `mesh_for` counts) and the architecture its `board_type`
/// names. A mixed fleet lists each architecture (`"2xBH+1xWH"`). `None` when
/// the snapshot is unreadable, empty, or names a board this table doesn't
/// know -- callers report an empty label then rather than guess.
pub fn chips_label(tt_smi_json: &str) -> Option<String> {
    let board_types = board_types(tt_smi_json)?;
    if board_types.is_empty() {
        return None;
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for b in &board_types {
        let arch = arch_for(b)?;
        match counts.iter_mut().find(|(a, _)| *a == arch) {
            Some((_, n)) => *n += 1,
            None => counts.push((arch, 1)),
        }
    }
    Some(
        counts
            .iter()
            .map(|(arch, n)| format!("{n}x{arch}"))
            .collect::<Vec<_>>()
            .join("+"),
    )
}

/// `board_type` -> architecture code the apps expand (`BH` Blackhole, `WH`
/// Wormhole, `GS` Grayskull). Blackhole boards are the p-series
/// (p100/p150/p300) and Blackhole Galaxy; Wormhole boards are the n-series
/// (n150/n300) and Wormhole Galaxy; Grayskull is the e-series (e75/e150).
fn arch_for(board_type: &str) -> Option<&'static str> {
    let b = board_type.to_ascii_lowercase();
    if b.contains("bh") || b.contains("blackhole") || b.starts_with('p') {
        Some("BH")
    } else if b.contains("wh")
        || b.contains("wormhole")
        || b.contains("galaxy")
        || b.starts_with('n')
    {
        Some("WH")
    } else if b.contains("grayskull") || b.starts_with('e') {
        Some("GS")
    } else {
        None
    }
}

/// Every entry's lowercased `board_info.board_type` from a `tt-smi -s`
/// snapshot, in the order `device_info` lists them. `None` when the snapshot
/// isn't parseable JSON or has no `device_info` array.
///
/// Split out of `detect_device_mesh` so a caller that knows the COUNT from
/// somewhere else -- the runpy backend deriving a mesh for the chips a gozer
/// lease actually granted, rather than for every board on the box -- can
/// reuse this table instead of growing a second one.
pub fn board_types(tt_smi_json: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(tt_smi_json).ok()?;
    Some(
        value
            .get("device_info")?
            .as_array()?
            .iter()
            .filter_map(|d| {
                Some(
                    d.get("board_info")?
                        .get("board_type")?
                        .as_str()?
                        .to_lowercase(),
                )
            })
            .collect(),
    )
}

/// THE table: `(board_type, count) -> mesh label`. `count` is a count of
/// `tt-smi -s` `device_info` entries, i.e. of ASICs, NOT of physical cards
/// -- a p300 card presents two, which is why four `p300c` entries are
/// `p300x2` (two boards) and two are `p300` (one board). gozer's grants
/// count the same way (one BDF per ASIC), so a granted chip count can be
/// passed straight in here.
///
/// `None` for any combination this codebase has no CONFIRMED mapping for.
/// Callers must treat that as "don't guess" -- see `resolve_tt_device`.
/// The mesh label for the shape a gozer lease actually GRANTED, derived from
/// a verbatim `tt-smi -s` snapshot plus the grant's chip count.
///
/// `detect_device_mesh` above answers for the WHOLE BOX, which on a two-board
/// machine is `p300x2` even when the lease covers one board. Handing that to
/// a serving backend next to a device selector naming half of it describes a
/// mesh the tenant does not own. So: take the board TYPE from the snapshot
/// (the box is the only thing that knows it) and the COUNT from the grant
/// (`Grant::chips` is one BDF per ASIC, exactly what `mesh_for` counts), and
/// look the pair up in the same single table.
///
/// **Fails closed**, like `gozer::Grant::reset_target`: an unparseable
/// snapshot, an empty one, a mixed fleet, or a (type, count) pair with no
/// confirmed mesh all return `Err`, and every caller is expected to refuse
/// the serve rather than fall back. Omitting the flag and letting the
/// launcher auto-detect is NOT a safe fallback -- its detection looks at the
/// whole box too, which is the very thing being avoided.
///
/// Shared by `RunPyBackend::leased_tt_device` and
/// `DockerBackend::leased_tt_device` so the fail-closed policy and the table
/// lookup exist once rather than once per backend. `lease_id` appears only in
/// the error messages, so an operator reading the journal can tell which
/// lease was refused.
pub fn leased_mesh(tt_smi_snapshot: &str, granted_chips: usize, lease_id: &str) -> Result<String> {
    let board_types = board_types(tt_smi_snapshot).unwrap_or_default();
    let Some(board_type) = board_types.first() else {
        return Err(anyhow::anyhow!(
            "cannot derive a device mesh for lease '{lease_id}': `tt-smi -s` reported no boards"
        ));
    };
    if !board_types.windows(2).all(|pair| pair[0] == pair[1]) {
        return Err(anyhow::anyhow!(
            "cannot derive a device mesh for lease '{lease_id}': this box has a mixed fleet \
             ({board_types:?}), so a chip count doesn't identify a mesh"
        ));
    }

    mesh_for(board_type, granted_chips)
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot derive a device mesh for lease '{lease_id}': no known mesh for \
                 {granted_chips}x {board_type}; refusing to serve rather than name a mesh \
                 wider than the lease"
            )
        })
}

pub fn mesh_for(board_type: &str, count: usize) -> Option<&'static str> {
    let mesh = match (board_type, count) {
        ("p300c", 4) => "p300x2",
        ("p300c", 2) => "p300",
        ("p150" | "p150c", 1) => "p150",
        ("p150" | "p150c", 2) => "p150x2",
        ("p150" | "p150c", 3) => "p150x3",
        ("p150" | "p150c", 4) => "p150x4",
        ("n300", 4) => "n300x4",
        ("n300", 1) => "n300",
        _ => return None,
    };
    Some(mesh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(boards: &[&str]) -> String {
        let entries: Vec<String> = boards
            .iter()
            .map(|b| format!(r#"{{"board_info":{{"board_type":"{b}"}}}}"#))
            .collect();
        format!(r#"{{"device_info":[{}]}}"#, entries.join(","))
    }

    #[test]
    fn chips_label_counts_asics_by_architecture() {
        assert_eq!(
            chips_label(&snapshot(&["p300c"; 4])).as_deref(),
            Some("4xBH")
        );
        assert_eq!(chips_label(&snapshot(&["p150a"])).as_deref(), Some("1xBH"));
        assert_eq!(
            chips_label(&snapshot(&["n300"; 2])).as_deref(),
            Some("2xWH")
        );
        assert_eq!(
            chips_label(&snapshot(&["tt-galaxy-wh"; 32])).as_deref(),
            Some("32xWH")
        );
        assert_eq!(
            chips_label(&snapshot(&["p150", "p150", "n150"])).as_deref(),
            Some("2xBH+1xWH")
        );
    }

    #[test]
    fn chips_label_is_none_when_detection_tells_nothing() {
        assert_eq!(chips_label(""), None); // the test stub's empty output
        assert_eq!(chips_label("not json"), None);
        assert_eq!(chips_label(r#"{"device_info":[]}"#), None);
        assert_eq!(chips_label(&snapshot(&["mystery"])), None);
    }

    #[test]
    fn maps_four_p300c_to_p300x2() {
        let json = r#"{"device_info":[
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"p300c"}},
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"p300c"}}]}"#;
        assert_eq!(detect_device_mesh(json).as_deref(), Some("p300x2"));
    }

    #[test]
    fn maps_single_n300() {
        let json = r#"{"device_info":[{"board_info":{"board_type":"n300"}}]}"#;
        assert_eq!(detect_device_mesh(json).as_deref(), Some("n300"));
    }

    #[test]
    fn mixed_fleet_is_none() {
        let json = r#"{"device_info":[
            {"board_info":{"board_type":"p300c"}},{"board_info":{"board_type":"n300"}}]}"#;
        assert_eq!(detect_device_mesh(json), None);
    }

    #[test]
    fn empty_device_info_is_none() {
        assert_eq!(detect_device_mesh(r#"{"device_info":[]}"#), None);
    }

    #[test]
    fn unknown_count_is_none() {
        let json = r#"{"device_info":[{"board_info":{"board_type":"p300c"}}]}"#;
        assert_eq!(detect_device_mesh(json), None); // 1x p300c is not a known mesh
    }

    #[test]
    fn garbage_json_is_none() {
        assert_eq!(detect_device_mesh("not json"), None);
    }

    #[test]
    fn maps_p150_counts() {
        let f = |n: usize| {
            let entry = r#"{"board_info":{"board_type":"p150"}}"#;
            let arr = std::iter::repeat_n(entry, n).collect::<Vec<_>>().join(",");
            format!(r#"{{"device_info":[{arr}]}}"#)
        };
        assert_eq!(detect_device_mesh(&f(1)).as_deref(), Some("p150"));
        assert_eq!(detect_device_mesh(&f(2)).as_deref(), Some("p150x2"));
        assert_eq!(detect_device_mesh(&f(3)).as_deref(), Some("p150x3"));
        assert_eq!(detect_device_mesh(&f(4)).as_deref(), Some("p150x4"));
    }
}
