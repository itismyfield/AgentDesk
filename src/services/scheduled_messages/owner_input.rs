//! Accepted custody preserves the launch barrier; only actual observer evidence may settle it.
use super::*;
use crate::services::discord::{HeadlessTurnStartOutcome, HeadlessTurnStartStatus};

#[derive(Debug)]
pub(super) enum AgentTurnStartDisposition {
    Started,
    #[cfg(test)]
    Accepted,
    #[cfg(test)]
    Pending,
    Consumed(String),
    SnapshotInvalid(String),
}

pub(super) fn start_disposition(
    status: HeadlessTurnStartStatus,
    turn_id: &str,
) -> AgentTurnStartDisposition {
    match status {
        #[cfg(test)]
        HeadlessTurnStartStatus::Accepted => AgentTurnStartDisposition::Accepted,
        HeadlessTurnStartStatus::Started => AgentTurnStartDisposition::Started,
        HeadlessTurnStartStatus::Consumed => AgentTurnStartDisposition::Consumed(turn_id.into()),
    }
}

pub(super) async fn consume_start(
    pool: &PgPool,
    fire: &ClaimedFire,
    turn_id: &str,
    outcome: HeadlessTurnStartOutcome,
) -> AgentTurnStartDisposition {
    let disposition = start_disposition(outcome.status, turn_id);
    match disposition {
        #[cfg(test)]
        AgentTurnStartDisposition::Accepted | AgentTurnStartDisposition::Pending => {
            return disposition;
        }
        AgentTurnStartDisposition::Consumed(_) | AgentTurnStartDisposition::SnapshotInvalid(_) => {
            return disposition;
        }
        AgentTurnStartDisposition::Started => {}
    }
    // A post-start mismatch or failed acknowledgement never re-arms a committed launch.
    if outcome.turn_id != turn_id {
        tracing::error!(
            expected_turn_id = turn_id,
            actual_turn_id = outcome.turn_id,
            "[smsg] started turn id mismatched its reservation; leaving launch committed and fail-closed"
        );
    } else {
        match db::mark_delivery_agent_turn_started_pg(
            pool,
            &fire.message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
            LEASE_SECS,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                delivery_id = fire.delivery_id,
                turn_id,
                "[smsg] launched turn could not record its runtime acknowledgement; launch remains fail-closed"
            ),
            Err(error) => tracing::warn!(
                delivery_id = fire.delivery_id,
                turn_id,
                "[smsg] failed to record runtime acknowledgement; launch remains fail-closed: {error}"
            ),
        }
    }
    match db::release_agent_delivery_to_poller_pg(pool, &fire.delivery_id, &fire.claim_token).await
    {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            delivery_id = fire.delivery_id,
            turn_id,
            "[smsg] started turn could not be released to poller; lease adoption will recover it"
        ),
        Err(error) => tracing::warn!(
            delivery_id = fire.delivery_id,
            turn_id,
            "[smsg] failed to release started turn to poller; lease adoption will recover it: {error}"
        ),
    }
    disposition
}

pub(super) async fn may_observe(
    connection: &mut sqlx::PgConnection,
    delivery: &RunningAgentDelivery,
) -> anyhow::Result<bool> {
    let Some((error,started))=sqlx::query_as::<_,(Option<String>,Option<DateTime<Utc>>)>("SELECT error,turn_started_at FROM scheduled_message_deliveries WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running'")
        .bind(&delivery.delivery_id).bind(&delivery.claim_token).bind(&delivery.turn_id).fetch_optional(&mut *connection).await? else {return Ok(false)};
    let Some(raw) = error
        .as_deref()
        .and_then(|error| error.strip_prefix("owner-input:"))
    else {
        return Ok(true);
    };
    let stored: serde_json::Value = serde_json::from_str(raw)?;
    #[cfg(test)]
    {
        use crate::services::turn_orchestrator::ChannelMailboxHandle;
        let input = ChannelMailboxHandle::restore_owner_input(stored)?;
        if input.context["delivery_id"] != delivery.delivery_id
            || input.context["turn_id"].as_str() != delivery.turn_id.as_deref()
        {
            anyhow::bail!("scheduled owner delivery/turn mismatch");
        }
        let handle = OWNER
            .try_with(Clone::clone)
            .map_err(|_| anyhow::anyhow!("scheduled owner unavailable"))?;
        let bound = input
            .may_observe(&handle)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(bound && started.is_some_and(|started| delivery.started_at >= started))
    }
    #[cfg(not(test))]
    {
        let _ = (stored, started);
        Ok(false)
    }
}

#[cfg(test)]
tokio::task_local! { pub(super) static OWNER: crate::services::turn_orchestrator::ChannelMailboxHandle; }

#[cfg(test)]
pub(super) async fn start_owned(
    pool: &PgPool,
    fire: &ClaimedFire,
    turn_id: &str,
    original: crate::services::turn_orchestrator::PendingQueueItem,
    context: serde_json::Value,
) -> anyhow::Result<AgentTurnStartDisposition> {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let handle = OWNER.with(Clone::clone);
    let provider = context["provider"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing scheduled provider"))?;
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT error FROM scheduled_message_deliveries WHERE id=$1 AND claim_token=$2",
    )
    .bind(&fire.delivery_id)
    .bind(&fire.claim_token)
    .fetch_one(pool)
    .await?;
    let input = if let Some(raw) = stored
        .as_deref()
        .and_then(|s| s.strip_prefix("owner-input:"))
    {
        serde_json::from_str(raw)?
    } else {
        let namespace = handle
            .v2_request(|slot| {
                slot.as_ref()
                    .and_then(|o| o.snapshot.namespace.clone())
                    .ok_or_else(|| "owner namespace missing".into())
            })
            .await
            .map_err(anyhow::Error::msg)?;
        ChannelMailboxHandle::new_owner_input(&original, &namespace, provider, context.clone())
    };
    let encoded = format!("owner-input:{}", serde_json::to_string(&input)?);
    let updated=sqlx::query("UPDATE scheduled_message_deliveries SET error=$4,updated_at=NOW() WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running' AND launch_committed_at IS NOT NULL")
        .bind(&fire.delivery_id).bind(&fire.claim_token).bind(turn_id).bind(&encoded).execute(pool).await?;
    if updated.rows_affected() != 1 {
        anyhow::bail!("scheduled owner intent lost claim fence");
    }
    Ok(match input.confirm_admission(&handle).await {
        Ok(true) => AgentTurnStartDisposition::Accepted,
        Ok(false) => AgentTurnStartDisposition::Pending,
        Err(reason) => {
            tracing::warn!(%reason,"scheduled owner intent held");
            AgentTurnStartDisposition::Pending
        }
    })
}

#[cfg(test)]
pub(super) async fn bind_observer(
    pool: &PgPool,
    delivery: &RunningAgentDelivery,
    handle: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    attempt: &str,
    source: Option<serde_json::Value>,
) -> anyhow::Result<bool> {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let mut tx = pool.begin().await?;
    if !db::lock_active_delivery_tx(
        &mut tx,
        &delivery.scheduled_message_id,
        &delivery.delivery_id,
        &delivery.claim_token,
    )
    .await?
    {
        return Ok(false);
    }
    let error:String=sqlx::query_scalar("SELECT error FROM scheduled_message_deliveries WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running'")
        .bind(&delivery.delivery_id).bind(&delivery.claim_token).bind(&delivery.turn_id).fetch_one(&mut *tx).await?;
    let raw = error
        .strip_prefix("owner-input:")
        .ok_or_else(|| anyhow::anyhow!("owner intent missing"))?;
    let mut input = ChannelMailboxHandle::restore_owner_input(serde_json::from_str(raw)?)?;
    let turn_id = delivery
        .turn_id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("logical turn missing"))?;
    if input.context["delivery_id"] != delivery.delivery_id {
        anyhow::bail!("stale scheduled observer");
    }
    input
        .bind_observer(handle, attempt, source, turn_id)
        .await
        .map_err(anyhow::Error::msg)?;
    let encoded = format!("owner-input:{}", serde_json::to_string(&input)?);
    let updated=sqlx::query("UPDATE scheduled_message_deliveries SET error=$4,turn_started_at=COALESCE(turn_started_at,NOW()),
            started_at=CASE WHEN turn_started_at IS NULL THEN NOW() ELSE started_at END,
            lease_expires_at=NOW()+($5::bigint*INTERVAL '1 second'),updated_at=NOW()
            WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running' AND launch_committed_at IS NOT NULL")
        .bind(&delivery.delivery_id).bind(&delivery.claim_token).bind(turn_id).bind(encoded).bind(LEASE_SECS).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(updated.rows_affected() == 1)
}
