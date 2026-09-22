use super::*;

pub(super) async fn acquire(
    pool: &PgPool,
    node: &str,
    provider: &str,
    channel: &str,
    nonce: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('agentdesk.execution_capacity.v1'),hashtext($1))",
    )
    .bind(node)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM node_execution_leases WHERE instance_id=$1 AND expires_at<=clock_timestamp()",
    )
    .bind(node)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO node_execution_leases(instance_id,provider,channel_id,nonce,expires_at) VALUES($1,$2,$3,$4,clock_timestamp()+($5*INTERVAL '1 second'))")
        .bind(node).bind(provider).bind(channel).bind(nonce).bind(LEASE_SECONDS).execute(&mut *tx).await?;
    tx.commit().await
}

pub(super) async fn renew(
    pool: &PgPool,
    node: &str,
    provider: &str,
    channel: &str,
    nonce: uuid::Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query("UPDATE node_execution_leases SET expires_at=clock_timestamp()+($5*INTERVAL '1 second') WHERE instance_id=$1 AND provider=$2 AND channel_id=$3 AND nonce=$4 AND expires_at>clock_timestamp()")
        .bind(node).bind(provider).bind(channel).bind(nonce).bind(LEASE_SECONDS).execute(pool).await.map(|r|r.rows_affected()==1)
}

pub(super) async fn release(
    pool: &PgPool,
    node: &str,
    provider: &str,
    channel: &str,
    nonce: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM node_execution_leases WHERE instance_id=$1 AND provider=$2 AND channel_id=$3 AND nonce=$4")
        .bind(node).bind(provider).bind(channel).bind(nonce).execute(pool).await?;
    Ok(())
}
