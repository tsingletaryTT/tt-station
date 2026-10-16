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
//!
//! Running as the agent's user isn't enough on its own: a bring-up run, an
//! Orchard run or a Code terminal runs as that user too, and must not answer
//! its own approvals or call owner routes. Those run in systemd units whose
//! names say so (`crate::sandbox::CONFINED_UNIT_PREFIXES`), so the agent
//! reads which processes are in those units' cgroups and whether one of them
//! holds the caller's socket (`/proc/<pid>/fd/* -> socket:[<inode>]`). A run
//! can't move itself out of its cgroup: the cgroup files are read-only inside
//! its Landlock sandbox (`crate::sandbox`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

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
    for e in entries
        .iter()
        .filter(|e| e.inode != 0 && same(e.local, peer))
    {
        match uid {
            None => uid = Some(e.uid),
            Some(u) if u == e.uid => {}
            Some(_) => return None,
        }
    }
    uid
}

/// The inode of the live socket at `peer` (the caller's end), if one matches.
pub fn socket_inode(peer: SocketAddr, entries: &[SocketEntry]) -> Option<u64> {
    entries
        .iter()
        .find(|e| e.inode != 0 && same(e.local, peer))
        .map(|e| e.inode)
}

/// The cgroup folder of this account's systemd user manager
/// (`…/user@<uid>.service`), from `/proc/self/cgroup`'s text: the agent's own
/// unit sits under it. If the agent isn't under it (run by hand outside the
/// session), the usual place.
pub fn user_manager_cgroup(self_cgroup: &str, uid: u32, cgroup_fs: &Path) -> PathBuf {
    let manager = format!("user@{uid}.service");
    let path = self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("");
    let mut out = cgroup_fs.to_path_buf();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        out.push(part);
        if part == manager {
            return out;
        }
    }
    cgroup_fs.join(format!("user.slice/user-{uid}.slice/{manager}"))
}

/// Every pid in `dir`'s cgroup and the cgroups under it.
fn cgroup_pids(dir: &Path, out: &mut Vec<u32>) {
    if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(text.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            cgroup_pids(&e.path(), out);
        }
    }
}

/// The confined units (`crate::sandbox::is_confined_unit`) under `root`, a
/// user manager's cgroup, with the pids in each. Units sit in slices
/// (`app.slice/…`), so this looks a few folders down.
pub fn confined_units(root: &Path) -> Vec<(String, Vec<u32>)> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, Vec<u32>)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if crate::sandbox::is_confined_unit(&name) {
                let mut pids = Vec::new();
                cgroup_pids(&e.path(), &mut pids);
                out.push((name, pids));
            } else if depth > 0 && name.ends_with(".slice") {
                walk(&e.path(), depth - 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 3, &mut out);
    out
}

/// Whether process `pid` (under `proc_root`) has socket `inode` open.
fn holds_socket(proc_root: &Path, pid: u32, inode: u64) -> bool {
    let want = format!("socket:[{inode}]");
    std::fs::read_dir(proc_root.join(pid.to_string()).join("fd"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|t| t.as_os_str() == want.as_str()))
}

/// The confined unit one of whose processes holds socket `inode`, if any.
pub fn confined_holder(inode: u64, manager_cgroup: &Path, proc_root: &Path) -> Option<String> {
    confined_units(manager_cgroup)
        .into_iter()
        .find(|(_, pids)| pids.iter().any(|&pid| holds_socket(proc_root, pid, inode)))
        .map(|(unit, _)| unit)
}

/// Who is on the other end of a local connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peer {
    /// The agent's own account, outside any confined unit: the box itself.
    Me,
    /// The agent's account, but from inside this confined unit (a bring-up,
    /// Orchard or Code run): no local trust.
    Confined(String),
    /// Anyone else (another account, root, a container), or unknown.
    Other,
}

/// The agent's own uid.
pub fn my_uid() -> u32 {
    // SAFETY: getuid has no failure mode and touches no memory.
    unsafe { libc::getuid() }
}

/// Whether the connection from `peer` belongs to a process running as the
/// agent's own user.
pub fn peer_is_me(peer: SocketAddr) -> bool {
    peer_trust(peer) == Peer::Me
}

/// Who the connection from `peer` is ([`Peer`]).
#[cfg(target_os = "linux")]
pub fn peer_trust(peer: SocketAddr) -> Peer {
    let mut entries = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            entries.extend(parse_proc_net_tcp(&text));
        }
    }
    let uid = my_uid();
    if owner_of(peer, &entries) != Some(uid) {
        return Peer::Other;
    }
    let Some(inode) = socket_inode(peer, &entries) else {
        return Peer::Other;
    };
    let me = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let manager = user_manager_cgroup(&me, uid, Path::new("/sys/fs/cgroup"));
    match confined_holder(inode, &manager, Path::new("/proc")) {
        Some(unit) => Peer::Confined(unit),
        None => Peer::Me,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn peer_trust(_peer: SocketAddr) -> Peer {
    Peer::Other
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
            SocketEntry {
                local: addr("127.0.0.1:5000"),
                remote: addr("127.0.0.1:8765"),
                uid: 1000,
                inode: 1,
            },
            SocketEntry {
                local: addr("127.0.0.1:5000"),
                remote: addr("127.0.0.1:9000"),
                uid: 1001,
                inode: 2,
            },
        ];
        assert_eq!(owner_of(addr("127.0.0.1:5000"), &e), None);
    }

    /// A user manager's cgroup tree and a /proc with sockets, as fixtures:
    /// a bring-up unit (pid 4242, holding socket 912345, with a child cgroup
    /// holding 912346), a Code session in a nested slice (pid 6000, socket
    /// 600), and an ordinary terminal (pid 5151, socket 777).
    fn fixture() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        let cg = t
            .path()
            .join("cg/user.slice/user-1000.slice/user@1000.service");
        let unit = cg.join("app.slice/tt-codex-bringup-r1.service");
        std::fs::create_dir_all(unit.join("child")).unwrap();
        std::fs::write(unit.join("cgroup.procs"), "4242\n").unwrap();
        std::fs::write(unit.join("child/cgroup.procs"), "4243\n").unwrap();
        let code = cg.join("app.slice/tt.slice/tt-code-s1.service");
        std::fs::create_dir_all(&code).unwrap();
        std::fs::write(code.join("cgroup.procs"), "6000\n").unwrap();
        let term = cg.join("app.slice/app-gnome-terminal-1.scope");
        std::fs::create_dir_all(&term).unwrap();
        std::fs::write(term.join("cgroup.procs"), "5151\n").unwrap();
        let agent = cg.join("app.slice/tt-station-agentd.service");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("cgroup.procs"), "100\n").unwrap();
        for (pid, fd, target) in [
            (4242, 3, "socket:[912345]"),
            (4242, 4, "/dev/null"),
            (4243, 5, "socket:[912346]"),
            (6000, 9, "socket:[600]"),
            (5151, 7, "socket:[777]"),
            (100, 8, "socket:[1]"),
        ] {
            let dir = t.path().join(format!("proc/{pid}/fd"));
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(target, dir.join(fd.to_string())).unwrap();
        }
        t
    }

    #[test]
    fn finds_the_user_managers_cgroup() {
        let fs = Path::new("/sys/fs/cgroup");
        assert_eq!(
            user_manager_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/tt-station-agentd.service\n",
                1000,
                fs
            ),
            fs.join("user.slice/user-1000.slice/user@1000.service")
        );
        // Not under it (started by hand, or a system unit): the usual place.
        assert_eq!(
            user_manager_cgroup("0::/system.slice/x.service\n", 1000, fs),
            fs.join("user.slice/user-1000.slice/user@1000.service")
        );
    }

    #[test]
    fn a_socket_held_inside_a_confined_unit_is_named() {
        let t = fixture();
        let cg = t
            .path()
            .join("cg/user.slice/user-1000.slice/user@1000.service");
        let proc_root = t.path().join("proc");
        assert_eq!(
            confined_holder(912345, &cg, &proc_root).as_deref(),
            Some("tt-codex-bringup-r1.service")
        );
        // A child cgroup of the unit is still the unit.
        assert_eq!(
            confined_holder(912346, &cg, &proc_root).as_deref(),
            Some("tt-codex-bringup-r1.service")
        );
        // Units in nested slices too.
        assert_eq!(
            confined_holder(600, &cg, &proc_root).as_deref(),
            Some("tt-code-s1.service")
        );
        // An ordinary terminal and the agent's own unit: not confined.
        assert_eq!(confined_holder(777, &cg, &proc_root), None);
        assert_eq!(confined_holder(1, &cg, &proc_root), None);
        assert_eq!(confined_holder(424242, &cg, &proc_root), None);
    }

    #[test]
    fn the_socket_inode_of_a_peer() {
        let e = parse_proc_net_tcp(TCP);
        assert_eq!(socket_inode(addr("127.0.0.1:41000"), &e), Some(912345));
        assert_eq!(socket_inode(addr("127.0.0.1:41004"), &e), None, "TIME_WAIT");
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
