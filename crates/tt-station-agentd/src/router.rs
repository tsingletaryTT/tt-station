// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The route table: which path and method reach which handler, behind which
//! auth layer. `app()` builds the `Router` from an `AppState` with no side
//! effects (no mDNS registration, no socket binding), so tests can spin up
//! the real router against an ephemeral port; `main.rs` does anything that
//! touches the outside world.

use axum::{
    routing::{get, post, put},
    Router,
};

use crate::auth::{known_host, only_local, require_auth};
use crate::handlers::assistant::{assistant_chat, assistant_confirm, assistant_stream};
use crate::handlers::devices::{delete_device, get_devices, post_device_owner, put_device_person};
use crate::handlers::docs::{add_doc, delete_doc, list_docs, search_docs, DOC_BODY_LIMIT};
use crate::handlers::logs::{get_logs, logs_ws};
use crate::handlers::media::{
    cancel_video, download_video, generate_image, generate_video, get_video, list_videos,
};
use crate::handlers::memories::{clear_memories, delete_memory, get_creation, list_memories};
use crate::handlers::pairing::{pair_complete, pair_init, pair_offer, pair_qr};
use crate::handlers::power::{power, power_info, reset, wake_on_lan};
use crate::handlers::security::{get_security, post_security};
use crate::handlers::serving::{get_serve, post_serve, run_model, stop_model};
use crate::handlers::ssh::{ssh_authorize, ssh_host_keys, ssh_info, ssh_revoke};
use crate::handlers::status::{
    get_config, get_endpoint, get_leases, get_models, get_serving, get_status,
};
use crate::handlers::telemetry::{get_energy, metrics_history, metrics_now, telemetry_ws};
use crate::handlers::ttcli::{tt_models, tt_pull, tt_pull_cancel, tt_pulls};
use crate::state::AppState;

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
    // Routes that start, stop or switch serving: refused to members when an
    // owner says only owners may (`crate::model_control`).
    let models =
        || axum::middleware::from_fn_with_state(state.clone(), crate::model_control::require);
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
        .route("/run", post(run_model).route_layer(models()))
        .route("/stop", post(stop_model).route_layer(models()))
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
        .route("/ssh/host-keys", get(ssh_host_keys))
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
        .route(
            "/serve",
            get(get_serve).merge(post(post_serve).route_layer(models())),
        )
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
        .route("/power/wake-on-lan", post(wake_on_lan))
        .route("/tls", get(crate::tls::get_tls))
        .route("/jobs", get(crate::jobs::get_jobs))
        .route(
            "/jobs/transcribe",
            post(crate::jobs::post_transcribe).layer(axum::extract::DefaultBodyLimit::max(
                crate::jobs::MAX_UPLOAD_BYTES,
            )),
        )
        .route("/jobs/{id}", axum::routing::delete(crate::jobs::delete_job))
        .route(
            "/jobs/{id}/run",
            post(crate::jobs::post_run_now).route_layer(models()),
        )
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
        .route(
            "/community/serve",
            post(crate::community::post_serve).route_layer(models()),
        )
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
        .route(
            "/vllm/native/serve",
            post(crate::native_vllm::post_serve).route_layer(models()),
        )
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
        // Which model the box prefers to serve: the model-control setting applies.
        .route(
            "/preferences/models",
            get(crate::prefs::get_prefs).merge(put(crate::prefs::put_prefs).route_layer(models())),
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
        // A schedule switches the box's mode at set times, as the box (they
        // keep no author), so writing one is for whoever may switch models.
        .route(
            "/schedules",
            get(crate::schedules::get_schedules)
                .merge(post(crate::schedules::post_schedule).route_layer(models())),
        )
        .route(
            "/schedules/{id}",
            axum::routing::patch(crate::schedules::patch_schedule)
                .route_layer(models())
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
        .route("/research/{id}/share", post(crate::research::post_share))
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
        .route("/code/models", get(crate::codemodels::get_models))
        .route(
            "/coding/folders",
            get(crate::coding::get_folders).post(crate::coding::post_folder),
        )
        .route("/skills/{id}/hermes", post(crate::skills::post_hermes))
        // Idle sleep stops the model and waking starts it: the model-control setting applies.
        .route(
            "/idle",
            get(crate::idle::get_idle).merge(put(crate::idle::put_idle).route_layer(models())),
        )
        .route(
            "/idle/wake",
            post(crate::idle::post_wake).route_layer(models()),
        )
        .route(
            "/idle/sleep",
            post(crate::idle::post_sleep).route_layer(models()),
        )
        .route("/updates", get(crate::updates::get_updates))
        .route("/updates/install", post(crate::updates::post_install))
        .route("/updates/undo", post(crate::updates::post_undo))
        .route(
            "/updates/channel/approve",
            post(crate::updates::post_approve_key),
        )
        .route("/approvals", get(crate::approvals::get_approvals))
        .route("/approvals/answer", post(crate::approvals::post_answer))
        .route(
            "/evals",
            get(crate::evals::get_evals).merge(post(crate::evals::post_eval).route_layer(models())),
        )
        .route(
            "/evals/stop",
            post(crate::evals::post_stop).route_layer(models()),
        )
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
        .route(
            "/modes/{id}",
            post(crate::modes::post_mode).route_layer(models()),
        )
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
            get(crate::coserve::get_coserve)
                .merge(post(crate::coserve::post_coserve).route_layer(models())),
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
