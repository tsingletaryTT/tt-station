//! Integration tests for the agent control routes (Task 10): `POST /run`,
//! `POST /stop`, `GET /endpoint`, all gated behind bearer auth on top of the
//! pairing tokens Task 7 mints.
//!
//! Like `tests/pairing.rs` and `tests/serving.rs`, these exercise the real
//! axum `Router` end to end (ephemeral port, real HTTP), with the
//! `ServingBackend` swapped for a `DockerBackend` wired to the shared
//! `FakeRunner` test double (see `tests/support/mod.rs`) so no real `docker`
//! binary or HTTP health probe is ever touched.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use libttstation::model::ServingStatus;
use tt_station_agentd::routes::{app, AppState, StatusAdvertiser};
use tt_station_agentd::serving::docker::{DockerBackend, DockerConfig};
use tt_station_agentd::serving::ServingBackend;

mod support;
use support::FakeRunner;

/// Build an `AppState` backed by a `DockerBackend` wired to a fresh
/// `FakeRunner` that reports healthy on the very first probe (so `/run`
/// resolves immediately rather than polling) -- the same backend shape
/// every test in this file needs, factored out so `spawn` and the
/// `StatusAdvertiser` tests (which also need to attach a fake advertiser
/// before the state is ever cloned into a router) can both build one.
fn fresh_state() -> AppState {
    let runner = FakeRunner::new(0);
    let config = DockerConfig {
        image: "some/image:tag".to_string(),
        host: "127.0.0.1".to_string(),
        host_port: 8080,
        ..Default::default()
    };
    let backend: Arc<dyn ServingBackend> = Arc::new(
        DockerBackend::new(config, Box::new(runner)).with_health_poll(5, Duration::from_millis(1)),
    );

    AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend)
}

/// Bind `state`'s router to an ephemeral port and serve it in the
/// background, handing back the base URL. Shared tail end of `spawn` and
/// the `StatusAdvertiser` tests below.
async fn serve(state: AppState) -> String {
    let router = app(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind ephemeral port");
    let addr = listener.local_addr().expect("failed to read local addr");

    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    format!("http://{addr}")
}

/// Spin up the real router on an ephemeral port, backed by a `DockerBackend`
/// wired to a fresh `FakeRunner` that reports healthy on the very first
/// probe (so `/run` resolves immediately rather than polling). Returns the
/// `AppState` (so tests can read `/status` state Rust-side if ever needed)
/// and the base URL.
async fn spawn() -> (AppState, String) {
    let state = fresh_state();
    let base = serve(state.clone()).await;
    (state, base)
}

/// Pair against a freshly-spawned agent (the same two-step dance
/// `tests/pairing.rs` exercises) and return a valid bearer token.
async fn pair(client: &reqwest::Client, state: &AppState, base: &str) -> String {
    let init_resp: serde_json::Value = client
        .post(format!("{base}/pair/init"))
        .send()
        .await
        .expect("POST /pair/init failed")
        .json()
        .await
        .expect("init response was not valid JSON");
    let pair_id = init_resp["pair_id"]
        .as_str()
        .expect("pair_id missing")
        .to_string();

    let code = state
        .last_code(&pair_id)
        .expect("expected a pending code for the freshly-issued pair_id");

    let complete_resp: serde_json::Value = client
        .post(format!("{base}/pair/complete"))
        .json(&serde_json::json!({ "pair_id": pair_id, "code": code }))
        .send()
        .await
        .expect("POST /pair/complete failed")
        .json()
        .await
        .expect("complete response was not valid JSON");

    complete_resp["token"]
        .as_str()
        .expect("token missing")
        .to_string()
}

/// `POST /run` with a valid bearer token should return an `endpoint` whose
/// `model` matches the request, and `/status` should flip to
/// `serving:<model>`.
#[tokio::test]
async fn run_with_valid_bearer_returns_endpoint_and_flips_status() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = run_resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["endpoint"]["model"], "llama3");
    assert_eq!(body["endpoint"]["requires_key"], false);
    assert!(body["endpoint"]["base_url"]
        .as_str()
        .expect("base_url missing")
        .contains("8080"));

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "serving:llama3");
}

/// `POST /run` without an `Authorization` header must be rejected with 401,
/// and must not start anything.
#[tokio::test]
async fn run_without_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let run_resp = client
        .post(format!("{base}/run"))
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `POST /run` with a garbage bearer token (never minted by `/pair/complete`)
/// must also be rejected with 401.
#[tokio::test]
async fn run_with_invalid_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth("not-a-real-token")
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

/// `GET /endpoint` while idle (nothing ever `/run`) must return 409, even
/// with a valid bearer token.
#[tokio::test]
async fn endpoint_while_idle_returns_409() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
}

/// `GET /endpoint` after a successful `/run` should return the same
/// `base_url` `/run` handed back.
#[tokio::test]
async fn endpoint_after_run_returns_base_url() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    let resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let body: serde_json::Value = resp.json().await.expect("response was not valid JSON");
    assert_eq!(body["model"], "llama3");
    assert!(body["base_url"]
        .as_str()
        .expect("base_url missing")
        .contains("8080"));
}

/// `POST /stop` after a `/run` should flip `/status` back to `idle`, and a
/// subsequent `/endpoint` should 409 again.
#[tokio::test]
async fn stop_flips_status_back_to_idle() {
    let client = reqwest::Client::new();
    let (state, base) = spawn().await;
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    let stop_resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /stop failed");
    assert_eq!(stop_resp.status(), reqwest::StatusCode::OK);

    let status_resp: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .expect("GET /status failed")
        .json()
        .await
        .expect("status response was not valid JSON");
    assert_eq!(status_resp["status"], "idle");

    let endpoint_resp = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("GET /endpoint failed");
    assert_eq!(endpoint_resp.status(), reqwest::StatusCode::CONFLICT);
}

/// `POST /stop` without a bearer token must be rejected with 401.
#[tokio::test]
async fn stop_without_bearer_returns_401() {
    let client = reqwest::Client::new();
    let (_state, base) = spawn().await;

    let resp = client
        .post(format!("{base}/stop"))
        .send()
        .await
        .expect("POST /stop failed");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------
// StatusAdvertiser seam (re-publishing mDNS `status` on run/stop)
// ---------------------------------------------------------------------

/// Test double for `StatusAdvertiser`: records every status it's told
/// about, in order, instead of doing any real mDNS I/O. Exercises exactly
/// the seam `main.rs`'s real `MdnsStatusAdvertiser` plugs into -- the actual
/// mDNS re-registration is I/O and deliberately NOT unit-tested here (see
/// the task brief); what's under test is that `/run`/`/stop` call the trait
/// at all, with the right status, at the right time (after the in-memory
/// state change, per `AppState::set_serving`/`set_idle`).
#[derive(Default)]
struct FakeStatusAdvertiser {
    seen: Mutex<Vec<ServingStatus>>,
}

impl StatusAdvertiser for FakeStatusAdvertiser {
    fn advertise_status(&self, status: &ServingStatus) {
        self.seen
            .lock()
            .expect("seen mutex poisoned")
            .push(status.clone());
    }
}

impl FakeStatusAdvertiser {
    fn seen(&self) -> Vec<ServingStatus> {
        self.seen.lock().expect("seen mutex poisoned").clone()
    }
}

/// A successful `POST /run` must re-publish `Serving(<model>)` through the
/// attached `StatusAdvertiser` -- the fix for `tt-station discover` over mDNS
/// showing a stale `idle` while the box is actually serving.
#[tokio::test]
async fn run_notifies_status_advertiser_of_serving() {
    let client = reqwest::Client::new();
    let advertiser = Arc::new(FakeStatusAdvertiser::default());
    let state = fresh_state().with_status_advertiser(advertiser.clone());
    let base = serve(state.clone()).await;
    let token = pair(&client, &state, &base).await;

    let run_resp = client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");
    assert_eq!(run_resp.status(), reqwest::StatusCode::OK);

    assert_eq!(
        advertiser.seen(),
        vec![ServingStatus::Serving("llama3".to_string())]
    );
}

/// `POST /stop` after a `/run` must re-publish `Idle` through the attached
/// `StatusAdvertiser`, in addition to the `Serving` re-publish `/run` itself
/// triggered -- both state transitions get their own re-publish, not just
/// the first one.
#[tokio::test]
async fn stop_notifies_status_advertiser_of_idle() {
    let client = reqwest::Client::new();
    let advertiser = Arc::new(FakeStatusAdvertiser::default());
    let state = fresh_state().with_status_advertiser(advertiser.clone());
    let base = serve(state.clone()).await;
    let token = pair(&client, &state, &base).await;

    client
        .post(format!("{base}/run"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "model": "llama3" }))
        .send()
        .await
        .expect("POST /run failed");

    let stop_resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .expect("POST /stop failed");
    assert_eq!(stop_resp.status(), reqwest::StatusCode::OK);

    assert_eq!(
        advertiser.seen(),
        vec![
            ServingStatus::Serving("llama3".to_string()),
            ServingStatus::Idle,
        ]
    );
}

/// A backend whose first `stop` parks on two barriers, so a test can finish a `/run` while
/// that stop is still inside the backend. It keeps the serve the newer `start` recorded, as
/// the `tt` backend does (its serve epoch), and reports it from `status`.
struct ParkingBackend {
    serving: Mutex<Option<String>>,
    stops: std::sync::atomic::AtomicUsize,
    entered: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

impl ServingBackend for ParkingBackend {
    fn start_forcing(
        &self,
        model: &str,
        _force: bool,
    ) -> anyhow::Result<libttstation::model::Endpoint> {
        *self.serving.lock().unwrap() = Some(model.to_string());
        Ok(libttstation::model::Endpoint {
            base_url: format!("http://127.0.0.1:20000/v1#{model}"),
            model: model.to_string(),
            requires_key: false,
        })
    }

    fn stop(&self, _model: &str) -> anyhow::Result<()> {
        if self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.entered.wait();
            self.resume.wait();
            // The serve recorded while this stop was parked belongs to the newer start.
            return Ok(());
        }
        *self.serving.lock().unwrap() = None;
        Ok(())
    }

    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(match self.serving.lock().unwrap().clone() {
            Some(m) => ServingStatus::Serving(m),
            None => ServingStatus::Idle,
        })
    }
}

/// Review of the Copilot fixes, Important 1: a slow `/stop` of serve A that finishes after
/// `/run` B must not set the agent's status, endpoint and advertisement to idle while B
/// serves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_stop_that_ends_after_a_new_run_keeps_the_new_serve_reported() {
    let entered = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let backend: Arc<dyn ServingBackend> = Arc::new(ParkingBackend {
        serving: Mutex::new(None),
        stops: std::sync::atomic::AtomicUsize::new(0),
        entered: entered.clone(),
        resume: resume.clone(),
    });
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let run = |model: &'static str| {
        let (client, base, token) = (client.clone(), base.clone(), token.clone());
        async move {
            client
                .post(format!("{base}/run"))
                .bearer_auth(token)
                .json(&serde_json::json!({ "model": model }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(run("model-a").await, reqwest::StatusCode::OK);

    let stop_a = {
        let (client, base, token) = (client.clone(), base.clone(), token.clone());
        tokio::spawn(async move {
            client
                .post(format!("{base}/stop"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .status()
        })
    };
    let e = entered.clone();
    tokio::task::spawn_blocking(move || e.wait()).await.unwrap();
    assert_eq!(run("model-b").await, reqwest::StatusCode::OK);
    let r = resume.clone();
    tokio::task::spawn_blocking(move || r.wait()).await.unwrap();
    assert_eq!(stop_a.await.unwrap(), reqwest::StatusCode::OK);

    let status: serde_json::Value = client
        .get(format!("{base}/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"], "serving:model-b", "{status}");
    let ep: serde_json::Value = client
        .get(format!("{base}/endpoint"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ep["model"], "model-b", "{ep}");
}

/// A backend whose stop always answers "a stop is already in progress" as a contention.
struct BusyStopBackend;

impl ServingBackend for BusyStopBackend {
    fn start_forcing(
        &self,
        _model: &str,
        _force: bool,
    ) -> anyhow::Result<libttstation::model::Endpoint> {
        anyhow::bail!("not used")
    }

    fn stop(&self, _model: &str) -> anyhow::Result<()> {
        Err(anyhow::Error::new(
            tt_station_agentd::gozer::Contention::new(
                "tt backend: a stop is already in progress (waited 60s); try again when it \
                 finishes",
            ),
        ))
    }

    fn status(&self) -> anyhow::Result<ServingStatus> {
        Ok(ServingStatus::Idle)
    }
}

/// Review fix round 2, minor 2: a stop refused because another stop is still running is a
/// conflict with the box's state, so `/stop` answers 409 with the reason (a retry can work),
/// the way `/run` already answers a contention.
#[tokio::test]
async fn a_stop_refused_because_another_stop_runs_is_409() {
    let backend: Arc<dyn ServingBackend> = Arc::new(BusyStopBackend);
    let state = AppState::new("qb2-lab".to_string(), "4xBH".to_string(), backend);
    let base = serve(state.clone()).await;
    let client = reqwest::Client::new();
    let token = pair(&client, &state, &base).await;
    let resp = client
        .post(format!("{base}/stop"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("a stop is already in progress"),
        "{body}"
    );
}
