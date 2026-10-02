use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::db::campaigns::{self, Campaign, CampaignError, CampaignInput, CampaignStatus};
use crate::error::{AppError, AppResult, ErrorCode};

/// A ledger write carries the whole DAG, so campaign routes need far more than the
/// axum default body limit; below it every checkpoint write is refused.
pub const LEDGER_BODY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Replaces the plain-text limit rejection with the limit that was hit and the remedy.
pub async fn body_limit_envelope(response: Response) -> Response {
    if response.status() != StatusCode::PAYLOAD_TOO_LARGE {
        return response;
    }
    AppError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::Validation,
        format!(
            "campaign request body exceeds the {LEDGER_BODY_LIMIT_BYTES} byte ledger limit; \
             split the campaign, or keep the bulky evidence in a durable external artifact and \
             store only its reference. Revision history is pruned and cannot hold it."
        ),
    )
    .with_context("limit_bytes", LEDGER_BODY_LIMIT_BYTES)
    .into_response()
}

#[derive(Deserialize)]
pub struct CreateCampaign {
    pub id: Option<String>,
    #[serde(flatten)]
    pub campaign: CampaignInput,
}

#[derive(Deserialize)]
pub struct ReplaceCampaign {
    pub expected_revision: i64,
    #[serde(flatten)]
    pub campaign: CampaignInput,
}

#[derive(Default, Deserialize)]
pub struct ListQuery {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

fn pool(state: &AppState) -> AppResult<&sqlx::PgPool> {
    state.pg_pool_ref().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "canonical PostgreSQL pool unavailable",
        )
    })
}

fn error(error: CampaignError) -> AppError {
    match error {
        CampaignError::Validation(message) => AppError::bad_request(message),
        CampaignError::NotFound => AppError::not_found("campaign not found"),
        CampaignError::Conflict => {
            AppError::conflict("campaign revision conflict; reload before retrying")
        }
        CampaignError::Database(error) => {
            tracing::error!(%error, "campaign ledger database operation failed");
            AppError::internal("campaign ledger database operation failed")
                .with_code(ErrorCode::Database)
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> AppResult<Json<Value>> {
    let limit = i64::from(query.limit.unwrap_or(100).clamp(1, 500));
    let offset = i64::from(query.offset.unwrap_or(0));
    let pool = pool(&state)?;
    let campaigns = campaigns::list(pool, limit, offset).await.map_err(error)?;
    let live = campaigns::live_status(pool, &campaigns)
        .await
        .map_err(error)?;
    Ok(Json(
        json!({ "campaigns": campaigns, "live": live, "limit": limit, "offset": offset }),
    ))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    let pool = pool(&state)?;
    let campaign = campaigns::get(pool, &id).await.map_err(error)?;
    let mut live = campaigns::live_status(pool, std::slice::from_ref(&campaign))
        .await
        .map_err(error)?;
    let live = live.remove(&campaign.id).unwrap_or_default();
    Ok(Json(json!({ "campaign": campaign, "live": live })))
}

pub async fn history(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<Value>> {
    Ok(Json(
        json!({ "revisions": campaigns::history(pool(&state)?, &id).await.map_err(error)? }),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateCampaign>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let id = body.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let pool = pool(&state)?;
    let campaign = campaigns::create(pool, id, body.campaign)
        .await
        .map_err(error)?;
    Ok((
        StatusCode::CREATED,
        Json(saved_with_handoff(&state, pool, campaign).await),
    ))
}

pub async fn replace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReplaceCampaign>,
) -> AppResult<Json<Value>> {
    let pool = pool(&state)?;
    let campaign = campaigns::replace(pool, &id, body.expected_revision, body.campaign)
        .await
        .map_err(error)?;
    Ok(Json(saved_with_handoff(&state, pool, campaign).await))
}

/// A saved write already succeeded, so a failed handoff is reported beside it, not as an error.
async fn saved_with_handoff(state: &AppState, pool: &sqlx::PgPool, campaign: Campaign) -> Value {
    if !campaign.auto_queue || campaign.status != CampaignStatus::Active {
        return json!({ "campaign": campaign });
    }
    match crate::services::auto_queue::route::hand_off_ready_nodes_pg(
        pool,
        &state.engine,
        &campaign,
    )
    .await
    {
        Ok(handoff) => json!({ "campaign": campaign, "handoff": handoff }),
        Err(message) => {
            tracing::warn!(campaign = %campaign.id, %message, "campaign handoff after save failed");
            json!({ "campaign": campaign, "handoff_error": message })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    fn router_with_pool(pool: Option<sqlx::PgPool>) -> Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some("campaign-test-token".into());
        let engine = crate::engine::PolicyEngine::new(&config).expect("test engine");
        let broadcast_tx = crate::eventbus::new_broadcast();
        let state = AppState {
            pg_pool: pool,
            engine,
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: None,
            cluster_instance_id: None,
        };
        super::super::domains::admin::router(state.clone()).with_state(state)
    }

    /// Sends `filler_bytes` of JSON payload to the ledger PUT and returns the raw response.
    async fn put_ledger_payload(app: &Router, filler_bytes: usize) -> (StatusCode, Vec<u8>) {
        let body = format!(
            r#"{{"expected_revision":1,"title":"size probe","status":"active","round":1,"nodes":[],"description":"{}"}}"#,
            "x".repeat(filler_bytes)
        );
        let request = Request::builder()
            .method(Method::PUT)
            .uri("/campaigns/size-probe")
            .header(header::AUTHORIZATION, "Bearer campaign-test-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        (status, bytes)
    }

    fn app(pool: sqlx::PgPool) -> Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some("campaign-test-token".into());
        let engine = crate::engine::PolicyEngine::new(&config).expect("test engine");
        let broadcast_tx = crate::eventbus::new_broadcast();
        let state = AppState {
            pg_pool: Some(pool),
            engine,
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: None,
            cluster_instance_id: None,
        };
        super::super::domains::admin::router(state.clone()).with_state(state)
    }

    async fn request(
        app: &Router,
        method: Method,
        path: &str,
        body: Option<Value>,
        auth: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if auth {
            request = request.header(header::AUTHORIZATION, "Bearer campaign-test-token");
        }
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn postgres_campaign_http_auth_conflict_and_history_pg() {
        let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        let app = app(pool.clone());
        let body = json!({"id": "api-test", "title": "API checkpoint", "status": "active", "round": 1, "nodes": []});
        assert_eq!(
            request(&app, Method::GET, "/campaigns", None, false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, Method::POST, "/campaigns", Some(body.clone()), false)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let (status, created) = request(&app, Method::POST, "/campaigns", Some(body), true).await;
        assert_eq!(status, StatusCode::CREATED);
        let mut update = created["campaign"].clone();
        update["expected_revision"] = json!(1);
        update["status"] = json!("paused");
        let (status, updated) = request(
            &app,
            Method::PUT,
            "/campaigns/api-test",
            Some(update.clone()),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["campaign"]["revision"], 2);
        assert_eq!(
            request(&app, Method::PUT, "/campaigns/api-test", Some(update), true)
                .await
                .0,
            StatusCode::CONFLICT
        );
        let (_, history) =
            request(&app, Method::GET, "/campaigns/api-test/history", None, true).await;
        assert_eq!(history["revisions"].as_array().unwrap().len(), 2);
        let mut opt_in = updated["campaign"].clone();
        opt_in["expected_revision"] = json!(2);
        opt_in["status"] = json!("active");
        opt_in["auto_queue"] = json!(true);
        let (status, saved) =
            request(&app, Method::PUT, "/campaigns/api-test", Some(opt_in), true).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["campaign"]["auto_queue"], true);
        assert_eq!(
            saved["handoff"],
            json!({"queued": [], "waiting": []}),
            "an active opted-in save hands off in the same response"
        );
        assert_eq!(
            request(&app, Method::GET, "/campaigns/absent", None, true)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        drop(app);
        pool.close().await;
        fixture.drop().await;
    }

    #[tokio::test]
    async fn ledger_put_accepts_a_document_larger_than_the_axum_default_limit() {
        let app = router_with_pool(None);
        let (status, _) = put_ledger_payload(&app, 4 * 1024 * 1024).await;
        assert_ne!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE,
            "a 4 MiB ledger document must clear the raised campaign body limit"
        );
    }

    #[tokio::test]
    async fn ledger_put_over_the_limit_reports_the_limit_and_the_remedy() {
        let app = router_with_pool(None);
        let (status, bytes) = put_ledger_payload(&app, LEDGER_BODY_LIMIT_BYTES + 1).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let payload: Value = serde_json::from_slice(&bytes)
            .expect("an over-limit ledger write must answer with the JSON error envelope");
        assert_eq!(
            payload["context"]["limit_bytes"],
            json!(LEDGER_BODY_LIMIT_BYTES)
        );
        let message = payload["error"].as_str().unwrap_or_default();
        assert!(
            message.contains(&LEDGER_BODY_LIMIT_BYTES.to_string()) && message.contains("split"),
            "the rejection must name the limit and a non-lossy remedy, got {message}"
        );
        assert!(
            !message.contains("to history"),
            "pruned history cannot be offered as somewhere to move evidence, got {message}"
        );
    }

    /// Pinned to the literal so a later reduction cannot reintroduce the 413 wall
    /// while the limit-expressed tests still pass.
    #[test]
    fn ledger_body_limit_stays_at_sixteen_mebibytes() {
        assert_eq!(LEDGER_BODY_LIMIT_BYTES, 16 * 1024 * 1024);
    }

    #[tokio::test]
    async fn postgres_ledger_round_trips_a_document_past_the_axum_default_limit_pg() {
        let fixture = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        let app = router_with_pool(Some(pool.clone()));
        let created = json!({"id": "oversize", "title": "Oversize ledger", "status": "active",
                             "round": 1, "nodes": []});
        assert_eq!(
            request(&app, Method::POST, "/campaigns", Some(created), true)
                .await
                .0,
            StatusCode::CREATED
        );

        // Past axum's 2 MiB default, so a pass here proves the raised limit carries a
        // real document through parse, validation and persistence, not merely past the layer.
        let bulk = "d".repeat(3 * 1024 * 1024);
        let update = json!({"expected_revision": 1, "title": "Oversize ledger", "status": "active",
                            "round": 1,
                            "nodes": [{"id": "bulky", "title": "Bulky node", "status": "pending",
                                       "stage": "implement", "round": 1, "details": bulk.clone()}]});
        let body = serde_json::to_vec(&update).expect("serialize oversize body");
        assert!(
            body.len() > 2 * 1024 * 1024,
            "probe must exceed the axum default"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri("/campaigns/oversize")
                    .header(header::AUTHORIZATION, "Bearer campaign-test-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let stored = crate::db::campaigns::get(&pool, "oversize")
            .await
            .expect("stored oversize campaign");
        assert_eq!(stored.revision, 2);
        assert_eq!(stored.nodes[0].input.details, bulk);
        pool.close().await;
        fixture.drop().await;
    }
}
