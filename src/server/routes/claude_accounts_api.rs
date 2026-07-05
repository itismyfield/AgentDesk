use axum::{Json, extract::State, http::StatusCode};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::services::cswap::{CswapError, CswapService};

use super::AppState;

#[derive(Debug, Deserialize)]
pub struct SwitchClaudeAccountRequest {
    pub account: String,
}

/// GET /api/claude-accounts — local cswap account usage + active account.
pub async fn get_claude_accounts(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let hostname = crate::services::platform::hostname_short();
    let instance_id = state.cluster_instance_id.clone();
    let service = CswapService::global();

    match service
        .list_accounts(hostname.clone(), instance_id.clone())
        .await
    {
        Ok(response) => match serde_json::to_value(&response) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "execution_failure",
                    "code": "serialization_failure",
                    "hostname": hostname,
                    "instanceId": instance_id,
                    "fetchedAt": Utc::now(),
                    "error": format!("serialize claude accounts response: {error}"),
                })),
            ),
        },
        Err(error) => (
            status_for_error(&error),
            Json(error_body(error, hostname, instance_id)),
        ),
    }
}

/// POST /api/claude-accounts/switch — switch this node's global Claude auth.
pub async fn switch_claude_account(
    State(state): State<AppState>,
    Json(body): Json<SwitchClaudeAccountRequest>,
) -> (StatusCode, Json<Value>) {
    let hostname = crate::services::platform::hostname_short();
    let instance_id = state.cluster_instance_id.clone();
    let service = CswapService::global();

    let result = match service.switch_account(&body.account).await {
        Ok(result) => result,
        Err(error) => {
            return (
                status_for_error(&error),
                Json(error_body(error, hostname, instance_id)),
            );
        }
    };

    let switched_at = Utc::now();
    tracing::info!(
        target = "audit.claude_accounts",
        timestamp = %switched_at.to_rfc3339(),
        node = %hostname,
        instance_id = ?instance_id,
        requested_account = %body.account,
        switched = result.switched,
        from = ?result.from,
        to = ?result.to,
        reason = ?result.reason,
        "Claude account switch requested via dashboard"
    );

    let rate_limit_refresh = if let Some(pg_pool) = state.pg_pool_ref() {
        match serde_json::to_value(
            crate::server::trigger_claude_rate_limit_refresh_if_leader(pg_pool).await,
        ) {
            Ok(value) => value,
            Err(error) => json!({
                "triggered": false,
                "dispatchGateRefreshed": false,
                "refreshedAt": null,
                "reason": "serialization_failure",
                "error": error.to_string(),
            }),
        }
    } else {
        json!({
            "triggered": false,
            "dispatchGateRefreshed": false,
            "refreshedAt": null,
            "reason": "postgres_pool_unavailable",
            "error": null,
        })
    };

    let mut response = match serde_json::to_value(&result) {
        Ok(Value::Object(map)) => map,
        Ok(value) => {
            let mut map = Map::new();
            map.insert("cswap".to_string(), value);
            map
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "execution_failure",
                    "code": "serialization_failure",
                    "hostname": hostname,
                    "instanceId": instance_id,
                    "switchedAt": switched_at,
                    "error": format!("serialize cswap switch response: {error}"),
                })),
            );
        }
    };

    response.insert("schemaVersion".to_string(), json!(1));
    response.insert("status".to_string(), json!("ok"));
    response.insert("hostname".to_string(), json!(hostname));
    response.insert("instanceId".to_string(), json!(instance_id));
    response.insert("switchedAt".to_string(), json!(switched_at));
    response.insert("rateLimitRefresh".to_string(), rate_limit_refresh);

    (StatusCode::OK, Json(Value::Object(response)))
}

fn status_for_error(error: &CswapError) -> StatusCode {
    match error {
        CswapError::NotInstalled => StatusCode::SERVICE_UNAVAILABLE,
        CswapError::Timeout { .. } => StatusCode::GATEWAY_TIMEOUT,
        CswapError::AccountRequired => StatusCode::BAD_REQUEST,
        CswapError::SwitchInProgress => StatusCode::CONFLICT,
        CswapError::CommandFailed { .. }
        | CswapError::Exec(_)
        | CswapError::InvalidUtf8(_)
        | CswapError::Json(_) => StatusCode::BAD_GATEWAY,
    }
}

fn error_body(error: CswapError, hostname: String, instance_id: Option<String>) -> Value {
    let status = match error {
        CswapError::NotInstalled => "not_installed",
        CswapError::SwitchInProgress => "switch_in_progress",
        CswapError::Timeout { .. }
        | CswapError::CommandFailed { .. }
        | CswapError::Exec(_)
        | CswapError::InvalidUtf8(_)
        | CswapError::Json(_) => "execution_failure",
        CswapError::AccountRequired => "bad_request",
    };
    json!({
        "schemaVersion": 1,
        "status": status,
        "code": error.code(),
        "hostname": hostname,
        "instanceId": instance_id,
        "fetchedAt": Utc::now(),
        "error": error.to_string(),
        "install": {
            "command": "uv tool install claude-swap",
            "binaryHint": "~/.local/bin/cswap"
        }
    })
}
