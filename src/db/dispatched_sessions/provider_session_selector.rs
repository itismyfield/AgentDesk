use sqlx::PgPool;

#[derive(sqlx::FromRow)]
pub struct ProviderSessionIds {
    pub resolved_session_key: String,
    pub claude_session_id: Option<String>,
    pub raw_provider_session_id: Option<String>,
    pub cwd: Option<String>,
    pub cache_entry_age_secs: Option<i64>,
    pub raw_provider_transcript_len_watermark: Option<i64>,
    pub raw_provider_transcript_watermark_session_id: Option<String>,
    pub raw_provider_transcript_growth_proven: bool,
}

macro_rules! selector_query {
    ($predicate:literal) => {
        concat!(
            "SELECT session_key AS resolved_session_key, claude_session_id, raw_provider_session_id, cwd,
                    EXTRACT(EPOCH FROM (NOW() - COALESCE(claude_session_id_recorded_at, created_at)))::BIGINT
                        AS cache_entry_age_secs,
                    raw_provider_transcript_len_watermark,
                    raw_provider_transcript_watermark_session_id,
                    raw_provider_transcript_growth_proven
             FROM sessions WHERE ",
            $predicate
        )
    };
}

pub async fn load_provider_session_ids_pg(
    pool: &PgPool,
    session_key: &str,
    provider: Option<&str>,
) -> Result<Option<ProviderSessionIds>, String> {
    let Some(key) =
        super::resolve_session_locator_pg(pool, session_key, "provider-session").await?
    else {
        return Ok(None);
    };
    sqlx::query_as(selector_query!(
        "session_key = $1 AND ($2::TEXT IS NULL OR provider = $2)"
    ))
    .bind(key)
    .bind(provider)
    .fetch_optional(pool)
    .await
    .map_err(|error| error.to_string())
}

#[cfg(unix)]
pub(super) async fn load_for_hosted_execution_pg(
    pool: &PgPool,
    row_id: i64,
    raw: &serde_json::Value,
    owner: &super::hosted_execution::HostedOwner,
) -> Result<Option<ProviderSessionIds>, String> {
    sqlx::query_as(selector_query!(
        "id = $1 AND hosted_execution = $2 AND provider = $3
         AND identity_kind = 'discord_channel' AND discord_token_hash = $4 AND channel_id = $5"
    ))
    .bind(row_id)
    .bind(raw)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| error.to_string())
}
