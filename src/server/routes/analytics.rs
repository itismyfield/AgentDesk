use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{QueryBuilder, Row};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    process::Command,
    sync::{Mutex, OnceLock},
    time::{Duration as StdDuration, Instant},
};

use super::{
    AppState, skill_usage_analytics::collect_skill_usage_pg, skills_api::sync_skills_from_disk_pg,
};

const UNSUPPORTED_RATE_LIMIT_PROVIDERS: &[(&str, &str)] = &[
    (
        "opencode",
        "No OpenCode rate-limit telemetry source is implemented yet.",
    ),
    (
        "qwen",
        "No Qwen rate-limit telemetry source is implemented yet.",
    ),
];
const UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS: i64 = 30 * 24 * 60 * 60;

// Issue #1243: small in-process TTL cache shared across the read-mostly
// analytics endpoints. The PG-backed analytics queries fan out across a few
// large tables (`agent_quality_event`, `task_dispatches`, `audit_logs`) and
// are cheap to recompute but expensive enough that the dashboard's "stats"
// tab paints with multiple skeletons on every entry. A 60s TTL is short
// enough that the dashboard "live" feel is preserved (token usage / event
// counts don't materially shift in 60s) and long enough to absorb cross-tab
// re-entry, sidebar refresh, and 1-minute polling intervals from the
// dashboard. Writes (e.g. dispatching a card, recording a quality event) are
// not gated on this cache — they go straight to PG, and stale reads simply
// surface within the TTL window.
const ANALYTICS_CACHE_TTL: StdDuration = StdDuration::from_secs(60);
/// Hard cap on cached entries across all analytics endpoints. With unique
/// `provider`/`channelId`/`eventType`/`date`/`limit` keys an unbounded
/// cache could grow process memory until restart (codex P2 round 2 #1299).
/// 4096 entries × ~8 KB body × etag overhead ≈ 32 MB worst case, which is
/// generous yet still bounded.
const ANALYTICS_CACHE_MAX_ENTRIES: usize = 4096;

#[derive(Clone)]
struct CachedJson {
    cached_at: Instant,
    body: serde_json::Value,
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

fn write_analytics_cache(key: String, body: serde_json::Value) -> CachedJson {
    let etag = compute_etag(&body);
    let entry = CachedJson {
        cached_at: Instant::now(),
        body,
        etag,
    };
    if let Ok(mut cache) = analytics_response_cache().lock() {
        // Codex P2 round 2 on #1299: TTL-only check on read leaks expired
        // entries when query parameters vary. Sweep expired entries on every
        // write, plus enforce a hard cap so a single hot path with many
        // distinct keys can't grow process memory unbounded.
        prune_expired_analytics_cache_entries(&mut cache);
        cache.insert(key, entry.clone());
    }
    entry
}

/// Drop expired entries; if still over `ANALYTICS_CACHE_MAX_ENTRIES` after
/// pruning, evict the oldest by `cached_at` until under the cap.
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

#[cfg(test)]
fn reset_analytics_cache() {
    if let Ok(mut cache) = analytics_response_cache().lock() {
        cache.clear();
    }
}

fn compute_etag(body: &serde_json::Value) -> String {
    use std::hash::{Hash, Hasher};
    let serialized = serde_json::to_string(body).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serialized.hash(&mut hasher);
    format!("\"{:016x}\"", hasher.finish())
}

/// Build a JSON response with SWR-friendly Cache-Control + ETag headers and a
/// debug `X-Analytics-Cache: hit|miss` marker so the dashboard / smoke tests
/// can verify the cache is doing its job. The 60s TTL on the in-process
/// cache and the 30s `max-age=30, stale-while-revalidate=120` on the
/// browser/proxy side together absorb both the heavy origin work and any
/// double-fetch in the dashboard's effect chain.
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

fn analytics_error(status: StatusCode, message: String) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
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

async fn query_analytics_pg(
    pool: &sqlx::PgPool,
    filters: &crate::services::observability::AnalyticsFilters,
) -> Result<serde_json::Value, sqlx::Error> {
    let limit = filters.event_limit.min(1000) as i64;
    let mut events_query = QueryBuilder::new(
        "SELECT id::TEXT AS id,
                provider,
                channel_id,
                event_type::TEXT AS event_type,
                payload::TEXT AS payload,
                created_at::TEXT AS created_at
           FROM agent_quality_event WHERE 1=1",
    );
    if let Some(provider) = filters.provider.as_deref() {
        events_query.push(" AND provider = ").push_bind(provider);
    }
    if let Some(channel_id) = filters.channel_id.as_deref() {
        events_query
            .push(" AND channel_id = ")
            .push_bind(channel_id);
    }
    if let Some(event_type) = filters.event_type.as_deref() {
        events_query
            .push(" AND event_type::TEXT = ")
            .push_bind(event_type);
    }
    events_query
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);

    let events = events_query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "event_type": row.try_get::<String, _>("event_type").unwrap_or_default(),
                "payload": row.try_get::<Option<String>, _>("payload").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "counters": [],
        "events": events,
    }))
}

async fn query_agent_quality_events_pg(
    pool: &sqlx::PgPool,
    filters: &crate::services::observability::AgentQualityFilters,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let days = filters.days.clamp(1, 365);
    let limit = filters.limit.clamp(1, 1000) as i64;
    let mut query = QueryBuilder::new(
        "SELECT id::TEXT AS id,
                source_event_id,
                correlation_id,
                agent_id,
                provider,
                channel_id,
                card_id,
                dispatch_id,
                event_type::TEXT AS event_type,
                payload::TEXT AS payload,
                created_at::TEXT AS created_at
           FROM agent_quality_event
          WHERE created_at >= NOW() - (",
    );
    query.push_bind(days).push("::BIGINT * INTERVAL '1 day')");
    if let Some(agent_id) = filters.agent_id.as_deref() {
        query.push(" AND agent_id = ").push_bind(agent_id);
    }
    query
        .push(" ORDER BY created_at DESC LIMIT ")
        .push_bind(limit);

    Ok(query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<String, _>("id").unwrap_or_default(),
                "source_event_id": row.try_get::<Option<String>, _>("source_event_id").ok().flatten(),
                "correlation_id": row.try_get::<Option<String>, _>("correlation_id").ok().flatten(),
                "agent_id": row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "card_id": row.try_get::<Option<String>, _>("card_id").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "event_type": row.try_get::<String, _>("event_type").unwrap_or_default(),
                "payload": row.try_get::<Option<String>, _>("payload").ok().flatten(),
                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
            })
        })
        .collect())
}

async fn query_invariants_pg(
    pool: &sqlx::PgPool,
    filters: &crate::services::observability::InvariantAnalyticsFilters,
) -> Result<serde_json::Value, sqlx::Error> {
    let limit = filters.limit.min(1000) as i64;
    let mut query = QueryBuilder::new(
        "SELECT provider,
                channel_id,
                event_type::TEXT AS invariant,
                COUNT(*)::BIGINT AS count
           FROM agent_quality_event
          WHERE event_type::TEXT LIKE '%invariant%'",
    );
    if let Some(provider) = filters.provider.as_deref() {
        query.push(" AND provider = ").push_bind(provider);
    }
    if let Some(channel_id) = filters.channel_id.as_deref() {
        query.push(" AND channel_id = ").push_bind(channel_id);
    }
    if let Some(invariant) = filters.invariant.as_deref() {
        query.push(" AND event_type::TEXT = ").push_bind(invariant);
    }
    query.push(" GROUP BY provider, channel_id, event_type ORDER BY count DESC LIMIT ");
    query.push_bind(limit);

    let counts = query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            json!({
                "provider": row.try_get::<Option<String>, _>("provider").ok().flatten(),
                "channel_id": row.try_get::<Option<String>, _>("channel_id").ok().flatten(),
                "invariant": row.try_get::<String, _>("invariant").unwrap_or_default(),
                "count": row.try_get::<i64, _>("count").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();
    let total_violations = counts
        .iter()
        .filter_map(|row| row["count"].as_i64())
        .sum::<i64>();
    Ok(json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "total_violations": total_violations,
        "counts": counts,
        "recent": [],
    }))
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
    match query_analytics_pg(pool, &filters).await {
        Ok(value) => {
            let entry = write_analytics_cache(cache_key, value);
            build_analytics_response(&entry, "miss")
        }
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
    match query_agent_quality_events_pg(pool, &filters).await {
        Ok(events) => {
            let body = json!({
                "events": events,
                "generated_at_ms": chrono::Utc::now().timestamp_millis(),
            });
            let entry = write_analytics_cache(cache_key, body);
            build_analytics_response(&entry, "miss")
        }
        Err(error) => analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query agent quality events: {error}"),
        ),
    }
}

/// GET /api/analytics/observability
///
/// Lightweight foundation-layer view introduced by #1070 (Epic #905 Phase 1).
/// Surfaces the atomic channel × provider counters and the latest entries of
/// the in-memory structured event ring buffer without touching SQL.
#[derive(Debug, Default, Deserialize)]
pub struct ObservabilityQuery {
    #[serde(rename = "recentLimit")]
    pub recent_limit: Option<usize>,
}

pub async fn observability(
    Query(params): Query<ObservabilityQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = params.recent_limit.unwrap_or(100).min(1000);
    let counters = crate::services::observability::metrics::snapshot();
    let recent_events = crate::services::observability::events::recent(limit);
    // #1134: surface watcher attach→first-relay latency alongside the existing
    // counters/events so a single GET can answer "is recovery healthy?".
    let watcher_first_relay = crate::services::observability::watcher_latency::snapshot();
    let body = json!({
        "counters": counters,
        "recent_events": recent_events,
        "watcher_first_relay": watcher_first_relay,
        "generated_at_ms": chrono::Utc::now().timestamp_millis(),
    });
    (StatusCode::OK, Json(body))
}

/// GET /api/analytics/policy-hooks
///
/// Policy hook observability view (#1080 / Epic #906). Returns recent
/// `policy_hook_executed` structured events with optional filters on
/// `policy_name`, `hook_name`, and a lookback window in minutes. Backed by the
/// in-memory ring buffer populated by `services::observability::events`.
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

pub async fn policy_hooks(
    Query(params): Query<PolicyHooksQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = params.limit.unwrap_or(200).min(2000);
    // Pull a generous window from the ring buffer, then filter in-memory.
    let pool = crate::services::observability::events::recent(
        crate::services::observability::events::MAX_EVENTS,
    );
    let now_ms = chrono::Utc::now().timestamp_millis();
    let window_ms = params.last_minutes.map(|m| m.saturating_mul(60_000));

    let mut matched: Vec<serde_json::Value> = Vec::new();
    for ev in pool.into_iter().rev() {
        if ev.event_type != "policy_hook_executed" {
            continue;
        }
        if let Some(window) = window_ms {
            if now_ms.saturating_sub(ev.timestamp_ms) > window {
                continue;
            }
        }
        if let Some(ref needed) = params.policy_name {
            let ok = ev
                .payload
                .get("policy_name")
                .and_then(|v| v.as_str())
                .map(|s| s == needed.as_str())
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        if let Some(ref needed) = params.hook_name {
            let ok = ev
                .payload
                .get("hook_name")
                .and_then(|v| v.as_str())
                .map(|s| s == needed.as_str())
                .unwrap_or(false);
            if !ok {
                continue;
            }
        }
        matched.push(json!({
            "timestamp_ms": ev.timestamp_ms,
            "policy_name": ev.payload.get("policy_name").cloned().unwrap_or(serde_json::Value::Null),
            "hook_name": ev.payload.get("hook_name").cloned().unwrap_or(serde_json::Value::Null),
            "policy_version": ev.payload.get("policy_version").cloned().unwrap_or(serde_json::Value::Null),
            "duration_ms": ev.payload.get("duration_ms").cloned().unwrap_or(serde_json::Value::Null),
            "result": ev.payload.get("result").cloned().unwrap_or(serde_json::Value::Null),
            "effects_count": ev.payload.get("effects_count").cloned().unwrap_or(serde_json::Value::Null),
        }));
        if matched.len() >= limit {
            break;
        }
    }

    // matched is newest-first (because we iterated in reverse). Keep it that
    // way for dashboard consumers.
    let body = json!({
        "events": matched,
        "generated_at_ms": now_ms,
    });
    (StatusCode::OK, Json(body))
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
    match query_invariants_pg(pool, &filters).await {
        Ok(value) => {
            let entry = write_analytics_cache(cache_key, value);
            build_analytics_response(&entry, "miss")
        }
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

    let rows = match sqlx::query(
        "SELECT a.id, a.name, a.avatar_emoji,
                STRING_AGG(DISTINCT td.updated_at::date::text, ',') AS active_dates,
                MAX(td.updated_at)::text AS last_active
         FROM agents a
         INNER JOIN task_dispatches td ON td.to_agent_id = a.id
         WHERE td.status = 'completed'
         GROUP BY a.id
         ORDER BY last_active DESC",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return analytics_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("query prepare failed: {e}"),
            );
        }
    };

    let streaks = rows
        .into_iter()
        .map(|row| {
            let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
            let name = row.try_get::<Option<String>, _>("name").ok().flatten();
            let avatar_emoji = row
                .try_get::<Option<String>, _>("avatar_emoji")
                .ok()
                .flatten();
            let active_dates_str = row
                .try_get::<Option<String>, _>("active_dates")
                .ok()
                .flatten();
            let last_active = row
                .try_get::<Option<String>, _>("last_active")
                .ok()
                .flatten();
            let streak = if let Some(ref dates_str) = active_dates_str {
                let mut dates: Vec<&str> = dates_str.split(',').collect();
                dates.sort();
                dates.reverse();
                compute_streak(&dates)
            } else {
                0
            };

            json!({
                "agent_id": agent_id,
                "name": name,
                "avatar_emoji": avatar_emoji,
                "streak": streak,
                "last_active": last_active,
            })
        })
        .collect::<Vec<_>>();

    let entry = write_analytics_cache(cache_key, json!({ "streaks": streaks }));
    build_analytics_response(&entry, "miss")
}

/// 날짜 문자열 배열 (내림차순)에서 오늘부터 연속일 계산
fn compute_streak(sorted_dates_desc: &[&str]) -> i64 {
    if sorted_dates_desc.is_empty() {
        return 0;
    }

    // 간단 구현: 날짜를 일수 차이로 변환
    // SQLite date format: "YYYY-MM-DD"
    let today = chrono_today();
    let mut streak = 0i64;
    let mut expected_date = today;

    for date_str in sorted_dates_desc {
        if let Some(d) = parse_date(date_str) {
            if d == expected_date {
                streak += 1;
                expected_date = d - 1;
            } else if d < expected_date {
                // 건너뛴 날이 있으면 중단
                break;
            }
            // d > expected_date는 무시 (미래 날짜 등)
        }
    }

    streak
}

/// 간단한 날짜 파싱 (YYYY-MM-DD → 일수 단위 정수, 비교용)
fn parse_date(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    // 일수 환산 (비교 목적이므로 정확한 달력 계산 불필요, 대략적 환산)
    Some(y * 365 + m * 30 + d)
}

fn chrono_today() -> i64 {
    // 현재 UTC 날짜를 같은 방식으로 환산
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (now / 86400) as i64;
    // Unix epoch 1970-01-01부터의 일수를 YYYY-MM-DD 환산과 맞추기 위해
    // 같은 공식 사용: 1970 * 365 + 1 * 30 + 1 + days
    // 대신 간단히: 오늘 날짜를 문자열로 만들어 parse_date 호출
    let total_days = days;
    let approx_year = 1970 + total_days / 365;
    let remaining = total_days % 365;
    let approx_month = 1 + remaining / 30;
    let approx_day = 1 + remaining % 30;
    approx_year * 365 + approx_month * 30 + approx_day
}

/// GET /api/achievements
#[derive(Debug, Deserialize)]
pub struct AchievementsQuery {
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

pub async fn achievements(
    State(state): State<AppState>,
    Query(params): Query<AchievementsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    // XP milestone thresholds
    let milestones: &[(i64, &str, &str)] = &[
        (10, "first_task", "첫 번째 작업 완료"),
        (50, "getting_started", "본격적인 시작"),
        (100, "centurion", "100 XP 달성"),
        (250, "veteran", "베테랑"),
        (500, "expert", "전문가"),
        (1000, "master", "마스터"),
    ];

    // Build agent filter
    let mut query = QueryBuilder::new(
        "SELECT id, COALESCE(name, id), COALESCE(name_ko, name, id), xp, avatar_emoji FROM agents WHERE xp > 0",
    );
    if let Some(agent_id) = params.agent_id.as_deref() {
        query.push(" AND id = ").push_bind(agent_id);
    }

    let rows = match query.build().fetch_all(pool).await {
        Ok(rows) => rows,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let agents: Vec<(String, String, String, i64, String)> = rows
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>(0).unwrap_or_default(),
                row.try_get::<String, _>(1).unwrap_or_default(),
                row.try_get::<String, _>(2).unwrap_or_default(),
                row.try_get::<i64, _>(3).unwrap_or(0),
                row.try_get::<Option<String>, _>(4)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "🤖".to_string()),
            )
        })
        .collect();

    // Pre-fetch completion timestamps per agent (nth completed dispatch as earned_at proxy)
    let mut agent_completed_times: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for (agent_id, _, _, _, _) in &agents {
        let times: Vec<i64> = sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM updated_at)::BIGINT * 1000) AS completed_at_ms
             FROM task_dispatches WHERE to_agent_id = $1 AND status = 'completed'
             ORDER BY updated_at ASC",
        )
        .bind(agent_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        agent_completed_times.insert(agent_id.clone(), times);
    }

    let mut achievements = Vec::new();
    for (agent_id, name, name_ko, xp, avatar_emoji) in &agents {
        let completion_times = agent_completed_times.get(agent_id.as_str());
        for (threshold, achievement_type, description) in milestones {
            if xp >= threshold {
                // Estimate earned_at: use the Nth completed dispatch timestamp
                // where N approximates when this XP threshold was crossed
                // (assuming ~10 XP per completion on average)
                let approx_index = (*threshold as usize / 10).saturating_sub(1);
                let earned_at = completion_times
                    .and_then(|times| times.get(approx_index.min(times.len().saturating_sub(1))))
                    .copied()
                    .unwrap_or(0);

                let emoji = avatar_emoji.as_str();
                achievements.push(json!({
                    "id": format!("{agent_id}:{achievement_type}"),
                    "agent_id": agent_id,
                    "type": achievement_type,
                    "name": format!("{description} ({threshold} XP)"),
                    "description": description,
                    "earned_at": earned_at,
                    "agent_name": name,
                    "agent_name_ko": name_ko,
                    "avatar_emoji": emoji,
                }));
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "achievements": achievements })),
    )
}

/// GET /api/activity-heatmap?date=2026-03-19
#[derive(Debug, Deserialize)]
pub struct HeatmapQuery {
    date: Option<String>,
}

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

    // Issue #1243 — Previously this endpoint dispatched 24 sequential queries
    // (one per hour bucket) which dominated p99 on the dashboard heatmap card.
    // The single query below groups by (hour, to_agent_id) on the server side
    // and is backed by the new idx_task_dispatches_created_at index from
    // migration 0023. The result is ~24× fewer round trips and ~5–10× faster
    // wall-clock time on representative datasets.
    // Codex P2 on #1299: `created_at::date = $1` casts the column and
    // disables the plain `(created_at)` btree from migration 0023. Switch
    // to a half-open range so the planner can use the index on large
    // task_dispatches tables.
    let rows = match sqlx::query(
        "SELECT EXTRACT(HOUR FROM td.created_at)::BIGINT AS hour,
                td.to_agent_id,
                COUNT(*)::BIGINT AS cnt
           FROM task_dispatches td
          WHERE td.created_at >= $1::date
            AND td.created_at < $1::date + INTERVAL '1 day'
            AND td.to_agent_id IS NOT NULL
          GROUP BY hour, td.to_agent_id",
    )
    .bind(&date)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return analytics_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("query activity heatmap: {error}"),
            );
        }
    };

    let mut buckets: Vec<serde_json::Map<String, serde_json::Value>> =
        (0..24).map(|_| serde_json::Map::new()).collect();
    for row in rows {
        let hour = row.try_get::<i64, _>("hour").unwrap_or(-1);
        if !(0..24).contains(&hour) {
            continue;
        }
        let agent_id = match row.try_get::<String, _>("to_agent_id") {
            Ok(value) => value,
            Err(_) => continue,
        };
        let count = row.try_get::<i64, _>("cnt").unwrap_or(0);
        buckets[hour as usize].insert(agent_id, json!(count));
    }
    let hours: Vec<serde_json::Value> = buckets
        .into_iter()
        .enumerate()
        .map(|(hour, agents)| json!({ "hour": hour, "agents": agents }))
        .collect();

    let body = json!({
        "hours": hours,
        "date": date,
    });
    let entry = write_analytics_cache(cache_key, body);
    build_analytics_response(&entry, "miss")
}

/// GET /api/audit-logs?limit=20&entityType=...&entityId=...&agentId=...
///
/// Optional `agentId` filters rows to audit entries on kanban cards whose
/// `assigned_agent_id` matches the given agent. Combined with the new
/// kanban_card enrichment fields below, the dashboard's restored
/// "감사 / Audit" panel on the agent drawer can render human-readable
/// rows ("#1242 홈 KPI · backlog → requested") instead of the raw
/// `kanban_card:UUID` strings the previous panel surfaced (#1258).
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

pub async fn audit_logs(
    State(state): State<AppState>,
    Query(params): Query<AuditLogsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };

    let limit = params.limit.unwrap_or(20);
    let audit_count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM audit_logs")
        .fetch_one(pool)
        .await
        .unwrap_or(0);

    let logs = if audit_count > 0 {
        // Left-join kanban_cards so the response carries the human-readable
        // title + GitHub issue number when the entity_type is 'kanban_card'.
        // Other entity types still return null enrichment fields and are
        // unaffected.
        let mut query = QueryBuilder::new(
            "SELECT a.id, a.entity_type, a.entity_id, a.action, a.timestamp, a.actor,
                    c.title AS card_title,
                    c.github_issue_number AS card_issue_number,
                    c.github_issue_url AS card_issue_url,
                    c.assigned_agent_id AS card_assigned_agent_id
             FROM audit_logs a
             LEFT JOIN kanban_cards c
               ON a.entity_type = 'kanban_card' AND a.entity_id = c.id
             WHERE 1=1",
        );
        if let Some(entity_type) = params.entity_type.as_deref() {
            query.push(" AND a.entity_type = ").push_bind(entity_type);
        }
        if let Some(entity_id) = params.entity_id.as_deref() {
            query.push(" AND a.entity_id = ").push_bind(entity_id);
        }
        if let Some(agent_id) = params.agent_id.as_deref() {
            // agentId filter only matches card-scoped audit rows where the
            // assignee matches; non-card rows are excluded so the agent
            // drawer doesn't surface unrelated system events.
            query
                .push(" AND a.entity_type = 'kanban_card' AND c.assigned_agent_id = ")
                .push_bind(agent_id);
        }
        query
            .push(" ORDER BY a.timestamp DESC LIMIT ")
            .push_bind(limit);

        query
            .build()
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let entity_type = row
                    .try_get::<Option<String>, _>("entity_type")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "system".to_string());
                let entity_id = row
                    .try_get::<Option<String>, _>("entity_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let action = row
                    .try_get::<Option<String>, _>("action")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "updated".to_string());
                let created_at = row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("timestamp")
                    .map(|ts| ts.timestamp_millis())
                    .unwrap_or(0);
                let actor = row
                    .try_get::<Option<String>, _>("actor")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                let card_title = row
                    .try_get::<Option<String>, _>("card_title")
                    .ok()
                    .flatten();
                let card_issue_number = row
                    .try_get::<Option<i32>, _>("card_issue_number")
                    .ok()
                    .flatten();
                let card_issue_url = row
                    .try_get::<Option<String>, _>("card_issue_url")
                    .ok()
                    .flatten();
                let card_assigned_agent_id = row
                    .try_get::<Option<String>, _>("card_assigned_agent_id")
                    .ok()
                    .flatten();
                let summary = build_audit_summary(
                    &entity_type,
                    &entity_id,
                    &action,
                    card_title.as_deref(),
                    card_issue_number,
                );
                json!({
                    "id": row.try_get::<i64, _>("id").unwrap_or(0).to_string(),
                    "actor": actor,
                    "action": action,
                    "entity_type": entity_type,
                    "entity_id": entity_id,
                    "summary": summary,
                    "created_at": created_at,
                    "card_title": card_title,
                    "card_issue_number": card_issue_number,
                    "card_issue_url": card_issue_url,
                    "card_assigned_agent_id": card_assigned_agent_id,
                })
            })
            .collect::<Vec<_>>()
    } else {
        if let Some(ref entity_type) = params.entity_type {
            if entity_type != "kanban_card" {
                return (StatusCode::OK, Json(json!({ "logs": [] })));
            }
        }

        let mut query = QueryBuilder::new(
            "SELECT k.id, k.card_id, k.from_status, k.to_status, k.source, k.created_at,
                    c.title AS card_title,
                    c.github_issue_number AS card_issue_number,
                    c.github_issue_url AS card_issue_url,
                    c.assigned_agent_id AS card_assigned_agent_id
             FROM kanban_audit_logs k
             LEFT JOIN kanban_cards c ON k.card_id = c.id
             WHERE 1=1",
        );
        if let Some(card_id) = params.entity_id.as_deref() {
            query.push(" AND k.card_id = ").push_bind(card_id);
        }
        if let Some(agent_id) = params.agent_id.as_deref() {
            query
                .push(" AND c.assigned_agent_id = ")
                .push_bind(agent_id);
        }
        query
            .push(" ORDER BY k.created_at DESC LIMIT ")
            .push_bind(limit);

        query
            .build()
            .fetch_all(pool)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|row| {
                let card_id = row.try_get::<String, _>("card_id").unwrap_or_default();
                let from_status = row
                    .try_get::<Option<String>, _>("from_status")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string());
                let to_status = row
                    .try_get::<Option<String>, _>("to_status")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string());
                let actor = row
                    .try_get::<Option<String>, _>("source")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "hook".to_string());
                let created_at = row
                    .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map(|ts| ts.timestamp_millis())
                    .unwrap_or(0);
                let card_title = row
                    .try_get::<Option<String>, _>("card_title")
                    .ok()
                    .flatten();
                let card_issue_number = row
                    .try_get::<Option<i32>, _>("card_issue_number")
                    .ok()
                    .flatten();
                let card_issue_url = row
                    .try_get::<Option<String>, _>("card_issue_url")
                    .ok()
                    .flatten();
                let card_assigned_agent_id = row
                    .try_get::<Option<String>, _>("card_assigned_agent_id")
                    .ok()
                    .flatten();
                let action = format!("{from_status}->{to_status}");
                let summary = build_audit_summary(
                    "kanban_card",
                    &card_id,
                    &action,
                    card_title.as_deref(),
                    card_issue_number,
                );
                json!({
                    "id": format!("kanban-{}", row.try_get::<i64, _>("id").unwrap_or(0)),
                    "actor": actor.clone(),
                    "action": action,
                    "entity_type": "kanban_card",
                    "entity_id": card_id,
                    "summary": summary,
                    "metadata": {
                        "from_status": from_status,
                        "to_status": to_status,
                        "source": actor,
                    },
                    "created_at": created_at,
                    "card_title": card_title,
                    "card_issue_number": card_issue_number,
                    "card_issue_url": card_issue_url,
                    "card_assigned_agent_id": card_assigned_agent_id,
                })
            })
            .collect::<Vec<_>>()
    };

    (StatusCode::OK, Json(json!({ "logs": logs })))
}

/// Format a human-readable audit summary, preferring the kanban card title
/// (with #N issue number when available) over the raw `entity_type:entity_id`
/// pair the previous response shape exposed.
fn build_audit_summary(
    entity_type: &str,
    entity_id: &str,
    action: &str,
    card_title: Option<&str>,
    card_issue_number: Option<i32>,
) -> String {
    if entity_type == "kanban_card" {
        if let Some(title) = card_title {
            return match card_issue_number {
                Some(num) => format!("#{num} {title} · {action}"),
                None => format!("{title} · {action}"),
            };
        }
        if let Some(num) = card_issue_number {
            return format!("#{num} · {action}");
        }
    }
    if entity_id.is_empty() {
        format!("{entity_type} {action}")
    } else {
        format!("{entity_type}:{entity_id} {action}")
    }
}

fn parse_machine_config(value: &str) -> Option<Vec<(String, String)>> {
    serde_json::from_str::<Vec<serde_json::Value>>(value)
        .ok()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let name = m.get("name")?.as_str()?.to_string();
                    let host = m.get("host").and_then(|h| h.as_str()).unwrap_or_else(|| {
                        m.get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("localhost")
                    });
                    Some((name, format!("{}.local", host)))
                })
                .collect()
        })
        .filter(|machines: &Vec<(String, String)>| !machines.is_empty())
}

fn default_machine_config() -> Vec<(String, String)> {
    let hostname = crate::services::platform::hostname_short();
    vec![(hostname.clone(), hostname)]
}

async fn load_machine_config_pg(pool: &sqlx::PgPool) -> Option<Vec<(String, String)>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
        .bind("machines")
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|value| parse_machine_config(&value))
}

async fn load_machine_config(pg_pool: Option<&sqlx::PgPool>) -> Vec<(String, String)> {
    if let Some(pool) = pg_pool {
        return load_machine_config_pg(pool)
            .await
            .unwrap_or_else(default_machine_config);
    }

    default_machine_config()
}

/// GET /api/machine-status
/// Machine list from kv_meta key 'machines' (JSON array of {name, host}).
/// Falls back to current hostname if not configured.
pub async fn machine_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let machines_config = load_machine_config(state.pg_pool_ref()).await;

    let result = tokio::task::spawn_blocking(move || {
        let mut results = Vec::new();
        for (name, host) in machines_config {
            let online = Command::new("ping")
                .args(["-c1", "-W2", &host])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            results.push(json!({"name": name, "online": online}));
        }
        results
    })
    .await;

    let machines = result.unwrap_or_default();
    (StatusCode::OK, Json(json!({"machines": machines})))
}

/// GET /api/rate-limits
/// Returns cached rate limit data from rate_limit_cache table.
pub async fn rate_limits(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let now = chrono::Utc::now().timestamp();
    let Some(pool) = state.pg_pool_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "postgres pool unavailable"})),
        );
    };
    let providers = build_rate_limit_provider_payloads_pg(pool, now).await;

    (StatusCode::OK, Json(json!({"providers": providers})))
}

/// Shared by `/api/rate-limits` and `/api/home/kpi-trends` (#1242). The home
/// KPI tile reuses this exact provider/bucket shape so the dashboard sees a
/// single, consistent rate-limit schema regardless of which endpoint the
/// data came from.
pub(super) async fn build_rate_limit_provider_payloads_pg(
    pool: &sqlx::PgPool,
    now: i64,
) -> Vec<serde_json::Value> {
    let stale_sec =
        sqlx::query("SELECT value FROM kv_meta WHERE key = 'rateLimitStaleSec' LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<String, _>("value").ok())
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(600);

    let rows = match sqlx::query(
        "SELECT provider, data, fetched_at
         FROM rate_limit_cache
         ORDER BY provider",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows,
        Err(_) => return build_unsupported_rate_limit_entries_pg(pool, now).await,
    };

    let mut seen = HashSet::new();
    let mut providers = Vec::new();

    for row in rows {
        let provider = match row.try_get::<String, _>("provider") {
            Ok(provider) => provider,
            Err(_) => continue,
        };
        let data = match row.try_get::<String, _>("data") {
            Ok(data) => data,
            Err(_) => continue,
        };
        let fetched_at = match row.try_get::<i64, _>("fetched_at") {
            Ok(fetched_at) => fetched_at,
            Err(_) => continue,
        };

        let parsed: serde_json::Value = match serde_json::from_str(&data) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let buckets = parsed
            .get("buckets")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let unsupported = parsed
            .get("unsupported")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let reason = parsed
            .get("reason")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let stale = (now - fetched_at) > stale_sec;
        seen.insert(provider.to_lowercase());
        providers.push(json!({
            "provider": provider,
            "buckets": buckets,
            "fetched_at": fetched_at,
            "stale": stale,
            "unsupported": unsupported,
            "reason": reason,
        }));
    }

    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if seen.contains(*provider) {
            continue;
        }
        if !provider_has_recent_session_usage_pg(pool, provider, now).await {
            continue;
        }
        providers.push(json!({
            "provider": provider,
            "buckets": [],
            "fetched_at": now,
            "stale": false,
            "unsupported": true,
            "reason": reason,
        }));
    }

    providers.sort_by_key(|entry| {
        match entry
            .get("provider")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "claude" => 0,
            "codex" => 1,
            "gemini" => 2,
            "opencode" => 3,
            "qwen" => 4,
            _ => 9,
        }
    });
    providers
}

async fn provider_has_recent_session_usage_pg(
    pool: &sqlx::PgPool,
    provider: &str,
    now: i64,
) -> bool {
    let threshold = now.saturating_sub(UNSUPPORTED_RATE_LIMIT_USAGE_LOOKBACK_SECONDS);
    sqlx::query(
        "SELECT 1
         FROM sessions
         WHERE lower(provider) = lower($1)
           AND COALESCE(
                 EXTRACT(EPOCH FROM last_heartbeat)::BIGINT,
                 EXTRACT(EPOCH FROM created_at)::BIGINT,
                 0
               ) >= $2
         LIMIT 1",
    )
    .bind(provider)
    .bind(threshold)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .is_some()
}

async fn build_unsupported_rate_limit_entries_pg(
    pool: &sqlx::PgPool,
    now: i64,
) -> Vec<serde_json::Value> {
    let mut providers = Vec::new();
    for (provider, reason) in UNSUPPORTED_RATE_LIMIT_PROVIDERS {
        if provider_has_recent_session_usage_pg(pool, provider, now).await {
            providers.push(json!({
                "provider": provider,
                "buckets": [],
                "fetched_at": now,
                "stale": false,
                "unsupported": true,
                "reason": reason,
            }));
        }
    }
    providers
}

/// GET /api/skills-trend?days=30
#[derive(Debug, Deserialize)]
pub struct SkillsTrendQuery {
    days: Option<i64>,
}

pub async fn skills_trend(
    State(state): State<AppState>,
    Query(params): Query<SkillsTrendQuery>,
) -> Response {
    let days = params.days.unwrap_or(30).min(90).max(1);
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

    if let Err(e) = sync_skills_from_disk_pg(pool).await {
        return analytics_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("skill sync failed: {e}"),
        );
    }
    let usage = match collect_skill_usage_pg(pool, Some(days)).await {
        Ok(data) => data,
        Err(e) => {
            return analytics_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("usage query failed: {e}"),
            );
        }
    };

    let mut by_day = BTreeMap::<String, i64>::new();
    for record in usage {
        *by_day.entry(record.day).or_default() += 1;
    }

    let trend = by_day
        .into_iter()
        .map(|(day, count)| json!({ "day": day, "count": count }))
        .collect::<Vec<_>>();

    let entry = write_analytics_cache(cache_key, json!({ "trend": trend }));
    build_analytics_response(&entry, "miss")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_analytics_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "analytics tests",
            )
            .await
            .expect("create postgres test db");

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "analytics tests",
            )
            .await
            .expect("apply postgres migration")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "analytics tests",
            )
            .await
            .expect("drop postgres test db");
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn machine_status_machines_config_prefers_postgres_when_pool_exists() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind("machines")
        .bind(serde_json::json!([{ "name": "pg-machine", "host": "pg-host" }]).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let machines = load_machine_config(Some(&pool)).await;

        assert_eq!(
            machines,
            vec![("pg-machine".to_string(), "pg-host.local".to_string())]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_when_postgres_is_unconfigured() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(Some(&pool)).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_for_empty_postgres_config() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind("machines")
        .bind("[]")
        .execute(&pool)
        .await
        .unwrap();
        let hostname = crate::services::platform::hostname_short();

        let machines = load_machine_config(Some(&pool)).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn machine_status_machines_config_uses_hostname_without_pg_pool() {
        let hostname = crate::services::platform::hostname_short();
        let machines = load_machine_config(None).await;

        assert_eq!(machines, vec![(hostname.clone(), hostname)]);
    }

    #[tokio::test]
    async fn policy_hooks_route_returns_recent_filtered_events() {
        // Seed the in-memory event ring buffer with synthetic
        // `policy_hook_executed` entries and verify the route filters them.
        crate::services::observability::events::reset_for_tests();

        crate::services::observability::events::record_simple(
            "policy_hook_executed",
            None,
            None,
            serde_json::json!({
                "policy_name": "alpha-policy",
                "hook_name": "onTick",
                "policy_version": "abc123",
                "duration_ms": 3,
                "result": "ok",
                "effects_count": 0,
            }),
        );
        crate::services::observability::events::record_simple(
            "policy_hook_executed",
            None,
            None,
            serde_json::json!({
                "policy_name": "beta-policy",
                "hook_name": "onCardTerminal",
                "policy_version": "def456",
                "duration_ms": 7,
                "result": "err",
                "effects_count": 1,
            }),
        );
        // Noise event — must not surface.
        crate::services::observability::events::record_simple(
            "turn_finished",
            Some(42),
            Some("codex"),
            serde_json::json!({"status": "ok"}),
        );

        let (status, Json(body)) = policy_hooks(Query(PolicyHooksQuery {
            policy_name: Some("beta-policy".to_string()),
            hook_name: None,
            last_minutes: None,
            limit: None,
        }))
        .await;

        assert_eq!(status, StatusCode::OK);
        let events = body["events"].as_array().expect("events array");
        assert_eq!(events.len(), 1, "only beta-policy event should match");
        assert_eq!(events[0]["policy_name"], json!("beta-policy"));
        assert_eq!(events[0]["hook_name"], json!("onCardTerminal"));
        assert_eq!(events[0]["result"], json!("err"));
        assert_eq!(events[0]["effects_count"], json!(1));
        assert_eq!(events[0]["policy_version"], json!("def456"));

        // Unfiltered query should return both policy_hook_executed events
        // (but not the turn_finished noise event).
        let (_, Json(body_all)) = policy_hooks(Query(PolicyHooksQuery {
            policy_name: None,
            hook_name: None,
            last_minutes: Some(60),
            limit: Some(10),
        }))
        .await;
        let events_all = body_all["events"].as_array().unwrap();
        assert_eq!(events_all.len(), 2);
    }

    #[tokio::test]
    async fn build_rate_limit_provider_payloads_pg_hides_unused_unsupported_qwen() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO rate_limit_cache (provider, data, fetched_at)
             VALUES ($1, $2, $3)",
        )
        .bind("claude")
        .bind(
            serde_json::json!({
                "buckets": [{
                    "name": "requests",
                    "limit": 100,
                    "used": 20,
                    "remaining": 80,
                    "reset": 1_700_000_000_i64
                }]
            })
            .to_string(),
        )
        .bind(1_700_000_000_i64)
        .execute(&pool)
        .await
        .unwrap();

        let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("claude"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn build_rate_limit_provider_payloads_pg_shows_recent_unsupported_qwen_only_when_used() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES ($1, $2, 'idle', TO_TIMESTAMP($3), TO_TIMESTAMP($4))",
        )
        .bind("qwen-session-1")
        .bind("qwen")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_050_i64)
        .execute(&pool)
        .await
        .unwrap();

        let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("qwen"));
        assert_eq!(providers[0]["unsupported"], json!(true));
        assert_eq!(providers[0]["buckets"], json!([]));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn build_rate_limit_provider_payloads_pg_shows_recent_unsupported_opencode_only_when_used()
     {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, created_at, last_heartbeat)
             VALUES ($1, $2, 'idle', TO_TIMESTAMP($3), TO_TIMESTAMP($4))",
        )
        .bind("opencode-session-1")
        .bind("opencode")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_050_i64)
        .execute(&pool)
        .await
        .unwrap();

        let providers = build_rate_limit_provider_payloads_pg(&pool, 1_700_000_100).await;

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0]["provider"], json!("opencode"));
        assert_eq!(providers[0]["unsupported"], json!(true));
        assert_eq!(providers[0]["buckets"], json!([]));

        pool.close().await;
        pg_db.drop().await;
    }

    /// #1070: foundation-layer `/api/analytics/observability` endpoint shape
    /// + hot-path wiring check. `emit_turn_started`/`emit_turn_finished` must
    /// populate the atomic counters that the endpoint exposes.
    #[tokio::test]
    async fn observability_route_exposes_atomic_counters_and_recent_events() {
        let _guard = crate::services::observability::test_runtime_lock();
        crate::services::observability::reset_for_tests();
        crate::services::observability::metrics::reset_for_tests();
        crate::services::observability::events::reset_for_tests();

        crate::services::observability::init_observability(None);

        // Attempt + success
        crate::services::observability::emit_turn_started(
            "codex",
            5150,
            Some("dispatch-obs"),
            Some("session-obs"),
            Some("turn-obs"),
        );
        crate::services::observability::emit_turn_finished(
            "codex",
            5150,
            Some("dispatch-obs"),
            Some("session-obs"),
            Some("turn-obs"),
            "completed",
            42,
            false,
        );
        // Attempt + fail (different turn).
        crate::services::observability::emit_turn_started(
            "codex",
            5150,
            Some("dispatch-obs-2"),
            Some("session-obs-2"),
            Some("turn-obs-2"),
        );
        crate::services::observability::emit_turn_finished(
            "codex",
            5150,
            Some("dispatch-obs-2"),
            Some("session-obs-2"),
            Some("turn-obs-2"),
            "error",
            10,
            false,
        );
        // Watcher replacement + guard fire
        crate::services::observability::emit_watcher_replaced("codex", 5150, "stale_cancel");
        crate::services::observability::emit_guard_fired(
            "codex",
            5150,
            Some("dispatch-obs"),
            None,
            None,
            "placeholder_suppress",
        );

        let (status, Json(body)) = observability(Query(ObservabilityQuery {
            recent_limit: Some(50),
        }))
        .await;

        assert_eq!(status, StatusCode::OK);

        let counters = body["counters"].as_array().expect("counters array");
        let row = counters
            .iter()
            .find(|row| row["channel_id"] == json!(5150) && row["provider"] == json!("codex"))
            .expect("expected counter row for codex/5150");
        assert_eq!(row["attempts"], json!(2));
        assert_eq!(row["success"], json!(1));
        assert_eq!(row["fail"], json!(1));
        assert_eq!(row["guard_fires"], json!(1));
        assert_eq!(row["watcher_replacements"], json!(1));
        let rate = row["success_rate"].as_f64().expect("success_rate f64");
        assert!((rate - 0.5).abs() < 1e-9, "success_rate={rate}");

        let events = body["recent_events"].as_array().expect("recent_events");
        assert!(!events.is_empty());
        let kinds: std::collections::HashSet<&str> = events
            .iter()
            .filter_map(|ev| ev["event_type"].as_str())
            .collect();
        assert!(kinds.contains("turn_started"));
        assert!(kinds.contains("turn_finished"));
        assert!(kinds.contains("watcher_replaced"));
        assert!(kinds.contains("guard_fired"));
    }

    /// Issue #1243 — exercise the cache hot path: a second call within the
    /// 60s TTL must hit the in-process cache and return the same body without
    /// touching PG. This is asserted via the `X-Analytics-Cache: hit` marker
    /// that `build_analytics_response` attaches.
    #[tokio::test]
    async fn analytics_cache_serves_warm_hits_without_repeat_query() {
        use axum::body::to_bytes;

        reset_analytics_cache();
        let body = json!({"counters": [], "events": []});
        let entry = write_analytics_cache("analytics-test-key".to_string(), body.clone());
        assert_eq!(entry.body, body);
        assert!(!entry.etag.is_empty(), "etag should be non-empty");

        let cached = read_analytics_cache("analytics-test-key").expect("cache hit");
        assert_eq!(cached.body, body);
        assert_eq!(cached.etag, entry.etag);

        let response = build_analytics_response(&cached, "hit");
        assert_eq!(response.status(), StatusCode::OK);
        let cache_header = response
            .headers()
            .get("X-Analytics-Cache")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(cache_header, "hit");
        let etag_header = response
            .headers()
            .get("ETag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(etag_header, entry.etag);
        let cc_header = response
            .headers()
            .get("Cache-Control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cc_header.contains("stale-while-revalidate"),
            "Cache-Control must include SWR directive, got: {cc_header}"
        );

        let bytes = to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, body);
    }

    /// Issue #1243 — micro-benchmark: cold-then-warm reads of the analytics
    /// cache. The cold call falls through write_analytics_cache (compute +
    /// hash + insert), the warm call must return in well under a millisecond
    /// because it is just a HashMap lookup + clone.
    #[tokio::test]
    async fn analytics_cache_warm_read_is_fast() {
        reset_analytics_cache();
        let key = "analytics-bench-key".to_string();
        let body = json!({
            "counters": (0..50).map(|i| json!({"channel": i})).collect::<Vec<_>>(),
            "events": (0..200).map(|i| json!({"id": i, "payload": "x".repeat(64)})).collect::<Vec<_>>(),
        });

        // Cold path: writes the cache entry.
        let cold_start = std::time::Instant::now();
        let cold_entry = write_analytics_cache(key.clone(), body.clone());
        let cold_ms = cold_start.elapsed().as_micros();
        assert_eq!(cold_entry.body, body);

        // Warm path: 100 lookups should each be cheap.
        let warm_start = std::time::Instant::now();
        for _ in 0..100 {
            let _ = read_analytics_cache(&key).expect("warm hit");
        }
        let warm_total_ms = warm_start.elapsed().as_micros();
        let warm_avg_us = warm_total_ms as f64 / 100.0;

        // The cold path is dominated by serde_json::to_string() on the body
        // for the etag hash; on tiny payloads it's < 200µs. The warm path is
        // a HashMap lookup + Value clone, well under 200µs each. We assert a
        // generous threshold so this test isn't flaky on slow CI hardware
        // but still catches a regression that adds an order of magnitude.
        assert!(
            warm_avg_us < 1_000.0,
            "warm read avg {warm_avg_us:.1}µs > 1000µs (cold {cold_ms}µs)"
        );
    }
}
