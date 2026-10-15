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
/// docs/reference/power-controls.md §6).
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
    /// Commands (run once, on the box, with sudo) that would fill the gaps.
    pub setup: Vec<SetupStep>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct WiredNic {
    pub name: String,
    pub mac: String,
    /// A cable is plugged in and the link is up.
    pub connected: bool,
    /// The kernel will let it wake the machine (`power/wakeup`).
    pub wake_enabled: bool,
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
    let wired: Vec<WiredNic> = std::fs::read_dir("/sys/class/net")
        .map(|it| {
            let mut nics: Vec<WiredNic> = it
                .flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let base = format!("/sys/class/net/{name}");
                    // Physical (has a device), not wireless, not a bridge/veth.
                    let physical = std::path::Path::new(&format!("{base}/device")).exists();
                    let wireless = std::path::Path::new(&format!("{base}/wireless")).exists();
                    (physical && !wireless).then(|| WiredNic {
                        mac: read_trim(&format!("{base}/address")).unwrap_or_default(),
                        connected: read_trim(&format!("{base}/carrier")).as_deref() == Some("1"),
                        wake_enabled: read_trim(&format!("{base}/device/power/wakeup")).as_deref()
                            == Some("enabled"),
                        name,
                    })
                })
                .collect();
            nics.sort_by(|a, b| a.name.cmp(&b.name));
            nics
        })
        .unwrap_or_default();
    let mut readiness = PowerReadiness {
        can_power_off: logind("CanPowerOff"),
        can_reboot: logind("CanReboot"),
        can_suspend: logind("CanSuspend"),
        headless_power,
        starts_at_boot,
        wired,
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
    match r.wired.first() {
        None => {}
        Some(nic) if !nic.connected => steps.push(SetupStep {
            why: format!(
                "Waking the box over the network needs a cable: plug {} into your router (Wi-Fi can't wake a sleeping or powered-off computer).",
                nic.name
            ),
            command: String::new(),
        }),
        Some(nic) if !nic.wake_enabled => steps.push(SetupStep {
            why: format!("Let {} wake the box (and keep it on across reboots).", nic.name),
            command: format!(
                "sudo nmcli connection modify \"$(nmcli -g GENERAL.CONNECTION device show {0})\" 802-3-ethernet.wake-on-lan magic && sudo ethtool -s {0} wol g",
                nic.name
            ),
        }),
        Some(_) => {}
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

    fn ready(power: &str, linger: bool, nic: Option<(bool, bool)>) -> PowerReadiness {
        PowerReadiness {
            can_power_off: power.to_string(),
            can_reboot: power.to_string(),
            can_suspend: power.to_string(),
            headless_power: power == "yes",
            starts_at_boot: linger,
            wired: nic
                .map(|(connected, wake)| WiredNic {
                    name: "enp10s0".into(),
                    mac: "9c:6b:00:ab:4b:83".into(),
                    connected,
                    wake_enabled: wake,
                })
                .into_iter()
                .collect(),
            setup: vec![],
        }
    }

    #[test]
    fn setup_steps_cover_exactly_what_is_missing() {
        assert_eq!(parse_busctl_string("s \"yes\"\n"), "yes");
        assert_eq!(parse_busctl_string("garbage"), "unknown");

        let all_set = ready("yes", true, Some((true, true)));
        assert!(setup_steps(&all_set, "ttuser", "/r").is_empty());

        // This QuietBox as found: no linger, cable unplugged.
        let mut found = ready("yes", false, Some((false, false)));
        found.headless_power = false; // no polkit rule yet
        let steps = setup_steps(&found, "ttuser", "/r");
        assert_eq!(steps.len(), 3, "{steps:?}");
        assert_eq!(steps[0].command, "sudo loginctl enable-linger ttuser");
        assert_eq!(
            steps[1].command,
            "sudo groupadd -f tt-station-power && sudo usermod -aG tt-station-power ttuser && sudo install -m 644 /r /etc/polkit-1/rules.d/50-tt-station-power.rules"
        );
        assert!(steps[2].why.contains("plug enp10s0"));

        let cabled = ready("yes", true, Some((true, false)));
        let steps = setup_steps(&cabled, "ttuser", "/r");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].command.contains("wol g"));

        let challenge = ready("challenge", true, None);
        assert!(setup_steps(&challenge, "ttuser", "/r")[0]
            .command
            .contains("polkit"));
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
