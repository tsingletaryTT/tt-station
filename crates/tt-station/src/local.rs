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
}

/// Locate the official `tt` and prove it is the official one.
///
/// The check exists because of a real incident on the owner's Mac: `~/.local/bin/tt` was a stale
/// copy of *tt-station's own* CLI from before the rename, shadowing the official tool. Ours never
/// had `--version` and answered `--help` with "Operator CLI for tt-station". The official CLI
/// answers `--version` with `tt <semver>`. Anything else is treated as not-the-official-CLI.
pub fn find_official_tt() -> Result<OfficialTt> {
    let bin = std::env::var(OFFICIAL_TT_ENV).unwrap_or_else(|_| "tt".to_string());
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

/// Ask the official CLI which models run on `mesh`, via `tt --json model list --hw <mesh>`.
pub fn right_sized(tt: &OfficialTt, mesh: &str) -> Result<Vec<RightSized>> {
    let out = Command::new(&tt.bin)
        .args(["--json", "model", "list", "--hw", mesh])
        .output()
        .with_context(|| format!("running `{} model list`", tt.bin))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`tt model list --hw {mesh}` failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        ));
    }
    parse_model_list(&out.stdout, mesh)
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

    let mut report = Report { cards, device_mesh, official_tt: None, models: None, models_error: None };
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
    println!("╚══ serving on this Mac is not wired up yet (needs the dext; see macos/TTStationDriver)");
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
    fn missing_models_array_is_an_error() {
        assert!(parse_model_list(br#"{"error":{"what":"x"}}"#, "p100").is_err());
    }
}
