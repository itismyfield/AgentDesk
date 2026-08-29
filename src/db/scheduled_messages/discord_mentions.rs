//! Rolling-upgrade capability gates for Discord-only scheduled mentions.

use sqlx::{PgPool, Postgres, Transaction};

/// True when every online cluster worker can render scheduled Discord mentions
/// without leaking them into external-provider payloads.
pub async fn discord_mentions_rollout_ready_pg(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT NOT EXISTS (\
             SELECT 1 FROM worker_nodes \
             WHERE status = 'online' \
               AND COALESCE(\
                   capabilities #>> '{scheduled_messages,discord_mention_consumer_v1}', \
                   'false'\
               ) <> 'true'\
         )",
    )
    .fetch_one(pool)
    .await
}

/// Declare that this claim transaction understands the mention column and
/// renders it only into the Discord outbox.
pub(super) async fn declare_discord_mention_consumer_v1_tx(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT set_config(\
            'agentdesk.scheduled_discord_mention_consumer_v1', \
            'enabled', \
            true\
         )",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}
