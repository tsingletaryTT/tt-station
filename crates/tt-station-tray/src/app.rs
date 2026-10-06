//! The tray's event loop: what each menu action does ([`dispatch`]), the 5-second poll that
//! keeps the menu current, and start-up (tray-host check, one-time notice, tray spawn).
//!
//! **Threads.** Everything here runs on the tokio runtime `main` owns. The window runs on the
//! GTK main thread (see [`crate::window`]). The two talk through one channel of [`UiRequest`]s,
//! from here to the GTK thread. Why that mechanism:
//!
//! * It is a `tokio::sync::mpsc` unbounded channel, which the tray already uses for menu clicks,
//!   so nothing new enters the dependency tree. tokio's channels do not need a tokio runtime to
//!   be awaited.
//! * The GTK side awaits `recv()` inside `glib::spawn_future_local`. A send wakes that task
//!   through its waker, which wakes the GLib main loop. Nothing polls on a timer. This is the
//!   same pattern `window::spawn_call` already uses for the opposite direction (awaiting a tokio
//!   `JoinHandle` from the GLib executor).
//! * `glib::MainContext::channel` (the usual answer in older gtk-rs) was removed in glib 0.19,
//!   and this tree has glib 0.20.
//!
//! **Which station an action acts on.** Every action carries a [`StationId`]. [`dispatch`] finds
//! that one station and calls only its methods. Stop goes through [`Station::stop`], which for
//! this machine already stops only the server the tray shows (see `local.rs`), and for a box
//! stops what that box's agent serves.
//!
//! **Restart and Run last.** Both use the run options the run sheet remembered in the registry
//! (`Registry::last_run`) for that station and model. The registry file is read fresh for each
//! action, because the window may have written it a moment ago.
//!
//! **Failures.** A failing station call becomes [`ActionOutcome::Message`] whose text holds the
//! error chain word for word (`{:#}`), so a 409's holder text or "managed by TT-Studio" reaches
//! the person unchanged. The app shows a message as a desktop notification ([`launchers::notify`]).
//!
//! **Not run on a desktop.** [`dispatch`], [`AppContext::refresh`], [`menu_for`] and
//! [`maybe_show_notice`] are tested (`tests/app_actions.rs`) against mock-box, a fake `tt` and
//! recording fakes. [`start_with_window`], [`run_no_window`] and [`run_loops`] start a real tray
//! and were written on a machine with a live desktop session where launching it was not allowed,
//! so they compile and pass clippy but have never been run.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libttstation::run_options::RunOptions;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use crate::launchers::{self, Runner, SystemRunner, DEFAULT_OPENWEBUI_PORT};
use crate::local::LocalStation;
use crate::menu_model::{build_menu, MenuAction, MenuItem};
use crate::pairing_flow::{open_store, token_for};
use crate::registry::Registry;
use crate::remote::RemoteStation;
use crate::sni_host::{self, HostStatus};
use crate::station::{ServingNow, Station, StationId, StationState};
use crate::tray::TtTray;
use crate::window::view_model::{mac_to_remember, station_with_registry_mac};

/// How often the poll loop refreshes every station.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long one station's `state()` may take in a poll before it is shown as unreachable.
/// Station calls have no timeout of their own (a box that is off can hold a TCP connect for a
/// long time), and without this one dead box would hold back every row and the menu itself.
/// Three seconds is well above a healthy agent's `/status` or a local `tt model ps`, and short
/// enough that the menu stays current with the 5-second poll.
pub const STATE_TIMEOUT: Duration = Duration::from_secs(3);

/// What [`dispatch`] wants the app to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Nothing more to do; the next poll shows the new state.
    Done,
    /// Show this text to the person (a desktop notification).
    Message(String),
    /// Open (or raise) the main window.
    OpenWindow,
    /// Leave the app. Not in the brief's list of three; `MenuAction::Quit` needs it.
    Quit,
}

/// Requests from the tray side to the GTK main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiRequest {
    /// Open or raise the main window.
    OpenWindow,
    /// Start-up finished. `open_window`: show the window now (no tray host, or the tray failed).
    /// `tray_running`: the tray icon is up, so closing the window must not end the app.
    Started {
        open_window: bool,
        tray_running: bool,
    },
    /// Quit the app.
    Quit,
    /// The tray side stopped (its icon went away). The GTK side releases its hold, so the
    /// process ends when no window is open and does not linger with neither tray nor window.
    TrayGone,
}

/// Sends a Wake-on-LAN packet. The real one broadcasts over UDP ([`UdpBroadcast`]); tests use a
/// recording sender and never touch the network.
pub trait WolSender: Send + Sync {
    fn send(&self, packet: &[u8]) -> std::io::Result<()>;
}

/// Broadcasts to `255.255.255.255:9`, the same send `tt-station wake` does.
pub struct UdpBroadcast;

impl WolSender for UdpBroadcast {
    fn send(&self, packet: &[u8]) -> std::io::Result<()> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;
        socket.send_to(packet, "255.255.255.255:9")?;
        Ok(())
    }
}

/// Everything [`dispatch`] and the poll loop need: the stations, where the registry lives, and
/// the handles for launching (process runner, Wake sender).
pub struct AppContext {
    registry_path: PathBuf,
    /// Rebuild the station list from the registry and token store on each poll. True for the
    /// real app (the window may add or pair a box); false for a fixed list in tests.
    follow_registry: bool,
    /// Index 0 is this machine when built by [`AppContext::from_env`]. Never locked across an
    /// `.await`.
    stations: Mutex<Vec<Arc<Station>>>,
    /// What `stations` was built from: (id, name, has token) per known box.
    built_from: Mutex<Vec<(String, String, bool)>>,
    /// The states from the most recent poll.
    states: Mutex<Vec<StationState>>,
    pub runner: Arc<dyn Runner>,
    pub wol: Arc<dyn WolSender>,
    /// Base data directory (`$XDG_DATA_HOME` or `~/.local/share`); opencode configs go under
    /// `tt-station/opencode/` in it.
    pub data_dir: PathBuf,
    /// Host port Open WebUI is published on.
    pub openwebui_port: u16,
    /// Whether this is a Wayland session (picks `wl-copy` for the clipboard).
    pub wayland: bool,
    /// Started with `--no-window`: there is no window to open.
    pub no_window: bool,
    /// Per-station limit on `state()` in [`AppContext::refresh`]; [`STATE_TIMEOUT`] by default.
    /// Tests set a short one.
    pub state_timeout: Duration,
}

impl AppContext {
    /// A context over a fixed list of stations (tests, `--smoke`).
    pub fn new(
        registry_path: &Path,
        stations: Vec<Station>,
        runner: Arc<dyn Runner>,
        wol: Arc<dyn WolSender>,
        data_dir: &Path,
    ) -> AppContext {
        AppContext {
            registry_path: registry_path.to_path_buf(),
            follow_registry: false,
            stations: Mutex::new(stations.into_iter().map(Arc::new).collect()),
            built_from: Mutex::new(Vec::new()),
            states: Mutex::new(Vec::new()),
            runner,
            wol,
            data_dir: data_dir.to_path_buf(),
            openwebui_port: DEFAULT_OPENWEBUI_PORT,
            wayland: false,
            no_window: false,
            state_timeout: STATE_TIMEOUT,
        }
    }

    /// The real app's context: this machine plus every box in the registry, the real process
    /// runner and the UDP Wake sender. `TTS_OPENWEBUI_PORT` overrides the Open WebUI port, as it
    /// does for the box panel.
    pub fn from_env(registry_path: &Path) -> AppContext {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_dir = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".local/share")))
            .unwrap_or_else(|| PathBuf::from("."));
        let mut ctx = AppContext::new(
            registry_path,
            Vec::new(),
            Arc::new(SystemRunner),
            Arc::new(UdpBroadcast),
            &data_dir,
        );
        ctx.follow_registry = true;
        ctx.wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        if let Some(p) = std::env::var("TTS_OPENWEBUI_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
        {
            ctx.openwebui_port = p;
        }
        ctx.sync_stations(&Registry::load(registry_path));
        ctx
    }

    /// Use `t` as the per-station poll limit in place of [`STATE_TIMEOUT`].
    pub fn with_state_timeout(mut self, t: Duration) -> AppContext {
        self.state_timeout = t;
        self
    }

    /// Mark the context as started with `--no-window`.
    pub fn with_no_window(mut self, no_window: bool) -> AppContext {
        self.no_window = no_window;
        self
    }

    /// The registry, read fresh from disk.
    pub fn registry(&self) -> Registry {
        Registry::load(&self.registry_path)
    }

    /// The station with `id`, if it is listed.
    pub fn station(&self, id: &StationId) -> Option<Arc<Station>> {
        self.stations
            .lock()
            .expect("stations mutex poisoned")
            .iter()
            .find(|s| s.id() == *id)
            .cloned()
    }

    /// The states from the most recent [`AppContext::refresh`].
    pub fn states(&self) -> Vec<StationState> {
        self.states.lock().expect("states mutex poisoned").clone()
    }

    /// Rebuild the station list when the registry's boxes or their pairing changed. Rebuilding
    /// only on change keeps each `RemoteStation`'s remembered contention text between polls.
    fn sync_stations(&self, reg: &Registry) {
        if !self.follow_registry {
            return;
        }
        let store = open_store();
        let key: Vec<(String, String, bool)> = reg
            .stations()
            .iter()
            .map(|k| {
                let paired = token_for(store.as_ref(), &k.id()).ok().flatten().is_some();
                (k.id().0, k.name.clone(), paired)
            })
            .collect();
        let mut built = self.built_from.lock().expect("built_from mutex poisoned");
        let mut stations = self.stations.lock().expect("stations mutex poisoned");
        if *built == key && !stations.is_empty() {
            return;
        }
        let mut list = vec![Arc::new(Station::Local(LocalStation::from_env()))];
        for k in reg.stations() {
            // A store read error counts as "not paired", as in the window.
            let token = token_for(store.as_ref(), &k.id()).ok().flatten();
            list.push(Arc::new(Station::Remote(RemoteStation::new(
                k.base(),
                k.name.clone(),
                token,
            ))));
        }
        *stations = list;
        *built = key;
    }

    /// Refresh every station's state, merge the registry's MAC into offline boxes, record a
    /// newly seen MAC, and keep the result for [`AppContext::states`].
    ///
    /// The stations are polled concurrently, each limited to `state_timeout`. A station that
    /// does not answer in time, or whose task failed, is shown as unreachable
    /// ([`Station::offline_state`]) and keeps its place in the list; the others are not held
    /// back. So one refresh takes at most about `state_timeout`.
    ///
    /// A new MAC is written with [`Registry::update`], which reads the file again under the
    /// registry lock after the polls finish. A station, run or removal the window saved while
    /// the polls ran is kept (the registry's doc names what this does not cover).
    pub async fn refresh(&self) -> Vec<StationState> {
        let reg = self.registry();
        self.sync_stations(&reg);
        let stations: Vec<Arc<Station>> = self
            .stations
            .lock()
            .expect("stations mutex poisoned")
            .clone();
        let limit = self.state_timeout;
        let tasks: Vec<_> = stations
            .iter()
            .map(|s| {
                let s = s.clone();
                tokio::spawn(async move { tokio::time::timeout(limit, s.state()).await })
            })
            .collect();
        let mut states = Vec::with_capacity(tasks.len());
        let mut new_macs: Vec<(StationId, String)> = Vec::new();
        for (station, task) in stations.iter().zip(tasks) {
            let state = match task.await {
                Ok(Ok(state)) => state,
                // Too slow: dropping the future cancels it (a local `tt` is killed on drop).
                Ok(Err(_elapsed)) => station.offline_state(),
                Err(e) => {
                    eprintln!(
                        "tt-station-tray: polling {} failed ({e}); showing it as unreachable",
                        station.id().0
                    );
                    station.offline_state()
                }
            };
            let known = reg.stations().iter().find(|k| k.id() == state.id).cloned();
            if let Some(k) = &known {
                if let Some(mac) = mac_to_remember(&state, k) {
                    new_macs.push((k.id(), mac));
                }
            }
            states.push(station_with_registry_mac(state, known.as_ref()));
        }
        if !new_macs.is_empty() {
            let saved = Registry::update(&self.registry_path, |r| {
                for (id, mac) in &new_macs {
                    r.set_mac(id, mac);
                }
            });
            if let Err(e) = saved {
                eprintln!("tt-station-tray: could not save the registry: {e:#}");
            }
        }
        *self.states.lock().expect("states mutex poisoned") = states.clone();
        states
    }
}

/// The menu for `states`, with "Run last model" taken from the registry's most recent model per
/// station.
pub fn menu_for(states: &[StationState], reg: &Registry) -> Vec<MenuItem> {
    build_menu(states, &|id| reg.last_model(id).map(str::to_string))
}

/// Whether the tray should ask for attention: some station's last run was refused.
pub fn needs_attention(states: &[StationState]) -> bool {
    states.iter().any(|s| s.reachable && s.contention.is_some())
}

/// Whether to open the window as soon as the app starts: when no tray host is present (an icon
/// would be invisible) or the tray did not start. Otherwise the window waits for "Open window".
pub fn open_window_at_start(host: HostStatus, tray_started: bool) -> bool {
    host == HostStatus::Missing || !tray_started
}

/// Act on one menu action. See the module docs for the rules.
pub async fn dispatch(action: MenuAction, ctx: &AppContext) -> ActionOutcome {
    match action {
        MenuAction::OpenWindow if ctx.no_window => ActionOutcome::Message(
            "This tray was started with --no-window, so there is no window to open. Quit it and \
             start tt-station-tray without --no-window."
                .into(),
        ),
        MenuAction::OpenWindow => ActionOutcome::OpenWindow,
        MenuAction::Quit => ActionOutcome::Quit,
        MenuAction::Stop(id) => {
            with_station(ctx, &id, |st| async move {
                failed_or_done("Stop", st.stop().await)
            })
            .await
        }
        MenuAction::Restart(id) => restart(ctx, &id).await,
        MenuAction::RunLast(id) => run_last(ctx, &id).await,
        MenuAction::Wake(id) => wake(ctx, &id),
        MenuAction::OpenWebUi(id) => {
            let port = ctx.openwebui_port;
            connect(ctx, &id, move |r, ep| {
                launchers::notify(
                    r.as_ref(),
                    "Starting Open WebUI. The first start downloads the image and can take a few minutes.",
                );
                launchers::launch_open_webui(r.as_ref(), &ep, port)
            })
            .await
        }
        MenuAction::OpenOpenCode(id) => {
            let data_dir = ctx.data_dir.clone();
            connect(ctx, &id, move |r, ep| {
                launchers::launch_opencode(r.as_ref(), &ep, &data_dir)
            })
            .await
        }
        MenuAction::CopyEndpoint(id) => {
            let wayland = ctx.wayland;
            connect(ctx, &id, move |r, ep| {
                launchers::copy_to_clipboard(r.as_ref(), &launchers::copy_text(&ep), wayland)
            })
            .await
        }
    }
}

/// `Done` on success, `Message("<verb> failed: <error chain>")` on failure.
fn failed_or_done<T>(verb: &str, r: anyhow::Result<T>) -> ActionOutcome {
    match r {
        Ok(_) => ActionOutcome::Done,
        Err(e) => ActionOutcome::Message(format!("{verb} failed: {e:#}")),
    }
}

fn not_listed(id: &StationId) -> ActionOutcome {
    ActionOutcome::Message(format!("The station {} is no longer listed.", id.0))
}

async fn with_station<F, Fut>(ctx: &AppContext, id: &StationId, f: F) -> ActionOutcome
where
    F: FnOnce(Arc<Station>) -> Fut,
    Fut: std::future::Future<Output = ActionOutcome>,
{
    match ctx.station(id) {
        Some(st) => f(st).await,
        None => not_listed(id),
    }
}

/// Stop what the station serves and start it again with the options remembered for it.
///
/// **Which name to run.** The served id (what `/v1/models` or `tt model ps` reports, such as
/// `meta-llama/Llama-3.1-8B-Instruct`) is often not the name the run sheet ran and remembered
/// (`Llama-3.1-8B-Instruct`). So the run name is the served id when the registry has options
/// under it, else the station's most recently run model ([`Registry::last_model`]). That second
/// case assumes the shown server is the one this app last started there; a server started by
/// another tool after that would be replaced by the remembered model.
///
/// **Order.** The run name is settled before anything is stopped. When there is none, Restart
/// refuses with a message and leaves the server running.
async fn restart(ctx: &AppContext, id: &StationId) -> ActionOutcome {
    let Some(st) = ctx.station(id) else {
        return not_listed(id);
    };
    let state = st.state().await;
    let Some(serving) = state.serving else {
        return ActionOutcome::Message(format!(
            "{} is not serving a model to restart.",
            state.name
        ));
    };
    let reg = ctx.registry();
    let run_name = if reg.last_run(id, &serving.model).is_some() {
        Some(serving.model.clone())
    } else {
        reg.last_model(id).map(str::to_string)
    };
    let Some(run_name) = run_name else {
        return ActionOutcome::Message(format!(
            "Restart needs the model name this app used to start {} on {}, and none is              remembered. The server was left running. Open the window to start it from there.",
            serving.model, state.name
        ));
    };
    let options = reg.last_run(id, &run_name).cloned().unwrap_or_default();
    if let Err(e) = st.stop().await {
        return ActionOutcome::Message(format!("Restart failed: {e:#}"));
    }
    failed_or_done("Restart", st.run(&run_name, false, &options).await)
}

/// Run the model most recently run on this station, with its remembered options.
async fn run_last(ctx: &AppContext, id: &StationId) -> ActionOutcome {
    let Some(st) = ctx.station(id) else {
        return not_listed(id);
    };
    let reg = ctx.registry();
    let Some(model) = reg.last_model(id).map(str::to_string) else {
        return ActionOutcome::Message(
            "No model has been run on this station from this app yet. Open the window to pick one."
                .into(),
        );
    };
    let options: RunOptions = reg.last_run(id, &model).cloned().unwrap_or_default();
    failed_or_done("Run", st.run(&model, false, &options).await)
}

/// Send a magic packet to the station's MAC: the registry's, else the last polled one.
fn wake(ctx: &AppContext, id: &StationId) -> ActionOutcome {
    let from_registry = ctx
        .registry()
        .stations()
        .iter()
        .find(|k| k.id() == *id)
        .and_then(|k| k.mac.clone());
    let mac = from_registry.or_else(|| {
        ctx.states()
            .into_iter()
            .find(|s| s.id == *id)
            .and_then(|s| s.mac)
    });
    let Some(mac) = mac else {
        return ActionOutcome::Message(format!(
            "The network address (MAC) of {} is not known, so it cannot be woken.",
            id.0
        ));
    };
    let Some(bytes) = libttstation::wol::parse_mac(&mac) else {
        return ActionOutcome::Message(format!("{mac:?} is not a valid MAC address."));
    };
    match ctx.wol.send(&libttstation::wol::magic_packet(bytes)) {
        Ok(()) => ActionOutcome::Message(format!(
            "Sent a wake-up packet to {}. It can take a minute to come online.",
            id.0
        )),
        Err(e) => ActionOutcome::Message(format!("Wake failed: {e}")),
    }
}

/// Run a Connect action for what the station serves now. The launcher work blocks (it waits for
/// docker and for Open WebUI's first start), so it runs on tokio's blocking pool.
async fn connect<F>(ctx: &AppContext, id: &StationId, f: F) -> ActionOutcome
where
    F: FnOnce(Arc<dyn Runner>, ServingNow) -> Result<String, String> + Send + 'static,
{
    let Some(st) = ctx.station(id) else {
        return not_listed(id);
    };
    let state = st.state().await;
    let Some(ep) = state.serving else {
        return ActionOutcome::Message(format!("{} is not serving a model.", state.name));
    };
    let runner = ctx.runner.clone();
    match tokio::task::spawn_blocking(move || f(runner, ep)).await {
        // Success and failure are both a line for the person to read.
        Ok(Ok(msg)) | Ok(Err(msg)) => ActionOutcome::Message(msg),
        Err(e) => ActionOutcome::Message(format!("The launcher stopped unexpectedly: {e}")),
    }
}

// ---------------------------------------------------------------------------------------------
// Start-up and the loops
// ---------------------------------------------------------------------------------------------

/// Show the one-time "no tray host" notice when [`sni_host::should_show_notice`] says so, and
/// record it with [`sni_host::mark_notice_shown`]. A failure to record is logged in one line and
/// otherwise ignored (the notice may repeat next time). Returns whether the notice was shown.
pub fn maybe_show_notice(
    r: &dyn Runner,
    host: HostStatus,
    os_release: &str,
    desktop: &str,
    config_dir: &Path,
) -> bool {
    let hint = sni_host::install_hint(os_release, desktop);
    let marker = sni_host::notice_marker_path(config_dir).exists();
    if !sni_host::should_show_notice(host, hint.as_deref(), marker) {
        return false;
    }
    if let Some(h) = &hint {
        launchers::notify(r, h);
    }
    if let Err(e) = sni_host::mark_notice_shown(config_dir) {
        eprintln!("tt-station-tray: could not record that the tray notice was shown: {e}");
    }
    true
}

/// The config directory the notice marker lives in: the registry file's directory.
fn config_dir(ctx: &AppContext) -> PathBuf {
    ctx.registry_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The tray host check with the notice, using this machine's `/etc/os-release` and
/// `XDG_CURRENT_DESKTOP`. Must be awaited on the tokio runtime (see [`sni_host::probe`]).
async fn probe_and_notify(ctx: &AppContext) -> HostStatus {
    let host = sni_host::probe().await;
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    maybe_show_notice(
        ctx.runner.as_ref(),
        host,
        &os_release,
        &desktop,
        &config_dir(ctx),
    );
    host
}

/// Start the SNI tray with the menu for the states known so far (none at start, which gives
/// Open window and Quit). It does not poll first: the first poll happens in [`run_loops`], so
/// a slow station cannot delay the tray or `Started`. Returns the handle and the receiver of
/// menu clicks, or the error text (no session bus, no watcher, no host).
async fn spawn_tray(
    ctx: &AppContext,
) -> Result<(ksni::Handle<TtTray>, UnboundedReceiver<MenuAction>), String> {
    use ksni::TrayMethods;
    let states = ctx.states();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tray = TtTray {
        items: menu_for(&states, &ctx.registry()),
        tx,
        needs_attention: needs_attention(&states),
    };
    tray.spawn()
        .await
        .map(|h| (h, rx))
        .map_err(|e| e.to_string())
}

/// Default mode, tray side. Probe the host, maybe show the notice, start the tray (skipped when
/// no host is present, since ksni would refuse anyway), tell the GTK thread whether to open the
/// window, then run the action and poll loops. No station is polled before `Started` is sent.
/// A tray that fails to start is logged in one line and the app carries on as a windowed app.
/// When the loops end (the tray went away) the GTK side gets [`UiRequest::TrayGone`].
pub async fn start_with_window(ctx: Arc<AppContext>, ui: UnboundedSender<UiRequest>) {
    let host = probe_and_notify(&ctx).await;
    let tray = if host == HostStatus::Missing {
        Err("no StatusNotifier host on the session bus".to_string())
    } else {
        spawn_tray(&ctx).await
    };
    if let Err(e) = &tray {
        eprintln!(
            "tt-station-tray: the tray icon did not start ({e}); continuing with the window only"
        );
    }
    let _ = ui.send(UiRequest::Started {
        open_window: open_window_at_start(host, tray.is_ok()),
        tray_running: tray.is_ok(),
    });
    if let Ok((handle, actions)) = tray {
        let ui2 = ui.clone();
        run_loops(ctx, handle, actions, move |req| {
            let _ = ui2.send(req);
        })
        .await;
        let _ = ui.send(UiRequest::TrayGone);
    }
}

/// `--no-window` mode. Returns the process exit code: 1 when there is no tray host or the tray
/// does not start (a tray-only app with no tray would run invisibly), 0 after Quit.
pub async fn run_no_window(ctx: Arc<AppContext>) -> i32 {
    let host = sni_host::probe().await;
    if host == HostStatus::Missing {
        eprintln!("tt-station-tray: no system tray host was found on this desktop, and --no-window needs one.");
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        if let Some(h) = sni_host::install_hint(&os_release, &desktop) {
            eprintln!("{h}");
        }
        return 1;
    }
    let (handle, actions) = match spawn_tray(&ctx).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tt-station-tray: the tray icon did not start: {e}");
            return 1;
        }
    };
    let quit = Arc::new(Notify::new());
    let q = quit.clone();
    tokio::select! {
        _ = run_loops(ctx, handle, actions, move |req| {
            if req == UiRequest::Quit {
                q.notify_one();
            }
        }) => {}
        _ = quit.notified() => {}
    }
    0
}

/// Where [`run_loops`] puts each new menu: the ksni handle in the app, a recorder in tests.
pub trait MenuPusher: Send + Sync + 'static {
    /// Replace the menu. Returns false when the tray is gone, which ends the poll loop.
    fn push(
        &self,
        items: Vec<MenuItem>,
        attention: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>;
}

impl MenuPusher for ksni::Handle<TtTray> {
    fn push(
        &self,
        items: Vec<MenuItem>,
        attention: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            self.update(|t| {
                t.items = items;
                t.needs_attention = attention;
            })
            .await
            .is_some()
        })
    }
}

/// The action loop and the poll loop, until the tray goes away.
///
/// Each click runs [`dispatch`] in its own task, so a slow action (Open WebUI waiting for its
/// first start) does not hold up Stop. After every action, and every [`POLL_INTERVAL`], the
/// stations are refreshed and the menu is pushed to the tray. `ui` receives `OpenWindow` and
/// `Quit`.
pub async fn run_loops(
    ctx: Arc<AppContext>,
    pusher: impl MenuPusher,
    mut actions: UnboundedReceiver<MenuAction>,
    ui: impl Fn(UiRequest) + Send + Sync + 'static,
) {
    let poke = Arc::new(Notify::new());
    let ui = Arc::new(ui);

    let poll = {
        let ctx = ctx.clone();
        let poke = poke.clone();
        async move {
            loop {
                let states = ctx.refresh().await;
                let menu = menu_for(&states, &ctx.registry());
                let attention = needs_attention(&states);
                if !pusher.push(menu, attention).await {
                    eprintln!("tt-station-tray: the tray icon stopped; menu updates end here");
                    return;
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                    _ = poke.notified() => {}
                }
            }
        }
    };

    let clicks = async move {
        while let Some(action) = actions.recv().await {
            let ctx = ctx.clone();
            let poke = poke.clone();
            let ui = ui.clone();
            tokio::spawn(async move {
                match dispatch(action, &ctx).await {
                    ActionOutcome::Done => {}
                    ActionOutcome::Message(m) => launchers::notify(ctx.runner.as_ref(), &m),
                    ActionOutcome::OpenWindow => ui(UiRequest::OpenWindow),
                    ActionOutcome::Quit => ui(UiRequest::Quit),
                }
                poke.notify_one();
            });
        }
    };

    // The poll loop ends when the tray is gone; the click loop when its channel closes. Either
    // one ending means the tray is no longer usable.
    tokio::select! {
        _ = poll => {}
        _ = clicks => {}
    }
}

/// The part of `--smoke` that covers this module: [`dispatch`] against a fixed, empty context in
/// a fresh temporary directory, with a runner and Wake sender that do nothing. No network, no
/// processes, no display.
pub fn smoke_check() -> anyhow::Result<()> {
    use anyhow::ensure;

    struct Inert;
    impl Runner for Inert {
        fn resolve(&self, _: &str) -> Option<PathBuf> {
            None
        }
        fn output(&self, _: &[String]) -> std::io::Result<launchers::RunOutput> {
            Err(std::io::Error::other("smoke: nothing runs"))
        }
        fn run_with_input(&self, _: &[String], _: &str) -> std::io::Result<bool> {
            Ok(false)
        }
        fn spawn_detached(&self, _: &[String]) -> std::io::Result<()> {
            Ok(())
        }
        fn wait_http_ok(&self, _: &str, _: Duration) -> bool {
            false
        }
    }
    impl WolSender for Inert {
        fn send(&self, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::Error::other("smoke: no network"))
        }
    }

    let dir =
        std::env::temp_dir().join(format!("tt-station-tray-app-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = (|| {
        let ctx = AppContext::new(
            &dir.join("tray-stations.json"),
            Vec::new(),
            Arc::new(Inert),
            Arc::new(Inert),
            &dir,
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let id = StationId("local".into());
        let open = rt.block_on(dispatch(MenuAction::OpenWindow, &ctx));
        ensure!(
            open == ActionOutcome::OpenWindow,
            "OpenWindow gave {open:?}"
        );
        let quit = rt.block_on(dispatch(MenuAction::Quit, &ctx));
        ensure!(quit == ActionOutcome::Quit, "Quit gave {quit:?}");
        let gone = rt.block_on(dispatch(MenuAction::Stop(id), &ctx));
        ensure!(
            matches!(&gone, ActionOutcome::Message(m) if m.contains("no longer listed")),
            "Stop on an unknown station gave {gone:?}"
        );
        let menu = menu_for(&[], &ctx.registry());
        ensure!(menu.len() == 2, "empty menu has {} items", menu.len());
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::ServingNow;

    #[test]
    fn window_opens_at_start_only_without_a_working_tray() {
        assert!(!open_window_at_start(HostStatus::Present, true));
        assert!(open_window_at_start(HostStatus::Missing, true));
        assert!(open_window_at_start(HostStatus::Present, false));
        assert!(open_window_at_start(HostStatus::Missing, false));
    }

    fn st(contention: Option<&str>, reachable: bool) -> StationState {
        StationState {
            id: StationId("b:1".into()),
            name: "b".into(),
            reachable,
            mesh: None,
            serving: Some(ServingNow {
                model: "m".into(),
                base_url: "http://b:1/v1".into(),
            }),
            paired: true,
            contention: contention.map(str::to_string),
            mac: None,
        }
    }

    #[test]
    fn attention_only_for_a_reachable_contended_station() {
        assert!(!needs_attention(&[]));
        assert!(!needs_attention(&[st(None, true)]));
        assert!(needs_attention(&[st(None, true), st(Some("held"), true)]));
        // An offline box's stale contention text is not shown, so it asks for nothing.
        assert!(!needs_attention(&[st(Some("held"), false)]));
    }
}
