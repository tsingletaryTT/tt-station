//! Wake-on-LAN magic-packet construction (client-side; `tt wake` sends it).

/// Parse a MAC address (`:` or `-` separated hex) into 6 bytes.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

/// Build the 102-byte WoL magic packet: 6 bytes of `0xFF` then the target MAC
/// repeated 16 times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0u8; 102];
    for b in p.iter_mut().take(6) {
        *b = 0xff;
    }
    for i in 0..16 {
        p[6 + i * 6..6 + i * 6 + 6].copy_from_slice(&mac);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_mac_accepts_colon_and_dash() {
        assert_eq!(parse_mac("01:02:03:04:05:06"), Some([1, 2, 3, 4, 5, 6]));
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-ff"), Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(parse_mac("nope"), None);
    }
    #[test]
    fn magic_packet_is_6xff_then_16x_mac() {
        let mac = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        let p = magic_packet(mac);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[0..6], &[0xff; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..6 + i * 6 + 6], &mac);
        }
    }
}
