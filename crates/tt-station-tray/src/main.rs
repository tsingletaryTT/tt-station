//! `tt-station-tray` binary.
//!
//! * `--version` / `-V`: print the version.
//! * `--smoke`: headless self-check of the window's logic ([`window::smoke_check`]). Prints
//!   `ok` and exits 0, or prints the failure and exits 1. Never initialises GTK, so it runs in
//!   CI and over SSH.
//! * no arguments: open the main window.
//!
//! The tray icon (`tray.rs`) is not started from here yet; wiring it to the window is a later
//! task of the Linux tray plan.

use tt_station_tray::window;

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") | Some("-V") => {
            println!("tt-station-tray {}", env!("CARGO_PKG_VERSION"))
        }
        Some("--smoke") => match window::smoke_check() {
            Ok(()) => println!("ok"),
            Err(e) => {
                eprintln!("tt-station-tray --smoke failed: {e:#}");
                std::process::exit(1);
            }
        },
        None => {
            // A multi-thread runtime: station calls run on its workers while GTK owns the
            // main thread (see `window` module docs).
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("tt-station-tray: could not start the async runtime: {e}");
                    std::process::exit(1);
                }
            };
            let code = window::run_app(rt.handle().clone(), &window::default_registry_path());
            drop(rt);
            std::process::exit(code.value());
        }
        Some(other) => {
            eprintln!("tt-station-tray: unknown argument {other:?} (try --version or --smoke)");
            std::process::exit(2);
        }
    }
}
