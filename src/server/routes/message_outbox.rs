use std::{collections::BTreeSet, net::SocketAddr};

use axum::{
    Json,
    extract::{ConnectInfo, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{AppState, health_api::local_or_configured_control_endpoint_allowed};
use crate::services::message_outbox_recovery::{
    RecoveryError, inspect_failed_rows, redrive_failed_rows,
};

const MAX_EXACT_IDS: usize = 50;
const MONITOR_ALERT_DEDUPE_TTL_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Deserialize)]
pub struct FailedQuery {
    ids: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedriveRequest {
    ids: Vec<i64>,
    idempotency_key: String,
    reason: String,
    #[serde(default = "default_dry_run")]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorAlertRequest {
    target: String,
    content: String,
    action_id: String,
    action: String,
}

fn default_dry_run() -> bool {
    true
}

fn exact_ids(ids: Vec<i64>) -> Result<Vec<i64>, &'static str> {
    if ids.is_empty() {
        return Err("ids must contain at least one exact message_outbox id");
    }
    if ids.len() > MAX_EXACT_IDS {
        return Err("ids exceeds the maximum of 50 exact message_outbox ids");
    }
    if ids.iter().any(|id| *id <= 0) {
        return Err("ids must contain only positive message_outbox ids");
    }
    let unique: BTreeSet<_> = ids.iter().copied().collect();
    if unique.len() != ids.len() {
        return Err("ids must not contain duplicates");
    }
    Ok(unique.into_iter().collect())
}

fn query_ids(value: &str) -> Result<Vec<i64>, &'static str> {
    if value.trim().is_empty() {
        return Err("ids query parameter is required");
    }
    let parsed = value
        .split(',')
        .map(|part| part.trim().parse::<i64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "ids must be a comma-separated list of positive integers")?;
    exact_ids(parsed)
}

fn control_allowed(state: &AppState, peer: SocketAddr) -> Result<(), Response> {
    if local_or_configured_control_endpoint_allowed(&state.config, Some(peer)) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"ok": false, "error": "auth_token required for non-loopback host"})),
        )
            .into_response())
    }
}

fn normalized_monitor_alert(
    request: MonitorAlertRequest,
) -> Result<(String, String, String, &'static str), &'static str> {
    let target = request.target.trim();
    let channel_id = target
        .strip_prefix("channel:")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0);
    if channel_id.is_none() {
        return Err("target must be channel:<positive Discord channel id>");
    }
    let content = request.content.trim();
    if content.is_empty() || content.len() > 2_000 {
        return Err("content is required and must be at most 2000 bytes");
    }
    let action_id = request.action_id.trim();
    if action_id.len() != 32
        || !action_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("action_id must be 32 lowercase hexadecimal characters");
    }
    let reason_code = match request.action.trim() {
        "alert" => "auto_queue.monitor_alert",
        "recovery" => "auto_queue.monitor_recovery",
        _ => return Err("action must be alert or recovery"),
    };
    Ok((
        target.to_string(),
        content.to_string(),
        action_id.to_string(),
        reason_code,
    ))
}

pub async fn enqueue_monitor_alert(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<MonitorAlertRequest>,
) -> Response {
    if let Err(response) = control_allowed(&state, peer) {
        return response;
    }
    let (target, content, action_id, reason_code) = match normalized_monitor_alert(request) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
                .into_response();
        }
    };
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "pg pool unavailable"})),
        )
            .into_response();
    };
    let session_key = format!("auto_queue_monitor:{action_id}");
    match crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: &content,
            bot: "notify",
            source: "auto-queue-monitor",
            reason_code: Some(reason_code),
            session_key: Some(&session_key),
        },
        MONITOR_ALERT_DEDUPE_TTL_SECS,
    )
    .await
    {
        Ok(enqueued) => Json(serde_json::json!({
            "ok": true,
            "enqueued": enqueued,
            "action_id": action_id
        }))
        .into_response(),
        Err(error) => {
            tracing::error!(%error, %action_id, "monitor alert outbox enqueue failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"ok": false, "error": "monitor alert enqueue failed"})),
            )
                .into_response()
        }
    }
}

pub async fn list_failed(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Query(query): Query<FailedQuery>,
) -> Response {
    if let Err(response) = control_allowed(&state, peer) {
        return response;
    }
    let ids = match query_ids(&query.ids) {
        Ok(ids) => ids,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
                .into_response();
        }
    };
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "pg pool unavailable"})),
        )
            .into_response();
    };
    match inspect_failed_rows(pool, &ids).await {
        Ok(rows) => {
            let found: BTreeSet<_> = rows.iter().map(|row| row.id).collect();
            let missing_ids: Vec<_> = ids.into_iter().filter(|id| !found.contains(id)).collect();
            Json(serde_json::json!({"ok": true, "count": rows.len(), "rows": rows, "missing_ids": missing_ids})).into_response()
        }
        Err(error) => server_error(error),
    }
}

pub async fn redrive_failed(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(request): Json<RedriveRequest>,
) -> Response {
    if let Err(response) = control_allowed(&state, peer) {
        return response;
    }
    let ids = match exact_ids(request.ids) {
        Ok(ids) => ids,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"ok": false, "error": error})),
            )
                .into_response();
        }
    };
    let key = request.idempotency_key.trim();
    let reason = request.reason.trim();
    if key.is_empty() || key.len() > 128 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": "idempotency_key is required and must be at most 128 bytes"}))).into_response();
    }
    if reason.is_empty() || reason.len() > 500 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"ok": false, "error": "reason is required and must be at most 500 bytes"}))).into_response();
    }
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"ok": false, "error": "pg pool unavailable"})),
        )
            .into_response();
    };
    match redrive_failed_rows(pool, &ids, key, reason, request.dry_run).await {
        Ok(results) => Json(serde_json::json!({"ok": true, "dry_run": request.dry_run, "idempotency_key": key, "results": results})).into_response(),
        Err(RecoveryError::SourceNotAllowed { id, label }) => (StatusCode::CONFLICT, Json(serde_json::json!({"ok": false, "error": format!("message_outbox row {id} source `{label}` is not registered for LoopbackInternal"), "code": "source_not_allowed", "id": id}))).into_response(),
        Err(error) => server_error(error),
    }
}

fn server_error(error: RecoveryError) -> Response {
    tracing::error!(error = %error, "message_outbox recovery API failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"ok": false, "error": "message_outbox recovery failed"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::{RedriveRequest, exact_ids, query_ids};

    #[test]
    fn exact_id_contract_rejects_mass_and_ambiguous_inputs() {
        assert_eq!(
            query_ids("13651,13652,13653").unwrap(),
            vec![13651, 13652, 13653]
        );
        assert!(query_ids("").is_err());
        assert!(query_ids("all").is_err());
        assert!(exact_ids(vec![]).is_err());
        assert!(exact_ids(vec![13651, 13651]).is_err());
        assert!(exact_ids(vec![0]).is_err());
        assert!(exact_ids((1..=51).collect()).is_err());
    }

    #[test]
    fn redrive_contract_defaults_to_dry_run_and_denies_mass_field() {
        let request: RedriveRequest = serde_json::from_value(serde_json::json!({
            "ids": [13651],
            "idempotency_key": "issue-4424-v1",
            "reason": "verified incident"
        }))
        .unwrap();
        assert!(request.dry_run);
        assert!(
            serde_json::from_value::<RedriveRequest>(serde_json::json!({
                "ids": [13651], "idempotency_key": "key", "reason": "reason", "all": true
            }))
            .is_err()
        );
    }

    fn request(method: &str, uri: &str, body: &str, peer: &str) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        request.extensions_mut().insert(axum::extract::ConnectInfo(
            peer.parse::<std::net::SocketAddr>().unwrap(),
        ));
        request
    }

    fn app_with_pg(
        mut config: crate::config::Config,
        pg_pool: Option<sqlx::PgPool>,
    ) -> axum::Router {
        config.policies.hot_reload = false;
        let engine = crate::engine::PolicyEngine::new(&config).unwrap();
        let tx = crate::server::ws::new_broadcast();
        let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
        crate::server::routes::api_router_with_pg(engine, config, tx, buffer, None, pg_pool)
    }

    fn app(config: crate::config::Config) -> axum::Router {
        app_with_pg(config, None)
    }

    #[tokio::test]
    async fn protected_message_outbox_routes_are_registered_contract() {
        let config = crate::config::Config::default();
        let get_response = app(config.clone())
            .oneshot(request(
                "GET",
                "/message-outbox/failed?ids=13651,13652,13653",
                "",
                "127.0.0.1:8791",
            ))
            .await
            .unwrap();
        assert_eq!(get_response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let post_response = app(config)
            .oneshot(request(
                "POST",
                "/message-outbox/failed/redrive",
                r#"{"ids":[13651],"idempotency_key":"issue-4424-v1","reason":"verified"}"#,
                "127.0.0.1:8791",
            ))
            .await
            .unwrap();
        assert_eq!(post_response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let monitor_response = app(crate::config::Config::default())
            .oneshot(request(
                "POST",
                "/message-outbox/monitor-alerts",
                r#"{"target":"channel:123","content":"alert","action_id":"0123456789abcdef0123456789abcdef","action":"alert"}"#,
                "127.0.0.1:8791",
            ))
            .await
            .unwrap();
        assert_eq!(monitor_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn monitor_alert_action_id_is_durable_and_idempotent_pg() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let router = app_with_pg(crate::config::Config::default(), Some(pool.clone()));
        let body = r#"{"target":"channel:123","content":"monitor alert","action_id":"0123456789abcdef0123456789abcdef","action":"alert"}"#;

        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(request(
                    "POST",
                    "/message-outbox/monitor-alerts",
                    body,
                    "127.0.0.1:8791",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let invalid = router
            .oneshot(request(
                "POST",
                "/message-outbox/monitor-alerts",
                r#"{"target":"channel:123","content":"monitor alert","action_id":"bad","action":"alert"}"#,
                "127.0.0.1:8791",
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let rows = crate::services::message_outbox::monitor_alert_rows_for_test(&pool)
            .await
            .expect("load monitor outbox rows");
        assert_eq!(
            rows,
            vec![(
                "auto-queue-monitor".to_string(),
                "notify".to_string(),
                "auto_queue.monitor_alert".to_string(),
                "auto_queue_monitor:0123456789abcdef0123456789abcdef".to_string(),
                "channel:123".to_string(),
                "monitor alert".to_string(),
                true,
            )]
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
