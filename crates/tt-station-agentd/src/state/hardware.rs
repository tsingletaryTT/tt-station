// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The box's hardware: the shared `tt-smi` snapshot behind `/telemetry`,
//! gozer's lease view, and the configured power commands.

use super::*;

impl AppState {
    /// `tt-smi` binary the `/telemetry` stream runs (see `with_telemetry_config`).
    pub(crate) fn tt_smi_bin(&self) -> &str {
        &self.inner.tt_smi_bin
    }

    /// Interval (ms) between `/telemetry` snapshots (see `with_telemetry_config`).
    pub(crate) fn telemetry_interval_ms(&self) -> u64 {
        self.inner.telemetry_interval_ms
    }

    /// Raw `tt-smi -s` snapshot, shared across all `/telemetry` clients with a
    /// one-interval TTL. The first caller in each interval runs `tt-smi`; callers
    /// that arrive while that result is still fresh reuse it — so N concurrent
    /// clients cause ~one `tt-smi` per interval instead of one each. The lock is
    /// held across the shell-out on purpose: a thundering herd of ticks blocks on
    /// it, and by the time they acquire it the fresh result is already cached, so
    /// only one `tt-smi` actually runs. Errors are NOT cached — the next tick
    /// retries — and while an entry is stale the previous frame is replaced, never
    /// served past its interval.
    pub(crate) async fn cached_snapshot(&self) -> anyhow::Result<String> {
        if self.chips_resetting() {
            anyhow::bail!("The chips are being reset; readings come back in a minute.");
        }
        if matches!(self.lifecycle().phase, Phase::Stopping) {
            anyhow::bail!("A model is stopping and its chips are being reset; readings come back in a minute.");
        }
        let ttl = Duration::from_millis(self.telemetry_interval_ms().max(1));
        let mut cache = self.inner.tt_smi_cache.lock().await;
        if let Some((at, json)) = cache.as_ref() {
            if at.elapsed() < ttl {
                return Ok(json.clone());
            }
        }
        let json = match collect_snapshot(self.tt_smi_bin().to_string()).await {
            Ok(json) => json,
            // A model start resets and opens the chips; a reading that fails
            // meanwhile is that, not a broken tt-smi.
            Err(e)
                if matches!(self.lifecycle().phase, Phase::Starting) || self.chips_resetting() =>
            {
                eprintln!("tt-station-agentd: tt-smi -s during a model start: {e:#}");
                anyhow::bail!("A model is starting on the chips; readings come back in a minute.");
            }
            Err(e) => return Err(e),
        };
        *cache = Some((Instant::now(), json.clone()));
        Ok(json)
    }

    /// The last raw `tt-smi -s` snapshot `cached_snapshot` stored, IF it is
    /// younger than `max_age` -- and never anything more. Unlike
    /// `cached_snapshot`, this NEVER shells out, so the assistant's
    /// `box_status` tool tries it first: when a `/telemetry` client (the Mac's
    /// device strip, tt-toplike) has already paid for a recent snapshot, a chat
    /// question reuses it for free. Only on a miss does `box_status` fall back
    /// to `cached_snapshot` (one deduplicated `tt-smi -s`, ~0.2s on a P300x2
    /// box with a model serving -- the same call `/telemetry` makes every
    /// interval while a viewer is connected).
    ///
    /// `try_lock` rather than `lock().await`: if a `/telemetry` tick is
    /// mid-shell-out holding the lock, treating "busy" as a miss lets the
    /// caller decide whether to wait (via `cached_snapshot`) instead of
    /// blocking here.
    pub(crate) fn peek_snapshot(&self, max_age: Duration) -> Option<String> {
        let cache = self.inner.tt_smi_cache.try_lock().ok()?;
        let (at, json) = cache.as_ref()?;
        (at.elapsed() < max_age).then(|| json.clone())
    }

    /// The cached [`crate::gozer::LeaseSnapshot`] behind `GET /leases` and
    /// `GET /status`'s `leasing` field -- see `Inner::gozer_leases_cache`'s
    /// doc comment for the caching rationale (mirrors `cached_snapshot`'s
    /// shape exactly, one level up: TTL'd cache, hold the lock across the
    /// `spawn_blocking` shell-out so a thundering herd collapses to one
    /// `gozer status` run).
    ///
    /// Returns [`crate::gozer::LeaseSnapshot::default`] (empty leases, 0
    /// boards, 0 max_concurrent) whenever gozer isn't installed on this box
    /// -- callers combine this with `self.gozer().is_some()` to distinguish
    /// "gozer absent" from "gozer present and genuinely reports zero
    /// boards," per `LeaseList`'s `available` marker. Also degrades to that
    /// same default (never a panic, never propagated as an error) if the
    /// shell-out itself fails, exits non-zero, or the `spawn_blocking` task
    /// panics -- `snapshot_leases` is already best-effort for all of those;
    /// this just extends the same contract through the cache and the join.
    pub(crate) async fn gozer_snapshot(&self) -> crate::gozer::LeaseSnapshot {
        let Some(capability) = self.inner.gozer.clone() else {
            return crate::gozer::LeaseSnapshot::default();
        };

        let mut cache = self.inner.gozer_leases_cache.lock().await;
        if let Some((at, snapshot)) = cache.as_ref() {
            if at.elapsed() < GOZER_LEASE_CACHE_TTL {
                return snapshot.clone();
            }
        }

        let snapshot = tokio::task::spawn_blocking(move || {
            let runner = RealCommandRunner;
            crate::gozer::snapshot_leases(&runner, &capability).unwrap_or_default()
        })
        .await
        .unwrap_or_default();
        *cache = Some((Instant::now(), snapshot.clone()));
        snapshot
    }

    /// Run the configured command for `action`, blocking (call under
    /// `spawn_blocking` -- this shells out, same rule as `ServingBackend::
    /// start`/`stop`). Machine ops (suspend/reboot/shutdown) best-effort
    /// stop any serving container first so a model isn't hard-killed by the
    /// machine going down; a stop failure is logged but non-fatal here --
    /// we're taking the box down regardless, so refusing the power action
    /// over a stop that didn't work would just strand the operator. Does
    /// NOT touch tokens/SSH/status: unlike `POST /reset` (which unpairs),
    /// every power action here -- including `reset-chips` -- preserves
    /// pairing.
    ///
    /// **`reset-chips` REFUSES while another tenant holds a lease, unless
    /// `force`.** It runs `power_reset_chips_cmd` -- `["tt-smi", "-r"]` by
    /// default, a WHOLE-BOX reset -- and, because `is_machine_op()` is false
    /// for it, it skips the backend entirely: no serving stop, no lease
    /// release, and (before this) no scoping of any kind. It is the second of
    /// the two whole-box reset paths (`POST /reset` is the other) and gets the
    /// identical treatment, through the identical `ForeignLeases::resolve`
    /// call: a lease held by anyone other than this agent's own session
    /// refuses the call and names the holder (as does a lease state that could
    /// not be determined at all), and `force` -- the operator's
    /// `tt-station power reset-chips --force` -- overrides either refusal and logs what
    /// it stepped on. The other three actions are untouched:
    /// suspend/reboot/shutdown take the whole machine down, where a lease
    /// check would be theatre, so `force` is meaningless for them and ignored.
    ///
    /// Ownership is matched against `Inner::serving_port`, which `main.rs`
    /// fills from the same `--serving-port` it gives the backend as its
    /// `service_port`/`host_port` -- so the port in a lease this agent took
    /// and the port checked here are the same value by construction, not by
    /// coincidence.
    ///
    /// Shells out through `RealCommandRunner`, like `gozer_snapshot` and
    /// `get_serving` -- NOT through the backend's injected runner, since the
    /// power path deliberately doesn't go near the backend.
    pub fn run_power_command(
        &self,
        action: crate::power::PowerAction,
        force: bool,
    ) -> anyhow::Result<()> {
        self.run_power_command_noted(action, force).map(|_| ())
    }

    /// [`AppState::run_power_command`], also handing back what a `force`d
    /// `reset-chips` overrode (the holders, for the activity log) -- `None`
    /// when nothing was overridden.
    ///
    /// **With gozer, `reset-chips` runs under a short whole-box lease**
    /// ([`crate::gozer::take_reset_lease`]): `gozer acquire --chips all` as
    /// `tt-station:<port>:reset-chips`, the configured command, then
    /// `gozer release --no-reset` (the command just reset them). Other gozer
    /// users see the chips held for the length of the reset rather than FREE
    /// chips being reset under them. When the lease can't be taken because
    /// the chips are this agent's own (a serve) or untracked work, the reset
    /// runs unleased as before; a forced reset past someone else's lease is
    /// unleased too (a lease can't be taken over theirs) and is logged.
    pub fn run_power_command_noted(
        &self,
        action: crate::power::PowerAction,
        force: bool,
    ) -> anyhow::Result<Option<String>> {
        use crate::power::PowerAction;

        // A board reset under this box's own model -- starting, serving or
        // stopping -- pulls the chips out from under it: the owner stops it
        // first, `force` or not (`force` is for other tenants' leases). Every
        // caller that resets on the way to a start stops the model first.
        if matches!(action, PowerAction::ResetChips)
            && matches!(
                self.lifecycle().phase,
                Phase::Starting | Phase::Serving | Phase::Stopping
            )
        {
            return Err(anyhow::Error::new(crate::gozer::Contention::new(
                "refusing reset-chips: this box's own model is running on the chips -- stop it \
                 first (`tt-station stop`)",
            )));
        }
        let mut lease: Option<(crate::gozer::Capability, crate::gozer::Grant)> = None;
        let mut overrode = None;
        if matches!(action, PowerAction::ResetChips) {
            if let Some(capability) = &self.inner.gozer {
                let runner = RealCommandRunner;
                // A lease whose owner is gone (a bring-up that was stopped or
                // deleted mid-run) is STALE: gozer itself says to clear it with
                // `gozer reconcile`, which only drops dead tenants. Do that
                // first, so a leftover can't block every later start.
                let stale = crate::gozer::snapshot_leases(&runner, capability)
                    .is_some_and(|s| s.leases.iter().any(|l| l.state == "STALE"));
                if stale {
                    match runner.run(&[capability.path.as_str(), "reconcile"]) {
                        Ok(_) => eprintln!("tt-station-agentd: reset-chips: cleared stale chip leases (gozer reconcile)"),
                        Err(e) => eprintln!("tt-station-agentd: reset-chips: gozer reconcile failed: {e}"),
                    }
                }
                // This box's own model holds chips (its serve lease): a
                // reset would pull them out from under it -- stop it first,
                // `force` or not.
                if let Some(own) = crate::gozer::snapshot_leases(&runner, capability)
                    .as_ref()
                    .and_then(|s| crate::gozer::own_serve_lease(s, self.inner.serving_port))
                {
                    return Err(anyhow::Error::new(crate::gozer::Contention::new(format!(
                        "refusing reset-chips: {own}"
                    ))));
                }
                // The SAME refusal `RunPyBackend::reset_forcing` makes, made
                // by the same `ForeignLeases::resolve` so the two paths cannot
                // drift: refuse when someone else holds chips AND when that
                // could not be determined at all -- unless the operator forced
                // it, in which case proceed and log the override, which is the
                // only trace a deliberate stomp leaves behind.
                match crate::gozer::take_reset_lease(
                    &runner,
                    capability,
                    self.inner.serving_port,
                    force,
                    "TT Station: resetting the chips",
                ) {
                    Ok(crate::gozer::ResetLease::Held(grant)) => {
                        lease = Some((capability.clone(), grant));
                    }
                    Ok(crate::gozer::ResetLease::Unleased(why)) => {
                        eprintln!("tt-station-agentd: reset-chips: {why}");
                    }
                    Ok(crate::gozer::ResetLease::Forced {
                        note,
                        overrode: what,
                    }) => {
                        eprintln!("tt-station-agentd: reset-chips: {note}");
                        overrode = Some(what);
                    }
                    Err(reason) => {
                        return Err(anyhow::Error::new(crate::gozer::Contention::new(format!(
                            "refusing reset-chips: it would run a WHOLE-BOX board reset -- \
                             {reason}"
                        ))));
                    }
                }
            }
        }

        if action.is_machine_op() {
            // On purpose: the next boot mustn't start the model again
            // (`crate::boot`). Suspend too -- a resume isn't a boot, but the
            // model is stopped either way.
            crate::boot::note_owner_stop(&self.config_dir());
            if let Some(ep) = self.endpoint() {
                if let Err(e) = self.backend().stop(&ep.model) {
                    eprintln!(
                        "power: best-effort stop of '{}' before {action:?} failed (continuing): {e}",
                        ep.model
                    );
                }
            }
        }

        let ran = self.run_configured_power_command(action);
        // Give the reset's lease back however the reset went -- without a
        // second reset: the command just did that.
        if let Some((capability, grant)) = lease {
            if let Err(e) =
                crate::gozer::release_no_reset(&RealCommandRunner, &capability, &grant.lease_id)
            {
                eprintln!(
                    "tt-station-agentd: reset-chips: couldn't release the reset's lease '{}': {e:#}",
                    grant.lease_id
                );
            }
        }
        ran.map(|()| overrode)
    }

    /// Run the configured command for `action` and nothing else: no lease, no
    /// refusal, no serving stop. A chip reset pauses tt-smi readings while it
    /// runs. Lab calls this for its reset, under its own lease.
    pub(crate) fn run_configured_power_command(
        &self,
        action: crate::power::PowerAction,
    ) -> anyhow::Result<()> {
        use crate::power::PowerAction;
        let cmd = match action {
            PowerAction::ResetChips => &self.inner.power_reset_chips_cmd,
            PowerAction::Suspend => &self.inner.power_suspend_cmd,
            PowerAction::Reboot => &self.inner.power_reboot_cmd,
            PowerAction::Shutdown => &self.inner.power_shutdown_cmd,
        };
        let (bin, args) = cmd
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("empty power command configured for {action:?}"))?;
        // Pause tt-smi readings for the length of a reset (resumed on every way out).
        let _paused = matches!(action, PowerAction::ResetChips).then(|| self.pause_readings());
        let status = std::process::Command::new(bin)
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to spawn power command {cmd:?}: {e}"))?;
        if !status.success() {
            anyhow::bail!("power command {cmd:?} exited with {status}");
        }
        Ok(())
    }
}
