//! `tt-station-agentd`: the box-side daemon that runs on a QuietBox.
//!
//! Bootstraps shared `AppState`, advertises the box on the LAN via mDNS
//! (`_tenstorrent._tcp`, same TXT-record shape `mock-box` uses -- see Task
//! 3), and serves the HTTP control-plane API (`GET /status` today; pairing
//! routes, control routes, and a real serving backend arrive in Tasks
//! 7/9/10 and extend `AppState` rather than replacing it).
//!
//! Keep this file to bootstrap only: parse args, build state, spawn mDNS,
//! serve. Route handlers live in `routes.rs`.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use libttstation::discovery::SERVICE_TYPE;
use libttstation::model::{txt_encode, BoxRecord, ServingStatus};
use mdns_sd::{ServiceDaemon, ServiceInfo};

use tt_station_agentd::routes::{app, AppState};

/// Which serving backend to use for running models. Only the *choice* is
/// wired up in Task 6 -- actually dispatching to Docker or dstack arrives
/// in Task 9.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Docker,
    Dstack,
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Docker => write!(f, "docker"),
            Backend::Dstack => write!(f, "dstack"),
        }
    }
}

#[derive(Parser)]
#[command(name = "tt-station-agentd", about = "Box-side daemon for a Tenstorrent QuietBox")]
struct Cli {
    /// Box name; used as both the mDNS instance name and the `name` TXT/JSON key.
    #[arg(long)]
    name: String,

    /// Control-plane HTTP port to listen on and advertise in the `ctrl` TXT key.
    #[arg(long = "ctrl-port")]
    ctrl_port: u16,

    /// Which serving backend to use. Backend dispatch itself lands in Task 9;
    /// for now the choice is just parsed and stored on `AppState`.
    #[arg(long, value_enum, default_value_t = Backend::Docker)]
    backend: Backend,

    /// Chip inventory string advertised in the `chips` TXT key and returned
    /// from `/status`.
    #[arg(long, default_value = "4xBH")]
    chips: String,

    /// API version advertised in the `apiver` TXT key.
    #[arg(long, default_value_t = 1)]
    apiver: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let state = AppState::new(cli.name.clone(), cli.chips.clone(), cli.backend.to_string());

    // Bind the control-plane socket FIRST, then advertise on the LAN, so
    // discovery never races ahead of the control-plane API actually being
    // reachable.
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cli.ctrl_port))
        .await
        .with_context(|| format!("failed to bind control port {}", cli.ctrl_port))?;

    // Advertise the box's current status (read from the same `AppState` that
    // backs `/status`) so the mDNS TXT record and the HTTP status endpoint
    // can never desync at boot -- there's exactly one source of truth.
    let _mdns_guard =
        advertise(&cli, state.status()).context("failed to start mDNS advertisement")?;

    println!(
        "tt-station-agentd: '{}' serving on port {} (backend={}, chips={})",
        cli.name, cli.ctrl_port, cli.backend, cli.chips
    );

    // Serve until a shutdown signal arrives, then return normally so
    // `_mdns_guard` drops and unregisters the mDNS service. Without this,
    // the usual way to stop a daemon (SIGINT/SIGTERM) would kill the
    // process before Rust destructors run, leaving the box falsely
    // advertised until the mDNS TTL expires.
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("agent HTTP server failed")?;

    println!("tt-station-agentd: shutdown signal received, unregistering mDNS and exiting");

    Ok(())
}

/// Resolves once a shutdown signal (Ctrl-C, or on Unix also SIGTERM) is
/// received, so it can be handed to `axum::serve(..).with_graceful_shutdown`.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Handle to the running mDNS advertisement. Unregisters and shuts down the
/// daemon on drop so the box cleanly disappears from discovery. `main`'s
/// graceful-shutdown handling (see `shutdown_signal`) ensures this drop runs
/// on a normal exit *and* on Ctrl-C/SIGTERM, not just process exit.
struct MdnsGuard {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for MdnsGuard {
    fn drop(&mut self) {
        if let Ok(receiver) = self.daemon.unregister(&self.fullname) {
            let _ = receiver.recv();
        }
        let _ = self.daemon.shutdown();
    }
}

/// Build a [`BoxRecord`] from CLI flags plus the box's current `status`,
/// encode it into mDNS TXT records via `libttstation`'s `txt_encode` (the
/// exact same helper `mock-box` uses, so the keys can't drift from what
/// `MdnsProvider` decodes), and register the `_tenstorrent._tcp` service
/// with the local mDNS responder.
///
/// `status` is passed in (rather than hardcoded) so the caller can source it
/// straight from the same `AppState` that backs `/status` -- one source of
/// truth for what the box's status is at boot.
fn advertise(cli: &Cli, status: ServingStatus) -> Result<MdnsGuard> {
    let host = format!("{}.local.", cli.name);
    let record = BoxRecord {
        name: cli.name.clone(),
        host: host.clone(),
        ctrl_port: cli.ctrl_port,
        chips: cli.chips.clone(),
        status,
        apiver: cli.apiver,
    };

    let txt_pairs = txt_encode(&record);
    let txt_refs: Vec<(&str, &str)> = txt_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let daemon = ServiceDaemon::new().context("failed to start mDNS daemon")?;

    // Empty address + enable_addr_auto() lets mdns-sd discover this host's
    // real LAN address(es) instead of us hardcoding one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &record.name,
        &host,
        "",
        cli.ctrl_port,
        &txt_refs[..],
    )
    .context("failed to build mDNS ServiceInfo")?
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    daemon
        .register(service_info)
        .context("failed to register mDNS service")?;

    Ok(MdnsGuard { daemon, fullname })
}
