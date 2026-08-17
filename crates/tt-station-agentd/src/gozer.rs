//! Client for `gozer`, the CLI that arbitrates which Tenstorrent chips
//! belong to which tenant when several agents share one box -- see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` for the
//! full design; this module is Task 1's foundation.
//!
//! **Scope of this module today:** capability probing (`probe`), the
//! leasing verbs a serving backend needs (`acquire`, `release`,
//! `contention_detail`), grant/outcome parsing (`Outcome::from_captured`),
//! and the release-on-drop `LeaseGuard`. The startup reconciliation sweep
//! is separate follow-up work.
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
//! session. `acquire` below is the one place that builds that call, and it
//! always passes the flag.

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::serving::docker::{CapturedOutput, CommandRunner};

/// gozer exit codes this module gives specific meaning to (see the design
/// doc's "Failure modes" table for the full contract). Codes not named here
/// (14 topology unreadable, 16 mutex stuck, 130 interrupted) fall through to
/// `Outcome::Failed` in `Outcome::from_captured` until a later task gives
/// them dedicated handling.
const EXIT_QUEUED: i32 = 10;
const EXIT_UNAVAILABLE: i32 = 12;
/// "No such lease" -- `release`'s idempotent case, see `release`'s doc.
const EXIT_NO_LEASE: i32 = 13;

/// The `--chips` value every `acquire` from this agent asks for.
///
/// `"1"` is gozer's own default, and on a board-grain box (this one) gozer
/// EXPANDS a one-chip request to the whole board it lives on -- reported
/// back as `Grant::expanded`. So "1" means "one board's worth", which is
/// exactly the unit this integration exists to hand out: two boards, two
/// tenants. Asking for `"all"` instead would hand a single serve the whole
/// box and defeat the point; asking for a literal chip count would need
/// per-board topology knowledge that gozer already owns and agentd
/// deliberately does not reimplement (see the module doc).
pub const DEFAULT_LEASE_CHIPS: &str = "1";

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

impl Grant {
    /// The single argv value for a lease-scoped `tt-smi -r`: this grant's
    /// BDFs, comma-joined -- `tt-smi -r 0000:01:00.0,0000:02:00.0`. That
    /// form is per-ASIC and is exactly what gozer's own `reset.py` builds.
    ///
    /// **Every chip is checked to be a PCI BDF first, and a grant that
    /// isn't fails rather than resetting anything.** This mirrors the guard
    /// in gozer's `reset.py` ("gozer resets by BDF only, never by index")
    /// and it is not paranoia: `tt-smi -r` accepts a bare integer too and
    /// reads it as a UMD *logical id* -- a different namespace -- so a
    /// malformed grant that slipped through would reset a device nobody
    /// leased, which is the one outcome this whole integration exists to
    /// prevent. Refusing to reset is always the safer failure.
    ///
    /// An empty grant is also refused: `tt-smi -r` with no target is a
    /// WHOLE-BOX reset, so silently degrading to it would be the worst
    /// possible interpretation of "reset only what I leased".
    pub fn reset_target(&self) -> Result<String> {
        if self.chips.is_empty() {
            return Err(anyhow::anyhow!(
                "lease '{}' granted no chips; refusing to build a reset command, because \
                 `tt-smi -r` with no target resets the WHOLE BOX",
                self.lease_id
            ));
        }
        for chip in &self.chips {
            if !is_pci_bdf(chip) {
                return Err(anyhow::anyhow!(
                    "lease '{}' names chip {chip:?}, which is not a PCI BDF -- refusing to \
                     reset by anything else, since `tt-smi -r` would read it as a UMD logical \
                     id and could reset a device nobody leased",
                    self.lease_id
                ));
            }
        }
        Ok(self.chips.join(","))
    }
}

/// Whether `s` is a PCI BDF in gozer's canonical form:
/// `DDDD:BB:DD.F` -- 4 hex domain digits, 2 hex bus digits, 2 hex device
/// digits, and a single-digit function, e.g. `0000:01:00.0`. Mirrors the
/// `BDF_RE` guard in gozer's `reset.py`; hand-rolled rather than pulling in
/// a regex dependency for one pattern.
fn is_pci_bdf(s: &str) -> bool {
    let Some((domain, rest)) = s.split_once(':') else {
        return false;
    };
    let Some((bus, rest)) = rest.split_once(':') else {
        return false;
    };
    let Some((device, function)) = rest.split_once('.') else {
        return false;
    };
    let hex = |field: &str, width: usize| {
        field.len() == width && field.chars().all(|c| c.is_ascii_hexdigit())
    };
    hex(domain, 4)
        && hex(bus, 2)
        && hex(device, 2)
        // The function is a single digit 0-7 (PCI allows eight functions).
        && function.len() == 1
        && matches!(function.as_bytes()[0], b'0'..=b'7')
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

/// Take a chip lease: `gozer acquire --chips <chips> --owner-pid <this pid>
/// --who <who> --reason <reason> --no-queue --json`.
///
/// This is the ONLY place in the codebase that builds an `acquire`
/// invocation, so the two flags that carry weight are guaranteed present:
///
/// * `--owner-pid` is this agent's own pid. Without it the lease is
///   *detached*, and gozer reaps a detached lease once it stops seeing an
///   fd -- which for a root-owned serving container is immediately and
///   forever (see the module doc). With it, the lease lives exactly as long
///   as agentd does.
/// * `--no-queue` because this integration does NOT wait in line. gozer
///   refuses *before* taking a ticket, so there is no queue entry left on
///   disk for a caller to have to cancel; the caller reports who holds the
///   chips instead (see `contention_detail`). A queued exit (10) is
///   therefore not expected here, and `Outcome::from_captured` maps it to
///   the same `Unavailable` as 12 regardless.
///
/// Never panics and never propagates a spawn failure as an `Err`: a gozer
/// that can't even be run degrades to `Outcome::Failed`, which the caller
/// surfaces as a serve failure rather than as an accidental unleased serve.
pub fn acquire(
    runner: &dyn CommandRunner,
    capability: &Capability,
    chips: &str,
    who: &str,
    reason: &str,
) -> Outcome {
    let owner_pid = std::process::id().to_string();
    let args = [
        capability.path.as_str(),
        "acquire",
        "--chips",
        chips,
        "--owner-pid",
        owner_pid.as_str(),
        "--who",
        who,
        "--reason",
        reason,
        "--no-queue",
        "--json",
    ];

    match runner.run_capturing(&args) {
        Ok(captured) => Outcome::from_captured(&captured),
        Err(err) => Outcome::Failed(format!(
            "could not run '{} acquire': {err:#}",
            capability.path
        )),
    }
}

/// Give a lease back: `gozer release <lease_id> --json`.
///
/// gozer resets the released chips as part of this (that's why `POST /stop`
/// needs no separate reset verb, and why a caller must stop its container
/// BEFORE releasing). Exit-code handling, per the design doc's "Failure
/// modes" table:
///
/// * `0` -- released. `Ok(())`.
/// * `13` (no such lease) -- the lease is already gone: reaped, or released
///   by an operator by hand. There is nothing left to do and nothing the
///   caller can fix, so this is `Ok(())` with a log line rather than an
///   error that would fail an otherwise-successful `stop`. `stop` is
///   documented as idempotent and this keeps it so.
/// * anything else, `15` (release refused) especially -- surfaced as `Err`,
///   NOT swallowed: the release did not happen, the chips were not reset,
///   and the caller (or the operator reading the log) needs to know.
pub fn release(runner: &dyn CommandRunner, capability: &Capability, lease_id: &str) -> Result<()> {
    let args = [capability.path.as_str(), "release", lease_id, "--json"];
    let captured = runner
        .run_capturing(&args)
        .with_context(|| format!("could not run '{} release {lease_id}'", capability.path))?;

    match captured.code {
        0 => Ok(()),
        EXIT_NO_LEASE => {
            eprintln!(
                "tt-station-agentd: gozer reports no such lease '{lease_id}' (already released \
                 or reaped); treating the release as done"
            );
            Ok(())
        }
        code => {
            // gozer's release payload is `{"released": bool, "message": str}`
            // -- prefer its own message, then stderr, then raw stdout.
            let detail = serde_json::from_str::<ReleaseReport>(&captured.stdout)
                .ok()
                .and_then(|report| report.message)
                .unwrap_or_else(|| {
                    if captured.stderr.is_empty() {
                        captured.stdout.clone()
                    } else {
                        captured.stderr.clone()
                    }
                });
            Err(anyhow::anyhow!(
                "gozer refused to release lease '{lease_id}' (exit {code}): {detail} -- the \
                 chips were NOT released or reset"
            ))
        }
    }
}

/// `gozer release`'s `--json` payload. Only `message` is read; `released` is
/// redundant with the exit code, which is the authoritative signal.
#[derive(Debug, Deserialize)]
struct ReleaseReport {
    #[serde(default)]
    message: Option<String>,
}

/// One chip's record in `gozer status --json`'s `chips` array. Only the
/// fields needed to name a holder are declared -- `serde` ignores the rest
/// (`dev_index`, `card`, `pid`, `pids_holding`, `overstayed`) so a gozer
/// that grows a field doesn't break parsing here.
///
/// Note what is NOT here: a `since`. gozer's per-chip status carries `who`,
/// `reason` and `state` but records no start time, so agentd can name a
/// holder and CANNOT say how long they have held the board. It must not
/// invent one.
#[derive(Debug, Deserialize)]
struct StatusChip {
    #[serde(default)]
    board: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    who: Option<String>,
}

/// `gozer status --json`'s top-level payload.
#[derive(Debug, Default, Deserialize)]
struct StatusReport {
    #[serde(default)]
    chips: Vec<StatusChip>,
}

/// Human-readable "who has the chips" detail for a contention error, built
/// by cross-referencing `gozer status --json`.
///
/// This exists because `acquire`'s own unavailable payload is just
/// `{"granted": false, "queued": false}` -- it names nobody. `status` is the
/// only place the holder appears, and it is safe to call for this: it runs
/// `reconcile(reap=False)` and is documented as read-only, so asking who
/// holds a board never mutates the gate.
///
/// Returns something like `board 0100014311601055 is held by
/// claude:ttm-optimize`, one clause per busy board, deduplicated and in
/// gozer's own chip order. **No duration** -- see `StatusChip`. Best-effort
/// throughout: a `status` that fails to run, exits non-zero, or returns
/// unparseable JSON yields `None`, and the caller reports the contention
/// without a holder rather than failing twice.
pub fn contention_detail(runner: &dyn CommandRunner, capability: &Capability) -> Option<String> {
    let captured = runner
        .run_capturing(&[capability.path.as_str(), "status", "--json"])
        .ok()?;
    if captured.code != 0 {
        return None;
    }
    let report: StatusReport = serde_json::from_str(&captured.stdout).ok()?;

    let mut clauses: Vec<String> = Vec::new();
    for chip in &report.chips {
        // FREE chips aren't why the acquire failed, and a chip with neither
        // a holder nor an untracked-process state says nothing useful.
        let state = chip.state.as_deref().unwrap_or("");
        if state == "FREE" {
            continue;
        }
        let board = chip.board.as_deref().unwrap_or("(unknown board)");
        let clause = match (&chip.who, state) {
            (Some(who), _) => format!("board {board} is held by {who}"),
            // gozer's own term for "a process has the chip open with no
            // lease" -- there is no `who` to report, but saying so is more
            // actionable than silence.
            (None, "BUSY-UNTRACKED") => {
                format!("board {board} is in use by an untracked process (no lease)")
            }
            (None, _) => continue,
        };
        if !clauses.contains(&clause) {
            clauses.push(clause);
        }
    }

    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join("; "))
    }
}

/// A held lease, released on `Drop`.
///
/// **Why a guard rather than release calls.** A lease taken at the top of a
/// serving backend's `start` leaks on three in-flight failures that `stop()`
/// never sees -- a concurrent `/stop` cancelling the bring-up, the container
/// dying during startup, and the health poll timing out -- plus whatever
/// fourth exit path someone adds later. `Drop` runs on early return and on
/// unwind, so it covers all of them, including the one not written yet. Five
/// hand-written release call sites would not.
///
/// **Disarming.** A serve that comes up healthy must KEEP its lease: the
/// chips stay in use until `stop`. `into_lease_id` hands the id out and
/// disarms the guard, transferring ownership to the caller (in practice, to
/// the backend's own `lease` field, released later by `stop`).
///
/// Borrows the `CommandRunner` rather than owning one, so a backend can
/// build a guard from the same injected runner it uses for everything else
/// -- which is also what lets tests observe the release without a real
/// gozer.
pub struct LeaseGuard<'a> {
    runner: &'a dyn CommandRunner,
    capability: Capability,
    grant: Grant,
    /// `true` while this guard still owns the lease. Cleared by
    /// `into_lease_id`, after which `Drop` does nothing.
    armed: bool,
}

impl<'a> LeaseGuard<'a> {
    /// Take ownership of `grant`, to be released through `capability` on
    /// drop unless disarmed first.
    pub fn new(runner: &'a dyn CommandRunner, capability: Capability, grant: Grant) -> Self {
        LeaseGuard {
            runner,
            capability,
            grant,
            armed: true,
        }
    }

    /// What gozer granted: the BDFs to reset, the device indices to pin, and
    /// the lease id.
    pub fn grant(&self) -> &Grant {
        &self.grant
    }

    /// Disarm and hand back the lease id -- for the success path, where the
    /// lease must outlive the function that took it. After this, `Drop`
    /// releases nothing and the caller owns the lease.
    pub fn into_lease_id(mut self) -> String {
        self.armed = false;
        self.grant.lease_id.clone()
    }
}

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best-effort and never a panic: this runs on an error path that
        // already has its own failure to report, and unwinding out of a
        // `Drop` during a panic would abort the process. Log loudly instead
        // -- a leaked lease is an operator-visible problem (`gozer status`
        // will show it) and the log line is what explains it.
        if let Err(err) = release(self.runner, &self.capability, &self.grant.lease_id) {
            eprintln!(
                "tt-station-agentd: failed to release lease '{}' while unwinding: {err:#}",
                self.grant.lease_id
            );
        }
    }
}
