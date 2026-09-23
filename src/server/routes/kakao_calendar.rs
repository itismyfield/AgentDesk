//! Single trusted-operator API. Explicit Bearer proof is required even in local/no-auth mode.
use super::AppState;
use crate::db::calendar_sync as db;
use crate::error::{AppError, ErrorCode};
use crate::services::{calendar_sync as service, kakao::account};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

type ApiResult = Result<(StatusCode, Json<Value>), AppError>;
const DEFAULT_PAGE_SIZE: i64 = 25;
const MAX_PAGE_SIZE: i64 = 100;

fn operator<'a>(state: &'a AppState, headers: &HeaderMap) -> Result<&'a sqlx::PgPool, AppError> {
    verify_operator_token(state.config.server.auth_token.as_deref(), headers)?;
    if state.config.cluster.enabled {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "calendar supports a single credential-owning node",
        ));
    }
    state
        .pg_pool_ref()
        .ok_or_else(|| AppError::internal("PostgreSQL unavailable"))
}

fn verify_operator_token(expected: Option<&str>, headers: &HeaderMap) -> Result<(), AppError> {
    let expected = expected.filter(|value| !value.is_empty());
    let presented = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !expected
        .zip(presented)
        .is_some_and(|(a, b)| crate::utils::auth::constant_time_token_eq(a, b))
    {
        return Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::Config,
            "calendar requires operator Bearer authentication",
        ));
    }
    Ok(())
}

fn key(headers: &HeaderMap) -> Result<&str, AppError> {
    headers
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or_else(|| {
            AppError::bad_request(
                "Idempotency-Key with 1 to 128 printable ASCII characters required",
            )
        })
}

fn map_error(error: service::CalendarError) -> AppError {
    match error {
        service::CalendarError::Invalid(message) => AppError::bad_request(message),
        service::CalendarError::Storage(db::CalendarDbError::NotFound) => {
            AppError::not_found("managed event not found")
        }
        service::CalendarError::Storage(
            db::CalendarDbError::Conflict | db::CalendarDbError::Binding,
        ) => AppError::conflict("request, revision or account binding conflict"),
        service::CalendarError::Storage(_) => AppError::internal("calendar storage unavailable"),
        service::CalendarError::Provider(error) => AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            service::error_code(&error),
        ),
    }
}
fn storage(error: db::CalendarDbError) -> AppError {
    map_error(error.into())
}
fn ok(value: Value) -> ApiResult {
    Ok((StatusCode::OK, Json(value)))
}
fn accepted(receipt: db::Receipt) -> ApiResult {
    Ok((StatusCode::ACCEPTED, Json(json!(receipt))))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_requires_real_bearer_not_claimed_identity_or_local_origin() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", "admin".parse().unwrap());
        headers.insert("x-channel-id", "trusted".parse().unwrap());
        headers.insert("origin", "http://localhost:8080".parse().unwrap());
        assert!(verify_operator_token(None, &headers).is_err());
        assert!(verify_operator_token(Some("test-operator"), &headers).is_err());
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(verify_operator_token(Some("test-operator"), &headers).is_err());
        headers.insert("authorization", "Bearer test-operator".parse().unwrap());
        assert!(verify_operator_token(Some("test-operator"), &headers).is_ok());
        assert!(verify_operator_token(Some(""), &headers).is_err());
    }
    #[test]
    fn calendar_mutations_require_stable_nonempty_bounded_keys() {
        let mut headers = HeaderMap::new();
        assert!(key(&headers).is_err());
        headers.insert(
            "idempotency-key",
            "turn-123-calendar-create".parse().unwrap(),
        );
        assert_eq!(key(&headers).unwrap(), "turn-123-calendar-create");
        headers.insert("idempotency-key", " ".parse().unwrap());
        assert!(key(&headers).is_err());
    }
}

pub async fn accounts(State(state): State<AppState>, headers: HeaderMap) -> ApiResult {
    let pool = operator(&state, &headers)?;
    let ids = account::calendar_accounts().map_err(|e| map_error(e.into()))?;
    let accounts = db::account_checks(pool, &ids).await.map_err(storage)?;
    ok(json!({"accounts":accounts,"credentialBoundary":"single-node Unix private token store"}))
}

pub async fn check_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account): Path<String>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    service::check_account(pool, &account)
        .await
        .map_err(map_error)?;
    ok(
        json!({"accountId":account,"credentialVerified":true,"calendarConsent":true,"checkedAt":chrono::Utc::now()}),
    )
}

pub async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<service::CreateEvent>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    accepted(
        service::create(pool, key(&headers)?, body)
            .await
            .map_err(map_error)?,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    before: Option<Uuid>,
    limit: Option<i64>,
}
pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(page): Query<Page>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    let accounts = account::calendar_accounts().map_err(|e| map_error(e.into()))?;
    let limit = page.limit.unwrap_or(DEFAULT_PAGE_SIZE);
    if !(1..=MAX_PAGE_SIZE).contains(&limit) {
        return Err(AppError::bad_request(format!(
            "limit must be 1 to {MAX_PAGE_SIZE}"
        )));
    }
    let mut events = db::list(pool, &accounts, page.before, limit + 1)
        .await
        .map_err(storage)?;
    let next = if events.len() > limit as usize {
        Some(events[limit as usize - 1]["eventId"].clone())
    } else {
        None
    };
    events.truncate(limit as usize);
    ok(json!({"events":events,"nextCursor":next}))
}
pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<Uuid>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    service::authorize_event(pool, event)
        .await
        .map_err(map_error)?;
    ok(db::get(pool, event).await.map_err(storage)?)
}
pub async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<Uuid>,
    Json(body): Json<Value>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    accepted(
        service::mutate(pool, event, key(&headers)?, body, false)
            .await
            .map_err(map_error)?,
    )
}
pub async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<Uuid>,
    Json(body): Json<Value>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    accepted(
        service::mutate(pool, event, key(&headers)?, body, true)
            .await
            .map_err(map_error)?,
    )
}
pub async fn operations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(event): Path<Uuid>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    service::authorize_event(pool, event)
        .await
        .map_err(map_error)?;
    ok(json!({"operations":db::operations(pool,event).await.map_err(storage)?,"limit":200}))
}

pub async fn recover(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((event, operation)): Path<(Uuid, Uuid)>,
    Json(body): Json<service::Recovery>,
) -> ApiResult {
    let pool = operator(&state, &headers)?;
    let resolution = service::recover(pool, event, operation, body)
        .await
        .map_err(map_error)?;
    ok(json!({"operationId":operation,"resolution":resolution}))
}
