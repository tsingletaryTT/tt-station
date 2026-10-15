//! HTTP routes for `tt-station-agentd`.
//!
//! `AppState` is the one piece of shared state every handler (present and
//! future) reaches through. It's deliberately built as a cheap-to-`Clone`
//! handle (`Arc<Inner>`) around an `Inner` struct so later tasks can grow it
//! -- Task 7 adding a pairing-token set, Task 9 swapping in a real serving
//! backend, Task 10 adding control routes that mutate `status` -- without
//! reshaping the handle itself or how it's threaded through `axum::State`.
//!
//! `app()` builds the `Router` from an `AppState` with no side effects (no
//! mDNS registration, no socket binding), so tests can spin up the real
//! router against an ephemeral port without dragging the network stack
//! along for the ride. `main.rs` is responsible for anything that touches
//! the outside world.

use axum::{
    routing::{get, post},
    Router,
};

pub use crate::auth::*;
pub(crate) use crate::error::*;
pub(crate) use crate::handlers::assistant::*;
pub(crate) use crate::handlers::devices::*;
pub(crate) use crate::handlers::docs::*;
pub(crate) use crate::handlers::logs::*;
pub(crate) use crate::handlers::media::*;
pub(crate) use crate::handlers::memories::*;
pub(crate) use crate::handlers::pairing::*;
pub(crate) use crate::handlers::power::*;
pub(crate) use crate::handlers::security::*;
pub(crate) use crate::handlers::serving::*;
pub(crate) use crate::handlers::ssh::*;
pub(crate) use crate::handlers::status::*;
pub use crate::handlers::telemetry::*;
pub(crate) use crate::handlers::ttcli::*;
pub use crate::state::*;

/// The plain-HTTP app: the full API for local callers only, plus the routes
/// that only the box itself may use (pairing a phone from the desktop app).
pub fn local_http_app(state: AppState) -> Router {
    let local_only = Router::new()
        .route("/pair/qr", post(pair_qr))
        .route("/security", get(get_security).post(post_security))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state.clone());
    app(state.clone())
        .merge(local_only)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            only_local,
        ))
        .layer(axum::middleware::from_fn_with_state(state, known_host))
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/status", get(get_status))
        .route("/healthz", get(crate::health::healthz))
        .route("/config", get(get_config))
        .route("/models", get(get_models))
        .route("/serving", get(get_serving))
        .route("/logs", get(get_logs))
        .route("/logs/stream", get(logs_ws))
        .route("/telemetry", get(telemetry_ws))
        .route("/pair/init", post(pair_init))
        .route("/pair/complete", post(pair_complete))
        .route("/pair/offer", post(pair_offer))
        .route("/run", post(run_model))
        .route("/stop", post(stop_model))
        .route("/reset", post(reset))
        .route("/devices", get(get_devices))
        .route("/devices/{id}", axum::routing::delete(delete_device))
        .route("/devices/{id}/owner", post(post_device_owner))
        .route(
            "/devices/{id}/person",
            axum::routing::put(put_device_person),
        )
        .route("/audit", get(crate::audit::get_audit))
        .route("/power", post(power))
        .route("/endpoint", get(get_endpoint))
        .route("/leases", get(get_leases))
        .route("/ssh/authorize", post(ssh_authorize).delete(ssh_revoke))
        .route("/ssh/info", get(ssh_info))
        .route("/discover", get(crate::discover::get_discover))
        .route("/assistant", post(assistant_chat))
        .route("/assistant/confirm", post(assistant_confirm))
        .route(
            "/assistant/client-result",
            post(crate::assistant::post_client_result),
        )
        .route("/assistant/stream", post(assistant_stream))
        .route("/tt/models", get(tt_models))
        .route("/tt/pulls", get(tt_pulls))
        .route("/tt/pull", post(tt_pull))
        .route("/tt/pull/cancel", post(tt_pull_cancel))
        .route("/serve", get(get_serve).post(post_serve))
        .route(
            "/keys",
            get(crate::gateway::list_keys).post(crate::gateway::create_key),
        )
        .route(
            "/keys/{id}",
            axum::routing::delete(crate::gateway::revoke_key).patch(crate::gateway::change_key),
        )
        .route("/connect", get(crate::gateway::connect_info))
        .route("/bench", post(crate::bench::post_bench))
        .route("/creations/{id}", get(get_creation))
        .route("/memories", get(list_memories).delete(clear_memories))
        .route("/power/info", get(power_info))
        .route("/tls", get(crate::tls::get_tls))
        .route("/jobs", get(crate::jobs::get_jobs))
        .route(
            "/jobs/transcribe",
            post(crate::jobs::post_transcribe).layer(axum::extract::DefaultBodyLimit::max(
                crate::jobs::MAX_UPLOAD_BYTES,
            )),
        )
        .route("/jobs/{id}", axum::routing::delete(crate::jobs::delete_job))
        .route("/jobs/{id}/run", post(crate::jobs::post_run_now))
        .route("/jobs/{id}/result", get(crate::jobs::get_result))
        .route("/jobs/{id}/share", post(crate::jobs::post_share))
        .route(
            "/data/settings",
            get(crate::export::get_settings).put(crate::export::put_settings),
        )
        .route("/export", post(crate::export::post_export))
        .route(
            "/import",
            post(crate::export::post_import).layer(axum::extract::DefaultBodyLimit::max(
                crate::export::MAX_IMPORT_BYTES as usize,
            )),
        )
        .route("/app", get(crate::app_ui::get_app_root))
        .route("/app/", get(crate::app_ui::get_app_root))
        .route("/app/{*rel}", get(crate::app_ui::get_app_path))
        .route("/apps", get(crate::apps::get_apps))
        .route("/apps/{id}/start", post(crate::apps::post_start))
        .route("/apps/{id}/stop", post(crate::apps::post_stop))
        .route(
            "/apps/{id}/network",
            axum::routing::put(crate::apps::put_network),
        )
        .route("/apps/{id}", axum::routing::delete(crate::apps::delete_app))
        .route("/energy", get(get_energy))
        .route("/record", get(crate::recorder::get_record))
        .route("/assistant/brain", get(crate::assistant::get_brain))
        .route("/bringup/engines", get(crate::bringup::get_engines))
        .route("/community", get(crate::community::get_community))
        .route("/community/catalog", get(crate::community::get_catalog))
        .route(
            "/community/info/{ns}/{name}",
            get(crate::community::get_info),
        )
        .route("/community/pull", post(crate::community::post_pull))
        .route("/community/pull/pause", post(crate::community::post_pause))
        .route(
            "/community/pull/cancel",
            post(crate::community::post_cancel),
        )
        .route("/community/serve", post(crate::community::post_serve))
        .route(
            "/community/{ns}/{name}",
            axum::routing::delete(crate::community::delete_package),
        )
        .route("/hermes", get(crate::hermes::get_hermes))
        .route("/hermes/install", post(crate::hermes::post_install))
        .route("/hermes/open", post(crate::hermes::post_open))
        .route("/hermes/key", post(crate::hermes::post_key))
        .route("/vllm/native", get(crate::native_vllm::get_native))
        .route("/vllm/native/setup", post(crate::native_vllm::post_setup))
        .route("/vllm/native/serve", post(crate::native_vllm::post_serve))
        .route("/bringup/survey", post(crate::bringup::post_survey))
        .route(
            "/bringup/claude/login",
            post(crate::bringup::post_claude_login),
        )
        .route(
            "/bringup/codex/runs",
            get(crate::bringup::get_runs).post(crate::bringup::post_run),
        )
        .route(
            "/bringup/codex/runs/{id}",
            get(crate::bringup::get_run).delete(crate::bringup::delete_run),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals",
            post(crate::bringup::post_approval),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals/{aid}/wait",
            post(crate::bringup::post_wait),
        )
        .route(
            "/bringup/codex/runs/{id}/approvals/{aid}",
            post(crate::bringup::post_decide),
        )
        .route(
            "/bringup/codex/runs/{id}/{action}",
            post(crate::bringup::post_action),
        )
        .route("/toplike", get(crate::toplike_term::get_toplike))
        .route("/toplike/term", get(crate::toplike_term::term_ws))
        .route("/coding/term", get(crate::coding::term_ws))
        .route(
            "/preferences/models",
            get(crate::prefs::get_prefs).put(crate::prefs::put_prefs),
        )
        .route("/desktop/apps", get(crate::desktop::get_apps))
        .route("/desktop/open", post(crate::desktop::post_open))
        .route(
            "/voice",
            get(crate::voice::get_voice)
                .put(crate::voice::put_voice)
                .delete(crate::voice::delete_voice),
        )
        .route("/voice/setup", post(crate::voice::post_setup))
        .route("/voice/speak", post(crate::voice::post_speak))
        .route(
            "/voice/hear",
            get(crate::voice::get_hear)
                .post(crate::voice::post_hear)
                .delete(crate::voice::delete_hear),
        )
        .route("/voice/say", post(crate::voice::post_say))
        .route("/voice/hear/finish", post(crate::voice::post_hear_finish))
        .route(
            "/voice/listen",
            post(crate::voice::post_listen)
                .layer(axum::extract::DefaultBodyLimit::max(25 * 1024 * 1024)),
        )
        .route("/record/start", post(crate::recorder::post_start))
        .route("/record/stop", post(crate::recorder::post_stop))
        .route(
            "/schedules",
            get(crate::schedules::get_schedules).post(crate::schedules::post_schedule),
        )
        .route(
            "/schedules/{id}",
            axum::routing::patch(crate::schedules::patch_schedule)
                .delete(crate::schedules::delete_schedule),
        )
        .route(
            "/people",
            get(crate::people::get_people).post(crate::people::post_person),
        )
        .route("/people/me", axum::routing::put(crate::people::put_me))
        .route(
            "/people/{id}",
            axum::routing::patch(crate::people::patch_person).delete(crate::people::delete_person),
        )
        .route("/orchard", get(crate::orchard::get_orchard))
        .route("/orchard/setup", post(crate::orchard::post_setup))
        .route("/orchard/preflight", post(crate::orchard::post_preflight))
        .route("/orchard/runs", post(crate::orchard::post_run))
        .route(
            "/orchard/runs/{name}",
            get(crate::orchard::get_run).delete(crate::orchard::delete_run),
        )
        .route(
            "/orchard/runs/{name}/serve",
            post(crate::orchard::post_serve),
        )
        .route("/orchard/packaging", post(crate::orchard::post_packaging))
        .route(
            "/orchard/lab",
            get(crate::orchard_lab::get_lab).put(crate::orchard_lab::put_lab),
        )
        .route("/orchard/lab/check", post(crate::orchard_lab::post_check))
        .route("/orchard/lab/setup", post(crate::orchard_lab::post_setup))
        .route(
            "/orchard/runs/{name}/{action}",
            post(crate::orchard::post_control),
        )
        .route(
            "/orchard/runs/{name}/report/{which}",
            get(crate::orchard::get_report),
        )
        .route("/orchard/runs/{name}/story", get(crate::orchard::get_story))
        .route(
            "/favorites",
            get(crate::favorites::get_favorites).put(crate::favorites::put_favorite),
        )
        .route(
            "/research",
            get(crate::research::get_research).post(crate::research::post_research),
        )
        .route(
            "/research/{id}",
            get(crate::research::get_job).delete(crate::research::delete_job),
        )
        .route("/research/{id}/stop", post(crate::research::post_stop))
        .route("/research/{id}/rename", post(crate::research::post_rename))
        .route(
            "/research/{id}/explain",
            post(crate::research::post_explain),
        )
        .route("/skills", get(crate::skills::get_skills))
        .route(
            "/embeddings",
            get(crate::embeddings::get_embeddings).put(crate::embeddings::put_embeddings),
        )
        .route("/code", get(crate::workspace::get_code))
        .route("/code/setup", post(crate::workspace::post_setup))
        .route("/code/sessions", post(crate::workspace::post_session))
        .route(
            "/code/sessions/{id}",
            get(crate::workspace::get_session).delete(crate::workspace::delete_session),
        )
        .route(
            "/code/sessions/{id}/message",
            post(crate::workspace::post_message),
        )
        .route(
            "/code/sessions/{id}/approve",
            post(crate::workspace::post_approve),
        )
        .route(
            "/code/sessions/{id}/interrupt",
            post(crate::workspace::post_interrupt),
        )
        .route(
            "/code/sessions/{id}/stop",
            post(crate::workspace::post_stop),
        )
        .route(
            "/code/sessions/{id}/changes",
            get(crate::workspace::get_changes),
        )
        .route(
            "/code/sessions/{id}/commit",
            post(crate::workspace::post_commit),
        )
        .route(
            "/code/sessions/{id}/discard",
            post(crate::workspace::post_discard),
        )
        .route(
            "/code/sessions/{id}/forget",
            post(crate::workspace::post_forget),
        )
        .route(
            "/code/sessions/{id}/settings",
            post(crate::workspace::post_settings),
        )
        .route(
            "/code/sessions/{id}/rename",
            post(crate::workspace::post_rename),
        )
        .route(
            "/code/sessions/{id}/speed",
            get(crate::codespeed::get_speed),
        )
        .route("/code/models", get(crate::codespeed::get_models))
        .route("/coding", get(crate::coding::get_coding))
        .route("/coding/sessions", post(crate::coding::post_session))
        .route(
            "/coding/sessions/{id}",
            axum::routing::delete(crate::coding::delete_session),
        )
        .route("/coding/sessions/{id}/stop", post(crate::coding::post_stop))
        .route(
            "/coding/sessions/{id}/changes",
            get(crate::coding::get_changes),
        )
        .route(
            "/coding/folders",
            get(crate::coding::get_folders).post(crate::coding::post_folder),
        )
        .route("/skills/{id}/hermes", post(crate::skills::post_hermes))
        .route(
            "/idle",
            get(crate::idle::get_idle).put(crate::idle::put_idle),
        )
        .route("/idle/wake", post(crate::idle::post_wake))
        .route("/updates", get(crate::updates::get_updates))
        .route("/updates/install", post(crate::updates::post_install))
        .route("/updates/undo", post(crate::updates::post_undo))
        .route("/approvals", get(crate::approvals::get_approvals))
        .route("/approvals/answer", post(crate::approvals::post_answer))
        .route(
            "/evals",
            get(crate::evals::get_evals).post(crate::evals::post_eval),
        )
        .route("/evals/stop", post(crate::evals::post_stop))
        .route(
            "/evals/{id}",
            get(crate::evals::get_eval).delete(crate::evals::delete_eval),
        )
        .route("/library", get(crate::library::get_library))
        .route("/library/sources", post(crate::library::post_source))
        .route(
            "/library/sources/{id}",
            axum::routing::delete(crate::library::delete_source),
        )
        .route(
            "/library/sources/{id}/reindex",
            post(crate::library::post_reindex),
        )
        .route("/library/search", post(crate::library::post_search))
        .route(
            "/routines",
            get(crate::routines::get_routines).post(crate::routines::post_routine),
        )
        .route(
            "/routines/{id}",
            axum::routing::put(crate::routines::put_routine)
                .delete(crate::routines::delete_routine),
        )
        .route("/routines/{id}/run", post(crate::routines::post_run))
        .route("/routines/{id}/runs", get(crate::routines::get_runs))
        .route("/routines/runs/{run}", get(crate::routines::get_run))
        .route(
            "/routines/runs/{run}/approve",
            post(crate::routines::post_approve),
        )
        .route(
            "/routines/runs/{run}/stop",
            post(crate::routines::post_stop),
        )
        .route("/lab", get(crate::lab::get_lab))
        .route("/lab/prepare", post(crate::lab::post_prepare))
        .route("/lab/run", post(crate::lab::post_run))
        .route("/modes", get(crate::modes::get_modes))
        .route("/modes/{id}", post(crate::modes::post_mode))
        .route("/software", get(crate::tttools::get_software))
        .route(
            "/software/cli-update",
            post(crate::tttools::post_cli_update),
        )
        .route(
            "/tt/models/{name}",
            axum::routing::delete(crate::tttools::delete_model),
        )
        .route("/clients", get(crate::tttools::get_clients))
        .route("/clients/{tool}/start", post(crate::tttools::start_client))
        .route("/clients/{tool}/stop", post(crate::tttools::stop_client))
        .route("/doctor", get(crate::doctor::get_doctor))
        .route("/doctor/fix/{fix}", post(crate::doctor::post_fix))
        .route("/diagnostics", post(crate::doctor::post_diagnostics))
        .route("/diagnostics/{id}", get(crate::doctor::get_diagnostics))
        .route("/notify", get(crate::notify::get_notify))
        .route("/notify/devices", post(crate::notify::register_device))
        .route(
            "/notify/devices/{token}",
            axum::routing::delete(crate::notify::unregister_device),
        )
        .route("/notify/test", post(crate::notify::test_notify))
        .route("/events", get(crate::notify::get_events))
        .route("/memories/{id}", axum::routing::delete(delete_memory))
        .route("/v1/models", get(crate::gateway::v1_models))
        .route("/v1/chat/completions", post(crate::gateway::v1_text))
        .route("/v1/completions", post(crate::gateway::v1_text))
        .route("/v1/embeddings", post(crate::gateway::v1_embeddings))
        .route("/v1/rerank", post(crate::gateway::v1_rerank))
        .route("/v1/messages", post(crate::anthropic::v1_messages))
        .route(
            "/v1/messages/count_tokens",
            post(crate::anthropic::count_tokens),
        )
        .route("/v1/images/generations", post(crate::gateway::v1_images))
        .route(
            "/coserve",
            get(crate::coserve::get_coserve).post(crate::coserve::post_coserve),
        )
        .route("/metrics/history", get(metrics_history))
        .route("/metrics/now", get(metrics_now))
        .route(
            "/docs",
            get(list_docs)
                .post(add_doc)
                .layer(axum::extract::DefaultBodyLimit::max(DOC_BODY_LIMIT)),
        )
        .route("/docs/search", post(search_docs))
        .route("/docs/{id}", axum::routing::delete(delete_doc))
        .route("/generate/image", post(generate_image))
        .route("/generate/video", post(generate_video))
        .route("/generate/videos", get(list_videos))
        .route("/generate/video/{id}", get(get_video))
        .route("/generate/video/{id}/download", get(download_video))
        .route("/generate/video/{id}/cancel", post(cancel_video))
        // Default-deny (see `require_auth`): last, so it covers every route above.
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state)
}

#[cfg(test)]
mod telemetry_inference_tests {
    use super::*;
    use axum::http::StatusCode;
    use libttstation::model::Endpoint;
    use std::time::{Duration, Instant};

    #[test]
    fn port_from_base_url_parses_the_common_shapes() {
        assert_eq!(port_from_base_url("http://host:8003/v1"), Some(8003));
        assert_eq!(port_from_base_url("http://127.0.0.1:8000/v1"), Some(8000));
        // No port at all.
        assert_eq!(port_from_base_url("http://host/v1"), None);
        // No scheme separator.
        assert_eq!(port_from_base_url("host:8003/v1"), None);
        // Non-numeric "port".
        assert_eq!(port_from_base_url("http://host:abc/v1"), None);
    }

    #[test]
    fn resolve_metrics_port_prefers_stored_endpoint_over_configured_default() {
        // No endpoint stored (idle box) -> falls back to the configured
        // serving port.
        let state = AppState::new(
            "qb2-lab".to_string(),
            "4xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_serving_config("127.0.0.1".to_string(), 9000);
        assert_eq!(resolve_metrics_port(&state), 9000);

        // Once something is (recorded as) serving, its endpoint's own port
        // wins over the agent's configured default.
        state.set_serving(Endpoint {
            base_url: "http://127.0.0.1:8003/v1".to_string(),
            model: "meta-llama/Llama-3.1-8B-Instruct".to_string(),
            requires_key: false,
        });
        assert_eq!(resolve_metrics_port(&state), 8003);
    }

    #[tokio::test]
    async fn cached_snapshot_dedupes_tt_smi_within_ttl() {
        // Stub `tt-smi`: a script that records one run (appends a byte to a
        // counter file) and prints a minimal snapshot. Two `cached_snapshot`
        // calls within the TTL must execute it only ONCE — the whole point of
        // the shared cache (N clients → ~one tt-smi per interval, not one each).
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttsmi-dedup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let _ = std::fs::remove_file(&counter);
        let script = dir.join("tt-smi-stub.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{c}'\nprintf '{{\"device_info\":[]}}'\n",
                c = counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // 10s TTL so both calls land in the same window.
        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_telemetry_config(script.to_string_lossy().into_owned(), 10_000);

        let a = state.cached_snapshot().await.unwrap();
        let b = state.cached_snapshot().await.unwrap();
        assert_eq!(a, b, "second call reuses the cached snapshot");
        let runs = std::fs::read(&counter).map(|v| v.len()).unwrap_or(0);
        assert_eq!(
            runs, 1,
            "tt-smi ran once within the TTL, not per call (got {runs})"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_recorder_reads_the_chips_every_thirty_seconds_on_its_own() {
        assert!(chips_due(None), "the first tick reads them");
        assert!(!chips_due(Some(Instant::now())));
        assert!(!chips_due(Some(Instant::now() - METRICS_INTERVAL * 5)));
        assert!(chips_due(Some(Instant::now() - CHIPS_INTERVAL)));
    }

    #[tokio::test]
    async fn concurrent_telemetry_viewers_share_one_tt_smi() {
        // Eight `/telemetry` sockets ticking at once (each tick is a
        // `cached_snapshot`) run tt-smi once between them, not once each.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttsmi-herd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("count");
        let _ = std::fs::remove_file(&counter);
        let script = dir.join("tt-smi-stub.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf x >> '{c}'\nsleep 0.3\nprintf '{{\"device_info\":[]}}'\n",
                c = counter.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_telemetry_config(script.to_string_lossy().into_owned(), 1_000);
        let ticks: Vec<_> = (0..8)
            .map(|_| {
                let st = state.clone();
                tokio::spawn(async move { st.cached_snapshot().await.is_ok() })
            })
            .collect();
        for t in ticks {
            assert!(t.await.unwrap());
        }
        let runs = std::fs::read(&counter).map(|v| v.len()).unwrap_or(0);
        assert_eq!(runs, 1, "eight viewers, one tt-smi (got {runs})");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_power_command_runs_the_configured_command() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ttpower-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let _ = std::fs::remove_file(&marker);
        let script = dir.join("fake-power.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf x >> '{m}'\n", m = marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Through sh, not exec'd directly: another test forking while this
        // file is still open for writing makes exec fail with "Text file busy".
        let cmd = vec!["/bin/sh".to_string(), script.to_string_lossy().into_owned()];

        let state = AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
        .with_power_config(cmd.clone(), cmd.clone(), cmd.clone(), cmd.clone());

        state
            .run_power_command(crate::power::PowerAction::Reboot, false)
            .expect("power command runs");
        assert!(marker.exists(), "configured power command was executed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pending(tool: &str, age: Duration) -> crate::assistant::PendingAction {
        crate::assistant::PendingAction {
            tool: tool.to_string(),
            args: serde_json::json!({}),
            summary: String::new(),
            created: Instant::now() - age,
        }
    }

    fn bare_state() -> AppState {
        AppState::new(
            "t".to_string(),
            "1xBH".to_string(),
            std::sync::Arc::new(crate::serving::dstack::DstackBackend),
        )
    }

    #[test]
    fn pending_actions_are_taken_once_and_expire_after_the_ttl() {
        let state = bare_state();
        let fresh = state.insert_pending_action(pending("stop_model", Duration::ZERO));
        let stale = state.insert_pending_action(pending(
            "stop_model",
            crate::assistant::PENDING_TTL + Duration::from_secs(1),
        ));
        assert!(state.take_pending_action(&stale).is_none(), "expired");
        assert_eq!(
            state.take_pending_action(&fresh).unwrap().tool,
            "stop_model"
        );
        assert!(state.take_pending_action(&fresh).is_none(), "consumed");
    }

    #[test]
    fn pending_actions_are_capped_by_evicting_the_oldest() {
        let state = bare_state();
        let oldest = state.insert_pending_action(pending("run_model", Duration::from_secs(60)));
        let mut rest = Vec::new();
        for _ in 0..crate::assistant::MAX_PENDING {
            rest.push(state.insert_pending_action(pending("stop_model", Duration::ZERO)));
        }
        assert!(
            state.take_pending_action(&oldest).is_none(),
            "oldest evicted"
        );
        for id in rest {
            assert!(state.take_pending_action(&id).is_some());
        }
    }

    #[test]
    fn power_success_status_is_202_for_machine_ops_200_for_reset_chips() {
        use crate::power::PowerAction;
        assert_eq!(
            power_success_status(PowerAction::ResetChips),
            StatusCode::OK
        );
        assert_eq!(
            power_success_status(PowerAction::Suspend),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Reboot),
            StatusCode::ACCEPTED
        );
        assert_eq!(
            power_success_status(PowerAction::Shutdown),
            StatusCode::ACCEPTED
        );
    }
}
