//! `intake_outbox` table primitives — Phases 1 + 2 of the
//! intake-node-routing design (`docs/design/intake-node-routing.md`).
//!
//! Phase 1 shipped only the schema (migration
//! `0052_intake_node_routing.sql`) and a small test module that verifies
//! the migration applies correctly and the constraints behave as designed.
//!
//! Phase 2 adds the claim / transition / sweep helpers that the leader
//! intake hook (Phase 4) and the worker polling loop (Phase 3) build on.
//! All helpers are pure SQL — no Discord/serenity types, no caches, no
//! global state — so they can be unit-tested with PG fixtures and reused
//! by leader and worker code paths without coupling.

use super::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
use super::intake_outbox_status::IntakeOutboxStatus;
use serde_json::Value;
use sqlx::{AssertSqlSafe, PgPool};

#[cfg(test)]
pub(crate) mod owner_fence_pg_tests;

/// Owned snapshot of an `intake_outbox` row. The `Phase 3` worker poll
/// claims a row, deserializes the payload columns into this struct, then
/// hands it to `services::discord::execute_intake_turn_core` after
/// converting `into_intake_request()`.
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct IntakeOutboxRow {
    pub id: i64,
    pub target_instance_id: String,
    pub forwarded_by_instance_id: String,
    pub required_labels: Value,
    pub execution_requirements: Value,
    pub attachment_refs: Value,
    pub channel_id: String,
    pub user_msg_id: String,
    pub replay_source_message_ids: Option<Vec<String>>,
    pub request_owner_id: String,
    pub request_owner_name: Option<String>,
    pub user_text: String,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: String,
    pub merge_consecutive: bool,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub preserve_on_cancel: Option<bool>,
    pub agent_id: String,
    /// Provider of the bot that forwarded this row (#4349). Carried through
    /// `force_fail_and_retry_as_new` so a retry stays on the same bot.
    pub provider: String,
    pub status: IntakeOutboxStatus,
    // reason: intake-outbox row column consumed by select claim/retry routes,
    // not every compile target. See #3034.
    #[allow(dead_code)]
    pub claim_owner: Option<String>,
    pub attempt_no: i32,
    pub parent_outbox_id: Option<i64>,
    // reason: intake-outbox row column consumed by select claim/retry routes,
    // not every compile target. See #3034.
    #[allow(dead_code)]
    pub retry_count: i32,
    pub owner_generation: Option<i64>,
    pub owner_instance_id: Option<String>,
    pub admission_kind: String,
}

/// Per-message payload required to INSERT a fresh row. Mirrors the
/// `intake_outbox` payload columns (excluding bookkeeping like status,
/// attempt_no, parent_outbox_id which the caller computes separately).
#[derive(Clone, Debug)]
pub(crate) struct InsertPendingPayload {
    pub target_instance_id: String,
    pub forwarded_by_instance_id: String,
    pub required_labels: Value,
    pub execution_requirements: Value,
    pub attachment_refs: Value,
    pub channel_id: String,
    pub user_msg_id: String,
    pub replay_source_message_ids: Option<Vec<String>>,
    pub request_owner_id: String,
    pub request_owner_name: Option<String>,
    pub user_text: String,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: String,
    pub merge_consecutive: bool,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub preserve_on_cancel: bool,
    pub agent_id: String,
    /// Provider of the bot that forwarded this row (#4349). Claim
    /// eligibility is scoped on this, not on `agents.provider`, because a
    /// cc/cdx paired agent has one `agents.provider` but two bots.
    pub provider: String,
}

/// INSERT a fresh `pending` row into `intake_outbox` for the given
/// payload. Caller computes `attempt_no` via `family_max_attempt + 1`
/// (round-3 P0 #1) and passes `parent_outbox_id = Some(parent.id)` when
/// retrying after a `failed_pre_accept`. For the very first attempt of
/// a `(channel_id, user_msg_id)` family, pass `attempt_no = 1` and
/// `parent_outbox_id = None`.
///
/// Returns the row's `id`. May error with SQLSTATE 23505 against either
/// `intake_outbox_unique_message_attempt` (duplicate `(channel_id,
/// user_msg_id, attempt_no)`) or `intake_outbox_one_open_route_per_channel`
/// (another row for the same channel is in an OPEN status). Callers
/// distinguish via `error.constraint_name()` per the design doc §B-bis.
pub(crate) async fn insert_pending(
    pool: &PgPool,
    payload: &InsertPendingPayload,
    attempt_no: i32,
    parent_outbox_id: Option<i64>,
) -> Result<i64, sqlx::Error> {
    let id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, request_owner_name,
            user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
            merge_consecutive, reply_to_user_message, defer_watcher_resume,
            wait_for_completion, preserve_on_cancel, agent_id, provider,
            status, attempt_no, parent_outbox_id, execution_requirements, attachment_refs,
            replay_source_message_ids
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6, $7,
            $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17, $18, $19,
            $20, $21, $22, $23, $24,
            $25
        )
        RETURNING id
        "#,
    )
    .bind(&payload.target_instance_id)
    .bind(&payload.forwarded_by_instance_id)
    .bind(&payload.required_labels)
    .bind(&payload.channel_id)
    .bind(&payload.user_msg_id)
    .bind(&payload.request_owner_id)
    .bind(payload.request_owner_name.as_deref())
    .bind(&payload.user_text)
    .bind(payload.reply_context.as_deref())
    .bind(payload.has_reply_boundary)
    .bind(payload.dm_hint)
    .bind(&payload.turn_kind)
    .bind(payload.merge_consecutive)
    .bind(payload.reply_to_user_message)
    .bind(payload.defer_watcher_resume)
    .bind(payload.wait_for_completion)
    .bind(payload.preserve_on_cancel)
    .bind(&payload.agent_id)
    .bind(&payload.provider)
    .bind(IntakeOutboxStatus::Pending)
    .bind(attempt_no)
    .bind(parent_outbox_id)
    .bind(&payload.execution_requirements)
    .bind(&payload.attachment_refs)
    .bind(payload.replay_source_message_ids.as_deref())
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Discriminates the two SQLSTATE 23505 unique-constraint violations
/// that `insert_pending` can return. Phase 4 (leader hook) uses this
/// to decide whether to:
/// - retry the INSERT with a fresh `attempt_no = family_max + 1` (the
///   3-tuple constraint races a sweep-driven retry), or
/// - fall back to running the turn locally (the partial unique index
///   says another row for this channel is already OPEN).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeInsertConflict {
    /// `intake_outbox_unique_message_attempt`: another row already
    /// exists for `(channel_id, user_msg_id, attempt_no)`. Recompute
    /// `family_max + 1` and retry.
    DuplicateMessageAttempt,
    /// `intake_outbox_one_open_route_per_channel`: another row for the
    /// same `channel_id` is already in an OPEN status. The leader
    /// should fall back to local-route or refuse.
    OpenRoutePerChannel,
}

/// Map a `sqlx::Error` from `insert_pending` to its conflict class.
/// Returns `None` for non-23505 errors (genuine DB failures the caller
/// should surface) and for 23505 errors with an unrecognised constraint
/// name (defensively treated as "not one of ours").
pub(crate) fn classify_insert_pending_error(error: &sqlx::Error) -> Option<IntakeInsertConflict> {
    let db_error = error.as_database_error()?;
    let constraint = db_error.constraint()?;
    match constraint {
        "intake_outbox_unique_message_attempt" => {
            Some(IntakeInsertConflict::DuplicateMessageAttempt)
        }
        "intake_outbox_one_open_route_per_channel" => {
            Some(IntakeInsertConflict::OpenRoutePerChannel)
        }
        _ => None,
    }
}

/// Compute `MAX(attempt_no)` for a `(channel_id, user_msg_id)` family,
/// or 0 if no row exists yet. Used by retry-on-`failed_pre_accept` to
/// allocate `attempt_no = family_max + 1` (round-4 P0 — monotonic).
// reason: intake-outbox retry helper exercised only by the pg-integration test
// suite; production retry path wired on select routes. See #3034.
#[allow(dead_code)]
pub(crate) async fn family_max_attempt(
    pool: &PgPool,
    channel_id: &str,
    user_msg_id: &str,
) -> Result<i32, sqlx::Error> {
    let max_attempt: Option<i32> = sqlx::query_scalar(
        "SELECT MAX(attempt_no) FROM intake_outbox
         WHERE channel_id = $1 AND user_msg_id = $2",
    )
    .bind(channel_id)
    .bind(user_msg_id)
    .fetch_one(pool)
    .await?;
    Ok(max_attempt.unwrap_or(0))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FailedPreAcceptSweepOutcome {
    Empty,
    Retried {
        source_id: i64,
        new_id: i64,
        attempt_no: i32,
    },
    NoCapableTarget {
        source_id: i64,
    },
    BudgetExhausted {
        source_id: i64,
        attempt_no: i32,
    },
}

/// Historical delegated inputs stay terminal; retry never replays their old target.
const SOURCE_HOME_STAMP_SQL: &str = "parent.home_epoch IS NULL";

pub(crate) async fn sweep_failed_pre_accept_once(
    pool: &PgPool,
    local_leader: &str,
    max_attempts_per_message: u32,
    worker_lease_ttl_secs: u64,
    retry_authorization_secs: Option<u64>,
) -> Result<FailedPreAcceptSweepOutcome, sqlx::Error> {
    let max_attempts = max_attempts_per_message.clamp(1, i32::MAX as u32) as i32;
    let stale_after_secs = worker_lease_ttl_secs.max(1) as i64;
    let retry_after_secs = retry_authorization_secs.map(|value| value.max(1) as i64);
    let _ = local_leader;
    let mut tx = pool.begin().await?;
    // LIMITS: both candidate queries retain `failed_pre_accept` literals, as do
    // the pending claim, claimed stale sweep, and accepted SLA predicates below.
    // Each matches a partial-index predicate that must remain a planner constant;
    // a typed parameter can prevent generic-plan implication.
    let capable_source_sql = format!(
        r#"
        SELECT parent.*
          FROM intake_outbox parent
         WHERE parent.status = 'failed_pre_accept'
           AND btrim(parent.provider) <> ''
           AND ($3::BIGINT IS NULL OR parent.completed_at >= NOW() - ($3 * INTERVAL '1 second'))
           AND parent.attempt_no < $2
           AND parent.attempt_no = (
                SELECT MAX(family.attempt_no)
                  FROM intake_outbox family
                 WHERE family.channel_id = parent.channel_id
                   AND family.user_msg_id = parent.user_msg_id
           )
           AND NOT EXISTS (
                SELECT 1 FROM intake_outbox open_row
                 WHERE open_row.channel_id = parent.channel_id
                   AND open_row.status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})
           )
           AND NOT replay_sources_blocked(parent.provider, parent.channel_id, COALESCE(parent.replay_source_message_ids, ARRAY[parent.user_msg_id]))
           AND {SOURCE_HOME_STAMP_SQL}
           AND EXISTS (
                SELECT 1 FROM worker_nodes worker
                 WHERE worker.status = 'online'
                   AND worker.last_heartbeat_at >= NOW() - ($1::BIGINT * INTERVAL '1 second')
                   AND jsonb_typeof(worker.capabilities) = 'object'
                   AND jsonb_typeof(worker.capabilities->'intake_worker') = 'object'
                   AND jsonb_typeof(worker.capabilities->'intake_worker'->'enabled') = 'boolean'
                   AND worker.capabilities->'intake_worker'->'enabled' = 'true'::jsonb
                   AND jsonb_typeof(worker.capabilities->'intake_worker'->'providers') = 'array'
                   AND EXISTS (
                       SELECT 1
                         FROM jsonb_array_elements(worker.capabilities->'intake_worker'->'providers')
                              AS provider_entry(value)
                        WHERE jsonb_typeof(provider_entry.value) = 'string'
                          AND lower(btrim(provider_entry.value #>> '{{}}')) = lower(btrim(parent.provider))
                   )
                   AND (COALESCE(parent.preserve_on_cancel, false) = false OR
                        (jsonb_typeof(worker.capabilities->'intake_worker'->'features') = 'array' AND
                         EXISTS (
                             SELECT 1
                               FROM jsonb_array_elements(worker.capabilities->'intake_worker'->'features')
                                    AS feature_entry(value)
                              WHERE jsonb_typeof(feature_entry.value) = 'string'
                                AND lower(btrim(feature_entry.value #>> '{{}}')) = 'preserve_on_cancel_v1'
                         )))
                   AND jsonb_typeof(parent.required_labels) = 'array'
                   AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements(parent.required_labels) v
                                    WHERE jsonb_typeof(v) <> 'string')
                   AND jsonb_typeof(worker.labels) = 'array'
                   AND worker.labels @> parent.required_labels
                   AND ((parent.admission_kind IN ('local','forwarded')
                         AND parent.owner_instance_id IS NULL AND parent.owner_generation IS NULL)
                        OR (parent.admission_kind = 'forwarded'
                         AND btrim(parent.owner_instance_id) <> '' AND parent.owner_generation >= 0
                         AND worker.instance_id = parent.owner_instance_id
                         AND EXISTS (
                            SELECT 1 FROM intake_session_owners owner
                             WHERE owner.provider = lower(btrim(parent.provider))
                               AND owner.raw_channel_id = btrim(parent.channel_id)
                               AND owner.status = 'active'
                               AND owner.owner_instance_id = parent.owner_instance_id
                               AND owner.generation = parent.owner_generation
                        )))
           )
         ORDER BY parent.updated_at ASC, parent.id ASC
         LIMIT 1
         FOR UPDATE OF parent SKIP LOCKED
        "#
    );
    let capable_source: Option<IntakeOutboxRow> = sqlx::query_as(AssertSqlSafe(capable_source_sql))
        .bind(stale_after_secs)
        .bind(max_attempts)
        .bind(retry_after_secs)
        .fetch_optional(&mut *tx)
        .await?;
    let source = if capable_source.is_some() {
        capable_source
    } else {
        let fallback_source_sql = format!(
            r#"
            SELECT parent.*
              FROM intake_outbox parent
             WHERE parent.status = 'failed_pre_accept'
               AND btrim(parent.provider) <> ''
               AND ($1::BIGINT IS NULL OR parent.completed_at >= NOW() - ($1 * INTERVAL '1 second'))
               AND parent.attempt_no = (
                    SELECT MAX(family.attempt_no)
                      FROM intake_outbox family
                     WHERE family.channel_id = parent.channel_id
                       AND family.user_msg_id = parent.user_msg_id
               )
               AND NOT EXISTS (
                    SELECT 1 FROM intake_outbox open_row
                     WHERE open_row.channel_id = parent.channel_id
                       AND open_row.status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})
               )
               AND NOT replay_sources_blocked(parent.provider, parent.channel_id, COALESCE(parent.replay_source_message_ids, ARRAY[parent.user_msg_id]))
               AND {SOURCE_HOME_STAMP_SQL}
             ORDER BY parent.updated_at ASC, parent.id ASC
             LIMIT 1
             FOR UPDATE OF parent SKIP LOCKED
            "#
        );
        sqlx::query_as(AssertSqlSafe(fallback_source_sql))
            .bind(retry_after_secs)
            .fetch_optional(&mut *tx)
            .await?
    };

    let Some(source) = source else {
        tx.commit().await?;
        return Ok(FailedPreAcceptSweepOutcome::Empty);
    };
    let family_max = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT MAX(attempt_no) FROM intake_outbox
         WHERE channel_id = $1 AND user_msg_id = $2",
    )
    .bind(&source.channel_id)
    .bind(&source.user_msg_id)
    .fetch_one(&mut *tx)
    .await?
    .unwrap_or(0);
    if family_max >= max_attempts {
        tx.commit().await?;
        return Ok(FailedPreAcceptSweepOutcome::BudgetExhausted {
            source_id: source.id,
            attempt_no: family_max,
        });
    }
    let owner_fenced = source.admission_kind == "forwarded"
        && source
            .owner_instance_id
            .as_ref()
            .is_some_and(|id| !id.trim().is_empty())
        && source
            .owner_generation
            .is_some_and(|generation| generation >= 0);
    if owner_fenced {
        let owner_key =
            crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
                &source.provider,
                &source.channel_id,
            )
            .advisory_key();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(owner_key)
            .execute(&mut *tx)
            .await?;
    }

    // Re-check capability after the source lock (a worker may have re-heartbeated).
    let targets: Vec<Value> = sqlx::query_scalar(
        r#"
        SELECT to_jsonb(worker) || jsonb_build_object('api_base_url', worker.capabilities->'agentdesk_api'->>'base_url')
          FROM worker_nodes worker
         WHERE worker.status = 'online'
           AND worker.last_heartbeat_at >= NOW() - ($1::BIGINT * INTERVAL '1 second')
           AND jsonb_typeof(worker.capabilities) = 'object'
           AND jsonb_typeof(worker.capabilities->'intake_worker') = 'object'
           AND jsonb_typeof(worker.capabilities->'intake_worker'->'enabled') = 'boolean'
           AND worker.capabilities->'intake_worker'->'enabled' = 'true'::jsonb
           AND jsonb_typeof(worker.capabilities->'intake_worker'->'providers') = 'array'
           AND EXISTS (
               SELECT 1
                 FROM jsonb_array_elements(worker.capabilities->'intake_worker'->'providers')
                      AS provider_entry(value)
                WHERE jsonb_typeof(provider_entry.value) = 'string'
                  AND lower(btrim(provider_entry.value #>> '{}')) = lower(btrim($2))
           )
           AND (
               COALESCE($3, false) = false
               OR (
                   jsonb_typeof(worker.capabilities->'intake_worker'->'features') = 'array'
                   AND EXISTS (
                       SELECT 1
                         FROM jsonb_array_elements(worker.capabilities->'intake_worker'->'features')
                              AS feature_entry(value)
                        WHERE jsonb_typeof(feature_entry.value) = 'string'
                          AND lower(btrim(feature_entry.value #>> '{}')) = 'preserve_on_cancel_v1'
                   )
               )
           )
           AND jsonb_typeof($8::JSONB) = 'array'
           AND NOT EXISTS (SELECT 1 FROM jsonb_array_elements($8::JSONB) v
                            WHERE jsonb_typeof(v) <> 'string')
           AND jsonb_typeof(worker.labels) = 'array'
           AND worker.labels @> $8::JSONB
           AND ((NOT $4 AND $5 IS NULL AND $6 IS NULL) OR ($4
                AND worker.instance_id = $5
                AND EXISTS (
                    SELECT 1 FROM intake_session_owners owner
                     WHERE owner.provider = lower(btrim($2))
                       AND owner.raw_channel_id = btrim($7)
                       AND owner.status = 'active'
                       AND owner.owner_instance_id = $5
                       AND owner.generation = $6
                )))
         ORDER BY worker.last_heartbeat_at DESC, worker.instance_id ASC
         FOR SHARE
        "#,
    )
    .bind(stale_after_secs)
    .bind(&source.provider)
    .bind(source.preserve_on_cancel)
    .bind(owner_fenced)
    .bind(source.owner_instance_id.as_deref())
    .bind(source.owner_generation)
    .bind(&source.channel_id)
    .bind(&source.required_labels)
    .fetch_all(&mut *tx)
    .await?;
    let requirements =
        crate::services::cluster::execution_requirements::ExecutionRequirements::parse(
            source.execution_requirements.clone(),
        )
        .map_err(sqlx::Error::Protocol)?;
    let auth_profile = crate::services::cluster::readiness::expected_auth_profile(
        &source.provider,
        &source.channel_id,
        &source.agent_id,
    );
    let target = targets
        .iter()
        .find(|node| {
            requirements.intake_reasons(node).is_empty()
                && (source.attachment_refs.as_array().is_some_and(Vec::is_empty)
                    || crate::services::cluster::attachment_transfer::supports(node))
                && crate::services::cluster::readiness::evaluate_declared(
                    node,
                    &source.provider,
                    &auth_profile,
                )
                .eligible
        })
        .and_then(|node| node["instance_id"].as_str());
    let Some(target) = target else {
        sqlx::query("UPDATE intake_outbox SET updated_at = NOW() WHERE id = $1")
            .bind(source.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(FailedPreAcceptSweepOutcome::NoCapableTarget {
            source_id: source.id,
        });
    };

    let next_attempt = family_max + 1;
    let new_id: i64 = sqlx::query_scalar(
        r#"INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, request_owner_name,
            user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
            merge_consecutive, reply_to_user_message, defer_watcher_resume,
            wait_for_completion, preserve_on_cancel, agent_id, provider,
            owner_instance_id, owner_generation, admission_kind,
            status, attempt_no, parent_outbox_id, execution_requirements, attachment_refs,
            replay_source_message_ids
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                  $13, $14, $15, $16, $17, $18, $19, $20, $21, $22,
                  $23, $24, $25, $26, $27, $28)
        RETURNING id"#,
    )
    .bind(&target)
    .bind(&source.forwarded_by_instance_id)
    .bind(&source.required_labels)
    .bind(&source.channel_id)
    .bind(&source.user_msg_id)
    .bind(&source.request_owner_id)
    .bind(source.request_owner_name.as_deref())
    .bind(&source.user_text)
    .bind(source.reply_context.as_deref())
    .bind(source.has_reply_boundary)
    .bind(source.dm_hint)
    .bind(&source.turn_kind)
    .bind(source.merge_consecutive)
    .bind(source.reply_to_user_message)
    .bind(source.defer_watcher_resume)
    .bind(source.wait_for_completion)
    .bind(source.preserve_on_cancel)
    .bind(&source.agent_id)
    .bind(&source.provider)
    .bind(source.owner_instance_id.as_deref())
    .bind(source.owner_generation)
    .bind(&source.admission_kind)
    .bind(IntakeOutboxStatus::Pending)
    .bind(next_attempt)
    .bind(source.id)
    .bind(&source.execution_requirements)
    .bind(&source.attachment_refs)
    .bind(source.replay_source_message_ids.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(FailedPreAcceptSweepOutcome::Retried {
        source_id: source.id,
        new_id,
        attempt_no: next_attempt,
    })
}
pub(crate) mod consent_handoff;
mod worker_status;
#[cfg(test)]
use worker_status::confirm_claim;
#[cfg(test)]
pub(crate) use worker_status::{claim_pending_for_target, sweep_stale_pre_accept_claims};
pub(crate) use worker_status::{
    claim_pending_for_target_except, mark_accepted, mark_done, mark_failed_post_accept,
    mark_failed_pre_accept, mark_spawned, return_claimed_to_pending, sweep_stamped_claims,
};

/// Operator-driven status query. Returns the most recent `limit` rows
/// (or rows for a specific channel when `channel_id_filter` is Some),
/// ordered by `created_at DESC`. Phase 5 ops CLI uses this for
/// `agentdesk intake-outbox status`.
pub(crate) async fn list_recent_rows(
    pool: &PgPool,
    channel_id_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<IntakeOutboxRow>, sqlx::Error> {
    let limit = limit.max(1);
    if let Some(channel) = channel_id_filter {
        sqlx::query_as(
            "SELECT * FROM intake_outbox
             WHERE channel_id = $1
             ORDER BY created_at DESC
             LIMIT $2",
        )
        .bind(channel)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query_as(
            "SELECT * FROM intake_outbox
             ORDER BY created_at DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
    }
}
/// Whether the channel still has an intake row in an open status; that row owes Legacy execution.
#[cfg(test)]
pub(crate) async fn channel_has_open_row(
    pool: &PgPool,
    channel_id: &str,
) -> Result<bool, sqlx::Error> {
    let sql = format!(
        "SELECT EXISTS (SELECT 1 FROM intake_outbox
                         WHERE channel_id = $1 AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL}))"
    );
    sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(channel_id)
        .fetch_one(pool)
        .await
}

/// Leader sweep that returns rows currently in `accepted` longer than
/// `sla_secs` without reaching `spawned`. Round-3 P1 #3: the operator
/// alert IS the recovery signal — auto-retry forbidden post-accept.
/// Returns `(id, accepted_at)` tuples for each row exceeding the SLA.
// reason: SLA-alert maintenance helper exercised by the pg-integration test
// suite; production alert path wired on the leader sweep. See #3034.
#[allow(dead_code)]
pub(crate) async fn list_accepted_unspawned_sla(
    pool: &PgPool,
    sla_secs: i64,
) -> Result<Vec<(i64, chrono::DateTime<chrono::Utc>)>, sqlx::Error> {
    let rows: Vec<(i64, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT id, accepted_at FROM intake_outbox
         WHERE status = 'accepted'
           AND accepted_at IS NOT NULL
           AND accepted_at < NOW() - ($1::BIGINT * INTERVAL '1 second')
         ORDER BY accepted_at ASC",
    )
    .bind(sla_secs.max(1))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[cfg(test)]
mod migration_pg_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::intake_outbox_force_fail::{ForceFailError, force_fail_and_retry_as_new};
    use crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
    use crate::db::intake_outbox_status::{IntakeOutboxStatus, OperatorRetryClass};
    use serde_json::json;
    use sqlx::{AssertSqlSafe, Row};

    /// Migration 0080 verbatim. Every statement is idempotent — the DDL is
    /// `IF [NOT] EXISTS` and each backfill/fail-closed UPDATE is gated on
    /// `provider = ''` — so replaying it over synthetic pre-#4349 rows exercises
    /// exactly what a real deploy runs.
    const MIGRATION_0080: &str =
        include_str!("../../migrations/postgres/0080_intake_outbox_provider.sql");

    /// Executed as one script over the simple query protocol, exactly how
    /// `sqlx::migrate` runs it in production. Splitting on `;` would break on
    /// semicolons inside comments and string literals.
    async fn replay_migration_0080(pool: &sqlx::PgPool) {
        sqlx::raw_sql(MIGRATION_0080)
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("replay 0080 failed: {e}")); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    }

    /// A pre-#4349 row: `provider = ''`, which is what the column default leaves
    /// behind for rows written before the migration ran.
    async fn seed_legacy_row(
        pool: &sqlx::PgPool,
        channel: &str,
        status: &str,
        claim_owner: Option<&str>,
    ) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id,
                channel_id, user_msg_id, request_owner_id, user_text, turn_kind,
                agent_id, provider, status, claim_owner
             ) VALUES (
                'worker-1', 'leader-1',
                $1, $1, '100', 'hi', 'foreground',
                'agent-paired', '', $2, $3
             ) RETURNING id",
        )
        .bind(channel)
        .bind(status)
        .bind(claim_owner)
        .fetch_one(pool)
        .await
        .expect("seed legacy row") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    }

    async fn row_state(pool: &sqlx::PgPool, id: i64) -> (String, String) {
        sqlx::query_as("SELECT provider, status FROM intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read row state") // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
    }

    /// #4349 review round 1 (REJECT): an earlier draft failed *every* open row to
    /// `failed_pre_accept`. `accepted`/`spawned` are POST-accept — the turn may
    /// already have emitted to Discord, and auto-retry is forbidden past
    /// `accepted` (intake_worker.rs: "a failure is post-accept and is NOT
    /// auto-retried"). Labelling them pre-accept misclassifies an orphaned turn
    /// as retryable.
    ///
    /// The migration must instead recover `provider` from `claim_owner`
    /// ("{instance_id}:{provider}") for every row a worker ever held, and
    /// fail-close only what is genuinely unrecoverable — pre-accept rows to
    /// `failed_pre_accept`, post-accept rows to `failed_post_accept`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn migration_0080_recovers_provider_from_claim_owner_and_fails_closed_by_accept_phase() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_cc)
             VALUES ('agent-paired', 'Paired', 'codex', 'ch-cdx-mig', 'ch-cc-mig')",
        )
        .execute(&pool)
        .await
        .expect("seed paired agent"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

        // Worker-held rows. `claim_owner` names the real forwarding bot, and for
        // the claude ones it disagrees with `agents.provider` ('codex') — which
        // is precisely the #4349 bug.
        let claimed = seed_legacy_row(&pool, "ch-a", "claimed", Some("worker-1:claude")).await;
        let accepted = seed_legacy_row(&pool, "ch-b", "accepted", Some("worker-1:claude")).await;
        let spawned = seed_legacy_row(&pool, "ch-c", "spawned", Some("worker-1:codex")).await;
        let done = seed_legacy_row(&pool, "ch-d", "done", Some("worker-1:claude")).await;

        // `pending` never carries a claim_owner (the sweep nulls it on reset).
        let pending = seed_legacy_row(&pool, "ch-e", "pending", None).await;
        // Terminal but never claimed — falls back to the agent's provider.
        let dead_terminal = seed_legacy_row(&pool, "ch-f", "failed_pre_accept", None).await;
        // Defensive arm: post-accept with an unusable claim_owner.
        let orphan_post = seed_legacy_row(&pool, "ch-g", "spawned", Some("malformed")).await;

        replay_migration_0080(&pool).await;

        // Recovered exactly; status untouched.
        assert_eq!(
            row_state(&pool, claimed).await,
            ("claude".to_string(), "claimed".to_string())
        );
        assert_eq!(
            row_state(&pool, accepted).await,
            ("claude".to_string(), "accepted".to_string())
        );
        assert_eq!(
            row_state(&pool, spawned).await,
            ("codex".to_string(), "spawned".to_string())
        );
        assert_eq!(
            row_state(&pool, done).await,
            ("claude".to_string(), "done".to_string())
        );

        // Unrecoverable + pre-accept → retryable terminal.
        assert_eq!(
            row_state(&pool, pending).await,
            ("".to_string(), "failed_pre_accept".to_string()),
            "pending is pre-accept; failing it closed must stay retryable"
        );

        // Terminal, never claimed → the agent's provider is the honest record.
        assert_eq!(
            row_state(&pool, dead_terminal).await,
            ("codex".to_string(), "failed_pre_accept".to_string())
        );

        // Unrecoverable + post-accept → operator-only, NEVER pre-accept.
        let (orphan_provider, orphan_status) = row_state(&pool, orphan_post).await;
        assert_eq!(orphan_provider, "");
        assert_eq!(
            orphan_status, "failed_post_accept",
            "post-accept row must not be misclassified as retryable pre-accept"
        );
        // It lands in a state force_fail_and_retry_as_new would otherwise accept…
        assert_eq!(
            IntakeOutboxStatus::FailedPostAccept.operator_retry(),
            OperatorRetryClass::AlreadyTerminal
        );

        // …but retrying it would insert `pending` work with provider='' that no
        // worker can claim. #4349 review r2: refuse loudly, change nothing.
        let err = force_fail_and_retry_as_new(&pool, orphan_post, "operator: retry")
            .await
            .expect_err("orphan row with no provider must be refused");
        match err {
            ForceFailError::UnknownProvider { id } => assert_eq!(id, orphan_post),
            other => panic!("expected UnknownProvider, got {other:?}"), // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        }

        // The refusal is total: no child row, original row untouched.
        let family: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-g'",
        )
        .fetch_one(&pool)
        .await
        .expect("count family"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        assert_eq!(family, 1, "refusal must not insert a retry attempt");
        assert_eq!(
            row_state(&pool, orphan_post).await,
            ("".to_string(), "failed_post_accept".to_string())
        );

        // No pending row anywhere can carry an unclaimable empty provider.
        let unclaimable_sql = format!(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox
             WHERE provider = '' AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
        );
        let unclaimable: i64 = sqlx::query_scalar(AssertSqlSafe(unclaimable_sql))
            .fetch_one(&pool)
            .await
            .expect("count unclaimable open rows"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        assert_eq!(
            unclaimable, 0,
            "migration must leave no open row that a worker can never claim"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// The migration must add the new `agents.preferred_intake_node_labels`
    /// column with a default of `'[]'::JSONB`. Existing agent reads must
    /// continue to work — verified by inserting a row without referencing
    /// the column and then reading the default back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn agents_preferred_intake_node_labels_defaults_to_empty_array() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-intake-default', 'Test', 'claude', '111')",
        )
        .execute(&pool)
        .await
        .expect("seed agent");

        let value: serde_json::Value = sqlx::query_scalar(
            "SELECT preferred_intake_node_labels FROM agents WHERE id = 'agent-intake-default'",
        )
        .fetch_one(&pool)
        .await
        .expect("read column");
        assert_eq!(value, json!([]));

        pool.close().await;
        pg_db.drop().await;
    }

    /// CHECK constraint must reject an unrecognized status value. This guard
    /// distinguishes spelling mistakes from the official terminal `unknown`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_status_check_rejects_unrecognized_value() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let result = insert_minimal_row(&pool, "ch", "msg", 1, "running").await;
        let error = result.expect_err("status='running' must be rejected");
        let message = error.to_string();
        assert!(
            message.contains("intake_outbox_status_check"),
            "expected status CHECK violation, got: {message}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// `attempt_no` must be at least 1 (covers the underflow case where
    /// a follow-up retry helper computes `MAX - 1` by mistake).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_attempt_no_check_rejects_zero() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let result = insert_minimal_row(&pool, "ch", "msg", 0, "pending").await;
        let error = result.expect_err("attempt_no=0 must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_attempt_no_positive"),
            "expected attempt_no CHECK violation, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two rows in the same `(channel_id, user_msg_id)` family with the
    /// same `attempt_no` must violate the named 3-tuple constraint. This
    /// is the constraint name Rust callers of `retry-local` /
    /// `retry-as-new` match against to decide whether to recompute
    /// `family_max + 1` and retry the INSERT.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_unique_message_attempt_blocks_duplicate_attempt_no() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Drive the row terminal so the partial unique index does not
        // mask the 3-tuple violation.
        insert_minimal_row(&pool, "ch-attempt", "msg-1", 1, "done")
            .await
            .expect("first attempt insert");

        let result = insert_minimal_row(&pool, "ch-attempt", "msg-1", 1, "done").await;
        let error = result.expect_err("duplicate attempt_no must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_unique_message_attempt"),
            "expected 3-tuple constraint violation, got: {error}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// At most ONE row per channel may exist in any OPEN status. The
    /// partial unique index name is the discriminator the Rust handler
    /// matches against to decide whether to fall back to `Local`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_one_open_route_per_channel_blocks_second_open_row() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        insert_minimal_row(&pool, "ch-open", "msg-A", 1, "pending")
            .await
            .expect("first open row");

        let result = insert_minimal_row(&pool, "ch-open", "msg-B", 1, "pending").await;
        let error = result.expect_err("second OPEN row must be rejected");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_one_open_route_per_channel"),
            "expected partial-unique-index violation, got: {error}"
        );

        // After the first row terminates, a fresh OPEN row for a
        // different user_msg_id is allowed.
        sqlx::query(
            "UPDATE intake_outbox SET status='done', completed_at=NOW()
             WHERE channel_id='ch-open' AND user_msg_id='msg-A' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("transition to done");

        insert_minimal_row(&pool, "ch-open", "msg-B", 1, "pending")
            .await
            .expect("fresh OPEN row after parent terminal");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e1_channel_open_row_probe_sees_only_open_statuses_of_that_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        assert!(!super::channel_has_open_row(&pool, "ch-e1").await.unwrap());
        insert_minimal_row(&pool, "ch-e1", "msg-A", 1, "pending")
            .await
            .expect("open row");
        assert!(super::channel_has_open_row(&pool, "ch-e1").await.unwrap());
        assert!(
            !super::channel_has_open_row(&pool, "ch-e1-other")
                .await
                .unwrap()
        );
        sqlx::query(
            "UPDATE intake_outbox SET status='done', completed_at=NOW()
             WHERE channel_id='ch-e1' AND user_msg_id='msg-A'",
        )
        .execute(&pool)
        .await
        .expect("transition to done");
        assert!(!super::channel_has_open_row(&pool, "ch-e1").await.unwrap());
        pool.close().await;
        pg_db.drop().await;
    }

    /// T2-M is schema-only: before any writer produces `dispatched`, widening
    /// the open-status predicate must select exactly the same rows as the
    /// pre-T2-M predicate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_dispatched_extension_is_dormant_without_dispatched_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        for (offset, status) in [
            "pending",
            "claimed",
            "accepted",
            "spawned",
            "done",
            "failed_pre_accept",
            "failed_post_accept",
        ]
        .into_iter()
        .enumerate()
        {
            insert_minimal_row(
                &pool,
                &format!("ch-dormant-{offset}"),
                &format!("msg-dormant-{offset}"),
                1,
                status,
            )
            .await
            .expect("seed pre-dispatched lifecycle row"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        }

        let dispatched_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE status = 'dispatched'",
        )
        .fetch_one(&pool)
        .await
        .expect("count dispatched rows"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        assert_eq!(
            dispatched_rows, 0,
            "fixture must prove dormancy without manufacturing dispatched behavior"
        );

        let prior_statuses = ["pending", "claimed", "accepted", "spawned"]
            .map(|status| format!("'{status}'"))
            .join(", ");
        let prior_sql = format!(
            "SELECT id FROM intake_outbox
             WHERE status IN ({prior_statuses})
             ORDER BY id"
        );
        let old_ids: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(prior_sql))
            .fetch_all(&pool)
            .await
            .expect("select pre-T2-M open rows"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        let widened_sql = format!(
            "SELECT id FROM intake_outbox
             WHERE status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})
             ORDER BY id"
        );
        let widened_ids: Vec<i64> = sqlx::query_scalar(AssertSqlSafe(widened_sql))
            .fetch_all(&pool)
            .await
            .expect("select T2-M open rows"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        assert_eq!(
            widened_ids, old_ids,
            "adding an unwritten dispatched status must leave query results unchanged"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// S-R1 installs the official terminal `unknown` state, dispatch clock
    /// invariant, and both reconciler-supporting indexes without changing the
    /// five-state open-route predicate.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_sr1_schema_enforces_unknown_clock_and_indexes_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let missing_clock =
            insert_minimal_row(&pool, "ch-dispatched", "msg-no-clock", 1, "dispatched")
                .await
                .expect_err("dispatched without dispatched_at must fail closed");
        assert!(
            missing_clock
                .to_string()
                .contains("intake_outbox_dispatched_requires_clock"),
            "expected named dispatch-clock CHECK violation, got: {missing_clock}"
        );

        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, request_owner_name,
                user_text, turn_kind, agent_id, status, attempt_no, dispatched_at
             ) VALUES (
                'worker-1', 'leader-1', '[]'::JSONB,
                'ch-dispatched', 'msg-clock', 'user-1', 'Tester',
                'hello', 'standard', 'agent-x', 'dispatched', 1, NOW()
             )",
        )
        .execute(&pool)
        .await
        .expect("dispatched with a clock must satisfy both CHECKs"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        let error = insert_minimal_row(&pool, "ch-dispatched", "msg-B", 1, "pending")
            .await
            .expect_err("clocked dispatched must retain the channel open-route fence");
        assert!(
            error
                .to_string()
                .contains("intake_outbox_one_open_route_per_channel"),
            "expected named open-route index violation after dispatched row, got: {error}"
        );

        insert_minimal_row(&pool, "ch-unknown", "msg-unknown", 1, "unknown")
            .await
            .expect("official unknown must satisfy intake_outbox_status_check"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module
        let decoded: IntakeOutboxStatus = sqlx::query_scalar(
            "SELECT status FROM intake_outbox
             WHERE channel_id='ch-unknown' AND user_msg_id='msg-unknown'",
        )
        .fetch_one(&pool)
        .await
        .expect("decode official unknown through the typed SQLx codec"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
        assert_eq!(decoded, IntakeOutboxStatus::Unknown);
        assert_eq!(decoded.to_string(), "unknown");
        assert!(
            !decoded.is_open(),
            "official unknown must release its route"
        );
        insert_minimal_row(&pool, "ch-unknown", "msg-after-unknown", 1, "pending")
            .await
            .expect("a same-channel pending row is allowed after terminal unknown"); // agentdesk-audit: allow-unwrap — test helper/assert in #[cfg(test)] module

        let future = insert_minimal_row(&pool, "ch-future", "msg-future", 1, "future_status")
            .await
            .expect_err("unregistered future_status must remain rejected");
        assert!(
            future.to_string().contains("intake_outbox_status_check"),
            "expected named status CHECK violation for future_status, got: {future}"
        );

        for constraint in [
            "intake_outbox_status_check",
            "intake_outbox_dispatched_requires_clock",
        ] {
            let convalidated: bool = sqlx::query_scalar(
                "SELECT convalidated FROM pg_constraint
                 WHERE conrelid = 'intake_outbox'::regclass AND conname = $1",
            )
            .bind(constraint)
            .fetch_one(&pool)
            .await
            .expect("read S-R1 CHECK validation state"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
            assert!(!convalidated, "{constraint} must remain NOT VALID in S-R1");
        }
        for (index, table, column_or_expression, predicate) in [
            (
                "idx_intake_outbox_stale_dispatched",
                "intake_outbox",
                "dispatched_at",
                "(status = 'dispatched'::text)",
            ),
            (
                "idx_delivery_journal_intake_binding",
                "delivery_journal_events",
                "(canonical_payload ->> 'intake_outbox_id'::text)",
                "(event_kind = 'O'::text)",
            ),
        ] {
            let catalog: (String, String, String, bool, i16, i16, String, String) = sqlx::query_as(
                "SELECT index_ns.nspname, table_ns.nspname, t.relname, i.indisvalid,
                        i.indnkeyatts, i.indnatts,
                        pg_get_indexdef(i.indexrelid, 1, true),
                        pg_get_expr(i.indpred, i.indrelid)
                   FROM pg_index i
                   JOIN pg_class c ON c.oid = i.indexrelid
                   JOIN pg_class t ON t.oid = i.indrelid
                   JOIN pg_namespace index_ns ON index_ns.oid = c.relnamespace
                   JOIN pg_namespace table_ns ON table_ns.oid = t.relnamespace
                  WHERE index_ns.nspname = 'public' AND c.relname = $1",
            )
            .bind(index)
            .fetch_one(&pool)
            .await
            .expect("read S-R1 concurrent index catalog contract"); // agentdesk-audit: allow-unwrap — test assertion in #[cfg(test)] module
            assert_eq!(catalog.0, "public", "{index} index must live in public");
            assert_eq!(catalog.1, "public", "{index} table must live in public");
            assert_eq!(catalog.2, table, "{index} must bind the intended table");
            assert!(catalog.3, "fresh migration must build valid index {index}");
            assert_eq!(catalog.4, 1, "{index} must have exactly one key attribute");
            assert_eq!(catalog.5, 1, "{index} must not have INCLUDE attributes");
            assert_eq!(
                catalog.6, column_or_expression,
                "{index} must retain its exact indexed expression"
            );
            assert_eq!(
                catalog.7, predicate,
                "{index} must retain its exact partial predicate"
            );
        }

        pool.close().await;
        pg_db.drop().await;
    }

    /// The BEFORE-UPDATE trigger must keep `updated_at` fresh on every
    /// state transition without callers having to set it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_touch_updated_at_trigger_advances_on_update() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        insert_minimal_row(&pool, "ch-trig", "msg-T", 1, "pending")
            .await
            .expect("seed row");

        let row = sqlx::query(
            "SELECT created_at, updated_at FROM intake_outbox
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("load timestamps");
        let initial_updated_at: chrono::DateTime<chrono::Utc> = row
            .try_get("updated_at")
            .expect("decode initial updated_at");

        // Sleep just enough for NOW() to advance reliably.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        sqlx::query(
            "UPDATE intake_outbox SET last_error='probe'
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("update last_error");

        let new_updated_at: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
            "SELECT updated_at FROM intake_outbox
             WHERE channel_id='ch-trig' AND user_msg_id='msg-T' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("read new updated_at");

        assert!(
            new_updated_at > initial_updated_at,
            "updated_at must advance on UPDATE: was {initial_updated_at}, now {new_updated_at}"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// `parent_outbox_id REFERENCES intake_outbox(id) ON DELETE SET NULL`
    /// is design-required for retention safety: when a future cleanup job
    /// prunes ancestor rows, the child's audit-chain pointer becomes NULL
    /// rather than cascading the delete or leaving a dangling FK. Verify
    /// the constraint actually behaves that way.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_parent_on_delete_set_null_preserves_child() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Seed a terminal parent. attempt_no = 1.
        insert_minimal_row(&pool, "ch-cascade", "msg-X", 1, "failed_pre_accept")
            .await
            .expect("seed parent");
        let parent_id: i64 = sqlx::query_scalar(
            "SELECT id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=1",
        )
        .fetch_one(&pool)
        .await
        .expect("load parent id");

        // Seed a child attempt_no = 2 referencing the parent. Use a raw
        // INSERT that includes parent_outbox_id (the helper does not).
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, request_owner_name,
                user_text, turn_kind, agent_id,
                status, attempt_no, parent_outbox_id
             ) VALUES (
                'leader-1', 'leader-1', '[]'::JSONB,
                'ch-cascade', 'msg-X', 'user-1', 'Tester',
                'hello', 'standard', 'agent-x',
                'done', 2, $1
             )",
        )
        .bind(parent_id)
        .execute(&pool)
        .await
        .expect("seed child");

        // Sanity: child references the parent.
        let pre_delete: Option<i64> = sqlx::query_scalar(
            "SELECT parent_outbox_id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("read parent ref");
        assert_eq!(pre_delete, Some(parent_id));

        // Delete the parent row directly. The child must remain (no
        // cascade) but its parent_outbox_id must be NULLed.
        let deleted = sqlx::query("DELETE FROM intake_outbox WHERE id = $1")
            .bind(parent_id)
            .execute(&pool)
            .await
            .expect("delete parent")
            .rows_affected();
        assert_eq!(deleted, 1, "parent must delete cleanly");

        let child_after: Option<i64> = sqlx::query_scalar(
            "SELECT parent_outbox_id FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("read child after parent delete");
        assert_eq!(
            child_after, None,
            "child's parent_outbox_id must be NULLed by ON DELETE SET NULL"
        );

        let child_id_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox
             WHERE channel_id='ch-cascade' AND user_msg_id='msg-X' AND attempt_no=2",
        )
        .fetch_one(&pool)
        .await
        .expect("count child");
        assert_eq!(child_id_count, 1, "child row must NOT cascade-delete");

        pool.close().await;
        pg_db.drop().await;
    }

    async fn insert_minimal_row(
        pool: &sqlx::PgPool,
        channel_id: &str,
        user_msg_id: &str,
        attempt_no: i32,
        status: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, request_owner_name,
                user_text, turn_kind, agent_id,
                status, attempt_no
             ) VALUES (
                'worker-1', 'leader-1', '[]'::JSONB,
                $1, $2, 'user-1', 'Tester',
                'hello', 'standard', 'agent-x',
                $3, $4
             )",
        )
        .bind(channel_id)
        .bind(user_msg_id)
        .bind(status)
        .bind(attempt_no)
        .execute(pool)
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod postgres_tests;
