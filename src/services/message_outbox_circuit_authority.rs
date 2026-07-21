//! PostgreSQL authority for channel-scoped circuit alert activation (#4615 S3a).
//!
//! S3a is dormant: live producer wiring is forbidden until S3b adds the worker
//! delivery fence. Vouch cancellation only covers `held`/`pending` rows.

use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CircuitCoordinate {
    pub provider: String,
    pub channel_id: String,
    pub owner_instance_id: String,
    pub owner_generation: i64,
    pub episode_key: String,
    pub baseline_relay_offset: i64,
    pub open_generation: i64,
    pub authority_epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AuthorityReservation {
    Reserved(CircuitCoordinate),
    Stale,
    NotOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StageHeldOutcome {
    Staged { id: i64 },
    Idempotent { id: i64 },
    Conflict,
    Stale,
    NotOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CircuitActivation {
    Activated,
    AlreadyActivated,
    Stale,
    NotOwner,
}

/// Idempotent resume outcome for a row whose immutable circuit coordinate is
/// stored in `message_outbox`. `NotCircuit` deliberately leaves legacy rows on
/// the existing activation path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResumeActivation {
    Activated,
    AlreadyDeliverable,
    Terminal,
    RevokedOrFenced,
    Superseded,
    OwnerAdvanced,
    NotCircuit,
    Missing,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreshVouchRevoke {
    Revoked,
    Stale,
    NotOwner,
}

fn normalize(provider: &str, channel_id: &str) -> (String, String) {
    (
        provider.trim().to_lowercase(),
        channel_id.trim().to_string(),
    )
}

async fn lock_channel(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    channel_id: &str,
) -> Result<(), sqlx::Error> {
    let identity = crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
        provider, channel_id,
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn owner_is_current(
    tx: &mut Transaction<'_, Postgres>,
    coordinate: &CircuitCoordinate,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM intake_session_owners
          WHERE provider=$1 AND raw_channel_id=$2 AND status='active'
            AND owner_instance_id=$3 AND generation=$4)",
    )
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .fetch_one(&mut **tx)
    .await
}

fn same_logical_coordinate(row: &sqlx::postgres::PgRow, coordinate: &CircuitCoordinate) -> bool {
    row.get::<String, _>("owner_instance_id") == coordinate.owner_instance_id
        && row.get::<i64, _>("owner_generation") == coordinate.owner_generation
        && row.get::<String, _>("episode_key") == coordinate.episode_key
        && row.get::<i64, _>("baseline_relay_offset") == coordinate.baseline_relay_offset
        && row.get::<i64, _>("open_generation") == coordinate.open_generation
}

fn valid_same_episode_frontier_transition(
    current_baseline: i64,
    current_open_generation: i64,
    requested_baseline: i64,
    requested_open_generation: i64,
) -> bool {
    // Every frontier advance in `reserve_in_root` and `open_alert_cas_in_root`
    // updates both values. PostgreSQL authority observes only the subsequence
    // that reaches alert reservation, so multiple producer advances may occur
    // between observations; both axes must therefore advance strictly, without
    // requiring adjacent open generations.
    requested_baseline > current_baseline && requested_open_generation > current_open_generation
}

/// Reserve the first or next channel-global authority epoch under owner lock.
/// `expected_authority_epoch` is the caller's current-file pin: `None` creates
/// epoch 1; `Some(n)` may idempotently return the current coordinate at `n`, or
/// advance a different caller-authorized coordinate to exactly `n + 1`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_next_authority(
    pool: &PgPool,
    provider: &str,
    channel_id: &str,
    owner_instance_id: &str,
    owner_generation: i64,
    episode_key: &str,
    baseline_relay_offset: i64,
    open_generation: i64,
    expected_authority_epoch: Option<i64>,
) -> Result<AuthorityReservation, sqlx::Error> {
    let (provider, channel_id) = normalize(provider, channel_id);
    let requested = CircuitCoordinate {
        provider,
        channel_id,
        owner_instance_id: owner_instance_id.to_string(),
        owner_generation,
        episode_key: episode_key.to_string(),
        baseline_relay_offset,
        open_generation,
        authority_epoch: 0,
    };
    if requested.owner_generation < 0
        || requested.baseline_relay_offset < 0
        || requested.open_generation < 0
        || expected_authority_epoch.is_some_and(|epoch| epoch <= 0)
    {
        return Ok(AuthorityReservation::Stale);
    }
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &requested.provider, &requested.channel_id).await?;
    if !owner_is_current(&mut tx, &requested).await? {
        tx.rollback().await?;
        return Ok(AuthorityReservation::NotOwner);
    }
    let current = sqlx::query(
        "SELECT owner_instance_id,owner_generation,episode_key,baseline_relay_offset,
                open_generation,authority_epoch,revoked_at
           FROM message_outbox_circuit_authority
          WHERE provider=$1 AND channel_id=$2",
    )
    .bind(&requested.provider)
    .bind(&requested.channel_id)
    .fetch_optional(&mut *tx)
    .await?;

    let next_epoch = match current.as_ref() {
        None if expected_authority_epoch.is_none() => 1,
        Some(row)
            if expected_authority_epoch == Some(row.get("authority_epoch"))
                && same_logical_coordinate(row, &requested)
                && row
                    .get::<Option<chrono::DateTime<chrono::Utc>>, _>("revoked_at")
                    .is_none() =>
        {
            let mut coordinate = requested;
            coordinate.authority_epoch = row.get("authority_epoch");
            tx.commit().await?;
            return Ok(AuthorityReservation::Reserved(coordinate));
        }
        Some(row) if expected_authority_epoch == Some(row.get("authority_epoch")) => {
            let same_episode = row.get::<String, _>("episode_key") == requested.episode_key;
            if same_episode
                && !valid_same_episode_frontier_transition(
                    row.get("baseline_relay_offset"),
                    row.get("open_generation"),
                    requested.baseline_relay_offset,
                    requested.open_generation,
                )
            {
                tx.rollback().await?;
                return Ok(AuthorityReservation::Stale);
            }
            if requested.owner_generation < row.get::<i64, _>("owner_generation") {
                tx.rollback().await?;
                return Ok(AuthorityReservation::Stale);
            }
            let Some(next_epoch) = row.get::<i64, _>("authority_epoch").checked_add(1) else {
                tx.rollback().await?;
                return Ok(AuthorityReservation::Stale);
            };
            next_epoch
        }
        _ => {
            tx.rollback().await?;
            return Ok(AuthorityReservation::Stale);
        }
    };
    let mut coordinate = requested;
    coordinate.authority_epoch = next_epoch;
    sqlx::query(
        "INSERT INTO message_outbox_circuit_authority
             (provider,channel_id,owner_instance_id,owner_generation,episode_key,
              baseline_relay_offset,open_generation,authority_epoch,revoked_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,NULL)
         ON CONFLICT (provider,channel_id) DO UPDATE SET
             owner_instance_id=EXCLUDED.owner_instance_id,
             owner_generation=EXCLUDED.owner_generation,
             episode_key=EXCLUDED.episode_key,
             baseline_relay_offset=EXCLUDED.baseline_relay_offset,
             open_generation=EXCLUDED.open_generation,
             authority_epoch=EXCLUDED.authority_epoch,
             revoked_at=NULL,updated_at=NOW()",
    )
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(AuthorityReservation::Reserved(coordinate))
}

async fn authority_is_current(
    tx: &mut Transaction<'_, Postgres>,
    coordinate: &CircuitCoordinate,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM message_outbox_circuit_authority
          WHERE provider=$1 AND channel_id=$2 AND owner_instance_id=$3
            AND owner_generation=$4 AND episode_key=$5 AND baseline_relay_offset=$6
            AND open_generation=$7 AND authority_epoch=$8 AND revoked_at IS NULL)",
    )
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .fetch_one(&mut **tx)
    .await
}

/// Stage an exact worker-invisible row. Dedupe collisions are idempotent only
/// when payload identity and every circuit stamp match; all others fail closed.
pub(crate) async fn stage_held(
    pool: &PgPool,
    message: crate::services::message_outbox::OutboxMessage<'_>,
    coordinate: &CircuitCoordinate,
    dedupe_ttl_secs: i64,
) -> Result<StageHeldOutcome, crate::services::message_outbox::OutboxEnqueueError> {
    crate::services::message_outbox::validate_outbox_source(message.source)?;
    let reason_code = crate::services::message_outbox::normalized_reason_code(message.reason_code);
    let session_key = crate::services::message_outbox::normalized_session_key(
        message.target,
        message.session_key,
    );
    let dedupe_key = crate::services::message_outbox::dedupe_key_for_message(
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
    )
    .ok_or_else(|| {
        crate::services::message_outbox::OutboxEnqueueError::Database(sqlx::Error::Protocol(
            "circuit staging requires a dedupe identity".to_string(),
        ))
    })?;
    let dedupe_ttl_secs = dedupe_ttl_secs.max(1);
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &coordinate.provider, &coordinate.channel_id).await?;
    if !owner_is_current(&mut tx, coordinate).await? {
        tx.rollback().await?;
        return Ok(StageHeldOutcome::NotOwner);
    }
    if !authority_is_current(&mut tx, coordinate).await? {
        tx.rollback().await?;
        return Ok(StageHeldOutcome::Stale);
    }
    sqlx::query(
        "DELETE FROM message_outbox WHERE dedupe_key=$1 AND status='held'
           AND dedupe_expires_at IS NOT NULL AND dedupe_expires_at<=NOW()",
    )
    .bind(&dedupe_key)
    .execute(&mut *tx)
    .await?;
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
             (target,content,bot,source,status,reason_code,session_key,dedupe_key,dedupe_expires_at,
              circuit_provider,circuit_channel_id,circuit_episode_key,circuit_baseline_relay_offset,
              circuit_open_generation,circuit_authority_epoch,circuit_dedupe_ttl_secs,
              circuit_owner_instance_id,circuit_owner_generation)
         VALUES ($1,$2,$3,$4,'held',$5,$6,$7,NOW()+($8::BIGINT*INTERVAL '1 second'),
                 $9,$10,$11,$12,$13,$14,$8,$15,$16)
         ON CONFLICT (dedupe_key) WHERE dedupe_key IS NOT NULL
             AND status NOT IN ('failed','cancelled') DO NOTHING RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(&dedupe_key)
    .bind(dedupe_ttl_secs)
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = inserted {
        tx.commit().await?;
        return Ok(StageHeldOutcome::Staged { id });
    }
    let existing = sqlx::query(
        "SELECT id,target,content,bot,source,reason_code,session_key,status,
                circuit_provider,circuit_channel_id,circuit_episode_key,circuit_baseline_relay_offset,
                circuit_open_generation,circuit_authority_epoch,circuit_dedupe_ttl_secs,
                circuit_owner_instance_id,circuit_owner_generation
           FROM message_outbox WHERE dedupe_key=$1 AND status NOT IN ('failed','cancelled')",
    ).bind(&dedupe_key).fetch_optional(&mut *tx).await?;
    let exact = existing.as_ref().is_some_and(|row| {
        row.get::<String, _>("target") == message.target
            && row.get::<String, _>("content") == message.content
            && row.get::<String, _>("bot") == message.bot
            && row.get::<String, _>("source") == message.source
            && row.get::<Option<String>, _>("reason_code").as_deref() == reason_code
            && row.get::<Option<String>, _>("session_key") == session_key
            && row.get::<String, _>("status") == "held"
            && row.get::<Option<String>, _>("circuit_provider").as_deref()
                == Some(coordinate.provider.as_str())
            && row
                .get::<Option<String>, _>("circuit_channel_id")
                .as_deref()
                == Some(coordinate.channel_id.as_str())
            && row
                .get::<Option<String>, _>("circuit_episode_key")
                .as_deref()
                == Some(coordinate.episode_key.as_str())
            && row.get::<Option<i64>, _>("circuit_baseline_relay_offset")
                == Some(coordinate.baseline_relay_offset)
            && row.get::<Option<i64>, _>("circuit_open_generation")
                == Some(coordinate.open_generation)
            && row.get::<Option<i64>, _>("circuit_authority_epoch")
                == Some(coordinate.authority_epoch)
            && row.get::<Option<i64>, _>("circuit_dedupe_ttl_secs") == Some(dedupe_ttl_secs)
            && row
                .get::<Option<String>, _>("circuit_owner_instance_id")
                .as_deref()
                == Some(coordinate.owner_instance_id.as_str())
            && row.get::<Option<i64>, _>("circuit_owner_generation")
                == Some(coordinate.owner_generation)
    });
    let outcome = if exact {
        StageHeldOutcome::Idempotent {
            id: existing.unwrap().get("id"),
        }
    } else {
        StageHeldOutcome::Conflict
    };
    tx.rollback().await?;
    Ok(outcome)
}

#[allow(dead_code)]
pub(crate) async fn activate_fenced(
    pool: &PgPool,
    outbox_id: i64,
    coordinate: &CircuitCoordinate,
) -> Result<CircuitActivation, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &coordinate.provider, &coordinate.channel_id).await?;
    if !owner_is_current(&mut tx, coordinate).await? {
        tx.rollback().await?;
        return Ok(CircuitActivation::NotOwner);
    }
    if !authority_is_current(&mut tx, coordinate).await? {
        tx.rollback().await?;
        return Ok(CircuitActivation::Stale);
    }
    let changed = sqlx::query(
        "UPDATE message_outbox SET status='pending' WHERE id=$1 AND status='held'
           AND circuit_provider=$2 AND circuit_channel_id=$3 AND circuit_episode_key=$4
           AND circuit_baseline_relay_offset=$5 AND circuit_open_generation=$6
           AND circuit_authority_epoch=$7 AND circuit_owner_instance_id=$8
           AND circuit_owner_generation=$9",
    )
    .bind(outbox_id)
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 1 {
        tx.commit().await?;
        return Ok(CircuitActivation::Activated);
    }
    let already_pending: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM message_outbox WHERE id=$1 AND status='pending'
           AND circuit_provider=$2 AND circuit_channel_id=$3 AND circuit_episode_key=$4
           AND circuit_baseline_relay_offset=$5 AND circuit_open_generation=$6
           AND circuit_authority_epoch=$7 AND circuit_owner_instance_id=$8
           AND circuit_owner_generation=$9)",
    )
    .bind(outbox_id)
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .fetch_one(&mut *tx)
    .await?;
    tx.rollback().await?;
    Ok(if already_pending {
        CircuitActivation::AlreadyActivated
    } else {
        CircuitActivation::Stale
    })
}

/// Resume a staged alert by its immutable outbox coordinate. The row itself,
/// rather than the current feature flag, determines whether fenced activation is
/// required so an ON→OFF rollback can never un-fence an already stamped row.
pub(crate) async fn activate_fenced_by_id(
    pool: &PgPool,
    outbox_id: i64,
) -> Result<ResumeActivation, sqlx::Error> {
    let row = sqlx::query(
        "SELECT status,circuit_provider,circuit_channel_id,circuit_owner_instance_id,
                circuit_owner_generation,circuit_episode_key,circuit_baseline_relay_offset,
                circuit_open_generation,circuit_authority_epoch
           FROM message_outbox WHERE id=$1",
    )
    .bind(outbox_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(ResumeActivation::Missing);
    };
    let Some(provider) = row.get::<Option<String>, _>("circuit_provider") else {
        return Ok(ResumeActivation::NotCircuit);
    };
    let coordinate = CircuitCoordinate {
        provider,
        channel_id: row
            .get::<Option<String>, _>("circuit_channel_id")
            .unwrap_or_default(),
        owner_instance_id: row
            .get::<Option<String>, _>("circuit_owner_instance_id")
            .unwrap_or_default(),
        owner_generation: row
            .get::<Option<i64>, _>("circuit_owner_generation")
            .unwrap_or(-1),
        episode_key: row
            .get::<Option<String>, _>("circuit_episode_key")
            .unwrap_or_default(),
        baseline_relay_offset: row
            .get::<Option<i64>, _>("circuit_baseline_relay_offset")
            .unwrap_or(-1),
        open_generation: row
            .get::<Option<i64>, _>("circuit_open_generation")
            .unwrap_or(-1),
        authority_epoch: row
            .get::<Option<i64>, _>("circuit_authority_epoch")
            .unwrap_or(-1),
    };
    let status: String = row.get("status");
    if status != "held" {
        return Ok(match status.as_str() {
            "pending" | "processing" | "sent" | "delivered" => ResumeActivation::AlreadyDeliverable,
            "failed" => ResumeActivation::Terminal,
            // Unlike legacy activation, circuit cancellation means authority
            // revoke/fencing; reopening would re-stage a superseded alert.
            "cancelled" => ResumeActivation::RevokedOrFenced,
            _ => ResumeActivation::Unknown,
        });
    }

    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &coordinate.provider, &coordinate.channel_id).await?;
    if !owner_is_current(&mut tx, &coordinate).await? {
        tx.rollback().await?;
        return Ok(ResumeActivation::OwnerAdvanced);
    }
    if !authority_is_current(&mut tx, &coordinate).await? {
        tx.rollback().await?;
        return Ok(ResumeActivation::Superseded);
    }
    let changed = sqlx::query(
        "UPDATE message_outbox SET status='pending' WHERE id=$1 AND status='held'
           AND circuit_provider=$2 AND circuit_channel_id=$3 AND circuit_episode_key=$4
           AND circuit_baseline_relay_offset=$5 AND circuit_open_generation=$6
           AND circuit_authority_epoch=$7 AND circuit_owner_instance_id=$8
           AND circuit_owner_generation=$9",
    )
    .bind(outbox_id)
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 1 {
        tx.commit().await?;
        return Ok(ResumeActivation::Activated);
    }
    let status = sqlx::query_scalar::<_, String>("SELECT status FROM message_outbox WHERE id=$1")
        .bind(outbox_id)
        .fetch_optional(&mut *tx)
        .await?;
    tx.rollback().await?;
    Ok(match status.as_deref() {
        None => ResumeActivation::Missing,
        Some("pending" | "processing" | "sent" | "delivered") => {
            ResumeActivation::AlreadyDeliverable
        }
        Some("failed") => ResumeActivation::Terminal,
        Some("cancelled") => ResumeActivation::RevokedOrFenced,
        Some("held") => ResumeActivation::Superseded,
        Some(_) => ResumeActivation::Unknown,
    })
}

#[allow(dead_code)]
pub(crate) async fn revoke_on_fresh_vouch(
    pool: &PgPool,
    coordinate: &CircuitCoordinate,
    reason: &str,
) -> Result<FreshVouchRevoke, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &coordinate.provider, &coordinate.channel_id).await?;
    if !owner_is_current(&mut tx, coordinate).await? {
        tx.rollback().await?;
        return Ok(FreshVouchRevoke::NotOwner);
    }
    let revoked = sqlx::query(
        "UPDATE message_outbox_circuit_authority SET revoked_at=NOW(),updated_at=NOW()
          WHERE provider=$1 AND channel_id=$2 AND owner_instance_id=$3 AND owner_generation=$4
            AND episode_key=$5 AND baseline_relay_offset=$6 AND open_generation=$7
            AND authority_epoch=$8 AND revoked_at IS NULL",
    )
    .bind(&coordinate.provider)
    .bind(&coordinate.channel_id)
    .bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if revoked != 1 {
        tx.rollback().await?;
        return Ok(FreshVouchRevoke::Stale);
    }
    sqlx::query(
        "UPDATE message_outbox SET status='cancelled',cancelled_at=NOW(),cancel_reason=$9,
                dedupe_key=NULL,dedupe_expires_at=NULL,claimed_at=NULL,claim_owner=NULL,next_attempt_at=NULL
          WHERE status IN ('held','pending') AND circuit_provider=$1 AND circuit_channel_id=$2
            AND circuit_owner_instance_id=$3 AND circuit_owner_generation=$4
            AND circuit_episode_key=$5 AND circuit_baseline_relay_offset=$6
            AND circuit_open_generation=$7 AND circuit_authority_epoch=$8",
    ).bind(&coordinate.provider).bind(&coordinate.channel_id).bind(&coordinate.owner_instance_id)
    .bind(coordinate.owner_generation).bind(&coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset).bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch).bind(reason).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(FreshVouchRevoke::Revoked)
}

/// Cancellation reason stamped on a row fenced off by the worker delivery fence.
pub(crate) const DELIVERY_FENCE_CANCEL_REASON: &str = "circuit_authority_superseded_at_delivery";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryFenceOutcome {
    /// Non-circuit row, or the row's circuit authority is still current: the
    /// lease is retained, `delivery_fence_checked_at` is stamped, and the caller
    /// must proceed with the Discord send.
    Cleared,
    /// The row's circuit episode/authority was superseded (a newer epoch was
    /// reserved) or revoked (`revoked_at`): the row is cancelled, its dedupe
    /// identity released, `delivery_fence_checked_at` stamped, and the caller
    /// must NOT deliver.
    Fenced,
    /// The lease (`claim_owner`/`claimed_at`) no longer matches or the row left
    /// `processing`: a stale worker performed no mutation and must NOT deliver.
    LeaseLost,
}

/// Re-validate a claimed `processing` outbox row's circuit authority in the
/// instant before the worker performs the Discord HTTP send (#4615 S3b — the
/// worker delivery fence that activates S3a's dormant authority columns).
///
/// The fence is lease-guarded exactly like `mark_message_outbox_sent_pg` /
/// `mark_message_outbox_failed_pg`: every mutation requires the row to still be
/// `processing` under the caller's `claim_owner` + `claimed_at`, so a worker
/// whose lease was stolen (stale-claim reclaimed by another owner) neither
/// fences nor stamps the row.
///
/// Non-circuit rows (`circuit_provider IS NULL`) are always cleared and keep
/// their existing lifecycle. A circuit row is fenced when no non-revoked
/// authority row matches its stamped coordinate — this catches the exact gap
/// `revoke_on_fresh_vouch` cannot close, because that revoke only cancels
/// `held`/`pending` rows and a row already escaped to `processing` would
/// otherwise deliver a superseded alert. Both the fence-cancel and the
/// clear-stamp run in one transaction and target disjoint rows.
pub(crate) async fn fence_claimed_delivery(
    pool: &PgPool,
    outbox_id: i64,
    claim_owner: &str,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> Result<DeliveryFenceOutcome, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let fenced = sqlx::query(
        "UPDATE message_outbox
            SET status='cancelled', cancelled_at=NOW(), cancel_reason=$4,
                delivery_fence_checked_at=NOW(), dedupe_key=NULL, dedupe_expires_at=NULL,
                claimed_at=NULL, claim_owner=NULL, next_attempt_at=NULL
          WHERE id=$1 AND claim_owner=$2 AND claimed_at=$3 AND status='processing'
            AND circuit_provider IS NOT NULL
            AND NOT EXISTS (
                SELECT 1 FROM message_outbox_circuit_authority a
                 WHERE a.provider = message_outbox.circuit_provider
                   AND a.channel_id = message_outbox.circuit_channel_id
                   AND a.owner_instance_id = message_outbox.circuit_owner_instance_id
                   AND a.owner_generation = message_outbox.circuit_owner_generation
                   AND a.episode_key = message_outbox.circuit_episode_key
                   AND a.baseline_relay_offset = message_outbox.circuit_baseline_relay_offset
                   AND a.open_generation = message_outbox.circuit_open_generation
                   AND a.authority_epoch = message_outbox.circuit_authority_epoch
                   AND a.revoked_at IS NULL)",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .bind(DELIVERY_FENCE_CANCEL_REASON)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if fenced == 1 {
        tx.commit().await?;
        return Ok(DeliveryFenceOutcome::Fenced);
    }
    let cleared = sqlx::query(
        "UPDATE message_outbox SET delivery_fence_checked_at=NOW()
          WHERE id=$1 AND claim_owner=$2 AND claimed_at=$3 AND status='processing'",
    )
    .bind(outbox_id)
    .bind(claim_owner)
    .bind(claimed_at)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(if cleared == 1 {
        DeliveryFenceOutcome::Cleared
    } else {
        DeliveryFenceOutcome::LeaseLost
    })
}
