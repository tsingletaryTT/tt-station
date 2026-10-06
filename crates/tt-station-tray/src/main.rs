//! `tt-station-tray` binary.
//!
//! * no arguments (default): the tray icon, the 5-second poll that keeps its menu current, and
//!   the main window. The window opens at start only when no tray host is present or the tray
//!   could not start; otherwise it opens from the tray's "Open window". Closing the window
//!   leaves the tray running. A second launch raises the running instance's window
//!   (GApplication single instance, see [`window::run_app`]).
//! * `--no-window`: the tray only, with no GTK at all. With no tray host it says so on stderr
//!   and exits 1, because a tray-only app with no tray would run invisibly. "Open window" in
//!   this mode shows a message instead. It does not take part in single-instance detection.
//! * `--version` / `-V`: print the version.
//! * `--smoke`: headless self-check ([`window::smoke_check`] and [`app::smoke_check`]). Prints
//!   `ok` and exits 0, or prints the failure and exits 1. Never initialises GTK, never touches
//!   the session bus, so it runs in CI and over SSH.

use std::sync::Arc;
use std::time::Duration;

use tt_station_tray::app::{self, AppContext};
use tt_station_tray::window;

/// How long `main` waits for blocking work (an Open WebUI start, say) when the app quits.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

fn main() {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version") | Some("-V") => {
            println!("tt-station-tray {}", env!("CARGO_PKG_VERSION"))
        }
        Some("--smoke") => match window::smoke_check().and_then(|()| app::smoke_check()) {
            Ok(()) => println!("ok"),
            Err(e) => {
                eprintln!("tt-station-tray --smoke failed: {e:#}");
                std::process::exit(1);
            }
        },
        Some("--no-window") => {
            let rt = runtime();
            let ctx = Arc::new(
                AppContext::from_env(&window::default_registry_path()).with_no_window(true),
            );
            let code = rt.block_on(app::run_no_window(ctx));
            rt.shutdown_timeout(SHUTDOWN_GRACE);
            std::process::exit(code);
        }
        None => {
            // A multi-thread runtime: station calls and the tray run on its workers while GTK
            // owns the main thread (see the `window` and `app` module docs).
            let rt = runtime();
            let registry_path = window::default_registry_path();
            let ctx = Arc::new(AppContext::from_env(&registry_path));
            let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
            let handle = rt.handle().clone();
            let link = window::TrayLink {
                requests: ui_rx,
                on_startup: Box::new(move || {
                    // `sni_host::probe` inside needs a tokio context, which this task has.
                    handle.spawn(app::start_with_window(ctx, ui_tx));
                }),
            };
            let code = window::run_app(rt.handle().clone(), &registry_path, Some(link));
            rt.shutdown_timeout(SHUTDOWN_GRACE);
            std::process::exit(code.value());
        }
        Some(other) => {
            eprintln!(
                "tt-station-tray: unknown argument {other:?} (try --version, --smoke or --no-window)"
            );
            std::process::exit(2);
        }
    }
}

/// The multi-thread tokio runtime, or exit 1 with the reason.
fn runtime() -> tokio::runtime::Runtime {
    match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("tt-station-tray: could not start the async runtime: {e}");
            std::process::exit(1);
        }
    }
}
