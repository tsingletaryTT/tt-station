//! The Models page: search, kind and source filters, "cached only", the grouped list and a
//! detail pane with a "Run..." button.
//!
//! The groups come from [`view_model::group_models`]. The list is a `gtk::ListView` over a
//! `gtk::StringList`: one string per heading or model, with widgets made only for the rows on
//! screen, so a catalog of dozens or hundreds of models scrolls without building a widget per
//! row up front.
//!
//! The spec's detail pane shows `tt model info` data. No `Station` method returns that yet, so
//! the detail pane shows the fields of [`ModelRow`] (kind, source, hardware, cached, fit).
//!
//! **Reference performance.** For this machine the page also shows tt-inference-server's
//! reference targets ([`crate::perf`]): a short tokens-per-second suffix on list rows and a
//! labelled block in the detail pane, both built by [`view_model::model_line`] and
//! [`view_model::model_detail`]. The table is found and parsed by
//! [`Station::perf_reference`](crate::station::Station::perf_reference) in a second task that
//! starts after the rows are shown (so the list never waits for it), on the tokio runtime and
//! never on the GTK thread, and is cached by the station. When it arrives the list is redrawn
//! with the selected model kept selected, and the count line gains a one-line legend saying
//! the figures are targets. A box has no table, so its rows and details show no performance
//! text.
//!
//! **Switching stations.** A load first empties the list, so the previous station's rows and an
//! enabled "Run..." never stay on screen while the next list loads. A selection remembers the
//! station its list came from, and "Run..." opens the run sheet only when that is the station
//! now selected ([`view_model::run_target`]).
//!
//! Never looked at or clicked: the list's look, scrolling, the heading style, selection and
//! the filter dropdowns' behaviour.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gtk::prelude::*;
use gtk4 as gtk;

use crate::perf::PerfRef;
use crate::station::{ModelRow, StationId};

use super::view_model::{
    filter_choices, group_models, load_is_current, model_detail, model_line, models_status,
    needs_reload, on_load_dropped, perf_sample_model, reselect, run_target, ChosenModel,
    ModelFilter, ModelGroups, PerfContext,
};
use super::{set_margins, spawn_call, App};

/// One line of the list: a group heading or a model with the heading of its group.
#[derive(Clone)]
enum Entry {
    Heading(String),
    Model(ModelRow, &'static str),
}

/// What the page keeps between events.
struct Page {
    rows: RefCell<Vec<ModelRow>>,
    mesh_known: Cell<bool>,
    /// The loaded station's mesh, for the reference performance lookup.
    mesh: RefCell<Option<String>>,
    /// The loaded station's reference performance table, when it has one (this machine only).
    perf: RefCell<Option<Arc<PerfRef>>>,
    entries: RefCell<Vec<Entry>>,
    /// The station index the rows were loaded for, so a station change triggers a reload.
    loaded_for: Cell<Option<usize>>,
    /// The id of the station whose models `rows` holds, `None` while a load runs. A selection
    /// records it, so "Run..." can refuse a model from another station's list.
    rows_for: RefCell<Option<StationId>>,
    strings: gtk::StringList,
    /// The list's selection model, kept here so a redraw can keep the chosen model selected.
    selection: gtk::SingleSelection,
    search: gtk::SearchEntry,
    kind: gtk::DropDown,
    source: gtk::DropDown,
    cached: gtk::CheckButton,
    status: gtk::Label,
}

const ALL_KINDS: &str = "All kinds";
const ALL_SOURCES: &str = "All sources";

impl Page {
    /// Run `f` with the reference performance context for the loaded station: `Some` only when
    /// both a table and a mesh are known.
    fn with_perf<R>(&self, f: impl FnOnce(Option<PerfContext<'_>>) -> R) -> R {
        let perf = self.perf.borrow();
        let mesh = self.mesh.borrow();
        let ctx = match (perf.as_deref(), mesh.as_deref()) {
            (Some(table), Some(mesh)) => Some(PerfContext { table, mesh }),
            _ => None,
        };
        f(ctx)
    }

    /// The filter as the widgets show it now.
    fn filter(&self) -> ModelFilter {
        let pick = |d: &gtk::DropDown| -> Option<String> {
            let item = d.selected_item()?.downcast::<gtk::StringObject>().ok()?;
            let s = item.string().to_string();
            (s != ALL_KINDS && s != ALL_SOURCES).then_some(s)
        };
        ModelFilter {
            query: self.search.text().to_string(),
            kind: pick(&self.kind),
            source: pick(&self.source),
            cached_only: self.cached.is_active(),
        }
    }

    /// Regroup the loaded rows with the current filter and replace the list's strings.
    fn regroup(&self) {
        let groups: ModelGroups =
            group_models(&self.rows.borrow(), &self.filter(), self.mesh_known.get());
        let mut entries = Vec::new();
        for (heading, rows) in groups.sections() {
            entries.push(Entry::Heading(format!("{heading} ({})", rows.len())));
            entries.extend(rows.iter().cloned().map(|r| Entry::Model(r, heading)));
        }
        let lines: Vec<String> =
            self.with_perf(|ctx| entries.iter().map(|e| line(e, ctx)).collect());
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        *self.entries.borrow_mut() = entries;
        self.strings.splice(0, self.strings.n_items(), &refs);
        self.status.remove_css_class("error");
        let shown = groups.runs_here.len()
            + groups.experimental.len()
            + groups.other_hardware.len()
            + groups.unknown_fit.len();
        let perf_shown = self.with_perf(|ctx| ctx.is_some());
        self.status
            .set_text(&models_status(shown, self.rows.borrow().len(), perf_shown));
    }

    /// [`Page::regroup`], then select the same model again if it is still listed. Used when
    /// reference numbers arrive after the rows, so the person's selection survives the redraw
    /// (the selection handler then refreshes the detail pane with the numbers).
    fn regroup_keeping_selection(&self) {
        let chosen = match self
            .entries
            .borrow()
            .get(self.selection.selected() as usize)
        {
            Some(Entry::Model(r, _)) => Some(r.name.clone()),
            _ => None,
        };
        self.regroup();
        let index = {
            let entries = self.entries.borrow();
            let names: Vec<Option<&str>> = entries
                .iter()
                .map(|e| match e {
                    Entry::Model(r, _) => Some(r.name.as_str()),
                    Entry::Heading(_) => None,
                })
                .collect();
            reselect(&names, chosen.as_deref())
        };
        if let Some(i) = index {
            self.selection.set_selected(i as u32);
        }
    }
}

/// The text of one list line (the model text comes from [`view_model::model_line`]).
fn line(e: &Entry, ctx: Option<PerfContext<'_>>) -> String {
    match e {
        Entry::Heading(h) => h.clone(),
        Entry::Model(r, _) => model_line(r, ctx),
    }
}

pub fn build(app: &App) -> gtk::Widget {
    let heading = gtk::Label::new(Some(&app.current_heading()));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");

    let strings = gtk::StringList::new(&[]);
    let page = Rc::new(Page {
        rows: RefCell::new(Vec::new()),
        mesh_known: Cell::new(false),
        mesh: RefCell::new(None),
        perf: RefCell::new(None),
        entries: RefCell::new(Vec::new()),
        loaded_for: Cell::new(None),
        rows_for: RefCell::new(None),
        selection: gtk::SingleSelection::new(Some(strings.clone())),
        strings,
        search: gtk::SearchEntry::new(),
        kind: gtk::DropDown::from_strings(&[ALL_KINDS]),
        source: gtk::DropDown::from_strings(&[ALL_SOURCES]),
        cached: gtk::CheckButton::with_label("Cached only"),
        status: gtk::Label::new(None),
    });
    page.search.set_hexpand(true);
    page.search.set_placeholder_text(Some("Search models"));
    page.status.set_xalign(0.0);
    page.status.set_wrap(true);
    page.status.set_selectable(true);
    let refresh = gtk::Button::with_label("Refresh");

    // List: one Label per visible row, text and style set on bind.
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        set_margins(&label, 4);
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&label));
        }
    });
    {
        let page = page.clone();
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(label) = item.child().and_downcast::<gtk::Label>() else {
                return;
            };
            let text = item
                .item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string())
                .unwrap_or_default();
            label.set_text(&text);
            let is_heading = matches!(
                page.entries.borrow().get(item.position() as usize),
                Some(Entry::Heading(_))
            );
            if is_heading {
                label.add_css_class("heading");
            } else {
                label.remove_css_class("heading");
            }
        });
    }
    let selection = page.selection.clone();
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    let list_scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hexpand(true)
        .build();

    // Detail pane.
    let detail_label = gtk::Label::new(Some("Select a model to see its details."));
    detail_label.set_xalign(0.0);
    detail_label.set_yalign(0.0);
    detail_label.set_wrap(true);
    // Let the label break inside long words (model ids such as `org/very-long-model-name` have
    // no spaces) and cap its natural width. No `width_chars`: the detail box's 280 px size
    // request sets the minimum width.
    detail_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    detail_label.set_max_width_chars(48);
    detail_label.set_selectable(true);
    // The label sits in a vertical-only ScrolledWindow so the detail pane's minimum height does
    // not depend on its width. A wrapping label placed straight in the `gtk::Paned` below made
    // the Paned report one minimum height when measured without a width and a larger one for a
    // given width (the label wraps onto more lines at the narrow width the Paned gives its end
    // child), and GTK logged "GtkPaned ... reports a minimum height of 286, but minimum height
    // for width of 1048576 is 381. Expect overlapping widgets." each time a model was selected.
    let detail_scroll = gtk::ScrolledWindow::builder()
        .child(&detail_label)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(120)
        .vexpand(true)
        .build();
    let run = gtk::Button::with_label("Run...");
    run.set_sensitive(false);
    let chosen: Rc<RefCell<Option<ChosenModel>>> = Rc::new(RefCell::new(None));
    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    detail_box.set_size_request(280, -1);
    set_margins(&detail_box, 8);
    detail_box.append(&detail_scroll);
    detail_box.append(&run);

    {
        let page = page.clone();
        let detail_label = detail_label.clone();
        let run = run.clone();
        let chosen = chosen.clone();
        selection.connect_selected_notify(move |sel| {
            let entry = page.entries.borrow().get(sel.selected() as usize).cloned();
            match entry {
                Some(Entry::Model(r, group)) => {
                    let text = page.with_perf(|ctx| model_detail(&r, group, ctx));
                    detail_label.set_text(&text);
                    let station = page.rows_for.borrow().clone();
                    *chosen.borrow_mut() = station.map(|station| ChosenModel {
                        station,
                        model: r.name.clone(),
                    });
                    run.set_sensitive(chosen.borrow().is_some());
                }
                _ => {
                    detail_label.set_text("Select a model to see its details.");
                    *chosen.borrow_mut() = None;
                    run.set_sensitive(false);
                }
            }
        });
    }
    {
        let app = app.clone();
        run.connect_clicked(move |b| {
            // Only a model chosen on the station the run sheet will use (the selected one).
            let current = app.current().0.id();
            let Some(model) = run_target(chosen.borrow().as_ref(), &current).map(str::to_string)
            else {
                return;
            };
            let parent = b.root().and_then(|r| r.downcast::<gtk::Window>().ok());
            super::run_sheet::open(&app, parent.as_ref(), &model);
        });
    }

    // Filters regroup on every change.
    {
        let p = page.clone();
        page.search.connect_search_changed(move |_| p.regroup());
    }
    {
        let p = page.clone();
        page.kind.connect_selected_notify(move |_| p.regroup());
    }
    {
        let p = page.clone();
        page.source.connect_selected_notify(move |_| p.regroup());
    }
    {
        let p = page.clone();
        page.cached.connect_toggled(move |_| p.regroup());
    }
    {
        let app = app.clone();
        let page = page.clone();
        refresh.connect_clicked(move |_| load(&app, &page));
    }
    {
        // Reload when the selected station changes; update the heading on any change.
        let weak = Rc::downgrade(app);
        let page = page.clone();
        let heading = heading.clone();
        app.on_change(move || {
            let Some(app) = weak.upgrade() else { return };
            heading.set_text(&app.current_heading());
            if needs_reload(page.loaded_for.get(), app.selected.get()) {
                load(&app, &page);
            }
        });
    }

    let filters = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    filters.append(&page.search);
    filters.append(&page.kind);
    filters.append(&page.source);
    filters.append(&page.cached);
    filters.append(&refresh);

    let body = gtk::Paned::new(gtk::Orientation::Horizontal);
    body.set_start_child(Some(&list_scroll));
    body.set_end_child(Some(&detail_box));
    body.set_resize_end_child(false);
    // Neither side may shrink below its own minimum, so the Paned's minimum size is the sum of
    // its children's and the detail pane's text never overlaps the list.
    body.set_shrink_start_child(false);
    body.set_shrink_end_child(false);
    body.set_vexpand(true);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 8);
    set_margins(&root, 12);
    root.append(&heading);
    root.append(&filters);
    root.append(&page.status);
    root.append(&body);
    root.upcast()
}

/// Fetch the selected station's models and its mesh, then refill the filters and the list.
/// Reference performance is fetched afterwards by [`load_perf`], so the rows never wait for
/// it.
fn load(app: &App, page: &Rc<Page>) {
    let i = app.selected.get();
    page.loaded_for.set(Some(i));
    let (station, _) = app.current();
    // Clear the previous list first, so no row (and no enabled "Run...") from another station
    // stays on screen while this one loads. Emptying the list unselects, which clears the
    // chosen model and disables "Run..." through the selection handler.
    *page.rows_for.borrow_mut() = None;
    page.selection.set_selected(gtk::INVALID_LIST_POSITION);
    page.rows.borrow_mut().clear();
    page.regroup();
    page.status.set_text("Loading models...");
    let page = page.clone();
    let app2 = app.clone();
    let station2 = station.clone();
    spawn_call(
        app,
        async move {
            let models = station.models().await;
            // Read the mesh after `models()`: the local station learns it from `tt model list`.
            let mesh = station.state().await.mesh;
            (models, mesh)
        },
        move |(models, mesh)| {
            // Drop a result that arrived after the person switched station or a newer load
            // started; showing it would let "Run..." send this station's model elsewhere.
            if !load_is_current(page.loaded_for.get(), app2.selected.get(), i) {
                // Clear this load's marker so returning to station `i` reloads it.
                page.loaded_for
                    .set(on_load_dropped(page.loaded_for.get(), i));
                return;
            }
            // Never show another station's table with these rows.
            *page.perf.borrow_mut() = None;
            match models {
                Ok(rows) => {
                    let (kinds, sources) = filter_choices(&rows);
                    let mut k = vec![ALL_KINDS.to_string()];
                    k.extend(kinds);
                    let mut s = vec![ALL_SOURCES.to_string()];
                    s.extend(sources);
                    let kr: Vec<&str> = k.iter().map(String::as_str).collect();
                    let sr: Vec<&str> = s.iter().map(String::as_str).collect();
                    let sample = perf_sample_model(&rows).map(str::to_string);
                    *page.rows.borrow_mut() = rows;
                    *page.rows_for.borrow_mut() = Some(station2.id());
                    page.mesh_known.set(mesh.is_some());
                    *page.mesh.borrow_mut() = mesh;
                    page.kind.set_model(Some(&gtk::StringList::new(&kr)));
                    page.source.set_model(Some(&gtk::StringList::new(&sr)));
                    page.regroup();
                    load_perf(&app2, &page, station2, i, sample);
                }
                Err(e) => {
                    page.rows.borrow_mut().clear();
                    page.regroup();
                    page.status
                        .set_text(&format!("Could not list models: {e:#}"));
                    page.status.add_css_class("error");
                }
            }
        },
    );
}

/// Fetch the reference performance table for the station of load `i` (one dry run and a file
/// parse the first time, cached by the station afterwards; always `None` for a box), then
/// attach it and redraw the list, keeping the selected model selected. A result for a load
/// that is no longer current is dropped.
fn load_perf(
    app: &App,
    page: &Rc<Page>,
    station: Arc<crate::station::Station>,
    i: usize,
    sample: Option<String>,
) {
    let page = page.clone();
    let app2 = app.clone();
    spawn_call(
        app,
        async move { station.perf_reference(sample.as_deref()).await },
        move |perf| {
            if perf.is_none() || !load_is_current(page.loaded_for.get(), app2.selected.get(), i) {
                return;
            }
            *page.perf.borrow_mut() = perf;
            page.regroup_keeping_selection();
        },
    );
}
