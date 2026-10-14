//! Everything the main window decides, as plain data with no GTK and no I/O.
//!
//! The GTK files next to this one (`stations_pane.rs`, `models_pane.rs`, `run_sheet.rs`,
//! `pair_dialog.rs`) only copy values from here into widgets and send clicks back. Every rule
//! about what the window shows lives in this file so that each rule is a unit test. No one has
//! looked at this window or exercised its click paths, so these tests are the only check on
//! what it shows.
//!
//! What is here:
//! * [`group_models`] and [`ModelFilter`]: the model browser's search, filters and groups.
//! * [`RunForm`]: the run sheet's field values, their conversion to [`RunOptions`], and the
//!   command preview. The preview calls [`local_serve_argv`], the function
//!   [`crate::local::LocalStation::run`] uses, so the preview shows the command that runs.
//! * [`place_state`], [`station_with_registry_mac`] and [`station_row`]: where a poll answer
//!   goes (by station id), the Stations pane rows, and the MAC of an offline box (the tray's
//!   Wake item depends on it).
//! * [`run_gate`]: whether the run sheet's Start button is enabled.
//! * [`ActionResult`]: the text shown after Stop, Run or Logs, which keeps the error text the
//!   station returned (for example a 409 that names who holds the chips).

use std::collections::BTreeMap;

use libttstation::official_tt::OFFICIAL_TT_DEFAULT_BIN;
use libttstation::run_options::{
    validate_model_name, OptionError, OptionSpec, RunOptions, RunSchema,
};
use serde_json::{json, Map, Value};

use crate::local::{local_serve_argv, DEFAULT_SERVE_PORT};
use crate::menu_model::Dot;
use crate::perf::{self, PerfRef, PerfSummary};
use crate::registry::KnownStation;
use crate::station::{ModelRow, StationId, StationState};

// ---------------------------------------------------------------------------------------------
// Model browser
// ---------------------------------------------------------------------------------------------

/// The model browser's search box and filter dropdowns. An empty `query` and `None` filters
/// match everything. All set filters must match (they combine with AND).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFilter {
    /// Matched against the model name, ignoring case, as a substring.
    pub query: String,
    /// Exact `ModelRow::kind` (ignoring case). A row with no kind never matches a set filter.
    pub kind: Option<String>,
    /// Exact `ModelRow::source` (ignoring case).
    pub source: Option<String>,
    /// Only rows whose weights are already on disk.
    pub cached_only: bool,
}

/// The browser's groups. Each filtered row lands in exactly one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelGroups {
    /// Released models that list this station's mesh.
    pub runs_here: Vec<ModelRow>,
    /// Community models (Hub bundles, local folders) that list this station's mesh.
    pub experimental: Vec<ModelRow>,
    /// Models that do not list this station's mesh.
    pub other_hardware: Vec<ModelRow>,
    /// Every row, when the station's mesh is not known (a box whose `/status` has no
    /// `device_mesh`, or the local station before `tt model list` has named a device). Shown
    /// with no verdict, because `ModelRow::fits` is false for all of them and says nothing.
    pub unknown_fit: Vec<ModelRow>,
}

/// Whether `source` names a released model rather than a community one.
///
/// Released: `tt-inference-server` and `tt catalog` (tt's own catalog), and `box` (a box's
/// `/models` lists only what its agent can serve with vLLM). Everything else, such as
/// `HuggingFace` or `local`, is community.
pub fn is_released_source(source: &str) -> bool {
    ["tt-inference-server", "tt catalog", "box"]
        .iter()
        .any(|s| s.eq_ignore_ascii_case(source))
}

/// Whether `row` passes every set part of `filter`.
fn matches(row: &ModelRow, filter: &ModelFilter) -> bool {
    let query = filter.query.trim().to_lowercase();
    let name_ok = query.is_empty() || row.name.to_lowercase().contains(&query);
    let kind_ok = match &filter.kind {
        None => true,
        Some(k) => row
            .kind
            .as_deref()
            .is_some_and(|rk| rk.eq_ignore_ascii_case(k)),
    };
    let source_ok = match &filter.source {
        None => true,
        Some(s) => row.source.eq_ignore_ascii_case(s),
    };
    name_ok && kind_ok && source_ok && (row.cached || !filter.cached_only)
}

/// Filter `rows` with `filter` and sort them into groups, keeping their order.
///
/// `mesh_known` says whether the station's mesh is known. When it is false every row goes to
/// [`ModelGroups::unknown_fit`]: `fits` is false for every row then, and putting them all in
/// "needs other hardware" would grey out every model on a box that simply did not report its
/// mesh. When it is true: a row that fits goes to `runs_here` if its source is released (see
/// [`is_released_source`]) and to `experimental` otherwise; a row that does not fit goes to
/// `other_hardware`.
///
/// The brief's signature had no `mesh_known` argument. It was added because `ModelRow` has no
/// "unknown" state of its own.
pub fn group_models(rows: &[ModelRow], filter: &ModelFilter, mesh_known: bool) -> ModelGroups {
    let mut g = ModelGroups::default();
    for row in rows.iter().filter(|r| matches(r, filter)) {
        let group = if !mesh_known {
            &mut g.unknown_fit
        } else if !row.fits {
            &mut g.other_hardware
        } else if is_released_source(&row.source) {
            &mut g.runs_here
        } else {
            &mut g.experimental
        };
        group.push(row.clone());
    }
    g
}

impl ModelGroups {
    /// The non-empty groups in display order, each with its heading. The browser shows them
    /// as sections of one list.
    pub fn sections(&self) -> Vec<(&'static str, &[ModelRow])> {
        [
            ("Runs on this station", self.runs_here.as_slice()),
            ("Experimental", self.experimental.as_slice()),
            ("Needs other hardware", self.other_hardware.as_slice()),
            (
                "Hardware fit unknown (the station did not report its mesh)",
                self.unknown_fit.as_slice(),
            ),
        ]
        .into_iter()
        .filter(|(_, rows)| !rows.is_empty())
        .collect()
    }
}

/// The distinct kinds and sources in `rows`, sorted, for the filter dropdowns.
pub fn filter_choices(rows: &[ModelRow]) -> (Vec<String>, Vec<String>) {
    let kinds: std::collections::BTreeSet<String> =
        rows.iter().filter_map(|r| r.kind.clone()).collect();
    let sources: std::collections::BTreeSet<String> =
        rows.iter().map(|r| r.source.clone()).collect();
    (kinds.into_iter().collect(), sources.into_iter().collect())
}

/// What the Models page needs to show reference performance: the station's parsed
/// tt-inference-server table and its mesh. The page has one only for a station that has both
/// (today: this machine, once its mesh is known and a reference file was found). With none,
/// the page shows no performance text at all.
#[derive(Debug, Clone, Copy)]
pub struct PerfContext<'a> {
    pub table: &'a PerfRef,
    pub mesh: &'a str,
}

/// The reference numbers for `row` in `ctx`, when there are any.
///
/// Only rows whose `source` is `tt-inference-server` (the official catalog, as
/// `tt --json model list` labels it) are looked up. A Hub bundle or a local folder is a
/// different build of a model, so a released model's targets say nothing about it even when
/// the names match.
pub fn model_perf(row: &ModelRow, ctx: Option<PerfContext<'_>>) -> Option<PerfSummary> {
    let ctx = ctx?;
    if !row.source.eq_ignore_ascii_case("tt-inference-server") {
        return None;
    }
    perf::summary(ctx.table, &row.name, row.hf_repo.as_deref(), ctx.mesh)
}

/// One-line legend for the Models page: a tooltip on the list, and appended to the model count
/// when the page has a reference table.
pub const PERF_LEGEND: &str =
    "Reference numbers are tt-inference-server targets, not measurements from this machine.";

/// The Models page's count line, with [`PERF_LEGEND`] appended when reference numbers are
/// shown (`perf_shown`).
pub fn models_status(shown: usize, total: usize, perf_shown: bool) -> String {
    let base = format!("{shown} of {total} models shown");
    if perf_shown {
        format!("{base}. {PERF_LEGEND}")
    } else {
        base
    }
}

/// Where the model named `chosen` sits in the list after a regroup, so the page can keep it
/// selected when reference numbers arrive after the rows. `names` holds one entry per list
/// line: the model name, or `None` for a heading.
pub fn reselect(names: &[Option<&str>], chosen: Option<&str>) -> Option<usize> {
    let chosen = chosen?;
    names.iter().position(|n| *n == Some(chosen))
}

/// One model's line in the Models list: name, kind, source, `cached`, and the reference
/// tokens-per-second suffix ([`perf::row_suffix`]) when the table has numbers for it.
pub fn model_line(row: &ModelRow, ctx: Option<PerfContext<'_>>) -> String {
    let mut s = format!("    {}", row.name);
    if let Some(k) = &row.kind {
        s.push_str(&format!("  ·  {k}"));
    }
    s.push_str(&format!("  ·  {}", row.source));
    if row.cached {
        s.push_str("  ·  cached");
    }
    if let Some(suffix) = model_perf(row, ctx).as_ref().and_then(perf::row_suffix) {
        s.push_str(&suffix);
    }
    s
}

/// The detail pane text for `row` in group `group`: its fields, then (when `ctx` is given)
/// either the reference performance block ([`perf::detail_block`]) or a line saying the table
/// has no numbers for this mesh ([`perf::no_data_line`]). Without `ctx` nothing about
/// performance is shown, because there is nothing true to say.
pub fn model_detail(row: &ModelRow, group: &str, ctx: Option<PerfContext<'_>>) -> String {
    let hw = if row.hardware.is_empty() {
        "(not listed)".to_string()
    } else {
        row.hardware.join(", ")
    };
    let mut s = format!(
        "{}\n\nGroup: {group}\nKind: {}\nSource: {}\nHardware: {hw}\nWeights cached: {}",
        row.name,
        row.kind.as_deref().unwrap_or("(not reported)"),
        row.source,
        if row.cached { "yes" } else { "no" },
    );
    if let Some(ctx) = ctx {
        let block = match model_perf(row, Some(ctx)) {
            Some(summary) => perf::detail_block(&summary),
            None => perf::no_data_line(ctx.mesh, ctx.table.version.as_deref()),
        };
        s.push_str("\n\n");
        s.push_str(&block);
    }
    s
}

/// A catalog model name for the one `tt serve --dry-run` that tells
/// [`crate::local::LocalStation::perf_reference`] which tt-inference-server checkout `tt` uses:
/// the first row whose source is `tt-inference-server`. `None` when there is none (the lookup
/// then falls back to the newest installed checkout).
pub fn perf_sample_model(rows: &[ModelRow]) -> Option<&str> {
    rows.iter()
        .find(|r| r.source.eq_ignore_ascii_case("tt-inference-server"))
        .map(|r| r.name.as_str())
}

/// Whether a model list fetched for station `i` may still be shown.
///
/// The Models page starts a load for the selected station and records it in `loaded_for`. A
/// slow load (a local `tt model list` can take seconds) may finish after the person picked
/// another station, or after a newer load started. Its rows must then be dropped: shown under
/// the wrong station, "Run..." would send that station's model to a different one. So a result
/// is current only when it belongs to the latest load and that station is still selected.
pub fn load_is_current(loaded_for: Option<usize>, selected: usize, i: usize) -> bool {
    loaded_for == Some(i) && selected == i
}

/// The new `loaded_for` after a load for station `i` was dropped by [`load_is_current`].
///
/// If no newer load has started (`loaded_for` is still `Some(i)`), the marker is cleared to
/// `None`. Otherwise the page would believe station `i` is loaded although its rows were thrown
/// away, and coming back to station `i` would not reload it ("Loading models..." forever). A
/// newer load's marker is left alone.
pub fn on_load_dropped(loaded_for: Option<usize>, i: usize) -> Option<usize> {
    if loaded_for == Some(i) {
        None
    } else {
        loaded_for
    }
}

/// Whether the Models page must start a load for the `selected` station: true unless the latest
/// load was for that station and has not been dropped.
pub fn needs_reload(loaded_for: Option<usize>, selected: usize) -> bool {
    loaded_for != Some(selected)
}

// ---------------------------------------------------------------------------------------------
// Run sheet
// ---------------------------------------------------------------------------------------------

/// What the command preview describes.
#[derive(Debug, Clone, PartialEq)]
pub enum PreviewTarget {
    /// This machine. The preview is the exact `tt serve` command.
    Local {
        /// The `tt` binary as shown to the person (the path `official_tt::resolve_bin` found,
        /// or plain `tt`).
        program: String,
        /// The port used when the form leaves `port` empty. Matches the station's own default.
        default_port: u16,
        /// The `tt --json serve <model> --dry-run` plan, once fetched. Until then the preview
        /// treats the model as a catalog model, the same default `ServeTarget::from_dry_run`
        /// uses for a plan with no `backend` key.
        plan: Option<serde_json::Value>,
    },
    /// A box. Its agent builds the real command, so the preview lists only the options sent.
    Remote,
}

impl PreviewTarget {
    /// A local target running `program` with the station default port and no plan yet.
    pub fn local(program: impl Into<String>) -> PreviewTarget {
        PreviewTarget::Local {
            program: program.into(),
            default_port: DEFAULT_SERVE_PORT,
            plan: None,
        }
    }
}

/// Field values of the run sheet's Advanced section, keyed by `OptionSpec::name`.
///
/// Values are text, as typed. An empty value means "not set". [`RunForm::to_options`] turns the
/// text into [`RunOptions`] through `RunOptions::from_value`, the same validation the agent
/// applies, so the form cannot accept a value the station would refuse.
#[derive(Debug, Clone, PartialEq)]
pub struct RunForm {
    specs: Vec<OptionSpec>,
    values: BTreeMap<String, String>,
    target: PreviewTarget,
}

impl RunForm {
    /// A form for `schema`. Each field starts at the schema default (or empty), then the values
    /// of `last` (the options last used for this model on this station) replace those
    /// defaults. The preview target starts as [`PreviewTarget::local`] with the official binary's
    /// default name (`official_tt::OFFICIAL_TT_DEFAULT_BIN`); call
    /// [`RunForm::with_target`] to change it.
    pub fn from_schema(schema: &RunSchema, last: Option<&RunOptions>) -> RunForm {
        let mut form = RunForm {
            specs: schema.options.clone(),
            values: schema
                .options
                .iter()
                .map(|s| (s.name.clone(), s.default.clone().unwrap_or_default()))
                .collect(),
            target: PreviewTarget::local(OFFICIAL_TT_DEFAULT_BIN),
        };
        if let Some(last) = last {
            for (name, value) in option_fields(last) {
                form.set(&name, &value);
            }
        }
        form
    }

    /// Replace the preview target.
    pub fn with_target(mut self, target: PreviewTarget) -> RunForm {
        self.target = target;
        self
    }

    /// Store the dry-run plan once it arrives (local stations only; ignored for a box).
    pub fn set_plan(&mut self, plan: Option<serde_json::Value>) {
        if let PreviewTarget::Local { plan: p, .. } = &mut self.target {
            *p = plan;
        }
    }

    /// The fields in schema order, for building the form.
    pub fn specs(&self) -> &[OptionSpec] {
        &self.specs
    }

    /// The current text of field `name` (empty when unset or unknown).
    pub fn get(&self, name: &str) -> &str {
        self.values.get(name).map(String::as_str).unwrap_or("")
    }

    /// Set field `name` to `value`. Unknown names are ignored, so a stale widget cannot add a
    /// key the schema does not have.
    pub fn set(&mut self, name: &str, value: &str) {
        if self.specs.iter().any(|s| s.name == name) {
            self.values.insert(name.to_string(), value.to_string());
        }
    }

    /// The form as validated [`RunOptions`].
    ///
    /// A field left at its schema default counts as unset (so the default `workflow` of
    /// `server` sends no `--workflow`). A rejected value comes back as an [`OptionError`] whose
    /// `field` is the form's field name, including `vllm:<flag>` for a vLLM flag, so the run
    /// sheet can mark the field.
    pub fn to_options(&self) -> Result<RunOptions, OptionError> {
        let mut top = Map::new();
        let mut vllm: Vec<Value> = Vec::new();
        for spec in &self.specs {
            let text = self.get(&spec.name).trim();
            if text.is_empty() || Some(text) == spec.default.as_deref() {
                continue;
            }
            if let Some(flag) = spec.name.strip_prefix("vllm:") {
                let arg = match (spec.kind.as_str(), text) {
                    ("bool", "true") => json!({ "flag": flag }),
                    ("bool", "false") => continue,
                    ("bool", _) => return Err(field_error(&spec.name, "must be true or false")),
                    _ => json!({ "flag": flag, "value": text }),
                };
                // Check each flag by itself so the error names this field, not `vllm_args`.
                RunOptions::from_value(&json!({ "vllm_args": [arg.clone()] }))
                    .map_err(|e| field_error(&spec.name, &e.message))?;
                vllm.push(arg);
                continue;
            }
            // Typed values become JSON numbers and booleans. Text that does not parse is passed
            // through as a string, so `from_value` rejects it and names the field.
            let value = match spec.kind.as_str() {
                "int" => text
                    .parse::<u64>()
                    .map(Value::from)
                    .unwrap_or_else(|_| Value::from(text)),
                "bool" => match text {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::from(text),
                },
                _ => Value::from(text),
            };
            top.insert(spec.name.clone(), value);
        }
        if !vllm.is_empty() {
            top.insert("vllm_args".to_string(), Value::Array(vllm));
        }
        RunOptions::from_value(&Value::Object(top))
    }

    /// The exact command, as one line, or a message when the form is not valid.
    ///
    /// Local: `<program> ` followed by [`local_serve_argv`] joined with spaces, the same argv
    /// `LocalStation::run` executes for the same plan and options. Remote: `Options sent to the
    /// box: ` followed by the option arguments, because the agent builds the real command.
    pub fn command_preview(&self, model: &str) -> String {
        let fix = |e: OptionError| format!("Fix this first: {e}");
        let opts = match self.to_options() {
            Ok(o) => o,
            Err(e) => return fix(e),
        };
        match &self.target {
            PreviewTarget::Local {
                program,
                default_port,
                plan,
            } => {
                if let Err(e) = validate_model_name(model) {
                    return fix(e);
                }
                let plan = plan.clone().unwrap_or(Value::Null);
                match local_serve_argv(&plan, model, *default_port, &opts) {
                    Ok(argv) => format!("{program} {}", argv.join(" ")),
                    Err(e) => fix(e),
                }
            }
            PreviewTarget::Remote => {
                let args = opts.to_serve_args();
                if args.is_empty() {
                    "Options sent to the box: (none, the box's defaults apply)".to_string()
                } else {
                    format!("Options sent to the box: {}", args.join(" "))
                }
            }
        }
    }
}

/// An [`OptionError`] for form field `field`. `OptionError::new` is private to libttstation.
fn field_error(field: &str, message: &str) -> OptionError {
    OptionError {
        field: field.to_string(),
        message: message.to_string(),
    }
}

/// `opts` as (form field, text) pairs: the inverse of [`RunForm::to_options`]. Unset fields
/// are left out so the schema default stays. A boolean vLLM flag is `"true"`.
fn option_fields(opts: &RunOptions) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(d) = &opts.device {
        out.push(("device".to_string(), d.clone()));
    }
    if let Some(p) = opts.port {
        out.push(("port".to_string(), p.to_string()));
    }
    if let Some(p) = &opts.profile {
        out.push(("profile".to_string(), p.clone()));
    }
    if opts.offline {
        out.push(("offline".to_string(), "true".to_string()));
    }
    if let Some(w) = opts.workflow {
        out.push(("workflow".to_string(), w.as_str().to_string()));
    }
    for a in &opts.vllm_args {
        let value = a.value.clone().unwrap_or_else(|| "true".to_string());
        out.push((format!("vllm:{}", a.flag), value));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Stations pane
// ---------------------------------------------------------------------------------------------

/// `state` with the registry's MAC filled in when the station did not report one.
///
/// A box that is off cannot answer `/status`, so `RemoteStation::state` leaves `mac` empty for
/// it. The registry remembers the MAC from an earlier visit; without it the tray could not offer
/// Wake for the one box that needs waking. A MAC the station reported itself is kept.
pub fn station_with_registry_mac(
    mut state: StationState,
    known: Option<&KnownStation>,
) -> StationState {
    if state.mac.is_none() {
        state.mac = known.and_then(|k| k.mac.clone());
    }
    state
}

/// The MAC to write back to the registry, when the station reported one the registry lacks or
/// has a different value for. `None` means no change.
pub fn mac_to_remember(state: &StationState, known: &KnownStation) -> Option<String> {
    let mac = state.mac.as_ref()?;
    (known.mac.as_ref() != Some(mac)).then(|| mac.clone())
}

/// What the window does with one station's poll answer: the state to store, and a MAC to save
/// in the registry (by station id) when the answer carried a new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedState {
    pub state: StationState,
    pub remember_mac: Option<(StationId, String)>,
}

/// Decide where a poll answer goes. `slot_id` is the id of the station now at the index the
/// poll was started for (`None` if that index no longer exists).
///
/// The station list is rebuilt when a station is added or removed, and a poll can finish
/// after that. So the answer is dropped unless the station now in the slot has the answer's
/// id. The registry entry is looked up by id as well, never by position, so the offline-MAC
/// merge ([`station_with_registry_mac`]) and the MAC write-back ([`mac_to_remember`]) only
/// ever touch the station that answered. The tray side matches by `state.id` the same way.
pub fn place_state(
    slot_id: Option<&StationId>,
    state: StationState,
    known: &[KnownStation],
) -> Option<PlacedState> {
    if slot_id != Some(&state.id) {
        return None;
    }
    let entry = known.iter().find(|k| k.id() == state.id);
    let remember_mac = entry.and_then(|k| mac_to_remember(&state, k).map(|mac| (k.id(), mac)));
    Some(PlacedState {
        state: station_with_registry_mac(state, entry),
        remember_mac,
    })
}

/// One Stations pane row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationRowView {
    pub title: String,
    /// State, mesh, pairing and the served model, in one line.
    pub subtitle: String,
    pub dot: Dot,
}

/// Build a Stations pane row from a station's state.
pub fn station_row(state: &StationState) -> StationRowView {
    let dot = if !state.reachable {
        Dot::Grey
    } else if !state.paired {
        Dot::Amber
    } else if state.contention.is_some() {
        Dot::Red
    } else if state.serving.is_some() {
        Dot::Green
    } else {
        Dot::Amber
    };
    let mut parts: Vec<String> = Vec::new();
    parts.push(match (&state.serving, state.reachable) {
        (_, false) if is_local(state) => format!("offline: {LOCAL_TT_MISSING}"),
        (_, false) if state.mac.is_some() => "offline (can be woken)".to_string(),
        (_, false) => "offline".to_string(),
        (Some(s), true) => format!("serving {}", s.model),
        (None, true) => "idle".to_string(),
    });
    if let Some(m) = &state.mesh {
        parts.push(m.clone());
    }
    if !is_local(state) {
        parts.push(if state.paired { "paired" } else { "not paired" }.to_string());
    }
    if let (true, Some(c)) = (state.reachable, &state.contention) {
        parts.push(format!("last run refused: {c}"));
    }
    StationRowView {
        title: state.name.clone(),
        subtitle: parts.join(" - "),
        dot,
    }
}

/// Shown when the local station cannot run because the official `tt` is missing. The install
/// command is the one the Mac app's first run uses (`macos/scripts/ensure-official-tt.sh`).
pub const LOCAL_TT_MISSING: &str = "the official tt CLI was not found on this machine; install \
                                    it with `uv tool install tenstorrent`";

/// Whether the run sheet's Start button is enabled, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunGate {
    Allowed,
    /// The text shown next to the disabled button.
    Disabled(String),
}

/// The rule for the Start button.
///
/// The local station needs no pairing: it is allowed whenever it is reachable (the official
/// `tt` runs). A box must be reachable and paired. An unreachable local station means `tt` is
/// missing or not the official one, and the message says how to install it.
pub fn run_gate(state: &StationState) -> RunGate {
    if is_local(state) {
        return if state.reachable {
            RunGate::Allowed
        } else {
            RunGate::Disabled(format!("Cannot run: {LOCAL_TT_MISSING}."))
        };
    }
    if !state.reachable {
        RunGate::Disabled(format!("{} is offline.", state.name))
    } else if !state.paired {
        RunGate::Disabled(format!(
            "Pair with {} first (Stations, Add station).",
            state.name
        ))
    } else {
        RunGate::Allowed
    }
}

/// Whether `state` is this machine.
fn is_local(state: &StationState) -> bool {
    state.id.0 == "local"
}

// ---------------------------------------------------------------------------------------------
// Action results
// ---------------------------------------------------------------------------------------------

/// The line shown after the person clicked Stop, Start or Refresh logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionResult {
    Ok(String),
    /// Shown in the error style. The text includes the station's error message unchanged.
    Failed(String),
}

impl ActionResult {
    /// `"<verb> done"` on success. On failure, `"<verb> failed: "` followed by the whole error
    /// chain (`{:#}`), so the holder text of a 409 or "managed by TT-Studio" reaches the person
    /// word for word.
    pub fn from_result<T>(verb: &str, result: &anyhow::Result<T>) -> ActionResult {
        match result {
            Ok(_) => ActionResult::Ok(format!("{verb} done")),
            Err(e) => ActionResult::Failed(format!("{verb} failed: {e:#}")),
        }
    }

    pub fn message(&self) -> &str {
        match self {
            ActionResult::Ok(m) | ActionResult::Failed(m) => m,
        }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, ActionResult::Failed(_))
    }
}

// ---------------------------------------------------------------------------------------------
// Add station
// ---------------------------------------------------------------------------------------------

/// The port a box's agent listens on unless told otherwise (`tt-station-agentd`'s default).
pub const DEFAULT_AGENT_PORT: u16 = 8765;

/// Split what the person typed in the Add station dialog into a host and a port.
///
/// Accepts `host`, `host:port`, `[v6]:port`, a bare IPv6 literal, and any of these after
/// `http://`. A missing port is [`DEFAULT_AGENT_PORT`]. The host is returned bare (no
/// brackets), the form `Registry::add_manual` stores; that function does the strict host check.
pub fn parse_address(text: &str) -> Result<(String, u16), String> {
    let t = text.trim();
    let t = t
        .strip_prefix("http://")
        .or_else(|| t.strip_prefix("https://"))
        .unwrap_or(t)
        .trim_end_matches('/');
    if t.is_empty() {
        return Err("enter the box's address, for example qb2-lab.local".into());
    }
    let port_of = |p: &str| match p.parse::<u16>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(format!("{p:?} is not a port (1 to 65535)")),
    };
    // `[v6]` or `[v6]:port`.
    if let Some(rest) = t.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| "an IPv6 address in brackets needs the closing ]".to_string())?;
        let port = match after.strip_prefix(':') {
            Some(p) => port_of(p)?,
            None if after.is_empty() => DEFAULT_AGENT_PORT,
            None => return Err(format!("unexpected text after the address: {after:?}")),
        };
        return Ok((host.to_string(), port));
    }
    // More than one ':' and no brackets: a bare IPv6 literal with no port.
    if t.matches(':').count() > 1 {
        return Ok((t.to_string(), DEFAULT_AGENT_PORT));
    }
    match t.split_once(':') {
        Some((host, p)) => Ok((host.to_string(), port_of(p)?)),
        None => Ok((t.to_string(), DEFAULT_AGENT_PORT)),
    }
}

/// Where the tray keeps its station registry: `$TT_CONFIG_DIR/tray-stations.json` when that is
/// set (the same override the CLI's token store honours), else
/// `$XDG_CONFIG_HOME/tt-station/tray-stations.json`, else `~/.config/tt-station/...`.
pub fn registry_path_from(
    tt_config_dir: Option<&str>,
    xdg_config_home: Option<&str>,
    home: Option<&str>,
) -> std::path::PathBuf {
    use std::path::PathBuf;
    let set = |v: Option<&str>| v.filter(|s| !s.is_empty()).map(PathBuf::from);
    let dir = set(tt_config_dir)
        .or_else(|| set(xdg_config_home).map(|x| x.join("tt-station")))
        .unwrap_or_else(|| {
            set(home)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("tt-station")
        });
    dir.join("tray-stations.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::station::{ServingNow, StationId};
    use libttstation::run_options::{run_schema, serve_argv, serve_argv_for, ServeTarget, VllmArg};
    use serde_json::json;

    fn row(name: &str, kind: Option<&str>, source: &str, cached: bool, fits: bool) -> ModelRow {
        ModelRow {
            name: name.to_string(),
            hf_repo: None,
            kind: kind.map(str::to_string),
            source: source.to_string(),
            hardware: vec!["p300x2".to_string()],
            cached,
            fits,
        }
    }

    // --- Reference performance on the Models page -----------------------------------------

    const PERF_HF: &str = include_str!("../../tests/fixtures/perf_reference_hf.json");

    fn catalog(name: &str, hf: Option<&str>) -> ModelRow {
        ModelRow {
            hf_repo: hf.map(str::to_string),
            ..row(name, Some("llm"), "tt-inference-server", false, true)
        }
    }

    #[test]
    fn the_selected_models_detail_carries_its_reference_block() {
        let table = perf::parse(PERF_HF, Some("0.22.0")).unwrap();
        let ctx = PerfContext {
            table: &table,
            mesh: "p300x2",
        };
        let r = catalog("gemma-4-31B-it", Some("google/gemma-4-31B-it"));
        let text = model_detail(&r, "Runs on this station", Some(ctx));
        assert!(
            text.starts_with("gemma-4-31B-it\n\nGroup: Runs on this station\n"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "\n\nReference performance on p300x2 (tt-inference-server 0.22.0 targets, not \
                 measured on this machine)\n\
                 1 user, 128 in / 128 out: 46 ms to first token, 37 tok/s per user (theoretical)\n\
                 32 users, 128 in / 128 out: 400 tok/s total (theoretical)\n\
                 \"theoretical\" is the ceiling the file sets for this hardware."
            ),
            "{text}"
        );
        assert_eq!(
            model_line(&r, Some(ctx)),
            "    gemma-4-31B-it  ·  llm  ·  tt-inference-server  ·  target 37 tok/s/user"
        );
        // The row's hf_repo reaches the lookup: this name alone matches nothing in the file.
        let by_repo = catalog("Qwen 3 32B (display name)", Some("Qwen/Qwen3-32B"));
        assert!(model_line(&by_repo, Some(ctx)).ends_with("  ·  target 37 tok/s/user"));
    }

    #[test]
    fn a_model_without_numbers_says_so_and_gets_no_suffix() {
        let table = perf::parse(PERF_HF, Some("0.22.0")).unwrap();
        let ctx = PerfContext {
            table: &table,
            mesh: "p300x2",
        };
        let r = catalog(
            "Llama-3.1-8B-Instruct",
            Some("meta-llama/Llama-3.1-8B-Instruct"),
        );
        assert!(model_detail(&r, "g", Some(ctx))
            .ends_with("\n\nNo reference numbers for p300x2 in tt-inference-server 0.22.0."));
        assert_eq!(
            model_line(&r, Some(ctx)),
            "    Llama-3.1-8B-Instruct  ·  llm  ·  tt-inference-server"
        );
    }

    #[test]
    fn without_a_perf_context_nothing_about_performance_is_shown() {
        let r = catalog("gemma-4-31B-it", Some("google/gemma-4-31B-it"));
        let text = model_detail(&r, "g", None);
        assert!(!text.contains("eference"), "{text}");
        assert!(!model_line(&r, None).contains("tok/s"));
    }

    #[test]
    fn only_catalog_rows_get_reference_numbers() {
        let table = perf::parse(PERF_HF, Some("0.22.0")).unwrap();
        let ctx = PerfContext {
            table: &table,
            mesh: "p300x2",
        };
        // A Hub bundle whose repo id equals a released model's key still gets nothing.
        let bundle = ModelRow {
            hf_repo: Some("Qwen/Qwen3-32B".into()),
            ..row("Qwen/Qwen3-32B", None, "HuggingFace", false, true)
        };
        assert_eq!(model_perf(&bundle, Some(ctx)), None);
        assert!(!model_line(&bundle, Some(ctx)).contains("tok/s"));
        let catalog_row = catalog("Qwen3-32B", Some("Qwen/Qwen3-32B"));
        assert!(model_perf(&catalog_row, Some(ctx)).is_some());
    }

    /// The page shows rows from `models()` alone first, then attaches the table when it
    /// arrives and redraws. Both renderings come from the same functions the GTK code binds.
    #[test]
    fn rows_render_before_perf_and_gain_the_suffix_when_it_arrives() {
        let r = catalog("gemma-4-31B-it", Some("google/gemma-4-31B-it"));
        assert_eq!(
            model_line(&r, None),
            "    gemma-4-31B-it  ·  llm  ·  tt-inference-server"
        );
        assert_eq!(models_status(16, 107, false), "16 of 107 models shown");
        let table = perf::parse(PERF_HF, Some("0.22.0")).unwrap();
        let ctx = PerfContext {
            table: &table,
            mesh: "p300x2",
        };
        assert_eq!(
            model_line(&r, Some(ctx)),
            "    gemma-4-31B-it  ·  llm  ·  tt-inference-server  ·  target 37 tok/s/user"
        );
        assert_eq!(
            models_status(16, 107, true),
            "16 of 107 models shown. Reference numbers are tt-inference-server targets, not \
             measurements from this machine."
        );
    }

    #[test]
    fn the_selected_model_stays_selected_after_a_regroup() {
        let names = [None, Some("a"), Some("gemma-4-31B-it"), None, Some("b")];
        assert_eq!(reselect(&names, Some("gemma-4-31B-it")), Some(2));
        assert_eq!(reselect(&names, Some("gone")), None);
        assert_eq!(reselect(&names, None), None);
    }

    #[test]
    fn the_sample_model_is_the_first_catalog_row() {
        let rows = vec![
            row("a/bundle", None, "HuggingFace", false, true),
            row("Cat-1", None, "tt-inference-server", false, true),
            row("Cat-2", None, "tt-inference-server", false, true),
        ];
        assert_eq!(perf_sample_model(&rows), Some("Cat-1"));
        assert_eq!(perf_sample_model(&rows[..1]), None);
    }

    /// 60 rows: names `Model-00` .. `Model-59`, cycling kind, source, cached and fits so that
    /// every combination appears several times.
    fn sixty() -> Vec<ModelRow> {
        let kinds = [Some("llm"), Some("embedding"), None];
        let sources = ["tt-inference-server", "HuggingFace", "local", "box"];
        (0..60)
            .map(|i| {
                row(
                    &format!("Model-{i:02}"),
                    kinds[i % 3],
                    sources[i % 4],
                    i % 2 == 0,
                    i % 5 != 0,
                )
            })
            .collect()
    }

    fn all(g: &ModelGroups) -> Vec<&ModelRow> {
        g.runs_here
            .iter()
            .chain(&g.experimental)
            .chain(&g.other_hardware)
            .chain(&g.unknown_fit)
            .collect()
    }

    #[test]
    fn search_matches_name_case_insensitively_and_across_dozens_of_rows() {
        let rows = sixty();
        let f = ModelFilter {
            query: "mODEL-4".into(),
            ..Default::default()
        };
        let g = group_models(&rows, &f, true);
        let mut names: Vec<_> = all(&g).iter().map(|r| r.name.clone()).collect();
        names.sort();
        let want: Vec<String> = (40..50).map(|i| format!("Model-{i}")).collect();
        assert_eq!(names, want);
        // Rows deep in the list are found too.
        let f = ModelFilter {
            query: "model-59".into(),
            ..Default::default()
        };
        assert_eq!(all(&group_models(&rows, &f, true)).len(), 1);
        let f = ModelFilter {
            query: "no-such".into(),
            ..Default::default()
        };
        assert!(all(&group_models(&rows, &f, true)).is_empty());
    }

    #[test]
    fn filters_combine_with_and() {
        let rows = sixty();
        let f = ModelFilter {
            query: String::new(),
            kind: Some("LLM".into()),
            source: Some("huggingface".into()),
            cached_only: true,
        };
        let got = group_models(&rows, &f, true);
        let got = all(&got);
        // i % 3 == 0 (llm), i % 4 == 1 (HuggingFace), i % 2 == 0 (cached): the last two
        // contradict, so nothing matches.
        assert!(got.is_empty(), "{got:?}");

        let f = ModelFilter {
            kind: Some("llm".into()),
            source: Some("tt-inference-server".into()),
            cached_only: true,
            ..Default::default()
        };
        let g = group_models(&rows, &f, true);
        let mut got: Vec<_> = all(&g).iter().map(|r| r.name.clone()).collect();
        got.sort();
        // i % 3 == 0 and i % 4 == 0 and even: i % 12 == 0.
        let want: Vec<String> = (0..60)
            .step_by(12)
            .map(|i| format!("Model-{i:02}"))
            .collect();
        assert_eq!(got, want);
        for r in all(&g) {
            assert!(r.cached && r.kind.as_deref() == Some("llm"));
        }

        // A set kind filter never matches a row with no kind.
        let f = ModelFilter {
            kind: Some("".into()),
            ..Default::default()
        };
        assert!(all(&group_models(&rows, &f, true))
            .iter()
            .all(|r| r.kind.is_some()));
    }

    #[test]
    fn groups_follow_fits_then_source() {
        let rows = vec![
            row("released-fits", None, "tt-inference-server", false, true),
            row("catalog-fits", None, "tt catalog", false, true),
            row("box-fits", None, "box", false, true),
            row("hub-fits", None, "HuggingFace", false, true),
            row("local-fits", None, "local", false, true),
            row("released-no", None, "tt-inference-server", false, false),
            row("hub-no", None, "HuggingFace", false, false),
        ];
        let g = group_models(&rows, &ModelFilter::default(), true);
        let names = |v: &[ModelRow]| v.iter().map(|r| r.name.clone()).collect::<Vec<_>>();
        assert_eq!(
            names(&g.runs_here),
            ["released-fits", "catalog-fits", "box-fits"]
        );
        assert_eq!(names(&g.experimental), ["hub-fits", "local-fits"]);
        assert_eq!(names(&g.other_hardware), ["released-no", "hub-no"]);
        assert!(g.unknown_fit.is_empty());
    }

    #[test]
    fn unknown_mesh_puts_every_row_in_unknown_fit() {
        let rows = sixty();
        let g = group_models(&rows, &ModelFilter::default(), false);
        assert_eq!(g.unknown_fit.len(), 60);
        assert!(g.runs_here.is_empty());
        assert!(g.experimental.is_empty());
        assert!(
            g.other_hardware.is_empty(),
            "unknown mesh must not grey out every row"
        );
        // Filters still apply.
        let f = ModelFilter {
            cached_only: true,
            ..Default::default()
        };
        assert_eq!(group_models(&rows, &f, false).unknown_fit.len(), 30);
    }

    #[test]
    fn empty_filter_returns_everything_once() {
        let rows = sixty();
        let g = group_models(&rows, &ModelFilter::default(), true);
        let mut names: Vec<_> = all(&g).iter().map(|r| r.name.clone()).collect();
        assert_eq!(names.len(), 60);
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 60, "no row appears in two groups");
        assert!(!g.runs_here.is_empty() && !g.experimental.is_empty());
        assert!(!g.other_hardware.is_empty());
    }

    #[test]
    fn filter_choices_are_distinct_and_sorted() {
        let (kinds, sources) = filter_choices(&sixty());
        assert_eq!(kinds, ["embedding", "llm"]);
        assert_eq!(
            sources,
            ["HuggingFace", "box", "local", "tt-inference-server"]
        );
    }

    #[test]
    fn run_form_defaults_come_from_schema_and_last_run_overrides_them() {
        let schema = run_schema();
        let f = RunForm::from_schema(&schema, None);
        assert_eq!(f.get("workflow"), "server");
        assert_eq!(f.get("offline"), "false");
        assert_eq!(f.get("port"), "");
        assert_eq!(f.specs().len(), schema.options.len());
        assert!(f.to_options().unwrap().is_empty(), "defaults send nothing");

        let last = RunOptions {
            port: Some(20001),
            offline: true,
            device: Some("p300x2".into()),
            vllm_args: vec![
                VllmArg {
                    flag: "max-model-len".into(),
                    value: Some("4096".into()),
                },
                VllmArg {
                    flag: "enable-auto-tool-choice".into(),
                    value: None,
                },
            ],
            ..Default::default()
        };
        let f = RunForm::from_schema(&schema, Some(&last));
        assert_eq!(f.get("port"), "20001");
        assert_eq!(f.get("offline"), "true");
        assert_eq!(f.get("device"), "p300x2");
        assert_eq!(f.get("vllm:max-model-len"), "4096");
        assert_eq!(f.get("vllm:enable-auto-tool-choice"), "true");
        assert_eq!(f.get("workflow"), "server", "unset in last: default kept");
        // Round trip: the form gives back what it was seeded with.
        assert_eq!(f.to_options().unwrap(), last);
    }

    #[test]
    fn run_form_set_ignores_unknown_fields() {
        let mut f = RunForm::from_schema(&run_schema(), None);
        f.set("privileged", "true");
        assert_eq!(f.get("privileged"), "");
        f.set("port", "20002");
        assert_eq!(f.to_options().unwrap().port, Some(20002));
    }

    #[test]
    fn run_form_rejects_a_bad_value_with_its_field_name() {
        let schema = run_schema();
        let cases = [
            ("port", "80"),
            ("port", "abc"),
            ("device", "P300 X2"),
            ("profile", "--privileged"),
            ("workflow", "shell"),
            ("vllm:max-model-len", "4096 --x"),
            ("vllm:tool-call-parser", "$(id)"),
        ];
        for (field, bad) in cases {
            let mut f = RunForm::from_schema(&schema, None);
            f.set(field, bad);
            let e = f.to_options().expect_err(&format!("{field}={bad:?}"));
            assert_eq!(e.field, field, "{field}={bad:?}: {e}");
        }
    }

    #[test]
    fn command_preview_matches_serve_argv() {
        let schema = run_schema();
        let model = "Llama-3.1-8B-Instruct";

        // Defaults, no plan yet: catalog, default port.
        let f = RunForm::from_schema(&schema, None);
        let opts = f.to_options().unwrap();
        let mut want = vec!["tt".to_string()];
        want.extend(serve_argv(model, DEFAULT_SERVE_PORT, &opts));
        assert_eq!(f.command_preview(model), want.join(" "));
        assert_eq!(
            f.command_preview(model),
            "tt serve Llama-3.1-8B-Instruct --port 20000"
        );

        // A set port and offline, catalog plan.
        let mut f = RunForm::from_schema(&schema, None)
            .with_target(PreviewTarget::local("/home/u/.local/bin/tt"));
        f.set("port", "20005");
        f.set("offline", "true");
        f.set_plan(Some(json!({"served_through": "run.py"})));
        let opts = f.to_options().unwrap();
        let mut want = vec!["/home/u/.local/bin/tt".to_string()];
        want.extend(serve_argv(model, DEFAULT_SERVE_PORT, &opts));
        assert_eq!(f.command_preview(model), want.join(" "));

        // A bundle plan: same function LocalStation::run uses, so --detach appears.
        let bundle = "acme/tiny-bundle";
        let mut f = RunForm::from_schema(&schema, None);
        f.set("vllm:max-model-len", "4096");
        f.set_plan(Some(json!({"backend": "tt-model"})));
        let opts = f.to_options().unwrap();
        let mut want = vec!["tt".to_string()];
        want.extend(
            serve_argv_for(ServeTarget::Bundle, bundle, DEFAULT_SERVE_PORT, &opts).unwrap(),
        );
        assert_eq!(f.command_preview(bundle), want.join(" "));
        assert!(f.command_preview(bundle).contains("--detach"));
    }

    #[test]
    fn command_preview_shows_errors_instead_of_a_command() {
        let mut f = RunForm::from_schema(&run_schema(), None);
        f.set("port", "80");
        let p = f.command_preview("Llama-3.1-8B-Instruct");
        assert!(p.contains("port"), "{p}");
        assert!(!p.contains("serve"), "{p}");
        // vLLM flags on a catalog model are refused by serve_argv_for, as LocalStation::run does.
        let mut f = RunForm::from_schema(&run_schema(), None);
        f.set("vllm:max-model-len", "4096");
        let p = f.command_preview("Llama-3.1-8B-Instruct");
        assert!(p.contains("vllm_args"), "{p}");
        // A bad model name is refused too.
        let f = RunForm::from_schema(&run_schema(), None);
        assert!(f.command_preview("--port=1").contains("model"));
    }

    #[test]
    fn remote_preview_is_labelled_options_only() {
        let mut f = RunForm::from_schema(&run_schema(), None).with_target(PreviewTarget::Remote);
        f.set("port", "20003");
        assert_eq!(
            f.command_preview("Llama-3.1-8B-Instruct"),
            "Options sent to the box: --port 20003"
        );
        let f = RunForm::from_schema(&run_schema(), None).with_target(PreviewTarget::Remote);
        assert_eq!(
            f.command_preview("Llama-3.1-8B-Instruct"),
            "Options sent to the box: (none, the box's defaults apply)"
        );
    }

    fn state(id: &str, reachable: bool, paired: bool, mac: Option<&str>) -> StationState {
        StationState {
            id: StationId(id.into()),
            name: id.into(),
            reachable,
            mesh: Some("p300x2".into()),
            serving: None,
            paired,
            contention: None,
            mac: mac.map(str::to_string),
        }
    }

    fn known(mac: Option<&str>) -> KnownStation {
        KnownStation {
            name: "qb".into(),
            host: "qb".into(),
            port: 8765,
            mac: mac.map(str::to_string),
        }
    }

    #[test]
    fn offline_station_gets_the_registry_mac_and_reachable_keeps_its_own() {
        let k = known(Some("aa:bb:cc:dd:ee:ff"));
        let off = station_with_registry_mac(state("qb:8765", false, true, None), Some(&k));
        assert_eq!(off.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        let on = station_with_registry_mac(
            state("qb:8765", true, true, Some("11:22:33:44:55:66")),
            Some(&k),
        );
        assert_eq!(on.mac.as_deref(), Some("11:22:33:44:55:66"));
        let none = station_with_registry_mac(state("qb:8765", false, true, None), None);
        assert_eq!(none.mac, None);
        // The merged state makes the tray offer Wake.
        let menu = crate::menu_model::build_menu(&[off], &|_| None);
        assert!(menu.iter().any(|i| matches!(
            i,
            crate::menu_model::MenuItem::Action {
                action: crate::menu_model::MenuAction::Wake(_),
                ..
            }
        )));
    }

    #[test]
    fn a_new_or_changed_mac_is_remembered() {
        let s = state("qb:8765", true, true, Some("11:22:33:44:55:66"));
        assert_eq!(
            mac_to_remember(&s, &known(None)).as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(
            mac_to_remember(&s, &known(Some("aa:aa:aa:aa:aa:aa"))).as_deref(),
            Some("11:22:33:44:55:66")
        );
        assert_eq!(mac_to_remember(&s, &known(Some("11:22:33:44:55:66"))), None);
        assert_eq!(
            mac_to_remember(&state("qb:8765", false, true, None), &known(Some("x"))),
            None
        );
    }

    fn known_at(host: &str, mac: Option<&str>) -> KnownStation {
        KnownStation {
            name: host.into(),
            host: host.into(),
            port: 8765,
            mac: mac.map(str::to_string),
        }
    }

    /// A poll answer for a station that is no longer in that slot (the list was rebuilt by
    /// Add or Remove while the poll ran) is dropped.
    #[test]
    fn a_stale_state_for_a_removed_or_shifted_station_is_dropped() {
        let reg = [known_at("b", None)];
        let a = state("a:8765", true, true, Some("aa:aa:aa:aa:aa:aa"));
        // Slot 1 held box A when the poll started; A was removed, so B is there now.
        let b_id = StationId("b:8765".into());
        assert_eq!(place_state(Some(&b_id), a.clone(), &reg), None);
        // The list shrank: slot 1 no longer exists.
        assert_eq!(place_state(None, a, &reg), None);
    }

    /// The MAC merge and the MAC to save are both found by id, never by position, so one
    /// box's MAC is never written onto another.
    #[test]
    fn the_mac_merge_never_crosses_stations() {
        // Registry order differs from the slot: A is entry 1, B is entry 0.
        let reg = [
            known_at("b", Some("bb:bb:bb:bb:bb:bb")),
            known_at("a", None),
        ];
        let a_id = StationId("a:8765".into());
        let up = place_state(
            Some(&a_id),
            state("a:8765", true, true, Some("aa:aa:aa:aa:aa:aa")),
            &reg,
        )
        .expect("the slot still holds A");
        assert_eq!(
            up.remember_mac,
            Some((a_id.clone(), "aa:aa:aa:aa:aa:aa".to_string()))
        );
        assert_eq!(up.state.mac.as_deref(), Some("aa:aa:aa:aa:aa:aa"));

        // Offline A takes no MAC from B's entry.
        let up = place_state(Some(&a_id), state("a:8765", false, true, None), &reg).unwrap();
        assert_eq!(up.state.mac, None);
        assert_eq!(up.remember_mac, None);

        // Offline B gets its own MAC.
        let b_id = StationId("b:8765".into());
        let up = place_state(Some(&b_id), state("b:8765", false, true, None), &reg).unwrap();
        assert_eq!(up.state.mac.as_deref(), Some("bb:bb:bb:bb:bb:bb"));

        // This machine is not in the registry: kept as is, nothing saved.
        let local = StationId("local".into());
        let up = place_state(Some(&local), state("local", true, true, None), &reg).unwrap();
        assert_eq!(up.remember_mac, None);
        assert_eq!(up.state.id, local);
    }

    #[test]
    fn station_rows_say_state_mesh_pairing_and_model() {
        let mut s = state("qb:8765", true, true, None);
        s.serving = Some(ServingNow {
            model: "Llama".into(),
            base_url: "http://qb:20000/v1".into(),
        });
        let r = station_row(&s);
        assert_eq!(r.dot, Dot::Green);
        assert_eq!(r.subtitle, "serving Llama - p300x2 - paired");
        let r = station_row(&state("qb:8765", true, false, None));
        assert_eq!(r.dot, Dot::Amber);
        assert_eq!(r.subtitle, "idle - p300x2 - not paired");
        let r = station_row(&state("qb:8765", false, true, None));
        assert_eq!(r.dot, Dot::Grey);
        assert!(r.subtitle.starts_with("offline"));
        let mut c = state("qb:8765", true, true, None);
        c.contention = Some("held by someone".into());
        assert_eq!(station_row(&c).dot, Dot::Red);
        let local_off = state("local", false, true, None);
        assert!(station_row(&local_off)
            .subtitle
            .contains("uv tool install tenstorrent"));
    }

    #[test]
    fn local_runs_without_pairing_and_remote_needs_it() {
        // Local: pairing is irrelevant, even if a state somehow says unpaired.
        assert_eq!(
            run_gate(&state("local", true, false, None)),
            RunGate::Allowed
        );
        assert_eq!(
            run_gate(&state("local", true, true, None)),
            RunGate::Allowed
        );
        match run_gate(&state("local", false, true, None)) {
            RunGate::Disabled(m) => assert!(m.contains("uv tool install tenstorrent"), "{m}"),
            g => panic!("{g:?}"),
        }
        // Remote: paired and reachable.
        assert_eq!(
            run_gate(&state("qb:8765", true, true, None)),
            RunGate::Allowed
        );
        match run_gate(&state("qb:8765", true, false, None)) {
            RunGate::Disabled(m) => assert!(m.contains("Pair"), "{m}"),
            g => panic!("{g:?}"),
        }
        assert!(matches!(
            run_gate(&state("qb:8765", false, true, None)),
            RunGate::Disabled(_)
        ));
    }

    #[test]
    fn an_err_becomes_a_visible_message_and_409_text_is_kept_verbatim() {
        let holder = "box refused the request (409): chips are held by \
                      claude:storyreg-s5489 (pid 145240) since 2026-10-05T10:00:00Z";
        let r: anyhow::Result<()> = Err(anyhow::anyhow!(holder.to_string()));
        let a = ActionResult::from_result("Start", &r);
        assert!(a.is_error());
        assert!(a.message().contains(holder), "{}", a.message());
        assert!(a.message().starts_with("Start failed"));

        let studio: anyhow::Result<()> = Err(anyhow::anyhow!(
            "`qwen` is managed by TT-Studio; stop it there"
        ));
        let a = ActionResult::from_result("Stop", &studio);
        assert!(a.message().contains("managed by TT-Studio; stop it there"));

        // Context chains are shown whole.
        let chained: anyhow::Result<()> =
            Err(anyhow::anyhow!("inner reason")).map_err(|e| e.context("outer"));
        let a = ActionResult::from_result("Logs", &chained);
        assert!(a.message().contains("outer") && a.message().contains("inner reason"));

        let ok: anyhow::Result<u8> = Ok(1);
        let a = ActionResult::from_result("Stop", &ok);
        assert!(!a.is_error());
        assert_eq!(a.message(), "Stop done");
    }

    #[test]
    fn addresses_parse_with_and_without_ports() {
        let ok = |t: &str| parse_address(t).unwrap();
        assert_eq!(ok("qb2-lab.local"), ("qb2-lab.local".into(), 8765));
        assert_eq!(ok("  10.0.0.5:9000 "), ("10.0.0.5".into(), 9000));
        assert_eq!(ok("http://qb:8766/"), ("qb".into(), 8766));
        assert_eq!(ok("[::1]:8770"), ("::1".into(), 8770));
        assert_eq!(ok("[fe80::1]"), ("fe80::1".into(), 8765));
        assert_eq!(ok("fe80::1"), ("fe80::1".into(), 8765), "bare v6 literal");
        assert!(parse_address("").is_err());
        assert!(parse_address("qb:0").is_err());
        assert!(parse_address("qb:99999").is_err());
        assert!(parse_address("qb:abc").is_err());
        assert!(parse_address("[::1").is_err());
    }

    #[test]
    fn registry_path_prefers_tt_config_dir_then_xdg_then_home() {
        use std::path::PathBuf;
        assert_eq!(
            registry_path_from(Some("/c"), Some("/x"), Some("/h")),
            PathBuf::from("/c/tray-stations.json")
        );
        assert_eq!(
            registry_path_from(None, Some("/x"), Some("/h")),
            PathBuf::from("/x/tt-station/tray-stations.json")
        );
        assert_eq!(
            registry_path_from(Some(""), Some(""), Some("/h")),
            PathBuf::from("/h/.config/tt-station/tray-stations.json"),
            "empty variables count as unset"
        );
    }

    #[test]
    fn a_model_list_for_another_station_or_an_older_load_is_dropped() {
        assert!(
            load_is_current(Some(0), 0, 0),
            "the latest load, still selected"
        );
        assert!(
            !load_is_current(Some(0), 1, 0),
            "the person switched to a box meanwhile"
        );
        assert!(
            !load_is_current(Some(1), 1, 0),
            "a newer load for station 1 replaced it"
        );
        assert!(
            !load_is_current(Some(1), 0, 0),
            "switched away and back, but a newer load ran"
        );
        assert!(!load_is_current(None, 0, 0), "no load is recorded");
    }

    /// The page's load bookkeeping as a tiny state machine, to replay event sequences.
    struct LoadSim {
        loaded_for: Option<usize>,
        selected: usize,
    }

    impl LoadSim {
        fn start_load(&mut self) -> usize {
            self.loaded_for = Some(self.selected);
            self.selected
        }
        /// A load for `i` returns. Returns whether its rows were shown.
        fn result_arrives(&mut self, i: usize) -> bool {
            if load_is_current(self.loaded_for, self.selected, i) {
                true
            } else {
                self.loaded_for = on_load_dropped(self.loaded_for, i);
                false
            }
        }
    }

    #[test]
    fn a_dropped_load_does_not_leave_its_station_marked_loaded() {
        // Start a load for station 0, select station 1 before it returns (no load started for
        // 1), the late result for 0 is dropped, then select 0 again: it must reload.
        let mut sim = LoadSim {
            loaded_for: None,
            selected: 0,
        };
        let first = sim.start_load();
        sim.selected = 1;
        assert!(!sim.result_arrives(first), "late result for 0 is dropped");
        assert_eq!(
            sim.loaded_for, None,
            "the marker for the dropped load is cleared"
        );
        sim.selected = 0;
        assert!(
            needs_reload(sim.loaded_for, sim.selected),
            "station 0 reloads"
        );
        let second = sim.start_load();
        assert!(
            sim.result_arrives(second),
            "and this time its rows are shown"
        );
        assert!(!needs_reload(sim.loaded_for, sim.selected));

        // A dropped older load leaves a newer load's marker alone.
        assert_eq!(on_load_dropped(Some(1), 0), Some(1));
        assert_eq!(on_load_dropped(Some(0), 0), None);
        assert_eq!(on_load_dropped(None, 0), None);
    }
}
