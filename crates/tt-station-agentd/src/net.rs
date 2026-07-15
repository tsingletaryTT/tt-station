//! Best-effort primary-NIC MAC detection, advertised in `/status` + the mDNS
//! TXT record so the Mac can send a Wake-on-LAN magic packet when the box is
//! off. Mirrors how `device::detect_device_mesh` feeds `/status`.

/// Normalize a MAC string to lowercase colon form (`aa:bb:cc:dd:ee:ff`).
/// Returns `None` for anything that isn't 6 hex octets, or the all-zero MAC
/// (which real NICs never have and some virtual interfaces report).
pub fn normalize_mac(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = Vec::with_capacity(6);
    for p in parts {
        if p.len() != 2 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        out.push(p.to_ascii_lowercase());
    }
    let joined = out.join(":");
    if joined == "00:00:00:00:00:00" {
        return None;
    }
    Some(joined)
}

/// The MAC of the interface carrying the box's LAN IP, or `None` if it can't
/// be determined. Best-effort: parses `ip -o link` / `/sys/class/net`; any
/// failure yields `None` (Wake is simply disabled for that box).
pub fn primary_mac() -> Option<String> {
    // Prefer the iface backing the default route; fall back to the first
    // non-loopback iface with a usable MAC. Read /sys/class/net/<if>/address.
    let route = std::process::Command::new("ip")
        .args(["route", "get", "1.1.1.1"])
        .output()
        .ok()?;
    let route = String::from_utf8_lossy(&route.stdout);
    let dev = route.split_whitespace().skip_while(|t| *t != "dev").nth(1);
    if let Some(dev) = dev {
        if let Ok(addr) = std::fs::read_to_string(format!("/sys/class/net/{dev}/address")) {
            if let Some(mac) = normalize_mac(addr.trim()) {
                return Some(mac);
            }
        }
    }
    // Fallback: scan /sys/class/net for the first real MAC (skip lo).
    for entry in std::fs::read_dir("/sys/class/net").ok()?.flatten() {
        let name = entry.file_name();
        if name == "lo" {
            continue;
        }
        if let Ok(addr) = std::fs::read_to_string(entry.path().join("address")) {
            if let Some(mac) = normalize_mac(addr.trim()) {
                return Some(mac);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalize_mac_accepts_colon_and_dash_lowercases() {
        assert_eq!(
            normalize_mac("AA:BB:CC:DD:EE:FF").as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(
            normalize_mac("aa-bb-cc-dd-ee-ff").as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        assert_eq!(normalize_mac("00:00:00:00:00:00"), None); // all-zero = not a real NIC
        assert_eq!(normalize_mac("aa:bb:cc"), None);
        assert_eq!(normalize_mac("zz:bb:cc:dd:ee:ff"), None);
        assert_eq!(normalize_mac(""), None);
    }
}
