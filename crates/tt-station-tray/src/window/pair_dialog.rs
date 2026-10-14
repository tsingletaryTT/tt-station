//! Add station: type an address (or pick a box found on the network), then type the 6-digit
//! code the box shows on its own screen.
//!
//! The handshake is [`crate::pairing_flow::pair_station`], the same function and token store the
//! CLI uses. `pair_station` asks for the code through a plain blocking closure, after the box
//! has answered `pair-init`. Here that closure tells the dialog "show the code field now" over a
//! oneshot channel and then waits for the code on a std channel.
//!
//! **Why a separate thread.** The token store (`Box<dyn SecretStore>`) is not `Send`, so the
//! pairing future cannot go to the tokio runtime's worker pool, and the closure blocks while the
//! person types. So the pairing runs on its own short-lived thread with
//! `tokio::runtime::Handle::block_on`, which drives reqwest on the shared runtime and is free to
//! block. The GTK main thread only awaits channels.
//!
//! Closing the dialog while the code is being asked drops the code sender; the closure then
//! returns `None`, `pair_station` stops before `pair-complete` (so the abandoned attempt does
//! not count toward the box's wrong-code limit), and nothing is stored.
//!
//! The address is checked with `registry::validate_address`, the check `add_manual` uses,
//! before the box is contacted ([`super::view_model::pairing_target`]). A box already listed
//! under another letter case of its host is paired under its listed id, so the token lands
//! under the key the station list reads.
//!
//! The box is added to the registry only after pairing succeeds.
//!
//! Never looked at or clicked: the dialog's layout, the two-step reveal of the code field, the
//! discovered-box list, and closing mid-pairing.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;

use crate::discover::discover_all;
use crate::pairing_flow::{open_store, pair_station};
use crate::registry::{AddError, KnownStation};

use super::view_model::{pairing_target, ActionResult};
use super::{set_margins, show_result, spawn_call, App};

pub fn open(app: &App, parent: Option<&gtk::Window>) {
    let window = gtk::Window::builder()
        .title("Add station")
        .modal(true)
        .default_width(460)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    let intro = gtk::Label::new(Some(
        "Enter the box's address (host or host:port; the agent's default port is 8765).",
    ));
    intro.set_wrap(true);
    intro.set_xalign(0.0);
    let address = gtk::Entry::new();
    address.set_placeholder_text(Some("qb2-lab.local"));
    let find = gtk::Button::with_label("Find boxes on the network");
    let found = gtk::ListBox::new();
    let cont = gtk::Button::with_label("Continue");
    cont.add_css_class("suggested-action");

    let code_label = gtk::Label::new(Some(
        "Enter the 6-digit code shown on the box's screen (its panel or `tt-station console`).",
    ));
    code_label.set_wrap(true);
    code_label.set_xalign(0.0);
    let code = gtk::Entry::new();
    code.set_max_length(6);
    let pair = gtk::Button::with_label("Pair");
    pair.add_css_class("suggested-action");
    for w in [
        code_label.upcast_ref::<gtk::Widget>(),
        code.upcast_ref(),
        pair.upcast_ref(),
    ] {
        w.set_visible(false);
    }

    let status = gtk::Label::new(None);
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_selectable(true);

    // Find boxes: mDNS plus the registry's manual stations; clicking a result fills the entry.
    {
        let app = app.clone();
        let found = found.clone();
        let status = status.clone();
        let address = address.clone();
        find.connect_clicked(move |_| {
            status.set_text("Looking for boxes...");
            let manual: Vec<KnownStation> = app.registry.borrow().stations().to_vec();
            let found = found.clone();
            let status = status.clone();
            let address = address.clone();
            spawn_call(
                &app,
                async move { discover_all(&manual, Duration::from_secs(3)).await },
                move |boxes| {
                    while let Some(c) = found.first_child() {
                        found.remove(&c);
                    }
                    status.set_text(&format!("Found {} box(es).", boxes.len()));
                    for b in boxes {
                        let target = format!("{}:{}", b.host, b.ctrl_port);
                        let btn = gtk::Button::with_label(&format!("{} ({target})", b.name));
                        let address = address.clone();
                        btn.connect_clicked(move |_| address.set_text(&target));
                        found.append(&btn);
                    }
                },
            );
        });
    }

    // The code sender lives here while the box waits for a code.
    let code_tx: Rc<RefCell<Option<mpsc::Sender<String>>>> = Rc::new(RefCell::new(None));

    {
        let app = app.clone();
        let window = window.clone();
        let status = status.clone();
        let address = address.clone();
        let code_tx = code_tx.clone();
        let (code_label, code, pair) = (code_label.clone(), code.clone(), pair.clone());
        cont.connect_clicked(move |cont| {
            // The registry's own strict check runs before any pairing, and a box already listed
            // under another letter case is paired under its listed id (see `pairing_target`).
            let k = match pairing_target(&address.text(), app.registry.borrow().stations()) {
                Ok(k) => k,
                Err(e) => {
                    show_result(&status, &ActionResult::Failed(e));
                    return;
                }
            };
            let (host, port) = (k.host.clone(), k.port);
            let base = k.base();
            cont.set_sensitive(false);
            status.remove_css_class("error");
            status.set_text(&format!("Contacting {base}..."));

            let (ask_tx, ask_rx) = tokio::sync::oneshot::channel::<()>();
            let (tx, rx) = mpsc::channel::<String>();
            *code_tx.borrow_mut() = Some(tx);
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
            let rt = app.rt.clone();
            std::thread::spawn(move || {
                let r = rt.block_on(async move {
                    let store = open_store();
                    let ask = move || {
                        let _ = ask_tx.send(());
                        // `None` (sender dropped: the dialog closed) aborts before
                        // pair-complete, so no attempt is spent.
                        rx.recv().ok()
                    };
                    pair_station(&base, ask, store.as_ref()).await
                });
                let _ = done_tx.send(r);
            });

            // Step 2: the box answered pair-init; show the code field.
            {
                let (code_label, code, pair, status) = (
                    code_label.clone(),
                    code.clone(),
                    pair.clone(),
                    status.clone(),
                );
                glib::spawn_future_local(async move {
                    if ask_rx.await.is_ok() {
                        status.set_text("The box is showing a pairing code.");
                        code_label.set_visible(true);
                        code.set_visible(true);
                        pair.set_visible(true);
                        pair.set_sensitive(true);
                        code.grab_focus();
                    }
                });
            }
            // Step 3: pairing finished either way.
            {
                let app = app.clone();
                let window = window.clone();
                let status = status.clone();
                let cont = cont.clone();
                let code_tx = code_tx.clone();
                glib::spawn_future_local(async move {
                    let r = done_rx
                        .await
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("pairing stopped unexpectedly")));
                    code_tx.borrow_mut().take();
                    if r.is_ok() {
                        let added = app.registry.borrow_mut().add_manual(&host, port);
                        match added {
                            Ok(_) | Err(AddError::Duplicate) => {
                                if let Err(e) = app.registry.borrow().save() {
                                    eprintln!(
                                        "tt-station-tray: could not save the registry: {e:#}"
                                    );
                                }
                                app.rebuild_stations();
                                super::stations_pane::refresh_all(&app);
                                window.close();
                                return;
                            }
                            Err(e) => {
                                show_result(
                                    &status,
                                    &ActionResult::Failed(format!(
                                        "Paired, but the address could not be saved: {e}"
                                    )),
                                );
                            }
                        }
                    } else {
                        show_result(&status, &ActionResult::from_result("Pairing", &r));
                    }
                    cont.set_sensitive(true);
                });
            }
        });
    }

    {
        let code_tx = code_tx.clone();
        let code = code.clone();
        pair.connect_clicked(move |b| {
            if let Some(tx) = code_tx.borrow_mut().take() {
                let _ = tx.send(code.text().to_string());
                b.set_sensitive(false);
            }
        });
    }
    {
        let code_tx = code_tx.clone();
        window.connect_close_request(move |_| {
            // Unblocks a pairing thread waiting for a code; it aborts before pair-complete.
            code_tx.borrow_mut().take();
            glib::Propagation::Proceed
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    set_margins(&content, 16);
    content.append(&intro);
    content.append(&address);
    content.append(&find);
    content.append(&found);
    content.append(&cont);
    content.append(&code_label);
    content.append(&code);
    content.append(&pair);
    content.append(&status);
    window.set_child(Some(&content));
    window.present();
}
