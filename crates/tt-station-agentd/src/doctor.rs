// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! Box Doctor: a plain-language health check of everything the box depends on
//! -- the chips, their firmware and driver, cooling, disk, memory pages, Docker,
//! the serving model, stray containers, remote-power setup -- with a one-tap fix
//! where one is safe, a nightly run that alerts the owner, and a diagnostics
//! bundle to hand to Tenstorrent support.
//!
//! The checks are pure functions over [`Facts`] (gathered separately), so each
//! verdict is testable without hardware.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::http::StatusCode;
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

use crate::routes::{AppState, BearerAuth, ErrorResponse, Phase};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

/// A fix the agent can apply itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fix {
    /// Stop serving containers the agent doesn't own (they hold chips).
    StopStrayContainers,
    /// Stop the model and start it again.
    RestartModel,
    /// `tt-smi -r` (only while nothing is serving).
    ResetChips,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    pub id: &'static str,
    pub title: String,
    pub status: Status,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Fix>,
    /// A command for the owner to run on the box, when the fix isn't automatic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

impl Check {
    fn new(
        id: &'static str,
        title: impl Into<String>,
        status: Status,
        detail: impl Into<String>,
    ) -> Check {
        Check {
            id,
            title: title.into(),
            status,
            detail: detail.into(),
            fix: None,
            command: None,
        }
    }
    fn fix(mut self, fix: Fix) -> Check {
        self.fix = Some(fix);
        self
    }
    fn command(mut self, command: impl Into<String>) -> Check {
        self.command = Some(command.into());
        self
    }
}

/// One chip, as `tt-smi -s` reports it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chip {
    pub board_type: String,
    pub bus_id: String,
    pub temp_c: Option<f32>,
    pub trip_c: Option<f32>,
    pub firmware: String,
    pub pcie: String,
}

/// Everything the checks look at.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub expected_chips: usize,
    /// `None` when `tt-smi` couldn't be read at all.
    pub chips: Option<Vec<Chip>>,
    pub driver: Option<String>,
    pub disk_free_gb: Option<f64>,
    pub hugepages_1g: Option<u64>,
    pub docker_ok: bool,
    /// (phase, model, seconds in this phase)
    pub serving: Option<(Phase, Option<String>, u64)>,
    /// Whether the serving endpoint answered just now.
    pub serving_reachable: Option<bool>,
    /// Inference containers on ports the agent doesn't serve on.
    pub stray_containers: Vec<String>,
    pub starts_at_boot: Option<bool>,
    pub headless_power: Option<bool>,
    pub push_ready: Option<bool>,
    /// Starts that failed in the last day: (model, reason).
    pub recent_failures: Vec<(String, String)>,
    /// A newer `tt` CLI: (current, latest).
    pub tt_update: Option<(String, String)>,
    /// Old install copies tt no longer uses: (path, bytes).
    pub leftovers: Vec<(String, u64)>,
    /// How the box reaches the internet: (interface, is Wi-Fi, an Ethernet port with no cable).
    pub network: Option<(String, bool, Option<String>)>,
    /// The IOMMU translates DMA (rather than passing it through): some model
    /// packages can hang the chips' driver while pinning memory (tt-kmd #295).
    pub iommu_translated: Option<bool>,
    /// Programs the kernel saw stuck in the Tenstorrent driver since boot.
    pub driver_hangs: u32,
    /// A model start with no progress for this many seconds.
    pub stuck_start: Option<u64>,
    /// Set while a Lab run or an Orchard bring-up has the chips: tt-smi isn't
    /// read then, and their containers aren't strays.
    pub chips_lent_to: Option<String>,
}

/// Parse `tt-smi -s` JSON into chips and the driver version.
pub fn parse_tt_smi(json: &str) -> Option<(Vec<Chip>, Option<String>)> {
    let v: Value = serde_json::from_str(json).ok()?;
    let num = |x: &Value| -> Option<f32> {
        x.as_str()
            .and_then(|s| s.trim().parse().ok())
            .or_else(|| x.as_f64().map(|f| f as f32))
    };
    let chips = v["device_info"]
        .as_array()?
        .iter()
        .map(|d| Chip {
            board_type: d["board_info"]["board_type"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            bus_id: d["board_info"]["bus_id"].as_str().unwrap_or("").to_string(),
            temp_c: num(&d["telemetry"]["asic_temperature"]),
            trip_c: num(&d["limits"]["therm_trip_l1_limit"]),
            firmware: d["firmwares"]["fw_bundle_version"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            pcie: format!(
                "Gen{} x{}",
                d["board_info"]["pcie_speed"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into()),
                d["board_info"]["pcie_width"].as_str().unwrap_or("?")
            ),
        })
        .collect();
    let driver = v["host_info"]["Driver"].as_str().map(str::to_string);
    Some((chips, driver))
}

/// The verdicts.
pub fn checks(f: &Facts) -> Vec<Check> {
    let mut out = Vec::new();

    match &f.chips {
        _ if f.chips_lent_to.is_some() => out.push(Check::new(
            "chips",
            "Chips in use",
            Status::Ok,
            f.chips_lent_to.clone().unwrap_or_default(),
        )),
        None => out.push(
            Check::new("chips", "Chips not readable", Status::Fail,
                "tt-smi couldn't read the Tenstorrent chips. A reset usually clears this; if not, power the box off and on.")
                .fix(Fix::ResetChips),
        ),
        Some(chips) if chips.len() < f.expected_chips => out.push(
            Check::new("chips", format!("{} of {} chips found", chips.len(), f.expected_chips), Status::Fail,
                "Some chips aren't showing up. Try a chip reset; if they stay missing, shut down, check the cards are seated, and contact support.")
                .fix(Fix::ResetChips),
        ),
        Some(chips) => out.push(Check::new(
            "chips",
            format!("{} chips found", chips.len()),
            Status::Ok,
            format!(
                "{} · {}",
                chips.iter().map(|c| c.board_type.as_str()).collect::<Vec<_>>().join(", "),
                chips.first().map(|c| c.pcie.as_str()).unwrap_or("")
            ),
        )),
    }

    if let Some(chips) = &f.chips {
        let temps: Vec<(f32, f32)> = chips
            .iter()
            .filter_map(|c| Some((c.temp_c?, c.trip_c.unwrap_or(90.0))))
            .collect();
        if let Some(&(hot, trip)) = temps.iter().max_by(|a, b| a.0.total_cmp(&b.0)) {
            let (status, detail) = if hot >= trip {
                (Status::Fail, format!("The hottest chip is at {hot:.0} °C, at its {trip:.0} °C limit: it is throttling. Check the box's vents and fans."))
            } else if hot >= trip - 10.0 {
                (Status::Warn, format!("The hottest chip is at {hot:.0} °C, close to its {trip:.0} °C limit. Give the box more airflow."))
            } else {
                (
                    Status::Ok,
                    format!("Hottest chip {hot:.0} °C (limit {trip:.0} °C)."),
                )
            };
            out.push(Check::new("cooling", "Cooling", status, detail));
        }

        let mut versions: Vec<&str> = chips
            .iter()
            .map(|c| c.firmware.as_str())
            .filter(|v| !v.is_empty())
            .collect();
        versions.sort_unstable();
        versions.dedup();
        match versions.as_slice() {
            [] => {}
            [one] => out.push(Check::new(
                "firmware",
                format!("Firmware {one}"),
                Status::Ok,
                format!("All chips match. Driver {}.", f.driver.as_deref().unwrap_or("unknown")),
            )),
            many => out.push(
                Check::new("firmware", "Mixed firmware", Status::Warn,
                    format!("The chips run different firmware ({}). Update them together so the cards behave alike.", many.join(", ")))
                    .command("tt-flash --fw-tar <fw_pack.tar.gz>  # with the latest tt-system-firmware bundle"),
            ),
        }
    }

    if let Some(free) = f.disk_free_gb {
        let status = if free < 30.0 {
            Status::Fail
        } else if free < 120.0 {
            Status::Warn
        } else {
            Status::Ok
        };
        out.push(Check::new(
            "disk",
            format!("{free:.0} GB free"),
            status,
            match status {
                Status::Ok => "Room for several more models.".to_string(),
                _ => "Models are 20-140 GB each. Delete ones you don't use (Models tab) before downloading more.".to_string(),
            },
        ));
    }

    if let Some(pages) = f.hugepages_1g {
        out.push(if pages == 0 {
            Check::new(
                "hugepages",
                "No 1 GB memory pages",
                Status::Fail,
                "Models need 1 GB huge pages reserved at boot. Re-run the Tenstorrent host setup.",
            )
            .command("tt-installer  # or: sudo ./setup-hugepages.sh from tt-system-tools")
        } else {
            Check::new(
                "hugepages",
                format!("{pages} × 1 GB memory pages"),
                Status::Ok,
                "Reserved for the chips.",
            )
        });
    }

    out.push(if f.docker_ok {
        Check::new(
            "docker",
            "Docker running",
            Status::Ok,
            "Models run in containers.",
        )
    } else {
        Check::new(
            "docker",
            "Docker isn't answering",
            Status::Fail,
            "Models run in Docker containers; start it.",
        )
        .command("sudo systemctl restart docker")
    });

    if let Some((phase, model, secs)) = &f.serving {
        let display = model.as_deref().map(crate::notify::display_model);
        let name = display.as_deref().unwrap_or("The model");
        match phase {
            Phase::Serving => out.push(match f.serving_reachable {
                Some(false) => Check::new("serving", format!("{name} isn't answering"), Status::Fail,
                    "The model is supposed to be serving but its server didn't respond. Restarting it usually fixes this.")
                    .fix(Fix::RestartModel),
                _ => Check::new("serving", format!("{name} is serving"), Status::Ok, "Answering requests."),
            }),
            Phase::Starting if *secs > 45 * 60 => out.push(
                Check::new("serving", format!("{name} has been loading for {} min", secs / 60), Status::Warn,
                    "That's longer than any model should take. Restart it.")
                    .fix(Fix::RestartModel),
            ),
            Phase::Failed => out.push(Check::new("serving", format!("{name} failed to start"), Status::Warn,
                "See Notifications or the serving log for why; starting it again often works.")),
            _ => {}
        }
    }

    if !f.stray_containers.is_empty() && f.chips_lent_to.is_none() {
        out.push(
            Check::new(
                "stray",
                format!("{} leftover model server(s)", f.stray_containers.len()),
                Status::Warn,
                format!(
                    "Containers the box didn't start are holding chips: {}.",
                    f.stray_containers.join(", ")
                ),
            )
            .fix(Fix::StopStrayContainers),
        );
    }

    for (model, reason) in f.recent_failures.iter().take(3) {
        out.push(Check::new(
            "recent_failure",
            format!("{model} failed today"),
            Status::Info,
            reason.clone(),
        ));
    }

    if f.starts_at_boot == Some(false) {
        out.push(
            Check::new(
                "boot",
                "Not reachable after a restart until someone logs in",
                Status::Warn,
                "Let the box's agent start at boot.",
            )
            .command(format!(
                "sudo loginctl enable-linger {}",
                std::env::var("USER").unwrap_or_else(|_| "ttuser".into())
            )),
        );
    }
    if let Some((current, latest)) = &f.tt_update {
        out.push(
            Check::new(
                "software",
                format!("tt {latest} is available"),
                Status::Info,
                format!("This box has tt {current}. See Box → Software."),
            )
            .command("tt self update"),
        );
    }
    if !f.leftovers.is_empty() {
        let total: u64 = f.leftovers.iter().map(|(_, b)| b).sum();
        out.push(
            Check::new(
                "leftovers",
                format!("{:.1} GB of old installs", total as f64 / 1e9),
                Status::Info,
                "Copies from an earlier tt-installer run that tt no longer uses.",
            )
            .command(format!(
                "rm -rf {}",
                f.leftovers
                    .iter()
                    .map(|(p, _)| p.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        );
    }
    if let Some(secs) = f.stuck_start {
        out.push(Check::new(
            "stuck_start",
            "A model start looks stuck",
            Status::Warn,
            format!("No progress for {} minutes. Stop it in Models (its chips are reset); after 20 minutes TT Station does that itself and puts the last model that worked back.", secs / 60),
        ));
    }
    if f.driver_hangs > 0 {
        out.push(Check::new(
            "driver_hang",
            "The chips' driver got stuck",
            Status::Warn,
            format!("{} program{} got stuck in the Tenstorrent driver since the box started (the kernel log says so). If chip readings or model starts hang again, restart the box.",
                f.driver_hangs, if f.driver_hangs == 1 { "" } else { "s" }),
        ));
    }
    if f.iommu_translated == Some(true) {
        out.push(
            Check::new(
                "iommu",
                "IOMMU is translating",
                Status::Info,
                "Some model packages hang the chips' driver while pinning large buffers in this mode (tt-kmd #295; seen here with a Qwen3.8 Flash Next package). Passthrough avoids it: add iommu=pt to the kernel command line and restart. It's a system change, so it's yours to make.",
            )
            .command("sudo sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=\"/&iommu=pt /' /etc/default/grub && sudo update-grub   # then restart"),
        );
    }
    if let Some((dev, wifi, unplugged)) = &f.network {
        if *wifi {
            out.push(Check::new(
                "network",
                "On Wi-Fi",
                Status::Info,
                match unplugged {
                    Some(port) => format!("The box reaches the internet over Wi-Fi ({dev}); its Ethernet port ({port}) has no cable. Model downloads (often 50-400 GB) go two to three times faster over Ethernet."),
                    None => format!("The box reaches the internet over Wi-Fi ({dev}). Model downloads (often 50-400 GB) go faster over Ethernet."),
                },
            ));
        }
    }
    if f.headless_power == Some(false) {
        out.push(Check::new(
            "power",
            "Remote power off/sleep needs setup",
            Status::Info,
            "See Box → Power for the one-time command.",
        ));
    }
    if f.push_ready == Some(false) {
        out.push(Check::new(
            "push",
            "Push notifications not set up",
            Status::Info,
            "The box needs the app's push key; until then the app checks for news itself.",
        ));
    }
    out
}

/// The overall verdict: the worst status.
pub fn overall(checks: &[Check]) -> Status {
    checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok)
}

fn disk_free_gb(path: &Path) -> Option<f64> {
    let out = std::process::Command::new("df")
        .args(["-B1", "--output=avail"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse::<f64>()
        .ok()
        .map(|b| b / 1e9)
}

/// Gather the facts (blocking parts on a worker thread).
pub async fn gather(state: &AppState) -> Facts {
    let snapshot = tokio::time::timeout(Duration::from_secs(20), state.cached_snapshot())
        .await
        .ok()
        .and_then(Result::ok);
    let parsed = snapshot.as_deref().and_then(parse_tt_smi);
    let expected_chips = state
        .chips()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4);

    let life = state.lifecycle();
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .saturating_sub(life.since);
    let serving = Some((
        life.phase,
        life.model.clone().or(life.served_id.clone()),
        since,
    ));
    let serving_reachable = match (life.phase, state.endpoint()) {
        // Up, but the liveness watcher's one-token checks keep missing.
        (Phase::Serving, Some(_)) if state.liveness().is_degraded() => Some(false),
        (Phase::Serving, Some(endpoint)) => {
            let base = endpoint
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .to_string();
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .build()
                .ok();
            let mut ok = false;
            if let Some(client) = client {
                for path in ["/v1/models", "/tt-liveness", "/health"] {
                    if client
                        .get(format!("{base}{path}"))
                        .send()
                        .await
                        .is_ok_and(|r| r.status().is_success())
                    {
                        ok = true;
                        break;
                    }
                }
            }
            Some(ok)
        }
        _ => None,
    };

    let our_ports: Vec<u16> = {
        let mut ports = vec![];
        if let Some(e) = state.endpoint() {
            if let Ok(u) = reqwest::Url::parse(&e.base_url) {
                ports.extend(u.port());
            }
        }
        if let Some(e) = state.companion().and_then(|c| c.endpoint()) {
            if let Ok(u) = reqwest::Url::parse(&e.base_url) {
                ports.extend(u.port());
            }
        }
        ports
    };
    let push_ready = Some(state.notifier().push_status().0);
    let recent_failures: Vec<(String, String)> = {
        let day_ago = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(86_400);
        state
            .notifier()
            .since(0)
            .into_iter()
            .filter(|e| e.at >= day_ago && e.kind.ends_with("_failed"))
            .map(|e| {
                (
                    e.title
                        .replace(" couldn't start", "")
                        .replace("Download of ", "")
                        .replace(" failed", ""),
                    e.body,
                )
            })
            .collect()
    };

    let tt_cli = state.tt_cli();
    let (tt_update, leftovers) = tokio::task::spawn_blocking(move || {
        let update = tt_cli
            .and_then(|tt| tt.run_json(&["self", "check-update"]).ok())
            .filter(|v| v["newer"].as_bool() == Some(true))
            .map(|v| {
                (
                    v["current"].as_str().unwrap_or("").to_string(),
                    v["latest"].as_str().unwrap_or("").to_string(),
                )
            });
        let home = std::env::var("HOME").unwrap_or_default();
        let leftovers: Vec<(String, u64)> = ["tt-inference-server", "tt-studio"]
            .iter()
            .map(|n| PathBuf::from(&home).join(".local/lib").join(n))
            .filter(|p| p.is_dir())
            .map(|p| {
                let bytes = std::process::Command::new("du")
                    .arg("-sb")
                    .arg(&p)
                    .output()
                    .ok()
                    .and_then(|o| {
                        String::from_utf8_lossy(&o.stdout)
                            .split_whitespace()
                            .next()?
                            .parse()
                            .ok()
                    })
                    .unwrap_or(0);
                (p.display().to_string(), bytes)
            })
            .collect();
        (update, leftovers)
    })
    .await
    .unwrap_or((None, vec![]));

    let config_dir = state.config_dir();
    let hf_root = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"));
    let blocking = tokio::task::spawn_blocking(move || {
        let docker_ps = std::process::Command::new("docker")
            .args(["ps", "--format", "{{.ID}} {{.Image}} {{.Ports}}"])
            .stdin(std::process::Stdio::null())
            .output();
        let docker_ok = docker_ps.as_ref().is_ok_and(|o| o.status.success());
        let stray: Vec<String> = docker_ps
            .ok()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| {
                        l.contains("tt-inference-server") || l.contains("tt-media-inference-server")
                    })
                    .filter(|l| !our_ports.iter().any(|p| l.contains(&format!(":{p}->"))))
                    .filter_map(|l| l.split_whitespace().next().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let hugepages =
            std::fs::read_to_string("/sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages")
                .ok()
                .and_then(|s| s.trim().parse().ok());
        let readiness = crate::power::readiness_blocking(
            &std::env::var("USER").unwrap_or_else(|_| "ttuser".into()),
            &config_dir,
        );
        (
            docker_ok,
            stray,
            hugepages,
            disk_free_gb(&hf_root),
            readiness.starts_at_boot,
            readiness.headless_power,
        )
    })
    .await;
    let (docker_ok, stray_containers, hugepages_1g, disk_free_gb, boot, headless) =
        blocking.unwrap_or((false, vec![], None, None, false, false));
    let (network, iommu_translated, driver_hangs) = tokio::task::spawn_blocking(system_facts)
        .await
        .unwrap_or((None, None, 0));

    Facts {
        expected_chips,
        chips: parsed.as_ref().map(|(c, _)| c.clone()),
        driver: parsed.and_then(|(_, d)| d),
        disk_free_gb,
        hugepages_1g,
        docker_ok,
        serving,
        serving_reachable,
        stray_containers,
        starts_at_boot: Some(boot),
        headless_power: Some(headless),
        push_ready,
        recent_failures,
        tt_update,
        leftovers,
        chips_lent_to: state.chips_holder().map(str::to_string),
        network,
        iommu_translated,
        driver_hangs,
        stuck_start: crate::community::stalled_s(),
    }
}

/// The network route: its interface, whether it's Wi-Fi, and an unplugged wired port.
type Route = (String, bool, Option<String>);

/// The network route, the IOMMU mode and driver hangs, from the system itself.
fn system_facts() -> (Option<Route>, Option<bool>, u32) {
    use std::process::{Command, Stdio};
    let run = |cmd: &str, args: &[&str]| {
        Command::new(cmd)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    let route = run("ip", &["route", "get", "1.1.1.1"]);
    let dev = route
        .split_whitespace()
        .skip_while(|w| *w != "dev")
        .nth(1)
        .map(str::to_string);
    let network = dev.map(|dev| {
        let wifi = Path::new(&format!("/sys/class/net/{dev}/wireless")).exists();
        let unplugged = std::fs::read_dir("/sys/class/net")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("en") || n.starts_with("eth"))
            .find(|n| {
                std::fs::read_to_string(format!("/sys/class/net/{n}/carrier"))
                    .map(|c| c.trim() == "0")
                    .unwrap_or(true)
            });
        (dev, wifi, unplugged)
    });
    let kernel = run("journalctl", &["-k", "-b", "--no-pager", "-q"]);
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let iommu = if cmdline.contains("iommu=pt") {
        Some(false)
    } else if kernel.contains("Default domain type: Translated") {
        Some(true)
    } else if kernel.contains("Default domain type: Passthrough") {
        Some(false)
    } else {
        None
    };
    // "INFO: task tt-smi:45992 blocked for more than 122 seconds", with the driver
    // in its stack; counted once per stuck program.
    let mut stuck: std::collections::HashSet<String> = Default::default();
    let lines: Vec<&str> = kernel.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if let Some(rest) = l
            .split("INFO: task ")
            .nth(1)
            .filter(|_| l.contains("blocked for more than"))
        {
            let window = lines[i..lines.len().min(i + 40)].join("\n");
            if window.contains("[tenstorrent]") {
                stuck.insert(rest.split_whitespace().next().unwrap_or("").to_string());
            }
        }
    }
    (network, iommu, stuck.len() as u32)
}

/// Run the Doctor.
pub async fn examine(state: &AppState) -> (Status, Vec<Check>) {
    let facts = gather(state).await;
    let checks = checks(&facts);
    (overall(&checks), checks)
}

/// Every night at about 3:30, check the box and tell the owner about problems.
pub async fn nightly_forever(state: AppState) {
    let mut last_day = String::new();
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        let (day, hour, minute) = local_day_hm();
        if day == last_day || hour != 3 || minute < 30 {
            continue;
        }
        last_day = day;
        let (status, checks) = examine(&state).await;
        let problems: Vec<&Check> = checks.iter().filter(|c| c.status >= Status::Warn).collect();
        if status >= Status::Warn {
            state.notifier().emit(crate::notify::Draft {
                kind: "doctor",
                title: format!(
                    "Box Doctor found {} problem{}",
                    problems.len(),
                    if problems.len() == 1 { "" } else { "s" }
                ),
                body: problems
                    .iter()
                    .map(|c| c.title.as_str())
                    .collect::<Vec<_>>()
                    .join(" · "),
            });
        }
    }
}

fn local_day_hm() -> (String, u32, u32) {
    // SAFETY: localtime_r only writes into the provided struct.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        (
            format!("{}-{}-{}", tm.tm_year, tm.tm_mon, tm.tm_mday),
            tm.tm_hour as u32,
            tm.tm_min as u32,
        )
    }
}

// ---------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    (status, Json(ErrorResponse { error: msg.into() }))
}

/// `GET /doctor` (authed): run the checks.
pub(crate) async fn get_doctor(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Json<Value> {
    let (status, checks) = examine(&state).await;
    Json(json!({ "status": status, "checks": checks }))
}

/// `POST /doctor/fix/{fix}` (authed): apply a safe fix, then re-check.
pub(crate) async fn post_fix(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(fix): axum::extract::Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    if let Some(holder) = state.chips_holder() {
        return Err(err(StatusCode::CONFLICT, holder));
    }
    match fix.as_str() {
        "stop_stray_containers" => {
            let facts = gather(&state).await;
            let ids = facts.stray_containers.clone();
            tokio::task::spawn_blocking(move || {
                for id in ids {
                    let _ = std::process::Command::new("docker")
                        .args(["stop", &id])
                        .stdin(std::process::Stdio::null())
                        .output();
                }
            })
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        "restart_model" => {
            let life = state.lifecycle();
            let model = life
                .model
                .or(life.served_id)
                .ok_or_else(|| err(StatusCode::CONFLICT, "Nothing to restart."))?;
            state
                .stop_serving()
                .await
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            let bg = state.clone();
            tokio::spawn(async move {
                let _ = bg.start_serving(&model, false).await;
            });
        }
        "reset_chips" => {
            if state.lifecycle().phase != Phase::Idle {
                return Err(err(
                    StatusCode::CONFLICT,
                    "Stop the model before resetting the chips.",
                ));
            }
            let s = state.clone();
            tokio::task::spawn_blocking(move || {
                s.run_power_command(crate::power::PowerAction::ResetChips, false)
            })
            .await
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        }
        other => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                format!("No fix called {other}."),
            ))
        }
    }
    let (status, checks) = examine(&state).await;
    Ok(Json(
        json!({ "applied": fix, "status": status, "checks": checks }),
    ))
}

/// `POST /diagnostics` (authed): bundle what support needs -- the Doctor's
/// report, tt-smi, versions, recent events, the agent's log and the serving
/// logs (secrets redacted) -- into a .tar.gz, kept on the box. Answers its id;
/// `GET /diagnostics/{id}` downloads it.
pub(crate) async fn post_diagnostics(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
) -> Result<Json<Value>, (StatusCode, Json<ErrorResponse>)> {
    let (status, checks) = examine(&state).await;
    let snapshot = state.cached_snapshot().await.unwrap_or_default();
    let events = state.notifier().since(0);
    let lifecycle = state.lifecycle();
    let dir = state.config_dir().join("diagnostics");
    let id = format!(
        "ttdiag-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        &crate::pairing::issue_pair_id()[..6]
    );
    let name = state.name().to_string();
    let work = dir.join(&id);
    let built = tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
        std::fs::create_dir_all(&work)?;
        let write = |file: &str, text: String| std::fs::write(work.join(file), text);
        write(
            "doctor.json",
            serde_json::to_string_pretty(
                &json!({ "box": name, "status": status, "checks": checks, "lifecycle": lifecycle }),
            )
            .unwrap_or_default(),
        )?;
        write("tt-smi.json", snapshot)?;
        write(
            "events.json",
            serde_json::to_string_pretty(&events).unwrap_or_default(),
        )?;
        let run = |args: &[&str]| -> String {
            std::process::Command::new(args[0])
                .args(&args[1..])
                .stdin(std::process::Stdio::null())
                .output()
                .map(|o| {
                    format!(
                        "$ {}\n{}{}",
                        args.join(" "),
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                })
                .unwrap_or_else(|e| format!("$ {}\n{e}\n", args.join(" ")))
        };
        let redact = |text: String| {
            text.lines()
                .map(crate::logs::redact_line)
                .collect::<Vec<_>>()
                .join("\n")
        };
        write(
            "versions.txt",
            [
                run(&["uname", "-a"]),
                run(&["docker", "version", "--format", "{{.Server.Version}}"]),
                run(&[
                    "docker",
                    "ps",
                    "-a",
                    "--format",
                    "{{.ID}} {{.Image}} {{.Status}} {{.Ports}}",
                ]),
                run(&[
                    "sh",
                    "-c",
                    "tt --version 2>/dev/null || ~/.tenstorrent-venv/bin/tt --version",
                ]),
                run(&[
                    "sh",
                    "-c",
                    "cat /sys/kernel/mm/hugepages/hugepages-1048576kB/nr_hugepages",
                ]),
                run(&["df", "-h", "/"]),
            ]
            .join("\n"),
        )?;
        write(
            "agent.log",
            redact(run(&[
                "journalctl",
                "--user",
                "-u",
                "tt-station-agentd",
                "-n",
                "3000",
                "--no-pager",
            ])),
        )?;
        let containers = run(&["docker", "ps", "-q"]);
        let mut serving_logs = String::new();
        for id in containers.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            serving_logs.push_str(&run(&["docker", "logs", "--tail", "800", id.trim()]));
        }
        write("serving.log", redact(serving_logs))?;
        let archive = work.with_extension("tar.gz");
        let status = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(work.parent().unwrap_or(Path::new("/")))
            .arg(work.file_name().unwrap_or_default())
            .stdin(std::process::Stdio::null())
            .status()?;
        let _ = std::fs::remove_dir_all(&work);
        if !status.success() {
            return Err(std::io::Error::other("tar failed"));
        }
        Ok(std::fs::metadata(&archive)?.len())
    })
    .await
    .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("couldn't build the bundle: {e}"),
        )
    })?;
    Ok(Json(json!({ "id": id, "bytes": built })))
}

/// `GET /diagnostics/{id}` (authed): the bundle.
pub(crate) async fn get_diagnostics(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let valid =
        id.starts_with("ttdiag-") && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let path = state
        .config_dir()
        .join("diagnostics")
        .join(format!("{id}.tar.gz"));
    match (valid, std::fs::read(&path)) {
        (true, Ok(bytes)) => (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "application/gzip".to_string(),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{id}.tar.gz\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        _ => err(StatusCode::NOT_FOUND, "No such bundle.").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(temp: f32, fw: &str) -> Chip {
        Chip {
            board_type: "p300c".into(),
            bus_id: "0000:01:00.0".into(),
            temp_c: Some(temp),
            trip_c: Some(90.0),
            firmware: fw.into(),
            pcie: "Gen4 x4".into(),
        }
    }

    #[test]
    fn warns_about_this_weeks_problems() {
        let f = Facts {
            network: Some(("wlp9s0".into(), true, Some("enp10s0".into()))),
            iommu_translated: Some(true),
            driver_hangs: 2,
            stuck_start: Some(15 * 60),
            ..Default::default()
        };
        let c = checks(&f);
        let get = |id: &str| {
            c.iter()
                .find(|x| x.id == id)
                .unwrap_or_else(|| panic!("no {id} check"))
        };
        assert!(get("network").detail.contains("enp10s0"));
        assert_eq!(get("iommu").status, Status::Info);
        assert!(get("iommu")
            .command
            .as_deref()
            .unwrap_or("")
            .contains("iommu=pt"));
        assert_eq!(get("driver_hang").status, Status::Warn);
        assert!(get("stuck_start").detail.contains("15 minutes"));
        let wired = Facts {
            network: Some(("enp10s0".into(), false, None)),
            ..Default::default()
        };
        assert!(
            checks(&wired).iter().all(|x| x.id != "network"),
            "nothing to say about a wired box"
        );
    }

    fn healthy() -> Facts {
        Facts {
            expected_chips: 4,
            chips: Some(vec![chip(50.0, "19.13.1.0"); 4]),
            driver: Some("TT-KMD 2.11.0".into()),
            disk_free_gb: Some(2400.0),
            hugepages_1g: Some(16),
            docker_ok: true,
            serving: Some((Phase::Serving, Some("Qwen3-32B".into()), 600)),
            serving_reachable: Some(true),
            stray_containers: vec![],
            starts_at_boot: Some(true),
            headless_power: Some(true),
            push_ready: Some(true),
            recent_failures: vec![],
            tt_update: None,
            leftovers: vec![],
            chips_lent_to: None,
            network: None,
            iommu_translated: None,
            driver_hangs: 0,
            stuck_start: None,
        }
    }

    #[test]
    fn a_healthy_box_is_all_ok() {
        let c = checks(&healthy());
        assert_eq!(overall(&c), Status::Ok, "{c:#?}");
        assert!(c
            .iter()
            .any(|x| x.id == "firmware" && x.title == "Firmware 19.13.1.0"));
    }

    #[test]
    fn problems_get_the_right_verdict_and_fix() {
        let mut f = healthy();
        f.chips = Some(vec![chip(50.0, "19.13.1.0"); 3]);
        let c = checks(&f);
        let chips = c.iter().find(|x| x.id == "chips").unwrap();
        assert_eq!(
            (chips.status, chips.fix),
            (Status::Fail, Some(Fix::ResetChips))
        );

        let mut f = healthy();
        f.chips = Some(vec![chip(84.0, "19.13.1.0"), chip(50.0, "18.0.0")]);
        let c = checks(&f);
        assert_eq!(
            c.iter().find(|x| x.id == "cooling").unwrap().status,
            Status::Warn
        );
        assert_eq!(
            c.iter().find(|x| x.id == "firmware").unwrap().status,
            Status::Warn
        );

        let mut f = healthy();
        f.serving_reachable = Some(false);
        f.stray_containers = vec!["abc123".into()];
        f.disk_free_gb = Some(20.0);
        f.hugepages_1g = Some(0);
        f.docker_ok = false;
        f.starts_at_boot = Some(false);
        let c = checks(&f);
        let get = |id: &str| c.iter().find(|x| x.id == id).unwrap().clone();
        assert_eq!(get("serving").fix, Some(Fix::RestartModel));
        assert_eq!(get("stray").fix, Some(Fix::StopStrayContainers));
        assert_eq!(get("disk").status, Status::Fail);
        assert_eq!(get("hugepages").status, Status::Fail);
        assert_eq!(get("docker").status, Status::Fail);
        assert!(get("boot").command.unwrap().contains("enable-linger"));
        assert_eq!(overall(&c), Status::Fail);

        let mut f = healthy();
        f.serving = Some((Phase::Starting, Some("Qwen3-32B".into()), 50 * 60));
        assert_eq!(
            checks(&f).iter().find(|x| x.id == "serving").unwrap().fix,
            Some(Fix::RestartModel)
        );
    }

    #[test]
    fn parses_real_tt_smi_output() {
        let json = r#"{"host_info": {"Driver": "TT-KMD 2.11.0"}, "device_info": [
          {"board_info": {"bus_id": "0000:01:00.0", "board_type": "p300c", "pcie_speed": 4, "pcie_width": "4"},
           "telemetry": {"asic_temperature": "44.1"}, "limits": {"therm_trip_l1_limit": "90"},
           "firmwares": {"fw_bundle_version": "19.13.1.0"}}]}"#;
        let (chips, driver) = parse_tt_smi(json).unwrap();
        assert_eq!(driver.as_deref(), Some("TT-KMD 2.11.0"));
        assert_eq!(chips[0].temp_c, Some(44.1));
        assert_eq!(chips[0].trip_c, Some(90.0));
        assert_eq!(chips[0].pcie, "Gen4 x4");
        assert!(parse_tt_smi("not json").is_none());
    }
}
