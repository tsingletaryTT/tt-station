//! Client for `gozer`, the CLI that arbitrates which Tenstorrent chips
//! belong to which tenant when several agents share one box -- see
//! `docs/superpowers/specs/2026-08-16-gozer-integration-design.md` for the
//! full design; this module is Task 1's foundation.
//!
//! **Scope of this module today:** capability probing (`probe`), the
//! leasing verbs a serving backend needs (`acquire`, `release`,
//! `contention_detail`), grant/outcome parsing (`Outcome::from_captured`),
//! the release-on-drop `LeaseGuard`, (Task 4) the read-only
//! `snapshot_leases` behind `GET /leases`/`GET /status`'s `leasing` field,
//! and (Task 5) the `startup_sweep` that reclaims this agent's own abandoned
//! leases plus the `foreign_leases` guard both whole-box reset paths
//! refuse on.
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
//! root-owned container, that's immediately and forever. So every acquire
//! here passes an owner (`acquire_owned` is the one place that builds the
//! call). It starts as this agent's pid -- the agent must take the lease
//! before it knows which chips to launch on -- and is then HANDED TO THE
//! WORKLOAD (`hand_off`, `lease_gated_unit`): gozer judges liveness by
//! `/proc/<pid>` existing, which holds for a root-owned container process
//! too. See the "Workload ownership" section below and
//! `docs/reference/chip-leasing.md` §5.
//!
//! # Known limitations
//!
//! Recorded here, in the code a maintainer of this integration actually
//! opens, rather than only in the design doc. None of these is a bug to be
//! fixed by tightening the code below; each is a real boundary of what this
//! version knows.
//!
//! 1. **`--device-id`'s namespace (runpy backend) -- VERIFIED from source.**
//!    tt-inference-server's run.py turns `--device-id 2,3` into
//!    `docker run --device /dev/tenstorrent/2:/dev/tenstorrent/2 --device
//!    /dev/tenstorrent/3:...` and nothing else (`workflows/run_docker_server.py`),
//!    and gozer's `dev_index` is the kernel's `/dev/tenstorrent/<n>` number
//!    (`tenstorrent!N` in sysfs), so a leased serve's container gets exactly
//!    the granted nodes. The grant's `TT_VISIBLE_DEVICES` (BDFs), which
//!    `RunPyBackend::start` forwards to the run.py child, does NOT reach the
//!    container -- run.py never passes its own environment through -- so it
//!    only affects run.py's host-side `tt-smi -s` checks; whether those cope
//!    with a narrowed device list on a half-box lease is the one thing left to
//!    check on hardware (see `docs/reference/chip-leasing.md` §5).
//! 2. **A lease REAPED rather than released never resets its chips**
//!    (inherited from gozer: `--fresh` is the protected path). The next
//!    plain `acquire` then gets un-reset silicon. So agentd prefers an
//!    explicit `release` everywhere -- which is exactly what `LeaseGuard`,
//!    `release_lease`, and the startup sweep exist to guarantee.
//! 3. **`GET /leases` is CACHED** (`routes::GOZER_LEASE_CACHE_TTL`), so it
//!    can lag reality by up to that TTL -- a lease taken or released by
//!    another tenant a moment ago may not be visible yet. It is a status
//!    view, never an interlock; nothing may decide whether to reset chips
//!    from `/leases`. (The two reset refusals deliberately call
//!    `foreign_leases` fresh instead of reading that cache.)
//! 4. **A GAP IN GOZER, worked around here: `gozer status --json` carries no
//!    `lease_id`.** `gozer release` takes an id, and `status` reports `who`
//!    but never the id of the lease that `who` holds (verified against the
//!    real `gozer 0.1.0`). So the startup sweep has to make a SECOND
//!    round-trip -- `gozer history --json` -- to map a live `who` back to an
//!    id, and a live lease that history cannot account for is reported and
//!    left alone rather than guessed at (gozer reuses ids; guessing could
//!    reset a different tenant's chips).
//!    **The fix belongs in tt-gozer, and it is small: add `lease_id` to each
//!    chip's entry in `cmd_status`'s payload.** That deletes the history
//!    round-trip, `open_leases_from_history`, and the whole "id could not be
//!    resolved" branch outright. Someone should do it. It must NOT be
//!    quietly absorbed here as permanent complexity -- every further
//!    workaround built on the history log makes the real fix harder to
//!    justify.
//! 5. **`HeldLease.container_name` (docker backend) is recorded only at
//!    successful handover**; the failing exits inside `start` stop the
//!    container through an in-scope local instead. The two agree today
//!    because both derive from the same `container_name(model)` binding --
//!    but they are two implementations of "stop the right container", and a
//!    change to either has to keep them agreeing.
//! 6. **`DockerBackend` has no cancel flag**, deliberately: `RunPyBackend`
//!    has one because a `/stop` mid-bring-up must abort its health poll, and
//!    adding one to the docker backend would change UNLEASED behaviour --
//!    outside this integration's remit. Its in-flight failure paths are
//!    bounded by `LeaseGuard` instead, so a lease can still never leak.

use anyhow::{Context, Result};
use libttstation::model::LeaseEntry;
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
/// **`"all"` -- the whole box.** The overwhelmingly common request this
/// integration serves is an owner on a Mac asking for their own hardware
/// ("give me all my TT chips"), which is exactly how tt-station behaved
/// before gozer existed. So the common case stays whole-box and
/// contention-free: one tenant, no neighbour to negotiate with, no `409` on
/// the first `tt-station run` of the day.
///
/// **This REVERSES an earlier ruling in this integration.** For its first
/// five tasks the value was `"1"` -- gozer's own default, which a board-grain
/// box expands to the whole board that chip lives on -- on the reasoning that
/// "two boards, two tenants" is the unit the integration exists to hand out.
/// That inverted the priority. Per-board sharing is the SPECIAL case, entered
/// deliberately; making it the default meant a Mac user who asked for nothing
/// in particular got half their box and then contended with their own local
/// agents over the other half. See the design doc's "Ownership: an advisory
/// model" section.
///
/// **Per-board leases are not gone**, in either direction:
///
/// * other tenants (a local agent, `gozer acquire --chips 1`) still take
///   board-grain leases, and this agent's whole-box request then contends with
///   them -- reported with the holder named, and overridable with `--force`
///   (see [`Contention`] and `ForeignLeases::resolve`);
/// * [`acquire`] still takes `chips` as a parameter, so a narrower request is
///   one argument away. Nothing in agentd's config surface exposes it today;
///   if a per-serve chip count is ever wanted, it belongs there rather than in
///   a second constant here.
pub const DEFAULT_LEASE_CHIPS: &str = "all";

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
    /// `/dev/tenstorrent/<n>` indices -- gozer reads them from the kernel's
    /// `tenstorrent!N` sysfs link -- which is also what run.py's
    /// `--device-id` and the docker backend's `--device` take (known
    /// limitation 1).
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
    /// Environment gozer says a leased workload should run with -- today
    /// `{"TT_VISIBLE_DEVICES": "<bdf>,<bdf>"}`. Defaults to empty for a gozer
    /// version whose payload omits it.
    ///
    /// `RunPyBackend` forwards it to the `run.py` child alongside
    /// `MODEL_SOURCE`. run.py does not pass it on to the container (known
    /// limitation 1): it only scopes run.py's own host-side `tt-smi` checks.
    /// The container's pin is `--device-id`, from `dev_indices`.
    ///
    /// A `BTreeMap` rather than a `HashMap` so the forwarded order is
    /// deterministic, which keeps a test's view of the child environment
    /// stable.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
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
    acquire_owned(
        runner,
        capability,
        chips,
        None,
        std::process::id(),
        who,
        reason,
    )
}

/// [`acquire`] with an explicit owner: the lease lives exactly as long as
/// `owner_pid` does. This is how a lease is handed to the WORKLOAD rather
/// than to this agent -- the serving container's main process
/// ([`hand_off`]), or a holder unit's main process ([`open_gate`]) -- so an
/// agent restart no longer orphans it. gozer refuses a pid that is not alive
/// at acquire time, so a workload that died meanwhile gets no lease.
///
/// `exact` (a BDF) pins the grant to the one unit that chip belongs to --
/// the re-take half of [`hand_off`], which must get back exactly the chips
/// the workload is already driving.
pub fn acquire_owned(
    runner: &dyn CommandRunner,
    capability: &Capability,
    chips: &str,
    exact: Option<&str>,
    owner_pid: u32,
    who: &str,
    reason: &str,
) -> Outcome {
    let owner_pid = owner_pid.to_string();
    let mut args = vec![capability.path.as_str(), "acquire", "--chips", chips];
    if let Some(exact) = exact {
        args.extend(["--exact", exact]);
    }
    args.extend([
        "--owner-pid",
        owner_pid.as_str(),
        "--who",
        who,
        "--reason",
        reason,
        "--no-queue",
        "--json",
    ]);

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

/// One chip's record in `gozer status --json`'s `chips` array.
///
/// Started (Task 1) declaring only `board`/`state`/`who` -- enough to name a
/// contention holder (see `contention_detail`). Task 4's `snapshot_leases`
/// needs the rest of a chip's identity to build a full `LeaseEntry`, so
/// `bdf`/`dev_index`/`reason` were added here rather than in a second,
/// near-duplicate deserialization struct -- both consumers share one parse
/// of one payload shape. `card`/`pid`/`pids_holding`/`overstayed` are still
/// left undeclared: `serde` ignores unknown fields by default, so a gozer
/// that grows a field (or one this codebase doesn't surface yet) never
/// breaks parsing here.
///
/// Note what is NOT here: a `since`. gozer's per-chip status carries `who`,
/// `reason` and `state` but records no start time, so agentd can name a
/// holder and CANNOT say how long they have held the board. It must not
/// invent one -- see `LeaseEntry::since`'s doc comment for where that
/// constraint surfaces on the wire.
#[derive(Debug, Deserialize)]
struct StatusChip {
    #[serde(default)]
    board: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    who: Option<String>,
    #[serde(default)]
    bdf: Option<String>,
    #[serde(default)]
    dev_index: Option<u32>,
    #[serde(default)]
    reason: Option<String>,
    /// The lease's OWNER pid (`--owner-pid`, or the adopted holder's) -- what
    /// gozer judges the lease's liveness by. The reconciler compares it with
    /// the workload's own pid.
    #[serde(default)]
    pid: Option<i64>,
}

/// `gozer status --json`'s top-level payload. `grain` (`"board"` or
/// `"chip"`) is gozer's own term for its allocation unit on this box (see
/// `gozer topology --json`, which reports the identical value) -- read here
/// too so `snapshot_leases` can compute `max_concurrent` without a second
/// shell-out to `gozer topology`.
///
/// **`chips` is deliberately NOT `#[serde(default)]`.** Every other field
/// here and in `StatusChip` is, because tolerating unknown/absent fields is
/// how this survives a gozer that grows or renames something it doesn't
/// read. `chips` is the exception: it is the field the reset guards' entire
/// decision rests on. With a default, `{"grain":"board"}` -- or any future
/// payload that renames or nests the array -- would parse cleanly to an
/// EMPTY vec, `snapshot_leases` would answer `Some(empty)`,
/// `foreign_leases` would report `None`, and both whole-box `tt-smi -r`
/// paths would run on a box whose occupancy was never actually read. That
/// is the same "I could not understand the answer" -> "nobody is here"
/// collapse `ForeignLeases::Undetermined` exists to prevent, arriving
/// through the schema instead of the exit code. Required, it fails the
/// parse, `snapshot_leases` yields `None`, and the guards refuse.
///
/// `GET /leases` already absorbs that `None` via `unwrap_or_default()`
/// (see `state/hardware.rs`'s `gozer_snapshot`), so the strictness costs nothing
/// there: an unreadable payload reports zero leases, exactly as an
/// unreadable one always did.
#[derive(Debug, Default, Deserialize)]
struct StatusReport {
    chips: Vec<StatusChip>,
    #[serde(default)]
    grain: Option<String>,
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
///
/// **A lease this agent itself holds is worded differently.** `own_service_port`
/// is the caller's own serving port, and any clause whose `who` is
/// `tt-station:<that port>:<model>` is rewritten by [`own_lease_clause`] into
/// "this box is already serving `<model>`; `tt-station stop` first". Naming our own
/// `who` back at the operator as though it were a stranger sends them looking
/// for a tenant who does not exist -- see that function's doc comment.
pub fn contention_detail(
    runner: &dyn CommandRunner,
    capability: &Capability,
    own_service_port: u16,
) -> Option<String> {
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
            // OUR OWN lease first -- see `own_lease_clause`.
            (Some(who), _) => match own_lease_clause(who, own_service_port) {
                Some(mine) => mine,
                None => format!("board {board} is held by {who}"),
            },
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

/// Everything `GET /leases` and `GET /status`'s `leasing` field need, from
/// ONE `gozer status --json` call: every chip's lease record (see
/// [`LeaseEntry`]) plus the two topology facts derivable from that same
/// payload -- `boards` (distinct board serials seen) and `max_concurrent`
/// (how many tenants this box's current grain can serve at once).
///
/// Deliberately built from `status`, not a second call to `gozer topology`:
/// `status`'s own payload already carries `grain` and each chip's `board`
/// serial (see `StatusReport`/`StatusChip`), so `boards`/`max_concurrent`
/// fall out of the same parse `snapshot_leases` does for `leases` -- one
/// shell-out, one cache entry (see `state/hardware.rs`'s `AppState::gozer_snapshot`),
/// serving both routes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LeaseSnapshot {
    pub leases: Vec<LeaseEntry>,
    pub boards: u32,
    pub max_concurrent: u32,
}

/// Build a [`LeaseSnapshot`] from `gozer status --json`.
///
/// Best-effort throughout, mirroring `contention_detail`: a `status` that
/// fails to run, exits non-zero (including `16` mutex-stuck -- see the
/// design doc's failure-modes table), or returns unparseable JSON all yield
/// `None`. The caller (`handlers/status.rs`'s `GET /leases` and `/status`'s
/// `leasing` field) treats `None` as "nothing to report right now" rather
/// than an error -- gozer misbehaving must never turn either route into a
/// 5xx.
///
/// `max_concurrent` depends on gozer's reported `grain`: `"chip"` means
/// every chip is independently leasable, so it equals the chip count;
/// anything else (today, only `"board"`) means a whole board is the unit,
/// so it equals the distinct board count. `boards` always reports the
/// distinct-board-serial count regardless of grain -- that's a hardware
/// fact, not a leasing-unit fact.
///
/// **`since` is never populated.** See [`LeaseEntry`]'s doc comment and
/// `StatusChip`'s: gozer's per-chip status has no lease-start timestamp, so
/// inventing one here would be exactly the fabricated-duration hazard the
/// design doc calls out. Every `LeaseEntry` this function builds carries
/// `since: None`.
pub fn snapshot_leases(
    runner: &dyn CommandRunner,
    capability: &Capability,
) -> Option<LeaseSnapshot> {
    let captured = runner
        .run_capturing(&[capability.path.as_str(), "status", "--json"])
        .ok()?;
    if captured.code != 0 {
        return None;
    }
    let report: StatusReport = serde_json::from_str(&captured.stdout).ok()?;

    let mut board_order: Vec<String> = Vec::new();
    let mut leases = Vec::with_capacity(report.chips.len());
    for chip in &report.chips {
        let board = chip.board.clone().unwrap_or_default();
        if !board.is_empty() && !board_order.contains(&board) {
            board_order.push(board.clone());
        }
        leases.push(LeaseEntry {
            chip: chip.dev_index.unwrap_or_default(),
            bdf: chip.bdf.clone().unwrap_or_default(),
            board,
            state: chip.state.clone().unwrap_or_default(),
            who: chip.who.clone(),
            // NEVER populated -- see this function's doc comment.
            since: None,
            reason: chip.reason.clone(),
        });
    }

    let boards = board_order.len() as u32;
    let max_concurrent = if report.grain.as_deref() == Some("chip") {
        report.chips.len() as u32
    } else {
        boards
    };

    Some(LeaseSnapshot {
        leases,
        boards,
        max_concurrent,
    })
}

/// The `--who` prefix every lease this agent takes carries, and the filter
/// the startup sweep selects on. The full format is a shared contract
/// between both serving backends: `tt-station:<service_port>:<model>` (see
/// `RunPyBackend::acquire_lease` / `DockerBackend::acquire_lease`, which are
/// the only two places it is written, and [`who_service_port`], which is the
/// only place it is read).
pub const WHO_PREFIX: &str = "tt-station:";

/// The SERVICE PORT out of one of this agent's own `--who` strings
/// (`tt-station:<service_port>:<model>`), or `None` for a `who` that isn't
/// ours or doesn't carry a parseable port.
///
/// **The port is the second field, not "everything after the last colon".**
/// A model id may itself contain colons (`ghcr.io/org/model:0.14`), so this
/// splits into exactly three parts and reads the middle one.
///
/// Why the port and not a container name: `run.py` names its own container
/// and only reveals the id after launch -- long after the lease has to
/// exist -- so agentd cannot know a name at acquire time. The port is known
/// before launch and survives a container restart, which makes it the
/// better key regardless (see the design doc's "One startup sweep").
pub fn who_service_port(who: &str) -> Option<u16> {
    Some(split_who(who)?.0)
}

/// `(service_port, model)` out of one of this agent's own `--who` strings
/// (`tt-station:<service_port>:<model>`), or `None` for a `who` that isn't
/// ours or doesn't carry a parseable port.
///
/// Splits into exactly THREE parts and reads the middle one as the port: a
/// model id may itself contain colons (`ghcr.io/org/model:0.14`), so the
/// model is "everything after the second colon", never "the last field".
fn split_who(who: &str) -> Option<(u16, &str)> {
    let rest = who.strip_prefix(WHO_PREFIX)?;
    let (port, model) = rest.split_once(':')?;
    // A `who` with no model half is not one this agent wrote; refusing it
    // keeps the sweep's input to exactly the shape both backends emit.
    if model.is_empty() {
        return None;
    }
    Some((port.parse::<u16>().ok()?, model))
}

/// The contention clause for a lease held by THIS agent's own serving port,
/// or `None` when `who` names anybody else.
///
/// A contention message exists to tell an operator what to do next, and the
/// two cases could not be further apart: a stranger's lease means "find that
/// tenant"; our own means "you are already serving on this box -- stop it
/// first". Reporting the second as the first (`board <serial> is held by
/// tt-station:8080:Qwen3-32B`) names the operator's own session back at them
/// in a shape that reads like somebody else's, and sends them hunting for a
/// tenant who does not exist.
///
/// Reachable even after `start` learned to release its own lease before
/// acquiring: two concurrent `POST /run`s are not serialised (`handlers/serving.rs`'s
/// `run_model`), and an adopted lease from a previous process can be reaped
/// out from under this one -- both leave a `tt-station:<own port>:` lease
/// held that this backend has no in-memory record of.
pub fn own_lease_clause(who: &str, own_service_port: u16) -> Option<String> {
    let (port, model) = split_who(who)?;
    if port != own_service_port {
        return None;
    }
    Some(format!(
        "this box is already serving {model} on port {port} -- stop it first (`tt-station stop`)"
    ))
}

/// What one [`startup_sweep`] did, for the caller to log. Not consumed for
/// control flow anywhere -- the sweep's real effect is the `gozer release`
/// calls it makes -- but a startup line naming what was reclaimed (and what
/// could not be) is the only place an operator learns why a board came back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Lease ids released because nothing was serving on their port.
    pub released: Vec<String>,
    /// Lease ids left alone because something IS serving on their port.
    pub kept: Vec<String>,
    /// The subset of `kept` whose service port is THIS agent's own -- i.e.
    /// leases a previous process of this same agent took, whose serve is
    /// still running, and which the freshly-started backend knows nothing
    /// about. The caller hands each to `ServingBackend::adopt_lease` so the
    /// next `/stop` RELEASES it (resetting its chips) rather than leaving it
    /// to gozer's reap, which does not reset. See that method's doc comment.
    pub adoptable: Vec<AdoptableLease>,
    /// `who` strings that name a live tt-station lease this sweep could NOT
    /// act on -- no lease id resolvable from `gozer history`, or no
    /// parseable service port in the `who`. Reported, never guessed at.
    pub unresolved: Vec<String>,
}

/// A KEPT lease that belongs to this agent's own serving port, and so can be
/// handed to the freshly-built backend -- see [`SweepReport::adoptable`] and
/// `ServingBackend::adopt_lease`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptableLease {
    pub lease_id: String,
    /// The lease's full `tt-station:<port>:<model>` identity, which the
    /// docker backend needs to reconstruct the container name it must stop
    /// before releasing.
    pub who: String,
}

/// How many `gozer history` records the sweep reads. Bounded because this
/// runs between the capability probe and the socket bind, where everything
/// delays serving (see `main.rs`'s `detect_startup_device_mesh`); 200 is
/// far more than the handful of events a box accumulates between agentd
/// restarts, and `history.jsonl` is cleared with `/tmp` on every reboot.
///
/// **Truncating the window can only under-report, never over-report.**
/// `gozer history -n N` returns the LAST N records, and a lease's
/// `released`/`reaped` always follows its `granted` -- so a `granted` inside
/// the window always has its closing record inside it too. The only thing
/// falling off the front can do is hide a `granted` whose lease is still
/// open, which makes that lease `unresolved` (left alone), not falsely
/// released. Verified against the real `gozer 0.1.0` history payload.
const HISTORY_SCAN_RECORDS: &str = "200";

/// One `gozer history --json` record. Only the three fields the sweep needs
/// are declared; serde ignores the rest (`ts`, `chips`, `reason`, ...).
#[derive(Debug, Deserialize)]
struct HistoryRecord {
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    lease_id: Option<String>,
    #[serde(default)]
    who: Option<String>,
    /// The granted BDFs (on `granted`/`adopted`), so a lease can be matched
    /// to its chips in `gozer status` -- which reports owners per chip but no
    /// lease id.
    ///
    /// Lenient: anything but a list of strings reads as "unknown" rather than
    /// failing the whole history parse.
    #[serde(default, deserialize_with = "lenient_bdfs")]
    chips: Vec<String>,
}

fn lenient_bdfs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// `gozer history --json`'s top-level payload.
#[derive(Debug, Default, Deserialize)]
struct HistoryReport {
    #[serde(default)]
    history: Vec<HistoryRecord>,
}

/// THE STARTUP SWEEP: release every `tt-station:` lease whose service port
/// has nothing serving on it.
///
/// One rule, one direction (the design doc's "One startup sweep"). The
/// reverse case -- a container with no lease -- is already covered by
/// `--owner-pid`: agentd's death makes the lease reapable, and the orphaned
/// container shows up as `BUSY-UNTRACKED`, which gozer refuses to allocate.
///
/// The order of the three questions is the safety property:
///
/// 1. **What is running?** [`crate::serving::discovery::running_published_ports`].
///    An unreadable `docker ps` ends the sweep immediately -- gozer is not
///    even asked -- because "nothing is serving" and "I could not tell" must
///    never collapse into the same answer when the consequence is a chip
///    reset.
/// 2. **Which leases are live, and whose?** [`snapshot_leases`]. Only chips
///    that are not `FREE` and whose `who` starts with [`WHO_PREFIX`] are
///    candidates; a neighbour's lease is never touched.
/// 3. **Which lease id is that?** `gozer history --json`. This step exists
///    only because `gozer status --json` reports `who` but NOT `lease_id`,
///    and `gozer release` takes an id -- so the id is reconstructed from the
///    history log: the latest `granted`/`adopted` record for that exact
///    `who` with no later `released`/`reaped` for the same id. A `who` whose
///    id cannot be resolved that way is REPORTED, never guessed at
///    (`SweepReport::unresolved`): gozer reuses lease ids, and releasing a
///    stale one could reset a different tenant's chips. Exposing `lease_id`
///    in gozer's own `status --json` would remove this step entirely and is
///    the clean fix -- see the module doc's known limitations.
///
/// Best-effort throughout, and never fatal: every failure mode (docker
/// unreadable, `status` unreadable, `history` unreadable, a release gozer
/// refuses) leaves the lease exactly where it was and logs.
///
/// `own_service_port` is this agent's own serving port, and it decides one
/// thing only: which KEPT leases land in [`SweepReport::adoptable`]. It never
/// affects what gets released -- the release rule is still "nothing is
/// serving on this lease's port", for every `tt-station:` lease regardless of
/// whose port it names.
pub fn startup_sweep(
    runner: &dyn CommandRunner,
    capability: &Capability,
    own_service_port: u16,
) -> SweepReport {
    let mut report = SweepReport::default();

    // 1. Docker first: without it there is no question to answer, and no
    //    reason to spawn a single gozer subprocess.
    //
    // !!! DO NOT "SIMPLIFY" THIS TO `discover_serving` !!!
    // It is the obvious reuse -- same module, same `docker ps`, and it even
    // returns the ports -- and it is WRONG here. `discover_serving` only
    // reports a container once its `/v1/models` answers with a loaded model.
    // A container still loading a 70B publishes its port for many minutes
    // before that happens, so under that gate this sweep would read a live
    // bring-up as "nothing serving", release its lease, and `gozer release`
    // would reset the chips out from under it mid-load. That is exactly the
    // collision this whole integration exists to prevent, arriving through
    // the sweep meant to prevent it. `running_published_ports` asks the
    // narrower, safer question -- "is anything at all holding this port?" --
    // and shares the `docker ps` format and port parser with
    // `discover_serving` so there is still only one of each.
    let Some(ports_in_use) = crate::serving::discovery::running_published_ports(runner) else {
        eprintln!(
            "tt-station-agentd: skipping the startup lease sweep -- `docker ps` could not be \
             read, so nothing can be proven stale"
        );
        return report;
    };

    // 2. Live lease state, from the same `gozer status --json` parse
    //    `GET /leases` uses.
    let Some(snapshot) = snapshot_leases(runner, capability) else {
        eprintln!(
            "tt-station-agentd: skipping the startup lease sweep -- `gozer status --json` could \
             not be read"
        );
        return report;
    };
    let mut live_who: Vec<String> = Vec::new();
    // `who`s with at least one chip whose OWNER is still alive (any state but
    // `STALE`). Leases are owned by their workload now (see [`hand_off`]), so
    // a live owner IS a live workload -- even one that publishes no docker
    // port (a holder unit's vLLM), which `ports_in_use` cannot see.
    let mut owned_who: Vec<String> = Vec::new();
    for lease in &snapshot.leases {
        if lease.state == "FREE" {
            continue;
        }
        let Some(who) = lease.who.as_deref() else {
            continue;
        };
        if who.starts_with(WHO_PREFIX) && !live_who.iter().any(|seen| seen == who) {
            live_who.push(who.to_string());
        }
        if lease.state != "STALE" && !owned_who.iter().any(|seen| seen == who) {
            owned_who.push(who.to_string());
        }
    }
    if live_who.is_empty() {
        return report;
    }

    // 3. `who` -> lease id, from the history log (see this fn's doc).
    let open = open_leases_from_history(runner, capability);

    let mut handled: Vec<String> = Vec::new();
    for (lease_id, who) in open {
        if !live_who.contains(&who) {
            // Either not one of ours, or history says open while gozer's
            // live state does not. Not corroborated -> not touched.
            continue;
        }
        handled.push(who.clone());
        let Some(port) = who_service_port(&who) else {
            eprintln!(
                "tt-station-agentd: lease '{lease_id}' is held by '{who}', which carries no \
                 parseable service port; leaving it alone"
            );
            report.unresolved.push(who);
            continue;
        };
        let owner_alive = owned_who.contains(&who);
        if ports_in_use.contains(&port) || owner_alive {
            eprintln!(
                "tt-station-agentd: leaving lease '{lease_id}' ({who}) alone -- {}",
                if owner_alive {
                    "its owning workload is still alive".to_string()
                } else {
                    format!("something is still serving on port {port}")
                }
            );
            // A kept lease on OUR OWN serving port is one a previous process
            // of this agent took for a serve that is still up -- the
            // `Restart=on-failure` case. The freshly-built backend has no
            // record of it, so hand it back to the caller to adopt or nothing
            // will ever release it, and a REAPED lease is never reset.
            if port == own_service_port {
                report.adoptable.push(AdoptableLease {
                    lease_id: lease_id.clone(),
                    who: who.clone(),
                });
            }
            report.kept.push(lease_id);
            continue;
        }
        eprintln!(
            "tt-station-agentd: releasing stale lease '{lease_id}' ({who}) -- nothing is serving \
             on port {port}"
        );
        match release(runner, capability, &lease_id) {
            Ok(()) => report.released.push(lease_id),
            Err(err) => eprintln!(
                "tt-station-agentd: could not release stale lease '{lease_id}': {err:#} -- it \
                 stays held; clear it by hand with `gozer release {lease_id}`"
            ),
        }
    }

    // Anything gozer reports as a live tt-station lease that history could
    // not put an id to. Named, so an operator can clear it by hand.
    for who in live_who {
        if !handled.contains(&who) {
            eprintln!(
                "tt-station-agentd: lease held by '{who}' has no resolvable lease id in \
                 `gozer history`; leaving it alone (clear it by hand if it is stale)"
            );
            report.unresolved.push(who);
        }
    }

    report
}

/// `(lease_id, who)` for every lease `gozer history --json` shows as still
/// open: a `granted`/`adopted` record with no later `released`/`reaped` for
/// the same id. Empty on any failure (unreadable history, non-zero exit,
/// unparseable JSON) -- which makes the sweep a no-op rather than a guess.
///
/// A later `granted` for an id that is already open REPLACES the earlier
/// record's `who` rather than adding a second entry: gozer reuses ids, and
/// the most recent grant is the one that describes the lease alive now.
fn open_leases_from_history(
    runner: &dyn CommandRunner,
    capability: &Capability,
) -> Vec<(String, String)> {
    open_lease_records(runner, capability)
        .into_iter()
        .map(|open| (open.lease_id, open.who))
        .collect()
}

/// One still-open lease reconstructed from `gozer history --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenLease {
    lease_id: String,
    who: String,
    chips: Vec<String>,
}

/// Every lease `gozer history --json` shows as still open -- see
/// [`open_leases_from_history`], which is this without the chips.
fn open_lease_records(runner: &dyn CommandRunner, capability: &Capability) -> Vec<OpenLease> {
    let Ok(captured) = runner.run_capturing(&[
        capability.path.as_str(),
        "history",
        "--json",
        "-n",
        HISTORY_SCAN_RECORDS,
    ]) else {
        eprintln!("tt-station-agentd: could not run `gozer history`; no lease ids to sweep with");
        return Vec::new();
    };
    if captured.code != 0 {
        eprintln!(
            "tt-station-agentd: `gozer history --json` exited {}; no lease ids to sweep with",
            captured.code
        );
        return Vec::new();
    }
    let Ok(report) = serde_json::from_str::<HistoryReport>(&captured.stdout) else {
        eprintln!(
            "tt-station-agentd: could not parse `gozer history --json`; no lease ids to sweep with"
        );
        return Vec::new();
    };

    let mut open: Vec<OpenLease> = Vec::new();
    for record in report.history {
        let Some(lease_id) = record.lease_id else {
            continue;
        };
        match record.event.as_deref() {
            Some("granted") | Some("adopted") => {
                let who = record.who.unwrap_or_default();
                let chips = record.chips;
                match open.iter_mut().find(|o| o.lease_id == lease_id) {
                    Some(existing) => {
                        existing.who = who;
                        existing.chips = chips;
                    }
                    None => open.push(OpenLease {
                        lease_id,
                        who,
                        chips,
                    }),
                }
            }
            Some("released") | Some("reaped") => open.retain(|o| o.lease_id != lease_id),
            // `queued`/`refused`/anything else leaves the lease as it was.
            _ => {}
        }
    }
    open
}

/// What [`foreign_leases`] found: the three-way answer both whole-box reset
/// guards need.
///
/// **The third variant is the point.** "Nobody else is on this box" and "I
/// cannot tell whether anybody else is on this box" are different facts, and
/// an `Option` would collapse them into the same `None` -- which is exactly
/// how a guard ends up resetting a neighbour's chips while believing it
/// checked. They are separate variants so the caller is forced to decide
/// what to do about the second, and so a test can tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForeignLeases {
    /// gozer answered, and nothing is held by anyone but this agent's own
    /// session. The reset may proceed exactly as it always did.
    None,
    /// gozer answered, and someone else holds chips.
    ///
    /// **The STATE is part of the clause, not decoration.** `HELD`/`CLAIMED`
    /// means a tenant is there to ask; `STALE`/`HELD-FOREIGN` means they are
    /// already gone, and the remedy is completely different -- see
    /// [`ForeignLeases::refusal_reason`].
    Held {
        /// A human-readable clause per board
        /// (`"board <serial> is held by <who> (<state>)"`, joined with
        /// `"; "`), and NO duration -- gozer reports no lease start time (see
        /// `StatusChip`). This is what a refusal message quotes.
        holders: String,
        /// Every foreign-held chip's BDF, in gozer's own order.
        ///
        /// Carried separately from `holders` (rather than folded into that
        /// prose) because it has one consumer with one job: the line a
        /// `--force`d whole-box reset logs. When the owner deliberately steps
        /// on a neighbour, that log line is the ONLY remaining record of what
        /// was stepped on -- the refusal that would have named it never
        /// happened -- so it names the chips as well as the holder. See
        /// [`ForeignLeases::forced_override_note`].
        chips: Vec<String>,
    },
    /// gozer's lease state could not be read at all (the command failed to
    /// run, exited non-zero -- 14 topology unreadable, 16 mutex stuck -- or
    /// returned unparseable JSON). Nothing is known about who holds what.
    Undetermined,
}

impl ForeignLeases {
    /// Resolve this verdict for a caller that may have been handed a
    /// `--force`, so the two whole-box reset paths cannot drift in either the
    /// decision or the wording:
    ///
    /// * `Ok(None)` -- proceed; nothing foreign is held.
    /// * `Ok(Some(note))` -- proceed, and LOG `note`. The caller was forced,
    ///   and a deliberate stomp on a neighbour has to leave a trace (see
    ///   [`ForeignLeases::forced_override_note`]).
    /// * `Err(reason)` -- refuse; `reason` explains, names the holder, and
    ///   says `--force` overrides it.
    ///
    /// **This is a POLITENESS gate, and that is why `force` is here at all.**
    /// A lease communicates "someone is working on this board"; it does not
    /// make the owner a trespasser on their own machine. Everything this
    /// method guards is a refusal *on somebody else's behalf*, so somebody --
    /// the owner, at a keyboard, typing `--force` -- can decide to overrule
    /// it. The refusals that exist to stop this tool corrupting work *by
    /// accident* (stop-before-release ordering, the swap's compare-and-take,
    /// `Grant::reset_target`'s BDF check) take no `force` and must never grow
    /// one: nobody is making a decision when those fire.
    pub fn resolve(&self, force: bool) -> std::result::Result<Option<String>, String> {
        match self.refusal_reason() {
            None => Ok(None),
            Some(_) if force => Ok(self.forced_override_note()),
            Some(reason) => Err(reason),
        }
    }

    /// The line a `--force`d caller must log, or `None` when there was
    /// nothing to override.
    ///
    /// Names the HOLDER and (for `Held`) the exact chips, because once
    /// `--force` is given the refusal never happens and this line is the only
    /// surviving record that the owner took a board off somebody. An operator
    /// reading the journal afterwards -- or the neighbour asking why their
    /// model died -- has nothing else to go on.
    pub fn forced_override_note(&self) -> Option<String> {
        match self {
            ForeignLeases::None => None,
            ForeignLeases::Held { holders, chips } => Some(format!(
                "--force given: proceeding with a WHOLE-BOX reset even though {holders}. \
                 Chips reset out from under that holder: {}. This was a deliberate \
                 override, not a fault.",
                if chips.is_empty() {
                    "(none reported)".to_string()
                } else {
                    chips.join(",")
                }
            )),
            ForeignLeases::Undetermined => Some(
                "--force given: proceeding with a WHOLE-BOX reset even though `gozer status` \
                 could not be read, so whether another tenant holds chips is UNKNOWN. If one \
                 did, their chips were just reset. This was a deliberate override, not a fault."
                    .to_string(),
            ),
        }
    }

    /// Why a whole-box reset is refused ABSENT a `--force`, or `None` when it
    /// may proceed regardless. Prefer [`ForeignLeases::resolve`], which is
    /// what both callers use; this is the reason-builder behind it.
    ///
    /// Each caller prefixes the ACTION it is refusing (`"refusing to reset
    /// this box"` / `"refusing reset-chips"`); this supplies the reason and
    /// the remedy, so both guards word the same situation the same way.
    ///
    /// **Every reason here ends by naming `--force`.** A refusal that does not
    /// say how to overrule it reads as a hard rule, and these are not hard
    /// rules -- they are courtesies. The one that most needed saying is
    /// `Undetermined`: an operator whose gozer is wedged was previously left
    /// with ssh as the only way to reset their own box.
    ///
    /// **`Held` and `Undetermined` must never read alike.** An operator
    /// staring at a wedged box has to know which of the two they are in:
    /// one means "stop the other session", the other means "gozer itself is
    /// broken, fix it or reset by hand". A refusal that reported the wrong
    /// one would be worse than a generic failure, because it would send them
    /// looking for a tenant who isn't there.
    ///
    /// **`Held` is itself two situations, which is why the STATE is in the
    /// clause and `gozer reconcile` is in the remedy.** "Stop that session"
    /// is the whole answer only for a LIVE holder. For a `STALE` or
    /// `HELD-FOREIGN` lease the tenant is already gone: there is no session to
    /// stop, and the operator cannot fall back to `gozer release` either,
    /// because `gozer status` reports `who` but no lease id (see the module
    /// doc's known limitations). Naming only "stop that session" left the
    /// guard blocking a reset exactly when the box was wedged and nobody was
    /// there to unblock it.
    pub fn refusal_reason(&self) -> Option<String> {
        match self {
            ForeignLeases::None => None,
            ForeignLeases::Held { holders, .. } => Some(format!(
                "another tenant holds chips -- {holders}. If that session is live, stop it (or \
                 `gozer release` its lease) first. If its state is STALE or HELD-FOREIGN the \
                 tenant is already GONE -- and `gozer status` reports no lease id, so there is \
                 nothing to pass to `gozer release`: run `gozer reconcile` to clear it, then \
                 retry. This is a courtesy, not a lock: pass `--force` to reset anyway (it is \
                 your box) -- the holder's chips go down with yours, and the override is logged."
            )),
            ForeignLeases::Undetermined => Some(
                "`gozer status` could not be read, so whether another tenant holds chips cannot \
                 be determined -- and a whole-box reset would take theirs down with yours. Fix \
                 gozer first (`gozer status` should answer; `gozer reconcile` clears a stuck \
                 gate); if gozer was UNINSTALLED, restart tt-station-agentd -- it probes for \
                 gozer once at startup, so a live agent keeps trying to use one that is no \
                 longer there. Or pass `--force` to reset anyway: an operator who cannot read \
                 gozer must still be able to reset their own box without an ssh session."
                    .to_string(),
            ),
        }
    }
}

/// Whether anyone OTHER than this agent's own session on `own_service_port`
/// holds chips -- see [`ForeignLeases`] for the three-way answer.
///
/// This is the guard on both whole-box `tt-smi -r` paths: `POST /reset`
/// (`RunPyBackend::reset`) and `POST /power {"action":"reset-chips"}`
/// (`AppState::run_power_command`). A lease held by anyone else means the
/// reset is refused BY DEFAULT and the holder named -- and `--force`
/// overrides that (see [`ForeignLeases::resolve`], which both callers route
/// their decision through). Neither path evicts anybody implicitly; what
/// `--force` does is let the owner say "yes, I mean it" instead of leaving
/// them with ssh as the only route to their own hardware.
///
/// **Ownership is decided by the `who` prefix**, `tt-station:<port>:` --
/// the same string this agent writes at acquire time. A lease naming this
/// agent's own service port is ours (whatever model it names, and even if
/// it outlived the backend's in-memory record of it), so our own serve
/// never blocks our own reset.
///
/// **A `BUSY-UNTRACKED` chip is deliberately NOT a refusal.** It is not a
/// lease: there is no `who` to name and nobody to ask. It is also one of
/// the main reasons an operator reaches for a reset in the first place, so
/// refusing on it would disable the tool exactly when it is most needed.
///
/// **FAILS CLOSED BY DEFAULT, and that default is overridable.** An
/// unreadable `gozer status` is [`ForeignLeases::Undetermined`], and both
/// callers refuse on it unless `--force` was given.
///
/// An earlier version of this function failed OPEN, reasoning from the
/// design doc's failure-modes table ("14 topology unreadable → leasing
/// disabled for this call"). That row is about the SERVING path, where
/// degrading means the serve proceeds unleased and only the serving tenant
/// is affected; it does not transfer to a guard whose entire job is
/// protecting somebody else's hardware. Failing open said *"I cannot tell
/// whether anyone else is on this box, so I will reset it anyway"* -- with
/// nobody having decided that.
///
/// The asymmetry decides the DEFAULT: proceeding silently costs a neighbour
/// their running model and they get no say in it, so silence must not be
/// consent. But refusing with no way out was the other error: it cost the
/// owner their own box whenever gozer itself was wedged, leaving ssh as the
/// only route. `--force` is the way out, and it changes who is accountable
/// rather than what is safe -- somebody typed it, and it is logged (see
/// [`ForeignLeases::forced_override_note`]).
///
/// Note the message still says the state could not be READ, never that a
/// lease exists (see [`ForeignLeases::refusal_reason`]) -- those send an
/// operator to two different places.
///
/// The rest of the module's fail-closed rules are NOT overridable and must
/// not become so: `Grant::reset_target`'s BDF check and `leased_tt_device`
/// refuse because a guess would touch a chip nobody granted *by accident*.
/// There is no decision there to override.
pub fn foreign_leases(
    runner: &dyn CommandRunner,
    capability: &Capability,
    own_service_port: u16,
) -> ForeignLeases {
    // No snapshot -> nothing is KNOWN, which is not the same as nothing
    // being held. See `ForeignLeases::Undetermined`.
    let Some(snapshot) = snapshot_leases(runner, capability) else {
        return ForeignLeases::Undetermined;
    };
    let own_prefix = format!("{WHO_PREFIX}{own_service_port}:");

    let mut clauses: Vec<String> = Vec::new();
    // Every foreign-held chip, for the `--force` log line only -- one entry
    // per CHIP (unlike `clauses`, which dedupes to one per board), because
    // what a forced `tt-smi -r` steps on is chips.
    let mut chips: Vec<String> = Vec::new();
    for lease in &snapshot.leases {
        if lease.state == "FREE" {
            continue;
        }
        let Some(who) = lease.who.as_deref() else {
            // No `who` -- `BUSY-UNTRACKED` and friends. See the doc comment.
            continue;
        };
        if who.starts_with(&own_prefix) {
            continue;
        }
        if !lease.bdf.is_empty() && !chips.contains(&lease.bdf) {
            chips.push(lease.bdf.clone());
        }
        let board = if lease.board.is_empty() {
            "(unknown board)"
        } else {
            lease.board.as_str()
        };
        // The STATE is carried verbatim, in gozer's own vocabulary (`HELD`,
        // `HELD-FOREIGN`, `CLAIMED`, `STALE`, ...) -- see `LeaseEntry::state`,
        // which forbids renaming or restyling these strings, and
        // `ForeignLeases::refusal_reason`, which needs the operator to be able
        // to tell a live tenant from a departed one.
        let clause = format!("board {board} is held by {who} ({})", lease.state);
        if !clauses.contains(&clause) {
            clauses.push(clause);
        }
    }

    if clauses.is_empty() {
        ForeignLeases::None
    } else {
        ForeignLeases::Held {
            holders: clauses.join("; "),
            chips,
        }
    }
}

/// A request that could not proceed because chips are held by someone else
/// -- the box's state conflicts with what was asked, rather than anything
/// having gone wrong.
///
/// It exists to carry that distinction from wherever it is discovered
/// (`acquire_lease`'s unavailable outcome, `reset`'s refusal) up to the
/// route layer, which answers `409 Conflict` instead of `backend_error`'s
/// blanket `500` (see `routes::contention_aware_error`). Wrapped in an
/// `anyhow::Error` like any other failure, so every intermediate `?` and
/// `.context(..)` keeps working; the route recovers it with
/// `err.chain().any(|e| e.is::<Contention>())`.
///
/// **Never carries a duration.** gozer's per-chip status has no lease-start
/// time (see `StatusChip`), so a contention message names the board and the
/// holder and stops there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contention {
    message: String,
}

impl Contention {
    /// Build one from a fully-formed, caller-facing message (each backend
    /// words its own, e.g. `"runpy backend: cannot serve 'x' -- ..."`).
    pub fn new(message: impl Into<String>) -> Contention {
        Contention {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Contention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Contention {}

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

// ---------------------------------------------------------------------------
// Workload ownership: hand the lease to the process that uses the chips
// ---------------------------------------------------------------------------
//
// A lease taken with `--owner-pid <this agent>` lives exactly as long as the
// agent, not as long as the serve: every agent restart left it owned by a dead
// pid, so the next `gozer acquire` anywhere on the box reaped it (unreset) and
// handed a live container's chips to whoever asked -- and the startup adoption
// could only ever remember the id, never fix the owner ("UNOWNED LEASE").
//
// So a lease is owned by its WORKLOAD:
//
// * a serving CONTAINER: the agent takes the lease (it must: the grant decides
//   which chips the container gets), launches, and then [`hand_off`]s the lease
//   to the container's main pid (`docker inspect --format {{.State.Pid}}`). gozer
//   judges liveness by `/proc/<pid>` existing, which works for a root-owned
//   container process even though its fds are unreadable.
// * a HOLDER UNIT (a systemd user unit running the server on the host): the
//   unit starts behind a gate ([`gated_argv`]), the lease is acquired with the
//   unit's main pid, and only then does the gate `exec` the server -- same pid,
//   so the lease is the server's from its first instruction ([`lease_gated_unit`]).
// * `gozer run` workloads (bring-up, Orchard's own runs) already are: gozer run
//   owns the lease and the workload is its child.
//
// [`reconcile_own_leases`] then keeps it that way: it releases a lease whose
// owner and workload are both gone, and re-attaches one whose workload is alive
// but whose owner died.

/// argparse's exit status for an unknown sub-command -- how a gozer without
/// `transfer` answers [`hand_off`]'s first attempt.
const EXIT_USAGE: i32 = 2;

/// The main pid of a running container, or `None` when docker cannot say
/// (no such container, not running, docker unreadable).
pub fn container_pid(runner: &dyn CommandRunner, container: &str) -> Option<u32> {
    let out = runner
        .run(&["docker", "inspect", "--format", "{{.State.Pid}}", container])
        .ok()?;
    out.trim().parse::<u32>().ok().filter(|pid| *pid > 0)
}

/// The container publishing `port`, if any. The outer `None` means docker
/// could not be read -- which must never be mistaken for "nothing is serving"
/// when the consequence is a chip reset.
pub fn serving_container(runner: &dyn CommandRunner, port: u16) -> Option<Option<String>> {
    let filter = format!("publish={port}");
    let out = runner
        .run(&["docker", "ps", "--filter", &filter, "-q"])
        .ok()?;
    Some(out.split_whitespace().next().map(str::to_string))
}

/// One `gozer status --json`, parsed; `None` on any failure.
fn read_status(runner: &dyn CommandRunner, capability: &Capability) -> Option<StatusReport> {
    let captured = runner
        .run_capturing(&[capability.path.as_str(), "status", "--json"])
        .ok()?;
    if captured.code != 0 {
        return None;
    }
    serde_json::from_str(&captured.stdout).ok()
}

/// How many tenants are waiting in gozer's queue, or `None` if unreadable.
fn queue_len(runner: &dyn CommandRunner, capability: &Capability) -> Option<usize> {
    #[derive(Deserialize)]
    struct Queue {
        #[serde(default)]
        queue: Vec<serde_json::Value>,
    }
    let captured = runner
        .run_capturing(&[capability.path.as_str(), "queue", "--json"])
        .ok()?;
    if captured.code != 0 {
        return None;
    }
    serde_json::from_str::<Queue>(&captured.stdout)
        .ok()
        .map(|q| q.queue.len())
}

/// What [`hand_off`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handoff {
    /// The lease is now owned by the workload. `lease_id` is the id to track
    /// from here on -- unchanged after a `transfer`, NEW after a re-take.
    Moved { lease_id: String },
    /// Nothing changed: the lease is still held, by its previous owner. Says
    /// why; the reconciler tries again later.
    Kept(String),
    /// The old lease is gone and the chips could not be re-taken: the workload
    /// is running UNLEASED. Says why. The caller must stop tracking the old id.
    Lost(String),
}

/// Hand `lease_id` (granted `chips`) to the workload `pid`, so the lease lives
/// exactly as long as the workload instead of as long as this agent.
///
/// 1. **`gozer transfer <id> --owner-pid <pid>`** -- the atomic operation this
///    needs. gozer does not have it yet (see `docs/reference/chip-leasing.md`
///    §5, "gozer follow-up"); an unknown sub-command exits 2 and this falls
///    through. When gozer grows it, this path is used with no change here.
/// 2. **Interim: release `--no-reset`, then re-acquire the SAME chips with
///    `--owner-pid <pid>`.** `--no-reset` because the workload is driving
///    them; `--force` only because a host workload's own open fd would
///    otherwise refuse the release (a container's are invisible anyway) -- with
///    `--no-reset` the release touches no hardware. Between the two calls the
///    chips are briefly unlocked, so:
///    * it is not attempted while anyone is QUEUED (a queued tenant is
///      exactly who would win that window -- release opens a claim window
///      for the queue head), and the lease is [`Handoff::Kept`] instead;
///    * it re-takes exactly those chips (`--exact` for one unit, `all` for
///      the whole box) and refuses any other shape ([`Handoff::Kept`]);
///    * a grant that comes back different is released again (those chips
///      are idle) and reported [`Handoff::Lost`].
pub fn hand_off(
    runner: &dyn CommandRunner,
    capability: &Capability,
    lease_id: &str,
    chips: &[String],
    who: &str,
    reason: &str,
    pid: u32,
) -> Handoff {
    let path = capability.path.as_str();
    let pid_arg = pid.to_string();

    // 1. The atomic path, when gozer has it.
    let mut gone = false;
    match runner.run_capturing(&[
        path,
        "transfer",
        lease_id,
        "--owner-pid",
        &pid_arg,
        "--json",
    ]) {
        Ok(c) if c.code == 0 => {
            return Handoff::Moved {
                lease_id: lease_id.to_string(),
            }
        }
        Ok(c) if c.code == EXIT_NO_LEASE => gone = true,
        Ok(c) if c.code == EXIT_USAGE => {}
        Ok(c) => {
            return Handoff::Kept(format!(
                "gozer transfer exited {}: {}",
                c.code,
                c.stderr.trim()
            ))
        }
        Err(err) => return Handoff::Kept(format!("could not run gozer transfer: {err:#}")),
    }

    // 2. Interim: release --no-reset, re-acquire the same chips for `pid`.
    let Some(status) = read_status(runner, capability) else {
        return Handoff::Kept("`gozer status` could not be read".into());
    };
    let mut wanted: Vec<&str> = chips.iter().map(String::as_str).collect();
    wanted.sort_unstable();
    let mut all: Vec<&str> = status
        .chips
        .iter()
        .filter_map(|c| c.bdf.as_deref())
        .collect();
    all.sort_unstable();
    let boards: std::collections::BTreeSet<&str> = status
        .chips
        .iter()
        .filter(|c| c.bdf.as_deref().is_some_and(|b| wanted.contains(&b)))
        .filter_map(|c| c.board.as_deref())
        .collect();
    let chip_grain = status.grain.as_deref() == Some("chip");
    let (spec, exact) = if wanted.is_empty() {
        return Handoff::Kept(format!("lease '{lease_id}' names no chips"));
    } else if wanted == all {
        ("all", None)
    } else if (chip_grain && wanted.len() == 1) || (!chip_grain && boards.len() == 1) {
        ("1", Some(wanted[0]))
    } else {
        return Handoff::Kept(format!(
            "lease '{lease_id}' spans several units but not the whole box, which cannot be \
             re-taken exactly"
        ));
    };
    if !gone {
        match queue_len(runner, capability) {
            Some(0) => {}
            Some(n) => {
                return Handoff::Kept(format!(
                    "{n} tenant(s) are queued for chips; re-taking the lease now could hand \
                     them this workload's chips"
                ))
            }
            None => return Handoff::Kept("`gozer queue` could not be read".into()),
        }
        let released =
            runner.run_capturing(&[path, "release", lease_id, "--no-reset", "--force", "--json"]);
        match released {
            Ok(c) if c.code == 0 || c.code == EXIT_NO_LEASE => {}
            Ok(c) => {
                return Handoff::Kept(format!(
                    "gozer release --no-reset exited {}: {}",
                    c.code,
                    c.stdout.trim()
                ))
            }
            Err(err) => return Handoff::Kept(format!("could not run gozer release: {err:#}")),
        }
    }
    match acquire_owned(runner, capability, spec, exact, pid, who, reason) {
        Outcome::Granted(grant) => {
            let mut got: Vec<&str> = grant.chips.iter().map(String::as_str).collect();
            got.sort_unstable();
            if got == wanted {
                return Handoff::Moved {
                    lease_id: grant.lease_id,
                };
            }
            // Not the chips the workload is on: give them straight back.
            // Nothing runs there, so the reset the release does is harmless.
            let _ = release(runner, capability, &grant.lease_id);
            Handoff::Lost(format!(
                "re-acquire granted [{}] instead of [{}]",
                got.join(","),
                wanted.join(",")
            ))
        }
        Outcome::Unavailable { holder, .. } => Handoff::Lost(format!(
            "the chips were taken in between ({})",
            holder.unwrap_or_else(|| "holder unknown".into())
        )),
        Outcome::Failed(message) => Handoff::Lost(message),
    }
}

/// One of this agent's own leases (`tt-station:<own port>:`), as the
/// reconciler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnLease {
    pub lease_id: String,
    pub who: String,
    pub chips: Vec<String>,
    /// The pid gozer judges the lease by, when it reports one.
    pub owner_pid: Option<u32>,
    /// Whether gozer considers that owner alive (any state but `STALE`).
    pub owner_alive: bool,
}

/// What the reconciler decided for one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No live owner and no workload: give it back (which resets its chips --
    /// better than gozer's reap, which does not).
    Release { lease_id: String, why: String },
    /// The workload `to_pid` is alive but does not own the lease: hand it over.
    Rebind {
        lease_id: String,
        who: String,
        chips: Vec<String>,
        to_pid: u32,
        why: String,
    },
}

/// Decide what to do about this agent's own leases, given the serving
/// workload's pid (the container publishing the serving port), if any.
///
/// * owner dead, workload alive -> **Rebind** to the workload ("workload alive
///   but lease owner dead": an agent restart before the hand-off, or a lease
///   from before this change).
/// * owner dead, no workload -> **Release** ("lease with no live owner").
/// * owner is this agent, workload alive -> **Rebind** (a hand-off that was
///   deferred, e.g. while a tenant was queued).
/// * any other live owner -> nothing: it is a workload (a holder unit, a
///   container the lease was already handed to).
///
/// Pure, so the whole table is tested without a gozer or a docker.
pub fn plan_reconcile(
    leases: &[OwnLease],
    workload_pid: Option<u32>,
    agent_pid: u32,
) -> Vec<ReconcileAction> {
    let mut actions = Vec::new();
    for lease in leases {
        let owner = lease
            .owner_pid
            .map_or("no pid".to_string(), |p| format!("pid {p}"));
        match (lease.owner_alive, workload_pid) {
            (false, Some(pid)) => actions.push(ReconcileAction::Rebind {
                lease_id: lease.lease_id.clone(),
                who: lease.who.clone(),
                chips: lease.chips.clone(),
                to_pid: pid,
                why: format!("its owner ({owner}) is dead but its workload (pid {pid}) is alive"),
            }),
            (false, None) => actions.push(ReconcileAction::Release {
                lease_id: lease.lease_id.clone(),
                why: format!("its owner ({owner}) is dead and nothing is serving"),
            }),
            (true, Some(pid))
                if lease.owner_pid == Some(agent_pid) && lease.owner_pid != Some(pid) =>
            {
                actions.push(ReconcileAction::Rebind {
                    lease_id: lease.lease_id.clone(),
                    who: lease.who.clone(),
                    chips: lease.chips.clone(),
                    to_pid: pid,
                    why: format!(
                        "it is still owned by this agent, not by its workload (pid {pid})"
                    ),
                })
            }
            (true, _) => {}
        }
    }
    actions
}

/// What one [`reconcile_own_leases`] pass changed. The caller updates whatever
/// tracks these ids (the serving backend's slot, the community and holder
/// lease files) from `released` and `rebound`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Lease ids given back.
    pub released: Vec<String>,
    /// `(old id, new id)` -- the same id after a `transfer`.
    pub rebound: Vec<(String, String)>,
    /// Lease ids whose hand-off released them and could not re-take the chips.
    pub lost: Vec<String>,
    /// `who`s gozer shows as ours that history could not put an id to.
    pub unresolved: Vec<String>,
}

/// Bring this agent's own leases in line with what is running: see
/// [`plan_reconcile`] for the rules. `None` -- and NOTHING done -- when docker
/// or gozer could not be read, since "I could not tell" must never act like
/// "nothing is there" when the action is a chip reset.
pub fn reconcile_own_leases(
    runner: &dyn CommandRunner,
    capability: &Capability,
    own_service_port: u16,
    agent_pid: u32,
) -> Option<ReconcileReport> {
    let workload_pid = match serving_container(runner, own_service_port)? {
        // A container that publishes the port but whose pid cannot be read is
        // NOT "no workload".
        Some(id) => Some(container_pid(runner, &id)?),
        None => None,
    };
    let status = read_status(runner, capability)?;
    let prefix = format!("{WHO_PREFIX}{own_service_port}:");
    let ours: Vec<&StatusChip> = status
        .chips
        .iter()
        .filter(|c| c.state.as_deref() != Some("FREE"))
        .filter(|c| c.who.as_deref().is_some_and(|w| w.starts_with(&prefix)))
        .collect();
    let mut report = ReconcileReport::default();
    if ours.is_empty() {
        return Some(report);
    }

    let mut leases: Vec<OwnLease> = Vec::new();
    let mut matched: Vec<&str> = Vec::new();
    for open in open_lease_records(runner, capability) {
        // Its chips as gozer sees them now, held under the same `who`.
        let mine: Vec<&&StatusChip> = ours
            .iter()
            .filter(|c| c.who.as_deref() == Some(open.who.as_str()))
            .filter(|c| {
                open.chips.is_empty() || c.bdf.as_ref().is_some_and(|b| open.chips.contains(b))
            })
            .collect();
        if mine.is_empty() {
            continue;
        }
        for c in &mine {
            matched.extend(c.bdf.as_deref());
        }
        leases.push(OwnLease {
            lease_id: open.lease_id,
            who: open.who,
            chips: mine.iter().filter_map(|c| c.bdf.clone()).collect(),
            owner_pid: mine
                .iter()
                .find_map(|c| c.pid)
                .and_then(|p| u32::try_from(p).ok()),
            owner_alive: mine.iter().any(|c| c.state.as_deref() != Some("STALE")),
        });
    }
    for chip in &ours {
        if !chip.bdf.as_deref().is_some_and(|b| matched.contains(&b)) {
            let who = chip.who.clone().unwrap_or_default();
            if !report.unresolved.contains(&who) {
                eprintln!(
                    "tt-station-agentd: lease reconcile: '{who}' holds chips with no lease id in \
                     `gozer history`; leaving it alone"
                );
                report.unresolved.push(who);
            }
        }
    }

    for action in plan_reconcile(&leases, workload_pid, agent_pid) {
        match action {
            ReconcileAction::Release { lease_id, why } => {
                eprintln!("tt-station-agentd: lease reconcile: releasing '{lease_id}' -- {why}");
                match release(runner, capability, &lease_id) {
                    Ok(()) => report.released.push(lease_id),
                    Err(err) => eprintln!(
                        "tt-station-agentd: lease reconcile: could not release '{lease_id}': \
                         {err:#}"
                    ),
                }
            }
            ReconcileAction::Rebind {
                lease_id,
                who,
                chips,
                to_pid,
                why,
            } => {
                let reason = format!("workload pid {to_pid} (re-attached by tt-station-agentd)");
                match hand_off(runner, capability, &lease_id, &chips, &who, &reason, to_pid) {
                    Handoff::Moved { lease_id: new } => {
                        eprintln!(
                            "tt-station-agentd: lease reconcile: '{lease_id}' -> '{new}' now \
                             owned by workload pid {to_pid} -- {why}"
                        );
                        report.rebound.push((lease_id, new));
                    }
                    Handoff::Kept(reason) => eprintln!(
                        "tt-station-agentd: lease reconcile: '{lease_id}' left as is ({why}): \
                         {reason}; will retry"
                    ),
                    Handoff::Lost(reason) => {
                        eprintln!(
                            "tt-station-agentd: WARNING: lease reconcile: '{lease_id}' could not \
                             be re-attached to workload pid {to_pid}: {reason}. That workload is \
                             now serving WITHOUT a lease."
                        );
                        report.lost.push(lease_id);
                    }
                }
            }
        }
    }
    Some(report)
}

// ---------------------------------------------------------------------------
// Holder units: lease the unit's own main process
// ---------------------------------------------------------------------------

/// The gate a holder unit starts behind: wait (up to two minutes) for one line
/// on the FIFO `$0`; `go` execs the real command in place -- same pid, so the
/// lease taken on that pid meanwhile is the server's -- anything else (or
/// nothing, if the agent died) exits 75 without having touched a chip.
pub const GATE_SCRIPT: &str =
    r#"go=$(timeout 120 head -n 1 -- "$0"); [ "$go" = go ] || exit 75; exec "$@""#;

/// `argv` behind the gate on `fifo`: `sh -c GATE_SCRIPT <fifo> argv...`.
pub fn gated_argv(fifo: &std::path::Path, argv: &[String]) -> Vec<String> {
    let mut gated = vec![
        "sh".to_string(),
        "-c".to_string(),
        GATE_SCRIPT.to_string(),
        fifo.display().to_string(),
    ];
    gated.extend(argv.iter().cloned());
    gated
}

/// Make a fresh owner-only FIFO at `path` for [`gated_argv`].
pub fn make_gate(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let _ = std::fs::remove_file(path);
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Open the gate (`go`) or tell it to give up. Waits up to 10 s for the gate's
/// reader to appear (a non-blocking open fails until then).
pub fn signal_gate(path: &std::path::Path, go: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(mut f) => {
                f.write_all(if go { b"go\n" } else { b"no\n" })?;
                let _ = std::fs::remove_file(path);
                return Ok(());
            }
            Err(e)
                if std::time::Instant::now() < deadline
                    && e.raw_os_error() == Some(libc::ENXIO) =>
            {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
}

/// A systemd user unit's main pid, waiting briefly for it to be assigned.
pub fn unit_main_pid(unit: &str) -> Option<u32> {
    for _ in 0..50 {
        let pid = std::process::Command::new("systemctl")
            .args(["--user", "show", "-p", "MainPID", "--value", unit])
            .stdin(std::process::Stdio::null())
            .output()
            .ok()
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            })
            .filter(|p| *p > 0);
        if pid.is_some() {
            return pid;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

/// Lease the chips for a holder unit started with [`gated_argv`] on `fifo`,
/// OWNED BY THE UNIT'S MAIN PROCESS (`pid`, see [`unit_main_pid`]), then open
/// the gate. On any failure the gate is told to give up -- the unit exits
/// without opening a chip -- and the reason is returned.
///
/// The lease lives exactly as long as the server: the agent restarting changes
/// nothing, and the server exiting (crash or `systemctl stop`) makes it
/// reapable at once. Callers still RELEASE it explicitly after stopping the
/// unit, because a release resets the chips and a reap does not.
pub fn lease_gated_unit(
    runner: &dyn CommandRunner,
    capability: &Capability,
    fifo: &std::path::Path,
    pid: Option<u32>,
    who: &str,
    reason: &str,
) -> std::result::Result<Grant, String> {
    let refuse = |why: String| {
        let _ = signal_gate(fifo, false);
        Err(why)
    };
    let Some(pid) = pid else {
        return refuse("the serving unit's main pid could not be read".into());
    };
    match acquire_owned(
        runner,
        capability,
        DEFAULT_LEASE_CHIPS,
        None,
        pid,
        who,
        reason,
    ) {
        Outcome::Granted(grant) => {
            if let Err(e) = signal_gate(fifo, true) {
                // The server never started: give the chips straight back.
                let _ = release(runner, capability, &grant.lease_id);
                return Err(format!("couldn't start the serving unit: {e}"));
            }
            Ok(grant)
        }
        Outcome::Unavailable { holder, .. } => refuse(format!(
            "the chips are leased to someone else{}",
            holder.map(|h| format!(" ({h})")).unwrap_or_default()
        )),
        Outcome::Failed(message) => refuse(format!("gozer acquire failed: {message}")),
    }
}

/// A JSON list of lease ids kept in a file -- how leases on holder units are
/// remembered across agent restarts (`<config>/holder-leases.json`).
pub struct LeaseFile(pub std::path::PathBuf);

impl LeaseFile {
    pub fn ids(&self) -> Vec<String> {
        std::fs::read(&self.0)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write(&self, ids: &[String]) {
        if ids.is_empty() {
            let _ = std::fs::remove_file(&self.0);
            return;
        }
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&self.0, serde_json::to_vec(ids).unwrap_or_default());
    }

    pub fn add(&self, id: &str) {
        let mut ids = self.ids();
        if !ids.iter().any(|i| i == id) {
            ids.push(id.to_string());
        }
        self.write(&ids);
    }

    /// Every id, and the file emptied.
    pub fn take(&self) -> Vec<String> {
        let ids = self.ids();
        self.write(&[]);
        ids
    }

    /// Replace `old` with `new` (or drop it); `false` when `old` isn't here.
    pub fn swap(&self, old: &str, new: Option<&str>) -> bool {
        let mut ids = self.ids();
        let Some(i) = ids.iter().position(|id| id == old) else {
            return false;
        };
        match new {
            Some(new) => ids[i] = new.to_string(),
            None => {
                ids.remove(i);
            }
        }
        self.write(&ids);
        true
    }
}

/// Where holder-unit leases are remembered, under the agent's config dir.
pub fn holder_lease_file(config_dir: &std::path::Path) -> LeaseFile {
    LeaseFile(config_dir.join("holder-leases.json"))
}

/// [`hand_off`] to a serving CONTAINER: `container` if the caller knows it,
/// else whatever publishes `port`. [`Handoff::Kept`] when its pid cannot be
/// read -- the lease simply stays with this agent, and the reconciler retries.
#[allow(clippy::too_many_arguments)]
pub fn hand_off_to_container(
    runner: &dyn CommandRunner,
    capability: &Capability,
    lease_id: &str,
    chips: &[String],
    who: &str,
    reason: &str,
    container: Option<&str>,
    port: u16,
) -> Handoff {
    let container = match container {
        Some(c) => Some(c.to_string()),
        None => serving_container(runner, port).flatten(),
    };
    let Some(pid) = container.as_deref().and_then(|c| container_pid(runner, c)) else {
        return Handoff::Kept("the serving container's pid could not be read".into());
    };
    hand_off(runner, capability, lease_id, chips, who, reason, pid)
}

/// Apply a [`Handoff`] to a backend's lease slot (compare-and-swap through
/// `swap`), logging what happened. `swap(old, new)` must only replace `old`.
pub fn record_handoff(
    outcome: &Handoff,
    lease_id: &str,
    model: &str,
    swap: impl FnOnce(&str, Option<&str>) -> bool,
) {
    match outcome {
        Handoff::Moved { lease_id: new } => {
            if new != lease_id && !swap(lease_id, Some(new)) {
                eprintln!(
                    "tt-station-agentd: lease '{new}' (was '{lease_id}') changed hands while it \
                     was being handed to '{model}'s container; check `gozer status`"
                );
            }
            eprintln!(
                "tt-station-agentd: lease '{new}' for '{model}' is now owned by its serving \
                 container -- it survives agent restarts and ends with the container"
            );
        }
        Handoff::Kept(why) => eprintln!(
            "tt-station-agentd: lease '{lease_id}' for '{model}' stays owned by this agent for \
             now ({why}); the lease reconciler will hand it over"
        ),
        Handoff::Lost(why) => {
            swap(lease_id, None);
            eprintln!(
                "tt-station-agentd: WARNING: handing lease '{lease_id}' to '{model}'s container \
                 failed after the old lease was let go ({why}). '{model}' is serving WITHOUT a \
                 lease; `gozer status` shows its chips as free."
            );
        }
    }
}

/// How often [`reconcile_forever`] checks this agent's leases against what is
/// running. A dead workload's lease is released (and its chips reset) within
/// this long -- gozer's own reap, which does not reset, is the fallback.
pub const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Point every lease tracker at the reconciler's changes: the serving
/// backend's slot, the community package's lease file, and the holder-unit
/// lease file. Each is compare-and-swap, so only the one that tracked `old`
/// changes.
pub fn apply_reconcile(state: &crate::routes::AppState, report: &ReconcileReport) {
    let backend = state.backend();
    let holder = holder_lease_file(&state.config_dir());
    let swap = |old: &str, new: Option<&str>| {
        let _ = backend.swap_held_lease(old, new)
            || crate::community::swap_lease(state, old, new)
            || holder.swap(old, new);
    };
    for id in report.released.iter().chain(&report.lost) {
        swap(id, None);
    }
    for (old, new) in &report.rebound {
        swap(old, Some(new));
    }
}

/// The lease reconciler: once at startup (right after the sweep and
/// adoption), then every [`RECONCILE_INTERVAL`], run
/// [`reconcile_own_leases`] -- never while a start or stop is in flight, which
/// own their lease until they finish. A no-op without gozer.
pub async fn reconcile_forever(state: crate::routes::AppState) {
    let Some(capability) = state.gozer().cloned() else {
        return;
    };
    let port = state.serving_port();
    loop {
        let phase = state.lifecycle().phase;
        if !matches!(
            phase,
            crate::routes::Phase::Starting | crate::routes::Phase::Stopping
        ) {
            let cap = capability.clone();
            let report = tokio::task::spawn_blocking(move || {
                reconcile_own_leases(
                    &crate::serving::docker::RealCommandRunner,
                    &cap,
                    port,
                    std::process::id(),
                )
            })
            .await
            .ok()
            .flatten();
            if let Some(report) = report {
                apply_reconcile(&state, &report);
            }
        }
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

/// For a holder unit about to be started: the gate FIFO to start it behind
/// ([`gated_argv`]), freshly made under the agent's config dir -- or `None`
/// when gozer is absent (the unit starts ungated, unleased, exactly as
/// before) or the FIFO cannot be made (logged; same fallback).
pub fn unit_gate(state: &crate::routes::AppState, name: &str) -> Option<std::path::PathBuf> {
    // Only the box's own agent runs real units (a test's state never does).
    if !state.manages_units() {
        return None;
    }
    state.gozer()?;
    let fifo = state.config_dir().join(format!("{name}.gate"));
    match make_gate(&fifo) {
        Ok(()) => Some(fifo),
        Err(e) => {
            eprintln!(
                "tt-station-agentd: couldn't make the lease gate {}: {e}; starting the unit \
                 without a lease",
                fifo.display()
            );
            None
        }
    }
}

/// After `systemd-run` started `unit` behind `fifo`: lease the chips to the
/// unit's main process and let it run. The lease id is returned for the caller
/// to remember (and release after stopping the unit). On failure the gate
/// makes the unit exit before it touches a chip, the unit is stopped to be
/// sure, and the reason is returned.
pub async fn lease_started_unit(
    state: &crate::routes::AppState,
    unit: &str,
    fifo: std::path::PathBuf,
    model: &str,
) -> std::result::Result<String, String> {
    let Some(capability) = state.gozer().cloned() else {
        return Err("gozer is not available".into());
    };
    let who = format!("{WHO_PREFIX}{}:{model}", state.serving_port());
    let reason = format!("serving {model} in {unit} via tt-station-agentd");
    let u = unit.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let pid = unit_main_pid(&u);
        lease_gated_unit(
            &crate::serving::docker::RealCommandRunner,
            &capability,
            &fifo,
            pid,
            &who,
            &reason,
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    match outcome {
        Ok(grant) => {
            eprintln!(
                "tt-station-agentd: leased {} chip(s) as lease {} to {unit}'s main process",
                grant.chips.len(),
                grant.lease_id
            );
            Ok(grant.lease_id)
        }
        Err(why) => {
            let _ = tokio::process::Command::new("systemctl")
                .args(["--user", "stop", unit])
                .stdin(std::process::Stdio::null())
                .status()
                .await;
            Err(why)
        }
    }
}

/// Release `ids` after the unit that owned them has been stopped -- the
/// release resets the chips, which gozer's reap of a dead owner would not.
pub async fn release_after_stop(state: &crate::routes::AppState, ids: Vec<String>) {
    let Some(capability) = state.gozer().cloned() else {
        return;
    };
    if ids.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for id in ids {
            if let Err(err) = release(&crate::serving::docker::RealCommandRunner, &capability, &id)
            {
                eprintln!("tt-station-agentd: couldn't release lease '{id}': {err:#}");
            }
        }
    })
    .await;
}
