use serde_json::Value;
use sqlx::{AssertSqlSafe, Postgres, Transaction};

use super::{FailedPreAcceptSweepOutcome, IntakeOutboxRow, IntakeOutboxStatus};
use crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;

const SOURCE_HOME_STAMP_SQL: &str = "parent.home_epoch IS NULL";

pub(crate) enum Preparation<'a> {
    Early {
        tx: Transaction<'a, Postgres>,
        outcome: FailedPreAcceptSweepOutcome,
    },
    Ready(Box<PreparedSweep<'a>>),
}

// Locked rows and their transaction remain one non-cloneable, consumable value.
pub(crate) struct PreparedSweep<'a> {
    tx: Transaction<'a, Postgres>,
    source: IntakeOutboxRow,
    family_max: i32,
    targets: Vec<Value>,
}

pub(crate) async fn prepare_locked(
    mut tx: Transaction<'_, Postgres>,
    max_attempts_per_message: u32,
    worker_lease_ttl_secs: u64,
    retry_authorization_secs: Option<u64>,
) -> Result<Preparation<'_>, sqlx::Error> {
    let max_attempts = max_attempts_per_message.clamp(1, i32::MAX as u32) as i32;
    let stale_after_secs = worker_lease_ttl_secs.max(1) as i64;
    let retry_after_secs = retry_authorization_secs.map(|value| value.max(1) as i64);
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
        return Ok(Preparation::Early {
            tx,
            outcome: FailedPreAcceptSweepOutcome::Empty,
        });
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
        return Ok(Preparation::Early {
            tx,
            outcome: FailedPreAcceptSweepOutcome::BudgetExhausted {
                source_id: source.id,
                attempt_no: family_max,
            },
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
        let owner_key = crate::domain::intake_identity::OwnerIdentity::new(
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
    Ok(Preparation::Ready(Box::new(PreparedSweep {
        tx,
        source,
        family_max,
        targets,
    })))
}

impl<'a> PreparedSweep<'a> {
    pub(crate) fn source(&self) -> &IntakeOutboxRow {
        &self.source
    }

    pub(crate) fn targets(&self) -> &[Value] {
        &self.targets
    }

    pub(crate) async fn finish_locked(
        self,
        selected: Option<usize>,
    ) -> Result<(Transaction<'a, Postgres>, FailedPreAcceptSweepOutcome), sqlx::Error> {
        let Self {
            mut tx,
            source,
            family_max,
            targets,
        } = self;
        let target = selected
            .map(|index| {
                targets.get(index).ok_or_else(|| {
                    sqlx::Error::Protocol("retry target is outside the locked candidates".into())
                })
            })
            .transpose()?
            .and_then(|node| node["instance_id"].as_str());
        let Some(target) = target else {
            sqlx::query("UPDATE intake_outbox SET updated_at = NOW() WHERE id = $1")
                .bind(source.id)
                .execute(&mut *tx)
                .await?;
            return Ok((
                tx,
                FailedPreAcceptSweepOutcome::NoCapableTarget {
                    source_id: source.id,
                },
            ));
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
        Ok((
            tx,
            FailedPreAcceptSweepOutcome::Retried {
                source_id: source.id,
                new_id,
                attempt_no: next_attempt,
            },
        ))
    }
}
