use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::json;

use super::super::{agents_turn_deliver, imessage_input_notice};
use super::{AgentHandoffBody, AppState, pg_required_response};
use crate::api_caller_observability::RequestPrincipal;

/// POST /api/agents/{id}/handoff
/// #3556 — agent-to-agent turn-trigger handoff. Resolves the target's cc/cdx
/// mailbox and reserves a headless turn directly on it. Unlike
/// `/api/agents/{id}/message`, no announce message is posted: the turn is the
/// authoritative effect, so success/failure carry turn semantics (200 started,
/// 409 mailbox busy, 404 not found, 422 channel_kind unset, 503 unavailable).
/// This is the "execution intent" counterpart to the announce-only "notify"
/// path, and it cannot trip the #3576 announce-trigger double-run because it
/// never lands a message on the cc channel.
pub async fn agent_handoff(
    State(state): State<AppState>,
    Path(to_agent_id): Path<String>,
    principal: Option<Extension<RequestPrincipal>>,
    Json(body): Json<AgentHandoffBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let declared = (body.source.as_deref(), body.metadata.as_ref());
    if let Some(refused) = agents_turn_deliver::refuse_decision_source(declared.0, declared.1) {
        return refused;
    }
    let notice = imessage_input_notice::notice_line(body.source.as_deref(), &body.prompt);
    let Some(pool) = state.pg_pool_ref() else {
        imessage_input_notice::not_echoed(notice.as_deref(), 0, "handoff", "pool_unavailable");
        return pg_required_response();
    };
    let Some(registry) = state.health_registry.as_ref() else {
        imessage_input_notice::not_echoed(notice.as_deref(), 0, "handoff", "runtime_unavailable");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "Discord not available (standalone mode)"})),
        );
    };

    let channel_kind = match crate::services::discord::agent_handoff::AgentHandoffChannelKind::parse(
        body.channel_kind.as_deref(),
    ) {
        Ok(channel_kind) => channel_kind,
        Err(error) => {
            imessage_input_notice::not_echoed(
                notice.as_deref(),
                0,
                "handoff",
                "invalid_channel_kind",
            );
            return (error.status(), Json(error.body()));
        }
    };

    let (metadata, delegation) = match crate::services::agents::delegation::handoff_origin(
        pool,
        &body.from_agent_id,
        &to_agent_id,
        body.delegation == Some(true),
        principal
            .as_ref()
            .and_then(|Extension(principal)| principal.claimed_channel_id.as_deref()),
        body.origin_id.as_deref(),
        body.metadata,
    )
    .await
    {
        Ok(prepared) => prepared,
        Err((status, error)) => {
            imessage_input_notice::not_echoed(notice.as_deref(), 0, "handoff", error);
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
            return (status, Json(json!({"ok": false, "error": error})));
        }
    };

    match crate::services::discord::agent_handoff::start_agent_handoff_turn(
        registry,
        pool,
        &body.from_agent_id,
        &to_agent_id,
        &body.prompt,
        channel_kind,
        body.prefix.unwrap_or(true),
        body.expect_reply,
        body.source,
        metadata,
    )
    .await
    {
        Ok(response) => {
            imessage_input_notice::echo(
                pool,
                response.channel_id_num,
                &format!("turn:{}", response.turn_id),
                notice.as_deref(),
            )
            .await;
            let mut value = response.to_value();
            if let Some(delegation) = delegation {
                use crate::services::discord::delegation_card::post_accept_card;
                let card = post_accept_card(registry, &delegation, response.channel_id_num).await;
                value["origin_id"] = json!(delegation.origin.id());
                value["accept_card"] = match card {
                    Ok(message_id) => json!({"message_id": message_id.to_string()}),
                    Err(error) => json!({"error": error}),
                };
            }
            (StatusCode::OK, Json(value))
        }
        Err(error) => {
            imessage_input_notice::not_echoed(notice.as_deref(), 0, "handoff", "backend_error");
            (error.status(), Json(error.body()))
        }
    }
}
