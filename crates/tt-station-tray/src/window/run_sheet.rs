//! The Run dialog for one model on the selected station.
//!
//! Layout: the model, whether Start is allowed ([`view_model::run_gate`]), an "Advanced"
//! expander with one field per option of the station's [`RunSchema`] (plus "Force" for a box),
//! the command preview, and Start (or Restart when the station is serving).
//!
//! **The preview is the command that runs.** The text comes from [`RunForm::command_preview`],
//! which calls `local::local_serve_argv`, the function `LocalStation::run` uses. For the local
//! station the dialog fetches `tt --json serve <model> --dry-run` ([`Station::serve_plan`]) so
//! the preview classifies catalog against bundle exactly as the run will. For a box the preview
//! lists only the options sent, and says so, because the agent builds the command.
//!
//! **Fields come from the schema.** The local station's schema is `run_schema()`; a box's comes
//! from `GET /run/schema`. No flag list is hardcoded here.
//!
//! Remembered options: the form starts from the options last used for this model on this
//! station (`Registry::last_run`), and a successful Start saves the new ones.
//!
//! Unverified (headless build): the dialog's layout, the expander, the field widgets, and the
//! enable and disable behaviour of Start. Named presets from the spec are not built.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use libttstation::run_options::{OptionSpec, RunSchema};

use crate::station::Station;

use super::view_model::{run_gate, ActionResult, PreviewTarget, RunForm, RunGate};
use super::{set_margins, show_result, spawn_call, App};

pub fn open(app: &App, parent: Option<&gtk::Window>, model: &str) {
    let (station, state) = app.current();
    let model = model.to_string();

    let gate = match &state {
        Some(s) => run_gate(s),
        None => RunGate::Disabled(
            "This station has not been checked yet; press Refresh on the Stations page.".into(),
        ),
    };
    let serving = state.as_ref().is_some_and(|s| s.serving.is_some());
    let is_remote = matches!(*station, Station::Remote(_));
    let target = match &*station {
        Station::Local(l) => PreviewTarget::Local {
            program: l.tt_bin().display().to_string(),
            default_port: l.default_port(),
            plan: None,
        },
        Station::Remote(_) => PreviewTarget::Remote,
    };

    let window = gtk::Window::builder()
        .title(format!("Run {model}"))
        .modal(true)
        .default_width(560)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    let title = gtk::Label::new(Some(&model));
    title.add_css_class("title-2");
    title.set_xalign(0.0);
    let gate_label = gtk::Label::new(None);
    gate_label.set_xalign(0.0);
    gate_label.set_wrap(true);
    if let RunGate::Disabled(why) = &gate {
        gate_label.set_text(why);
        gate_label.add_css_class("error");
    }

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(12);
    let loading = gtk::Label::new(Some("Loading options..."));
    grid.attach(&loading, 0, 0, 2, 1);
    let force =
        gtk::CheckButton::with_label("Force (start even though another tenant holds the chips)");
    force.set_visible(is_remote);
    let advanced_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    advanced_box.append(&grid);
    advanced_box.append(&force);
    let advanced = gtk::Expander::new(Some("Advanced"));
    advanced.set_child(Some(&advanced_box));

    let preview_heading = gtk::Label::new(Some(if is_remote {
        "Preview (options only; the box builds the command)"
    } else {
        "Exact command"
    }));
    preview_heading.set_xalign(0.0);
    preview_heading.add_css_class("heading");
    let preview = gtk::Label::new(None);
    preview.set_xalign(0.0);
    preview.set_wrap(true);
    preview.set_selectable(true);
    preview.add_css_class("monospace");
    let field_error = gtk::Label::new(None);
    field_error.set_xalign(0.0);
    field_error.set_wrap(true);
    field_error.add_css_class("error");

    let start = gtk::Button::with_label(if serving { "Restart" } else { "Start" });
    start.add_css_class("suggested-action");
    start.set_sensitive(false);
    let result = gtk::Label::new(None);
    result.set_xalign(0.0);
    result.set_wrap(true);
    result.set_selectable(true);

    let form: Rc<RefCell<Option<RunForm>>> = Rc::new(RefCell::new(None));

    // Recompute the preview, the field error and Start's sensitivity from the form.
    let update: Rc<dyn Fn()> = {
        let form = form.clone();
        let preview = preview.clone();
        let field_error = field_error.clone();
        let start = start.clone();
        let model = model.clone();
        let allowed = gate == RunGate::Allowed;
        Rc::new(move || {
            let f = form.borrow();
            let Some(f) = f.as_ref() else { return };
            preview.set_text(&f.command_preview(&model));
            match f.to_options() {
                Ok(_) => {
                    field_error.set_text("");
                    start.set_sensitive(allowed);
                }
                Err(e) => {
                    field_error.set_text(&format!("{}: {}", e.field, e.message));
                    start.set_sensitive(false);
                }
            }
        })
    };

    // Schema first, then build the fields.
    {
        let form = form.clone();
        let update = update.clone();
        let grid = grid.clone();
        let last = app
            .registry
            .borrow()
            .last_run(&station.id(), &model)
            .cloned();
        let st = station.clone();
        spawn_call(app, async move { st.schema().await }, move |r| match r {
            Ok(schema) => {
                let f = RunForm::from_schema(&schema, last.as_ref()).with_target(target);
                *form.borrow_mut() = Some(f);
                grid.remove(&loading);
                build_fields(&grid, &schema, &form, &update);
                update();
            }
            Err(e) => loading.set_text(&format!("Could not load the run options: {e:#}")),
        });
    }
    // The dry-run plan, for the local preview. A failure leaves the catalog default and says so.
    if !is_remote {
        let form = form.clone();
        let update = update.clone();
        let st = station.clone();
        let m = model.clone();
        let result = result.clone();
        spawn_call(
            app,
            async move { st.serve_plan(&m).await },
            move |r| match r {
                Ok(plan) => {
                    if let Some(f) = form.borrow_mut().as_mut() {
                        f.set_plan(plan);
                    }
                    update();
                }
                Err(e) => result.set_text(&format!(
                    "Could not ask tt how it will serve this model ({e:#}); the preview assumes a \
                 catalog model."
                )),
            },
        );
    }

    {
        let app = app.clone();
        let form = form.clone();
        let result = result.clone();
        let force = force.clone();
        let station = station.clone();
        start.connect_clicked(move |btn| {
            let opts = match form.borrow().as_ref().map(RunForm::to_options) {
                Some(Ok(o)) => o,
                Some(Err(e)) => {
                    show_result(&result, &ActionResult::Failed(e.to_string()));
                    return;
                }
                None => return,
            };
            let verb = if serving { "Restart" } else { "Start" };
            btn.set_sensitive(false);
            result.remove_css_class("error");
            result.set_text(&format!(
                "{verb}ing {model}... this can take several minutes."
            ));
            let st = station.clone();
            let m = model.clone();
            let o = opts.clone();
            let f = force.is_active();
            let app2 = app.clone();
            let result = result.clone();
            let btn = btn.clone();
            let model = model.clone();
            // The station this sheet was opened for, even if the selection changes meanwhile.
            let id = station.id();
            spawn_call(
                &app,
                async move {
                    if serving {
                        st.stop().await?;
                    }
                    st.run(&m, f, &o).await
                },
                move |r| {
                    let mut shown = ActionResult::from_result(verb, &r);
                    if let Ok(now) = &r {
                        shown = ActionResult::Ok(format!(
                            "{verb} done: {} at {}",
                            now.model, now.base_url
                        ));
                        let mut reg = app2.registry.borrow_mut();
                        reg.remember_run(&id, &model, &opts);
                        if let Err(e) = reg.save() {
                            eprintln!("tt-station-tray: could not save the registry: {e:#}");
                        }
                    }
                    // A 409 holder message or any other Err text is shown word for word.
                    show_result(&result, &shown);
                    btn.set_sensitive(true);
                    super::stations_pane::refresh_all(&app2);
                },
            );
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    set_margins(&content, 16);
    content.append(&title);
    content.append(&gate_label);
    content.append(&advanced);
    content.append(&preview_heading);
    content.append(&preview);
    content.append(&field_error);
    content.append(&start);
    content.append(&result);
    window.set_child(Some(
        &gtk::ScrolledWindow::builder().child(&content).build(),
    ));
    window.present();
}

/// One row per option: a label (with the description as tooltip) and a widget that writes into
/// the form on every change.
fn build_fields(
    grid: &gtk::Grid,
    schema: &RunSchema,
    form: &Rc<RefCell<Option<RunForm>>>,
    update: &Rc<dyn Fn()>,
) {
    for (row, spec) in schema.options.iter().enumerate() {
        let row = row as i32;
        let bundle_only = !spec.applies_to.iter().any(|a| a == "catalog");
        let label = gtk::Label::new(Some(&if bundle_only {
            format!("{} (bundles only)", spec.name)
        } else {
            spec.name.clone()
        }));
        label.set_xalign(0.0);
        label.set_tooltip_text(Some(&spec.description));
        grid.attach(&label, 0, row, 1, 1);
        let current = form
            .borrow()
            .as_ref()
            .map(|f| f.get(&spec.name).to_string())
            .unwrap_or_default();
        let widget = field_widget(spec, &current, form, update);
        widget.set_tooltip_text(Some(&spec.description));
        grid.attach(&widget, 1, row, 1, 1);
    }
}

fn field_widget(
    spec: &OptionSpec,
    current: &str,
    form: &Rc<RefCell<Option<RunForm>>>,
    update: &Rc<dyn Fn()>,
) -> gtk::Widget {
    let name = spec.name.clone();
    let form = form.clone();
    let update = update.clone();
    let set = move |v: &str| {
        if let Some(f) = form.borrow_mut().as_mut() {
            f.set(&name, v);
        }
        update();
    };
    match spec.kind.as_str() {
        "bool" => {
            let c = gtk::CheckButton::new();
            c.set_active(current == "true");
            c.connect_toggled(move |c| set(if c.is_active() { "true" } else { "false" }));
            c.upcast()
        }
        "choice" if !spec.choices.is_empty() => {
            let refs: Vec<&str> = spec.choices.iter().map(String::as_str).collect();
            let d = gtk::DropDown::from_strings(&refs);
            if let Some(i) = spec.choices.iter().position(|c| c == current) {
                d.set_selected(i as u32);
            }
            let choices = spec.choices.clone();
            d.connect_selected_notify(move |d| {
                if let Some(c) = choices.get(d.selected() as usize) {
                    set(c);
                }
            });
            d.upcast()
        }
        _ => {
            let e = gtk::Entry::new();
            e.set_text(current);
            e.set_hexpand(true);
            if let Some(d) = &spec.default {
                e.set_placeholder_text(Some(d));
            } else if let (Some(lo), Some(hi)) = (spec.min, spec.max) {
                e.set_placeholder_text(Some(&format!("{lo} to {hi}")));
            }
            e.connect_changed(move |e| set(&e.text()));
            e.upcast()
        }
    }
}
