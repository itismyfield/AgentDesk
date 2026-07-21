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
            crate::services::observability::metrics::record_relay_circuit_activate_unknown();
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;

    async fn setup(
        name: &str,
    ) -> Option<(
        crate::dispatch::test_support::DispatchPostgresTestDb,
        PgPool,
    )> {
        let db =
            crate::dispatch::test_support::DispatchPostgresTestDb::try_create(name, name).await?;
        let pool = db.connect_and_migrate().await;
        Some((db, pool))
    }

    async fn active_owner(pool: &PgPool, channel_id: &str) {
        sqlx::query("INSERT INTO intake_session_owners(provider,raw_channel_id,owner_instance_id,generation,status) VALUES('discord',$1,'node-a',7,'active')")
            .bind(channel_id).execute(pool).await.unwrap();
    }

    fn request(channel_id: u64, reason: &str) -> CircuitAlertRequest {
        CircuitAlertRequest {
            target: format!("channel:{channel_id}"),
            content: "body".to_string(),
            reason_code: reason.to_string(),
            provider: "discord".to_string(),
            channel_id,
            episode_key: format!("episode-{reason}"),
            baseline_relay_offset: 10,
            open_generation: 1,
        }
    }

    #[tokio::test]
    async fn enqueue_default_off_leaves_all_circuit_columns_null_pg() {
        let Some((db, pool)) = setup("circuit_producer_default_off").await else {
            return;
        };
        active_owner(&pool, "46151").await;
        let _env_lock = crate::config::shared_test_env_lock().lock().unwrap();
        let previous = std::env::var_os(CIRCUIT_STAMP_ENV);
        unsafe { std::env::remove_var(CIRCUIT_STAMP_ENV) };
        let result = enqueue(Some(&pool), &request(46_151, "default-off"), 300).await;
        match previous {
            Some(value) => unsafe { std::env::set_var(CIRCUIT_STAMP_ENV, value) },
            None => unsafe { std::env::remove_var(CIRCUIT_STAMP_ENV) },
        }
        let id = result.unwrap();
        let row = sqlx::query("SELECT circuit_provider,circuit_channel_id,circuit_owner_instance_id,circuit_owner_generation,circuit_episode_key,circuit_baseline_relay_offset,circuit_open_generation,circuit_authority_epoch,circuit_dedupe_ttl_secs FROM message_outbox WHERE id=$1")
            .bind(id).fetch_one(&pool).await.unwrap();
        for column in [
            "circuit_provider",
            "circuit_channel_id",
            "circuit_owner_instance_id",
            "circuit_episode_key",
        ] {
            assert!(row.get::<Option<String>, _>(column).is_none(), "{column}");
        }
        for column in [
            "circuit_owner_generation",
            "circuit_baseline_relay_offset",
            "circuit_open_generation",
            "circuit_authority_epoch",
            "circuit_dedupe_ttl_secs",
        ] {
            assert!(row.get::<Option<i64>, _>(column).is_none(), "{column}");
        }
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn activate_unknown_status_fails_safe_without_reopen_pg() {
        let Some((db, pool)) = setup("circuit_producer_unknown_status").await else {
            return;
        };
        active_owner(&pool, "46152").await;
        let coordinate = match reserve_next_authority(
            &pool,
            "discord",
            "46152",
            "node-a",
            7,
            "episode-unknown",
            10,
            1,
            None,
        )
        .await
        .unwrap()
        {
            AuthorityReservation::Reserved(coordinate) => coordinate,
            other => panic!("unexpected reservation: {other:?}"),
        };
        let alert = request(46_152, "unknown");
        let id = match stage_held(&pool, legacy_message(&alert), &coordinate, 300)
            .await
            .unwrap()
        {
            StageHeldOutcome::Staged { id } => id,
            other => panic!("unexpected stage: {other:?}"),
        };
        sqlx::query("UPDATE message_outbox SET status='unexpected_status' WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(activate(Some(&pool), id).await.unwrap());
        let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "unexpected_status");
        pool.close().await;
        db.drop().await;
    }
}
