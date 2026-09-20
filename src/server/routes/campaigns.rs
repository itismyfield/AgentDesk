use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use crate::db::campaigns::{self, CampaignError, CampaignInput};
use crate::error::{AppError, AppResult, ErrorCode};

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
    let campaigns = campaigns::list(pool(&state)?, limit, offset)
        .await
        .map_err(error)?;
    Ok(Json(
        json!({ "campaigns": campaigns, "limit": limit, "offset": offset }),
    ))
}

pub async fn get(State(state): State<AppState>, Path(id): Path<String>) -> AppResult<Json<Value>> {
    Ok(Json(
        json!({ "campaign": campaigns::get(pool(&state)?, &id).await.map_err(error)? }),
    ))
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
    let campaign = campaigns::create(pool(&state)?, id, body.campaign)
        .await
        .map_err(error)?;
    Ok((StatusCode::CREATED, Json(json!({ "campaign": campaign }))))
}

pub async fn replace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ReplaceCampaign>,
) -> AppResult<Json<Value>> {
    let campaign = campaigns::replace(pool(&state)?, &id, body.expected_revision, body.campaign)
        .await
        .map_err(error)?;
    Ok(Json(json!({ "campaign": campaign })))
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
}
