// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! What a bring-up run may reach on the box: the sandbox its systemd unit
//! starts in.
//!
//! A bring-up (Codex or Claude Code running fast-models-fast's stages) runs
//! commands as the box's own account, so without a sandbox it can read
//! everything that account can: TT Station's folder (paired devices' token
//! hashes, the local secret, app keys), SSH and GnuPG keys, the keyring. The
//! permission policy in `codex_bringup.py` asks a person before risky
//! commands, but that is a guard, not containment. Three layers, all of which
//! work without root:
//!
//! 1. **Unit properties** ([`unit_properties`]): only the ones a systemd user
//!    manager actually applies here. On Ubuntu 24.04 (systemd 255, AppArmor
//!    `kernel.apparmor_restrict_unprivileged_userns=1`) the user manager can
//!    make a user namespace (`PrivateUsers=yes`) but AppArmor refuses it
//!    CAP_SYS_ADMIN inside it, so every mount-namespace property
//!    (`ProtectHome=`, `ProtectSystem=`, `InaccessiblePaths=`,
//!    `ReadWritePaths=`, `TemporaryFileSystem=`, `PrivateTmp=`,
//!    `PrivateNetwork=`, `ProtectKernelTunables=`, `ProtectProc=`) is
//!    *silently skipped*, `IPAddressDeny=` (BPF) is skipped, and
//!    `ProtectClock=`/`ProtectKernelModules=`/`ProtectKernelLogs=`/
//!    `PrivateDevices=` fail the unit (status 218). What holds is
//!    seccomp, `NoNewPrivileges=`, `RestrictSUIDSGID=`, `RestrictNamespaces=`,
//!    `RestrictAddressFamilies=`, `KeyringMode=`, `UMask=` and
//!    `UnsetEnvironment=`. The filesystem properties are added too once
//!    [`mount_namespaces_work`] sees the user manager honour them (after an
//!    owner runs `deploy/contain-bringup.sh`).
//! 2. **Landlock** (`sandbox_exec.py`, [`Confinement::wrap`]): unprivileged,
//!    so it holds today. Everything is readable except the [`hidden`] paths;
//!    only the run's own folder, the repo, caches and the agents' own state are
//!    writable; signals, ptrace and `/proc/<pid>/environ` don't reach outside.
//! 3. **No local trust** (`crate::localpeer`): a process in one of these units
//!    reaching the agent over plain HTTP is not treated as the box's owner.
//!
//! What this can't stop as the same Unix account is in
//! `docs/reference/bringup-containment.md`: the docker socket, and the user's
//! systemd/D-Bus sockets (`systemd-run --user` starts a process outside the
//! sandbox). A separate account for runs closes those; that needs root.

use std::path::{Path, PathBuf};

/// The launcher every confined run starts through.
pub const LAUNCHER: &str = include_str!("sandbox_exec.py");

/// Unit-name prefixes of the processes that never get the box's own local
/// trust (`crate::localpeer::confined_unit`): bring-up runs, Orchard runs
/// and model servers it starts, and Code terminals.
pub const CONFINED_UNIT_PREFIXES: &[&str] = &[
    "tt-codex-bringup-",
    "tt-claude-bringup-",
    "tt-bringup-",
    "tt-orchard-",
    "tt-code-",
];

/// The systemd properties a run's unit always gets: the ones a user manager
/// applies without mount namespaces (seccomp, no_new_privs, the umask).
pub const ALWAYS: &[&str] = &[
    "NoNewPrivileges=yes",
    "RestrictSUIDSGID=yes",
    "RestrictNamespaces=yes",
    "LockPersonality=yes",
    "RestrictRealtime=yes",
    "SystemCallArchitectures=native",
    "SystemCallFilter=~@mount @swap @reboot @module @raw-io @clock @cpu-emulation @obsolete",
    "RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK",
    "KeyringMode=private",
    "UMask=0077",
    // Not containment (a run can name the sockets itself), but nothing it
    // starts finds the user's bus, systemd manager, SSH or GnuPG agent by
    // accident.
    "UnsetEnvironment=XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS SSH_AUTH_SOCK GPG_AGENT_INFO DISPLAY WAYLAND_DISPLAY",
];

/// What a run may and may not touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confinement {
    /// Unreadable (and unwritable): secrets.
    pub hidden: Vec<PathBuf>,
    /// The only writable paths.
    pub writable: Vec<PathBuf>,
    /// Files under a hidden folder the run still reads (its drivers).
    pub readable_files: Vec<PathBuf>,
}

/// The account's secrets a run never needs: TT Station's own folder, keys,
/// the keyring, other tools' credentials and browsers' cookies.
pub fn hidden(home: &Path, config_dir: &Path) -> Vec<PathBuf> {
    let mut v = vec![config_dir.to_path_buf()];
    v.extend(
        [
            ".ssh",
            ".gnupg",
            ".local/share/keyrings",
            ".password-store",
            ".config/gh",
            ".git-credentials",
            ".netrc",
            ".aws",
            ".kube",
            ".config/google-chrome",
            ".config/chromium",
            ".config/BraveSoftware",
            ".mozilla",
            ".config/Claude",
        ]
        .iter()
        .map(|p| home.join(p)),
    );
    v
}

impl Confinement {
    /// A bring-up stage: it writes its run folder, the tt-metal checkout,
    /// caches (Hugging Face, pip, tt-metal's kernel cache) and its engine's
    /// own state, plus `extra` (`TT_STATION_BRINGUP_RW`, colon-separated).
    pub fn for_bringup(
        home: &Path,
        config_dir: &Path,
        run_dir: &Path,
        repo: &Path,
        drivers: &[PathBuf],
        extra: &[PathBuf],
    ) -> Confinement {
        let mut writable = vec![run_dir.to_path_buf(), repo.to_path_buf()];
        writable.extend(
            [
                ".cache",
                ".triton",
                ".codex",
                ".claude",
                ".claude.json",
                ".claude.json.backup",
                ".local/share/claude",
                ".local/state",
                ".config/tenstorrent",
                ".config/vllm",
            ]
            .iter()
            .map(|p| home.join(p)),
        );
        if let Some(hf) = std::env::var_os("HF_HOME") {
            writable.push(PathBuf::from(hf));
        }
        writable.extend(["/tmp", "/var/tmp", "/dev"].iter().map(PathBuf::from));
        writable.extend(extra.iter().cloned());
        Confinement {
            hidden: hidden(home, config_dir),
            writable,
            readable_files: drivers.to_vec(),
        }
    }

    /// `python sandbox_exec.py --deny … --rw … --ro-file … --`: the argv
    /// that runs a command inside this confinement.
    pub fn wrap(&self, python: &Path, launcher: &Path) -> Vec<String> {
        let mut v = vec![python.display().to_string(), launcher.display().to_string()];
        for p in &self.hidden {
            v.extend(["--deny".into(), p.display().to_string()]);
        }
        for p in &self.writable {
            v.extend(["--rw".into(), p.display().to_string()]);
        }
        for p in &self.readable_files {
            v.extend(["--ro-file".into(), p.display().to_string()]);
        }
        v.push("--".into());
        v
    }

    /// `--property=…` arguments for `systemd-run`: [`ALWAYS`], and with a
    /// user manager that can make mount namespaces, the same confinement as
    /// mounts (a second, independent layer under Landlock).
    pub fn unit_properties(&self, mount_ns: bool) -> Vec<String> {
        let mut props: Vec<String> = ALWAYS.iter().map(|p| p.to_string()).collect();
        if mount_ns {
            props.push("PrivateUsers=yes".into());
            props.push("ProtectSystem=strict".into());
            props.push("ProtectKernelTunables=yes".into());
            props.push("ProtectControlGroups=yes".into());
            props.push("ProtectProc=invisible".into());
            // A folder holding readable files is replaced by an empty one and
            // the files bound back in; other hidden paths just vanish. `-`:
            // a path that doesn't exist is fine.
            for p in &self.hidden {
                let keep: Vec<&PathBuf> = self
                    .readable_files
                    .iter()
                    .chain(self.writable.iter())
                    .filter(|f| f.starts_with(p) && *f != p)
                    .collect();
                if keep.is_empty() {
                    props.push(format!("InaccessiblePaths=-{}", p.display()));
                } else {
                    props.push(format!("TemporaryFileSystem={}:ro", p.display()));
                }
            }
            for f in &self.readable_files {
                props.push(format!("BindReadOnlyPaths=-{}", f.display()));
            }
            for p in &self.writable {
                if self.hidden.iter().any(|h| p.starts_with(h)) {
                    props.push(format!("BindPaths=-{}", p.display()));
                } else {
                    props.push(format!("ReadWritePaths=-{}", p.display()));
                }
            }
        }
        props
            .into_iter()
            .map(|p| format!("--property={p}"))
            .collect()
    }
}

/// `TT_STATION_BRINGUP_SANDBOX=off` turns the Landlock layer off (a run that
/// needs something it blocks, while that's fixed). The unit properties and
/// the refusal of local trust stay.
pub fn landlock_disabled() -> bool {
    std::env::var("TT_STATION_BRINGUP_SANDBOX").is_ok_and(|v| v == "off" || v == "0")
}

/// `TT_STATION_BRINGUP_RW`: more writable paths, colon-separated.
pub fn extra_writable() -> Vec<PathBuf> {
    std::env::var_os("TT_STATION_BRINGUP_RW")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this box's systemd user manager really applies mount-namespace
/// properties (it says nothing when it skips them): a throwaway unit hides a
/// folder and reports what it saw. Asked once per agent.
pub fn mount_namespaces_work(scratch: &Path) -> bool {
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ANSWER.get_or_init(|| probe_mount_namespaces(scratch))
}

fn probe_mount_namespaces(scratch: &Path) -> bool {
    let dir = scratch.join(format!("ns-probe-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() || std::fs::write(dir.join("seen"), "").is_err() {
        return false;
    }
    let out = std::process::Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect", "--wait", "--pipe"])
        .arg("--property=PrivateUsers=yes")
        .arg(format!("--property=InaccessiblePaths={}", dir.display()))
        .args(["sh", "-c"])
        .arg(format!(
            "test -e '{}' && echo visible || echo hidden",
            dir.join("seen").display()
        ))
        .stdin(std::process::Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    out.is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "hidden")
}

/// Whether `unit` is one whose processes get no local trust.
pub fn is_confined_unit(unit: &str) -> bool {
    CONFINED_UNIT_PREFIXES.iter().any(|p| unit.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conf() -> Confinement {
        let home = Path::new("/home/u");
        let cfg = home.join(".config/tt-station");
        Confinement::for_bringup(
            home,
            &cfg,
            &cfg.join("bringup/codex/r1"),
            &home.join("tt-metal"),
            &[cfg.join("bringup/codex_bringup.py")],
            &[PathBuf::from("/data/models")],
        )
    }

    #[test]
    fn a_bringup_hides_the_agents_folder_and_keys_and_writes_only_its_own() {
        let c = conf();
        for p in [
            "/home/u/.config/tt-station",
            "/home/u/.ssh",
            "/home/u/.gnupg",
            "/home/u/.local/share/keyrings",
        ] {
            assert!(c.hidden.contains(&PathBuf::from(p)), "{p}");
        }
        for p in [
            "/home/u/.config/tt-station/bringup/codex/r1",
            "/home/u/tt-metal",
            "/home/u/.cache",
            "/home/u/.codex",
            "/home/u/.claude",
            "/tmp",
            "/dev",
            "/data/models",
        ] {
            assert!(c.writable.contains(&PathBuf::from(p)), "{p}");
        }
        // Never the home folder, TT Station's folder or the whole bring-up folder
        // (other runs' secrets and the approvals live there).
        for p in [
            "/home/u",
            "/home/u/.config/tt-station",
            "/home/u/.config/tt-station/bringup",
            "/home/u/.config",
        ] {
            assert!(!c.writable.contains(&PathBuf::from(p)), "{p}");
        }
    }

    #[test]
    fn the_launcher_argv_names_every_path() {
        let argv = conf().wrap(Path::new("/py"), Path::new("/l/sandbox_exec.py"));
        assert_eq!(argv[..2], ["/py", "/l/sandbox_exec.py"]);
        assert_eq!(argv.last().unwrap(), "--");
        let pairs: Vec<(String, String)> = argv[2..argv.len() - 1]
            .chunks(2)
            .map(|c| (c[0].clone(), c[1].clone()))
            .collect();
        assert!(pairs.contains(&("--deny".into(), "/home/u/.config/tt-station".into())));
        assert!(pairs.contains(&("--deny".into(), "/home/u/.ssh".into())));
        assert!(pairs.contains(&(
            "--rw".into(),
            "/home/u/.config/tt-station/bringup/codex/r1".into()
        )));
        assert!(pairs.contains(&(
            "--ro-file".into(),
            "/home/u/.config/tt-station/bringup/codex_bringup.py".into()
        )));
    }

    #[test]
    fn unit_properties_are_the_ones_a_user_manager_applies() {
        let props = conf().unit_properties(false);
        for p in [
            "--property=NoNewPrivileges=yes",
            "--property=RestrictSUIDSGID=yes",
            "--property=RestrictNamespaces=yes",
            "--property=KeyringMode=private",
        ] {
            assert!(props.contains(&p.to_string()), "{p}: {props:?}");
        }
        assert!(props.iter().any(|p| p.contains("SystemCallFilter=~@mount")));
        assert!(props
            .iter()
            .any(|p| p.starts_with("--property=UnsetEnvironment=")
                && p.contains("XDG_RUNTIME_DIR")
                && p.contains("DBUS_SESSION_BUS_ADDRESS")));
        // Ones that would fail the unit (status 218) or that are silently
        // skipped here are left out.
        for bad in [
            "ProtectClock",
            "ProtectKernelModules",
            "ProtectKernelLogs",
            "PrivateDevices",
            "DeviceAllow",
            "ProtectHome",
            "InaccessiblePaths",
            "PrivateUsers",
            "MemoryDenyWriteExecute",
        ] {
            assert!(!props.iter().any(|p| p.contains(bad)), "{bad}");
        }
    }

    #[test]
    fn with_mount_namespaces_the_paths_become_mounts_too() {
        let props = conf().unit_properties(true);
        let has = |p: &str| props.contains(&format!("--property={p}"));
        assert!(has("PrivateUsers=yes"));
        assert!(has("ProtectSystem=strict"));
        assert!(has("ProtectProc=invisible"));
        // TT Station's folder holds the run's own folder and drivers: an empty
        // folder over it, with those bound back in.
        assert!(has("TemporaryFileSystem=/home/u/.config/tt-station:ro"));
        assert!(has(
            "BindReadOnlyPaths=-/home/u/.config/tt-station/bringup/codex_bringup.py"
        ));
        assert!(has(
            "BindPaths=-/home/u/.config/tt-station/bringup/codex/r1"
        ));
        assert!(has("InaccessiblePaths=-/home/u/.ssh"));
        assert!(has("ReadWritePaths=-/home/u/tt-metal"));
        assert!(!has("InaccessiblePaths=-/home/u/.config/tt-station"));
        // /dev stays: the chips are /dev/tenstorrent (DeviceAllow= is root's).
        assert!(!props.iter().any(|p| p.contains("PrivateDevices")));
    }

    #[test]
    fn confined_units() {
        for u in [
            "tt-codex-bringup-abc.service",
            "tt-claude-bringup-abc.service",
            "tt-orchard-acme--tiny.service",
            "tt-code-123.service",
        ] {
            assert!(is_confined_unit(u), "{u}");
        }
        for u in [
            "tt-station-agentd.service",
            "app-gnome-terminal-1.scope",
            "run-u1.service",
        ] {
            assert!(!is_confined_unit(u), "{u}");
        }
    }
}
