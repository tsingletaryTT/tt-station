//! `tt-station-tray` binary. For now it only prints its version; the tray UI arrives in later
//! tasks of the Linux tray plan.

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") | Some("-V") => println!("tt-station-tray {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!(
                "tt-station-tray {}: the tray UI is not built yet",
                env!("CARGO_PKG_VERSION")
            );
            std::process::exit(2);
        }
    }
}
