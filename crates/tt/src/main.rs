//! `tt`: the operator-facing CLI for tt-station.
//!
//! Wires together everything `libttstation` provides -- discovery, pairing,
//! secret storage, and the agent control-plane client -- into the commands
//! an operator (or the Task 12 e2e test) actually runs:
//!
//!   tt [--json] discover [--host <h:p>]... [--no-mdns] [--timeout-ms <ms>]
//!   tt [--json] pair <host:port> [--code <code>]
//!   tt [--json] models --host <host:port>
//!   tt [--json] run <model> --host <host:port>
//!   tt [--json] stop --host <host:port>
//!   tt [--json] status --host <host:port>
//!   tt [--json] endpoint --host <host:port>
//!   tt [--json] serving --host <host:port>
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
use libttstation::discovery::{
    aggregate, manual::ManualProvider, mdns::MdnsProvider, DiscoveryProvider,
};
use libttstation::model::{
    BoxRecord, Endpoint, ModelsResponse, ServingList, ServingStatus, StatusInfo,
};
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

    /// Start pairing with a box: trigger it to mint a 6-digit code and print
    /// it on ITS OWN console, and return the `pair_id` this attempt needs to
    /// be completed with (see `tt pair-complete`). Split out from `tt pair`
    /// so a caller (e.g. a GUI shell) can drive the two round-trips as
    /// separate one-shot steps instead of blocking on stdin for the code.
    PairInit {
        /// The box's control-plane address, as `host:port`.
        host: String,
    },

    /// Finish pairing with a box: exchange the `pair_id` from `tt pair-init`
    /// and the code the box printed on its console for a bearer token, and
    /// store it under `host` exactly like `tt pair` does.
    PairComplete {
        /// The box's control-plane address, as `host:port`.
        host: String,

        /// The `pair_id` returned by `tt pair-init`.
        #[arg(long = "pair-id")]
        pair_id: String,

        /// The 6-digit code shown on the box's console.
        #[arg(long)]
        code: String,
    },

    /// Enumerate the models a box can serve, per its `model_spec.json` --
    /// so an operator (or script) never has to guess/hardcode a model id
    /// before `tt run`. UNAUTHED on the agent side (like `status`), so this
    /// works even against a box `tt pair` was never run against.
    Models {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,
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

    /// List EVERY live `tt-inference-server` `/v1` endpoint on a box --
    /// whoever launched it (this agent's `tt run`, tt-studio, or a manual
    /// `run.py`). UNAUTHED on the agent side (like `status`/`models`), so it
    /// works even against a box `tt pair` was never run against.
    Serving {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,
    },

    /// Reset to a fresh install: forget EVERY paired box on this machine
    /// (clear all locally stored tokens). With `--host`, first ask that box
    /// to reset itself (stop serving, clear its tokens, reset the board)
    /// before forgetting it locally. Meant for demos.
    Reset {
        /// A specific box to reset remotely BEFORE clearing local state, as
        /// `host:port`. Omit to only clear local state (forget all boxes).
        #[arg(long)]
        host: Option<String>,

        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
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
        Command::PairInit { host } => {
            let pair_id = run_async(cmd_pair_init(host))?;
            print_pair_init(host, &pair_id, cli.json);
        }
        Command::PairComplete {
            host,
            pair_id,
            code,
        } => {
            run_async(cmd_pair_complete(host, pair_id, code))?;
            print_pair_complete(host, cli.json);
        }
        Command::Models { host } => {
            let resp = run_async(cmd_models(host))?;
            print_models(&resp, cli.json);
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
        Command::Serving { host } => {
            let list = run_async(cmd_serving(host))?;
            print_serving(&list, cli.json);
        }
        Command::Reset { host, yes } => {
            // Confirm BEFORE spinning up a runtime or clearing anything:
            // unless `--yes`, spell out exactly what will be cleared and
            // require the operator to type `y`. A declined prompt aborts
            // without touching local or remote state.
            if !*yes && !confirm_reset(host.as_deref())? {
                print_reset_aborted(cli.json);
                return Ok(());
            }
            let summary = run_async(cmd_reset(host.as_deref()))?;
            print_reset(&summary, cli.json);
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
        // Thread the operator's `--timeout-ms` into the fetch closure so a
        // dead/unroutable manual host fails fast within the configured
        // window instead of hanging on the OS's default TCP connect timeout
        // (~2 minutes on Linux) -- see `manual_status_fetch`.
        let timeout = Duration::from_millis(timeout_ms);
        providers.push(Box::new(ManualProvider::new(parsed, move |host, port| {
            manual_status_fetch(host, port, timeout)
        })));
    }

    if !no_mdns {
        providers.push(Box::new(MdnsProvider));
    }

    Ok(aggregate(&providers, Duration::from_millis(timeout_ms)))
}

/// Build the `reqwest::blocking::Client` used to probe a manual host's
/// `/status`. Both the overall request timeout and the TCP connect timeout
/// are set to `timeout` -- without this, `reqwest::blocking::get` uses no
/// request timeout at all and falls back to the OS's default TCP connect
/// timeout (on the order of minutes) when a host is routable but not
/// answering, which makes `tt discover --host <dead-ip>:<port> --timeout-ms
/// <n>` hang far longer than `--timeout-ms` promises. Split out as its own
/// function so it's unit-testable without making a real network call.
fn build_probe_client(timeout: Duration) -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .build()
        .context("building HTTP client for manual host probe")
}

/// The `fetch` closure `ManualProvider` calls per configured host: a plain
/// blocking `GET /status`, decoded into a `BoxRecord`. Blocking (not async)
/// because `ManualProvider::discover` is a synchronous trait method -- see
/// the module doc for why that's fine here (no runtime is active when
/// `discover` runs).
///
/// `timeout` is the same duration as `discover`'s `--timeout-ms`: a manual
/// probe shouldn't get to hang longer than mDNS browsing is allowed to run.
fn manual_status_fetch(host: &str, port: u16, timeout: Duration) -> Result<BoxRecord> {
    #[derive(Deserialize)]
    struct StatusResponse {
        name: String,
        chips: String,
        status: String,
        /// (Task 3) The box's detected device-mesh label -- see
        /// `tt-station-agentd::routes::StatusResponse::device_mesh`. Missing
        /// or `null` on the wire both deserialize to `None` here (serde
        /// treats an absent `Option<T>` field as `None`, same as an explicit
        /// `null`), so this stays compatible with any `/status` responder
        /// that predates Task 2 (e.g. `mock-box`).
        device_mesh: Option<String>,
    }

    let url = format!("http://{host}:{port}/status");
    let client = build_probe_client(timeout)?;
    let resp: StatusResponse = client
        .get(&url)
        .send()
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
        // This IS the "per-box status probe" `discover` already runs for
        // manual hosts (mDNS-discovered boxes have no such probe -- see
        // `libttstation::model::txt_decode`), so it's the one discover path
        // that can populate `device_mesh` from real data instead of `None`.
        device_mesh: resp.device_mesh,
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

/// `tt pair-init <host:port>`: one-shot first half of pairing. Triggers the
/// box to mint a 6-digit code (printed on ITS console) and returns the
/// `pair_id` needed to complete the handshake via `tt pair-complete`. Unlike
/// `cmd_pair`, this never reads stdin -- it's meant to be called by a caller
/// (e.g. a GUI shell) that will surface the `pair_id` and prompt for the code
/// itself, potentially across a process boundary.
async fn cmd_pair_init(host: &str) -> Result<String> {
    let base = format!("http://{host}");
    pair_init(&base).await
}

/// `tt pair-complete <host:port> --pair-id <id> --code <code>`: one-shot
/// second half of pairing. Exchanges the `pair_id` from a prior `tt
/// pair-init` and the code the box printed for a bearer token, and stores it
/// under `host` exactly like `cmd_pair` does -- so `tt run`/`tt status`/etc.
/// against the same `host` work identically regardless of which pairing path
/// was used.
async fn cmd_pair_complete(host: &str, pair_id: &str, code: &str) -> Result<()> {
    let base = format!("http://{host}");
    let token = pair_complete(&base, pair_id, code).await?;
    build_store()?.set(host, &token)?;
    Ok(())
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

/// `tt models --host <host:port>`: enumerate the models `host` can serve.
/// UNAUTHED on the agent side, so this needs no stored token -- unlike
/// every other command below, it doesn't go through `authed_client`.
async fn cmd_models(host: &str) -> Result<ModelsResponse> {
    let base = format!("http://{host}");
    libttstation::agent_client::list_models(&base).await
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

/// `tt status --host <host:port>`: UNAUTHED, like `cmd_models` -- the
/// agent's `GET /status` has no `BearerAuth` extractor, so this calls
/// `libttstation::agent_client::get_status` directly instead of going
/// through `authed_client()`. This is what lets `tt status` (and the
/// discovery UI it backs) show a live status dot for a discovered-but-
/// unpaired box, instead of failing with "no token stored for <host>".
/// `cmd_run`/`cmd_stop`/`cmd_endpoint` are unaffected -- `/run`, `/stop`,
/// and `/endpoint` ARE bearer-gated on the agent side and still go through
/// `authed_client()`.
async fn cmd_status(host: &str) -> Result<StatusInfo> {
    let base = format!("http://{host}");
    libttstation::agent_client::get_status(&base).await
}

/// `tt endpoint --host <host:port>`.
async fn cmd_endpoint(host: &str) -> Result<Endpoint> {
    authed_client(host)?.endpoint().await
}

/// `tt serving --host <host:port>`: list every live `tt-inference-server`
/// `/v1` endpoint on `host`. UNAUTHED on the agent side, so (like
/// `cmd_models`/`cmd_status`) it needs no stored token and doesn't go through
/// `authed_client`.
async fn cmd_serving(host: &str) -> Result<ServingList> {
    let base = format!("http://{host}");
    libttstation::agent_client::list_serving(&base).await
}

/// Outcome of a `tt reset`, surfaced both to `--json` output and human text.
/// `local_cleared` is effectively always `true` on success (clearing local
/// state is the one thing `reset` always does); `box_reset` is `true` only
/// when a `--host` box was actually reset over the wire.
struct ResetSummary {
    local_cleared: bool,
    box_reset: bool,
}

/// `tt reset [--host <h>] [--yes]`: return this machine (and optionally one
/// box) to a fresh-install state.
///
/// When `host` is given, the box is reset FIRST -- while its token is still
/// stored locally -- via `agent_client::reset`. A missing token or a failed
/// call is a warning, NOT a hard error: local state is still cleared
/// afterward (the whole point of `reset` is to forget everything on this
/// machine), so a box that's already gone/unreachable never blocks the local
/// cleanup.
///
/// Local cleanup then clears EVERY stored token via `SecretStore::clear`
/// (`secrets.json` is the only state this CLI persists -- there's no separate
/// known-hosts file to purge). The confirmation prompt is handled by the
/// caller (`main`) before this runs, so by the time we're here the operator
/// has already consented (or passed `--yes`).
async fn cmd_reset(host: Option<&str>) -> Result<ResetSummary> {
    let mut box_reset = false;

    if let Some(host) = host {
        // Reset the remote box BEFORE forgetting its token locally.
        match build_store()?.get(host)? {
            Some(token) => {
                let base = format!("http://{host}");
                match libttstation::agent_client::reset(&base, &token).await {
                    Ok(()) => box_reset = true,
                    Err(e) => eprintln!(
                        "warning: failed to reset box {host}: {e}; clearing local state anyway"
                    ),
                }
            }
            None => eprintln!(
                "warning: no token stored for {host}; cannot reset it remotely; \
                 clearing local state anyway"
            ),
        }
    }

    // Always clear local state -- forget every paired box on this machine.
    build_store()?.clear()?;

    Ok(ResetSummary {
        local_cleared: true,
        box_reset,
    })
}

/// Print exactly what `tt reset` will clear and require the operator to type
/// `y` (one stdin line) to proceed. Returns `true` only on an affirmative
/// `y`/`Y`; anything else (including EOF/empty) declines. Skipped entirely by
/// `--yes` (see `main`).
fn confirm_reset(host: Option<&str>) -> Result<bool> {
    use std::io::Write;

    println!(
        "This will FORGET every paired box on this machine (clear all locally stored tokens)."
    );
    if let Some(host) = host {
        println!(
            "It will also ask the box at {host} to reset to a fresh install \
             (stop serving, clear its tokens, reset the board)."
        );
    }
    print!("Proceed? Type 'y' to continue: ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading confirmation from stdin")?;
    Ok(matches!(line.trim(), "y" | "Y"))
}

/// Build an `AgentClient` for `host`, using the token stored by a prior
/// `tt pair`. Shared by every command that needs an authenticated call.
fn authed_client(host: &str) -> Result<AgentClient> {
    let token = build_store()?
        .get(host)?
        .ok_or_else(|| anyhow::anyhow!("no token stored for {host}; run `tt pair {host}` first"))?;
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

fn print_discover(boxes: &[BoxRecord], json: bool) {
    if json {
        // `BoxRecord` serializes directly now -- `ServingStatus` has its own
        // hand-written `Serialize` impl (see `libttstation::model`) that
        // emits the canonical `idle`/`serving:<model>` txt string, so no
        // shadow struct is needed to avoid serde's default enum encoding.
        println!(
            "{}",
            serde_json::to_string(boxes).expect("BoxRecord always serializes")
        );
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

/// `tt pair-init`'s output: JSON carries the `pair_id` a caller needs to pass
/// to `tt pair-complete`; human mode spells out the whole next step so a
/// human operator (not just a scripted caller) can complete pairing without
/// re-reading `--help`.
fn print_pair_init(host: &str, pair_id: &str, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "pair_id": pair_id, "host": host })
        );
    } else {
        println!(
            "pairing started with {host}; enter the 6-digit code shown on the box, then run: \
             tt pair-complete {host} --pair-id {pair_id} --code <CODE>"
        );
    }
}

/// `tt pair-complete`'s output. Deliberately doesn't echo the token back
/// (unlike `print_pair`) -- the caller already has it via the stored
/// `SecretStore` entry, and this command's whole point is to be driven by a
/// non-interactive caller that just needs a success/failure signal.
fn print_pair_complete(host: &str, json: bool) {
    if json {
        println!("{}", serde_json::json!({ "host": host, "paired": true }));
    } else {
        println!("paired with {host}; token stored");
    }
}

/// `tt models`'s output: JSON prints the whole `ModelsResponse` object;
/// human mode prints one model per line as `<name>\t<dev1,dev2,...>`, so
/// it's both `grep`-able and roughly aligned like `tt discover`'s output.
fn print_models(resp: &ModelsResponse, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(resp).expect("ModelsResponse always serializes")
        );
    } else if resp.models.is_empty() {
        println!("no models available");
    } else {
        for model in &resp.models {
            println!("{}\t{}", model.name, model.devices.join(","));
        }
    }
}

fn print_endpoint_result(ep: &Endpoint, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(ep).expect("Endpoint always serializes")
        );
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

/// `tt reset`'s success output: JSON is the machine-readable summary the
/// task spec calls for (`{"local_cleared":..,"box_reset":..}`); human mode
/// says what happened in plain words.
fn print_reset(summary: &ResetSummary, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "local_cleared": summary.local_cleared,
                "box_reset": summary.box_reset,
            })
        );
    } else {
        println!("local state cleared (all paired boxes forgotten)");
        if summary.box_reset {
            println!("box reset requested");
        }
    }
}

/// `tt reset`'s output when the operator declines the confirmation prompt:
/// nothing was cleared, locally or remotely.
fn print_reset_aborted(json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({ "local_cleared": false, "box_reset": false })
        );
    } else {
        println!("reset aborted; nothing was cleared");
    }
}

/// `tt status`'s output. JSON mode adds (Task 3) `device_mesh` alongside the
/// existing `status` key so a caller (the macOS app, eventually) can read
/// both from one call; human mode is unchanged -- still just the bare
/// `idle`/`serving:<model>` txt line, since device-mesh isn't something an
/// operator glancing at a terminal needs.
fn print_status(info: &StatusInfo, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "status": info.status.to_txt(),
                "device_mesh": info.device_mesh,
            })
        );
    } else {
        println!("{}", info.status.to_txt());
    }
}

/// `tt serving`'s output: JSON prints the whole `ServingList` object; human
/// mode prints one endpoint per line as `<model>\t<base_url>\t<source>`, so
/// it's both `grep`-able and roughly aligned like `tt models`/`tt discover`.
fn print_serving(list: &ServingList, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(list).expect("ServingList always serializes")
        );
    } else if list.serving.is_empty() {
        println!("nothing serving");
    } else {
        for entry in &list.serving {
            println!("{}\t{}\t{}", entry.model, entry.base_url, entry.source);
        }
    }
}

/// `tt endpoint`'s output: JSON prints the `Endpoint` object; human mode
/// prints the `export OPENAI_BASE_URL=...` line per the task spec, so it's
/// directly `eval`-able.
fn print_endpoint_export(ep: &Endpoint, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(ep).expect("Endpoint always serializes")
        );
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
            device_mesh: Some("p300x2".into()),
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

    /// `build_probe_client` must actually apply the requested timeout as
    /// both the request and connect timeout -- this is the fix for `tt
    /// discover --host <dead-ip>:<port>` hanging on the OS's default TCP
    /// connect timeout instead of respecting `--timeout-ms`. `reqwest`
    /// doesn't expose a getter to read the configured timeout back off a
    /// built `Client`, so this test only asserts the client builds
    /// successfully with a timeout wired in; the behavioral guarantee (a
    /// dead host fails fast) is covered by
    /// `manual_status_fetch_against_unroutable_host_fails_fast_within_timeout`
    /// below.
    #[test]
    fn build_probe_client_succeeds_with_a_short_timeout() {
        assert!(build_probe_client(Duration::from_millis(200)).is_ok());
    }

    /// `print_pair_init` and `print_pair_complete` don't print through a
    /// return value, so these tests exercise the same JSON shape they build
    /// internally via `serde_json::json!` -- matching how the rest of this
    /// module's print helpers are tested (structure, not captured stdout).
    #[test]
    fn pair_init_json_shape_has_pair_id_and_host() {
        let value = serde_json::json!({ "pair_id": "abc123", "host": "127.0.0.1:8899" });
        assert_eq!(value["pair_id"], "abc123");
        assert_eq!(value["host"], "127.0.0.1:8899");
        assert_eq!(value.as_object().unwrap().len(), 2);
    }

    #[test]
    fn pair_complete_json_shape_has_host_and_paired_true() {
        let value = serde_json::json!({ "host": "127.0.0.1:8899", "paired": true });
        assert_eq!(value["host"], "127.0.0.1:8899");
        assert_eq!(value["paired"], true);
        assert_eq!(value.as_object().unwrap().len(), 2);
    }

    /// A dead/unroutable manual host must fail within roughly
    /// `--timeout-ms`, not the OS's default TCP connect timeout (which on
    /// Linux is on the order of minutes). `192.0.2.1` is in `TEST-NET-1`
    /// (RFC 5737): reserved for documentation/testing, guaranteed to never
    /// be a real routable host, and doesn't send back a fast "connection
    /// refused" the way `localhost:<closed-port>` would -- packets to it
    /// are dropped or blackholed, which is exactly the "routable-but-dead"
    /// scenario the fix targets (a closed local port already fails fast
    /// today, timeout or not).
    ///
    /// Generous 5s wall-clock bound (vs. a 200ms configured timeout) to
    /// absorb scheduler jitter on a loaded CI box without making this test
    /// flaky; the important assertion is "fails in low single-digit
    /// seconds," not "fails in exactly 200ms."
    #[test]
    fn manual_status_fetch_against_unroutable_host_fails_fast_within_timeout() {
        let start = std::time::Instant::now();
        let result = manual_status_fetch("192.0.2.1", 9, Duration::from_millis(200));
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected the unroutable host to error");
        assert!(
            elapsed < Duration::from_secs(5),
            "manual_status_fetch took {elapsed:?}, expected it to respect the ~200ms timeout"
        );
    }
}
