//! The main window: Stations, Models and Logs pages behind a `gtk::StackSidebar`.
//!
//! **Plain GTK4, no libadwaita.** The design spec asked for GTK4 with libadwaita. The build
//! machine has the GTK 4.14 development files and no `libadwaita-dev` (and no root to install
//! it), so this uses `gtk4` 0.9 with the `v4_12` feature only. Nothing here depends on GNOME;
//! the window should look like a plain GTK app on KDE and XFCE.
//!
//! **Not seen.** This window was written on a headless machine. It compiles and passes clippy,
//! and every rule it shows comes from [`view_model`], which is unit tested. The window itself,
//! its layout, and every click path have never been displayed or exercised. The headless check
//! is [`smoke_check`] (`tt-station-tray --smoke`), which builds the view models and the registry
//! and never initialises GTK.
//!
//! **Threads.** GTK widgets live on the main thread. Station calls are `async` and use
//! `reqwest` and `tokio::process`, so they run on a tokio runtime that `main` owns.
//! [`spawn_call`] starts a future on that runtime and awaits its `JoinHandle` inside a
//! `glib::spawn_future_local` task, so the result comes back on the GTK main thread without
//! the main thread ever blocking. (`glib::MainContext::channel`, named in the brief, was removed
//! in glib 0.19; awaiting the join handle from the GLib executor does the same job.)
//!
//! Layout of the files:
//! * `view_model.rs`: all decisions, pure and tested.
//! * `stations_pane.rs`: the station list, Refresh, Stop, Add station.
//! * `models_pane.rs`: search, filters, grouped `ListView`, detail pane, "Run..." button.
//! * `run_sheet.rs`: the Run dialog with the Advanced expander and the command preview.
//! * `pair_dialog.rs`: Add station (address, then the 6-digit code).
//! * The Logs page is small and lives in this file.

pub mod models_pane;
pub mod pair_dialog;
pub mod run_sheet;
pub mod stations_pane;
pub mod view_model;

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gio, glib};
use gtk4 as gtk;

use crate::local::LocalStation;
use crate::pairing_flow::{open_store, token_for};
use crate::registry::Registry;
use crate::remote::RemoteStation;
use crate::station::{ModelRow, Station, StationState};

use view_model::{
    group_models, registry_path_from, station_with_registry_mac, ActionResult, ModelFilter,
    PreviewTarget, RunForm,
};

/// GApplication id. A second launch with the same id activates the running instance instead of
/// starting another window, which is the spec's "a second launch focuses the running instance".
pub const APP_ID: &str = "com.tenstorrent.ttstation.tray";

/// The registry file for this user, from the environment (see
/// [`view_model::registry_path_from`]).
pub fn default_registry_path() -> PathBuf {
    let var = |k: &str| std::env::var(k).ok();
    registry_path_from(
        var("TT_CONFIG_DIR").as_deref(),
        var("XDG_CONFIG_HOME").as_deref(),
        var("HOME").as_deref(),
    )
}

/// State shared by every page. Lives on the GTK main thread only (it is `Rc`, not `Arc`).
pub struct Shared {
    /// The tokio runtime station calls run on.
    pub rt: tokio::runtime::Handle,
    pub registry: RefCell<Registry>,
    /// Index 0 is always this machine; the rest follow the registry's order.
    pub stations: RefCell<Vec<Arc<Station>>>,
    /// Last known state per station, same order as `stations`. Filled by the Stations page.
    pub states: RefCell<Vec<Option<StationState>>>,
    /// The station the Models and Logs pages act on.
    pub selected: Cell<usize>,
    /// Called after `selected` or `states` changes, so pages can refresh their headings.
    listeners: RefCell<Vec<Box<dyn Fn()>>>,
}

pub type App = Rc<Shared>;

impl Shared {
    /// Build the station list from the registry: this machine first, then each known box with
    /// whatever pairing token the shared store holds for it.
    pub fn rebuild_stations(&self) {
        let store = open_store();
        let mut list = vec![Arc::new(Station::Local(LocalStation::from_env()))];
        for k in self.registry.borrow().stations() {
            // A store read error is treated as "not paired"; the Stations page shows that.
            let token = token_for(store.as_ref(), &k.id()).ok().flatten();
            list.push(Arc::new(Station::Remote(RemoteStation::new(
                k.base(),
                k.name.clone(),
                token,
            ))));
        }
        let n = list.len();
        *self.stations.borrow_mut() = list;
        *self.states.borrow_mut() = vec![None; n];
        if self.selected.get() >= n {
            self.selected.set(0);
        }
        self.notify();
    }

    /// The selected station and its last known state.
    pub fn current(&self) -> (Arc<Station>, Option<StationState>) {
        let i = self.selected.get();
        let st = self.stations.borrow()[i].clone();
        let state = self.states.borrow().get(i).cloned().flatten();
        (st, state)
    }

    /// Store a fresh state for station `i`, with the registry's MAC merged in for an offline
    /// box and a newly seen MAC written back to the registry.
    pub fn set_state(&self, i: usize, state: StationState) {
        let state = {
            let mut reg = self.registry.borrow_mut();
            // Station i >= 1 is registry entry i - 1 (see `rebuild_stations`).
            let known = i
                .checked_sub(1)
                .and_then(|k| reg.stations().get(k).cloned());
            if let Some(k) = &known {
                if let Some(mac) = view_model::mac_to_remember(&state, k) {
                    reg.set_mac(&k.id(), &mac);
                    if let Err(e) = reg.save() {
                        eprintln!("tt-station-tray: could not save the registry: {e:#}");
                    }
                }
            }
            station_with_registry_mac(state, known.as_ref())
        };
        if let Some(slot) = self.states.borrow_mut().get_mut(i) {
            *slot = Some(state);
        }
        self.notify();
    }

    pub fn on_change(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    pub fn notify(&self) {
        for f in self.listeners.borrow().iter() {
            f();
        }
    }

    /// A short heading naming the selected station, for the Models and Logs pages.
    pub fn current_heading(&self) -> String {
        let (_, state) = self.current();
        match state {
            Some(s) => format!("Station: {}", s.name),
            None => "Station: (not checked yet, press Refresh on Stations)".to_string(),
        }
    }
}

/// Run `fut` on the tokio runtime and hand its output to `done` on the GTK main thread.
///
/// `done` is not called if the tokio task panicked or was cancelled; the panic is printed.
pub fn spawn_call<T, F, D>(app: &App, fut: F, done: D)
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
    D: FnOnce(T) + 'static,
{
    let handle = app.rt.spawn(fut);
    glib::spawn_future_local(async move {
        match handle.await {
            Ok(v) => done(v),
            Err(e) => eprintln!("tt-station-tray: a background call failed: {e}"),
        }
    });
}

/// Show `r` in `label`: plain for success, with the `error` CSS class for a failure.
pub fn show_result(label: &gtk::Label, r: &ActionResult) {
    label.set_text(r.message());
    if r.is_error() {
        label.add_css_class("error");
    } else {
        label.remove_css_class("error");
    }
}

/// Start the GTK application and block until it quits. `rt` must outlive the call.
pub fn run_app(rt: tokio::runtime::Handle, registry_path: &Path) -> glib::ExitCode {
    let application = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::empty())
        .build();
    let registry_path = registry_path.to_path_buf();
    application.connect_activate(move |application| {
        // A second activation (another launch) presents the existing window.
        if let Some(w) = application.active_window() {
            w.present();
            return;
        }
        let app: App = Rc::new(Shared {
            rt: rt.clone(),
            registry: RefCell::new(Registry::load(&registry_path)),
            stations: RefCell::new(Vec::new()),
            states: RefCell::new(Vec::new()),
            selected: Cell::new(0),
            listeners: RefCell::new(Vec::new()),
        });
        app.rebuild_stations();
        build_window(application, &app).present();
    });
    // GTK must not parse our own flags (`--smoke`, `--version`); `main` already did.
    application.run_with_args::<&str>(&[])
}

fn build_window(application: &gtk::Application, app: &App) -> gtk::ApplicationWindow {
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    stack.add_titled(&stations_pane::build(app), Some("stations"), "Stations");
    stack.add_titled(&models_pane::build(app), Some("models"), "Models");
    stack.add_titled(&logs_page(app), Some("logs"), "Logs");

    let sidebar = gtk::StackSidebar::new();
    sidebar.set_stack(&stack);

    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    root.append(&sidebar);
    root.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    root.append(&stack);

    let window = gtk::ApplicationWindow::builder()
        .application(application)
        .title("TT Station")
        .default_width(960)
        .default_height(640)
        .child(&root)
        .build();
    // Check every station once on open.
    stations_pane::refresh_all(app);
    window
}

/// The Logs page: the last 200 lines of the selected station's serving log, on demand.
///
/// The spec asks for a live follow of `/logs/stream` and `/telemetry`. This first version
/// fetches a tail when Refresh is pressed; following is not built yet.
fn logs_page(app: &App) -> gtk::Widget {
    let heading = gtk::Label::new(Some(&app.current_heading()));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");
    let refresh = gtk::Button::with_label("Refresh logs");
    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_selectable(true);
    let view = gtk::TextView::new();
    view.set_editable(false);
    view.set_monospace(true);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .build();

    {
        let heading = heading.clone();
        let weak = Rc::downgrade(app);
        app.on_change(move || {
            if let Some(app) = weak.upgrade() {
                heading.set_text(&app.current_heading());
            }
        });
    }
    {
        let app = app.clone();
        let status = status.clone();
        let view = view.clone();
        refresh.connect_clicked(move |_| {
            let (station, _) = app.current();
            status.set_text("Fetching logs...");
            let status = status.clone();
            let view = view.clone();
            spawn_call(&app, async move { station.logs(200).await }, move |r| {
                if let Ok(lines) = &r {
                    view.buffer().set_text(&lines.join("\n"));
                }
                show_result(&status, &ActionResult::from_result("Logs", &r));
            });
        });
    }

    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    top.append(&heading);
    top.append(&refresh);
    let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    set_margins(&page, 12);
    page.append(&top);
    page.append(&status);
    page.append(&scroll);
    page.upcast()
}

/// Same margin on all four sides.
pub fn set_margins(w: &impl IsA<gtk::Widget>, m: i32) {
    w.set_margin_top(m);
    w.set_margin_bottom(m);
    w.set_margin_start(m);
    w.set_margin_end(m);
}

/// The headless check behind `tt-station-tray --smoke`. Never touches GTK or a display.
///
/// It builds a registry in a fresh temporary directory, adds and saves a station, reloads it,
/// groups a small fixture of model rows, and builds a run form and its preview. It returns an
/// error naming the first thing that did not hold. It proves the window's logic links and runs
/// in the shipped binary; it proves nothing about the widgets.
pub fn smoke_check() -> anyhow::Result<()> {
    use anyhow::{bail, ensure};
    use libttstation::run_options::run_schema;

    let dir = std::env::temp_dir().join(format!("tt-station-tray-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("tray-stations.json");
    let result = (|| {
        let mut reg = Registry::load(&path);
        reg.add_manual("qb2-smoke.local", view_model::DEFAULT_AGENT_PORT)
            .map_err(|e| anyhow::anyhow!("add_manual: {e}"))?;
        reg.save()?;
        let reg = Registry::load(&path);
        ensure!(reg.stations().len() == 1, "registry did not round-trip");

        let row = |name: &str, source: &str, fits: bool| ModelRow {
            name: name.into(),
            kind: Some("llm".into()),
            source: source.into(),
            hardware: vec!["p300x2".into()],
            cached: true,
            fits,
        };
        let rows = [
            row("Llama-3.1-8B-Instruct", "tt-inference-server", true),
            row("acme/bundle", "HuggingFace", true),
            row("Big-Model", "tt-inference-server", false),
        ];
        let g = group_models(&rows, &ModelFilter::default(), true);
        ensure!(
            g.runs_here.len() == 1 && g.experimental.len() == 1 && g.other_hardware.len() == 1,
            "group_models gave {g:?}"
        );
        let g = group_models(&rows, &ModelFilter::default(), false);
        ensure!(g.unknown_fit.len() == 3, "unknown mesh grouping gave {g:?}");

        let mut form =
            RunForm::from_schema(&run_schema(), None).with_target(PreviewTarget::local("tt"));
        form.set("port", "20001");
        let preview = form.command_preview("Llama-3.1-8B-Instruct");
        if preview != "tt serve Llama-3.1-8B-Instruct --port 20001" {
            bail!("unexpected preview: {preview}");
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}
