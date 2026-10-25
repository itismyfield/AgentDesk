use chrono::{DateTime, Utc};
use sqlx::{FromRow, PgPool};

#[derive(Clone, FromRow)]
pub(crate) struct SessionPrompt {
    pub provider: String,
    pub channel_id: String,
    pub provider_session_id: String,
    pub source_turn_id: String,
    pub started_at: DateTime<Utc>,
    pub system_prompt: String,
    pub anchor_count: i64,
}

pub(crate) async fn fetch(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    session: &str,
) -> Result<Option<SessionPrompt>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM provider_session_system_prompts WHERE provider=$1 AND channel_id=$2 AND provider_session_id=$3")
        .bind(provider).bind(channel).bind(session).fetch_optional(pool).await
}

pub(crate) async fn fetch_latest_session_id(
    pool: &PgPool,
    provider: &str,
    channel: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT provider_session_id FROM provider_session_system_prompts WHERE provider=$1 AND channel_id=$2 ORDER BY started_at DESC, provider_session_id DESC LIMIT 1")
        .bind(provider).bind(channel).fetch_optional(pool).await
}

pub(crate) async fn save(pool: &PgPool, prompt: &SessionPrompt) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    // Serialize insert/prune for one channel so concurrent starts cannot exceed the bound.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "session-prompt:{}:{}",
            prompt.provider, prompt.channel_id
        ))
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query("INSERT INTO provider_session_system_prompts (provider,channel_id,provider_session_id,source_turn_id,started_at,system_prompt,anchor_count) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
        .bind(&prompt.provider).bind(&prompt.channel_id).bind(&prompt.provider_session_id)
        .bind(&prompt.source_turn_id).bind(prompt.started_at).bind(&prompt.system_prompt)
        .bind(prompt.anchor_count).execute(&mut *tx).await?.rows_affected();
    if inserted > 0 {
        // Always retain the inserted session, including a start whose clock moved backwards.
        sqlx::query("DELETE FROM provider_session_system_prompts WHERE provider=$1 AND channel_id=$2 AND provider_session_id NOT IN (SELECT provider_session_id FROM provider_session_system_prompts WHERE provider=$1 AND channel_id=$2 ORDER BY (provider_session_id=$3) DESC, started_at DESC, provider_session_id DESC LIMIT 5)")
            .bind(&prompt.provider).bind(&prompt.channel_id).bind(&prompt.provider_session_id)
            .execute(&mut *tx).await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests;
