//! Feature-gated producer stamp for relay-recovery circuit alerts (#4615 S3c).
//!
//! The flag gates enqueue only. Activation always inspects the row so a stamped
//! row retains its delivery authority fence after a config rollback.

use super::circuit_breaker::CircuitAlertRequest;
use crate::services::cluster::intake_router_hook::owner_record::{
    OwnerIdentity, read_latest_owner_in_tx,
};
use crate::services::message_outbox::{
    OutboxMessage, activate_or_confirm_staged_outbox_pg, stage_outbox_pg_with_ttl,
};
use crate::services::message_outbox_circuit_authority::{
    AuthorityReservation, ResumeActivation, StageHeldOutcome, activate_fenced_by_id,
    reserve_next_authority, stage_held,
};
use sqlx::PgPool;

const CIRCUIT_STAMP_ENV: &str = "AGENTDESK_RELAY_CIRCUIT_STAMP";

fn stamp_enabled() -> bool {
    std::env::var(CIRCUIT_STAMP_ENV).as_deref() == Ok("1")
}

fn legacy_message(request: &CircuitAlertRequest) -> OutboxMessage<'_> {
    OutboxMessage {
        target: &request.target,
        content: &request.content,
        bot: crate::services::discord::bot_role::UtilityBotRole::Announce.alias(),
        source: "stall_watchdog",
        reason_code: Some(&request.reason_code),
        session_key: None,
    }
}

async fn resolve_active_owner(
    pool: &PgPool,
    provider: &str,
    channel_id: &str,
) -> Result<Option<(String, i64)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let identity = OwnerIdentity::new(provider, channel_id);
    let owner = read_latest_owner_in_tx(&mut tx, &identity).await?;
    tx.rollback().await?;
    Ok(owner.and_then(|owner| {
        (owner.status == "active").then_some((owner.owner_instance_id, owner.generation))
    }))
}

async fn legacy_enqueue(
    pool: &PgPool,
    request: &CircuitAlertRequest,
    dedupe_ttl_secs: i64,
) -> Result<i64, String> {
    stage_outbox_pg_with_ttl(pool, legacy_message(request), dedupe_ttl_secs)
        .await
        .map_err(|error| error.to_string())
}

fn fail_open(reason: &str, request: &CircuitAlertRequest, error: impl std::fmt::Display) {
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        reason,
        provider = %request.provider,
        channel_id = request.channel_id,
        error = %error,
        "relay circuit stamp failed open to legacy outbox staging"
    );
    // The existing alert_queued CAS in the breaker bounds this per open frontier;
    // no additional producer-side retry state may create duplicate escalation.
    crate::services::observability::metrics::record_relay_owner_unknown(
        request.channel_id,
        &request.provider,
    );
}

pub(super) async fn enqueue(
    pool: Option<&PgPool>,
    request: &CircuitAlertRequest,
    dedupe_ttl_secs: i64,
) -> Result<i64, String> {
    let pool = pool.ok_or_else(|| "pg_pool unavailable".to_string())?;
    if !stamp_enabled() {
        return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
    }

    let channel_id = request.channel_id.to_string();
    let Some((owner_instance_id, owner_generation)) =
        (match resolve_active_owner(pool, &request.provider, &channel_id).await {
            Ok(owner) => owner,
            Err(error) => {
                fail_open("owner_read", request, &error);
                return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
            }
        })
    else {
        return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
    };
    let (baseline, open_generation) = match (
        i64::try_from(request.baseline_relay_offset),
        i64::try_from(request.open_generation),
    ) {
        (Ok(baseline), Ok(open_generation)) => (baseline, open_generation),
        _ => {
            fail_open("coordinate_overflow", request, "u64 coordinate exceeds i64");
            return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
        }
    };
    let coordinate = match reserve_next_authority(
        pool,
        &request.provider,
        &channel_id,
        &owner_instance_id,
        owner_generation,
        &request.episode_key,
        baseline,
        open_generation,
        None,
    )
    .await
    {
        Ok(AuthorityReservation::Reserved(coordinate)) => coordinate,
        Ok(AuthorityReservation::Stale | AuthorityReservation::NotOwner) => {
            return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
        }
        Err(error) => {
            fail_open("reserve_authority", request, &error);
            return legacy_enqueue(pool, request, dedupe_ttl_secs).await;
        }
    };
    match stage_held(pool, legacy_message(request), &coordinate, dedupe_ttl_secs).await {
        Ok(StageHeldOutcome::Staged { id } | StageHeldOutcome::Idempotent { id }) => Ok(id),
        Ok(StageHeldOutcome::Stale | StageHeldOutcome::NotOwner | StageHeldOutcome::Conflict) => {
            legacy_enqueue(pool, request, dedupe_ttl_secs).await
        }
        Err(error) => {
            fail_open("stage_held", request, &error);
            legacy_enqueue(pool, request, dedupe_ttl_secs).await
        }
    }
}

pub(super) async fn activate(pool: Option<&PgPool>, id: i64) -> Result<bool, String> {
    let pool = pool.ok_or_else(|| "pg_pool unavailable".to_string())?;
    match activate_fenced_by_id(pool, id)
        .await
        .map_err(|error| error.to_string())?
    {
        ResumeActivation::Activated
        | ResumeActivation::AlreadyDeliverable
        | ResumeActivation::Terminal
        | ResumeActivation::RevokedOrFenced => Ok(true),
        ResumeActivation::Superseded
        | ResumeActivation::OwnerAdvanced
        | ResumeActivation::Missing => Ok(false),
        ResumeActivation::NotCircuit => activate_or_confirm_staged_outbox_pg(pool, id)
            .await
            .map_err(|error| error.to_string()),
        ResumeActivation::Unknown => {
            tracing::error!(
                target: "agentdesk::discord::relay_recovery",
                outbox_id = id,
                "unknown circuit outbox status; refusing to reopen or deliver"
            );
            Ok(true)
        }
    }
}
