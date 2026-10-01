//! `tt-station local`: this machine as its own host.
//!
//! Every other tt-station command talks to a *remote* box's agent. `local` is the first one that
//! treats the machine it runs on as the box: it detects Tenstorrent cards attached here (on a Mac,
//! through a Thunderbolt enclosure) and asks the **official** `tt` CLI which models are
//! right-sized for them. tt-station orchestrates, the official tooling decides (the CLAUDE.md
//! rule: prefer delegating to `tt` over reimplementing it).
//!
//! 1. Detect: `libttstation::local_device::scan` reads the IORegistry. No driver needed.
//! 2. Name the device config: `local_mesh` → the shared `device_mesh::mesh_for` table (`p100`).
//! 3. Right-size: `tt --json model list --hw <mesh>`. `--hw` makes the official CLI skip its own
//!    auto-detection, which needs tt-smi and so can't see a Mac-attached card yet.
//!
//! Serving is NOT here yet: `tt serve` assumes a Linux host with direct card access. See
//! docs/superpowers/specs/2026-09-28-macos-blackhole-dext-design.md (M4/M5).

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use libttstation::local_device::{self, LocalCard};
use serde::Serialize;
use serde_json::Value;

/// Env override for the official CLI's binary (tests point it at a fake).
pub const OFFICIAL_TT_ENV: &str = "TTS_OFFICIAL_TT_BIN";

/// Env override for the TTStationDriver host app's executable, which answers `telemetry` with one
/// JSON line once tt-station's dext has claimed the card (macos/TTStationDriver/Probe/bh_probe.c).
pub const DRIVER_HOST_ENV: &str = "TTS_DRIVER_HOST_BIN";
pub const DRIVER_HOST_DEFAULT: &str = "/Applications/TTStationDriver.app/Contents/MacOS/TTStationDriver";

/// Live chip telemetry read through tt-station's dext (ARC firmware, via libttbh). Fields are
/// `None` when that particular read failed.
#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct Telemetry {
    pub abi: u32,
    pub boot_status: Option<u32>,
    pub arc_ready: bool,
    pub asic_temp_c: Option<f64>,
    pub power_w: Option<u32>,
    pub vcore_mv: Option<u32>,
    pub current_a: Option<u32>,
    pub aiclk_mhz: Option<u32>,
}

/// Ask the driver host app for telemetry. Only meaningful when a card's driver is ours.
pub fn read_telemetry() -> Result<Telemetry> {
    let bin = std::env::var(DRIVER_HOST_ENV).unwrap_or_else(|_| DRIVER_HOST_DEFAULT.to_string());
    read_telemetry_from(&bin)
}

/// [`read_telemetry`] against an explicit host-app binary (tests point this at a fake).
fn read_telemetry_from(bin: &str) -> Result<Telemetry> {
    let out = Command::new(bin).arg("telemetry").output().with_context(|| format!("running `{bin} telemetry`"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The host app prints its JSON line even on failure (values null), so a nonzero exit has to be
    // checked first or a dead card would read as a card with no sensors. Exit codes are
    // `ttbh_telemetry_json`'s (Probe/bh_probe.c): 1 = dext unreachable, 2 = card looks dead.
    if !out.status.success() {
        let why = match out.status.code() {
            Some(1) => "couldn't reach tt-station's driver",
            Some(2) => "the card looks dead (ARC not answering)",
            _ => "failed",
        };
        return Err(anyhow!(
            "`{bin} telemetry` {why} (exit {}): {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            stdout.trim()
        ));
    }
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{')).ok_or_else(|| {
        anyhow!("`{bin} telemetry` printed no JSON (exit {}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim())
    })?;
    serde_json::from_str(line).context("parsing driver telemetry JSON")
}

/// The official `tt` CLI, verified to BE the official one.
#[derive(Debug, Clone, Serialize)]
pub struct OfficialTt {
    pub bin: String,
    pub version: String,
}

/// One right-sized model, flattened from `tt model list`'s per-device entry.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RightSized {
    pub name: String,
    pub model_type: Option<String>,
    pub engines: Vec<String>,
    pub status: Option<String>,
    pub max_context: Option<u64>,
}

#[derive(Debug, Serialize)]
struct Report {
    cards: Vec<LocalCard>,
    device_mesh: Option<&'static str>,
    official_tt: Option<OfficialTt>,
    models: Option<Vec<RightSized>>,
    /// Why `models` is absent, when it is.
    models_error: Option<String>,
    /// Live telemetry, present only when tt-station's own dext has claimed a card.
    telemetry: Option<Telemetry>,
    /// Why `telemetry` is absent although our dext is attached.
    telemetry_error: Option<String>,
}

/// Locate the official `tt` and prove it is the official one.
///
/// The check exists because of a real incident on the owner's Mac: `~/.local/bin/tt` was a stale
/// copy of *tt-station's own* CLI from before the rename, shadowing the official tool. Ours never
/// had `--version` and answered `--help` with "Operator CLI for tt-station". The official CLI
/// answers `--version` with `tt <semver>`. Anything else is treated as not-the-official-CLI.
pub fn find_official_tt() -> Result<OfficialTt> {
    let bin = match std::env::var(OFFICIAL_TT_ENV) {
        Ok(explicit) => explicit,
        Err(_) => locate_tt(
            std::env::var_os("PATH").as_deref(),
            std::env::var_os("HOME").map(std::path::PathBuf::from).as_deref(),
        )
        .unwrap_or_else(|| "tt".to_string()),
    };
    let out = Command::new(&bin).arg("--version").output().map_err(|e| {
        anyhow!("could not run `{bin}` ({e}); install the official CLI: uv tool install tenstorrent")
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first = stdout.lines().next().unwrap_or("").trim();
    let version = first
        .strip_prefix("tt ")
        .filter(|v| out.status.success() && v.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .ok_or_else(|| {
            anyhow!(
                "`{bin}` is not the official Tenstorrent CLI (`{bin} --version` gave {first:?}). \
                 Check `which -a tt` for a shadowing binary, then: uv tool install tenstorrent"
            )
        })?;
    Ok(OfficialTt { bin, version: version.to_string() })
}

/// Where `tt` lives: the first `tt` on `PATH`, else uv's and Homebrew's install locations.
///
/// The fallback matters because the macOS app runs `tt-station` with launchd's minimal PATH
/// (`/usr/bin:/bin:/usr/sbin:/sbin`), which never includes `~/.local/bin`, where
/// `uv tool install tenstorrent` puts the official CLI. A `tt` that IS on PATH always wins, even if
/// it turns out not to be the official one: [`find_official_tt`] then refuses it loudly instead of
/// quietly preferring another copy, because a shadowed `tt` is a problem the operator should see.
pub fn locate_tt(path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Option<String> {
    let is_exe = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    };
    let on_path = path
        .map(|p| std::env::split_paths(p).map(|d| d.join("tt")).collect::<Vec<_>>())
        .unwrap_or_default();
    let fallbacks = home
        .map(|h| h.join(".local/bin/tt"))
        .into_iter()
        .chain(["/opt/homebrew/bin/tt", "/usr/local/bin/tt"].map(std::path::PathBuf::from));
    on_path
        .into_iter()
        .chain(fallbacks)
        .find(|p| is_exe(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Ask the official CLI which models run on `mesh`, via `tt --json model list --hw <mesh>`.
pub fn right_sized(tt: &OfficialTt, mesh: &str) -> Result<Vec<RightSized>> {
    let out = Command::new(&tt.bin)
        .args(["--json", "model", "list", "--hw", mesh])
        .output()
        .with_context(|| format!("running `{} model list`", tt.bin))?;
    if !out.status.success() {
        return Err(anyhow!("`tt model list --hw {mesh}` failed ({}): {}", out.status, failure_detail(&out)));
    }
    parse_model_list(&out.stdout, mesh)
}

/// What a failed command said about why: stderr, where CLIs normally report failures, or stdout
/// when stderr is empty (some print a JSON error there under `--json`). This text becomes
/// `models_error`, which the CLI and the Mac pane show, so a blank one would hide the cause.
fn failure_detail(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout.is_empty() { "(no output)".to_string() } else { stdout }
}

/// Parse `tt --json model list --hw <mesh>` output: `{ "device": .., "models": [ {name,
/// model_type, engines, devices: { <mesh>: {status, max_context, ..} } } ] }`. Pure, for tests.
pub fn parse_model_list(json: &[u8], mesh: &str) -> Result<Vec<RightSized>> {
    let v: Value = serde_json::from_slice(json).context("tt model list: not JSON")?;
    let models = v
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("tt model list: no `models` array"))?;
    Ok(models
        .iter()
        .filter_map(|m| {
            let per = m.get("devices").and_then(|d| d.get(mesh));
            // Belt and braces: `--hw` already filters, but only keep entries that say they're
            // supported on this mesh (or don't say either way).
            if per.and_then(|p| p.get("supported")).and_then(Value::as_bool) == Some(false) {
                return None;
            }
            Some(RightSized {
                name: m.get("name")?.as_str()?.to_string(),
                model_type: m.get("model_type").and_then(Value::as_str).map(str::to_string),
                engines: per
                    .and_then(|p| p.get("engines"))
                    .or_else(|| m.get("engines"))
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|e| e.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                status: per.and_then(|p| p.get("status")).and_then(Value::as_str).map(str::to_string),
                max_context: per.and_then(|p| p.get("max_context")).and_then(Value::as_u64),
            })
        })
        .collect())
}

/// Entry point for `tt-station local`.
pub fn run(ioreg_file: Option<&Path>, no_models: bool, json: bool) -> Result<()> {
    let cards = match ioreg_file {
        Some(p) => local_device::parse_ioreg(
            &std::fs::read(p).with_context(|| format!("reading {}", p.display()))?,
        )?,
        None => local_device::scan()?,
    };
    let device_mesh = local_device::local_mesh(&cards);

    let mut report = Report {
        cards, device_mesh, official_tt: None, models: None, models_error: None, telemetry: None, telemetry_error: None,
    };
    // Telemetry needs our dext; without it there's nothing to ask (and no error to report).
    if report.cards.iter().any(LocalCard::has_tt_station_driver) {
        match read_telemetry() {
            Ok(t) => report.telemetry = Some(t),
            Err(e) => report.telemetry_error = Some(format!("{e:#}")),
        }
    }
    if !no_models {
        match device_mesh {
            None if report.cards.is_empty() => {}
            None => report.models_error = Some("no confirmed device config for these cards".into()),
            Some(mesh) => match find_official_tt().and_then(|tt| {
                let models = right_sized(&tt, mesh)?;
                Ok((tt, models))
            }) {
                Ok((tt, models)) => {
                    report.official_tt = Some(tt);
                    report.models = Some(models);
                }
                Err(e) => report.models_error = Some(format!("{e:#}")),
            },
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report);
    }
    Ok(())
}

fn fmt_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{} GiB", n >> 30),
        n if n >= 1 << 20 => format!("{} MiB", n >> 20),
        n if n >= 1 << 10 => format!("{} KiB", n >> 10),
        n => format!("{n} B"),
    }
}

/// Left-bar-only box (CLAUDE.md: no right-side borders, they break on narrow terminals).
fn print_human(r: &Report) {
    println!("╔══ tt-station local");
    if r.cards.is_empty() {
        println!("║  no Tenstorrent cards attached to this machine");
        println!("╚══");
        return;
    }
    for c in &r.cards {
        let link = c.link.map(|l| format!("PCIe Gen{} x{}", l.gen, l.width)).unwrap_or_else(|| "link down".into());
        println!(
            "║  {}  ({} {:04x}:{:04x}, subsys {:04x}){}  {link}  @{}",
            c.board_type.unwrap_or("unknown board"),
            c.chip.unwrap_or("unknown chip"),
            c.vendor_id,
            c.device_id,
            c.subsystem_id,
            if c.tunnelled { "  via Thunderbolt" } else { "" },
            c.location.as_deref().unwrap_or("?"),
        );
        let ranges: Vec<String> = c.memory_ranges.iter().map(|&n| fmt_size(n)).collect();
        println!("║    memory ranges: {}", ranges.join(", "));
        let driver = match c.driver.as_deref() {
            None => "none (unclaimed)".to_string(),
            Some(_) if c.has_tt_station_driver() => "TTStationDriver (tt-station's dext)".to_string(),
            Some(other) => format!("{other} (not tt-station's)"),
        };
        println!("║    driver: {driver}");
    }
    if let Some(t) = &r.telemetry {
        let f = |v: Option<u32>, unit: &str| v.map(|v| format!("{v} {unit}")).unwrap_or_else(|| "?".into());
        println!(
            "║  live: {}  {}  {}  {}  ARC {}",
            t.asic_temp_c.map(|c| format!("{c:.1} °C")).unwrap_or_else(|| "?".into()),
            f(t.power_w, "W"),
            f(t.vcore_mv, "mV"),
            f(t.aiclk_mhz, "MHz"),
            if t.arc_ready { "ready" } else { "not ready" },
        );
    } else if let Some(e) = &r.telemetry_error {
        println!("║  live telemetry unavailable: {e}");
    }
    println!("║  device config: {}", r.device_mesh.unwrap_or("(unknown — not guessing)"));
    match (&r.official_tt, &r.models) {
        (Some(tt), Some(models)) => {
            println!("║");
            println!("║  right-sized models (official tt {}: `tt model list --hw {}`):", tt.version, r.device_mesh.unwrap_or("?"));
            if models.is_empty() {
                println!("║    (none)");
            }
            for m in models {
                println!(
                    "║    {:<28} {:<5} {:<13} ctx {}",
                    m.name,
                    m.model_type.as_deref().unwrap_or("-"),
                    m.status.as_deref().unwrap_or("-"),
                    m.max_context.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                );
            }
        }
        _ => {
            if let Some(err) = &r.models_error {
                println!("║");
                println!("║  right-sizing unavailable: {err}");
            }
        }
    }
    println!(
        "╚══ serving on this Mac is not wired up yet{}",
        if r.cards.iter().any(LocalCard::has_tt_station_driver) { "" } else { " (needs the dext; see macos/TTStationDriver)" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from real `tt --json model list --hw p100` output (tt 1.0.1, 2026-09-28).
    const P100_LIST: &str = r#"{"device":"p100","models":[
      {"name":"Llama-3.1-8B","model_type":"llm","engines":["vLLM"],
       "devices":{"n150":{"status":"COMPLETE","supported":true,"max_context":65536},
                  "p100":{"engines":["vLLM"],"status":"EXPERIMENTAL","supported":true,"max_context":65536}}},
      {"name":"Llama-3.1-8B-Instruct","model_type":"llm","engines":["vLLM","forge"],
       "devices":{"p100":{"engines":["vLLM"],"status":"EXPERIMENTAL","supported":true,"max_context":65536}}}]}"#;

    #[test]
    fn parses_real_p100_listing() {
        let got = parse_model_list(P100_LIST.as_bytes(), "p100").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "Llama-3.1-8B");
        assert_eq!(got[0].status.as_deref(), Some("EXPERIMENTAL"));
        assert_eq!(got[0].max_context, Some(65536));
        // Per-device engines win over the model-level list.
        assert_eq!(got[1].engines, vec!["vLLM".to_string()]);
    }

    #[test]
    fn drops_entries_explicitly_unsupported_on_the_mesh() {
        let json = r#"{"models":[{"name":"X","devices":{"p100":{"supported":false}}}]}"#;
        assert!(parse_model_list(json.as_bytes(), "p100").unwrap().is_empty());
    }

    #[test]
    fn locate_tt_prefers_path_then_falls_back_to_uv_location() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let tt = bin.join("tt");
        std::fs::write(&tt, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tt, std::fs::Permissions::from_mode(0o755)).unwrap();

        // A GUI-style PATH without ~/.local/bin still finds uv's install.
        let gui_path = std::ffi::OsString::from("/nonexistent-a:/nonexistent-b");
        assert_eq!(locate_tt(Some(&gui_path), Some(home.path())), Some(tt.to_string_lossy().into_owned()));

        // Something on PATH wins over the fallback (and is then vetted by find_official_tt).
        let other = tempfile::tempdir().unwrap();
        let other_tt = other.path().join("tt");
        std::fs::write(&other_tt, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&other_tt, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            locate_tt(Some(other.path().as_os_str()), Some(home.path())),
            Some(other_tt.to_string_lossy().into_owned())
        );
    }

    /// A fake host app: prints `json`, exits `code`.
    #[cfg(unix)]
    fn fake_host(dir: &Path, json: &str, code: i32) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("TTStationDriver");
        std::fs::write(&p, format!("#!/bin/sh\necho '{json}'\necho 'stderr says why' >&2\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_json_from_a_failed_run_is_not_believed() {
        // The host app prints its JSON line (values null) even when the card is dead; exit 2 must
        // win over that well-formed JSON.
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"abi":2,"boot_status":null,"arc_ready":false,"asic_temp_c":null,"power_w":null,"vcore_mv":null,"current_a":null,"aiclk_mhz":null}"#;
        let err = read_telemetry_from(&fake_host(dir.path(), json, 2)).unwrap_err().to_string();
        assert!(err.contains("looks dead"), "{err}");
        assert!(err.contains("stderr says why"), "{err}");
        // ...and the same JSON with exit 0 parses, so the rejection above is the exit code's doing.
        assert!(read_telemetry_from(&fake_host(dir.path(), json, 0)).is_ok());
    }

    /// A fake official `tt` that fails with the given streams.
    #[cfg(unix)]
    fn fake_tt(dir: &Path, stdout: &str, stderr: &str, code: i32) -> OfficialTt {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("tt");
        std::fs::write(&p, format!("#!/bin/sh\nprintf '%s' '{stdout}'\nprintf '%s' '{stderr}' >&2\nexit {code}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        OfficialTt { bin: p.to_string_lossy().into_owned(), version: "1.0.1".into() }
    }

    #[cfg(unix)]
    #[test]
    fn model_list_failure_reports_stderr_and_exit_status() {
        let dir = tempfile::tempdir().unwrap();
        let err = right_sized(&fake_tt(dir.path(), "", "Error: unknown hardware 'p100'", 2), "p100")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown hardware"), "{err}");
        assert!(err.contains('2'), "exit status missing: {err}");
        // stderr empty: fall back to stdout (a JSON error under --json)...
        let err = right_sized(&fake_tt(dir.path(), r#"{"error":"no such hw"}"#, "", 1), "p100")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such hw"), "{err}");
        // ...and neither: say so rather than end in a blank.
        let err = right_sized(&fake_tt(dir.path(), "", "", 1), "p100").unwrap_err().to_string();
        assert!(err.contains("(no output)"), "{err}");
    }

    #[test]
    fn missing_models_array_is_an_error() {
        assert!(parse_model_list(br#"{"error":{"what":"x"}}"#, "p100").is_err());
    }
}
