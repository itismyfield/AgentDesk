//! First init holds the existing execution key and intake fence through its local commit.
use super::*;
use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};

pub(crate) async fn facts(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    executor: &str,
    generation: i64,
) -> Result<(IntakeFence, ActivationRows, i64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    custody::lock(&mut tx, &OwnerIdentity::new(provider, channel)).await?;
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=$4 AND status='active')")
        .bind(provider).bind(channel).bind(executor).bind(generation).fetch_one(&mut *tx).await?;
    #[cfg(test)]
    let valid = valid || crate::services::tui_o::ownership::execution::tests::mutant("fence");
    if !valid {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query("LOCK TABLE intake_outbox IN SHARE MODE")
        .execute(&mut *tx)
        .await?;
    let open_intake = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND NOT replay_only AND routing_receipt_id IS NULL AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})")))
        .bind(provider).bind(channel).fetch_one(&mut *tx).await?;
    let queued = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target=$1 AND source='headless_turn' AND status IN ('pending','processing')")
        .bind(format!("channel:{channel}")).fetch_one(&mut *tx).await?;
    Ok((
        IntakeFence {
            _transaction: Some(tx),
        },
        ActivationRows {
            open_intake,
            foreign_sessions: 0,
        },
        queued,
    ))
}
