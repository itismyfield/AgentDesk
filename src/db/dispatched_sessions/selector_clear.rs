//! Clear a Legacy selector and return what the committed write retired.

use sqlx::PgPool;

pub(crate) async fn clear_session_id_by_key_pg(pool: &PgPool, key: &str) -> Result<u64, String> {
    clear_selectors_pg(pool, &[key])
        .await
        .map(|cleared| cleared.len() as u64)
}

pub(crate) struct ClearedSelector {
    pub(crate) provider: Option<String>,
    pub(crate) selector: Option<String>,
}

pub(crate) async fn clear_selectors_pg(
    pool: &PgPool,
    keys: &[&str],
) -> Result<Vec<ClearedSelector>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin session selector clear transaction: {error}"))?;
    let configured = crate::config::session_hosts::configured_channel_ids();
    let mut cleared = Vec::new();
    for key in keys {
        let session_id =
            crate::db::dispatched_session_canonical_identity::resolve_session_id_for_mutation_pg(
                &mut tx, key,
            )
            .await
            .map_err(|error| format!("resolve session selector clear locator: {error:?}"))?;
        let Some(session_id) = session_id else {
            continue;
        };
        let previous = sqlx::query_as::<_, (Option<String>, Option<String>)>(
            "UPDATE sessions
             SET claude_session_id = NULL,
                 raw_provider_session_id = NULL,
                 claude_session_id_recorded_at = NULL,
                 raw_provider_transcript_len_watermark = 0,
                 raw_provider_transcript_watermark_session_id = NULL,
                 raw_provider_transcript_growth_proven = FALSE
             FROM (SELECT provider, claude_session_id FROM sessions WHERE id = $1 FOR UPDATE) previous
             WHERE sessions.id = $1
               AND hosted_execution IS NULL
               AND COALESCE(channel_id, '') <> ALL($2::TEXT[])
               AND COALESCE(thread_channel_id, '') <> ALL($2::TEXT[])
             RETURNING previous.provider, previous.claude_session_id",
        )
        .bind(session_id)
        .bind(&configured)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| format!("clear postgres session selectors: {error}"))?;
        if let Some((provider, selector)) = previous {
            cleared.push(ClearedSelector { provider, selector });
        }
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit session selector clear transaction: {error}"))?;
    Ok(cleared)
}
