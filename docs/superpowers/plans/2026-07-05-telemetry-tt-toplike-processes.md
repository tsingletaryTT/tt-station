# agentd `/telemetry` `tt_toplike.processes` Enrichment — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich agentd's `GET /telemetry` WebSocket frame with an optional additive `tt_toplike` key carrying a box-side process list, keeping the frame valid `tt-smi -s` JSON (inference deferred).

**Architecture:** A new `procscan` module builds a `TtToplike { schema, processes }` from a `sysinfo` process refresh plus a `/proc/<pid>/fd` scan for `/dev/tenstorrent` holders. `telemetry::enrich_frame` inserts it into the tt-smi JSON (or returns the frame verbatim on any hiccup). The telemetry WS loop owns a stateful `ProcessSampler` and enriches each tick.

**Tech Stack:** Rust, `sysinfo` (new dep), serde_json, axum WebSocket.

## Global Constraints

- The telemetry portion stays **byte-for-byte valid `tt-smi -s` JSON** — only *add* the top-level `tt_toplike` key; never reshape the tt-smi content.
- `tt_toplike` is **optional/additive**: on any failure to produce it (or if the tt-smi frame isn't a JSON object), emit the frame **verbatim** with no key. Never a half-populated object.
- **Scaffold scope:** emit `{ schema: 1, processes: [...] }` — **no `inference` field** (deferred; a missing `inference` sub-key means "not streamed → tt-toplike local fallback").
- Field names/types match the brief exactly: `ProcInfo { pid: u32, name: String, cmd: String, uses_tt: bool, cpu_pct: f32, mem_bytes: u64 }`, `TtToplike { schema: u32, processes: Vec<ProcInfo> }`, `schema == 1`.
- `uses_tt` is best-effort: a pid whose `/proc/<pid>/fd` is unreadable (owned by another uid, e.g. a docker/root vLLM) yields `false`; per-pid errors are swallowed, never propagated.
- Cap the process list at `MAX_PROCESSES = 12` (tt-holders kept first, then busiest by cpu).
- Additive to the WS loop: must NOT change the existing tt-smi error/skip behavior.
- TDD, DRY, YAGNI. `cargo test --workspace`, `cargo clippy -p tt-station-agentd --all-targets -- -D warnings`, `cargo fmt --check` (under the pinned 1.96.0 toolchain) must pass.

## Shared Type Definitions (authoritative)

In `crates/tt-station-agentd/src/procscan.rs`:

```rust
use serde::Serialize;

pub const TT_TOPLIKE_SCHEMA: u32 = 1;
pub const MAX_PROCESSES: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TtToplike {
    pub schema: u32,
    pub processes: Vec<ProcInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cmd: String,
    pub uses_tt: bool,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}
```

---

### Task 1: `procscan` types + pure helpers

**Files:**
- Create: `crates/tt-station-agentd/src/procscan.rs`
- Modify: `crates/tt-station-agentd/src/lib.rs` (`pub mod procscan;`)
- Modify: `Cargo.toml` (workspace) + `crates/tt-station-agentd/Cargo.toml` (add `sysinfo`)

**Interfaces:**
- Produces: `TtToplike`, `ProcInfo`, `TT_TOPLIKE_SCHEMA`, `MAX_PROCESSES` (Shared Types); `pub fn select_processes(procs: Vec<ProcInfo>, cap: usize) -> Vec<ProcInfo>`; `pub fn target_holds_tt_device(fd_link_targets: &[String]) -> bool`.

- [ ] **Step 1: Add the `sysinfo` dependency**

Workspace `Cargo.toml` `[workspace.dependencies]`: `sysinfo = "0.32"`. `crates/tt-station-agentd/Cargo.toml` `[dependencies]`: `sysinfo = { workspace = true }`. (If 0.32 doesn't resolve under the pinned toolchain, pick the newest that does and note it.)

- [ ] **Step 2: Register module + write failing tests**

`lib.rs`: add `pub mod procscan;`. Create `procscan.rs` with the Shared-Types structs/consts and:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, cpu: f32, uses_tt: bool) -> ProcInfo {
        ProcInfo { pid, name: format!("p{pid}"), cmd: String::new(), uses_tt, cpu_pct: cpu, mem_bytes: 0 }
    }

    #[test]
    fn holds_tt_device_matches_prefix() {
        assert!(target_holds_tt_device(&["/dev/tenstorrent0".into()]));
        assert!(target_holds_tt_device(&["/dev/null".into(), "/dev/tenstorrent1".into()]));
        assert!(!target_holds_tt_device(&["/dev/null".into(), "socket:[123]".into()]));
        assert!(!target_holds_tt_device(&[]));
    }

    #[test]
    fn select_puts_tt_holders_first_and_caps() {
        let procs = vec![
            proc(1, 90.0, false), proc(2, 5.0, true), proc(3, 50.0, false), proc(4, 1.0, true),
        ];
        let out = select_processes(procs, 3);
        assert_eq!(out.len(), 3);
        // both tt-holders survive the cap even though pid 4 is nearly idle
        assert!(out.iter().any(|p| p.pid == 2));
        assert!(out.iter().any(|p| p.pid == 4));
        // the remaining slot is the busiest non-holder (pid 1, cpu 90)
        assert!(out.iter().any(|p| p.pid == 1));
        assert!(!out.iter().any(|p| p.pid == 3));
    }

    #[test]
    fn select_orders_non_holders_by_cpu_desc() {
        let procs = vec![proc(1, 10.0, false), proc(2, 80.0, false), proc(3, 40.0, false)];
        let out = select_processes(procs, 10);
        let non_holder_pids: Vec<u32> = out.iter().map(|p| p.pid).collect();
        assert_eq!(non_holder_pids, vec![2, 3, 1]);
    }

    #[test]
    fn tt_toplike_serializes_with_brief_field_names() {
        let t = TtToplike { schema: TT_TOPLIKE_SCHEMA, processes: vec![proc(7, 3.5, true)] };
        let json = serde_json::to_string(&t).unwrap();
        for key in ["\"schema\"", "\"processes\"", "\"pid\"", "\"name\"", "\"cmd\"", "\"uses_tt\"", "\"cpu_pct\"", "\"mem_bytes\""] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        assert!(json.contains("\"schema\":1"));
    }
}
```

- [ ] **Step 3: Run → FAIL**

Run: `cargo test -p tt-station-agentd --lib procscan`
Expected: FAIL (helpers undefined).

- [ ] **Step 4: Implement the pure helpers**

```rust
/// True if any of a pid's fd symlink targets points at a Tenstorrent device
/// node. Pure (takes the already-readlink'd targets) so it's testable without
/// a real /proc.
pub fn target_holds_tt_device(fd_link_targets: &[String]) -> bool {
    fd_link_targets.iter().any(|t| t.starts_with("/dev/tenstorrent"))
}

/// Select which processes to report: every `uses_tt` holder (kept even if
/// idle), then the busiest remaining processes by `cpu_pct`, capped at `cap`.
pub fn select_processes(procs: Vec<ProcInfo>, cap: usize) -> Vec<ProcInfo> {
    let (mut holders, mut others): (Vec<_>, Vec<_>) = procs.into_iter().partition(|p| p.uses_tt);
    others.sort_by(|a, b| b.cpu_pct.partial_cmp(&a.cpu_pct).unwrap_or(std::cmp::Ordering::Equal));
    holders.extend(others);
    holders.truncate(cap);
    holders
}
```

- [ ] **Step 5: Run → PASS; clippy**

Run: `cargo test -p tt-station-agentd --lib procscan && cargo clippy -p tt-station-agentd --all-targets -- -D warnings`
Expected: PASS + clean.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/tt-station-agentd/Cargo.toml crates/tt-station-agentd/src/lib.rs crates/tt-station-agentd/src/procscan.rs
git commit -m "feat(agentd): procscan types + pure select/holds helpers (+sysinfo dep)"
```

---

### Task 2: `ProcessSampler` (sysinfo + `/proc/<pid>/fd` scan)

**Files:**
- Modify: `crates/tt-station-agentd/src/procscan.rs`

**Interfaces:**
- Consumes: `TtToplike`/`ProcInfo`/`select_processes`/`target_holds_tt_device`/`MAX_PROCESSES` (Task 1).
- Produces: `pub struct ProcessSampler`; `impl ProcessSampler { pub fn new() -> Self; pub fn sample(&mut self) -> TtToplike }`.

- [ ] **Step 1: Implement `ProcessSampler`**

```rust
/// Stateful process sampler. Owns a `sysinfo::System` so cpu% is meaningful
/// across successive `sample()` calls (sysinfo computes cpu usage from the
/// delta between two refreshes; the telemetry loop's interval provides them).
pub struct ProcessSampler {
    sys: sysinfo::System,
}

impl ProcessSampler {
    pub fn new() -> Self {
        Self { sys: sysinfo::System::new() }
    }

    /// One scan → a `TtToplike { schema, processes }`. Refreshes processes,
    /// flags `uses_tt` via a best-effort /proc/<pid>/fd scan, then selects and
    /// caps via `select_processes`. Infallible: per-pid errors degrade to
    /// `uses_tt=false`; an empty box yields `processes: []`.
    pub fn sample(&mut self) -> TtToplike {
        self.sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut procs: Vec<ProcInfo> = self
            .sys
            .processes()
            .iter()
            .map(|(pid, p)| {
                let pid_u = pid.as_u32();
                ProcInfo {
                    pid: pid_u,
                    name: p.name().to_string_lossy().into_owned(),
                    cmd: p.cmd().iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>().join(" "),
                    uses_tt: pid_holds_tt_device(pid_u),
                    cpu_pct: p.cpu_usage(),
                    mem_bytes: p.memory(), // sysinfo 0.30+ returns bytes
                }
            })
            .collect();
        procs = select_processes(std::mem::take(&mut procs), MAX_PROCESSES);
        TtToplike { schema: TT_TOPLIKE_SCHEMA, processes: procs }
    }
}

impl Default for ProcessSampler {
    fn default() -> Self { Self::new() }
}

/// Best-effort: read /proc/<pid>/fd, resolve each symlink, and test for a
/// Tenstorrent device holder. Any error (unreadable dir — e.g. another uid's
/// process) yields `false`; never panics.
fn pid_holds_tt_device(pid: u32) -> bool {
    let dir = format!("/proc/{pid}/fd");
    let Ok(entries) = std::fs::read_dir(&dir) else { return false; };
    let mut targets = Vec::new();
    for entry in entries.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            targets.push(target.to_string_lossy().into_owned());
        }
    }
    target_holds_tt_device(&targets)
}
```
> Verify the exact `sysinfo` 0.32 API names (`refresh_processes` signature/`ProcessesToUpdate`, `name()`/`cmd()` return types — `&OsStr`/`&[OsString]`, `memory()` unit = bytes, `cpu_usage()` = f32). Adapt calls to the resolved version; the shape above targets sysinfo 0.30–0.32. If an API differs, follow the crate and note it.

- [ ] **Step 2: Add a light behavioral test**

```rust
#[test]
fn sampler_reports_schema_and_finds_self() {
    let mut s = ProcessSampler::new();
    let _ = s.sample();          // first refresh seeds cpu% baseline
    let snap = s.sample();
    assert_eq!(snap.schema, TT_TOPLIKE_SCHEMA);
    assert!(snap.processes.len() <= MAX_PROCESSES);
    // the test process itself should be visible to sysinfo
    let me = std::process::id();
    // not asserting `me` is in the (capped/sorted) list — it may be idle and
    // truncated; just assert the scan produced a well-formed, bounded result.
    let _ = me;
}
```
> This is a real-`/proc`/sysinfo integration test; keep it undemanding (schema + bound) so it's stable in CI. The correctness-sensitive logic lives in the Task 1 pure helpers.

- [ ] **Step 3: Run → PASS; clippy; fmt**

Run: `cargo test -p tt-station-agentd --lib procscan && cargo clippy -p tt-station-agentd --all-targets -- -D warnings && cargo fmt -p tt-station-agentd`

- [ ] **Step 4: Commit**

```bash
git add crates/tt-station-agentd/src/procscan.rs
git commit -m "feat(agentd): ProcessSampler (sysinfo + /proc fd scan for uses_tt)"
```

---

### Task 3: `telemetry::enrich_frame`

**Files:**
- Modify: `crates/tt-station-agentd/src/telemetry.rs`

**Interfaces:**
- Consumes: `procscan::TtToplike` (Tasks 1–2).
- Produces: `pub fn enrich_frame(frame: &str, toplike: Option<&crate::procscan::TtToplike>) -> String`.

- [ ] **Step 1: Write failing tests**

Add to `telemetry.rs` tests:
```rust
use crate::procscan::{ProcInfo, TtToplike, TT_TOPLIKE_SCHEMA};

fn sample_toplike() -> TtToplike {
    TtToplike { schema: TT_TOPLIKE_SCHEMA, processes: vec![ProcInfo {
        pid: 7, name: "python3".into(), cmd: "run.py".into(),
        uses_tt: true, cpu_pct: 3.5, mem_bytes: 100 }] }
}

#[test]
fn enrich_inserts_key_and_keeps_tt_smi_valid() {
    let frame = r#"{"device_info":[{"board_info":{"board_type":"p150a"}}]}"#;
    let out = enrich_frame(frame, Some(&sample_toplike()));
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v.get("device_info").is_some());          // telemetry intact
    assert_eq!(v["tt_toplike"]["schema"], 1);
    assert_eq!(v["tt_toplike"]["processes"][0]["pid"], 7);
}

#[test]
fn enrich_none_returns_frame_verbatim() {
    let frame = r#"{"device_info":[]}"#;
    assert_eq!(enrich_frame(frame, None), frame);
}

#[test]
fn enrich_non_object_frame_returned_verbatim() {
    // not a JSON object → can't insert a key → return unchanged (graceful)
    let frame = "[1,2,3]";
    assert_eq!(enrich_frame(frame, Some(&sample_toplike())), frame);
    let garbage = "not json at all";
    assert_eq!(enrich_frame(garbage, Some(&sample_toplike())), garbage);
}
```

- [ ] **Step 2: Run → FAIL**

Run: `cargo test -p tt-station-agentd --lib telemetry`
Expected: FAIL (`enrich_frame` undefined).

- [ ] **Step 3: Implement**

```rust
/// Insert the optional `tt_toplike` object into a `tt-smi -s` JSON frame and
/// re-serialize. Returns `frame` **unchanged** when `toplike` is `None`, when
/// `frame` isn't a JSON object, or on any serialize error — so the telemetry
/// contract is preserved and a process-scan hiccup can never corrupt the frame.
pub fn enrich_frame(frame: &str, toplike: Option<&crate::procscan::TtToplike>) -> String {
    let Some(toplike) = toplike else { return frame.to_string(); };
    let Ok(serde_json::Value::Object(mut map)) = serde_json::from_str::<serde_json::Value>(frame) else {
        return frame.to_string();
    };
    let Ok(value) = serde_json::to_value(toplike) else { return frame.to_string(); };
    map.insert("tt_toplike".to_string(), value);
    serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| frame.to_string())
}
```

- [ ] **Step 4: Run → PASS; clippy**

Run: `cargo test -p tt-station-agentd --lib telemetry && cargo clippy -p tt-station-agentd --all-targets -- -D warnings`

- [ ] **Step 5: Commit**

```bash
git add crates/tt-station-agentd/src/telemetry.rs
git commit -m "feat(agentd): telemetry::enrich_frame (additive tt_toplike, verbatim on failure)"
```

---

### Task 4: Wire the sampler + enrichment into the telemetry WS loop

**Files:**
- Modify: `crates/tt-station-agentd/src/routes.rs` (`telemetry_stream`)

**Interfaces:**
- Consumes: `procscan::ProcessSampler` (Task 2), `telemetry::enrich_frame` (Task 3).

- [ ] **Step 1: Own a sampler and enrich each tick**

In `telemetry_stream` (around lines 1271–1300), before the `loop`, add:
```rust
    let mut sampler = crate::procscan::ProcessSampler::new();
```
In the `_ = ticker.tick()` arm, after the existing `let frame = match collect_snapshot(tt_smi_bin.clone()).await { ... }` produces the raw tt-smi frame, enrich it before sending:
```rust
                // Additive: fold the box's process list into the frame. The
                // scan is a fast local /proc read (unlike tt-smi's shell-out),
                // so it runs inline; a hiccup yields a verbatim frame, never an
                // error, so the existing tt-smi error/skip path is unchanged.
                let toplike = sampler.sample();
                let frame = crate::telemetry::enrich_frame(&frame, Some(&toplike));
```
Leave the error/skip branch (`collect_snapshot` returning `Err` → the small JSON error frame) exactly as-is — enrichment only wraps the success frame. `ProcessSampler` owns a `sysinfo::System` (Send), held across the `.await`, which is fine.

- [ ] **Step 2: Contract test (enriched frame round-trips as tt-smi JSON)**

Add a unit test in `routes.rs` tests (or extend an existing telemetry test): construct a canned tt-smi frame + a `ProcessSampler::new().sample()`, call `telemetry::enrich_frame`, and assert the output parses as a JSON object with both `device_info` (telemetry intact) and `tt_toplike.schema == 1`. (This mirrors Task 3's test but at the routes layer, proving the pieces compose.) If routes.rs has no natural place, this can live as an integration test in `crates/tt-station-agentd/tests/`.

- [ ] **Step 3: Build + workspace tests + clippy**

Run: `cargo build -p tt-station-agentd && cargo test -p tt-station-agentd && cargo clippy -p tt-station-agentd --all-targets -- -D warnings`
Expected: PASS + clean. (Optional live smoke, owner-run: start the agent, read `ws://127.0.0.1:8765/telemetry` and confirm each frame carries `tt_toplike.processes` alongside `device_info`.)

- [ ] **Step 4: Commit**

```bash
git add crates/tt-station-agentd/src/routes.rs
git commit -m "feat(agentd): enrich /telemetry frames with tt_toplike.processes"
```

---

### Task 5: Docs

**Files:**
- Modify: `CLAUDE.md`
- Modify: `docs/reference/tt-console.md` (only if it documents the telemetry frame; otherwise skip)

- [ ] **Step 1: Update CLAUDE.md**

In the Agent's `/telemetry` description, note it now emits an optional additive `tt_toplike` key (`{ schema: 1, processes[] }`, `inference` deferred) alongside the verbatim `tt-smi -s` JSON, per `TT_TOPLIKE_STREAM.md`; the frame stays valid tt-smi JSON so existing consumers are unaffected.

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: note tt_toplike.processes telemetry enrichment"
```

---

## Recommended execution order

1 → 2 → 3 → 4 → 5 (linear; each builds on the prior). Pull `origin/main` between tasks (the macOS/tt-toplike side pushes often).

## Self-review notes

- **Spec coverage:** frame shape + additive key (T3), process scan sysinfo+/proc (T1/T2), `select`/`uses_tt` (T1), sampler statefulness (T2), WS wiring (T4), graceful/verbatim + optional (T3), field-name/schema fidelity (T1 serde test), contract test (T3/T4), docs (T5). Inference explicitly out of scope (spec + Global Constraints). All spec sections map to a task.
- **Placeholder scan:** the one flagged unknown is the exact `sysinfo` 0.32 API surface (Task 2 Step 1 says verify + adapt) — a version lookup, not a guess.
- **Type consistency:** `TtToplike`/`ProcInfo`/`ProcessSampler`/`enrich_frame`/`select_processes`/`target_holds_tt_device` defined once in Shared Types / Task interfaces and referenced identically in T2–T4.
