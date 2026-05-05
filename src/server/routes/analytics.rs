use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration as StdDuration, Instant},
};

use super::{
    AppState, skill_usage_analytics::collect_skill_usage_pg, skills_api::sync_skills_from_disk_pg,
};
use crate::server::dto::analytics::AnalyticsErrorResponse;
use crate::services::analytics as analytics_service;

pub(crate) use crate::services::analytics::build_rate_limit_provider_payloads_pg;

const ANALYTICS_CACHE_TTL: StdDuration = StdDuration::from_secs(60);
const ANALYTICS_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Clone)]
struct CachedJson {
    cached_at: Instant,
    body: Value,
    etag: String,
}

fn analytics_response_cache() -> &'static Mutex<HashMap<String, CachedJson>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedJson>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn read_analytics_cache(key: &str) -> Option<CachedJson> {
    let cache = analytics_response_cache().lock().ok()?;
    let entry = cache.get(key)?.clone();
    if entry.cached_at.elapsed() > ANALYTICS_CACHE_TTL {
        return None;
    }
    Some(entry)
}

fn write_analytics_cache(key: String, body: Value) -> CachedJson {
    let etag = compute_etag(&body);
    let entry = CachedJson {
        cached_at: Instant::now(),
        body,
        etag,
    };
    if let Ok(mut cache) = analytics_response_cache().lock() {
        prune_expired_analytics_cache_entries(&mut cache);
        cache.insert(key, entry.clone());
    }
    entry
}

fn prune_expired_analytics_cache_entries(cache: &mut HashMap<String, CachedJson>) {
    cache.retain(|_, entry| entry.cached_at.elapsed() <= ANALYTICS_CACHE_TTL);
    if cache.len() <= ANALYTICS_CACHE_MAX_ENTRIES {
        return;
    }
    let mut by_age: Vec<(String, Instant)> = cache
        .iter()
        .map(|(k, v)| (k.clone(), v.cached_at))
        .collect();
    by_age.sort_by_key(|(_, t)| *t);
    let drop_count = cache.len() - ANALYTICS_CACHE_MAX_ENTRIES;
    for (key, _) in by_age.into_iter().take(drop_count) {
        cache.remove(&key);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn reset_analytics_cache() {
    if let Ok(mut cache) = analytics_response_cache().lock() {
        cache.clear();
    }
}

fn compute_etag(body: &Value) -> String {
    use std::hash::{Hash, Hasher};
    let serialized = serde_json::to_string(body).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

fn response_value<T: Serialize>(body: T) -> Value {
    serde_json::to_value(body).expect("analytics response DTO serializes")
}

fn json_response<T: Serialize>(status: StatusCode, body: T) -> (StatusCode, Json<Value>) {
    (status, Json(response_value(body)))
}

fn build_analytics_response(entry: &CachedJson, cache_state: &'static str) -> Response {
    let mut response = (StatusCode::OK, Json(entry.body.clone())).into_response();
    let headers = response.headers_mut();
    headers.insert(
        "Cache-Control",
        HeaderValue::from_static("private, max-age=30, stale-while-revalidate=120"),
    );
    if let Ok(value) = HeaderValue::from_str(&entry.etag) {
        headers.insert("ETag", value);
    }
    headers.insert("X-Analytics-Cache", HeaderValue::from_static(cache_state));
    response
}

fn cache_miss_response<T: Serialize>(cache_key: String, body: T) -> Response {
    let entry = write_analytics_cache(cache_key, response_value(body));
    build_analytics_response(&entry, "miss")
}

fn analytics_error(status: StatusCode, message: String) -> Response {
    json_response(status, AnalyticsErrorResponse { error: message }).into_response()
}

fn analytics_json_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<Value>) {
    json_response(
        status,
        AnalyticsErrorResponse {
            error: message.into(),
        },
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct AnalyticsQuery {
    pub provider: Option<String>,
    #[serde(rename = "channelId")]
    pub channel_id: Option<String>,
    #[serde(rename = "eventType")]
    pub event_type: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct QualityEventsQuery {
    pub agent_id: Option<String>,
    pub days: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct InvariantsQuery {
    pub provider: Option<String>,
    #[serde(rename = "channelId")]
    pub channel_id: Option<String>,
    pub invariant: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ObservabilityQuery {
    #[serde(rename = "recentLimit")]
    pub recent_limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PolicyHooksQuery {
    #[serde(rename = "policyName")]
    pub policy_name: Option<String>,
    #[serde(rename = "hookName")]
    pub hook_name: Option<String>,
    #[serde(rename = "lastMinutes")]
    pub last_minutes: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct AchievementsQuery {
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HeatmapQuery {
    date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuditLogsQuery {
    limit: Option<i64>,
    #[serde(rename = "entityType")]
    entity_type: Option<String>,
    #[serde(rename = "entityId")]
    entity_id: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SkillsTrendQuery {
    days: Option<i64>,
}

/// GET /api/analytics
pub async fn analytics(
    State(state): State<AppState>,
    Query(params): Query<AnalyticsQuery>,
) -> Response {
    let filters = crate::services::observability::AnalyticsFilters {
        provider: params.provider.clone(),
        channel_id: params.channel_id.clone(),
        event_type: params.event_type.clone(),
        event_limit: params.limit.unwrap_or(100),
        counter_limit: 200,
    };
    let cache_key = format!(
        "analytics|{}|{}|{}|{}",
        filters.provider.as_deref().unwrap_or(""),
        filters.channel_id.as_deref().unwrap_or(""),
        filters.event_type.as_deref().unwrap_or(""),
        filters.event_limit,
    );
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };
    match analytics_service::query_analytics_pg(pool, &filters).await {
        Ok(body) => cache_miss_response(cache_key, body),
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query analytics: {error}"),
        ),
    }
}

/// GET /api/quality/events
pub async fn quality_events(
    State(state): State<AppState>,
    Query(params): Query<QualityEventsQuery>,
) -> Response {
    let filters = crate::services::observability::AgentQualityFilters {
        agent_id: params.agent_id.clone(),
        days: params.days.unwrap_or(7),
        limit: params.limit.unwrap_or(200),
    };
    let cache_key = format!(
        "quality_events|{}|{}|{}",
        filters.agent_id.as_deref().unwrap_or(""),
        filters.days,
        filters.limit,
    );
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };
    match analytics_service::query_agent_quality_events_pg(pool, &filters).await {
        Ok(body) => cache_miss_response(cache_key, body),
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query agent quality events: {error}"),
        ),
    }
}

/// GET /api/analytics/observability
pub async fn observability(Query(params): Query<ObservabilityQuery>) -> (StatusCode, Json<Value>) {
    json_response(
        StatusCode::OK,
        analytics_service::observability_response(params.recent_limit.unwrap_or(100)),
    )
}

/// GET /api/analytics/policy-hooks
pub async fn policy_hooks(Query(params): Query<PolicyHooksQuery>) -> (StatusCode, Json<Value>) {
    json_response(
        StatusCode::OK,
        analytics_service::policy_hooks_response(analytics_service::PolicyHooksParams {
            policy_name: params.policy_name,
            hook_name: params.hook_name,
            last_minutes: params.last_minutes,
            limit: params.limit.unwrap_or(200).min(2000),
        }),
    )
}

/// GET /api/analytics/invariants
pub async fn invariants(
    State(state): State<AppState>,
    Query(params): Query<InvariantsQuery>,
) -> Response {
    let filters = crate::services::observability::InvariantAnalyticsFilters {
        provider: params.provider.clone(),
        channel_id: params.channel_id.clone(),
        invariant: params.invariant.clone(),
        limit: params.limit.unwrap_or(50),
    };
    let cache_key = format!(
        "invariants|{}|{}|{}|{}",
        filters.provider.as_deref().unwrap_or(""),
        filters.channel_id.as_deref().unwrap_or(""),
        filters.invariant.as_deref().unwrap_or(""),
        filters.limit,
    );
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };
    match analytics_service::query_invariants_pg(pool, &filters).await {
        Ok(body) => cache_miss_response(cache_key, body),
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query invariant analytics: {error}"),
        ),
    }
}

/// GET /api/streaks
pub async fn streaks(State(state): State<AppState>) -> Response {
    let cache_key = "streaks".to_string();
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };
    match analytics_service::streaks_pg(pool).await {
        Ok(body) => cache_miss_response(cache_key, body),
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query prepare failed: {error}"),
        ),
    }
}

/// GET /api/achievements
pub async fn achievements(
    State(state): State<AppState>,
    Query(params): Query<AchievementsQuery>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return analytics_json_error(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };

    match analytics_service::achievements_pg(pool, params.agent_id.as_deref()).await {
        Ok(body) => json_response(StatusCode::OK, body),
        Err(error) => analytics_json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{error}")),
    }
}

/// GET /api/activity-heatmap?date=2026-03-19
pub async fn activity_heatmap(
    State(state): State<AppState>,
    Query(params): Query<HeatmapQuery>,
) -> Response {
    let date = params
        .date
        .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%d").to_string());
    let cache_key = format!("activity_heatmap|{}", date);
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };
    match analytics_service::activity_heatmap_pg(pool, date).await {
        Ok(body) => cache_miss_response(cache_key, body),
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query activity heatmap: {error}"),
        ),
    }
}

/// GET /api/audit-logs?limit=20&entityType=...&entityId=...&agentId=...
pub async fn audit_logs(
    State(state): State<AppState>,
    Query(params): Query<AuditLogsQuery>,
) -> (StatusCode, Json<Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return analytics_json_error(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };

    let body = analytics_service::audit_logs_pg(
        pool,
        analytics_service::AuditLogsParams {
            limit: params.limit.unwrap_or(20),
            entity_type: params.entity_type.as_deref(),
            entity_id: params.entity_id.as_deref(),
            agent_id: params.agent_id.as_deref(),
        },
    )
    .await;
    json_response(StatusCode::OK, body)
}

/// GET /api/machine-status
pub async fn machine_status(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    json_response(
        StatusCode::OK,
        analytics_service::machine_status(state.pg_pool_ref()).await,
    )
}

/// GET /api/rate-limits
pub async fn rate_limits(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let now = chrono::Utc::now().timestamp();
    let Some(pool) = state.pg_pool_ref() else {
        return analytics_json_error(StatusCode::SERVICE_UNAVAILABLE, "postgres pool unavailable");
    };

    json_response(
        StatusCode::OK,
        analytics_service::rate_limits_pg(pool, now).await,
    )
}

/// GET /api/skills-trend?days=30
pub async fn skills_trend(
    State(state): State<AppState>,
    Query(params): Query<SkillsTrendQuery>,
) -> Response {
    let days = params.days.unwrap_or(30).clamp(1, 90);
    let cache_key = format!("skills_trend|{}", days);
    if let Some(entry) = read_analytics_cache(&cache_key) {
        return build_analytics_response(&entry, "hit");
    }

    let Some(pool) = state.pg_pool_ref() else {
        return analytics_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "postgres pool unavailable".to_string(),
        );
    };

    if let Err(error) = sync_skills_from_disk_pg(pool).await {
        return analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("skill sync failed: {error}"),
        );
    }
    let usage = match collect_skill_usage_pg(pool, Some(days)).await {
        Ok(data) => data,
        Err(error) => {
            return analytics_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("usage query failed: {error}"),
            );
        }
    };

    cache_miss_response(
        cache_key,
        analytics_service::skills_trend_from_days(usage.into_iter().map(|record| record.day)),
    )
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use crate::services::analytics::load_machine_config;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
#[path = "analytics_tests.rs"]
mod tests;
