//! The StatusNotifierItem tray, built on `ksni` 0.3.
//!
//! **Not exercised headless.** Showing a tray icon needs a session D-Bus and a desktop that
//! hosts StatusNotifierItems, and this code was written and tested on a machine with neither.
//! It compiles, and the one-to-one conversion from [`MenuItem`] to a plain description
//! ([`describe`]) is unit tested. It has never been shown on a real panel, and the click path
//! (`activate` closure -> channel) has never been run against a real menu.
//!
//! Shape: [`describe`] turns the pure menu model into [`ItemDesc`] values, keeping labels,
//! enabled flags and order. [`to_ksni`] then maps those onto `ksni::menu` items. Clicking an
//! action sends its [`MenuAction`] over a `tokio::sync::mpsc` channel; the app owns the
//! receiver and does the work, so this file does no I/O of its own.
//!
//! Icon: the freedesktop name `com.tenstorrent.ttstation.panel`, which the `tt-station-panel`
//! package installs under hicolor (art in `box-panel/assets/icons`). If the theme lacks it the
//! host shows its own fallback; no artwork is made here. Status dots are unicode glyphs in menu
//! labels, because a plain label cannot be colored.

use tokio::sync::mpsc::UnboundedSender;

use crate::menu_model::{Dot, MenuAction, MenuItem};

/// A menu entry in a form that does not depend on `ksni`, so the mapping can be tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemDesc {
    /// `action` is `None` for a heading, which is always disabled.
    Standard {
        label: String,
        enabled: bool,
        action: Option<MenuAction>,
    },
    Sub {
        label: String,
        items: Vec<ItemDesc>,
    },
    Separator,
}

/// Convert the menu model into [`ItemDesc`]s, one to one and in order. A header becomes a
/// disabled entry whose label starts with its status glyph.
pub fn describe(items: &[MenuItem]) -> Vec<ItemDesc> {
    items
        .iter()
        .map(|i| match i {
            MenuItem::Header { text, dot } => ItemDesc::Standard {
                label: format!("{} {}", Dot::glyph(*dot), text),
                enabled: false,
                action: None,
            },
            MenuItem::Action {
                label,
                action,
                enabled,
            } => ItemDesc::Standard {
                label: label.clone(),
                enabled: *enabled,
                action: Some(action.clone()),
            },
            MenuItem::Submenu { label, items } => ItemDesc::Sub {
                label: label.clone(),
                items: describe(items),
            },
            MenuItem::Separator => ItemDesc::Separator,
        })
        .collect()
}

/// The tray object `ksni` runs. Replace `items` through `Handle::update` to refresh the menu.
#[derive(Debug)]
pub struct TtTray {
    pub items: Vec<MenuItem>,
    /// Where chosen actions go.
    pub tx: UnboundedSender<MenuAction>,
    /// Ask the desktop to draw attention (a station is contended).
    pub needs_attention: bool,
}

fn to_ksni(items: &[ItemDesc], tx: &UnboundedSender<MenuAction>) -> Vec<ksni::MenuItem<TtTray>> {
    use ksni::menu::{StandardItem, SubMenu};
    items
        .iter()
        .map(|d| match d {
            ItemDesc::Standard {
                label,
                enabled,
                action,
            } => {
                let action = action.clone();
                let tx = tx.clone();
                StandardItem {
                    label: label.clone(),
                    enabled: *enabled,
                    activate: Box::new(move |_: &mut TtTray| {
                        if let Some(a) = &action {
                            // A closed receiver means the app is shutting down; nothing to do.
                            let _ = tx.send(a.clone());
                        }
                    }),
                    ..Default::default()
                }
                .into()
            }
            ItemDesc::Sub { label, items } => SubMenu {
                label: label.clone(),
                submenu: to_ksni(items, tx),
                ..Default::default()
            }
            .into(),
            ItemDesc::Separator => ksni::MenuItem::Separator,
        })
        .collect()
}

impl ksni::Tray for TtTray {
    fn id(&self) -> String {
        "tt-station-tray".into()
    }
    fn title(&self) -> String {
        "tt-station".into()
    }
    fn icon_name(&self) -> String {
        "com.tenstorrent.ttstation.panel".into()
    }
    fn status(&self) -> ksni::Status {
        if self.needs_attention {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        to_ksni(&describe(&self.items), &self.tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::StationId;

    #[test]
    fn describe_keeps_labels_enabled_flags_and_order() {
        let id = StationId("local".into());
        let items = vec![
            MenuItem::Header {
                text: "This machine".into(),
                dot: Dot::Green,
            },
            MenuItem::Action {
                label: "Stop".into(),
                action: MenuAction::Stop(id.clone()),
                enabled: true,
            },
            MenuItem::Action {
                label: "Greyed".into(),
                action: MenuAction::Quit,
                enabled: false,
            },
            MenuItem::Submenu {
                label: "Connect".into(),
                items: vec![MenuItem::Action {
                    label: "Open WebUI".into(),
                    action: MenuAction::OpenWebUi(id.clone()),
                    enabled: true,
                }],
            },
            MenuItem::Separator,
            MenuItem::Action {
                label: "Quit".into(),
                action: MenuAction::Quit,
                enabled: true,
            },
        ];
        let d = describe(&items);
        assert_eq!(d.len(), 6);
        assert_eq!(
            d[0],
            ItemDesc::Standard {
                label: format!("{} This machine", Dot::Green.glyph()),
                enabled: false,
                action: None
            }
        );
        assert_eq!(
            d[1],
            ItemDesc::Standard {
                label: "Stop".into(),
                enabled: true,
                action: Some(MenuAction::Stop(id.clone()))
            }
        );
        assert!(
            matches!(&d[2], ItemDesc::Standard { label, enabled: false, .. } if label == "Greyed")
        );
        assert_eq!(
            d[3],
            ItemDesc::Sub {
                label: "Connect".into(),
                items: vec![ItemDesc::Standard {
                    label: "Open WebUI".into(),
                    enabled: true,
                    action: Some(MenuAction::OpenWebUi(id))
                }]
            }
        );
        assert_eq!(d[4], ItemDesc::Separator);
        assert!(matches!(&d[5], ItemDesc::Standard { label, .. } if label == "Quit"));
    }

    #[test]
    fn ksni_menu_has_the_same_length_and_activate_sends_the_action() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tray = TtTray {
            items: vec![
                MenuItem::Separator,
                MenuItem::Action {
                    label: "Quit".into(),
                    action: MenuAction::Quit,
                    enabled: true,
                },
            ],
            tx,
            needs_attention: false,
        };
        let mut menu = ksni::Tray::menu(&tray);
        assert_eq!(menu.len(), 2);
        let ksni::MenuItem::Standard(item) = menu.remove(1) else {
            panic!("standard item")
        };
        assert_eq!(item.label, "Quit");
        let mut t2 = tray;
        (item.activate)(&mut t2);
        assert_eq!(rx.try_recv().unwrap(), MenuAction::Quit);
    }

    #[test]
    fn ksni_conversion_keeps_submenu_and_enabled_flag() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tray = TtTray {
            items: vec![MenuItem::Submenu {
                label: "Connect".into(),
                items: vec![
                    MenuItem::Action {
                        label: "On".into(),
                        action: MenuAction::Quit,
                        enabled: true,
                    },
                    MenuItem::Action {
                        label: "Off".into(),
                        action: MenuAction::Quit,
                        enabled: false,
                    },
                ],
            }],
            tx,
            needs_attention: false,
        };
        let mut menu = ksni::Tray::menu(&tray);
        let ksni::MenuItem::SubMenu(sub) = menu.remove(0) else {
            panic!("submenu")
        };
        assert_eq!(sub.label, "Connect");
        let flags: Vec<(String, bool)> = sub
            .submenu
            .iter()
            .map(|i| match i {
                ksni::MenuItem::Standard(s) => (s.label.clone(), s.enabled),
                _ => panic!("standard"),
            })
            .collect();
        assert_eq!(
            flags,
            vec![("On".to_string(), true), ("Off".to_string(), false)]
        );
    }
}
