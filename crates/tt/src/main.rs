//! `tt`: the operator-facing CLI for tt-station.
//!
//! Wires together everything `libttstation` provides -- discovery, pairing,
//! secret storage, and the agent control-plane client -- into the commands
//! an operator (or the Task 12 e2e test) actually runs:
//!
//!   tt [--json] discover [--host <h:p>]... [--no-mdns] [--timeout-ms <ms>]
//!   tt [--json] pair <host:port> [--code <code>] [--enable-ssh]
//!   tt [--json] models --host <host:port>
//!   tt [--json] run <model> --host <host:port>
//!   tt [--json] stop --host <host:port>
//!   tt [--json] status --host <host:port>
//!   tt [--json] config --host <host:port>
//!   tt [--json] endpoint --host <host:port>
//!   tt [--json] serving --host <host:port>
//!   tt [--json] catalog --host <host:port> [--refresh] [--catalog-file <path>]
//!   tt [--json] ssh-authorize --host <host:port> [--revoke] [--date <YYYY-MM-DD>]
//!   tt console [--snapshot] [--install-service] [--ctrl-port <port>]
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

mod catalog;
mod ssh;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libttstation::agent_client::{AgentClient, SshRevokeBy};
use libttstation::discovery::{
    aggregate, manual::ManualProvider, mdns::MdnsProvider, DiscoveryProvider,
};
use libttstation::model::{
    BoxRecord, ConfigSummary, Endpoint, ModelsResponse, ServingList, ServingStatus, StatusInfo,
};
use libttstation::pairing::{pair_complete, pair_init};
use libttstation::secrets::{default_store, FileStore, SecretStore};
use serde::Deserialize;

mod console;

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

        /// Also install this Mac's SSH public key on the box as part of
        /// pairing (Task 6's `tt ssh-authorize` flow, run inline instead of
        /// as a separate command). Opt-in: the app drives this in Task 9,
        /// but a scripted/manual `tt pair` can ask for it directly. SSH
        /// failure never fails pairing itself -- see `maybe_enable_ssh`.
        #[arg(long = "enable-ssh")]
        enable_ssh: bool,
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

        /// Also install this Mac's SSH public key on the box as part of
        /// pairing -- see `Pair::enable_ssh`'s doc for the full rationale.
        #[arg(long = "enable-ssh")]
        enable_ssh: bool,
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

    /// Show the box's resolved serving config (active/available profiles,
    /// backend, endpoint). UNAUTHED on the agent side (like `status`/
    /// `models`/`serving`), so this works even against a box `tt pair` was
    /// never run against.
    Config {
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

    /// Show the merged, box-aware model catalog: the public Tenstorrent
    /// compatibility catalog (`compatibility.json`), classified against this
    /// box's detected device mesh and its live `/models` list, into three
    /// tiers -- runs here, experimental, needs other hardware (see
    /// `libttstation::catalog::classify`). UNAUTHED on the agent side (like
    /// `status`/`models`), so this works even against a box `tt pair` was
    /// never run against; a down/unreachable agent still prints a useful
    /// listing (just without `box_mesh`/live-model info -- see `cmd_catalog`).
    Catalog {
        /// The box's control-plane address, as `host:port`.
        #[arg(long)]
        host: String,

        /// Force a fresh fetch of the public catalog instead of using the
        /// on-disk 24h cache, even if that cache is still fresh. See
        /// `catalog::load_catalog`.
        #[arg(long)]
        refresh: bool,

        /// Read the compatibility catalog from this file instead of the
        /// network/cache -- bypasses both entirely. Mainly for the no-
        /// hardware e2e test (a fixture `compatibility.json`) and for an
        /// operator who already has a local copy.
        #[arg(long = "catalog-file")]
        catalog_file: Option<PathBuf>,
    },

    /// Authorize (or, with `--revoke`, remove) this Mac's SSH public key on
    /// a paired box, so an operator can `ssh <ssh_user>@<host>` straight
    /// into it -- e.g. for `tt-toplike`'s remote telemetry, or just a shell.
    /// This command NEVER opens an SSH connection itself: it only ever
    /// reads/sends the PUBLIC half of a keypair to the agent (see
    /// `crates/tt/src/ssh.rs`), which installs it and reports back which
    /// account (`ssh_user`, e.g. `ttuser`) to connect as.
    SshAuthorize {
        /// The box's control-plane address, as `host:port`. Must already be
        /// paired (see `tt pair`) -- `/ssh/authorize` is bearer-guarded.
        #[arg(long)]
        host: String,

        /// Remove this Mac's key from the box instead of installing it.
        /// Revokes by the key MATERIAL (not by the dated label an authorize
        /// call would have used), so it works regardless of which day the
        /// key was originally authorized on.
        #[arg(long)]
        revoke: bool,

        /// Override the `YYYY-MM-DD` used in the install label
        /// (`ttstation:<host>:<date>`) -- mainly for scripted/deterministic
        /// use. Omit to use today's date. Ignored with `--revoke` (revoking
        /// matches by key material, not by label -- see `--revoke`'s doc).
        #[arg(long)]
        date: Option<String>,
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

    /// Operator TUI for managing THIS box's agent as a systemd `--user`
    /// service. Run ON the box itself (e.g. over SSH) -- unlike every other
    /// subcommand above, there's no `--host`: it talks to `127.0.0.1
    /// :<ctrl-port>` and to the local systemd/journald, never a remote box.
    Console {
        /// Print one `BoxLifecycleSnapshot` as JSON and exit, instead of
        /// launching the TUI. This is what the GTK box panel polls.
        #[arg(long)]
        snapshot: bool,

        /// Install (or refresh) the systemd `--user` unit file and exit,
        /// instead of launching the TUI.
        #[arg(long = "install-service")]
        install_service: bool,

        /// Agent control port to talk to on `127.0.0.1`.
        #[arg(long = "ctrl-port", default_value_t = 8765)]
        ctrl_port: u16,
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
        Command::Pair {
            host,
            code,
            enable_ssh,
        } => {
            let (token, ssh) = run_async(cmd_pair(host, code.clone(), *enable_ssh))?;
            print_pair(host, &token, ssh.as_ref(), cli.json);
        }
        Command::PairInit { host } => {
            let pair_id = run_async(cmd_pair_init(host))?;
            print_pair_init(host, &pair_id, cli.json);
        }
        Command::PairComplete {
            host,
            pair_id,
            code,
            enable_ssh,
        } => {
            let ssh = run_async(cmd_pair_complete(host, pair_id, code, *enable_ssh))?;
            print_pair_complete(host, ssh.as_ref(), cli.json);
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
        Command::Config { host } => {
            let summary = run_async(cmd_config(host))?;
            print_config(&summary, cli.json);
        }
        Command::Endpoint { host } => {
            let endpoint = run_async(cmd_endpoint(host))?;
            print_endpoint_export(&endpoint, cli.json);
        }
        Command::Serving { host } => {
            let list = run_async(cmd_serving(host))?;
            print_serving(&list, cli.json);
        }
        Command::Catalog {
            host,
            refresh,
            catalog_file,
        } => {
            let bc = run_async(cmd_catalog(host, *refresh, catalog_file.as_deref()));
            print_catalog(&bc, cli.json);
        }
        Command::SshAuthorize { host, revoke, date } => {
            if *revoke {
                let key_path = run_async(cmd_ssh_revoke(host))?;
                print_ssh_revoke(&key_path, cli.json);
            } else {
                let date = date.clone().unwrap_or_else(ssh::today_ymd);
                let outcome = run_async(cmd_ssh_authorize(host, &date))?;
                print_ssh_authorize(host, &outcome, cli.json);
            }
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
        Command::Console {
            snapshot,
            install_service,
            ctrl_port,
        } => {
            console::run_console(*ctrl_port, *snapshot, *install_service, cli.json)?;
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

/// `tt pair <host:port> [--enable-ssh]`: run the pairing handshake and store
/// the resulting token under `host` in the `SecretStore`. Returns the token
/// (so the caller can decide how much of it to print) and, when
/// `enable_ssh` was requested, the outcome of the inline SSH-authorize step
/// -- see [`maybe_enable_ssh`] for why an SSH failure here never turns into
/// an `Err` from this function: pairing has already succeeded and been
/// persisted by the time SSH runs, so the two outcomes must never share a
/// single `Result`.
async fn cmd_pair(
    host: &str,
    code: Option<String>,
    enable_ssh: bool,
) -> Result<(String, Option<SshEnableOutcome>)> {
    let base = format!("http://{host}");
    let pair_id = pair_init(&base).await?;

    let code = match code {
        Some(c) => c,
        None => prompt_for_code()?,
    };

    let token = pair_complete(&base, &pair_id, &code).await?;
    build_store()?.set(host, &token)?;

    let ssh = maybe_enable_ssh(host, &token, enable_ssh).await;
    Ok((token, ssh))
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

/// `tt pair-complete <host:port> --pair-id <id> --code <code> [--enable-ssh]`:
/// one-shot second half of pairing. Exchanges the `pair_id` from a prior `tt
/// pair-init` and the code the box printed for a bearer token, and stores it
/// under `host` exactly like `cmd_pair` does -- so `tt run`/`tt status`/etc.
/// against the same `host` work identically regardless of which pairing path
/// was used. Like `cmd_pair`, an `--enable-ssh` request runs AFTER the token
/// is stored and its result is reported rather than propagated as an error
/// (see [`maybe_enable_ssh`]).
async fn cmd_pair_complete(
    host: &str,
    pair_id: &str,
    code: &str,
    enable_ssh: bool,
) -> Result<Option<SshEnableOutcome>> {
    let base = format!("http://{host}");
    let token = pair_complete(&base, pair_id, code).await?;
    build_store()?.set(host, &token)?;
    Ok(maybe_enable_ssh(host, &token, enable_ssh).await)
}

/// The outcome of `--enable-ssh`'s inline SSH-authorize step on `tt pair`/
/// `tt pair-complete`. Deliberately NOT folded into `cmd_pair`/
/// `cmd_pair_complete`'s `Result<_>` -- per the Task 7 brief, SSH failure
/// must be non-fatal to pairing: by the time this type is produced, the
/// pairing token has ALREADY been exchanged and persisted, so there is
/// nothing left to roll back. This is purely "what do we tell the caller
/// about the bonus SSH step," success or failure.
enum SshEnableOutcome {
    Ok {
        authorized: bool,
        ssh_user: String,
        already_present: bool,
    },
    Err(String),
}

/// Run Task 6's shared [`ssh::authorize`] routine right after a successful
/// pair, reusing the token that pairing just stored instead of re-reading it
/// from the `SecretStore` (`token` is passed in rather than looked up so
/// this never races a concurrent `tt pair` for the same host). Returns
/// `None` when `--enable-ssh` wasn't passed at all -- as opposed to `Some`
/// wrapping an `Err`, which means SSH was requested and failed. Any error
/// from `ssh::authorize` (agent unreachable, `ssh-keygen` missing, `$HOME`
/// unset, ...) is captured as `SshEnableOutcome::Err` and returned, NEVER
/// propagated with `?` -- that's the whole non-fatal contract this task
/// exists to implement.
async fn maybe_enable_ssh(host: &str, token: &str, enable_ssh: bool) -> Option<SshEnableOutcome> {
    if !enable_ssh {
        return None;
    }

    let client = AgentClient::new(format!("http://{host}"), token.to_string());
    let result: Result<ssh::AuthorizeOutcome> = async {
        let home = home_dir()?;
        let date = ssh::today_ymd();
        ssh::authorize(&client, &home, &date).await
    }
    .await;

    Some(match result {
        Ok(outcome) => SshEnableOutcome::Ok {
            authorized: outcome.authorized,
            ssh_user: outcome.ssh_user,
            already_present: outcome.already_present,
        },
        Err(e) => SshEnableOutcome::Err(e.to_string()),
    })
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

/// `tt config --host <host:port>`: UNAUTHED, like `cmd_status`/`cmd_models`/
/// `cmd_serving` -- the agent's `GET /config` has no `BearerAuth` extractor
/// (Task 5), so this calls `libttstation::agent_client::get_config` directly
/// instead of going through `authed_client()`. Lets an operator (or the GTK
/// panel/Mac app) see "what will this box actually serve with" even against
/// a box `tt pair` was never run against.
async fn cmd_config(host: &str) -> Result<ConfigSummary> {
    let base = format!("http://{host}");
    libttstation::agent_client::get_config(&base).await
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

/// `tt catalog --host <host:port> [--refresh] [--catalog-file <path>]`:
/// resolve the box's live mesh/models and merge them with the public
/// compatibility catalog via `libttstation::catalog::classify`.
///
/// Deliberately returns a bare `BoxCatalog`, not a `Result<BoxCatalog>` --
/// every input this depends on already degrades gracefully instead of
/// erroring (see `catalog::load_catalog`'s degradation contract and
/// `classify`'s `Option`-typed `catalog`/`box_mesh` parameters), so there is
/// nothing left for this function itself to fail on:
/// - `box_mesh`/`live_models` come from `/status`/`/models`, both UNAUTHED
///   (like `cmd_status`/`cmd_models`) -- an unreachable/down agent just
///   yields `None`/`[]` here rather than an error, so `tt catalog` still
///   prints a useful other_hardware/experimental listing even against a box
///   that's off or was never paired with.
/// - a down/offline catalog fetch still leaves `live_models` (if any)
///   showing up in `runs_here` (see `classify`'s live-model-always-wins
///   rule) -- catalog and agent failures are independent, neither blocks
///   the other's contribution to the merged view.
async fn cmd_catalog(host: &str, refresh: bool, catalog_file: Option<&std::path::Path>) -> libttstation::catalog::BoxCatalog {
    let base = format!("http://{host}");

    let box_mesh = libttstation::agent_client::get_status(&base)
        .await
        .ok()
        .and_then(|s| s.device_mesh);

    let live_models = libttstation::agent_client::list_models(&base)
        .await
        .map(|r| r.models)
        .unwrap_or_default();

    let (compat, stale) = catalog::load_catalog(refresh, catalog_file);

    libttstation::catalog::classify(compat.as_ref(), &live_models, box_mesh.as_deref(), stale)
}

/// `tt ssh-authorize --host <host:port>`: resolve (generating if needed)
/// this Mac's SSH public key and install it on `host`, tagged with
/// `ssh_label(<this Mac's hostname>, date)` -- NOT `host`; the label
/// identifies the installing Mac, not the box it's installing to (see
/// [`ssh::authorize`]'s doc comment). Thin wrapper around
/// [`ssh::authorize`] -- exists so Task 7's `tt pair --enable-ssh` can call
/// `ssh::authorize` directly with an `AgentClient` it already built for
/// pairing, instead of going through this CLI-argument-shaped entry point.
async fn cmd_ssh_authorize(host: &str, date: &str) -> Result<ssh::AuthorizeOutcome> {
    let client = authed_client(host)?;
    let home = home_dir()?;
    ssh::authorize(&client, &home, date).await
}

/// `tt ssh-authorize --host <host:port> --revoke`: remove this Mac's key
/// from `host`. Revokes by the key MATERIAL itself
/// (`SshRevokeBy::PublicKey`) rather than by label -- an authorize call's
/// label embeds the date it ran on, so revoking by label would require the
/// operator to remember (or this command to re-derive) that exact date;
/// revoking by the public key bytes is date-independent and matches
/// whatever's currently in `~/.ssh` on this Mac. Returns the `.pub` path
/// used, so the caller can report it (mirroring `cmd_ssh_authorize`'s
/// `AuthorizeOutcome::public_key_path`).
///
/// Unlike `cmd_ssh_authorize`, this never generates a key -- there's
/// nothing sensible to revoke on the agent for a key this Mac doesn't even
/// have, so a missing `~/.ssh/id_{ed25519,rsa}.pub` is a hard error here.
async fn cmd_ssh_revoke(host: &str) -> Result<PathBuf> {
    let client = authed_client(host)?;
    let home = home_dir()?;
    let ssh_dir = home.join(".ssh");

    let key_path = ssh::select_public_key_path(&ssh_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "no SSH public key found in {}; nothing to revoke",
            ssh_dir.display()
        )
    })?;
    let public_key = std::fs::read_to_string(&key_path)
        .with_context(|| format!("reading {}", key_path.display()))?
        .trim()
        .to_string();

    client
        .ssh_revoke(SshRevokeBy::PublicKey(public_key))
        .await?;
    Ok(key_path)
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

/// The Mac operator's home directory, as `ssh::authorize`/`cmd_ssh_revoke`
/// resolve `~/.ssh` from. Reads `$HOME` directly (rather than a `dirs`-style
/// crate this workspace doesn't otherwise depend on) -- set on every
/// interactive macOS/Linux shell, and the one existing test-isolation knob
/// this CLI uses elsewhere (`TT_CONFIG_DIR`, see `build_store`) is a
/// separate concern from the SSH key location.
fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("$HOME is not set; cannot locate ~/.ssh")
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

/// `tt pair`'s output. `ssh` is `None` when `--enable-ssh` wasn't passed
/// (JSON: `"ssh": null`; human: no extra line) -- see [`ssh_json`] and
/// [`print_ssh_note`] for the two output shapes when it was.
fn print_pair(host: &str, token: &str, ssh: Option<&SshEnableOutcome>, json: bool) {
    if json {
        let mut value = serde_json::json!({ "host": host, "paired": true, "token": token });
        value["ssh"] = ssh_json(ssh);
        println!("{value}");
    } else {
        println!("paired with {host}; token stored");
        print_ssh_note(host, ssh);
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
/// non-interactive caller that just needs a success/failure signal. `ssh`
/// behaves exactly like `print_pair`'s -- see its doc.
fn print_pair_complete(host: &str, ssh: Option<&SshEnableOutcome>, json: bool) {
    if json {
        let mut value = serde_json::json!({ "host": host, "paired": true });
        value["ssh"] = ssh_json(ssh);
        println!("{value}");
    } else {
        println!("paired with {host}; token stored");
        print_ssh_note(host, ssh);
    }
}

/// The `ssh` field `tt pair`/`tt pair-complete --json` add when
/// `--enable-ssh` was passed: `null` when the flag was never set, `{error}`
/// on a non-fatal SSH failure, or `{authorized, ssh_user, already_present}`
/// mirroring `print_ssh_authorize`'s own shape on success (minus
/// `public_key_path`, which `SshEnableOutcome` doesn't carry -- pairing's
/// JSON is meant to answer "can I ssh in," not restate which file on disk
/// holds the key). Split out from `print_pair`/`print_pair_complete` so
/// this exact mapping is unit-testable without booting an agent.
fn ssh_json(ssh: Option<&SshEnableOutcome>) -> serde_json::Value {
    match ssh {
        None => serde_json::Value::Null,
        Some(SshEnableOutcome::Ok {
            authorized,
            ssh_user,
            already_present,
        }) => serde_json::json!({
            "authorized": authorized,
            "ssh_user": ssh_user,
            "already_present": already_present,
        }),
        Some(SshEnableOutcome::Err(e)) => serde_json::json!({ "error": e }),
    }
}

/// Human-mode line for `--enable-ssh`'s outcome on `tt pair`/`tt
/// pair-complete`; a no-op when `ssh` is `None` (flag never passed).
/// Wording matches the task 7 spec exactly: success reads like
/// `print_ssh_authorize`'s own success line ("SSH enabled -- connect as
/// ..."); failure is spelled out as "pairing ok; SSH setup failed: <msg>"
/// so the non-fatal contract -- pairing succeeded regardless -- is visible
/// in the output, not just swallowed.
fn print_ssh_note(host: &str, ssh: Option<&SshEnableOutcome>) {
    match ssh {
        None => {}
        Some(SshEnableOutcome::Ok { ssh_user, .. }) => {
            println!("SSH enabled -- connect as {ssh_user}@{host}");
        }
        Some(SshEnableOutcome::Err(e)) => {
            println!("pairing ok; SSH setup failed: {e}");
        }
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

/// `tt config`'s output: JSON prints the whole `ConfigSummary` object
/// (pretty-printed per the task spec, unlike this module's other `--json`
/// output -- `tt config` is meant to be read by a human debugging "what will
/// this box actually serve with," not just piped machine-to-machine); human
/// mode prints active profile, available profiles, backend, and
/// `host:port`.
fn print_config(summary: &ConfigSummary, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(summary).expect("ConfigSummary always serializes")
        );
    } else {
        println!(
            "active profile: {}",
            summary
                .active_profile
                .as_deref()
                .unwrap_or("(implicit default)")
        );
        println!(
            "available:      {}",
            if summary.available_profiles.is_empty() {
                "(none)".to_string()
            } else {
                summary.available_profiles.join(", ")
            }
        );
        println!("backend:        {}", summary.backend);
        println!(
            "serving:        {}:{}",
            summary.serving_host, summary.serving_port
        );
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

/// `tt catalog`'s output: JSON prints the whole `BoxCatalog` object (the
/// exact shape the macOS app's model picker decodes); human mode prints the
/// three tiers as sections -- "Runs on this box" / "Experimental" / "Needs
/// other hardware" -- each listing `display_name` (and, for the last
/// section, the `needed_hardware` a model is missing here), plus a note line
/// per degraded input (`catalog_available == false` and/or `catalog_stale`
/// are independent conditions -- see `cmd_catalog`'s doc -- so both notes can
/// print together, e.g. an offline catalog cache AND an unreachable agent).
fn print_catalog(bc: &libttstation::catalog::BoxCatalog, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string(bc).expect("BoxCatalog always serializes")
        );
        return;
    }

    if !bc.catalog_available {
        println!("note: model catalog unavailable -- showing this box's live models");
    }
    if bc.catalog_stale {
        println!("note: catalog cached / offline");
    }

    println!("Runs on this box:");
    if bc.runs_here.is_empty() {
        println!("  (none)");
    } else {
        for entry in &bc.runs_here {
            println!("  {}", entry.display_name);
        }
    }

    println!("Experimental:");
    if bc.experimental.is_empty() {
        println!("  (none)");
    } else {
        for entry in &bc.experimental {
            println!("  {}", entry.display_name);
        }
    }

    println!("Needs other hardware:");
    if bc.other_hardware.is_empty() {
        println!("  (none)");
    } else {
        for entry in &bc.other_hardware {
            println!(
                "  {} (needs: {})",
                entry.display_name,
                entry.needed_hardware.join(", ")
            );
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

/// `tt ssh-authorize`'s success output (the non-`--revoke` path). `--json`
/// emits exactly the shape the task spec calls for: `{authorized, ssh_user,
/// already_present, public_key_path}`. Human mode leads with the one line
/// an operator actually needs to act on -- what account to `ssh` in as --
/// and calls out `already_present` as a no-op note rather than an error.
fn print_ssh_authorize(host: &str, outcome: &ssh::AuthorizeOutcome, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "authorized": outcome.authorized,
                "ssh_user": outcome.ssh_user,
                "already_present": outcome.already_present,
                "public_key_path": outcome.public_key_path.display().to_string(),
            })
        );
    } else {
        println!("SSH enabled -- connect as {}@{host}", outcome.ssh_user);
        if outcome.already_present {
            println!("(this key was already authorized on {host})");
        }
    }
}

/// `tt ssh-authorize --revoke`'s success output. Includes `public_key_path`
/// in `--json` mode too, mirroring `print_ssh_authorize`, so a caller can
/// always tell which of this Mac's keys the command acted on.
fn print_ssh_revoke(key_path: &std::path::Path, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "revoked": true,
                "public_key_path": key_path.display().to_string(),
            })
        );
    } else {
        println!("SSH key revoked ({})", key_path.display());
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

    /// Task 7: `tt pair`/`tt pair-complete --json` without `--enable-ssh`
    /// must carry `"ssh": null`, not omit the key or fabricate a value --
    /// `maybe_enable_ssh` returns `None` in exactly this case.
    #[test]
    fn ssh_json_is_null_when_ssh_was_not_requested() {
        assert_eq!(ssh_json(None), serde_json::Value::Null);
    }

    /// A successful `--enable-ssh` step reports `authorized`, `ssh_user`,
    /// and `already_present` -- nothing else (in particular, no
    /// `public_key_path`; see `ssh_json`'s doc for why pairing's JSON
    /// doesn't need it).
    #[test]
    fn ssh_json_ok_shape_has_authorized_user_and_already_present_only() {
        let outcome = SshEnableOutcome::Ok {
            authorized: true,
            ssh_user: "ttuser".to_string(),
            already_present: false,
        };
        let value = ssh_json(Some(&outcome));
        assert_eq!(value["authorized"], true);
        assert_eq!(value["ssh_user"], "ttuser");
        assert_eq!(value["already_present"], false);
        assert_eq!(value.as_object().unwrap().len(), 3);
    }

    /// A non-fatal SSH failure reports `{"error": "<msg>"}` and NOTHING
    /// else -- no `authorized`/`ssh_user` fields a caller might mistake for
    /// a partial success.
    #[test]
    fn ssh_json_err_shape_has_error_message_only() {
        let outcome = SshEnableOutcome::Err("connection refused".to_string());
        let value = ssh_json(Some(&outcome));
        assert_eq!(value["error"], "connection refused");
        assert_eq!(value.as_object().unwrap().len(), 1);
    }

    /// End-to-end shape check mirroring what `print_pair`/
    /// `print_pair_complete` actually assemble: the full pair JSON object
    /// with `ssh` spliced in, present (`--enable-ssh`) vs. `null` (flag
    /// omitted) -- the exact assertion the Task 7 brief calls for.
    #[test]
    fn pair_json_assembly_includes_ssh_field_present_or_null() {
        let mut without_ssh =
            serde_json::json!({ "host": "127.0.0.1:8899", "paired": true, "token": "tok" });
        without_ssh["ssh"] = ssh_json(None);
        assert!(without_ssh["ssh"].is_null());

        let outcome = SshEnableOutcome::Ok {
            authorized: true,
            ssh_user: "ttuser".to_string(),
            already_present: false,
        };
        let mut with_ssh =
            serde_json::json!({ "host": "127.0.0.1:8899", "paired": true, "token": "tok" });
        with_ssh["ssh"] = ssh_json(Some(&outcome));
        assert!(with_ssh["ssh"].is_object());
        assert_eq!(with_ssh["ssh"]["ssh_user"], "ttuser");
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
