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

/// The polkit rule granting the logind power actions (see
/// docs/reference/power-controls.md §5).
pub const POLKIT_RULE: &str = include_str!("../../../deploy/tt-station-power.rules");

/// What the phone can do with this machine's power right now, and the one-time
/// setup for what it can't (`GET /power/info`).
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct PowerReadiness {
    /// logind's answers for this agent: `yes`, `challenge` (needs a password, so
    /// not from a phone), `no` or `na`.
    pub can_power_off: String,
    pub can_reboot: String,
    pub can_suspend: String,
    /// polkit lets the agent power-manage the box with nobody logged in (the
    /// `*-multiple-sessions` logind actions -- what the tt-station rule grants).
    pub headless_power: bool,
    /// Whether this user's services (the agent) start at boot without anyone
    /// logging in -- needed for the box to be reachable after a power-on.
    pub starts_at_boot: bool,
    /// Wired interfaces, the only ones Wake-on-LAN can work on.
    pub wired: Vec<WiredNic>,
    /// Whether a phone can wake this box over the network, and what it needs to.
    pub wake_on_lan: WakeOnLan,
    /// Commands (run once, on the box, with sudo) that would fill the gaps.
    pub setup: Vec<SetupStep>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct WiredNic {
    pub name: String,
    pub mac: String,
    /// A cable is plugged in and the link is up.
    pub connected: bool,
    /// The kernel will let it wake the machine (`power/wakeup` is `enabled`).
    pub wake_enabled: bool,
    /// The device can wake the machine at all (it has a `power/wakeup` switch).
    pub wake_supported: bool,
}

/// Wake-on-LAN, summed up for a client deciding whether a Wake button can work
/// (`GET /power/info` → `wake_on_lan`).
///
/// Only a wired port can wake a computer: Wi-Fi cards are off while it sleeps.
/// The magic packet carries the wired port's MAC, which is not necessarily
/// `/status`'s `mac` (that one is whichever port has the default route, often
/// Wi-Fi), so a client stores this one.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq)]
pub struct WakeOnLan {
    /// A wired port that can wake the machine, set to wake on a magic packet,
    /// with a cable in: a Wake button can work (from the same network).
    pub possible: bool,
    /// The wired port (`enp10s0`), when the box has one.
    pub nic: Option<String>,
    /// Its MAC: what a magic packet must carry.
    pub mac: Option<String>,
    /// It can wake the machine at all.
    pub supported: bool,
    /// It's set to wake on a magic packet: now (the kernel's switch) or at its
    /// next connection (NetworkManager's setting).
    pub enabled: bool,
    /// NetworkManager's connection says `wake-on-lan magic`, so it stays on
    /// across reboots and reconnects.
    pub persistent: bool,
    /// A cable is in and the link is up.
    pub connected: bool,
    /// The NetworkManager connection the setting belongs on, if there is one.
    pub connection: Option<String>,
    /// The broadcast address of each network the box is on (IPv4): where a
    /// client on the same network sends the packet.
    pub broadcast: Vec<String>,
    /// Why waking can't work yet, in plain words (`None` when it can).
    pub why_not: Option<String>,
    /// The one-time command (on the box, with sudo) that turns it on, while it's off.
    pub enable_command: Option<String>,
}

/// One NetworkManager connection, as `nmcli -t -f NAME,TYPE,DEVICE connection show` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct NmConnection {
    pub name: String,
    pub kind: String,
    /// The device it's active on; empty when it isn't active.
    pub device: String,
}

/// Parse `nmcli -t -f NAME,TYPE,DEVICE connection show`: `:`-separated, with
/// `\:` and `\\` escaped inside a field.
pub fn parse_nm_connections(out: &str) -> Vec<NmConnection> {
    out.lines()
        .filter_map(|line| {
            let mut fields = Vec::new();
            let mut cur = String::new();
            let mut chars = line.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => cur.extend(chars.next()),
                    ':' => fields.push(std::mem::take(&mut cur)),
                    _ => cur.push(c),
                }
            }
            fields.push(cur);
            (fields.len() == 3 && !fields[0].is_empty()).then(|| NmConnection {
                name: fields[0].clone(),
                kind: fields[1].clone(),
                device: fields[2].clone(),
            })
        })
        .collect()
}

/// The wired connection for `nic`: the one active on it, else an inactive
/// ethernet connection bound to it (or to no interface at all, which
/// NetworkManager uses for whichever port gets a cable). `bound_to` answers a
/// connection's `connection.interface-name` (empty when it has none).
pub fn pick_connection(
    conns: &[NmConnection],
    nic: &str,
    bound_to: impl Fn(&str) -> String,
) -> Option<String> {
    if let Some(c) = conns.iter().find(|c| c.device == nic) {
        return Some(c.name.clone());
    }
    conns
        .iter()
        .filter(|c| c.kind == "802-3-ethernet" && c.device.is_empty())
        .find(|c| {
            let bound = bound_to(&c.name);
            bound.is_empty() || bound == nic
        })
        .map(|c| c.name.clone())
}

/// The IPv4 broadcast addresses of the box's real networks, from
/// `ip -j -4 addr show` (Docker bridges, VPNs and loopback left out: nothing
/// on them can wake the box).
pub fn lan_broadcasts(ip_json: &str) -> Vec<String> {
    const VIRTUAL: [&str; 9] = [
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "tailscale",
        "wg",
        "tun",
        "zt",
    ];
    let Ok(serde_json::Value::Array(ifaces)) = serde_json::from_str(ip_json) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for iface in &ifaces {
        let name = iface["ifname"].as_str().unwrap_or("");
        if VIRTUAL.iter().any(|v| name.starts_with(v)) {
            continue;
        }
        for addr in iface["addr_info"].as_array().into_iter().flatten() {
            if let Some(b) = addr["broadcast"].as_str() {
                if !out.iter().any(|o| o == b) {
                    out.push(b.to_string());
                }
            }
        }
    }
    out
}

/// What the box knows about waking it, from facts already gathered: the wired
/// ports, the NetworkManager connection for the first one (and whether it says
/// `magic`), and the box's broadcast addresses.
pub fn wake_on_lan(
    wired: &[WiredNic],
    connection: Option<String>,
    persistent: bool,
    broadcast: Vec<String>,
) -> WakeOnLan {
    // The port with a cable in, else the first: the one to talk about.
    let Some(nic) = wired.iter().find(|n| n.connected).or(wired.first()) else {
        return WakeOnLan {
            broadcast,
            why_not: Some(
                "This box has no wired network port, and Wi-Fi can't wake a sleeping or powered-off computer."
                    .to_string(),
            ),
            ..WakeOnLan::default()
        };
    };
    let enabled = nic.wake_enabled || persistent;
    let enable_command = (nic.wake_supported && !persistent).then(|| match &connection {
        Some(c) => format!(
            "sudo nmcli connection modify {} 802-3-ethernet.wake-on-lan magic",
            shell_quote(c)
        ),
        // No NetworkManager connection to keep it in: on until the next reboot.
        None => format!("sudo ethtool -s {} wol g", nic.name),
    });
    let why_not = if !nic.wake_supported {
        Some(format!("{} can't wake this computer.", nic.name))
    } else if !nic.connected && !enabled {
        Some(format!(
            "Plug a network cable into {} and turn on Wake-on-LAN: Wi-Fi can't wake a sleeping or powered-off computer.",
            nic.name
        ))
    } else if !nic.connected {
        Some(format!(
            "Plug a network cable into {}: Wi-Fi can't wake a sleeping or powered-off computer.",
            nic.name
        ))
    } else if !enabled {
        Some(format!("Wake-on-LAN is off on {}.", nic.name))
    } else {
        None
    };
    WakeOnLan {
        possible: why_not.is_none(),
        nic: Some(nic.name.clone()),
        mac: Some(nic.mac.clone()).filter(|m| !m.is_empty()),
        supported: nic.wake_supported,
        enabled,
        persistent,
        connected: nic.connected,
        connection,
        broadcast,
        why_not,
        enable_command,
    }
}

/// `s`, quoted for a POSIX shell when it needs to be.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@%+=:,/".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// What turning Wake-on-LAN on did (`POST /power/wake-on-lan`).
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct WolEnable {
    /// The setting is on now (or already was).
    pub applied: bool,
    /// In plain words: what happened, or why the owner has to run `command`.
    pub message: String,
    /// The command to run once on the box, when the agent couldn't.
    pub command: Option<String>,
}

/// Turn Wake-on-LAN on for good: NetworkManager's `802-3-ethernet.wake-on-lan
/// magic` on the wired port's connection, which it applies whenever that
/// connection comes up. Runs `nmcli` as the agent's own account, so it works
/// only where polkit already lets that account change network settings
/// (no password prompt can reach a phone); otherwise it hands back the exact
/// command for the owner. `run` executes a command (argv) and returns its
/// output or its error. `Err` is a refusal (`409`): there is nothing to turn on.
pub fn enable_wake_on_lan(
    wol: &WakeOnLan,
    run: &dyn Fn(&[&str]) -> Result<String, String>,
) -> Result<WolEnable, String> {
    let Some(nic) = wol.nic.as_deref() else {
        return Err(wol
            .why_not
            .clone()
            .unwrap_or_else(|| "This box has no wired network port.".to_string()));
    };
    if !wol.supported {
        return Err(format!("{nic} can't wake this computer."));
    }
    let cable = if wol.connected {
        ""
    } else {
        " It takes effect once a network cable is plugged in."
    };
    if wol.persistent {
        return Ok(WolEnable {
            applied: true,
            message: format!("Wake-on-LAN is already on for {nic}.{cable}"),
            command: None,
        });
    }
    let Some(conn) = wol.connection.as_deref() else {
        return Ok(WolEnable {
            applied: false,
            message: format!(
                "NetworkManager doesn't manage {nic}, so the box can't keep the setting by itself. Run this on the box (it lasts until the next restart), or set Wake-on-LAN in your network configuration:"
            ),
            command: wol.enable_command.clone(),
        });
    };
    match run(&[
        "nmcli",
        "--wait",
        "15",
        "connection",
        "modify",
        conn,
        "802-3-ethernet.wake-on-lan",
        "magic",
    ]) {
        Ok(_) => {
            if wol.connected {
                // Apply it to the live link too; the next connection would anyway.
                let _ = run(&["nmcli", "--wait", "15", "device", "reapply", nic]);
            }
            Ok(WolEnable {
                applied: true,
                message: format!("Wake-on-LAN is on for {nic}, and stays on.{cable}"),
                command: None,
            })
        }
        Err(e) => Ok(WolEnable {
            applied: false,
            message: format!(
                "The box isn't allowed to change its network settings by itself ({}). Run this once on the box:",
                e.trim()
            ),
            command: wol.enable_command.clone(),
        }),
    }
}

/// Run `argv` with no stdin (so nothing can prompt), returning stdout or a
/// one-line error.
pub fn run_quiet(argv: &[&str]) -> Result<String, String> {
    let (bin, args) = argv.split_first().ok_or("no command")?;
    let out = std::process::Command::new(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        Err(line.trim().to_string())
    }
}

/// The NetworkManager facts for the wired port `nic`: its connection's name,
/// and whether that connection says `wake-on-lan magic`. `(None, false)` where
/// NetworkManager isn't running. Read-only.
pub fn nm_wake_setting(nic: &str) -> (Option<String>, bool) {
    let Ok(list) = run_quiet(&[
        "nmcli",
        "-t",
        "-f",
        "NAME,TYPE,DEVICE",
        "connection",
        "show",
    ]) else {
        return (None, false);
    };
    let field = |conn: &str, field: &str| {
        run_quiet(&["nmcli", "-g", field, "connection", "show", conn])
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let conn = pick_connection(&parse_nm_connections(&list), nic, |c| {
        field(c, "connection.interface-name")
    });
    let persistent = conn
        .as_deref()
        .is_some_and(|c| field(c, "802-3-ethernet.wake-on-lan").contains("magic"));
    (conn, persistent)
}

/// The facts behind [`WakeOnLan`], gathered from NetworkManager and `ip`. Read-only.
pub fn wake_on_lan_blocking(wired: &[WiredNic]) -> WakeOnLan {
    let nic = wired.iter().find(|n| n.connected).or(wired.first());
    let (connection, persistent) = nic
        .map(|n| nm_wake_setting(&n.name))
        .unwrap_or((None, false));
    let broadcast = run_quiet(&["ip", "-j", "-4", "addr", "show"])
        .map(|s| lan_broadcasts(&s))
        .unwrap_or_default();
    wake_on_lan(wired, connection, persistent, broadcast)
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct SetupStep {
    pub why: String,
    pub command: String,
}

/// Parse `busctl call ... CanPowerOff` output (`s "yes"`).
pub fn parse_busctl_string(out: &str) -> String {
    out.trim()
        .strip_prefix("s ")
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// The box's physical, non-wireless network ports, from sysfs. Blocking (cheap).
pub fn wired_nics_blocking() -> Vec<WiredNic> {
    std::fs::read_dir("/sys/class/net")
        .map(|it| {
            let mut nics: Vec<WiredNic> = it
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let base = format!("/sys/class/net/{name}");
                    // Physical (has a device), not wireless, not a bridge/veth.
                    let physical = std::path::Path::new(&format!("{base}/device")).exists();
                    let wireless = std::path::Path::new(&format!("{base}/wireless")).exists();
                    let wakeup = read_trim(&format!("{base}/device/power/wakeup"));
                    (physical && !wireless).then(|| WiredNic {
                        mac: read_trim(&format!("{base}/address")).unwrap_or_default(),
                        connected: read_trim(&format!("{base}/carrier")).as_deref() == Some("1"),
                        wake_enabled: wakeup.as_deref() == Some("enabled"),
                        wake_supported: wakeup.is_some(),
                        name,
                    })
                })
                .collect();
            nics.sort_by(|a, b| a.name.cmp(&b.name));
            nics
        })
        .unwrap_or_default()
}

/// The readiness report, from logind, `loginctl` and sysfs. Blocking.
pub fn readiness_blocking(user: &str, config_dir: &std::path::Path) -> PowerReadiness {
    let logind = |method: &str| {
        std::process::Command::new("busctl")
            .args([
                "call",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                method,
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_busctl_string(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_else(|| "unknown".to_string())
    };
    let pid = std::process::id().to_string();
    let headless_power = [
        "org.freedesktop.login1.power-off-multiple-sessions",
        "org.freedesktop.login1.reboot-multiple-sessions",
        "org.freedesktop.login1.suspend-multiple-sessions",
    ]
    .iter()
    .all(|action| {
        std::process::Command::new("pkcheck")
            .args(["--action-id", action, "--process", &pid])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    let starts_at_boot = std::process::Command::new("loginctl")
        .args(["show-user", user, "-p", "Linger"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes");
    let wired = wired_nics_blocking();
    let wake_on_lan = wake_on_lan_blocking(&wired);
    let mut readiness = PowerReadiness {
        can_power_off: logind("CanPowerOff"),
        can_reboot: logind("CanReboot"),
        can_suspend: logind("CanSuspend"),
        headless_power,
        starts_at_boot,
        wired,
        wake_on_lan,
        setup: Vec::new(),
    };
    // The polkit rule ships with the agent; stage a copy where the setup
    // command can point at it.
    let rule_path = config_dir.join("tt-station-power.rules");
    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::write(&rule_path, POLKIT_RULE);
    readiness.setup = setup_steps(&readiness, user, &rule_path.to_string_lossy());
    readiness
}

/// The one-time commands for whatever isn't ready.
pub fn setup_steps(r: &PowerReadiness, user: &str, rule_path: &str) -> Vec<SetupStep> {
    let mut steps = Vec::new();
    if !r.starts_at_boot {
        steps.push(SetupStep {
            why: "Start the agent at boot, so the box is reachable after it's turned on without anyone logging in.".to_string(),
            command: format!("sudo loginctl enable-linger {user}"),
        });
    }
    if !r.headless_power {
        steps.push(SetupStep {
            why: "Let the agent shut down, restart and sleep the box when nobody is logged in (only this account, from its user services)."
                .to_string(),
            // The rule grants the `tt-station-power` group: just the agent's account.
            command: format!(
                "sudo groupadd -f tt-station-power && sudo usermod -aG tt-station-power {user} && sudo install -m 644 {rule_path} /etc/polkit-1/rules.d/50-tt-station-power.rules"
            ),
        });
    }
    let wol = &r.wake_on_lan;
    if let Some(nic) = wol.nic.as_deref().filter(|_| wol.supported) {
        if let Some(command) = &wol.enable_command {
            steps.push(SetupStep {
                why: format!("Let {nic} wake the box (and keep it on across reboots)."),
                command: command.clone(),
            });
        }
        if !wol.connected {
            steps.push(SetupStep {
                why: format!(
                    "Waking the box over the network needs a cable: plug {nic} into your router (Wi-Fi can't wake a sleeping or powered-off computer)."
                ),
                command: String::new(),
            });
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_four_actions() {
        assert_eq!(
            PowerAction::parse("reset-chips"),
            Some(PowerAction::ResetChips)
        );
        assert_eq!(PowerAction::parse("suspend"), Some(PowerAction::Suspend));
        assert_eq!(PowerAction::parse("reboot"), Some(PowerAction::Reboot));
        assert_eq!(PowerAction::parse("shutdown"), Some(PowerAction::Shutdown));
        assert_eq!(PowerAction::parse("halt"), None);
        assert_eq!(PowerAction::parse(""), None);
    }

    fn wired_nic(connected: bool, wake: bool) -> WiredNic {
        WiredNic {
            name: "enp10s0".into(),
            mac: "9c:6b:00:ab:4b:83".into(),
            connected,
            wake_enabled: wake,
            wake_supported: true,
        }
    }

    /// `nic`: (cable in, kernel wake switch on); its NetworkManager connection
    /// is `netplan-en` and says `magic` exactly when the kernel switch is on.
    fn ready(power: &str, linger: bool, nic: Option<(bool, bool)>) -> PowerReadiness {
        let wired: Vec<WiredNic> = nic
            .map(|(connected, wake)| wired_nic(connected, wake))
            .into_iter()
            .collect();
        let persistent = nic.is_some_and(|(_, wake)| wake);
        PowerReadiness {
            can_power_off: power.to_string(),
            can_reboot: power.to_string(),
            can_suspend: power.to_string(),
            headless_power: power == "yes",
            starts_at_boot: linger,
            wake_on_lan: wake_on_lan(
                &wired,
                nic.map(|_| "netplan-en".to_string()),
                persistent,
                vec!["10.0.0.255".into()],
            ),
            wired,
            setup: vec![],
        }
    }

    #[test]
    fn setup_steps_cover_exactly_what_is_missing() {
        assert_eq!(parse_busctl_string("s \"yes\"\n"), "yes");
        assert_eq!(parse_busctl_string("garbage"), "unknown");

        let all_set = ready("yes", true, Some((true, true)));
        assert!(setup_steps(&all_set, "ttuser", "/r").is_empty());

        // This QuietBox as found: no linger, cable unplugged, Wake-on-LAN off.
        let mut found = ready("yes", false, Some((false, false)));
        found.headless_power = false; // no polkit rule yet
        let steps = setup_steps(&found, "ttuser", "/r");
        assert_eq!(steps.len(), 4, "{steps:?}");
        assert_eq!(steps[0].command, "sudo loginctl enable-linger ttuser");
        assert_eq!(
            steps[1].command,
            "sudo groupadd -f tt-station-power && sudo usermod -aG tt-station-power ttuser && sudo install -m 644 /r /etc/polkit-1/rules.d/50-tt-station-power.rules"
        );
        // Turning it on works with the cable still out: NetworkManager keeps
        // the setting and applies it when the connection comes up.
        assert_eq!(
            steps[2].command,
            "sudo nmcli connection modify netplan-en 802-3-ethernet.wake-on-lan magic"
        );
        assert!(steps[3].why.contains("plug enp10s0"));
        assert!(steps[3].command.is_empty());

        let cabled = ready("yes", true, Some((true, false)));
        let steps = setup_steps(&cabled, "ttuser", "/r");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].command.contains("wake-on-lan magic"));

        let challenge = ready("challenge", true, None);
        assert!(setup_steps(&challenge, "ttuser", "/r")[0]
            .command
            .contains("polkit"));
    }

    #[test]
    fn nmcli_terse_output_is_split_on_unescaped_colons() {
        let out = "netplan-en:802-3-ethernet:\nMontyC:802-11-wireless:wlp9s0\nOffice\\: 2F:802-3-ethernet:enp10s0\n\n";
        let conns = parse_nm_connections(out);
        assert_eq!(conns.len(), 3);
        assert_eq!(conns[0].name, "netplan-en");
        assert_eq!(conns[0].device, "");
        assert_eq!(conns[2].name, "Office: 2F");
        assert_eq!(conns[2].device, "enp10s0");
    }

    #[test]
    fn the_wired_connection_is_the_active_one_else_one_that_fits() {
        let conns = parse_nm_connections(
            "netplan-en:802-3-ethernet:\nlab:802-3-ethernet:\nMontyC:802-11-wireless:wlp9s0\n",
        );
        // Unplugged (as on this QuietBox): an inactive ethernet connection
        // bound to no interface (or to this one) is the one it would use.
        assert_eq!(
            pick_connection(&conns, "enp10s0", |_| String::new()).as_deref(),
            Some("netplan-en")
        );
        let bound = |c: &str| {
            if c == "netplan-en" {
                "eth9".into()
            } else {
                "enp10s0".into()
            }
        };
        assert_eq!(
            pick_connection(&conns, "enp10s0", bound).as_deref(),
            Some("lab")
        );
        let active = parse_nm_connections("wired:802-3-ethernet:enp10s0\n");
        assert_eq!(
            pick_connection(&active, "enp10s0", |_| "x".into()).as_deref(),
            Some("wired")
        );
        assert_eq!(pick_connection(&[], "enp10s0", |_| String::new()), None);
    }

    #[test]
    fn broadcasts_come_from_real_networks_only() {
        let ip = r#"[{"ifname":"lo","addr_info":[{"local":"127.0.0.1","prefixlen":8}]},
            {"ifname":"wlp9s0","addr_info":[{"local":"10.0.0.122","prefixlen":24,"broadcast":"10.0.0.255"}]},
            {"ifname":"docker0","addr_info":[{"local":"172.17.0.1","prefixlen":16,"broadcast":"172.17.255.255"}]},
            {"ifname":"br-9c378a7e0a16","addr_info":[{"local":"172.19.0.1","prefixlen":16,"broadcast":"172.19.255.255"}]},
            {"ifname":"enp10s0","addr_info":[{"local":"10.0.0.50","prefixlen":24,"broadcast":"10.0.0.255"}]}]"#;
        assert_eq!(lan_broadcasts(ip), vec!["10.0.0.255".to_string()]);
        assert!(lan_broadcasts("not json").is_empty());
    }

    #[test]
    fn wake_on_lan_says_plainly_what_is_missing() {
        let b = vec!["10.0.0.255".to_string()];
        // No wired port at all: Wi-Fi only.
        let wifi = wake_on_lan(&[], None, false, b.clone());
        assert!(!wifi.possible);
        assert!(wifi
            .why_not
            .as_deref()
            .unwrap()
            .contains("no wired network port"));
        assert_eq!(wifi.enable_command, None);

        // This QuietBox as found: unplugged, off.
        let found = wake_on_lan(
            &[wired_nic(false, false)],
            Some("netplan-en".into()),
            false,
            b.clone(),
        );
        assert!(!found.possible);
        assert_eq!(found.mac.as_deref(), Some("9c:6b:00:ab:4b:83"));
        assert!(found
            .why_not
            .as_deref()
            .unwrap()
            .contains("Plug a network cable into enp10s0 and turn on"));
        assert_eq!(
            found.enable_command.as_deref(),
            Some("sudo nmcli connection modify netplan-en 802-3-ethernet.wake-on-lan magic")
        );

        // Turned on but still unplugged: only the cable is missing.
        let on = wake_on_lan(
            &[wired_nic(false, false)],
            Some("netplan-en".into()),
            true,
            b.clone(),
        );
        assert!(on.enabled && on.persistent && !on.possible);
        assert_eq!(on.enable_command, None);
        assert!(on
            .why_not
            .as_deref()
            .unwrap()
            .starts_with("Plug a network cable into enp10s0:"));

        // Cabled and on: possible.
        let ready = wake_on_lan(
            &[wired_nic(true, true)],
            Some("Wired 1".into()),
            true,
            b.clone(),
        );
        assert!(ready.possible);
        assert_eq!(ready.why_not, None);
        assert_eq!(ready.broadcast, b);

        // Cabled, off, no NetworkManager: ethtool, which lasts until a restart.
        let bare = wake_on_lan(&[wired_nic(true, false)], None, false, vec![]);
        assert_eq!(
            bare.why_not.as_deref(),
            Some("Wake-on-LAN is off on enp10s0.")
        );
        assert_eq!(
            bare.enable_command.as_deref(),
            Some("sudo ethtool -s enp10s0 wol g")
        );

        // A connection name with a space is quoted for the shell.
        let spaced = wake_on_lan(
            &[wired_nic(true, false)],
            Some("Wired connection 1".into()),
            false,
            vec![],
        );
        assert_eq!(
            spaced.enable_command.as_deref(),
            Some("sudo nmcli connection modify 'Wired connection 1' 802-3-ethernet.wake-on-lan magic")
        );

        // A port that can't wake the machine: no command to offer.
        let mut cant = wired_nic(true, false);
        cant.wake_supported = false;
        let cant = wake_on_lan(&[cant], Some("w".into()), false, vec![]);
        assert_eq!(
            cant.why_not.as_deref(),
            Some("enp10s0 can't wake this computer.")
        );
        assert_eq!(cant.enable_command, None);
    }

    #[test]
    fn enabling_wake_on_lan_runs_nmcli_or_hands_back_the_command() {
        use std::cell::RefCell;
        let off = wake_on_lan(
            &[wired_nic(true, false)],
            Some("netplan-en".into()),
            false,
            vec![],
        );

        // polkit lets the agent's account change it: done, and applied to the live link.
        let ran = RefCell::new(Vec::<String>::new());
        let ok = |argv: &[&str]| {
            ran.borrow_mut().push(argv.join(" "));
            Ok(String::new())
        };
        let done = enable_wake_on_lan(&off, &ok).unwrap();
        assert!(done.applied, "{done:?}");
        assert_eq!(
            ran.borrow().as_slice(),
            [
                "nmcli --wait 15 connection modify netplan-en 802-3-ethernet.wake-on-lan magic",
                "nmcli --wait 15 device reapply enp10s0",
            ]
        );

        // It doesn't: the owner gets the exact command, and nothing pretends it worked.
        let denied = |_: &[&str]| {
            Err("Error: Failed to modify connection: Insufficient privileges".to_string())
        };
        let told = enable_wake_on_lan(&off, &denied).unwrap();
        assert!(!told.applied);
        assert!(told.message.contains("Insufficient privileges"));
        assert_eq!(
            told.command.as_deref(),
            Some("sudo nmcli connection modify netplan-en 802-3-ethernet.wake-on-lan magic")
        );

        // Unplugged: set now, takes effect with the cable; no reapply on a link that's down.
        let unplugged = wake_on_lan(
            &[wired_nic(false, false)],
            Some("netplan-en".into()),
            false,
            vec![],
        );
        let ran = RefCell::new(0);
        let count = |_: &[&str]| {
            *ran.borrow_mut() += 1;
            Ok(String::new())
        };
        let done = enable_wake_on_lan(&unplugged, &count).unwrap();
        assert!(done.applied && done.message.contains("once a network cable is plugged in"));
        assert_eq!(*ran.borrow(), 1);

        // Already on: nothing runs.
        let on = wake_on_lan(
            &[wired_nic(true, true)],
            Some("netplan-en".into()),
            true,
            vec![],
        );
        let never = |_: &[&str]| -> Result<String, String> { panic!("must not run") };
        assert!(enable_wake_on_lan(&on, &never).unwrap().applied);

        // No wired port: refused, nothing runs.
        let wifi = wake_on_lan(&[], None, false, vec![]);
        assert!(enable_wake_on_lan(&wifi, &never).is_err());

        // No NetworkManager connection: the (non-persistent) ethtool command.
        let bare = wake_on_lan(&[wired_nic(true, false)], None, false, vec![]);
        let told = enable_wake_on_lan(&bare, &never).unwrap();
        assert!(!told.applied);
        assert_eq!(
            told.command.as_deref(),
            Some("sudo ethtool -s enp10s0 wol g")
        );
    }

    #[test]
    fn the_power_rule_is_the_agents_account_only() {
        // Not every process of every sudo user: the agent's own group, from a
        // user manager (where the agent runs), for the six logind actions.
        assert!(!POLKIT_RULE.contains("isInGroup(\"sudo\")"));
        assert!(POLKIT_RULE.contains("subject.isInGroup(\"tt-station-power\")"));
        assert!(
            POLKIT_RULE.contains(r#"/^user[@-][0-9]+\.service$/.test(subject.system_unit || "")"#)
        );
        assert_eq!(
            POLKIT_RULE
                .matches("action.id == \"org.freedesktop.login1.")
                .count(),
            6
        );
        assert_eq!(POLKIT_RULE.matches("polkit.Result.YES").count(), 1);
    }

    #[test]
    fn only_reset_chips_is_not_a_machine_op() {
        assert!(!PowerAction::ResetChips.is_machine_op());
        assert!(PowerAction::Suspend.is_machine_op());
        assert!(PowerAction::Reboot.is_machine_op());
        assert!(PowerAction::Shutdown.is_machine_op());
    }
}
