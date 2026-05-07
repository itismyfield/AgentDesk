use sqlx::PgPool;

use super::markers::ApiFrictionReport;
use super::memory_sync::sync_event_memory_pg;
use super::storage::store_api_friction_events_pg;
use crate::db::Db;
use crate::services::discord::settings::ResolvedMemorySettings;
use crate::services::memory::TokenUsage;

#[derive(Clone, Debug)]
pub(crate) struct ApiFrictionRecordContext<'a> {
    pub channel_id: u64,
    pub session_key: Option<&'a str>,
    pub dispatch_id: Option<&'a str>,
    pub provider: &'a str,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ApiFrictionRecordResult {
    pub stored_event_count: usize,
    pub memory_stored_count: usize,
    pub memory_errors: Vec<String>,
    pub token_usage: TokenUsage,
}

pub(crate) async fn record_api_friction_reports(
    db: Option<&Db>,
    pg_pool: Option<&PgPool>,
    memory_settings: &ResolvedMemorySettings,
    context: ApiFrictionRecordContext<'_>,
    reports: &[ApiFrictionReport],
) -> Result<ApiFrictionRecordResult, String> {
    if reports.is_empty() {
        return Ok(ApiFrictionRecordResult::default());
    }

    let _ = db;
    let pg_pool = pg_pool.ok_or_else(|| {
        "postgres pool is required for API friction capture; sqlite fallback is unavailable"
            .to_string()
    })?;
    let inserted_events = store_api_friction_events_pg(pg_pool, &context, reports).await?;
    let stored_event_count = inserted_events.len();
    let memory_result = sync_event_memory_pg(pg_pool, memory_settings, inserted_events).await;

    Ok(ApiFrictionRecordResult {
        stored_event_count,
        memory_stored_count: memory_result.memory_stored_count,
        memory_errors: memory_result.memory_errors,
        token_usage: memory_result.token_usage,
    })
}
