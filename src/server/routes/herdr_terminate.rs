//! `POST /api/herdr/terminate`: an operator's explicit close of one Herdr execution on this node.

use std::net::SocketAddr;

use axum::extract::rejection::JsonRejection;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header::ORIGIN};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::api_caller_observability::{AuthStrength, RequestPrincipal};
use crate::services::discord::herdr_terminate::{
    OperatorTerminate, terminate_enabled, terminate_registered_herdr,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminateBody {
    session_key: String,
    execution_nonce: String,
    reason: String,
}

type Reply = (StatusCode, Json<Value>);

fn refused(status: StatusCode, result: &str, detail: &str) -> Reply {
    (
        status,
        Json(json!({"ok": false, "result": result, "detail": detail})),
    )
}

/// Peer, browser origin, principal, switch, body, dependencies, then the adapter: nothing before
/// the switch reads PG, the runtime registry or Herdr.
pub(super) async fn terminate(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    principal: Option<Extension<RequestPrincipal>>,
    headers: HeaderMap,
    body: Result<Json<TerminateBody>, JsonRejection>,
) -> Reply {
    // Browsers send `Origin` on a cross-origin POST; the CLI never does.
    if !peer.ip().is_loopback() || headers.contains_key(ORIGIN) {
        return refused(StatusCode::FORBIDDEN, "forbidden", "local operator only");
    }
    // Same mode test as the auth middleware: a non-empty token requires its Bearer.
    let token_mode = state
        .config
        .server
        .auth_token
        .as_deref()
        .is_some_and(|t| !t.is_empty());
    let admitted = principal.is_some_and(|Extension(principal)| {
        !token_mode || principal.auth_strength == AuthStrength::ServerAdmin
    });
    if !admitted {
        return refused(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "operator Bearer required",
        );
    }
    if !terminate_enabled() {
        return refused(StatusCode::CONFLICT, "disabled", "terminate disabled");
    }
    let Ok(Json(body)) = body else {
        return refused(StatusCode::BAD_REQUEST, "bad_request", "invalid body");
    };
    if [&body.session_key, &body.execution_nonce, &body.reason]
        .iter()
        .any(|field| field.trim().is_empty())
    {
        return refused(StatusCode::BAD_REQUEST, "bad_request", "empty field");
    }
    let (Some(registry), Some(pool)) = (state.health_registry.as_ref(), state.pg_pool.clone())
    else {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "runtime unavailable",
        );
    };
    let request = OperatorTerminate {
        session_key: body.session_key.clone(),
        execution_nonce: body.execution_nonce.clone(),
    };
    let result = terminate_registered_herdr(registry, pool, request, body.reason).await;
    let (name, detail) = result.outcome();
    let status = status_of(name);
    let reply = json!({
        "ok": status == StatusCode::OK,
        "result": name,
        "detail": detail,
        "session_key": body.session_key,
        "execution_nonce": body.execution_nonce,
    });
    (status, Json(reply))
}

/// Only a retire is success; a run that cannot say what it left is a server error.
fn status_of(result: &str) -> StatusCode {
    match result {
        "retired" => StatusCode::OK,
        "indeterminate" => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::CONFLICT,
    }
}

#[cfg(test)]
#[path = "herdr_terminate_tests.rs"]
mod tests;
