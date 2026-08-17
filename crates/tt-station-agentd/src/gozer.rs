//! Client for `gozer`, the CLI that arbitrates which Tenstorrent chips
//! belong to which tenant when several agents share one box -- see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` for the
//! full design; this module is Task 1's foundation.
//!
//! **Scope of this module today:** capability probing (`probe`) and
//! grant/outcome parsing (`Outcome::from_captured`). No leasing behavior is
//! wired into any serving backend yet -- lease-before-launch, the release
//! guard, and the startup reconciliation sweep are separate follow-up work.
//!
//! **gozer is OPTIONAL.** Its absence is a normal outcome (logged once, at
//! info, via `eprintln!` -- this crate doesn't pull in a logging framework
//! elsewhere, see `main.rs`'s `detect_startup_device_mesh` for the same
//! convention), never an error. A box without gozer installed serves
//! exactly as it always has: whole-box, no leasing.
//!
//! **This module never reads or writes gozer's state files** (`/tmp/tt-gozer`)
//! directly -- it only shells out to the `gozer` CLI through the injected
//! `CommandRunner` seam `serving/docker.rs` already defines (`run_capturing`).
//! A second, from-scratch implementation of gozer's lock protocol would have
//! to stay bug-compatible with one that took fifty commits and two Critical
//! concurrency bugs to stabilise (see the design doc's "Transport: shell out
//! to the CLI" section) -- not worth it just to avoid a subprocess call.
//!
//! **`--owner-pid` matters.** gozer proves a chip is in use by reading
//! `/proc/<pid>/fd`, readable only by the owning user; tt-station's serving
//! containers run as root, so their fds are invisible to gozer running as
//! the box's normal user. A lease acquired without a supervising pid is
//! *detached* and gozer reaps it ~900s after it stops seeing an fd -- for a
//! root-owned container, that's immediately and forever. Any future caller
//! that builds a `gozer acquire` invocation through this module's
//! `CommandRunner` seam MUST pass `--owner-pid <std::process::id()>` (this
//! agent's own pid), or the lease silently evaporates under a healthy
//! session. This module doesn't build that call yet (see the scope note
//! above), but whichever task does must not omit it.

use serde::Deserialize;

use crate::serving::docker::{CapturedOutput, CommandRunner};

/// gozer exit codes this module gives specific meaning to (see the design
/// doc's "Failure modes" table for the full contract). Codes not named here
/// (13 no such lease, 14 topology unreadable, 15 release refused, 16 mutex
/// stuck, 130 interrupted) fall through to `Outcome::Failed` in
/// `Outcome::from_captured` until a later task gives them dedicated
/// handling.
const EXIT_QUEUED: i32 = 10;
const EXIT_UNAVAILABLE: i32 = 12;

/// A working `gozer` binary this agent found, cached from a single startup
/// probe (see `probe`) rather than re-probed per call -- gozer's
/// presence/absence doesn't change while agentd is running, and a fresh
/// subprocess spawn on every leasing call would be pure overhead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    /// Resolved path to the `gozer` binary this agent will invoke for every
    /// later call -- either the explicitly configured path, or the absolute
    /// path found on `$PATH` (see `probe`'s resolution order). Never just
    /// the bare string `"gozer"` when found via `$PATH` search.
    pub path: String,
    /// Version string exactly as `gozer --version` printed it (e.g.
    /// `"gozer 0.1.0"`), unparsed. Nothing here does semver comparison
    /// today; a caller that needs one can parse this itself.
    pub version: String,
}

/// A successful `gozer acquire`: which chips were granted and how to target
/// them. Field names and shapes mirror gozer's own `--json` payload
/// (`gozer/cli.py`'s `cmd_acquire`) directly -- `lease_id`, `chips` (BDFs),
/// `dev_indices`, `units`, `expanded`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Grant {
    pub lease_id: String,
    /// PCI BDFs, gozer's own chip identifiers (e.g. `"0000:01:00.0"`).
    pub chips: Vec<String>,
    /// UMD-style logical device indices. NOT necessarily the same namespace
    /// as `/dev/tenstorrent/<n>` -- the design doc flags that mapping as an
    /// open question yet to be verified against real hardware, so this
    /// field is stored exactly as gozer reports it, uninterpreted. See the
    /// design doc's "Which backend, and what a grant actually pins" section
    /// for how a later task is expected to apply this per serving backend.
    pub dev_indices: Vec<u32>,
    /// Serving-backend-facing unit labels (e.g. board names), when gozer's
    /// response includes them. Defaults to empty rather than failing to
    /// parse when a given gozer version's payload omits it.
    #[serde(default)]
    pub units: Vec<String>,
    /// Whether gozer expanded a partial/board-level request to a full
    /// board (gozer's own term for this). Defaults to `false` when absent.
    #[serde(default)]
    pub expanded: bool,
}

/// Best-effort holder/since-when detail attached to an `Unavailable`
/// outcome, when the JSON payload on stdout happens to carry it. Parsed
/// leniently: missing keys or a payload that isn't even valid JSON both
/// degrade to `None`/`None` rather than turning an already-non-zero exit
/// into a `Failed` -- "unavailable, and I don't know who holds it" is still
/// a perfectly good `Unavailable`, just a less informative one.
#[derive(Debug, Default, Deserialize)]
struct UnavailableInfo {
    #[serde(default)]
    holder: Option<String>,
    #[serde(default)]
    since: Option<String>,
}

/// What a gozer verb (`acquire`, and later `release`/`run`/`reconcile`)
/// reported, parsed from its exit code and (for exit 0) its `--json` stdout
/// payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Exit 0 with a payload that parsed cleanly as a `Grant`.
    Granted(Grant),
    /// Exit 10 (queued) or 12 (unavailable) -- this integration doesn't
    /// queue (see the design doc's "When chips are unavailable" section:
    /// agentd cancels any ticket and reports a clear error rather than
    /// waiting), so both codes collapse to the same caller-facing outcome:
    /// no chips, plus whatever holder/since-when detail was reported.
    Unavailable {
        holder: Option<String>,
        since: Option<String>,
    },
    /// Anything else: a non-zero exit this module doesn't have specific
    /// handling for yet (13/14/15/16/130 -- see the design doc's "Failure
    /// modes" table for how a later task is expected to react to each), or
    /// exit 0 with a payload that didn't parse as a `Grant`. Carries a
    /// message good enough to log or surface to a caller. Never a panic --
    /// gozer misbehaving must degrade to a reportable error.
    Failed(String),
}

impl Outcome {
    /// Turn a `run_capturing` result for a gozer verb into an `Outcome`,
    /// per the exit-code contract in the design doc's "Failure modes"
    /// table: `0` → parse a `Grant`; `10`/`12` → `Unavailable`; anything
    /// else → `Failed`.
    pub fn from_captured(captured: &CapturedOutput) -> Outcome {
        match captured.code {
            0 => match serde_json::from_str::<Grant>(&captured.stdout) {
                Ok(grant) => Outcome::Granted(grant),
                Err(err) => Outcome::Failed(format!(
                    "gozer exited 0 but its --json payload didn't parse as a grant: {err} \
                     (stdout: {:?})",
                    captured.stdout
                )),
            },
            EXIT_QUEUED | EXIT_UNAVAILABLE => {
                // Lenient: absent keys or outright malformed JSON both fall
                // back to the empty default rather than becoming `Failed`
                // -- see `UnavailableInfo`'s doc comment.
                let info: UnavailableInfo =
                    serde_json::from_str(&captured.stdout).unwrap_or_default();
                Outcome::Unavailable {
                    holder: info.holder,
                    since: info.since,
                }
            }
            code => {
                let detail = if !captured.stderr.is_empty() {
                    &captured.stderr
                } else {
                    &captured.stdout
                };
                Outcome::Failed(format!("gozer exited {code}: {detail}"))
            }
        }
    }
}

/// Resolve which `gozer` binary to use: the explicit config/CLI path
/// (`path_hint`, already tilde-expanded by `config::expand_tilde` -- see
/// `config.rs`'s `gozer_path`) if given and non-empty, else the first
/// `gozer` found on `$PATH`. Returns the resolved path as a `String`
/// without ever invoking anything -- pure `$PATH`/filesystem lookup, no
/// `CommandRunner` involved, so `probe`'s "absence is normal" contract holds
/// even before a single command is run.
fn resolve_path(path_hint: Option<&str>) -> Option<String> {
    if let Some(hint) = path_hint.filter(|s| !s.is_empty()) {
        return Some(hint.to_string());
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join("gozer"))
        .find(|candidate| candidate.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Probe for a working `gozer` binary: resolve a path (explicit config
/// first, else `gozer` on `$PATH` -- see `resolve_path`), then run
/// `<path> --version` through the injected `runner`. NEVER invokes
/// `acquire`/`release`/`run`/`reconcile` -- see the module doc's "This
/// module never reads or writes gozer's state files" note, which extends to
/// never running gozer's mutating verbs from a probe.
///
/// Returns `Some(Capability)` only on a clean, parseable exit-0 response.
/// Every other case -- no path resolves, the command can't be spawned, a
/// non-zero exit, or empty version output -- returns `None`, because
/// gozer's absence is a normal outcome (see the module doc), never an
/// error. Logs exactly once, at info, either way (found: which path/version;
/// absent: why), via `eprintln!` -- this crate's existing convention (see
/// `main.rs`'s `detect_startup_device_mesh`).
pub fn probe(runner: &dyn CommandRunner, path_hint: Option<&str>) -> Option<Capability> {
    let Some(path) = resolve_path(path_hint) else {
        eprintln!(
            "tt-station-agentd: gozer not found (no configured path, and none on $PATH); \
             leasing unavailable, serving whole-box as before"
        );
        return None;
    };

    match runner.run_capturing(&[path.as_str(), "--version"]) {
        Ok(captured) if captured.code == 0 => {
            let version = captured.stdout.trim().to_string();
            if version.is_empty() {
                eprintln!(
                    "tt-station-agentd: gozer at '{path}' returned no version output on \
                     '--version'; treating gozer as absent"
                );
                return None;
            }
            eprintln!("tt-station-agentd: found {version} at '{path}'");
            Some(Capability { path, version })
        }
        Ok(captured) => {
            eprintln!(
                "tt-station-agentd: '{path} --version' exited {} (non-zero); treating gozer as \
                 absent",
                captured.code
            );
            None
        }
        Err(err) => {
            eprintln!(
                "tt-station-agentd: gozer not found ('{path} --version' failed to run: {err:#}); \
                 leasing unavailable, serving whole-box as before"
            );
            None
        }
    }
}
