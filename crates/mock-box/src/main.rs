//! `mock-box`: a development fixture that pretends to be a Tenstorrent box.
//!
//! Real Tenstorrent boxes advertise themselves on the LAN via mDNS so other
//! tools (the `tt` CLI, the station agent, etc.) can find them without any
//! manual configuration. This binary reproduces just that advertisement —
//! registering the `_tenstorrent._tcp` mDNS service with the same TXT-record
//! shape the real box would use — so the rest of tt-station (in particular
//! Task 4's mDNS `DiscoveryProvider`) can be built and tested without needing
//! physical hardware on hand.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use libttstation::model::{txt_encode, BoxRecord, ServingStatus};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// mDNS service type all Tenstorrent boxes (real and mocked) advertise under.
const SERVICE_TYPE: &str = "_tenstorrent._tcp.local.";

#[derive(Parser)]
#[command(name = "mock-box", about = "Pretend to be a Tenstorrent box for dev/test")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Advertise a fake Tenstorrent box via mDNS (_tenstorrent._tcp) until Ctrl-C.
    Advertise {
        /// Box name; used as both the mDNS instance name and the `name` TXT key.
        #[arg(long)]
        name: String,

        /// Control-plane port advertised in the `ctrl` TXT key.
        #[arg(long = "ctrl-port")]
        ctrl_port: u16,

        /// Chip inventory string advertised in the `chips` TXT key.
        #[arg(long, default_value = "4xBH")]
        chips: String,

        /// API version advertised in the `apiver` TXT key.
        #[arg(long, default_value_t = 1)]
        apiver: u8,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Advertise {
            name,
            ctrl_port,
            chips,
            apiver,
        } => advertise(name, ctrl_port, chips, apiver).await,
    }
}

/// Build a [`BoxRecord`] from CLI flags, encode it into mDNS TXT records via
/// `libttstation`'s `txt_encode` (so the keys stay byte-for-byte compatible
/// with what the real DiscoveryProvider decoder expects), and register it
/// with the local mDNS responder. Runs until Ctrl-C, then unregisters
/// cleanly so peers see the service disappear.
async fn advertise(name: String, ctrl_port: u16, chips: String, apiver: u8) -> Result<()> {
    let host = format!("{name}.local.");
    let record = BoxRecord {
        name: name.clone(),
        host: host.clone(),
        ctrl_port,
        chips,
        status: ServingStatus::Idle,
        apiver,
    };

    // Reuse the shared encoder so the advertised TXT keys (name, apiver,
    // chips, status, ctrl) exactly match what Task 4's decoder expects.
    let txt_pairs = txt_encode(&record);
    let txt_refs: Vec<(&str, &str)> = txt_pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mdns = ServiceDaemon::new().context("failed to start mDNS daemon")?;

    // Passing an empty address list + enable_addr_auto() lets mdns-sd figure
    // out this host's real LAN address(es) instead of us hardcoding one.
    let service_info = ServiceInfo::new(
        SERVICE_TYPE,
        &record.name,
        &host,
        "",
        ctrl_port,
        &txt_refs[..],
    )
    .context("failed to build mDNS ServiceInfo")?
    .enable_addr_auto();

    let fullname = service_info.get_fullname().to_string();
    mdns.register(service_info)
        .context("failed to register mDNS service")?;

    println!(
        "mock-box: advertising '{}' as {} (service type {}) on port {}",
        record.name, fullname, SERVICE_TYPE, ctrl_port
    );
    println!("mock-box: TXT records: {:?}", txt_pairs);
    println!("mock-box: press Ctrl-C to stop advertising");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for ctrl-c")?;

    println!("mock-box: Ctrl-C received, unregistering and shutting down");
    if let Ok(receiver) = mdns.unregister(&fullname) {
        // Best-effort wait for the unregister to be flushed so any browsers
        // on the LAN get a proper goodbye packet before we exit.
        let _ = receiver.recv();
    }
    let _ = mdns.shutdown();

    Ok(())
}
