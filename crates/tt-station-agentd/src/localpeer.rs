// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Which account is on the other end of a local connection.
//!
//! A program on the box that reaches the agent over plain HTTP is trusted as
//! the box's owner (it could read the agent's `local-secret` anyway) -- but
//! only if it runs as the agent's own user. Another account on the box, a
//! service running as root, or a container sharing the host's network all
//! connect from 127.0.0.1 too, and must not inherit that trust.
//!
//! Linux says who owns every TCP socket in `/proc/net/tcp` and
//! `/proc/net/tcp6`: one line per socket with its local and remote address,
//! its owner's uid and its inode. The caller's socket is the one whose
//! *local* address is the peer address the agent sees. Sockets in TIME_WAIT
//! linger in those files with no owner (inode 0, uid 0) and are skipped.
//!
//! Elsewhere (no `/proc`), nobody gets local trust: callers pair like any
//! other device.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// One socket from `/proc/net/tcp{,6}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketEntry {
    pub local: SocketAddr,
    pub remote: SocketAddr,
    pub uid: u32,
    pub inode: u64,
}

/// `0100007F:1F90` (IPv4) or a 32-digit IPv6 address and port, as the kernel
/// prints them: each 32-bit word of the address in host byte order, the port
/// as a plain number.
fn parse_addr(field: &str) -> Option<SocketAddr> {
    let (addr, port) = field.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |s: &str| u32::from_str_radix(s, 16).ok().map(u32::to_ne_bytes);
    let ip = match addr.len() {
        8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(&addr[i * 8..i * 8 + 8])?);
            }
            IpAddr::V6(Ipv6Addr::from(bytes))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Every socket in the text of a `/proc/net/tcp{,6}` file. Lines that don't
/// parse (the header, anything unexpected) are skipped.
pub fn parse_proc_net_tcp(text: &str) -> Vec<SocketEntry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 {
                return None;
            }
            Some(SocketEntry {
                local: parse_addr(f[1])?,
                remote: parse_addr(f[2])?,
                uid: f[7].parse().ok()?,
                inode: f[9].parse().ok()?,
            })
        })
        .collect()
}

/// Same address and port, an IPv4-mapped IPv6 address counting as its IPv4.
fn same(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() == b.port() && a.ip().to_canonical() == b.ip().to_canonical()
}

/// The uid that owns the socket at `peer`, if exactly one account does. A
/// peer no live socket matches, or that sockets of two different accounts
/// match, has no answer.
pub fn owner_of(peer: SocketAddr, entries: &[SocketEntry]) -> Option<u32> {
    let mut uid = None;
    for e in entries.iter().filter(|e| e.inode != 0 && same(e.local, peer)) {
        match uid {
            None => uid = Some(e.uid),
            Some(u) if u == e.uid => {}
            Some(_) => return None,
        }
    }
    uid
}

/// The agent's own uid.
pub fn my_uid() -> u32 {
    // SAFETY: getuid has no failure mode and touches no memory.
    unsafe { libc::getuid() }
}

/// Whether the connection from `peer` belongs to a process running as the
/// agent's own user.
#[cfg(target_os = "linux")]
pub fn peer_is_me(peer: SocketAddr) -> bool {
    let mut entries = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            entries.extend(parse_proc_net_tcp(&text));
        }
    }
    owner_of(peer, &entries) == Some(my_uid())
}

#[cfg(not(target_os = "linux"))]
pub fn peer_is_me(_peer: SocketAddr) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = include_str!("../tests/fixtures/proc_net_tcp");
    const TCP6: &str = include_str!("../tests/fixtures/proc_net_tcp6");

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_ipv4_lines() {
        let e = parse_proc_net_tcp(TCP);
        assert_eq!(e.len(), 7, "the header is skipped");
        assert_eq!(e[0].local, addr("0.0.0.0:8765"));
        assert_eq!(e[1].local, addr("127.0.0.1:41000"));
        assert_eq!(e[1].remote, addr("127.0.0.1:8765"));
        assert_eq!(e[1].uid, 1000);
        assert_eq!(e[1].inode, 912345);
        assert_eq!(e[4].local, addr("192.168.1.20:8765"));
        assert_eq!(e[4].remote, addr("192.168.1.31:52814"));
    }

    #[test]
    fn parses_ipv6_lines_including_mapped_ipv4() {
        let e = parse_proc_net_tcp(TCP6);
        assert_eq!(e.len(), 3);
        assert_eq!(e[0].local, addr("[::1]:43210"));
        assert_eq!(e[0].uid, 1000);
        assert_eq!(e[1].local, addr("[::ffff:127.0.0.1]:43300"));
        assert_eq!(e[1].uid, 0);
        assert_eq!(e[2].local, addr("[fe80::1]:8766"));
    }

    #[test]
    fn finds_who_owns_a_peer() {
        let mut e = parse_proc_net_tcp(TCP);
        e.extend(parse_proc_net_tcp(TCP6));
        // The box user's own client.
        assert_eq!(owner_of(addr("127.0.0.1:41000"), &e), Some(1000));
        // Another account on the box (or a root container on the host network).
        assert_eq!(owner_of(addr("127.0.0.1:41002"), &e), Some(0));
        assert_eq!(owner_of(addr("127.0.0.1:41003"), &e), Some(1001));
        // IPv6, and an IPv4 peer that the kernel lists as mapped.
        assert_eq!(owner_of(addr("[::1]:43210"), &e), Some(1000));
        assert_eq!(owner_of(addr("127.0.0.1:43300"), &e), Some(0));
        // A connection in TIME_WAIT has no owner: it isn't the caller.
        assert_eq!(owner_of(addr("127.0.0.1:41004"), &e), None);
        // Nothing matches: no answer, so no trust.
        assert_eq!(owner_of(addr("127.0.0.1:1"), &e), None);
    }

    #[test]
    fn two_accounts_on_one_address_get_no_answer() {
        let e = vec![
            SocketEntry { local: addr("127.0.0.1:5000"), remote: addr("127.0.0.1:8765"), uid: 1000, inode: 1 },
            SocketEntry { local: addr("127.0.0.1:5000"), remote: addr("127.0.0.1:9000"), uid: 1001, inode: 2 },
        ];
        assert_eq!(owner_of(addr("127.0.0.1:5000"), &e), None);
    }

    #[test]
    fn a_real_connection_from_this_process_is_mine() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server, peer) = listener.accept().unwrap();
        assert_eq!(peer, client.local_addr().unwrap());
        assert!(peer_is_me(peer));
        drop(client);
    }
}
