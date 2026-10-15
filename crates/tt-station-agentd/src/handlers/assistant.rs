// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2026 Tenstorrent USA, Inc.

//! The assistant's HTTP surface: `POST /assistant`, `/assistant/stream` and
//! `/assistant/confirm`. The assistant itself lives in `crate::assistant`.

use crate::auth::{BearerAuth, CallerAuth, OWNER_ONLY};
use crate::error::{contention_aware_error, tt_error, ErrorResponse};
use crate::state::AppState;
use axum::{http::StatusCode, Json};

/// Map an [`crate::assistant::AssistantError`] onto this crate's
/// `{ "error": "..." }` contract:
///
/// | Variant | Status | Why |
/// |---|---|---|
/// | `NotServing` | `409` | Same "well-formed, wrong box state" code `GET /endpoint` uses when idle. The message is written to be shown to a person as-is. |
/// | `BadRequest` | `400` | The client's history was unusable (see `assistant::prepare_history`). |
/// | `UnknownAction` | `404` | No such pending action -- never existed, already confirmed/denied, or expired. |
/// | `Upstream` | `502` | The served model couldn't be reached or answered garbage: the agent is a gateway here, and the failure is upstream of it. |
/// | `Backend` | `409`/`500` | A confirmed action failed -- `contention_aware_error`, exactly like `/run`/`/stop`/`/power`. |
fn assistant_error(err: crate::assistant::AssistantError) -> (StatusCode, Json<ErrorResponse>) {
    use crate::assistant::{AssistantError, PENDING_TTL};
    let (status, error) = match err {
        AssistantError::NotServing => (
            StatusCode::CONFLICT,
            "No model is serving — start one first".to_string(),
        ),
        AssistantError::NotChat(msg) => (StatusCode::CONFLICT, msg),
        AssistantError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
        AssistantError::UnknownAction => (
            StatusCode::NOT_FOUND,
            format!(
                "No pending action with that id — it was already answered or expired (actions expire after {} minutes)",
                PENDING_TTL.as_secs() / 60
            ),
        ),
        AssistantError::Upstream(msg) => (StatusCode::BAD_GATEWAY, msg),
        AssistantError::Backend(e) => return contention_aware_error(e),
    };
    (status, Json(ErrorResponse { error }))
}

/// `POST /assistant { "messages": [...] }` (bearer-guarded): chat with the
/// box, using the model it is currently serving as the brain. Read-only
/// tools run immediately; disruptive ones come back as
/// `needs_confirmation` actions for `POST /assistant/confirm`. `409` when
/// nothing is serving. The whole loop lives in [`crate::assistant::converse`]
/// -- see that module's doc for the confirmation model.
///
/// `BearerAuth` is listed before the `Json` body extractor so an
/// unauthenticated request is a `401` regardless of what its body looks like.
pub(crate) async fn assistant_chat(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::AssistantRequest>,
) -> Result<Json<crate::assistant::AssistantResponse>, (StatusCode, Json<ErrorResponse>)> {
    let opts = req.options(false);
    // The tools that act on the box's own computer are an owner's.
    let host_ok = caller.owner();
    if let Some(done) = crate::assistant::HOST_OK
        .scope(
            host_ok,
            crate::assistant::quick(&state, &req.messages, opts),
        )
        .await
    {
        return Ok(Json(done));
    }
    crate::people::CALLER
        .scope(
            who.person,
            crate::assistant::HOST_OK.scope(
                host_ok,
                crate::assistant::converse(&state, req.messages, opts),
            ),
        )
        .await
        .map(Json)
        .map_err(assistant_error)
}

/// `POST /assistant/stream` (bearer-guarded): `/assistant`, streamed as
/// server-sent events -- one `data: {"type": ...}` line per
/// [`crate::assistant::StreamEvent`]: `delta`/`reasoning` text as the model
/// writes it, `action`s as tools run, then `done` with the authoritative reply
/// (or `error`). Defaults to the `detailed` (Markdown) style. Problems known
/// before the model is asked (nothing serving, a non-chat model, a bad history)
/// are ordinary HTTP errors, like `/assistant`'s.
pub(crate) async fn assistant_stream(
    axum::extract::State(state): axum::extract::State<AppState>,
    who: crate::people::Who,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::AssistantRequest>,
) -> Result<
    axum::response::sse::Sse<
        impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
    >,
    (StatusCode, Json<ErrorResponse>),
> {
    let opts = req.options(true);
    let person = who.person;
    let signed_out = state.signed_out(who.token);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // The tools that act on the box's own computer are an owner's.
    let host_ok = caller.owner();
    if let Some(done) = crate::assistant::HOST_OK
        .scope(
            host_ok,
            crate::assistant::quick(&state, &req.messages, opts),
        )
        .await
    {
        // An instant command: already done; tell it like a stream.
        for action in done.actions {
            let _ = tx.send(crate::assistant::StreamEvent::Action { action });
        }
        let _ = tx.send(crate::assistant::StreamEvent::Delta {
            text: done.reply.clone(),
        });
        let _ = tx.send(crate::assistant::StreamEvent::Done {
            reply: done.reply,
            model: done.model,
        });
    } else {
        let chat = crate::people::CALLER
            .scope(
                person.clone(),
                crate::assistant::prepare_stream(&state, req.messages, opts),
            )
            .await
            .map_err(assistant_error)?;
        tokio::spawn(
            crate::people::CALLER.scope(
                person,
                crate::assistant::HOST_OK
                    .scope(host_ok, crate::assistant::converse_stream(state, chat, tx)),
            ),
        );
    }
    let events = futures_util::stream::unfold(rx, |mut rx| async move {
        let event = rx.recv().await?;
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Some((Ok(axum::response::sse::Event::default().data(data)), rx))
    });
    // Removing the device ends the stream.
    let events = futures_util::StreamExt::take_until(events, Box::pin(signed_out));
    Ok(axum::response::sse::Sse::new(events).keep_alive(axum::response::sse::KeepAlive::default()))
}

/// `POST /assistant/confirm { "action_id": "...", "approve": <bool> }`
/// (bearer-guarded): approve or deny a pending action from
/// `POST /assistant`. Consumes the action either way; `404` if it's unknown
/// or expired. An approved `run_model` returns `"started"` immediately and
/// serves in the background (exactly `/run`'s body, just not awaited);
/// `stop_model`/`reset_chips` run inline and return `"done"`. See
/// [`crate::assistant::confirm`].
pub(crate) async fn assistant_confirm(
    axum::extract::State(state): axum::extract::State<AppState>,
    _auth: BearerAuth,
    CallerAuth(caller): CallerAuth,
    Json(req): Json<crate::assistant::ConfirmRequest>,
) -> Result<Json<crate::assistant::ConfirmResponse>, (StatusCode, Json<ErrorResponse>)> {
    // Resetting the chips is a power action: an owner's, here as on `/power`.
    let tool = state.pending_action_tool(&req.action_id);
    if req.approve && tool.as_deref() == Some("reset_chips") && !caller.owner() {
        crate::audit::record(&state, "denied", "assistant reset_chips", "denied");
        return Err(tt_error(StatusCode::FORBIDDEN, OWNER_ONLY));
    }
    let approve = req.approve;
    let done = crate::assistant::HOST_OK
        .scope(caller.owner(), crate::assistant::confirm(&state, req))
        .await;
    let what = tool.unwrap_or_else(|| "unknown action".to_string());
    crate::audit::record(
        &state,
        "assistant_approval",
        &what,
        match (&done, approve) {
            (Err(_), _) => "failed",
            (Ok(_), true) => "approved",
            (Ok(_), false) => "declined",
        },
    );
    done.map(Json).map_err(assistant_error)
}
