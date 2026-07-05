//! Pure mapping from a `tt-smi -s` snapshot to this box's device-mesh label.
//!
//! The single source of truth for `(board_type, count) -> mesh`. Both the
//! runpy backend (choosing `--tt-device`) and the `/status` route (reporting
//! the box's mesh so clients can rank models by hardware fit) call this, so the
//! table lives in exactly one place.

use serde_json::Value;

/// Map a verbatim `tt-smi -s` JSON snapshot to a device-mesh label
/// (`"p300x2"`, `"p150x4"`, …). Returns `None` when `device_info` is empty,
/// the fleet is mixed (boards of differing `board_type`), or the
/// (type, count) pair isn't a known mesh.
pub fn detect_device_mesh(tt_smi_json: &str) -> Option<String> {
    let value: Value = serde_json::from_str(tt_smi_json).ok()?;
    let board_types: Vec<String> = value
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
        .collect();
    let count = board_types.len();
    if count == 0 || !board_types.windows(2).all(|p| p[0] == p[1]) {
        return None;
    }
    let mesh = match (board_types[0].as_str(), count) {
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
    Some(mesh.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let arr = std::iter::repeat(entry).take(n).collect::<Vec<_>>().join(",");
            format!(r#"{{"device_info":[{arr}]}}"#)
        };
        assert_eq!(detect_device_mesh(&f(1)).as_deref(), Some("p150"));
        assert_eq!(detect_device_mesh(&f(2)).as_deref(), Some("p150x2"));
        assert_eq!(detect_device_mesh(&f(3)).as_deref(), Some("p150x3"));
        assert_eq!(detect_device_mesh(&f(4)).as_deref(), Some("p150x4"));
    }
}
