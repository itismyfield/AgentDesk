//! First init holds the existing execution key and intake fence through its local commit.
use super::*;
use crate::db::channel_custody_lock;
use crate::domain::intake_identity::OwnerIdentity;

pub(crate) async fn facts(
    pool: &PgPool,
    provider: &str,
    channel: &str,
    executor: &str,
    generation: i64,
    own_turn: Option<&str>,
) -> Result<(IntakeFence, ActivationRows, i64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    channel_custody_lock::lock(&mut tx, &OwnerIdentity::new(provider, channel)).await?;
    let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=$4 AND status='active')")
        .bind(provider).bind(channel).bind(executor).bind(generation).fetch_one(&mut *tx).await?;
    if !valid {
        return Err(sqlx::Error::RowNotFound);
    }
    sqlx::query("LOCK TABLE intake_outbox IN SHARE MODE")
        .execute(&mut *tx)
        .await?;
    let open_intake = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND NOT replay_only AND routing_receipt_id IS NULL AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})")))
        .bind(provider).bind(channel).fetch_one(&mut *tx).await?;
    let own_turn_intake = sqlx::query_scalar(AssertSqlSafe(format!("SELECT COUNT(*) FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND user_msg_id=$3 AND NOT replay_only AND routing_receipt_id IS NULL AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})")))
        .bind(provider).bind(channel).bind(own_turn).fetch_one(&mut *tx).await?;
    let queued = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target=$1 AND source='headless_turn' AND status IN ('pending','processing')")
        .bind(format!("channel:{channel}")).fetch_one(&mut *tx).await?;
    Ok((
        IntakeFence {
            _transaction: Some(tx),
        },
        ActivationRows {
            open_intake,
            own_turn_intake,
            foreign_sessions: 0,
        },
        queued,
    ))
}
