//! What the box is serving: starting and stopping the model through the
//! backend, the lifecycle (`Phase`) every route reads, and reconciling the
//! stored status with what is actually running.

use super::*;

impl AppState {
    /// Cheap `Arc` clone of the serving backend, for a handler to move into
    /// `tokio::task::spawn_blocking` -- `ServingBackend::start`/`stop` are
    /// sync and must never be called directly from an async fn (see
    /// `serving/mod.rs`).
    pub(crate) fn backend(&self) -> Arc<dyn ServingBackend> {
        Arc::clone(&self.inner.backend)
    }

    /// The shared body of `POST /run`, factored out so the assistant's
    /// confirmed `run_model` action (see `crate::assistant::confirm`) brings a
    /// model up through EXACTLY the same steps and leaves `status`/`endpoint`/
    /// the mDNS advertisement in exactly the same state `/run` would.
    ///
    /// `backend.start` is sync and, for the real backends, blocks on a
    /// `reqwest::blocking` readiness poll for up to tens of minutes -- calling
    /// it directly on the async runtime would panic (blocking calls are
    /// forbidden inside a Tokio worker thread). It runs via
    /// `tokio::task::spawn_blocking` on a cloned `Arc<dyn ServingBackend>`
    /// handle, and `set_serving` happens only *after* it returns -- no mutex
    /// guard is ever held across the `.await`.
    ///
    /// A `spawn_blocking` join failure (the start task panicked) is folded
    /// into the returned `anyhow::Error`; callers that care about contention
    /// pass the error through `contention_aware_error`, which still finds a
    /// `gozer::Contention` anywhere in the chain.
    ///
    /// Refused with a [`StartInFlight`] (a `409` through `contention_aware_error`)
    /// while another start or a stop is in flight; see [`AppState::claim_start`].
    pub(crate) async fn start_serving(&self, model: &str, force: bool) -> anyhow::Result<Endpoint> {
        let prev = self.claim_start(model)?;
        self.start_serving_claimed(model, force, prev).await
    }

    /// [`AppState::start_serving`] for a caller that already holds the
    /// [`AppState::claim_start`] claim (`prev` is what it replaced).
    pub(crate) async fn start_serving_claimed(
        &self,
        model: &str,
        force: bool,
        prev: Lifecycle,
    ) -> anyhow::Result<Endpoint> {
        // Run the bring-up on its own task so it finishes -- and records its
        // outcome -- even when the caller goes away. A phone's `POST /run` dies
        // when the app is backgrounded minutes into a load; if this future were
        // dropped with it, the model would come up while the agent still said
        // `idle`, and a failure would be visible nowhere but the log.
        let me = self.clone();
        let model = model.to_string();
        tokio::spawn(async move {
            // An ordinary start takes the whole box: leave co-serving first.
            crate::coserve::leave_split(&me).await;
            let result = me.start_serving_tracked(&model, force).await;
            if result.is_err() {
                // Refused before the backend was asked (chips held): nothing
                // started, so the claim goes back. A real failure already moved
                // the lifecycle to `failed`, which this leaves alone.
                me.release_start(prev, &model);
            }
            result
        })
        .await
        .map_err(|join_err| anyhow::anyhow!("run task panicked: {join_err}"))?
    }

    /// Start `model` on the main backend WITHOUT leaving split mode -- the
    /// chat half of a co-serve (`crate::coserve`), pinned by the caller.
    pub(crate) async fn start_serving_placed(&self, model: &str) -> anyhow::Result<Endpoint> {
        let me = self.clone();
        let model = model.to_string();
        tokio::spawn(async move { me.start_serving_tracked(&model, false).await })
            .await
            .map_err(|join_err| anyhow::anyhow!("run task panicked: {join_err}"))?
    }

    async fn start_serving_tracked(&self, model: &str, force: bool) -> anyhow::Result<Endpoint> {
        if let Some(holder) = self.chips_holder() {
            anyhow::bail!("{holder}");
        }
        self.set_lifecycle(Phase::Starting, Some(model.to_string()), None, None);
        let started = Instant::now();
        let result = self.start_serving_inner(model, force).await;
        if result.is_ok() {
            self.load_times().record(model, started.elapsed().as_secs());
        }
        match &result {
            Ok(endpoint) => self.set_lifecycle(
                Phase::Serving,
                Some(model.to_string()),
                Some(endpoint.model.clone()),
                None,
            ),
            Err(e) => {
                // A start that failed because someone stopped it is a stop, not
                // a failure worth showing.
                let stopping = self.lifecycle().phase == Phase::Stopping;
                if !stopping {
                    crate::log!(error, "start of {model} failed: {e:#}");
                    self.set_lifecycle(
                        Phase::Failed,
                        Some(model.to_string()),
                        None,
                        Some(format!("{e:#}")),
                    );
                }
            }
        }
        result
    }

    async fn start_serving_inner(&self, model: &str, force: bool) -> anyhow::Result<Endpoint> {
        let backend = self.backend();
        let model = model.to_string();

        // `start`/`start_forced` rather than one call with a bool, so the
        // unforced path is exactly the call every pre-`--force` caller (and
        // every argv pin) already makes.
        let endpoint = tokio::task::spawn_blocking(move || {
            if force {
                backend.start_forced(&model)
            } else {
                backend.start(&model)
            }
        })
        .await
        .map_err(|join_err| anyhow::anyhow!("run task panicked: {join_err}"))??;

        self.set_serving(endpoint.clone());
        Ok(endpoint)
    }

    /// The shared body of `POST /stop`, factored out for the same reason as
    /// [`AppState::start_serving`]: the assistant's confirmed `stop_model`
    /// (and `reset_chips`, which stops first) must stop serving exactly the
    /// way `/stop` does.
    ///
    /// This ALWAYS calls `backend.stop()`, even when `current_model()` is
    /// `None`. A `/run` brings a model up on a `spawn_blocking` thread and
    /// `AppState`'s status stays `Idle` until `start` returns -- so a stop
    /// that arrives mid-bring-up sees `None` here, yet must still reach the
    /// backend to trip its cancel flag (`RunPyBackend::stop`) and abort the
    /// in-flight serve rather than leaving it grinding to the health-poll
    /// ceiling. Every `ServingBackend` is contractually idempotent here (see
    /// the doc on `ServingBackend::stop`), so calling it while genuinely idle
    /// is harmless.
    pub(crate) async fn stop_serving(&self) -> anyhow::Result<()> {
        // Breadcrumb before we stop -- distinguishes stopping a live model
        // from a mid-bring-up abort (status is still `Idle` in the latter).
        eprintln!(
            "tt-station-agentd: stop requested (was {})",
            match self.current_model() {
                Some(m) => format!("serving {m}"),
                None => "idle/starting".to_string(),
            }
        );

        // Model name is only used for logging by backends that track one;
        // `RunPyBackend::stop` ignores the argument entirely. When nothing is
        // serving there's no name to pass, so hand it an empty placeholder.
        // Stopping takes the whole box back: the companion goes too.
        crate::coserve::leave_split(self).await;
        // A model Orchard brought up and the owner tapped Serve on.
        crate::orchard::stop_harvest(self).await;
        // Stopping from here on (a community package's stop alone can take minutes).
        let was = self.lifecycle().model;
        self.set_lifecycle(Phase::Stopping, was, None, None);
        // A community package: its container outlives `tt-model serve`.
        crate::community::stop_package(self).await;

        let model = self.current_model().unwrap_or_default();
        let backend = self.backend();

        let stopped: anyhow::Result<()> =
            match tokio::task::spawn_blocking(move || backend.stop(&model)).await {
                Ok(result) => result,
                Err(join_err) => Err(anyhow::anyhow!("stop task panicked: {join_err}")),
            };
        if let Err(e) = &stopped {
            // Leave the lifecycle saying what's true: we tried and it didn't stop.
            let model = self.lifecycle().model;
            self.set_lifecycle(
                Phase::Failed,
                model,
                None,
                Some(format!("stop failed: {e:#}")),
            );
        }
        stopped?;

        self.set_idle();
        self.set_lifecycle(Phase::Idle, None, None, None);
        Ok(())
    }

    /// The current [`Lifecycle`].
    pub fn lifecycle(&self) -> Lifecycle {
        self.inner
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned")
            .clone()
    }

    pub(crate) fn set_lifecycle(
        &self,
        phase: Phase,
        model: Option<String>,
        served_id: Option<String>,
        error: Option<String>,
    ) {
        *self
            .inner
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned") = Lifecycle {
            phase,
            model,
            served_id,
            since: unix_now(),
            error,
            adopted: false,
        };
    }

    /// Claim the lifecycle for a start of `model`: check nothing else is
    /// starting or stopping and set `Starting` under ONE hold of the lock, so
    /// two clients asking at once can't both pass the check (the start itself
    /// only gets to `start_serving_tracked` after awaits -- `check_servable`,
    /// `coserve::leave_split` -- that used to leave that window open). Returns
    /// the lifecycle it replaced, for [`AppState::release_start`] when the start
    /// never gets going.
    pub(crate) fn claim_start(&self, model: &str) -> Result<Lifecycle, StartInFlight> {
        let mut life = self
            .inner
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned");
        match life.phase {
            Phase::Starting => {
                return Err(StartInFlight(format!(
                    "{} is already starting. Wait for it, or stop it first.",
                    life.model.clone().unwrap_or_else(|| "A model".to_string())
                )))
            }
            Phase::Stopping => {
                return Err(StartInFlight(
                    "The box is stopping a model. Try again in a moment.".to_string(),
                ))
            }
            _ => {}
        }
        let prev = life.clone();
        *life = Lifecycle {
            phase: Phase::Starting,
            model: Some(model.to_string()),
            served_id: None,
            since: unix_now(),
            error: None,
            adopted: false,
        };
        Ok(prev)
    }

    /// Hand back a [`AppState::claim_start`] claim whose start never got going
    /// (refused before the backend was asked): restore `prev`, unless something
    /// has moved the lifecycle on since.
    pub(crate) fn release_start(&self, prev: Lifecycle, model: &str) {
        let mut life = self
            .inner
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned");
        if life.phase == Phase::Starting && life.model.as_deref() == Some(model) {
            *life = prev;
        }
    }

    /// Why `model` can't be started right now from the tt catalog's view (not
    /// downloaded, partly downloaded, not for this mesh), or `None` if it can --
    /// including when there's no tt CLI or tt doesn't know the model. Blocking.
    pub(crate) fn not_servable_reason_blocking(&self, model: &str) -> Option<String> {
        let tt = self.tt_cli()?;
        let m = tt
            .catalog(self.device_mesh(), false)
            .ok()?
            .find(model)?
            .clone();
        if !m.supported {
            return Some(format!("{} doesn't run on this box.", m.name));
        }
        match m.weights {
            crate::ttcli::Weights::Complete | crate::ttcli::Weights::Container => None,
            crate::ttcli::Weights::Partial => Some(format!(
                "{} is only partly downloaded; finish the download (Models tab) before starting it.",
                m.name
            )),
            crate::ttcli::Weights::Missing => Some(format!(
                "{} isn't downloaded yet; download it (Models tab) before starting it.",
                m.name
            )),
        }
    }

    /// Adopt a serve this agent didn't start (one already up on its serving port
    /// at startup -- e.g. the agent was restarted under a loaded model), so a
    /// restart doesn't cost a multi-minute reload.
    pub fn adopt_serving(&self, endpoint: Endpoint) {
        eprintln!(
            "tt-station-agentd: adopting {} already serving at {}",
            endpoint.model, endpoint.base_url
        );
        let id = endpoint.model.clone();
        self.set_serving(endpoint);
        self.set_lifecycle(Phase::Serving, None, Some(id), None);
        self.inner
            .lifecycle
            .lock()
            .expect("lifecycle mutex poisoned")
            .adopted = true;
    }

    /// The served model's catalog type (`llm`, `image`, ...) when the tt CLI
    /// knows it, for [`model_kind`].
    pub(crate) fn served_kind_blocking(&self) -> &'static str {
        let life = self.lifecycle();
        let model_type = match (&life.model, self.tt_cli()) {
            (Some(name), Some(tt)) => tt
                .catalog(self.device_mesh(), false)
                .ok()
                .and_then(|c| c.find(name).map(|m| m.model_type.clone())),
            _ => None,
        };
        let served = life.served_id.or_else(|| self.current_model());
        model_kind(model_type.as_deref(), served.as_deref())
    }

    /// Snapshot the current serving status (locks briefly, then clones out).
    pub fn status(&self) -> ServingStatus {
        self.inner
            .status
            .lock()
            .expect("status mutex poisoned")
            .clone()
    }

    /// The current serving status, RECONCILED against docker reality, and
    /// self-healing the stored state when it's stale.
    ///
    /// `status` is only the agent's last serving *intent* -- flipped to
    /// `Serving` on `/run`, back to `Idle` only on the agent's own `/stop`. A
    /// model stopped out of band (a manual `docker stop`, a crash) never runs
    /// through `/stop`, so the stored status gets stuck reporting `Serving`
    /// while nothing is actually up. This probes docker (`discover_serving`,
    /// off the async runtime like `/serving`) and applies
    /// [`reconcile_status`]: if the agent's own serve is gone, it calls
    /// `set_idle()` so `/status`, `/endpoint`, AND the mDNS advertisement all
    /// stop lying, then returns the corrected status.
    ///
    /// The docker probe (delegated to the backend) only runs when the stored
    /// status is `Serving` (the only state that can be stale-positive); an
    /// `Idle` status returns immediately without touching the backend, keeping
    /// the common idle poll cheap. The probe is blocking I/O, so it runs on
    /// `spawn_blocking` (like `/serving`).
    pub(crate) async fn reconciled_status(&self) -> ServingStatus {
        let stored = self.status();
        if !matches!(stored, ServingStatus::Serving(_)) {
            return stored;
        }
        // Reused for a moment while the serve state is unchanged: every open
        // app polls this (see `discovery::DISCOVERY_TTL`).
        static RECONCILED: crate::serving::discovery::Recent<ServingStatus> =
            crate::serving::discovery::Recent::new();
        let key = self.serve_key();
        let reconciled = if let Some(r) = RECONCILED.get(&key) {
            r
        } else if self.lifecycle().adopted {
            // An adopted serve was never started by the backend, so the
            // backend's own record says idle; believe the serving port instead.
            let (host, port, probe) = (
                self.serving_host().to_string(),
                self.serving_port(),
                stored.clone(),
            );
            let k = key.clone();
            let alive = tokio::task::spawn_blocking(move || {
                let runner = RealCommandRunner;
                discover_serving_cached(&runner, &k, &host, port, &probe)
                    .iter()
                    .any(|e| e.host_port == port)
            })
            .await
            .unwrap_or(true);
            // Not one of the container images discovery knows (a community
            // tt-model package, Orchard's package, native vLLM with no
            // container): the port answering with a model is enough.
            let answering = alive || {
                let url = format!("http://127.0.0.1:{}/v1/models", self.serving_port());
                match reqwest::Client::new()
                    .get(url)
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => r
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .is_some_and(|v| v["data"].as_array().is_some_and(|d| !d.is_empty())),
                    _ => false,
                }
            };
            if answering {
                stored.clone()
            } else {
                ServingStatus::Idle
            }
        } else {
            let backend = self.backend();
            tokio::task::spawn_blocking(move || backend.reconciled_status())
                .await
                .ok()
                .and_then(Result::ok)
                // A join panic or backend error leaves the stored status as-is
                // (never worse than today's behavior).
                .unwrap_or_else(|| stored.clone())
        };
        RECONCILED.put(&key, reconciled.clone());
        if matches!(reconciled, ServingStatus::Idle) {
            // Stale-positive: the serve is gone. Heal the stored state
            // (clears the endpoint + re-advertises Idle over mDNS).
            self.set_idle();
            if self.lifecycle().phase == Phase::Serving {
                self.set_lifecycle(Phase::Idle, None, None, None);
            }
        }
        reconciled
    }

    /// The agent's own serve state in one string (status and lifecycle), for
    /// keying what is cached about it: any start, stop or phase change gives a
    /// new key, so nothing cached before it is reused after.
    pub(crate) fn serve_key(&self) -> String {
        let l = self.lifecycle();
        format!(
            "{:?}|{:?}|{}|{:?}|{:?}|{}",
            self.status(),
            l.phase,
            l.since,
            l.model,
            l.served_id,
            l.adopted
        )
    }

    /// Snapshot the currently-serving `Endpoint`, or `None` if idle.
    pub(crate) fn endpoint(&self) -> Option<Endpoint> {
        self.inner
            .endpoint
            .lock()
            .expect("endpoint mutex poisoned")
            .clone()
    }

    /// Which model is currently serving, read off `status` -- `None` when
    /// idle. Used by `/stop` to know which model to tell the backend to
    /// stop, without needing a separate "current model" field that could
    /// drift from `status`.
    pub(crate) fn current_model(&self) -> Option<String> {
        match &*self.inner.status.lock().expect("status mutex poisoned") {
            ServingStatus::Serving(model) => Some(model.clone()),
            ServingStatus::Idle => None,
        }
    }

    /// Record a successful `/run`: flip `status` to `Serving(endpoint.model)`
    /// and remember `endpoint` for `/endpoint` to hand back later. Both
    /// fields are updated while holding both locks so a concurrent
    /// `/status` or `/endpoint` request never observes one updated without
    /// the other.
    ///
    /// The new status is then re-published via `advertise_status` -- but
    /// only after both locks above are dropped (the block ends first), so
    /// the mDNS re-registration (real I/O in the `main.rs` impl) never runs
    /// while either mutex is held.
    pub(crate) fn set_serving(&self, endpoint: Endpoint) {
        let new_status = ServingStatus::Serving(endpoint.model.clone());
        {
            let mut status = self.inner.status.lock().expect("status mutex poisoned");
            let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
            *status = new_status.clone();
            *stored_endpoint = Some(endpoint);
        }
        self.advertise_status(&new_status);
    }

    /// Record a successful `/stop` (or a no-op `/stop` while already idle):
    /// `status` goes back to `Idle` and any stored `Endpoint` is cleared,
    /// then (same lock-drop-then-advertise discipline as `set_serving`) the
    /// `Idle` status is re-published.
    pub(crate) fn set_idle(&self) {
        {
            let mut status = self.inner.status.lock().expect("status mutex poisoned");
            let mut stored_endpoint = self.inner.endpoint.lock().expect("endpoint mutex poisoned");
            *status = ServingStatus::Idle;
            *stored_endpoint = None;
        }
        self.advertise_status(&ServingStatus::Idle);
    }

    /// Re-publish `status` via the attached `StatusAdvertiser`, if any.
    /// No-op when `advertiser` is `None` (every test that doesn't care about
    /// mDNS re-publishing, plus any agent run with persistence/advertising
    /// disabled).
    fn advertise_status(&self, status: &ServingStatus) {
        if let Some(advertiser) = &self.inner.advertiser {
            advertiser.advertise_status(status);
        }
    }
}
