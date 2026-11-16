//! One shared effect/attempt survives every owner and writer incarnation.
use super::*;
pub(super) async fn structural(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
) -> Result<(), AcquireError> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_grants g JOIN o_herdr_debt_effects e ON e.id=g.effect_id WHERE g.id=$1 AND g.claim_core=$2 AND g.episode=$3 AND e.id=$4 AND e.episode=g.episode AND e.provider=$5 AND e.channel_id=$6 AND e.input_key=$7 AND e.coverage=$8 AND e.plan_sha=$9 AND e.snapshots ? $10 AND e.payload_sha=$11 AND e.unit_key=$12 AND e.piece_index=$13 AND e.bot_id=$14)")
        .bind(r.grant_id).bind(r.core()?).bind(&r.episode).bind(r.effect_id).bind(&r.provider).bind(&r.channel).bind(&r.input_key).bind(&r.coverage).bind(&r.plan_sha).bind(&r.snapshot_sha).bind(&r.payload_sha).bind(serde_json::to_value(&r.unit_key).map_err(|_|denied("debt unit encoding"))?).bind(i64::from(r.piece_index)).bind(i64::try_from(r.authority.bot_id).map_err(|_|denied("debt bot id out of range"))?).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(denied("debt grant or original identity changed"));
    }
    Ok(())
}
pub(super) async fn check(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
    grant_phase: &str,
    effect_phase: &str,
) -> Result<(), AcquireError> {
    structural(tx, r).await?;
    let binding =
        serde_json::to_value(&r.authority).map_err(|_| denied("debt authority encoding"))?;
    let current:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_runs s WHERE s.provider=$1 AND s.channel_id=$2 AND s.binding=$3 AND s.active AND ((s.binding->>'domain'='execution' AND EXISTS(SELECT 1 FROM intake_session_owners o WHERE o.provider=s.provider AND o.raw_channel_id=s.channel_id AND o.owner_instance_id=s.binding->>'custodian' AND o.generation=(s.binding->>'revision')::bigint AND o.status='active' AND o.routing_cleanup IS NULL)) OR (s.binding->>'domain'='gateway' AND NOT EXISTS(SELECT 1 FROM intake_session_owners o WHERE o.provider=s.provider AND o.raw_channel_id=s.channel_id AND o.status='active') AND NOT EXISTS(SELECT 1 FROM intake_outbox d WHERE d.provider=s.provider AND d.channel_id=s.channel_id AND d.routing_receipt_id IS NOT NULL))))")
        .bind(&r.provider).bind(&r.channel).bind(binding).fetch_one(&mut **tx).await?;
    if !current {
        return Err(denied("debt custodian or writer run changed"));
    }
    let (grant,state,proof):(String,String,Option<Value>)=sqlx::query_as("SELECT g.state,e.state,e.receipt FROM o_herdr_debt_grants g JOIN o_herdr_debt_effects e ON e.id=g.effect_id WHERE g.id=$1")
        .bind(r.grant_id).fetch_one(&mut **tx).await?;
    if grant != grant_phase {
        return Err(denied("debt capability is not issued for this phase"));
    }
    receipt::admit(&state, effect_phase, proof.as_ref(), r)?;
    Ok(())
}
pub(super) async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
) -> Result<Uuid, AcquireError> {
    check(tx, r, "issued", "ready").await?;
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO o_herdr_debt_attempts(id,effect_id,grant_id,payload_sha,phase) VALUES($1,$2,$3,$4,'reserved')")
        .bind(attempt).bind(r.effect_id).bind(r.grant_id).bind(&r.payload_sha).execute(&mut **tx).await?;
    sqlx::query("UPDATE o_herdr_debt_effects SET state='reserved',attempt_id=$2 WHERE id=$1")
        .bind(r.effect_id)
        .bind(attempt)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE o_herdr_debt_grants SET state='reserved' WHERE id=$1")
        .bind(r.grant_id)
        .execute(&mut **tx)
        .await?;
    Ok(attempt)
}
