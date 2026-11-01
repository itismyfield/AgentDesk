//! Accepted custody preserves the launch barrier; only actual observer evidence may settle it.
use super::*;
use crate::services::discord::{HeadlessTurnStartOutcome, HeadlessTurnStartStatus};

#[derive(Debug)]
pub(super) enum AgentTurnStartDisposition {
    Started,
    Accepted,
    Pending,
    Consumed(String),
    SnapshotInvalid(String),
}

pub(super) fn start_disposition(
    status: HeadlessTurnStartStatus,
    turn_id: &str,
) -> AgentTurnStartDisposition {
    match status {
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
        AgentTurnStartDisposition::Accepted | AgentTurnStartDisposition::Pending => {
            release_to_poller(pool, fire, turn_id).await;
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
    release_to_poller(pool, fire, turn_id).await;
    disposition
}

async fn release_to_poller(pool: &PgPool, fire: &ClaimedFire, turn_id: &str) {
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
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let input = ChannelMailboxHandle::restore_owner_input(stored)?;
    if input.context["delivery_id"] != delivery.delivery_id
        || input.context["turn_id"].as_str() != delivery.turn_id.as_deref()
    {
        anyhow::bail!("scheduled owner delivery/turn mismatch");
    }
    let handle = owner(&input.provider, input.channel).await?;
    // The observer connects to the owner's exact attempt first; a failed projection is
    // repaired under the same ticket on the next poll.
    if let Some((attempt, source)) = input
        .unbound_attempt(&handle)
        .await
        .map_err(anyhow::Error::msg)?
    {
        bind_observer(connection, delivery, &handle, &attempt, source).await?;
        return Ok(false);
    }
    let bound = input
        .may_observe(&handle, started.is_none())
        .await
        .map_err(anyhow::Error::msg)?;
    Ok(bound && started.is_some_and(|started| delivery.started_at >= started))
}

/// The owner an intent names: a test's injected mailbox, else the live runtime's.
async fn owner(
    provider: &str,
    channel: u64,
) -> anyhow::Result<crate::services::turn_orchestrator::ChannelMailboxHandle> {
    #[cfg(test)]
    if let Ok(handle) = OWNER.try_with(Clone::clone) {
        return Ok(handle);
    }
    let registry = crate::engine::ops::turn_ops::health_registry()
        .ok_or_else(|| anyhow::anyhow!("scheduled owner unavailable"))?;
    let mailbox = crate::services::turn_orchestrator::ChannelMailboxHandle::owner_input_mailbox;
    mailbox(&registry, provider, channel)
        .await
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
tokio::task_local! { pub(super) static OWNER: crate::services::turn_orchestrator::ChannelMailboxHandle; }

/// The owner's start of a scheduled turn on its target after the launch fence, with the
/// message's metadata, hint and label as provenance; `None` leaves an unowned channel to the
/// legacy start.
pub(super) async fn owned(
    pool: &PgPool,
    fire: &ClaimedFire,
    (turn_id, channel, prompt): (&str, u64, &str),
    (provider, metadata, (hint, label)): (
        &crate::services::provider::ProviderKind,
        Option<&serde_json::Value>,
        (Option<&str>, Option<&str>),
    ),
) -> anyhow::Result<Option<AgentTurnStartDisposition>> {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let Ok(handle) = owner(provider.as_str(), channel).await else {
        return Ok(None);
    };
    let execution = ChannelMailboxHandle::owner_input_execution(provider.as_str(), channel, false);
    let owned = handle
        .owned_start(execution, (turn_id, channel, prompt))
        .await;
    let Some((original, namespace)) = owned.map_err(anyhow::Error::msg)? else {
        return Ok(None);
    };
    let mut context = metadata.cloned().unwrap_or_else(|| serde_json::json!({}));
    context["provider"] = serde_json::json!(provider.as_str());
    context["delivery_id"] = serde_json::json!(fire.delivery_id);
    context["origin"] = serde_json::json!("scheduled");
    // What the legacy start below would be given, so the owner's start resolves the same way.
    context["metadata"] = serde_json::json!(metadata);
    context["channel_name_hint"] = serde_json::json!(hint);
    context["synthetic_label"] = serde_json::json!(label);
    (context["source"], context["is_dm"]) = (serde_json::json!(super::OUTBOX_SOURCE), false.into());
    let started = start_owned(pool, fire, &handle, turn_id, (original, namespace, context));
    let started = started.await?;
    // The owner runs an accepted start once its drive looks at the queue.
    if let (AgentTurnStartDisposition::Accepted, Some(registry)) =
        (&started, crate::engine::ops::turn_ops::health_registry())
    {
        ChannelMailboxHandle::owner_input_wake(&registry, provider.as_str(), channel).await;
    }
    Ok(Some(started))
}

pub(super) async fn start_owned(
    pool: &PgPool,
    fire: &ClaimedFire,
    handle: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    turn_id: &str,
    (original, namespace, context): (
        crate::services::turn_orchestrator::PendingQueueItem,
        String,
        serde_json::Value,
    ),
) -> anyhow::Result<AgentTurnStartDisposition> {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
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
        ChannelMailboxHandle::new_owner_input(&original, &namespace, provider, context.clone())
    };
    let encoded = format!("owner-input:{}", serde_json::to_string(&input)?);
    let mut committed_input = input.clone();
    committed_input.admission_committed = Some(true);
    let committed = format!("owner-input:{}", serde_json::to_string(&committed_input)?);
    let updated=sqlx::query("UPDATE scheduled_message_deliveries SET error=$4,updated_at=NOW() WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running' AND launch_committed_at IS NOT NULL")
        .bind(&fire.delivery_id).bind(&fire.claim_token).bind(turn_id).bind(&encoded).execute(pool).await?;
    if updated.rows_affected() != 1 {
        anyhow::bail!("scheduled owner intent lost claim fence");
    }
    let disposition = match input.confirm_admission(handle).await {
        Ok(true) => AgentTurnStartDisposition::Accepted,
        Ok(false) => AgentTurnStartDisposition::Pending,
        Err(reason) => {
            tracing::warn!(%reason,"scheduled owner intent held");
            AgentTurnStartDisposition::Pending
        }
    };
    if matches!(disposition, AgentTurnStartDisposition::Accepted) {
        let projected = sqlx::query(
            "UPDATE scheduled_message_deliveries SET error=$5,updated_at=NOW()
            WHERE id=$1 AND claim_token=$2 AND turn_id=$3 AND status='running' AND error=$4",
        )
        .bind(&fire.delivery_id)
        .bind(&fire.claim_token)
        .bind(turn_id)
        .bind(&encoded)
        .bind(committed)
        .execute(pool)
        .await;
        match projected {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => tracing::warn!(
                delivery_id = fire.delivery_id,
                "scheduled admission projection lost its fence; existing owner intent retained"
            ),
            Err(error) => {
                tracing::warn!(delivery_id=fire.delivery_id,%error,"scheduled admission committed; projection will retain its original ticket")
            }
        }
    }
    release_to_poller(pool, fire, turn_id).await;
    Ok(disposition)
}

pub(super) async fn bind_observer(
    connection: &mut sqlx::PgConnection,
    delivery: &RunningAgentDelivery,
    handle: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    attempt: &str,
    source: Option<serde_json::Value>,
) -> anyhow::Result<bool> {
    use crate::services::turn_orchestrator::ChannelMailboxHandle;
    let mut tx = sqlx::Connection::begin(connection).await?;
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
