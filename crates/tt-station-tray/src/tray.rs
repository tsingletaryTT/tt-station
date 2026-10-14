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
//! Icon: the pixmap is the source of truth. The two logo PNGs in `assets/` are embedded with
//! `include_bytes!` and sent as `IconPixmap` (48 and 128 px, ARGB32), so the icon needs no
//! installed file and shows up when the binary runs straight from `target/`. `icon_name` is
//! empty on purpose: Plasma prefers a non-empty name over the pixmap and draws a blank slot when
//! the icon theme lacks that name (the first run on KDE did exactly that with the themed name
//! `com.tenstorrent.ttstation.tray`). The art is a copy of the panel's icons. Status dots are
//! unicode glyphs in menu labels, because a plain label cannot be colored.
//!
//! Clicks: `ksni` 0.3.6 fixes `ItemIsMenu` to false, so a left click calls `Activate`, which
//! opens the window (the same [`MenuAction::OpenWindow`] as the "Open window" menu item), and
//! a right click shows the menu. `secondary_activate` is not implemented.

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

/// The embedded logo, 48 px and 128 px (copies of the panel's icons).
const TRAY_48: &[u8] = include_bytes!("../assets/tray-48.png");
const TRAY_128: &[u8] = include_bytes!("../assets/tray-128.png");

/// Reorder RGBA8 pixels to ARGB32 in network byte order, i.e. bytes `[A, R, G, B]` per pixel,
/// which is what the StatusNotifierItem `IconPixmap` property carries.
pub fn rgba_to_argb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for p in rgba.chunks_exact(4) {
        out.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
    }
    out
}

/// Decode an RGBA8 PNG into `(pixels, width, height)`.
fn decode_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), png::DecodingError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // Expand palette/low-bit/tRNS images so the output is 8-bit; the art is RGBA already.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let info = reader.next_frame(&mut buf)?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(png::DecodingError::LimitsExceeded);
    }
    buf.truncate(info.buffer_size());
    Ok((buf, info.width, info.height))
}

impl ksni::Tray for TtTray {
    fn id(&self) -> String {
        "tt-station-tray".into()
    }
    fn title(&self) -> String {
        "tt-station".into()
    }
    /// Empty so the host draws [`Self::icon_pixmap`] (see the module doc).
    fn icon_name(&self) -> String {
        String::new()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let mut icons = Vec::new();
        for bytes in [TRAY_48, TRAY_128] {
            match decode_rgba(bytes) {
                Ok((rgba, w, h)) => icons.push(ksni::Icon {
                    width: w as i32,
                    height: h as i32,
                    data: rgba_to_argb(&rgba),
                }),
                Err(e) => {
                    eprintln!("tt-station-tray: embedded icon failed to decode: {e}");
                    return Vec::new();
                }
            }
        }
        icons
    }
    /// Left click (ksni 0.3.6 reports `ItemIsMenu = false`): open the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        // A closed receiver means the app is shutting down; nothing to do.
        let _ = self.tx.send(MenuAction::OpenWindow);
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

    /// A tray with an empty menu and a channel the test can read.
    fn bare_tray() -> (TtTray, tokio::sync::mpsc::UnboundedReceiver<MenuAction>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            TtTray {
                items: vec![],
                tx,
                needs_attention: false,
            },
            rx,
        )
    }

    #[test]
    fn rgba_to_argb_reorders_channels_per_pixel() {
        // Two pixels: (R,G,B,A) = (1,2,3,4) and (10,20,30,255).
        let out = rgba_to_argb(&[1, 2, 3, 4, 10, 20, 30, 255]);
        assert_eq!(out, vec![4, 1, 2, 3, 255, 10, 20, 30]);
    }

    #[test]
    fn icon_pixmap_has_the_two_embedded_sizes_with_argb_data() {
        let (tray, _rx) = bare_tray();
        let icons = ksni::Tray::icon_pixmap(&tray);
        assert_eq!(icons.len(), 2);
        let sizes: Vec<(i32, i32)> = icons.iter().map(|i| (i.width, i.height)).collect();
        assert_eq!(sizes, vec![(48, 48), (128, 128)]);
        for i in &icons {
            assert_eq!(i.data.len(), (i.width * i.height * 4) as usize);
            // Decode the same PNG independently and compare its first visible pixel: the
            // pixmap must carry it as [A, R, G, B].
            let (rgba, _, _) = decode_rgba(if i.width == 48 { TRAY_48 } else { TRAY_128 }).unwrap();
            let k = rgba.chunks_exact(4).position(|p| p[3] > 0).expect("visible pixel");
            let (r, g, b, a) = (rgba[k * 4], rgba[k * 4 + 1], rgba[k * 4 + 2], rgba[k * 4 + 3]);
            assert_eq!(&i.data[k * 4..k * 4 + 4], &[a, r, g, b]);
            // Guard against a symmetric pixel hiding a wrong order.
            assert!(!(a == r && r == g && g == b), "pick art with distinct channels");
        }
    }

    #[test]
    fn icon_name_is_empty_so_the_host_uses_the_pixmap() {
        let (tray, _rx) = bare_tray();
        assert_eq!(ksni::Tray::icon_name(&tray), "");
    }

    #[test]
    fn left_click_activate_sends_open_window() {
        let (mut tray, mut rx) = bare_tray();
        ksni::Tray::activate(&mut tray, 0, 0);
        assert_eq!(rx.try_recv().unwrap(), MenuAction::OpenWindow);
    }
}
