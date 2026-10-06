//! The tray menu as plain data, with no tray library and no I/O.
//!
//! [`build_menu`] turns the current [`StationState`]s into a list of [`MenuItem`]s. Every rule
//! about what the menu offers lives here so that each one is a unit test. `tray.rs` only
//! converts the result into a StatusNotifierItem menu and sends the chosen [`MenuAction`] back.
//!
//! Rules, per station:
//! * offline: grey header, no run actions; a Wake item only when a MAC is known;
//! * reachable but not paired (a remote): a pair prompt that opens the window;
//! * paired and contended (another tenant holds the chips): the holder text, and no Stop,
//!   Restart or run action, because the tray must not stop or re-run against someone else's work;
//! * paired and serving: Stop, Restart and a Connect submenu;
//! * paired and idle: "Run last model" only when `last_run` knows a model for the station.
//!
//! The menu always ends with a separator, Open window and Quit.

use crate::station::{StationId, StationState};

/// What the person chose. The tray sends one of these over a channel and the app acts on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    Stop(StationId),
    Restart(StationId),
    RunLast(StationId),
    OpenWebUi(StationId),
    OpenOpenCode(StationId),
    /// Copy the station's `/v1` URL to the clipboard.
    CopyEndpoint(StationId),
    /// Send a Wake-on-LAN packet to the box. Added to the brief's list because an offline box
    /// needs some action to wake it.
    Wake(StationId),
    OpenWindow,
    Quit,
}

/// Status color of a station header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot {
    /// Serving a model.
    Green,
    /// Reachable and idle, or reachable but not paired yet.
    Amber,
    /// Offline.
    Grey,
    /// Another tenant holds the chips.
    Red,
}

impl Dot {
    /// Brand semantic tint (green `#6FABA0`, yellow `#F6BC42`, red `#FF9E8A`; grey for
    /// offline). Kept for icon rendering; menu labels cannot be colored.
    pub fn hex(self) -> &'static str {
        match self {
            Dot::Green => "#6FABA0",
            Dot::Amber => "#F6BC42",
            Dot::Grey => "#8A9A98",
            Dot::Red => "#FF9E8A",
        }
    }

    /// A glyph for use in a menu label. The shapes differ so the state is not carried by
    /// color alone, because a plain-text menu label cannot be colored.
    pub fn glyph(self) -> &'static str {
        match self {
            Dot::Green => "\u{25CF}", // filled circle
            Dot::Amber => "\u{25D0}", // half circle
            Dot::Grey => "\u{25CB}",  // empty circle
            Dot::Red => "\u{25B2}",   // triangle
        }
    }
}

/// One entry in the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuItem {
    /// A station heading or a line of information. Not clickable.
    Header {
        text: String,
        dot: Dot,
    },
    /// A clickable entry. A disabled one is shown greyed out.
    Action {
        label: String,
        action: MenuAction,
        enabled: bool,
    },
    Submenu {
        label: String,
        items: Vec<MenuItem>,
    },
    Separator,
}

fn action(label: impl Into<String>, action: MenuAction) -> MenuItem {
    MenuItem::Action {
        label: label.into(),
        action,
        enabled: true,
    }
}

/// Build the whole menu. `last_run` returns the model most recently run on a station, if any.
pub fn build_menu(
    stations: &[StationState],
    last_run: &dyn Fn(&StationId) -> Option<String>,
) -> Vec<MenuItem> {
    let mut out = Vec::new();
    for st in stations {
        station_items(st, last_run, &mut out);
        out.push(MenuItem::Separator);
    }
    out.push(action("Open window", MenuAction::OpenWindow));
    out.push(action("Quit", MenuAction::Quit));
    out
}

fn station_items(
    st: &StationState,
    last_run: &dyn Fn(&StationId) -> Option<String>,
    out: &mut Vec<MenuItem>,
) {
    let mesh = st
        .mesh
        .as_deref()
        .map(|m| format!(" ({m})"))
        .unwrap_or_default();
    let header = |dot: Dot, suffix: &str| MenuItem::Header {
        text: format!("{}{}{}", st.name, mesh, suffix),
        dot,
    };

    if !st.reachable {
        out.push(header(Dot::Grey, " - offline"));
        if st.mac.is_some() {
            out.push(action(
                format!("Wake {}", st.name),
                MenuAction::Wake(st.id.clone()),
            ));
        }
        return;
    }
    if !st.paired {
        out.push(header(Dot::Amber, " - not paired"));
        out.push(action(
            format!("Pair with {}...", st.name),
            MenuAction::OpenWindow,
        ));
        return;
    }
    if let Some(holder) = &st.contention {
        // Contention wins over serving: do not offer to stop work that may not be ours.
        out.push(header(Dot::Red, " - busy"));
        out.push(MenuItem::Header {
            text: holder.clone(),
            dot: Dot::Red,
        });
        return;
    }
    if let Some(serving) = &st.serving {
        out.push(header(Dot::Green, &format!(" - serving {}", serving.model)));
        out.push(action("Stop", MenuAction::Stop(st.id.clone())));
        out.push(action("Restart", MenuAction::Restart(st.id.clone())));
        out.push(MenuItem::Submenu {
            label: "Connect".into(),
            items: vec![
                action("Open WebUI", MenuAction::OpenWebUi(st.id.clone())),
                action("opencode", MenuAction::OpenOpenCode(st.id.clone())),
                action("Copy /v1 URL", MenuAction::CopyEndpoint(st.id.clone())),
            ],
        });
        return;
    }
    out.push(header(Dot::Amber, " - idle"));
    if let Some(model) = last_run(&st.id) {
        out.push(action(
            format!("Run last model: {model}"),
            MenuAction::RunLast(st.id.clone()),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::ServingNow;

    fn st(id: &str) -> StationState {
        StationState {
            id: StationId(id.into()),
            name: id.into(),
            reachable: true,
            mesh: Some("p300x2".into()),
            serving: None,
            paired: true,
            contention: None,
            mac: None,
        }
    }
    fn serving(mut s: StationState) -> StationState {
        s.serving = Some(ServingNow {
            model: "llama".into(),
            base_url: "http://x/v1".into(),
        });
        s
    }
    fn none(_: &StationId) -> Option<String> {
        None
    }
    fn some(_: &StationId) -> Option<String> {
        Some("qwen".into())
    }
    /// Every action in the menu, depth first, with submenus flattened.
    fn actions(items: &[MenuItem]) -> Vec<MenuAction> {
        let mut v = Vec::new();
        for i in items {
            match i {
                MenuItem::Action { action, .. } => v.push(action.clone()),
                MenuItem::Submenu { items, .. } => v.extend(actions(items)),
                _ => {}
            }
        }
        v
    }
    fn is_stop_or_restart(a: &MenuAction) -> bool {
        matches!(a, MenuAction::Stop(_) | MenuAction::Restart(_))
    }
    fn is_run(a: &MenuAction) -> bool {
        matches!(
            a,
            MenuAction::Stop(_) | MenuAction::Restart(_) | MenuAction::RunLast(_)
        )
    }
    fn dots(items: &[MenuItem]) -> Vec<Dot> {
        items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Header { dot, .. } => Some(*dot),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn serving_station_offers_stop_restart_and_connect_submenu() {
        let m = build_menu(&[serving(st("local"))], &none);
        let id = StationId("local".into());
        let a = actions(&m);
        assert!(a.contains(&MenuAction::Stop(id.clone())));
        assert!(a.contains(&MenuAction::Restart(id.clone())));
        let sub = m
            .iter()
            .find_map(|i| match i {
                MenuItem::Submenu { label, items } if label == "Connect" => Some(items),
                _ => None,
            })
            .expect("Connect submenu");
        assert_eq!(
            actions(sub),
            vec![
                MenuAction::OpenWebUi(id.clone()),
                MenuAction::OpenOpenCode(id.clone()),
                MenuAction::CopyEndpoint(id)
            ]
        );
        assert_eq!(dots(&m), vec![Dot::Green]);
    }

    #[test]
    fn idle_station_offers_run_last_only_when_a_last_run_exists() {
        let id = StationId("local".into());
        let with = build_menu(&[st("local")], &some);
        assert!(actions(&with).contains(&MenuAction::RunLast(id)));
        let label = with
            .iter()
            .find_map(|i| match i {
                MenuItem::Action {
                    label,
                    action: MenuAction::RunLast(_),
                    ..
                } => Some(label.clone()),
                _ => None,
            })
            .unwrap();
        assert!(label.contains("qwen"));
        let without = build_menu(&[st("local")], &none);
        assert!(!actions(&without).iter().any(is_run));
        // Idle never offers Stop or Restart.
        assert!(!actions(&with).iter().any(is_stop_or_restart));
    }

    #[test]
    fn offline_station_is_grey_and_offers_no_actions_except_wake_when_mac_known() {
        let mut off = st("box:8765");
        off.reachable = false;
        let m = build_menu(&[off.clone()], &some);
        assert_eq!(dots(&m), vec![Dot::Grey]);
        let a = actions(&m);
        assert!(!a.iter().any(is_run), "no run actions offline: {a:?}");
        assert!(
            !a.iter().any(|a| matches!(a, MenuAction::Wake(_))),
            "no MAC, no Wake"
        );

        off.mac = Some("aa:bb:cc:dd:ee:ff".into());
        let a = actions(&build_menu(&[off], &some));
        assert!(a.contains(&MenuAction::Wake(StationId("box:8765".into()))));
        assert!(!a.iter().any(is_run));
    }

    #[test]
    fn contended_station_shows_the_holder_text_and_no_stop() {
        for serving_too in [false, true] {
            let mut c = st("box:8765");
            c.contention = Some("chips held by alice".into());
            if serving_too {
                c = serving(c);
            }
            let m = build_menu(&[c], &some);
            assert!(
                m.iter().any(|i| matches!(i, MenuItem::Header { text, .. } if text.contains("chips held by alice"))),
                "holder text shown"
            );
            let a = actions(&m);
            assert!(
                !a.iter().any(is_run),
                "no Stop/Restart/Run on a contended station: {a:?}"
            );
            assert_eq!(dots(&m)[0], Dot::Red);
        }
    }

    #[test]
    fn unpaired_remote_shows_pair_prompt_that_opens_the_window_and_no_run_actions() {
        let mut u = st("box:8765");
        u.paired = false;
        let m = build_menu(&[u], &some);
        let prompt = m.iter().find_map(|i| match i {
            MenuItem::Action { label, action, .. } if label.contains("Pair") => {
                Some(action.clone())
            }
            _ => None,
        });
        assert_eq!(prompt, Some(MenuAction::OpenWindow));
        assert!(!actions(&m).iter().any(is_run));
    }

    #[test]
    fn menu_always_ends_with_open_window_and_quit() {
        let mut off = st("b:1");
        off.reachable = false;
        let mut unp = st("b:2");
        unp.paired = false;
        for set in [vec![st("local")], vec![serving(st("local")), off, unp]] {
            let m = build_menu(&set, &none);
            let n = m.len();
            assert!(matches!(
                &m[n - 1],
                MenuItem::Action {
                    action: MenuAction::Quit,
                    ..
                }
            ));
            assert!(matches!(
                &m[n - 2],
                MenuItem::Action {
                    action: MenuAction::OpenWindow,
                    ..
                }
            ));
            assert_eq!(m[n - 3], MenuItem::Separator);
        }
    }

    #[test]
    fn no_stations_still_yields_open_window_and_quit() {
        let m = build_menu(&[], &none);
        assert_eq!(actions(&m), vec![MenuAction::OpenWindow, MenuAction::Quit]);
    }

    #[test]
    fn dot_colors_come_from_the_brand_palette() {
        assert_eq!(Dot::Green.hex(), "#6FABA0");
        assert_eq!(Dot::Amber.hex(), "#F6BC42");
        assert_eq!(Dot::Red.hex(), "#FF9E8A");
    }
}
