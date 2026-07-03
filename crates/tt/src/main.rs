//! `tt`: the operator-facing CLI for tt-station.
//!
//! Wires together everything `libttstation` provides -- discovery, pairing,
//! secret storage, and the agent control-plane client -- into the commands
//! an operator (or the Task 12 e2e test) actually runs:
//!
//!   tt [--json] discover [--host <h:p>]... [--no-mdns] [--timeout-ms <ms>]
//!   tt [--json] pair <host:port> [--code <code>]
//!   tt [--json] run <model> --host <host:port>
//!   tt [--json] stop --host <host:port>
//!   tt [--json] status --host <host:port>
//!   tt [--json] endpoint --host <host:port>
//!
//! `--json` is global (accepted before or after the subcommand) and switches
//! every command's stdout from human-readable text to machine-readable JSON.
//!
//! ## Why the raw `host:port` string is the identity key
//!
//! `pair` stores its bearer token under the exact `host:port` string the
//! operator typed; `run`/`stop`/`status`/`endpoint` look a token up under
//! the exact `--host` string they're given. There's no separate box-name
//! lookup -- keeping identity to "the address you paired with" means a
//! command never has to guess which of possibly several boxes named the
//! same thing you meant. It also matches how the e2e test drives this CLI:
//! `pair 127.0.0.1:<p> --code 000000` then `run llama3 --host 127.0.0.1:<p>`.
//!
//! ## Why `main` isn't `#[tokio::main]`
//!
//! `discover`'s manual-host probe (see [`manual_status_fetch`]) uses
//! `reqwest::blocking`, which spins up its own Tokio runtime internally --
//! calling it from inside an *already-running* async runtime panics
//! ("Cannot start a runtime from within a runtime"). `MdnsProvider::discover`
//! is also fully synchronous (no `.await` anywhere in it). So `discover` runs
//! with no runtime active at all, while every other command (which drives
//! `libttstation`'s async `pairing`/`agent_client` functions) builds its own
//! `tokio::runtime::Runtime` just for that one call. This keeps "blocking"
//! and "async" cleanly separated instead of fighting Tokio's single-blocking-
//! call-per-runtime rules.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libttstation::agent_client::AgentClient;
use libttstation::discovery::{aggregate, manual::ManualProvider, mdns::MdnsProvider, DiscoveryProvider};
use libttstation::model::{BoxRecord, Endpoint, ServingStatus};
use libttstation::pairing::{pair_complete, pair_init};
use libttstation::secrets::{default_store, FileStore, SecretStore};
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "tt", about = "Operator CLI for tt-station")]
struct Cli {
    /// Emit machine-readable JSON on stdout instead of human-readable text.
    /// Global so it can appear before or after the subcommand
    /// (`tt --json discover` and `tt discover --json` both work).
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Find Tenstorrent boxes on the network (mDNS) and/or at specific
    /// addresses (manual hosts).
    Discover {
        /// A `host:port` to probe directly, in addition to mDNS browsing.
        /// May be repeated.
        #[arg(long = "host")]
        hosts: Vec<String>,

        /// Skip mDNS browsing and only probe `--host` addresses. Useful in
        /// sandboxed/CI environments without multicast, and in the e2e test
        /// for determinism.
        #[arg(long = "no-mdns")]
        no_mdns: bool,

        /// How long to let mDNS browsing run, in milliseconds.
        #[arg(long = "timeout-ms", default_value_t = 1000)]
        timeout_ms: u64,
    },

    /// Pair with a box: exchange a human-read code for a bearer token and
    /// store it for future commands against the same host:port.
    Pair {
        /// The box's control-plane address, as `host:port`.
        host: String,

        /// The pairing code displayed on the box. If omitted, prompts on
        /// stdin (an operator reading the box's screen); the e2e test and
        /// scripted use always pass this explicitly.
        #[arg(long)]
        code: Option<String>,
    },

    /// Ask a paired box to start serving `model`.
    Run {
        /// The model identifier to serve (backend-specific, e.g. a Docker
        /// image tag or dstack app name).
        model: String,

        /// The box's control-plane address, as `host:port`. Must already be
        /// paired (see `tt pair`).
        #[arg(long)]
        host: String,
    },

    /// Ask a paired box to stop serving.
    Stop {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,
    },

    /// Show a paired box's current serving status.
    Status {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,
    },

    /// Show the endpoint of whatever a paired box is currently serving.
    Endpoint {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Command::Discover {
            hosts,
            no_mdns,
            timeout_ms,
        } => {
            let boxes = cmd_discover(hosts, *no_mdns, *timeout_ms)?;
            print_discover(&boxes, cli.json);
        }
        Command::Pair { host, code } => {
            let token = run_async(cmd_pair(host, code.clone()))?;
            print_pair(host, &token, cli.json);
        }
        Command::Run { model, host } => {
            let endpoint = run_async(cmd_run(host, model))?;
            print_endpoint_result(&endpoint, cli.json);
        }
        Command::Stop { host } => {
            run_async(cmd_stop(host))?;
            print_stop(cli.json);
        }
        Command::Status { host } => {
            let status = run_async(cmd_status(host))?;
            print_status(&status, cli.json);
        }
        Command::Endpoint { host } => {
            let endpoint = run_async(cmd_endpoint(host))?;
            print_endpoint_export(&endpoint, cli.json);
        }
    }

    Ok(())
}

/// Build a fresh single-purpose Tokio runtime and block on `fut`. See the
/// module doc for why this beats a top-level `#[tokio::main]` here: only the
/// commands that actually need async (everything but `discover`) pay for a
/// runtime, and `discover`'s blocking HTTP probe never has to fight one.
fn run_async<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Runtime::new()
        .expect("failed to start Tokio runtime")
        .block_on(fut)
}

// ---------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------

/// `tt discover`: run mDNS browsing (unless `--no-mdns`) and/or probe any
/// `--host` addresses, via `libttstation::discovery::aggregate`.
fn cmd_discover(hosts: &[String], no_mdns: bool, timeout_ms: u64) -> Result<Vec<BoxRecord>> {
    let mut providers: Vec<Box<dyn DiscoveryProvider>> = Vec::new();

    if !hosts.is_empty() {
        let mut parsed = Vec::with_capacity(hosts.len());
        for h in hosts {
            parsed.push(parse_host_port(h)?);
        }
        providers.push(Box::new(ManualProvider::new(parsed, manual_status_fetch)));
    }

    if !no_mdns {
        providers.push(Box::new(MdnsProvider));
    }

    Ok(aggregate(&providers, Duration::from_millis(timeout_ms)))
}

/// The `fetch` closure `ManualProvider` calls per configured host: a plain
/// blocking `GET /status`, decoded into a `BoxRecord`. Blocking (not async)
/// because `ManualProvider::discover` is a synchronous trait method -- see
/// the module doc for why that's fine here (no runtime is active when
/// `discover` runs).
fn manual_status_fetch(host: &str, port: u16) -> Result<BoxRecord> {
    #[derive(Deserialize)]
    struct StatusResponse {
        name: String,
        chips: String,
        status: String,
    }

    let url = format!("http://{host}:{port}/status");
    let resp: StatusResponse = reqwest::blocking::get(&url)
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .json()
        .with_context(|| format!("parsing response from {url}"))?;

    Ok(BoxRecord {
        name: resp.name,
        host: host.to_string(),
        ctrl_port: port,
        chips: resp.chips,
        status: ServingStatus::from_txt(&resp.status)?,
        // `/status` doesn't report an API version; manual hosts are assumed
        // to speak the same API version this CLI does.
        apiver: 1,
    })
}

/// `tt pair <host:port>`: run the pairing handshake and store the resulting
/// token under `host` in the `SecretStore`. Returns the token so the caller
/// can decide how much of it to print.
async fn cmd_pair(host: &str, code: Option<String>) -> Result<String> {
    let base = format!("http://{host}");
    let pair_id = pair_init(&base).await?;

    let code = match code {
        Some(c) => c,
        None => prompt_for_code()?,
    };

    let token = pair_complete(&base, &pair_id, &code).await?;
    build_store()?.set(host, &token)?;
    Ok(token)
}

/// Read a pairing code from stdin. Only reached when `--code` is omitted --
/// the e2e test and any scripted use always pass `--code` explicitly, so
/// this path exists for interactive operators reading a code off a box's
/// screen.
fn prompt_for_code() -> Result<String> {
    use std::io::Write;
    print!("Enter the pairing code shown on the box: ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading pairing code from stdin")?;
    Ok(line.trim().to_string())
}

/// `tt run <model> --host <host:port>`: load the stored token for `host` and
/// ask the agent to start serving `model`.
async fn cmd_run(host: &str, model: &str) -> Result<Endpoint> {
    let client = authed_client(host)?;
    client.run(model).await
}

/// `tt stop --host <host:port>`.
async fn cmd_stop(host: &str) -> Result<()> {
    authed_client(host)?.stop().await
}

/// `tt status --host <host:port>`.
async fn cmd_status(host: &str) -> Result<ServingStatus> {
    authed_client(host)?.status().await
}

/// `tt endpoint --host <host:port>`.
async fn cmd_endpoint(host: &str) -> Result<Endpoint> {
    authed_client(host)?.endpoint().await
}

/// Build an `AgentClient` for `host`, using the token stored by a prior
/// `tt pair`. Shared by every command that needs an authenticated call.
fn authed_client(host: &str) -> Result<AgentClient> {
    let token = build_store()?.get(host)?.ok_or_else(|| {
        anyhow::anyhow!("no token stored for {host}; run `tt pair {host}` first")
    })?;
    Ok(AgentClient::new(format!("http://{host}"), token))
}

/// Build the `SecretStore` this CLI uses: a `FileStore` rooted at
/// `$TT_CONFIG_DIR/secrets.json` when that env var is set (so tests and
/// operators who want an isolated config dir never touch real state), or
/// `libttstation::secrets::default_store()` otherwise (Keychain on macOS,
/// `FileStore` under `$XDG_CONFIG_HOME`/`~/.config` elsewhere).
fn build_store() -> Result<Box<dyn SecretStore>> {
    if let Ok(dir) = std::env::var("TT_CONFIG_DIR") {
        return Ok(Box::new(FileStore::new(
            PathBuf::from(dir).join("secrets.json"),
        )));
    }
    Ok(default_store())
}

// ---------------------------------------------------------------------
// Pure parsing/formatting helpers (unit-tested below)
// ---------------------------------------------------------------------

/// Parse a `host:port` string into its parts. Splits on the *last* `:` so
/// this at least doesn't choke if a bracketed IPv6 literal shows up later
/// (full IPv6 support is out of scope for this PoC -- every host used today
/// is a plain hostname or IPv4 literal).
fn parse_host_port(s: &str) -> Result<(String, u16)> {
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("expected host:port, got {s:?}"))?;
    let port: u16 = port
        .parse()
        .with_context(|| format!("invalid port in {s:?}"))?;
    Ok((host.to_string(), port))
}

/// One human-readable line describing a discovered box, e.g.
/// `qb2-lab  127.0.0.1:8899  4xBH  serving:llama3`.
fn format_boxrecord_line(rec: &BoxRecord) -> String {
    format!(
        "{}\t{}:{}\t{}\t{}",
        rec.name,
        rec.host,
        rec.ctrl_port,
        rec.chips,
        rec.status.to_txt()
    )
}

/// The `export OPENAI_BASE_URL=...` line `tt endpoint` prints by default
/// (non-`--json`), so an operator can `eval "$(tt endpoint --host ...)"` or
/// copy-paste it straight into a shell.
fn endpoint_export_line(ep: &Endpoint) -> String {
    format!("export OPENAI_BASE_URL={}", ep.base_url)
}

// ---------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------

/// JSON shape for one `tt discover` result. Not just `#[derive(Serialize)]`
/// on `BoxRecord` directly: `BoxRecord.status` is a `ServingStatus` enum,
/// and serde's default derive would encode it as `"Idle"` or
/// `{"Serving":"llama3"}` -- diverging from the `to_txt()` wire format
/// (`"idle"` / `"serving:llama3"`) that `tt --json status` and every HTTP
/// route in this codebase already use. Re-encoding `status` through
/// `to_txt()` here keeps every JSON-emitting command speaking the same
/// status representation.
#[derive(serde::Serialize)]
struct DiscoveredBox<'a> {
    name: &'a str,
    host: &'a str,
    ctrl_port: u16,
    chips: &'a str,
    status: String,
    apiver: u8,
}

impl<'a> From<&'a BoxRecord> for DiscoveredBox<'a> {
    fn from(rec: &'a BoxRecord) -> Self {
        DiscoveredBox {
            name: &rec.name,
            host: &rec.host,
            ctrl_port: rec.ctrl_port,
            chips: &rec.chips,
            status: rec.status.to_txt(),
            apiver: rec.apiver,
        }
    }
}

fn print_discover(boxes: &[BoxRecord], json: bool) {
    if json {
        let entries: Vec<DiscoveredBox> = boxes.iter().map(DiscoveredBox::from).collect();
        println!("{}", serde_json::to_string(&entries).expect("DiscoveredBox always serializes"));
    } else if boxes.is_empty() {
        println!("no boxes found");
    } else {
        for rec in boxes {
            println!("{}", format_boxrecord_line(rec));
        }
    }
}

fn print_pair(host: &str, token: &str, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "host": host, "paired": true, "token": token })
        );
    } else {
        println!("paired with {host}; token stored");
    }
}

fn print_endpoint_result(ep: &Endpoint, json: bool) {
    if json {
        println!("{}", serde_json::to_string(ep).expect("Endpoint always serializes"));
    } else {
        println!(
            "serving {} at {} (requires_key={})",
            ep.model, ep.base_url, ep.requires_key
        );
    }
}

fn print_stop(json: bool) {
    if json {
        println!("{}", serde_json::json!({}));
    } else {
        println!("stopped");
    }
}

fn print_status(status: &ServingStatus, json: bool) {
    if json {
        println!("{}", serde_json::json!({ "status": status.to_txt() }));
    } else {
        println!("{}", status.to_txt());
    }
}

/// `tt endpoint`'s output: JSON prints the `Endpoint` object; human mode
/// prints the `export OPENAI_BASE_URL=...` line per the task spec, so it's
/// directly `eval`-able.
fn print_endpoint_export(ep: &Endpoint, json: bool) {
    if json {
        println!("{}", serde_json::to_string(ep).expect("Endpoint always serializes"));
    } else {
        println!("{}", endpoint_export_line(ep));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_splits_on_last_colon() {
        assert_eq!(
            parse_host_port("127.0.0.1:8899").unwrap(),
            ("127.0.0.1".to_string(), 8899)
        );
    }

    #[test]
    fn parse_host_port_rejects_missing_colon() {
        assert!(parse_host_port("no-port-here").is_err());
    }

    #[test]
    fn parse_host_port_rejects_non_numeric_port() {
        assert!(parse_host_port("host:notaport").is_err());
    }

    #[test]
    fn format_boxrecord_line_includes_name_host_and_status() {
        let rec = BoxRecord {
            name: "qb2-lab".into(),
            host: "127.0.0.1".into(),
            ctrl_port: 8899,
            chips: "4xBH".into(),
            status: ServingStatus::Serving("llama3".into()),
            apiver: 1,
        };
        let line = format_boxrecord_line(&rec);
        assert!(line.contains("qb2-lab"));
        assert!(line.contains("127.0.0.1:8899"));
        assert!(line.contains("serving:llama3"));
    }

    #[test]
    fn endpoint_export_line_wraps_base_url_in_export_statement() {
        let ep = Endpoint {
            base_url: "http://127.0.0.1:8899/v1".into(),
            model: "llama3".into(),
            requires_key: false,
        };
        assert_eq!(
            endpoint_export_line(&ep),
            "export OPENAI_BASE_URL=http://127.0.0.1:8899/v1"
        );
    }
}
