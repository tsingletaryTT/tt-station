//! The Stations page: one row per station (this machine first, then the registry's boxes),
//! Refresh, Stop on the selected station, and Add station.
//!
//! Row text and dot come from [`view_model::station_row`]; the offline box's MAC is merged in
//! by [`super::Shared::set_state`]. Selecting a row makes it the station the Models and Logs
//! pages act on.
//!
//! Unverified (headless build): row layout, selection behaviour, and that the Stop result line
//! is visible where expected. The text in it comes from [`ActionResult`], which is tested.

use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;

use super::view_model::{station_row, ActionResult};
use super::{set_margins, show_result, spawn_call, App};

pub fn build(app: &App) -> gtk::Widget {
    let heading = gtk::Label::new(Some("Stations"));
    heading.add_css_class("title-2");
    heading.set_xalign(0.0);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .build();

    let status = gtk::Label::new(None);
    status.set_xalign(0.0);
    status.set_wrap(true);
    status.set_selectable(true);

    let refresh = gtk::Button::with_label("Refresh");
    let stop = gtk::Button::with_label("Stop serving");
    let add = gtk::Button::with_label("Add station...");

    {
        let app = app.clone();
        list.connect_row_selected(move |_, row| {
            if let Some(row) = row {
                let i = row.index().max(0) as usize;
                if i != app.selected.get() {
                    app.selected.set(i);
                    app.notify();
                }
            }
        });
    }
    {
        let app = app.clone();
        refresh.connect_clicked(move |_| refresh_all(&app));
    }
    {
        let app = app.clone();
        let status = status.clone();
        stop.connect_clicked(move |_| {
            let (station, _) = app.current();
            status.set_text("Stopping...");
            let status = status.clone();
            let app2 = app.clone();
            spawn_call(&app, async move { station.stop().await }, move |r| {
                // The Err text (TT-Studio row, several servers, not paired) is shown as is.
                show_result(&status, &ActionResult::from_result("Stop", &r));
                refresh_all(&app2);
            });
        });
    }
    {
        let app = app.clone();
        add.connect_clicked(move |b| {
            let parent = b.root().and_then(|r| r.downcast::<gtk::Window>().ok());
            super::pair_dialog::open(&app, parent.as_ref());
        });
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.append(&refresh);
    buttons.append(&stop);
    buttons.append(&add);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 8);
    set_margins(&page, 12);
    page.append(&heading);
    page.append(&scroll);
    page.append(&buttons);
    page.append(&status);

    redraw(app, &list);
    {
        let weak = Rc::downgrade(app);
        app.on_change(move || {
            if let Some(app) = weak.upgrade() {
                redraw(&app, &list);
            }
        });
    }
    page.upcast()
}

/// Ask every station for its state; each answer redraws the list when it arrives.
pub fn refresh_all(app: &App) {
    let stations = app.stations.borrow().clone();
    for (i, st) in stations.into_iter().enumerate() {
        let app2 = app.clone();
        spawn_call(app, async move { st.state().await }, move |state| {
            app2.set_state(i, state);
        });
    }
}

/// Rebuild the rows from the last known states. Called on every change notification; the list
/// is short (one row per station), so rebuilding is cheaper than diffing.
fn redraw(app: &App, list: &gtk::ListBox) {
    {
        // Removing rows fires `row-selected(None)`, which the handler ignores.
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        let states = app.states.borrow().clone();
        for (i, state) in states.iter().enumerate() {
            let (title, subtitle) = match state {
                Some(s) => {
                    let v = station_row(s);
                    (format!("{} {}", v.dot.glyph(), v.title), v.subtitle)
                }
                None if i == 0 => ("This machine".to_string(), "checking...".to_string()),
                None => {
                    let reg = app.registry.borrow();
                    let name = reg
                        .stations()
                        .get(i - 1)
                        .map(|k| k.name.clone())
                        .unwrap_or_default();
                    (name, "checking...".to_string())
                }
            };
            let t = gtk::Label::new(Some(&title));
            t.set_xalign(0.0);
            t.add_css_class("heading");
            let s = gtk::Label::new(Some(&subtitle));
            s.set_xalign(0.0);
            s.set_wrap(true);
            s.add_css_class("dim-label");
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            set_margins(&b, 6);
            b.append(&t);
            b.append(&s);
            list.append(&b);
        }
        if let Some(row) = list.row_at_index(app.selected.get() as i32) {
            list.select_row(Some(&row));
        }
    }
}
