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
//! Unverified (headless build): the list's look, scrolling, the heading style, selection and
//! the filter dropdowns' behaviour.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;

use crate::station::ModelRow;

use super::view_model::{filter_choices, group_models, ModelFilter, ModelGroups};
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
    entries: RefCell<Vec<Entry>>,
    /// The station index the rows were loaded for, so a station change triggers a reload.
    loaded_for: Cell<Option<usize>>,
    strings: gtk::StringList,
    search: gtk::SearchEntry,
    kind: gtk::DropDown,
    source: gtk::DropDown,
    cached: gtk::CheckButton,
    status: gtk::Label,
}

const ALL_KINDS: &str = "All kinds";
const ALL_SOURCES: &str = "All sources";

impl Page {
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
        let lines: Vec<String> = entries.iter().map(line).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        *self.entries.borrow_mut() = entries;
        self.strings.splice(0, self.strings.n_items(), &refs);
        self.status.remove_css_class("error");
        self.status.set_text(&format!(
            "{} of {} models shown",
            groups.runs_here.len()
                + groups.experimental.len()
                + groups.other_hardware.len()
                + groups.unknown_fit.len(),
            self.rows.borrow().len()
        ));
    }
}

/// The text of one list line.
fn line(e: &Entry) -> String {
    match e {
        Entry::Heading(h) => h.clone(),
        Entry::Model(r, _) => {
            let mut s = format!("    {}", r.name);
            if let Some(k) = &r.kind {
                s.push_str(&format!("  ·  {k}"));
            }
            s.push_str(&format!("  ·  {}", r.source));
            if r.cached {
                s.push_str("  ·  cached");
            }
            s
        }
    }
}

/// The detail pane text for a model.
fn detail(r: &ModelRow, group: &str) -> String {
    let hw = if r.hardware.is_empty() {
        "(not listed)".to_string()
    } else {
        r.hardware.join(", ")
    };
    format!(
        "{}\n\nGroup: {group}\nKind: {}\nSource: {}\nHardware: {hw}\nWeights cached: {}",
        r.name,
        r.kind.as_deref().unwrap_or("(not reported)"),
        r.source,
        if r.cached { "yes" } else { "no" },
    )
}

pub fn build(app: &App) -> gtk::Widget {
    let heading = gtk::Label::new(Some(&app.current_heading()));
    heading.set_xalign(0.0);
    heading.add_css_class("heading");

    let page = Rc::new(Page {
        rows: RefCell::new(Vec::new()),
        mesh_known: Cell::new(false),
        entries: RefCell::new(Vec::new()),
        loaded_for: Cell::new(None),
        strings: gtk::StringList::new(&[]),
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
    let selection = gtk::SingleSelection::new(Some(page.strings.clone()));
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
    detail_label.set_selectable(true);
    let run = gtk::Button::with_label("Run...");
    run.set_sensitive(false);
    let chosen: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let detail_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    detail_box.set_size_request(280, -1);
    set_margins(&detail_box, 8);
    detail_box.append(&detail_label);
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
                    detail_label.set_text(&detail(&r, group));
                    *chosen.borrow_mut() = Some(r.name.clone());
                    run.set_sensitive(true);
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
            let Some(model) = chosen.borrow().clone() else {
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
            if page.loaded_for.get() != Some(app.selected.get()) {
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
fn load(app: &App, page: &Rc<Page>) {
    let i = app.selected.get();
    page.loaded_for.set(Some(i));
    let (station, _) = app.current();
    page.status.set_text("Loading models...");
    let page = page.clone();
    spawn_call(
        app,
        async move {
            let models = station.models().await;
            // Read the mesh after `models()`: the local station learns it from `tt model list`.
            let mesh = station.state().await.mesh;
            (models, mesh)
        },
        move |(models, mesh)| match models {
            Ok(rows) => {
                let (kinds, sources) = filter_choices(&rows);
                let mut k = vec![ALL_KINDS.to_string()];
                k.extend(kinds);
                let mut s = vec![ALL_SOURCES.to_string()];
                s.extend(sources);
                let kr: Vec<&str> = k.iter().map(String::as_str).collect();
                let sr: Vec<&str> = s.iter().map(String::as_str).collect();
                *page.rows.borrow_mut() = rows;
                page.mesh_known.set(mesh.is_some());
                page.kind.set_model(Some(&gtk::StringList::new(&kr)));
                page.source.set_model(Some(&gtk::StringList::new(&sr)));
                page.regroup();
            }
            Err(e) => {
                page.rows.borrow_mut().clear();
                page.regroup();
                page.status
                    .set_text(&format!("Could not list models: {e:#}"));
                page.status.add_css_class("error");
            }
        },
    );
}
