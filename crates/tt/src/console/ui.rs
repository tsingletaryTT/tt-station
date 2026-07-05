//! The interactive `tt console` TUI -- Task 7.
//!
//! Layout is display logic (pure) plus a thin ratatui/crossterm shell:
//!
//!   - [`header_lines`], [`pairing_lines`], [`status_lines`] and
//!     [`serving_lines`] are PURE functions of a [`BoxLifecycleSnapshot`]:
//!     no I/O, no terminal, no wall clock. They hold all the "what does this
//!     panel say" logic, and are exhaustively unit-tested without a real
//!     terminal (see the `tests` module -- the `TestBackend` render test
//!     also exercises [`draw`] itself).
//!   - [`draw`] lays the four panels + a keybinding footer out with
//!     `ratatui::layout::Layout` and renders each as a `Paragraph` in a
//!     `Block`. Per the owner's global terminal-UI rule (see
//!     `~/CLAUDE.md`'s "Terminal / TUI output" section): **left/bottom
//!     borders only, never right-side** -- a right border is the one that
//!     breaks first when an SSH session's terminal is narrower than the
//!     layout assumes, so every `Block` here uses exactly
//!     `Borders::LEFT | Borders::BOTTOM`.
//!   - [`run_tui`] is the only impure piece: crossterm raw mode + alternate
//!     screen, a ~1s poll/redraw loop, and the operator keybindings. Kept
//!     deliberately small (state transitions only) so the panels above carry
//!     the logic that's worth testing.
//!
//! # Terminal teardown (read this before touching `run_tui`)
//!
//! An operator runs this over SSH. If a bug (or a panic) leaves the remote
//! terminal in raw mode / stuck on the alternate screen, that's not just an
//! annoyance -- the operator's shell can become unusable until they blind-type
//! `reset<Enter>` or reconnect. [`TerminalGuard`] exists solely to make that
//! impossible: it flips the terminal into raw+alternate-screen mode in its
//! constructor and flips it back in `Drop`. Because `Drop::drop` runs during
//! *any* unwind -- an early `?`-return, a `break`, or an actual panic (as
//! long as the process doesn't `panic = "abort"`, which this workspace
//! doesn't set) -- teardown happens no matter how `run_tui` exits. Nothing
//! in [`run_tui`] should ever restore the terminal by hand; that would just
//! create a second code path that could get out of sync with this one.
#![allow(dead_code)]

use crate::console::actions::LifecycleActions;
use crate::console::env::{collect_snapshot, LifecycleEnv};
use crate::console::names::ToolNames;
use crate::console::state::{derive_state, LifecycleState};

use libttstation::model::{BoxLifecycleSnapshot, ServiceState};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

use std::io;
use std::time::{Duration, Instant};

/// Tenstorrent brand teal (`#4fd1c5`), per the owner's global brand-colors
/// note -- the editor/IDE-surface variant (teal on deep blue-gray), which is
/// the closer match for a terminal tool than the docs-site's dark-forest
/// palette.
const TEAL: Color = Color::Rgb(0x4f, 0xd1, 0xc5);

// ---------------------------------------------------------------------
// Pure line-builders -- see the module doc for why these carry the actual
// display logic instead of `draw` itself.
// ---------------------------------------------------------------------

/// Header panel: box name + systemd service state on the first line,
/// hardware/profile on the second.
///
/// `mesh` is sourced from `snapshot.chips` (e.g. `"4xBH"`) rather than a
/// `device_mesh` field on `ServingStatus` -- `BoxLifecycleSnapshot` has no
/// such field (see `libttstation::model`); `chips` is the closest hardware
/// signal the snapshot actually carries.
pub fn header_lines(s: &BoxLifecycleSnapshot) -> Vec<String> {
    let name = s.name.clone().unwrap_or_else(|| "tt-station".into());
    let svc = service_label(&s.service);
    let mesh = s.chips.clone().unwrap_or_else(|| "unknown chips".into());
    let profile = s
        .config
        .as_ref()
        .and_then(|c| c.active_profile.clone())
        .unwrap_or_else(|| "\u{2014}".into()); // em dash: no profile pinned
    vec![
        format!("tt-station \u{b7} {name}          \u{25cf} service: {svc}"),
        format!("{mesh} \u{b7} profile: {profile}"),
    ]
}

fn service_label(s: &ServiceState) -> &'static str {
    match s {
        ServiceState::Active => "active",
        ServiceState::Inactive => "inactive",
        ServiceState::Activating => "activating",
        ServiceState::Deactivating => "deactivating",
        ServiceState::Failed => "failed",
        ServiceState::Unknown => "unknown",
    }
}

/// Pairing card: the box's current pairing code (spaced for readability, but
/// keeping the raw 6 digits contiguous so on-screen search/copy still finds
/// the exact code) and its remaining TTL, or a "nothing pending" hint.
pub fn pairing_lines(s: &BoxLifecycleSnapshot) -> Vec<String> {
    match &s.pairing {
        Some(p) => vec![
            format!("pairing code: {}", p.code),
            format!(
                "expires in {}s -- press 'p' to pair this box at 127.0.0.1",
                p.expires_in_secs
            ),
        ],
        None => vec![
            "no active pairing code".to_string(),
            "start a pairing on the box (or the GTK panel), then press 'p'".to_string(),
        ],
    }
}

/// Status panel: the derived operator-facing [`LifecycleState`] plus the
/// endpoint the box is currently exposing (if any). Live `/serving` detail
/// is its own panel -- see [`serving_lines`] -- so this stays a short,
/// at-a-glance summary.
pub fn status_lines(s: &BoxLifecycleSnapshot) -> Vec<String> {
    let mut lines = vec![format!(
        "state: {}",
        lifecycle_state_label(&derive_state(s))
    )];
    match &s.endpoint {
        Some(ep) => lines.push(format!("endpoint: {} ({})", ep.base_url, ep.model)),
        None => lines.push("endpoint: (none)".to_string()),
    }
    lines
}

fn lifecycle_state_label(state: &LifecycleState) -> String {
    match state {
        LifecycleState::Inactive => "inactive".to_string(),
        LifecycleState::Starting => "starting".to_string(),
        LifecycleState::Idle => "idle".to_string(),
        LifecycleState::Serving(model) => format!("serving {model}"),
        LifecycleState::Stopping => "stopping".to_string(),
        LifecycleState::Failed => "failed".to_string(),
    }
}

/// Serving panel: every live `tt-inference-server` `/v1` endpoint the agent's
/// `/serving` route reported (agent-launched or external/tt-studio), or a
/// "nothing serving" line when empty.
pub fn serving_lines(s: &BoxLifecycleSnapshot) -> Vec<String> {
    if s.serving.is_empty() {
        return vec!["/serving: none".to_string()];
    }
    let mut lines = vec![format!("/serving: {} endpoint(s)", s.serving.len())];
    for entry in &s.serving {
        lines.push(format!(
            "  - {} [{}] {} ({})",
            entry.model, entry.source, entry.base_url, entry.container
        ));
    }
    lines
}

/// Static keybinding footer -- not a function of the snapshot, but kept
/// alongside the other line-builders since [`draw`] renders it the same way.
fn footer_lines() -> Vec<String> {
    vec![
        "s start  x stop  r restart  R reset  p pair  c profile  i install  q/Esc quit".to_string(),
    ]
}

// ---------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------

/// Lay out the header / pairing / status / serving / footer panels and
/// render each as a `Paragraph` in a `Block`. Pure with respect to its
/// inputs (no I/O) even though it isn't unit-testable the same way the line
/// builders are -- see the `renders_without_panicking` test, which drives it
/// through a ratatui `TestBackend` instead of a real terminal.
pub fn draw(frame: &mut Frame, snap: &BoxLifecycleSnapshot) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4), // header
            Constraint::Length(4), // pairing card
            Constraint::Length(4), // status
            Constraint::Min(3),    // serving (grows to fill remaining space)
            Constraint::Length(3), // footer keybindings
        ])
        .split(area);

    render_panel(frame, chunks[0], "tt-station", &header_lines(snap));
    render_panel(frame, chunks[1], "pairing", &pairing_lines(snap));
    render_panel(frame, chunks[2], "status", &status_lines(snap));
    render_panel(frame, chunks[3], "serving", &serving_lines(snap));
    render_panel(frame, chunks[4], "keys", &footer_lines());
}

/// Render one bordered `Paragraph` panel. `Borders::LEFT | Borders::BOTTOM`
/// only -- see the module doc's terminal-UI rule.
fn render_panel(frame: &mut Frame, area: Rect, title: &str, lines: &[String]) {
    let text: Vec<Line> = lines.iter().map(|l| Line::from(l.clone())).collect();
    let block = Block::default()
        .borders(Borders::LEFT | Borders::BOTTOM)
        .border_style(Style::default().fg(TEAL))
        .title(format!(" {title} "))
        .title_alignment(Alignment::Left)
        .title_style(Style::default().fg(TEAL).add_modifier(Modifier::BOLD));
    let paragraph = Paragraph::new(text)
        .block(block)
        .style(Style::default().fg(Color::White));
    frame.render_widget(paragraph, area);
}

/// A one-line transient message (action result / error) drawn over the
/// bottom of the footer area. Kept separate from [`draw`] (rather than
/// threaded through its signature) so `draw` stays exactly the two-argument
/// pure function the tests exercise; `run_tui`'s loop overlays this itself.
fn render_message(frame: &mut Frame, area: Rect, message: &str) {
    let paragraph = Paragraph::new(Line::from(message.to_string()))
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .alignment(Alignment::Left);
    frame.render_widget(paragraph, area);
}

/// The reset confirmation modal, overlaid on the same closure as `draw`
/// rather than folded into it -- same reasoning as [`render_message`].
fn render_confirm_reset(frame: &mut Frame) {
    let area = frame.area();
    // A small centered box, not full-width -- big enough for the prompt.
    let width = area.width.min(50);
    let height = 3;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);

    let block = Block::default()
        .borders(Borders::LEFT | Borders::BOTTOM)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" confirm reset ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let paragraph = Paragraph::new("Reset this box? This clears pairing + serving. [y/N]")
        .block(block)
        .style(Style::default().fg(Color::White));
    frame.render_widget(paragraph, popup);
}

// ---------------------------------------------------------------------
// Terminal lifecycle + the interactive event loop
// ---------------------------------------------------------------------

/// RAII guard for crossterm raw mode + the alternate screen. See the module
/// doc's "Terminal teardown" section for why this exists and why `run_tui`
/// must never restore the terminal by any path other than this `Drop` impl.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort: if these fail (e.g. stdout already closed), there is
        // nothing more useful to do than swallow the error -- this runs
        // during unwind, including panics, where propagating a second error
        // would just replace a more informative one (or abort the unwind).
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Launch the interactive operator TUI: enter raw mode + the alternate
/// screen (via [`TerminalGuard`], restored on every exit path), then hand
/// off to [`event_loop`]. `ctrl_port` is the agent's control port on
/// `127.0.0.1` -- used both by [`crate::console::env::collect_snapshot`]
/// (via `env`, already bound to it) and to build the `host:port` argument
/// for the shelled-out reset/pair commands below.
pub fn run_tui(env: &dyn LifecycleEnv, names: &ToolNames, ctrl_port: u16) -> anyhow::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    // `_guard` and `terminal` both drop at the end of this function's scope
    // (or on early return via `?`, or on unwind) in reverse declaration
    // order -- `terminal` first, then `_guard`'s raw-mode/alt-screen
    // teardown last, which is exactly the order we want.
    event_loop(&mut terminal, env, names, ctrl_port)
}

/// One key press worth of confirmation state for the destructive `R`eset
/// action -- kept separate from `snap` (the [`BoxLifecycleSnapshot`]) since
/// it's UI-only state with no bearing on the box's actual lifecycle.
enum Mode {
    Normal,
    ConfirmReset,
}

/// The actual poll/draw/act loop. Split out from [`run_tui`] so the
/// terminal-setup/teardown code in that function stays trivially correct
/// (three lines, no logic) -- everything that can go wrong lives here
/// instead, inside the guard's scope.
fn event_loop<B>(
    terminal: &mut Terminal<B>,
    env: &dyn LifecycleEnv,
    names: &ToolNames,
    ctrl_port: u16,
) -> anyhow::Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let actions = LifecycleActions::new(env, names);
    let host = format!("127.0.0.1:{ctrl_port}");

    let mut snap = collect_snapshot(env, names);
    let mut mode = Mode::Normal;
    let mut message: Option<String> = None;
    let tick_rate = Duration::from_secs(1);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| {
            draw(frame, &snap);
            if let Some(msg) = &message {
                // Overlay the message on the footer's bottom row.
                let area = frame.area();
                let msg_area = Rect::new(
                    area.x,
                    area.y + area.height.saturating_sub(1),
                    area.width,
                    1,
                );
                render_message(frame, msg_area, msg);
            }
            if matches!(mode, Mode::ConfirmReset) {
                render_confirm_reset(frame);
            }
        })?;

        let poll_timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::from_millis(0));

        if event::poll(poll_timeout)? {
            if let Event::Key(key) = event::read()? {
                // crossterm on some platforms reports both Press and Release
                // (and Repeat) events; only act on Press to avoid
                // double-firing an action per physical key press.
                if key.kind == KeyEventKind::Press {
                    match mode {
                        Mode::ConfirmReset => match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => {
                                message = Some(run_reset(env, &names.tt_bin, &host));
                                snap = collect_snapshot(env, names);
                                mode = Mode::Normal;
                            }
                            _ => {
                                message = Some("reset cancelled".to_string());
                                mode = Mode::Normal;
                            }
                        },
                        Mode::Normal => match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            KeyCode::Char('s') => {
                                message = Some(run_action(actions.start(), "start"));
                                snap = collect_snapshot(env, names);
                            }
                            KeyCode::Char('x') => {
                                message = Some(run_action(actions.stop(), "stop"));
                                snap = collect_snapshot(env, names);
                            }
                            KeyCode::Char('r') => {
                                message = Some(run_action(actions.restart(), "restart"));
                                snap = collect_snapshot(env, names);
                            }
                            KeyCode::Char('R') => {
                                mode = Mode::ConfirmReset;
                            }
                            KeyCode::Char('p') => {
                                message = Some(run_pair(env, &names.tt_bin, &host, &snap));
                                snap = collect_snapshot(env, names);
                            }
                            KeyCode::Char('c') => {
                                message = run_profile_cycle(&actions, &snap);
                                snap = collect_snapshot(env, names);
                            }
                            KeyCode::Char('i') => {
                                let agent_path = super::which_agent(&names.agent_bin);
                                message = Some(run_action(
                                    actions.install_service(&agent_path),
                                    "install-service",
                                ));
                                snap = collect_snapshot(env, names);
                            }
                            _ => {}
                        },
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            snap = collect_snapshot(env, names);
            last_tick = Instant::now();
        }
    }

    Ok(())
}

/// Format a start/stop/restart/install-service result as a one-line status
/// message for the footer overlay.
fn run_action(result: anyhow::Result<()>, verb: &str) -> String {
    match result {
        Ok(()) => format!("{verb}: ok"),
        Err(e) => format!("{verb} failed: {e}"),
    }
}

/// `R`eset, wired by shelling out to the `tt` binary itself
/// (`tt reset --host 127.0.0.1:<port> --yes`) rather than duplicating the
/// HTTP call `cmd_reset` (in `main.rs`) already makes. This keeps the one
/// auth touchpoint (bearer token lookup/clearing) centralized in the CLI
/// that already owns it -- see the module doc and the task report for the
/// full rationale. A missing/invalid token surfaces here as a shelled
/// command failure; the message hints at pairing since that's the fix.
fn run_reset(env: &dyn LifecycleEnv, tt_bin: &str, host: &str) -> String {
    match env.run(&[tt_bin, "reset", "--host", host, "--yes"]) {
        Ok(()) => "reset: ok".to_string(),
        Err(e) => format!("reset failed: {e} (no token for this box? press 'p' to pair)"),
    }
}

/// `p`air-localhost, using the code the collector already found in the
/// journal (`snap.pairing`). Also shells out to `tt pair` for the same
/// reason as [`run_reset`] -- one auth touchpoint, not a second HTTP client.
fn run_pair(
    env: &dyn LifecycleEnv,
    tt_bin: &str,
    host: &str,
    snap: &BoxLifecycleSnapshot,
) -> String {
    match &snap.pairing {
        Some(p) => match env.run(&[tt_bin, "pair", host, "--code", &p.code]) {
            Ok(()) => "pair: ok".to_string(),
            Err(e) => format!("pair failed: {e}"),
        },
        None => "no pairing code available -- start a pairing on the box first".to_string(),
    }
}

/// `c`ycle to the next profile in `config.available_profiles` (wrapping
/// around) and apply it via [`LifecycleActions::set_profile`]. Returns
/// `None` (no message, no-op) when there are no profiles to cycle through --
/// distinct from an action failure, which does produce a message.
fn run_profile_cycle(
    actions: &LifecycleActions<'_>,
    snap: &BoxLifecycleSnapshot,
) -> Option<String> {
    let cfg = snap.config.as_ref()?;
    if cfg.available_profiles.is_empty() {
        return None;
    }
    let next = next_profile(cfg.active_profile.as_deref(), &cfg.available_profiles);
    Some(run_action(
        actions.set_profile(&next),
        &format!("profile -> {next}"),
    ))
}

/// Pure helper: pick the profile after `active` in `available` (wrapping to
/// the front), or the first profile if `active` is `None` or not found in
/// `available`. Split out from [`run_profile_cycle`] so the cycling logic
/// itself is unit-testable without a `LifecycleActions`/`LifecycleEnv`.
fn next_profile(active: Option<&str>, available: &[String]) -> String {
    let idx = active
        .and_then(|a| available.iter().position(|p| p == a))
        .map(|i| (i + 1) % available.len())
        .unwrap_or(0);
    available[idx].clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libttstation::model::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn idle_snap() -> BoxLifecycleSnapshot {
        BoxLifecycleSnapshot {
            service: ServiceState::Active,
            reachable: true,
            name: Some("qb2-lab".into()),
            chips: Some("4xBH".into()),
            status: Some(ServingStatus::Idle),
            endpoint: None,
            serving: vec![],
            config: None,
            pairing: None,
        }
    }

    #[test]
    fn header_shows_name_and_service() {
        let lines = header_lines(&idle_snap());
        assert!(lines.iter().any(|l| l.contains("qb2-lab")));
        assert!(lines.iter().any(|l| l.to_lowercase().contains("active")));
    }

    #[test]
    fn header_shows_dash_when_no_profile_pinned() {
        let lines = header_lines(&idle_snap());
        assert!(lines.iter().any(|l| l.contains("profile: \u{2014}")));
    }

    #[test]
    fn header_shows_active_profile_when_present() {
        let mut s = idle_snap();
        s.config = Some(ConfigSummary {
            active_profile: Some("bleeding".into()),
            available_profiles: vec!["stable".into(), "bleeding".into()],
            backend: "runpy".into(),
            serving_host: "qb2-lab.local".into(),
            serving_port: 8003,
            serving_image: None,
            tt_inference_repo: None,
            tt_device: None,
        });
        let lines = header_lines(&s);
        assert!(lines.iter().any(|l| l.contains("profile: bleeding")));
    }

    #[test]
    fn pairing_lines_show_code_when_present() {
        let mut s = idle_snap();
        s.pairing = Some(PairingState {
            code: "042817".into(),
            expires_in_secs: 100,
        });
        let lines = pairing_lines(&s);
        assert!(lines.iter().any(|l| l.contains("042817")));
    }

    #[test]
    fn pairing_lines_show_hint_when_absent() {
        let lines = pairing_lines(&idle_snap());
        assert!(lines
            .iter()
            .any(|l| l.to_lowercase().contains("no active pairing")));
    }

    #[test]
    fn status_lines_show_idle_state_and_no_endpoint() {
        let lines = status_lines(&idle_snap());
        assert!(lines.iter().any(|l| l.contains("state: idle")));
        assert!(lines.iter().any(|l| l.contains("endpoint: (none)")));
    }

    #[test]
    fn status_lines_show_serving_model() {
        let mut s = idle_snap();
        s.status = Some(ServingStatus::Serving("llama3".into()));
        let lines = status_lines(&s);
        assert!(lines.iter().any(|l| l.contains("state: serving llama3")));
    }

    #[test]
    fn serving_lines_show_entries() {
        let mut s = idle_snap();
        s.serving = vec![ServingEntry {
            model: "llama3".into(),
            base_url: "http://127.0.0.1:8003/v1".into(),
            host_port: 8003,
            container: "vllm-1".into(),
            source: "agent".into(),
        }];
        let lines = serving_lines(&s);
        assert!(lines.iter().any(|l| l.contains("llama3")));
        assert!(lines.iter().any(|l| l.contains("vllm-1")));
    }

    #[test]
    fn serving_lines_none_when_empty() {
        let lines = serving_lines(&idle_snap());
        assert!(lines.iter().any(|l| l.contains("none")));
    }

    #[test]
    fn next_profile_wraps_around() {
        let available = vec!["stable".to_string(), "bleeding".to_string()];
        assert_eq!(next_profile(None, &available), "stable");
        assert_eq!(next_profile(Some("stable"), &available), "bleeding");
        assert_eq!(next_profile(Some("bleeding"), &available), "stable");
        // Active profile not in the list at all -- falls back to the first.
        assert_eq!(next_profile(Some("unknown"), &available), "stable");
    }

    #[test]
    fn renders_without_panicking() {
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &idle_snap())).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("qb2-lab"));
    }

    #[test]
    fn renders_pairing_snapshot_without_panicking() {
        let mut s = idle_snap();
        s.pairing = Some(PairingState {
            code: "042817".into(),
            expires_in_secs: 42,
        });
        let backend = TestBackend::new(60, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &s)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("042817"));
    }
}
