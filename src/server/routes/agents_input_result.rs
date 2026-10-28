//! `GET /api/agents/{id}/input-result?origin_id=`: the execution an accepted input reached and,
//! once committed, how it ended. Unknown is 404; a failed read is 503, never 404.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::HumanInputCaller;
use crate::db::agents::AgentChannelBindings;
use crate::services::agents::input_result::{
    InputResult, MAX_ORIGIN_ID_LEN, OriginChannel, lookup,
};
use crate::services::provider::ProviderKind;

const CONTRACT: &str = "input-result-v1";

#[derive(Debug, Deserialize)]
pub struct InputResultQuery {
    #[serde(default)]
    origin_id: Option<String>,
}

type RouteResponse = (StatusCode, Json<Value>);

fn failure(status: StatusCode, error: &str) -> RouteResponse {
    (status, Json(json!({"ok": false, "error": error})))
}

/// The caller extractor answers before any lookup, exactly as on the human-input route.
pub async fn input_result(
    _caller: HumanInputCaller,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<InputResultQuery>,
) -> RouteResponse {
    let origin = query.origin_id.as_deref().map(str::trim).unwrap_or("");
    if origin.is_empty() {
        return failure(StatusCode::BAD_REQUEST, "origin_id_required");
    }
    if origin.len() > MAX_ORIGIN_ID_LEN {
        return failure(StatusCode::BAD_REQUEST, "origin_id_too_long");
    }
    let Some(pool) = state.pg_pool_ref() else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };
    let bindings = match crate::db::agents::load_agent_channel_bindings_pg(pool, &id).await {
        Ok(Some(bindings)) => bindings,
        Ok(None) => return failure(StatusCode::NOT_FOUND, "agent_not_found"),
        Err(_) => return failure(StatusCode::SERVICE_UNAVAILABLE, "input_result_unavailable"),
    };
    match lookup(pool, &id, origin, &origin_channels(&bindings)).await {
        Ok(Some(result)) => (StatusCode::OK, Json(result_body(&id, origin, result))),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "contract": CONTRACT, "error": "origin_not_found"})),
        ),
        Err(error) => {
            tracing::warn!(agent_id = %id, %error, "input result lookup failed");
            failure(StatusCode::SERVICE_UNAVAILABLE, "input_result_unavailable")
        }
    }
}

/// Every bound channel of the agent with the provider that runs there; a channel whose binding
/// names no single provider is read for each.
fn origin_channels(bindings: &AgentChannelBindings) -> Vec<OriginChannel> {
    let mut channels = Vec::new();
    for channel in bindings.all_channels() {
        let Some(channel_id) = super::dispatches::resolve_channel_alias_pub(&channel)
            .or_else(|| channel.parse::<u64>().ok())
        else {
            continue;
        };
        let providers = match bindings.provider_for_channel(|candidate| {
            super::agents::channel_identifier_matches(candidate, &channel)
        }) {
            Some(provider) => vec![provider],
            None => vec![ProviderKind::Claude, ProviderKind::Codex],
        };
        channels.extend(providers.into_iter().map(|provider| OriginChannel {
            provider: provider.as_str().to_string(),
            channel_id,
        }));
    }
    channels
}

/// The Discord-message turn id when the execution is named by one, else null.
fn message_turn_id(execution_id: &str) -> Option<&str> {
    let mut parts = execution_id.split(':');
    let numeric = |part: Option<&str>| part.is_some_and(|p| p.bytes().all(|b| b.is_ascii_digit()));
    (parts.next() == Some("discord")
        && numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none())
    .then_some(execution_id)
}

fn result_body(agent_id: &str, origin_id: &str, result: InputResult) -> Value {
    let (channel_id, execution_id, terminal) = match result {
        InputResult::Running {
            channel_id,
            execution_id,
        } => (channel_id.to_string(), execution_id, Value::Null),
        InputResult::Terminal(receipt) => (
            receipt.channel_id,
            receipt.turn_id,
            json!({
                "outcome": receipt.terminal_outcome,
                "confirmed_at": receipt.terminal_confirmed_at.to_rfc3339(),
                // No verified final-message boundary is produced yet.
                "final_answer_text": null,
                "final_answer_boundary": null,
                "final_answer_source": null,
            }),
        ),
    };
    json!({
        "ok": true,
        "contract": CONTRACT,
        "origin_id": origin_id,
        "agent_id": agent_id,
        "channel_id": channel_id,
        "consumption": "consumed",
        "execution_id": execution_id,
        "turn_id": message_turn_id(&execution_id),
        "terminal": terminal,
    })
}

#[cfg(test)]
#[path = "agents_input_result_tests.rs"]
mod tests;
