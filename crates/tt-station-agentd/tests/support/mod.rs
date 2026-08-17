//! Shared test-only helpers for `tt-station-agentd`'s integration tests.
//!
//! `tests/*.rs` files are each compiled as their own separate binary crate,
//! so a helper defined in one (e.g. the original `FakeRunner` in
//! `tests/serving.rs`) is invisible to any other. Rather than duplicating it,
//! this lives in `tests/support/mod.rs` -- a subdirectory module, not a
//! top-level `tests/*.rs` file, so Cargo doesn't treat it as its own test
//! target -- and gets pulled into whichever integration test needs it via
//! `mod support;`.
//!
//! `#[allow(dead_code)]` throughout: different consumers (`tests/serving.rs`,
//! `tests/control.rs`) exercise different subsets of this API, and each
//! integration test file is its own compilation unit, so "unused" is
//! per-target rather than a real dead-code signal.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use tt_station_agentd::serving::docker::{CapturedOutput, CommandRunner};

/// One canned `run_capturing` response: `(argv substring matcher, exit code,
/// stdout, stderr)`. Named alias (rather than the bare 4-tuple inline) so
/// `FakeRunner`'s `run_capturing_outputs` field doesn't trip clippy's
/// `type_complexity` lint.
type RunCapturingCanned = (String, i32, String, String);

/// One child process's environment, as `(key, value)` pairs in the order
/// `RunPyBackend` handed them over. Named alias for the same reason
/// `RunCapturingCanned` is one: the nested `Arc<Mutex<Vec<Vec<..>>>>` field it
/// lives behind trips clippy's `type_complexity`.
type ChildEnv = Vec<(String, String)>;

/// A side effect to run when a `run` argv matches, paired with its matcher.
/// Named alias for the same `type_complexity` reason as the two above.
type RunHook = (String, Box<dyn Fn() + Send + Sync>);

/// A scratch `model_spec.json` fixture, unique per call and removed on drop.
/// Was duplicated near-identically in `tests/models.rs` and `tests/runpy.rs`
/// (both files' own doc comments admitted it); consolidated here now that
/// both already pull in this `tests/support` module for other fakes.
#[allow(dead_code)]
pub struct TempModelSpec(std::path::PathBuf);

impl TempModelSpec {
    #[allow(dead_code)]
    pub fn write(contents: &str) -> Self {
        // A process-unique monotonic counter -- `Instant::now().elapsed()` is
        // ~0ns for a freshly-taken instant, so it does NOT make the filename
        // unique and parallel tests would collide on the same path (one
        // test's Drop deleting another's file mid-read). An atomic counter is
        // genuinely unique per call.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tt-station-model-spec-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).expect("write temp model_spec.json fixture");
        TempModelSpec(path)
    }

    #[allow(dead_code)]
    pub fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

impl Drop for TempModelSpec {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A fake `CommandRunner` that records every command it's asked to `run`
/// (so tests can assert on the exact argv `DockerBackend` builds) and
/// reports `health_ok` as healthy either immediately or after a configured
/// number of prior probes (so `DockerBackend`'s poll-until-healthy loop is
/// exercised for real, not just its "healthy on the first try" path).
///
/// Cheap to `Clone`: all state lives behind `Arc<Mutex<_>>`, so a test can
/// hand one clone to `DockerBackend` (which needs to own a
/// `Box<dyn CommandRunner>`) while keeping another clone around to inspect
/// what happened after the call returns.
#[derive(Clone)]
pub struct FakeRunner {
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    health_calls_before_ok: u32,
    health_calls_seen: Arc<Mutex<u32>>,
    /// Canned stdout for `run` calls whose argv (space-joined) CONTAINS a
    /// registered substring -- e.g. `"docker ps"` -- so `RunPyBackend::stop`
    /// (which parses `docker ps`'s stdout for container ids) can be
    /// exercised without a real `docker` binary. Checked in insertion order;
    /// first match wins. Calls that match nothing get `""`, same as before
    /// this field existed.
    run_outputs: Arc<Mutex<Vec<(String, String)>>>,
    /// Canned failures for `run` calls whose argv (space-joined) CONTAINS a
    /// registered substring -- e.g. `"tt-smi -r"` -- so tests can exercise
    /// what happens when a specific command (like the pre-serve board
    /// reset) fails, without making every `run` call fail. Checked in
    /// insertion order, same as `run_outputs`; a match short-circuits
    /// `run` with `Err` before it ever records success or consults
    /// `run_outputs`.
    run_failures: Arc<Mutex<Vec<(String, String)>>>,
    /// Canned response body every `http_get` call returns (regardless of
    /// `url`) once `set_http_get` is called -- e.g. `RunPyBackend::start`'s
    /// `GET /v1/models` readiness poll. `None` (the default, unset) means
    /// `http_get` returns `DEFAULT_HTTP_GET_BODY` (see below), NOT an error.
    http_get_response: Arc<Mutex<Option<String>>>,
    /// Canned SEQUENCE of `http_get` responses, consumed one per call and
    /// STICKING at the last entry once exhausted, so a test can model
    /// `/v1/models` erroring/empty for the first few polls and then coming
    /// up populated (exactly what `RunPyBackend::start`'s readiness poll
    /// waits for). Each entry is `Some(body)` (returned `Ok`) or `None`
    /// (returned `Err`). Takes precedence over `http_get_response` whenever
    /// it's non-empty. Mirrors `set_run_output`/`set_http_get`.
    http_get_sequence: Arc<Mutex<Vec<Option<String>>>>,
    /// How many `http_get` calls have been seen -- indexes into
    /// `http_get_sequence`.
    http_get_calls_seen: Arc<Mutex<usize>>,
    /// Canned `run_capturing` response -- `(code, stdout, stderr)` -- for
    /// calls whose space-joined argv CONTAINS a registered substring, e.g.
    /// `("gozer --version", 0, "gozer 0.1.0", "")`. Checked in insertion
    /// order, first match wins, same convention as `run_outputs`/
    /// `run_failures` -- but a DISTINCT list from those two: `run_capturing`
    /// preserves a non-zero exit code and stdout TOGETHER (that's the whole
    /// reason it exists -- see `CommandRunner::run_capturing`'s doc comment),
    /// which `run_outputs`/`run_failures` can't represent (they only know
    /// "succeeded with this stdout" or "failed with this message", never
    /// "exited N with this stdout/stderr"). A call matching nothing gets
    /// `CapturedOutput { code: 0, stdout: "", stderr: "" }`.
    run_capturing_outputs: Arc<Mutex<Vec<RunCapturingCanned>>>,
    /// Canned FAILURES for `run_capturing` -- mirrors `run_failures`, but for
    /// `run_capturing`'s separate canning list. Lets a test model "the
    /// command couldn't even be spawned" (e.g. `gozer` genuinely absent),
    /// which is a real `Err` and distinct from a clean non-zero EXIT (which
    /// `run_capturing_outputs` models instead). Checked in insertion order,
    /// same convention as `run_failures`; a match short-circuits
    /// `run_capturing` with `Err` before it consults `run_capturing_outputs`.
    run_capturing_failures: Arc<Mutex<Vec<(String, String)>>>,
    /// The `env` pairs of every `run_in_dir_with_env` call, in order. The
    /// default `CommandRunner::run_in_dir_with_env` throws the environment
    /// away, so without this a test cannot see what a child was given -- and
    /// `RunPyBackend` passes `MODEL_SOURCE` plus (under a lease) the grant's
    /// own `TT_VISIBLE_DEVICES` that way rather than in argv.
    child_envs: Arc<Mutex<Vec<ChildEnv>>>,
    /// Side effects fired when a `run` call's space-joined argv contains a
    /// registered substring -- the seam a test uses to interleave something
    /// into the middle of a backend call (e.g. landing a second lease in the
    /// backend's slot while `start`'s swap is between its `docker ps` and its
    /// `gozer release`). Fired before the canned failure/output lookup, so a
    /// hook still runs on a command that is about to fail.
    run_hooks: Arc<Mutex<Vec<RunHook>>>,
}

/// Default `http_get` body when nothing is configured: a non-empty `data`
/// array (so `RunPyBackend::start`'s `/v1/models` readiness gate is
/// satisfied and `start` succeeds) whose single entry carries NO `id` (so
/// `Endpoint.model` falls back to the caller's original `model` argument).
/// This lets the many argv-focused tests build a bare `FakeRunner::new(..)`
/// and still have `start` succeed with `endpoint.model == <arg>`, without
/// each having to stub `/v1/models` by hand.
const DEFAULT_HTTP_GET_BODY: &str = r#"{"data":[{}]}"#;

impl FakeRunner {
    #[allow(dead_code)]
    pub fn new(health_calls_before_ok: u32) -> Self {
        FakeRunner {
            commands: Arc::new(Mutex::new(Vec::new())),
            health_calls_before_ok,
            health_calls_seen: Arc::new(Mutex::new(0)),
            run_outputs: Arc::new(Mutex::new(Vec::new())),
            run_failures: Arc::new(Mutex::new(Vec::new())),
            http_get_response: Arc::new(Mutex::new(None)),
            http_get_sequence: Arc::new(Mutex::new(Vec::new())),
            http_get_calls_seen: Arc::new(Mutex::new(0)),
            run_capturing_outputs: Arc::new(Mutex::new(Vec::new())),
            run_capturing_failures: Arc::new(Mutex::new(Vec::new())),
            child_envs: Arc::new(Mutex::new(Vec::new())),
            run_hooks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Run `action` whenever a future `run` call's space-joined argv contains
    /// `matcher` -- see the `run_hooks` field.
    #[allow(dead_code)]
    pub fn on_run(&self, matcher: &str, action: impl Fn() + Send + Sync + 'static) {
        self.run_hooks
            .lock()
            .expect("run_hooks mutex poisoned")
            .push((matcher.to_string(), Box::new(action)));
    }

    /// The `env` pairs handed to each `run_in_dir_with_env` call, in order --
    /// see the `child_envs` field.
    #[allow(dead_code)]
    pub fn child_envs(&self) -> Vec<ChildEnv> {
        self.child_envs
            .lock()
            .expect("child_envs mutex poisoned")
            .clone()
    }

    #[allow(dead_code)]
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .clone()
    }

    /// How many `health_ok` probes have been made so far. Lets a test assert
    /// that `RunPyBackend::start` BAILED early (on a cancel or a dead
    /// container) instead of grinding its full health-poll ceiling -- the
    /// probe count is the observable "didn't run all `health_poll_attempts`".
    #[allow(dead_code)]
    pub fn health_calls(&self) -> u32 {
        *self
            .health_calls_seen
            .lock()
            .expect("health mutex poisoned")
    }

    /// Register canned stdout `output` for the next (and any subsequent)
    /// `run` call whose space-joined argv contains `matcher`. See the
    /// `run_outputs` field doc for why this exists (`docker ps` parsing in
    /// `RunPyBackend::stop`).
    #[allow(dead_code)]
    pub fn set_run_output(&self, matcher: &str, output: &str) {
        self.run_outputs
            .lock()
            .expect("run_outputs mutex poisoned")
            .push((matcher.to_string(), output.to_string()));
    }

    /// Make any future `run` call whose space-joined argv contains `matcher`
    /// return `Err` with `message` instead of succeeding -- e.g.
    /// `fail_run("tt-smi -r", "board reset timed out")` to exercise a
    /// failing pre-serve board reset without a real `tt-smi` binary.
    #[allow(dead_code)]
    pub fn fail_run(&self, matcher: &str, message: &str) {
        self.run_failures
            .lock()
            .expect("run_failures mutex poisoned")
            .push((matcher.to_string(), message.to_string()));
    }

    /// Set the canned body every future `http_get` call returns, regardless
    /// of `url` -- e.g. `set_http_get(r#"{"data":[{"id":"Qwen/Qwen3-32B"}]}"#)`
    /// to exercise `RunPyBackend::start`'s `/v1/models` readiness poll.
    /// Left unset (the default), `http_get` returns `DEFAULT_HTTP_GET_BODY`.
    #[allow(dead_code)]
    pub fn set_http_get(&self, body: &str) {
        *self
            .http_get_response
            .lock()
            .expect("http_get_response mutex poisoned") = Some(body.to_string());
    }

    /// Set a SEQUENCE of `http_get` responses, consumed one per call and
    /// sticking at the last once exhausted -- each entry `Some(body)` returns
    /// `Ok(body)`, `None` returns `Err`. Lets a test model `/v1/models`
    /// erroring/empty at first and then coming up populated, driving
    /// `RunPyBackend::start`'s readiness poll through more than one round.
    /// Takes precedence over `set_http_get` while non-empty.
    #[allow(dead_code)]
    pub fn set_http_get_sequence(&self, bodies: &[Option<&str>]) {
        *self
            .http_get_sequence
            .lock()
            .expect("http_get_sequence mutex poisoned") =
            bodies.iter().map(|b| b.map(str::to_string)).collect();
    }

    /// Register a canned `run_capturing` response -- exit `code` plus
    /// `stdout`/`stderr` -- for the next (and any subsequent) `run_capturing`
    /// call whose space-joined argv contains `matcher`. E.g.
    /// `set_run_capturing("gozer --version", 0, "gozer 0.1.0", "")`, or
    /// `set_run_capturing("gozer acquire", 12, r#"{"granted":false}"#, "")`
    /// to exercise gozer's non-zero-exit-with-JSON-on-stdout contract.
    #[allow(dead_code)]
    pub fn set_run_capturing(&self, matcher: &str, code: i32, stdout: &str, stderr: &str) {
        self.run_capturing_outputs
            .lock()
            .expect("run_capturing_outputs mutex poisoned")
            .push((
                matcher.to_string(),
                code,
                stdout.to_string(),
                stderr.to_string(),
            ));
    }

    /// Make any future `run_capturing` call whose space-joined argv contains
    /// `matcher` return `Err` with `message` instead of a `CapturedOutput` --
    /// e.g. `fail_run_capturing("gozer", "No such file or directory")` to
    /// exercise "gozer genuinely isn't installed" (a real spawn failure),
    /// distinct from a clean non-zero exit (see `set_run_capturing`).
    #[allow(dead_code)]
    pub fn fail_run_capturing(&self, matcher: &str, message: &str) {
        self.run_capturing_failures
            .lock()
            .expect("run_capturing_failures mutex poisoned")
            .push((matcher.to_string(), message.to_string()));
    }
}

/// A STATEFUL stand-in for the whole `gozer` CLI: a box with N boards that
/// really are handed out, really are held, and really are given back.
///
/// `FakeRunner`'s canned `set_run_capturing("gozer acquire", ..)` answers
/// every acquire with the SAME grant forever, which cannot express the one
/// fact the swap/strand tests are about: whether a second `start` finds the
/// first serve's board still held. So this models the gate itself --
///
/// * `acquire` grants the first FREE board (a fresh `lease-<n>` each time)
///   or exits `12` with gozer's own `{"granted":false,"queued":false}`
///   payload when every board is taken;
/// * `release <id>` frees the board holding that id (exit `0`) or exits `13`
///   ("no such lease"), exactly like the real CLI;
/// * `status --json` reports each chip `CLAIMED`+`who` or `FREE`;
/// * `history --json` replays the `granted`/`released` log the startup sweep
///   reconstructs lease ids from.
///
/// Everything that is NOT `gozer` (docker, tt-smi, run.py, health, HTTP)
/// delegates to an inner [`FakeRunner`], so a test still cans `docker ps`
/// output and still reads one ordered `commands()` list covering both.
///
/// One board = two chips (one BDF per ASIC, which is how gozer counts and
/// what `device::mesh_for` expects), so a single-board grant is 2 chips.
#[allow(dead_code)]
#[derive(Clone)]
pub struct FakeGozerBox {
    inner: FakeRunner,
    state: Arc<Mutex<GozerBoxState>>,
}

/// One board's identity plus who (if anyone) currently holds it.
struct FakeBoard {
    serial: String,
    /// PCI BDFs, one per ASIC -- what a grant's `chips` carries.
    chips: Vec<String>,
    /// `/dev/tenstorrent/<n>` indices, one per ASIC.
    dev_indices: Vec<u32>,
    /// `(lease_id, who)` while held; `None` when free.
    holder: Option<(String, String)>,
}

struct GozerBoxState {
    boards: Vec<FakeBoard>,
    /// Monotonic lease-id counter -- ids are never reused within one test,
    /// so an assertion can tell the first serve's lease from the second's.
    next_lease: u32,
    /// `(event, lease_id, who)` in order, as `gozer history --json` replays.
    history: Vec<(String, String, String)>,
}

#[allow(dead_code)]
impl FakeGozerBox {
    /// A box with `boards` boards, all free, whose non-gozer commands are
    /// served by a `FakeRunner::new(health_calls_before_ok)`.
    pub fn with_boards(boards: usize, health_calls_before_ok: u32) -> Self {
        let boards = (0..boards)
            .map(|b| FakeBoard {
                serial: format!("board-{b}"),
                // Two ASICs per board, numbered so no two boards collide.
                chips: vec![
                    format!("0000:{:02x}:00.0", b * 2 + 1),
                    format!("0000:{:02x}:00.0", b * 2 + 2),
                ],
                dev_indices: vec![(b * 2) as u32, (b * 2 + 1) as u32],
                holder: None,
            })
            .collect();
        FakeGozerBox {
            inner: FakeRunner::new(health_calls_before_ok),
            state: Arc::new(Mutex::new(GozerBoxState {
                boards,
                next_lease: 0,
                history: Vec::new(),
            })),
        }
    }

    /// The inner `FakeRunner`, for canning non-gozer commands
    /// (`set_run_output("docker ps", ..)`) and for reading the ordered
    /// `commands()` list -- which records gozer invocations too.
    pub fn runner(&self) -> FakeRunner {
        self.inner.clone()
    }

    /// Every command recorded so far, gozer and non-gozer alike, in order.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.inner.commands()
    }

    /// The lease ids currently held on this box, in board order. THE
    /// assertion for the strand bug: two leases here after two `start`s
    /// means a board was stranded with no container behind it.
    pub fn held_lease_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("gozer box mutex poisoned")
            .boards
            .iter()
            .filter_map(|b| b.holder.as_ref().map(|(id, _)| id.clone()))
            .collect()
    }

    /// Pre-hold a board as if a PREVIOUS agentd process had leased it and
    /// then died -- the state an `Restart=on-failure` restart wakes up to.
    /// Returns the lease id.
    pub fn preexisting_lease(&self, who: &str) -> String {
        let mut state = self.state.lock().expect("gozer box mutex poisoned");
        Self::grant_locked(&mut state, who).expect("a free board to pre-hold")
    }

    /// Grant the first free board to `who`, recording the history event.
    /// `None` when every board is taken.
    fn grant_locked(state: &mut GozerBoxState, who: &str) -> Option<String> {
        let index = state.boards.iter().position(|b| b.holder.is_none())?;
        state.next_lease += 1;
        let lease_id = format!("lease-{}", state.next_lease);
        state.boards[index].holder = Some((lease_id.clone(), who.to_string()));
        state
            .history
            .push(("granted".to_string(), lease_id.clone(), who.to_string()));
        Some(lease_id)
    }

    /// Service one `gozer <verb> ...` invocation.
    fn gozer(&self, args: &[&str]) -> CapturedOutput {
        let ok = |stdout: String| CapturedOutput {
            code: 0,
            stdout,
            stderr: String::new(),
        };
        let flag = |name: &str| {
            args.windows(2)
                .find(|w| w[0] == name)
                .map(|w| w[1].to_string())
        };

        let mut state = self.state.lock().expect("gozer box mutex poisoned");
        match args.get(1).copied() {
            Some("--version") => ok("gozer 0.1.0".to_string()),
            Some("acquire") => {
                let who = flag("--who").unwrap_or_default();
                match Self::grant_locked(&mut state, &who) {
                    Some(lease_id) => {
                        let board = state
                            .boards
                            .iter()
                            .find(|b| b.holder.as_ref().is_some_and(|(id, _)| *id == lease_id))
                            .expect("the board just granted");
                        ok(serde_json::json!({
                            "granted": true,
                            "lease_id": lease_id,
                            "units": [board.serial],
                            "chips": board.chips,
                            "dev_indices": board.dev_indices,
                            "env": {"TT_VISIBLE_DEVICES": board.chips.join(",")},
                            "expanded": true,
                        })
                        .to_string())
                    }
                    // gozer's own "no chips, and I am not queuing you" exit.
                    None => CapturedOutput {
                        code: 12,
                        stdout: r#"{"granted":false,"queued":false}"#.to_string(),
                        stderr: String::new(),
                    },
                }
            }
            Some("release") => {
                let lease_id = args.get(2).copied().unwrap_or_default();
                match state
                    .boards
                    .iter_mut()
                    .find(|b| b.holder.as_ref().is_some_and(|(id, _)| id == lease_id))
                {
                    Some(board) => {
                        let who = board.holder.take().map(|(_, who)| who).unwrap_or_default();
                        state.history.push((
                            "released".to_string(),
                            lease_id.to_string(),
                            who,
                        ));
                        ok(r#"{"released":true,"message":"released"}"#.to_string())
                    }
                    // Exit 13: "no such lease" -- release's idempotent case.
                    None => CapturedOutput {
                        code: 13,
                        stdout: r#"{"released":false,"message":"no such lease"}"#.to_string(),
                        stderr: String::new(),
                    },
                }
            }
            Some("status") => {
                let chips: Vec<serde_json::Value> = state
                    .boards
                    .iter()
                    .flat_map(|board| {
                        board.chips.iter().zip(&board.dev_indices).map(move |(bdf, index)| {
                            match &board.holder {
                                Some((_, who)) => serde_json::json!({
                                    "dev_index": index, "bdf": bdf, "board": board.serial,
                                    "state": "CLAIMED", "who": who, "reason": "serving",
                                }),
                                None => serde_json::json!({
                                    "dev_index": index, "bdf": bdf, "board": board.serial,
                                    "state": "FREE",
                                }),
                            }
                        })
                    })
                    .collect();
                ok(serde_json::json!({"grain": "board", "chips": chips, "queue": []}).to_string())
            }
            Some("history") => {
                let records: Vec<serde_json::Value> = state
                    .history
                    .iter()
                    .map(|(event, lease_id, who)| {
                        serde_json::json!({"event": event, "lease_id": lease_id, "who": who})
                    })
                    .collect();
                ok(serde_json::json!({"history": records}).to_string())
            }
            _ => CapturedOutput {
                code: 2,
                stdout: String::new(),
                stderr: format!("fake gozer: unsupported verb: {args:?}"),
            },
        }
    }
}

impl CommandRunner for FakeGozerBox {
    fn run(&self, args: &[&str]) -> Result<String> {
        self.inner.run(args)
    }

    /// Delegated rather than left to the default impl, which discards `env` --
    /// the inner `FakeRunner` is what records it (see its `child_envs`).
    fn run_in_dir_with_env(
        &self,
        dir: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String> {
        self.inner.run_in_dir_with_env(dir, args, env)
    }

    fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
        // Record FIRST (and discard the inner's canned answer) so gozer
        // invocations appear in the same ordered `commands()` list the
        // docker/tt-smi ones do -- every ordering assertion depends on that.
        let canned = self.inner.run_capturing(args)?;
        if args.first().copied() != Some("gozer") {
            return Ok(canned);
        }
        Ok(self.gozer(args))
    }

    fn health_ok(&self, url: &str) -> bool {
        self.inner.health_ok(url)
    }

    fn http_get(&self, url: &str) -> Result<String> {
        self.inner.http_get(url)
    }
}

impl CommandRunner for FakeRunner {
    fn run_in_dir_with_env(
        &self,
        dir: &str,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> Result<String> {
        self.child_envs
            .lock()
            .expect("child_envs mutex poisoned")
            .push(
                env.iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            );
        self.run_in_dir(dir, args)
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        let joined = args.join(" ");

        // Side effects first, so a hook still fires on a command that is
        // about to return a canned failure -- see `run_hooks`.
        for (matcher, action) in self
            .run_hooks
            .lock()
            .expect("run_hooks mutex poisoned")
            .iter()
        {
            if joined.contains(matcher.as_str()) {
                action();
            }
        }

        if let Some((_, message)) = self
            .run_failures
            .lock()
            .expect("run_failures mutex poisoned")
            .iter()
            .find(|(matcher, _)| joined.contains(matcher.as_str()))
        {
            return Err(anyhow::anyhow!(message.clone()));
        }

        let output = self
            .run_outputs
            .lock()
            .expect("run_outputs mutex poisoned")
            .iter()
            .find(|(matcher, _)| joined.contains(matcher.as_str()))
            .map(|(_, output)| output.clone())
            .unwrap_or_default();
        Ok(output)
    }

    fn run_capturing(&self, args: &[&str]) -> Result<CapturedOutput> {
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .push(args.iter().map(|s| s.to_string()).collect());

        let joined = args.join(" ");

        if let Some((_, message)) = self
            .run_capturing_failures
            .lock()
            .expect("run_capturing_failures mutex poisoned")
            .iter()
            .find(|(matcher, _)| joined.contains(matcher.as_str()))
        {
            return Err(anyhow::anyhow!(message.clone()));
        }

        let canned = self
            .run_capturing_outputs
            .lock()
            .expect("run_capturing_outputs mutex poisoned")
            .iter()
            .find(|(matcher, ..)| joined.contains(matcher.as_str()))
            .cloned();

        Ok(match canned {
            Some((_, code, stdout, stderr)) => CapturedOutput {
                code,
                stdout,
                stderr,
            },
            None => CapturedOutput {
                code: 0,
                stdout: String::new(),
                stderr: String::new(),
            },
        })
    }

    fn health_ok(&self, _url: &str) -> bool {
        let mut seen = self
            .health_calls_seen
            .lock()
            .expect("health mutex poisoned");
        *seen += 1;
        *seen > self.health_calls_before_ok
    }

    fn http_get(&self, _url: &str) -> Result<String> {
        // A configured sequence wins: consume one entry per call, sticking at
        // the last once exhausted (so a "ready" tail keeps answering ready).
        {
            let sequence = self
                .http_get_sequence
                .lock()
                .expect("http_get_sequence mutex poisoned");
            if !sequence.is_empty() {
                let mut seen = self
                    .http_get_calls_seen
                    .lock()
                    .expect("http_get_calls_seen mutex poisoned");
                let idx = (*seen).min(sequence.len() - 1);
                *seen += 1;
                return match &sequence[idx] {
                    Some(body) => Ok(body.clone()),
                    None => Err(anyhow::anyhow!("FakeRunner: sequenced http_get error")),
                };
            }
        }

        // Otherwise: an explicitly-set single body, else the default "ready
        // but idless" body (see `DEFAULT_HTTP_GET_BODY`).
        Ok(self
            .http_get_response
            .lock()
            .expect("http_get_response mutex poisoned")
            .clone()
            .unwrap_or_else(|| DEFAULT_HTTP_GET_BODY.to_string()))
    }
}
