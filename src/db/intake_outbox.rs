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

use serde_json::Value;
use sqlx::PgPool;

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
    pub channel_id: String,
    pub user_msg_id: String,
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
    pub agent_id: String,
    pub status: String,
    pub claim_owner: Option<String>,
    pub attempt_no: i32,
    pub parent_outbox_id: Option<i64>,
    pub retry_count: i32,
}

/// Per-message payload required to INSERT a fresh row. Mirrors the
/// `intake_outbox` payload columns (excluding bookkeeping like status,
/// attempt_no, parent_outbox_id which the caller computes separately).
#[derive(Clone, Debug)]
pub(crate) struct InsertPendingPayload {
    pub target_instance_id: String,
    pub forwarded_by_instance_id: String,
    pub required_labels: Value,
    pub channel_id: String,
    pub user_msg_id: String,
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
    pub agent_id: String,
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
            wait_for_completion, agent_id,
            status, attempt_no, parent_outbox_id
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6, $7,
            $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17,
            'pending', $18, $19
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
    .bind(&payload.agent_id)
    .bind(attempt_no)
    .bind(parent_outbox_id)
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

/// Worker-side claim. Atomically promotes a single `pending` row owned
/// by `target_instance_id` whose `agent_id` belongs to a `provider`-
/// matching agent into `claimed`, and stamps `claim_owner` +
/// `claimed_at`. Uses `FOR UPDATE SKIP LOCKED` so concurrent worker
/// pollers do not stall each other.
///
/// **Provider filter (codex Phase 5 P0 #1):** a single AgentDesk
/// process can host multiple bot tokens (claude, codex, etc.), each
/// with its own `run_bot` invocation and its own `Arc<Http>` +
/// `SharedData`. Without this filter, every bot's worker would share
/// the same `target_instance_id` and could claim a row destined for
/// another provider's runtime — running it with the wrong token,
/// settings, mailboxes, and placeholder controller. The JOIN on
/// `agents.provider` makes claim eligibility provider-scoped so each
/// worker only handles rows that belong to its own bot.
///
/// Returns `Ok(Some(row))` on a successful claim; `Ok(None)` when the
/// queue has no eligible row for this `(target_instance_id, provider)`
/// pair.
pub(crate) async fn claim_pending_for_target(
    pool: &PgPool,
    target_instance_id: &str,
    provider: &str,
    claim_owner: &str,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let candidate: Option<i64> = sqlx::query_scalar(
        "SELECT io.id FROM intake_outbox io
         INNER JOIN agents a ON a.id = io.agent_id
         WHERE io.target_instance_id = $1
           AND io.status = 'pending'
           AND a.provider = $2
         ORDER BY io.created_at ASC
         LIMIT 1
         FOR UPDATE OF io SKIP LOCKED",
    )
    .bind(target_instance_id)
    .bind(provider)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(id) = candidate else {
        tx.commit().await?;
        return Ok(None);
    };

    let row: IntakeOutboxRow = sqlx::query_as(
        "UPDATE intake_outbox
         SET status = 'claimed',
             claim_owner = $2,
             claimed_at = NOW()
         WHERE id = $1
         RETURNING *",
    )
    .bind(id)
    .bind(claim_owner)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Some(row))
}

/// Transition `claimed → accepted` after the worker has validated cwd
/// and is ready to spawn the turn. Verifies `claim_owner` matches via
/// the WHERE clause so a stale leader-side sweep cannot accidentally
/// promote someone else's claim.
///
/// Returns `Ok(true)` when the row was updated; `Ok(false)` when the
/// row was no longer in `claimed` (e.g., a stale-claim sweep beat the
/// worker to it, or the claim_owner no longer matches). Workers MUST
/// abort the turn on `Ok(false)` rather than spawning — proceeding past
/// a lost claim is the only path that double-emits a Discord turn.
pub(crate) async fn mark_accepted(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'accepted', accepted_at = NOW()
         WHERE id = $1 AND status = 'claimed' AND claim_owner = $2",
    )
    .bind(id)
    .bind(claim_owner)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Transition `accepted → spawned` once the worker actually begins the
/// turn. Round-3 P1 #3: the worker MUST advance to spawned promptly so
/// the leader's `accepted_unspawned_sla` index does not flag a stuck row.
///
/// Returns `Ok(true)` when the row was updated; `Ok(false)` if the row
/// was no longer in `accepted` (e.g., operator force-failed via
/// transition 12).
pub(crate) async fn mark_spawned(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'spawned', spawned_at = NOW()
         WHERE id = $1 AND status = 'accepted' AND claim_owner = $2",
    )
    .bind(id)
    .bind(claim_owner)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Successful completion: `spawned → done`. Worker calls this on
/// `Ok(())` from `execute_intake_turn_core`.
///
/// Returns `Ok(true)` on a real transition; `Ok(false)` if the row was
/// no longer in `spawned` (e.g., already moved to `failed_post_accept`
/// by an operator). Workers should log the divergence rather than
/// retry on `Ok(false)`.
pub(crate) async fn mark_done(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'done', completed_at = NOW()
         WHERE id = $1 AND status = 'spawned' AND claim_owner = $2",
    )
    .bind(id)
    .bind(claim_owner)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Pre-accept failure: `claimed → failed_pre_accept`. Bumps
/// `retry_count` + records `last_error`. Pre-accept failures are
/// retryable: the leader's failed-pre-accept sweep INSERTs a fresh row
/// with `attempt_no = family_max + 1` and `parent_outbox_id = id`
/// (transition 10).
///
/// Returns `Ok(true)` on a real transition; `Ok(false)` when the row
/// is no longer in `claimed` (sweep already reset it).
pub(crate) async fn mark_failed_pre_accept(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
    error_message: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'failed_pre_accept',
             completed_at = NOW(),
             last_error = $3,
             retry_count = retry_count + 1
         WHERE id = $1 AND status = 'claimed' AND claim_owner = $2",
    )
    .bind(id)
    .bind(claim_owner)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Post-accept failure: `accepted` or `spawned` → `failed_post_accept`.
/// Auto-retry is forbidden after `accepted` (round-2 P0 #2): a partial
/// turn may have already produced a Discord placeholder + tmux session,
/// so retrying could double-emit. Operator must manually intervene via
/// the Phase 5 ops CLI (transition 12 = force-fail + new attempt).
///
/// Returns `Ok(true)` on a real transition; `Ok(false)` when the row
/// was no longer in an `accepted`/`spawned` state.
pub(crate) async fn mark_failed_post_accept(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
    error_message: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'failed_post_accept',
             completed_at = NOW(),
             last_error = $3
         WHERE id = $1
           AND status IN ('accepted', 'spawned')
           AND claim_owner = $2",
    )
    .bind(id)
    .bind(claim_owner)
    .bind(error_message)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// Leader sweep: rows stuck in `claimed` past `stale_after_secs` (worker
/// died between claim and accept) are demoted back to `pending` so a
/// healthy worker can re-claim. Idempotent + safe under contention —
/// uses a WHERE clause that re-checks the stale predicate.
///
/// Returns the number of rows reset. Phase 4 emits this count as a
/// metric for operator monitoring.
pub(crate) async fn sweep_stale_pre_accept_claims(
    pool: &PgPool,
    stale_after_secs: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE intake_outbox
         SET status = 'pending',
             claim_owner = NULL,
             claimed_at = NULL
         WHERE status = 'claimed'
           AND claimed_at < NOW() - ($1::BIGINT * INTERVAL '1 second')",
    )
    .bind(stale_after_secs.max(1))
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Statuses that `force_fail_and_retry_as_new` will accept. Other
/// states (notably `done` — codex Phase 5 P0 #2) are refused: a worker
/// `mark_done` racing with operator force-fail could rewrite a
/// completed row into `failed_post_accept` and double-emit on retry.
const TRANSITION_12_ALLOWED: [&str; 3] = ["accepted", "spawned", "failed_post_accept"];

/// Reasons `force_fail_and_retry_as_new` may refuse to operate.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ForceFailError {
    #[error("intake_outbox row id={0} does not exist")]
    NotFound(i64),
    #[error(
        "intake_outbox row id={id} is in status='{status}'; force-fail is only allowed from \
         accepted/spawned/failed_post_accept (running transition 12 from any other state could \
         double-emit a Discord turn)"
    )]
    DisallowedStatus { id: i64, status: String },
    #[error("postgres error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Operator-driven force-fail + retry-as-new. Phase 5 transition 12:
/// when a row is stuck in `accepted`, `spawned`, or `failed_post_accept`
/// (worker hung mid-turn or post-accept failure pinged the operator
/// alert), this single-transaction helper:
///
///   1. Marks the stuck row as `failed_post_accept` with the operator's
///      reason text (no-op if already `failed_post_accept`).
///   2. INSERTs a fresh row in the same `(channel_id, user_msg_id)`
///      family with `attempt_no = MAX + 1` and
///      `parent_outbox_id = <stuck_id>`, copying the original payload
///      so the worker can re-run the turn from scratch.
///
/// Both writes happen inside one transaction so the partial unique
/// index `intake_outbox_one_open_route_per_channel` never observes
/// the stuck row AND the new row in OPEN states at the same moment —
/// the stuck row is force-failed BEFORE the new row enters `pending`.
///
/// Codex Phase 5 P0 #2: explicitly REFUSES `done`,
/// `failed_pre_accept`, `pending`, and `claimed`. The `done` case is
/// the dangerous one — a worker `mark_done` racing with the operator
/// CLI could otherwise rewrite a completed row into
/// `failed_post_accept` and trigger a double-execution. The other
/// rejected statuses are not stuck (the natural retry path covers
/// them) so refusing is the right semantic.
///
/// Returns the new row's `id`. Errors with `ForceFailError::NotFound`
/// if `stuck_id` does not exist, or `ForceFailError::DisallowedStatus`
/// for any rejected status.
pub(crate) async fn force_fail_and_retry_as_new(
    pool: &PgPool,
    stuck_id: i64,
    operator_reason: &str,
) -> Result<i64, ForceFailError> {
    let mut tx = pool.begin().await?;

    let row: Option<IntakeOutboxRow> =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1 FOR UPDATE")
            .bind(stuck_id)
            .fetch_optional(&mut *tx)
            .await?;
    let row = row.ok_or(ForceFailError::NotFound(stuck_id))?;

    if !TRANSITION_12_ALLOWED.contains(&row.status.as_str()) {
        return Err(ForceFailError::DisallowedStatus {
            id: stuck_id,
            status: row.status.clone(),
        });
    }

    // Force-terminate if not already terminal:
    //   - 'accepted' / 'spawned': hung mid-turn; mark failed_post_accept.
    //   - 'failed_post_accept': already terminal; just rebuild a new attempt.
    if row.status != "failed_post_accept" {
        sqlx::query(
            "UPDATE intake_outbox
             SET status = 'failed_post_accept',
                 completed_at = COALESCE(completed_at, NOW()),
                 last_error = $2
             WHERE id = $1",
        )
        .bind(stuck_id)
        .bind(format!(
            "operator force-fail (was: {}); reason: {}",
            row.status, operator_reason
        ))
        .execute(&mut *tx)
        .await?;
    }

    let next_attempt: Option<i32> = sqlx::query_scalar(
        "SELECT MAX(attempt_no) FROM intake_outbox
         WHERE channel_id = $1 AND user_msg_id = $2",
    )
    .bind(&row.channel_id)
    .bind(&row.user_msg_id)
    .fetch_one(&mut *tx)
    .await?;
    let next_attempt = next_attempt.unwrap_or(0) + 1;

    let new_id: i64 = sqlx::query_scalar(
        r#"
        INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, request_owner_name,
            user_text, reply_context, has_reply_boundary, dm_hint, turn_kind,
            merge_consecutive, reply_to_user_message, defer_watcher_resume,
            wait_for_completion, agent_id,
            status, attempt_no, parent_outbox_id
        ) VALUES (
            $1, $2, $3,
            $4, $5, $6, $7,
            $8, $9, $10, $11, $12,
            $13, $14, $15,
            $16, $17,
            'pending', $18, $19
        )
        RETURNING id
        "#,
    )
    .bind(&row.target_instance_id)
    .bind(&row.forwarded_by_instance_id)
    .bind(&row.required_labels)
    .bind(&row.channel_id)
    .bind(&row.user_msg_id)
    .bind(&row.request_owner_id)
    .bind(row.request_owner_name.as_deref())
    .bind(&row.user_text)
    .bind(row.reply_context.as_deref())
    .bind(row.has_reply_boundary)
    .bind(row.dm_hint)
    .bind(&row.turn_kind)
    .bind(row.merge_consecutive)
    .bind(row.reply_to_user_message)
    .bind(row.defer_watcher_resume)
    .bind(row.wait_for_completion)
    .bind(&row.agent_id)
    .bind(next_attempt)
    .bind(stuck_id)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_id)
}

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

/// Leader sweep that returns rows currently in `accepted` longer than
/// `sla_secs` without reaching `spawned`. Round-3 P1 #3: the operator
/// alert IS the recovery signal — auto-retry forbidden post-accept.
/// Returns `(id, accepted_at)` tuples for each row exceeding the SLA.
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
mod migration_tests {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;
    use sqlx::Row;

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

    /// CHECK constraint must reject an unknown status value. Production
    /// code only writes the seven values from the design; this guard
    /// catches accidental typos at insert time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn intake_outbox_status_check_rejects_unknown_value() {
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
mod helper_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use serde_json::json;

    fn payload(channel: &str, msg: &str) -> InsertPendingPayload {
        InsertPendingPayload {
            target_instance_id: "worker-1".to_string(),
            forwarded_by_instance_id: "leader-1".to_string(),
            required_labels: json!(["unreal"]),
            channel_id: channel.to_string(),
            user_msg_id: msg.to_string(),
            request_owner_id: "user-100".to_string(),
            request_owner_name: Some("Tester".to_string()),
            user_text: "hello world".to_string(),
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: "standard".to_string(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            agent_id: "agent-x".to_string(),
        }
    }

    /// Idempotently seed `agent-x` with provider='claude' so the
    /// `claim_pending_for_target` provider-JOIN finds a match. Phase 5
    /// codex P0 #1: all helper tests that exercise the claim path call
    /// this once near the top.
    async fn seed_default_test_agent(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-x', 'Test', 'claude', 'unused-channel')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .expect("seed default test agent");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn insert_pending_round_trips_payload_and_returns_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let id = insert_pending(&pool, &payload("ch-1", "msg-1"), 1, None)
            .await
            .expect("insert pending");
        assert!(id > 0);

        let row: IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("fetch row");
        assert_eq!(row.channel_id, "ch-1");
        assert_eq!(row.user_msg_id, "msg-1");
        assert_eq!(row.attempt_no, 1);
        assert_eq!(row.status, "pending");
        assert!(row.parent_outbox_id.is_none());
        assert_eq!(row.required_labels, json!(["unreal"]));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn family_max_attempt_returns_zero_when_no_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let max = family_max_attempt(&pool, "ch-empty", "msg-empty")
            .await
            .expect("read max");
        assert_eq!(max, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn family_max_attempt_tracks_highest_attempt_in_family() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        // Drive first attempt to terminal so we can stack a second.
        let id1 = insert_pending(&pool, &payload("ch-fam", "msg-fam"), 1, None)
            .await
            .expect("seed attempt 1");
        sqlx::query("UPDATE intake_outbox SET status='failed_pre_accept' WHERE id=$1")
            .bind(id1)
            .execute(&pool)
            .await
            .expect("terminate attempt 1");

        let id2 = insert_pending(&pool, &payload("ch-fam", "msg-fam"), 2, Some(id1))
            .await
            .expect("seed attempt 2");
        assert!(id2 > id1);

        let max = family_max_attempt(&pool, "ch-fam", "msg-fam")
            .await
            .expect("read max");
        assert_eq!(max, 2);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_pending_for_target_picks_oldest_pending_and_promotes_to_claimed() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let id1 = insert_pending(&pool, &payload("ch-claim-1", "msg-A"), 1, None)
            .await
            .expect("seed row 1");
        // Slight delay so the second row's created_at is strictly later.
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let _id2 = insert_pending(&pool, &payload("ch-claim-2", "msg-B"), 1, None)
            .await
            .expect("seed row 2");

        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
            .await
            .expect("claim")
            .expect("must claim something");
        assert_eq!(claimed.id, id1);
        assert_eq!(claimed.status, "claimed");
        assert_eq!(claimed.claim_owner.as_deref(), Some("worker-1.local"));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_pending_for_target_returns_none_when_queue_empty() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
            .await
            .expect("claim ok");
        assert!(claimed.is_none());

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_pending_for_target_filters_by_target_instance_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let mut other = payload("ch-other", "msg-other");
        other.target_instance_id = "worker-OTHER".to_string();
        insert_pending(&pool, &other, 1, None)
            .await
            .expect("seed for other worker");

        // Worker-1 polls — must NOT receive the row destined for worker-OTHER.
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "worker-1.local")
            .await
            .expect("claim ok");
        assert!(claimed.is_none(), "must not steal cross-target rows");

        // worker-OTHER polls — picks it up.
        let claimed =
            claim_pending_for_target(&pool, "worker-OTHER", "claude", "worker-OTHER.local")
                .await
                .expect("claim ok")
                .expect("worker-OTHER must claim");
        assert_eq!(claimed.target_instance_id, "worker-OTHER");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_state_machine_pending_to_done_advances_correctly() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let _id = insert_pending(&pool, &payload("ch-full", "msg-full"), 1, None)
            .await
            .expect("insert pending");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");
        assert_eq!(claimed.status, "claimed");

        let advanced = mark_accepted(&pool, claimed.id, "owner-1")
            .await
            .expect("mark_accepted");
        assert!(advanced, "claimed→accepted must advance");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read status after accept");
        assert_eq!(status, "accepted");

        let advanced = mark_spawned(&pool, claimed.id, "owner-1")
            .await
            .expect("mark_spawned");
        assert!(advanced, "accepted→spawned must advance");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read status after spawn");
        assert_eq!(status, "spawned");

        let advanced = mark_done(&pool, claimed.id, "owner-1")
            .await
            .expect("mark_done");
        assert!(advanced, "spawned→done must advance");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read status after done");
        assert_eq!(status, "done");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_accepted_rejects_wrong_claim_owner() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-owner", "msg-owner"), 1, None)
            .await
            .expect("insert");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-A")
            .await
            .expect("claim")
            .expect("row");

        // Wrong owner — must be a no-op AND `Ok(false)`.
        let advanced = mark_accepted(&pool, claimed.id, "owner-WRONG")
            .await
            .expect("mark_accepted ok (no-op for wrong owner)");
        assert!(!advanced, "wrong owner must NOT advance state");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read status");
        assert_eq!(status, "claimed", "wrong owner must NOT advance state");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_accepted_returns_false_when_sweep_already_reset_the_claim() {
        // Race: leader sweep reset the row to `pending` between claim
        // and accept. Worker MUST see `Ok(false)` so it can abort
        // instead of spawning a turn behind a no-longer-owned row.
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-race", "msg-race"), 1, None)
            .await
            .expect("insert");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-fast")
            .await
            .expect("claim")
            .expect("row");

        // Simulate sweep racing in: revert to pending.
        sqlx::query(
            "UPDATE intake_outbox SET status='pending', claim_owner=NULL, claimed_at=NULL
             WHERE id = $1",
        )
        .bind(claimed.id)
        .execute(&pool)
        .await
        .expect("simulate sweep");

        let advanced = mark_accepted(&pool, claimed.id, "owner-fast")
            .await
            .expect("mark_accepted ok");
        assert!(
            !advanced,
            "lost claim must report Ok(false) so worker aborts"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn classify_insert_pending_error_distinguishes_the_two_unique_violations() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        // First INSERT — succeeds. Drive it terminal so the partial
        // unique index does not mask the 3-tuple violation we want.
        let id = insert_pending(&pool, &payload("ch-classify", "msg-classify"), 1, None)
            .await
            .expect("first insert");
        sqlx::query("UPDATE intake_outbox SET status='done', completed_at=NOW() WHERE id=$1")
            .bind(id)
            .execute(&pool)
            .await
            .expect("terminate");

        // Duplicate (channel_id, user_msg_id, attempt_no) — must trip
        // the 3-tuple constraint.
        let dup_err = insert_pending(&pool, &payload("ch-classify", "msg-classify"), 1, None)
            .await
            .expect_err("dup attempt_no must reject");
        assert_eq!(
            classify_insert_pending_error(&dup_err),
            Some(IntakeInsertConflict::DuplicateMessageAttempt)
        );

        // OPEN-route violation: insert a fresh OPEN row, then try a
        // SECOND distinct user_msg_id on the same channel.
        let _open_id = insert_pending(&pool, &payload("ch-open", "msg-A"), 1, None)
            .await
            .expect("first open insert");
        let open_err = insert_pending(&pool, &payload("ch-open", "msg-B"), 1, None)
            .await
            .expect_err("second OPEN row must reject");
        assert_eq!(
            classify_insert_pending_error(&open_err),
            Some(IntakeInsertConflict::OpenRoutePerChannel)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_failed_pre_accept_records_error_and_bumps_retry_count() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-fail", "msg-fail"), 1, None)
            .await
            .expect("insert");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");

        mark_failed_pre_accept(&pool, claimed.id, "owner-1", "cwd validation failed")
            .await
            .expect("mark_failed_pre_accept");

        let row: IntakeOutboxRow = sqlx::query_as("SELECT * FROM intake_outbox WHERE id = $1")
            .bind(claimed.id)
            .fetch_one(&pool)
            .await
            .expect("read row");
        assert_eq!(row.status, "failed_pre_accept");
        assert_eq!(row.retry_count, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_failed_post_accept_works_from_accepted_or_spawned() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        // Setup A: fail from accepted.
        insert_pending(&pool, &payload("ch-postA", "msg-A"), 1, None)
            .await
            .expect("insert A");
        let row_a = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim A")
            .expect("row A");
        mark_accepted(&pool, row_a.id, "owner-1")
            .await
            .expect("accept A");
        mark_failed_post_accept(&pool, row_a.id, "owner-1", "spawned crash")
            .await
            .expect("fail from accepted");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(row_a.id)
            .fetch_one(&pool)
            .await
            .expect("read");
        assert_eq!(status, "failed_post_accept");

        // Setup B: fail from spawned.
        insert_pending(&pool, &payload("ch-postB", "msg-B"), 1, None)
            .await
            .expect("insert B");
        let row_b = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim B")
            .expect("row B");
        mark_accepted(&pool, row_b.id, "owner-1")
            .await
            .expect("accept B");
        mark_spawned(&pool, row_b.id, "owner-1")
            .await
            .expect("spawn B");
        mark_failed_post_accept(&pool, row_b.id, "owner-1", "tmux died")
            .await
            .expect("fail from spawned");
        let status: String = sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
            .bind(row_b.id)
            .fetch_one(&pool)
            .await
            .expect("read");
        assert_eq!(status, "failed_post_accept");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sweep_stale_pre_accept_claims_resets_only_stale_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        // Insert + claim one row, then push claimed_at backwards to simulate
        // staleness. A SECOND row is claimed fresh and must NOT be reset.
        insert_pending(&pool, &payload("ch-stale", "msg-stale"), 1, None)
            .await
            .expect("insert stale");
        let stale = claim_pending_for_target(&pool, "worker-1", "claude", "owner-died")
            .await
            .expect("claim stale")
            .expect("row stale");
        sqlx::query(
            "UPDATE intake_outbox SET claimed_at = NOW() - INTERVAL '300 seconds' WHERE id = $1",
        )
        .bind(stale.id)
        .execute(&pool)
        .await
        .expect("backdate claimed_at");

        insert_pending(&pool, &payload("ch-fresh", "msg-fresh"), 1, None)
            .await
            .expect("insert fresh");
        let fresh = claim_pending_for_target(&pool, "worker-1", "claude", "owner-alive")
            .await
            .expect("claim fresh")
            .expect("row fresh");

        let reset = sweep_stale_pre_accept_claims(&pool, 60)
            .await
            .expect("sweep");
        assert_eq!(reset, 1, "only the stale row should reset");

        let stale_status: String =
            sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
                .bind(stale.id)
                .fetch_one(&pool)
                .await
                .expect("read stale");
        assert_eq!(stale_status, "pending");

        let fresh_status: String =
            sqlx::query_scalar("SELECT status FROM intake_outbox WHERE id = $1")
                .bind(fresh.id)
                .fetch_one(&pool)
                .await
                .expect("read fresh");
        assert_eq!(fresh_status, "claimed", "fresh claim must not reset");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_accepted_unspawned_sla_returns_only_overdue_rows() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        // Stale: accepted long ago, never spawned.
        insert_pending(&pool, &payload("ch-sla1", "msg-sla1"), 1, None)
            .await
            .expect("insert sla1");
        let row1 = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row1");
        mark_accepted(&pool, row1.id, "owner-1")
            .await
            .expect("accept");
        sqlx::query(
            "UPDATE intake_outbox SET accepted_at = NOW() - INTERVAL '600 seconds' WHERE id = $1",
        )
        .bind(row1.id)
        .execute(&pool)
        .await
        .expect("backdate accepted_at");

        // Fresh accepted row — should NOT be flagged.
        insert_pending(&pool, &payload("ch-sla2", "msg-sla2"), 1, None)
            .await
            .expect("insert sla2");
        let row2 = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row2");
        mark_accepted(&pool, row2.id, "owner-1")
            .await
            .expect("accept");

        let stale = list_accepted_unspawned_sla(&pool, 60).await.expect("list");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].0, row1.id);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_fail_and_retry_as_new_terminates_stuck_row_and_inserts_attempt_2() {
        // Phase 5 transition 12: a row stuck in 'spawned' (worker hung
        // mid-turn) is force-failed by the operator, and a fresh row
        // is inserted with attempt_no=2 + parent_outbox_id pointing
        // at the stuck row. Both writes happen in one transaction so
        // the partial unique index never sees both rows OPEN.
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-stuck", "msg-stuck"), 1, None)
            .await
            .expect("seed");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");
        mark_accepted(&pool, claimed.id, "owner-1")
            .await
            .expect("accept");
        mark_spawned(&pool, claimed.id, "owner-1")
            .await
            .expect("spawn");

        let new_id = force_fail_and_retry_as_new(&pool, claimed.id, "operator: tmux pane crashed")
            .await
            .expect("force-fail");

        // Stuck row → failed_post_accept with operator reason embedded.
        let stuck: (String, Option<String>) =
            sqlx::query_as("SELECT status, last_error FROM intake_outbox WHERE id = $1")
                .bind(claimed.id)
                .fetch_one(&pool)
                .await
                .expect("read stuck row");
        assert_eq!(stuck.0, "failed_post_accept");
        let last_error = stuck.1.expect("last_error must be set");
        assert!(
            last_error.contains("operator force-fail (was: spawned)")
                && last_error.contains("tmux pane crashed"),
            "last_error must record original status + operator reason: {last_error}"
        );

        // New row → pending, attempt_no=2, parent points at stuck.
        let new_row: (String, i32, Option<i64>, String, String) = sqlx::query_as(
            "SELECT status, attempt_no, parent_outbox_id, channel_id, user_msg_id
             FROM intake_outbox WHERE id = $1",
        )
        .bind(new_id)
        .fetch_one(&pool)
        .await
        .expect("read new row");
        assert_eq!(new_row.0, "pending");
        assert_eq!(new_row.1, 2);
        assert_eq!(new_row.2, Some(claimed.id));
        assert_eq!(new_row.3, "ch-stuck");
        assert_eq!(new_row.4, "msg-stuck");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_fail_and_retry_as_new_on_already_terminal_row_just_appends_attempt() {
        // Operator hits the helper after the worker already wrote
        // failed_post_accept (the alert path's normal flow). The
        // helper must NOT overwrite the existing last_error and must
        // still insert the new attempt.
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-terminal", "msg-terminal"), 1, None)
            .await
            .expect("seed");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");
        mark_accepted(&pool, claimed.id, "owner-1")
            .await
            .expect("accept");
        mark_failed_post_accept(&pool, claimed.id, "owner-1", "original failure")
            .await
            .expect("fail post-accept");

        let new_id = force_fail_and_retry_as_new(&pool, claimed.id, "operator: retry approved")
            .await
            .expect("force-fail");

        let stuck_last_error: Option<String> =
            sqlx::query_scalar("SELECT last_error FROM intake_outbox WHERE id = $1")
                .bind(claimed.id)
                .fetch_one(&pool)
                .await
                .expect("read stuck last_error");
        assert_eq!(
            stuck_last_error.as_deref(),
            Some("original failure"),
            "already-terminal row must preserve its original last_error"
        );

        let new_attempt: i32 =
            sqlx::query_scalar("SELECT attempt_no FROM intake_outbox WHERE id = $1")
                .bind(new_id)
                .fetch_one(&pool)
                .await
                .expect("read new attempt");
        assert_eq!(new_attempt, 2);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_fail_and_retry_as_new_refuses_done_row_to_prevent_double_emit() {
        // Codex Phase 5 P0 #2: a worker `mark_done` racing with the
        // operator CLI could otherwise rewrite a completed row into
        // `failed_post_accept` and trigger a re-execution that
        // double-emits a Discord turn. The helper MUST refuse `done`.
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        insert_pending(&pool, &payload("ch-done", "msg-done"), 1, None)
            .await
            .expect("insert");
        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");
        mark_accepted(&pool, claimed.id, "owner-1")
            .await
            .expect("accept");
        mark_spawned(&pool, claimed.id, "owner-1")
            .await
            .expect("spawn");
        mark_done(&pool, claimed.id, "owner-1").await.expect("done");

        let err = force_fail_and_retry_as_new(&pool, claimed.id, "operator: bad timing")
            .await
            .expect_err("done row must be refused");
        match err {
            ForceFailError::DisallowedStatus { id, status } => {
                assert_eq!(id, claimed.id);
                assert_eq!(status, "done");
            }
            other => panic!("expected DisallowedStatus, got {other:?}"),
        }

        // Family did NOT grow — the rejection is total.
        let family_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox
             WHERE channel_id = 'ch-done' AND user_msg_id = 'msg-done'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(family_count, 1, "rejection must not insert a child");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_fail_and_retry_as_new_refuses_pending_and_claimed_states() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let pending_id = insert_pending(&pool, &payload("ch-p", "msg-p"), 1, None)
            .await
            .expect("insert pending");
        let err_pending = force_fail_and_retry_as_new(&pool, pending_id, "x")
            .await
            .expect_err("pending row must be refused");
        assert!(matches!(
            err_pending,
            ForceFailError::DisallowedStatus { .. }
        ));

        let claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-1")
            .await
            .expect("claim")
            .expect("row");
        let err_claimed = force_fail_and_retry_as_new(&pool, claimed.id, "x")
            .await
            .expect_err("claimed row must be refused");
        assert!(matches!(
            err_claimed,
            ForceFailError::DisallowedStatus { .. }
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn claim_pending_for_target_filters_by_provider_join() {
        // Codex Phase 5 P0 #1: a single AgentDesk process can host
        // claude AND codex bots; both call `run_intake_worker_loop`
        // with the same `target_instance_id`. The provider JOIN
        // ensures a claude bot's worker only claims rows whose
        // `agents.provider = 'claude'` and never picks up a codex
        // row (which would run with the wrong Http/SharedData/token).
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        // Seed two agents with different providers.
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-claude', 'Claude bot', 'claude', 'unused-ch-claude'),
                    ('agent-codex', 'Codex bot', 'codex', 'unused-ch-codex')",
        )
        .execute(&pool)
        .await
        .expect("seed agents");

        // Insert a row for each provider — both targeted at worker-1.
        let mut claude_payload = payload("ch-claude", "msg-claude");
        claude_payload.agent_id = "agent-claude".to_string();
        insert_pending(&pool, &claude_payload, 1, None)
            .await
            .expect("insert claude row");

        let mut codex_payload = payload("ch-codex", "msg-codex");
        codex_payload.agent_id = "agent-codex".to_string();
        insert_pending(&pool, &codex_payload, 1, None)
            .await
            .expect("insert codex row");

        // Claude worker polls — must see ONLY the claude row.
        let claude_claimed = claim_pending_for_target(&pool, "worker-1", "claude", "owner-claude")
            .await
            .expect("claude claim ok")
            .expect("must claim something");
        assert_eq!(claude_claimed.agent_id, "agent-claude");
        assert_eq!(claude_claimed.channel_id, "ch-claude");

        // Claude polls again — codex row must STILL be invisible.
        let again = claim_pending_for_target(&pool, "worker-1", "claude", "owner-claude")
            .await
            .expect("second claude claim ok");
        assert!(
            again.is_none(),
            "claude worker must not see codex's pending row"
        );

        // Codex worker polls — picks up the codex row.
        let codex_claimed = claim_pending_for_target(&pool, "worker-1", "codex", "owner-codex")
            .await
            .expect("codex claim ok")
            .expect("must claim something");
        assert_eq!(codex_claimed.agent_id, "agent-codex");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_recent_rows_supports_channel_filter_and_orders_descending() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_default_test_agent(&pool).await;

        let _id_a1 = insert_pending(&pool, &payload("ch-A", "msg-A1"), 1, None)
            .await
            .expect("seed A1");
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        let id_b = insert_pending(&pool, &payload("ch-B", "msg-B1"), 1, None)
            .await
            .expect("seed B");
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        sqlx::query("UPDATE intake_outbox SET status='done' WHERE channel_id='ch-A'")
            .execute(&pool)
            .await
            .expect("terminate A1");
        let id_a2 = insert_pending(&pool, &payload("ch-A", "msg-A2"), 1, None)
            .await
            .expect("seed A2");

        let all = list_recent_rows(&pool, None, 10).await.expect("list all");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, id_a2, "newest first");

        let just_a = list_recent_rows(&pool, Some("ch-A"), 10)
            .await
            .expect("list A");
        assert_eq!(just_a.len(), 2);
        assert!(just_a.iter().all(|r| r.channel_id == "ch-A"));

        let just_b = list_recent_rows(&pool, Some("ch-B"), 10)
            .await
            .expect("list B");
        assert_eq!(just_b.len(), 1);
        assert_eq!(just_b[0].id, id_b);

        pool.close().await;
        pg_db.drop().await;
    }
}
