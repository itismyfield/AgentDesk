use sqlx::{PgPool, Row as SqlxRow};

use super::entries::{ENTRY_STATUS_DONE, ENTRY_STATUS_USER_CANCELLED};
use super::slots::release_run_slots_on_pg_tx;

pub(crate) async fn queue_run_completion_notify_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let row = sqlx::query("SELECT repo, agent_id FROM auto_queue_runs WHERE id = $1")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("load completion notify targets for run {run_id}: {error}"))?;
    let repo: Option<String> = row
        .try_get("repo")
        .map_err(|error| format!("decode completion notify repo for run {run_id}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode completion notify agent_id for run {run_id}: {error}"))?;
    let targets = completion_notify_targets_on_pg(tx, run_id, agent_id.as_deref()).await?;
    if targets.is_empty() {
        return Ok(());
    }

    let entry_count =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = $1")
            .bind(run_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|error| format!("count auto-queue entries for run {run_id}: {error}"))?;
    let repo_label = repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in targets {
        let target = format!("channel:{channel_id}");
        crate::services::message_outbox::enqueue_outbox_pg_on_tx(
            tx,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &content,
                bot: crate::services::discord::bot_role::UtilityBotRole::Notify.alias(),
                source: "system",
                reason_code: None,
                session_key: None,
            },
        )
        .await
        .map_err(|error| {
            format!(
                "queue auto-queue completion notify for run {run_id} channel {channel_id}: {error}"
            )
        })?;
    }

    Ok(())
}

async fn completion_notify_targets_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    run_agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut targets = Vec::new();

    if let Some(agent_id) = run_agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let channel_id = sqlx::query("SELECT discord_channel_id FROM agents WHERE id = $1")
            .bind(agent_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| {
                format!("load completion notify agent channel for run {run_id}: {error}")
            })?
            .map(|row| {
                row.try_get::<Option<String>, _>("discord_channel_id")
                    .map_err(|error| {
                        format!("decode completion notify agent channel for run {run_id}: {error}")
                    })
            })
            .transpose()?
            .flatten();
        if let Some(channel_id) = channel_id
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            targets.push(channel_id);
        }
    }

    if targets.is_empty() {
        let rows = sqlx::query(
            "SELECT DISTINCT a.discord_channel_id
             FROM auto_queue_entries e
             JOIN agents a ON a.id = e.agent_id
             WHERE e.run_id = $1
               AND a.discord_channel_id IS NOT NULL
               AND TRIM(a.discord_channel_id) != ''",
        )
        .bind(run_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|error| {
            format!("load completion notify fallback channels for run {run_id}: {error}")
        })?;
        for row in rows {
            let channel_id: String = row.try_get("discord_channel_id").map_err(|error| {
                format!("decode completion notify fallback channel for run {run_id}: {error}")
            })?;
            targets.push(channel_id);
        }
    }

    targets.sort();
    targets.dedup();
    Ok(targets)
}

pub(super) async fn maybe_finalize_run_after_terminal_entry_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    new_status: &str,
) -> Result<bool, String> {
    if new_status == ENTRY_STATUS_DONE {
        return Ok(false);
    }
    // #815 P1: never finalize on `user_cancelled` — it must leave the run in a
    // resumable state so the operator can flip the entry back to `pending`.
    if new_status == ENTRY_STATUS_USER_CANCELLED {
        return Ok(false);
    }

    maybe_finalize_run_if_ready_pg(tx, run_id).await
}

pub(super) async fn acquire_run_advisory_xact_lock_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('aq_run:' || $1))")
        .bind(run_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("lock auto-queue run {run_id}: {error}"))?;
    Ok(())
}

/// Acquire multiple run tokens in the incumbent force-pause order.
///
/// Loading the ordered ids separately makes lock acquisition order explicit;
/// it does not rely on a planner preserving an `ORDER BY` through a CTE that
/// also invokes `pg_advisory_xact_lock`.
pub(crate) async fn acquire_run_advisory_xact_locks_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_ids: &[String],
) -> Result<Vec<String>, String> {
    let ordered_run_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_runs
         WHERE id = ANY($1)
         ORDER BY created_at ASC, id ASC",
    )
    .bind(run_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| format!("order auto-queue runs before advisory locking: {error}"))?;

    for run_id in &ordered_run_ids {
        acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await?;
    }

    Ok(ordered_run_ids)
}

async fn try_acquire_run_advisory_xact_lock_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_xact_lock(hashtext('aq_run:' || $1))")
        .bind(run_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| format!("try-lock auto-queue run {run_id}: {error}"))
}

/// Transaction-local opt-out for a failed-sync whose replacement attachment
/// completes later in the same combined transaction, after the intervening
/// dispatch, event, outbox, and card writes. The caller must restore the
/// setting in the same transaction; this helper only changes the
/// transaction-local flag.
pub(crate) async fn set_terminal_entry_finalize_suppressed_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    suppressed: bool,
) -> Result<(), String> {
    sqlx::query("SELECT set_config('agentdesk.suppress_terminal_entry_finalize', $1, true)")
        .bind(if suppressed { "on" } else { "off" })
        .execute(&mut **tx)
        .await
        .map_err(|error| format!("set terminal-entry finalizer suppression: {error}"))?;
    Ok(())
}

/// Why the canonical completion writer left a run unchanged. Every variant names
/// who resolves it and whether the run keeps its slot, so no blocked run lacks an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCompletionBlockedReason {
    /// Another transaction holds `aq_run:<run_id>`; the next terminal-entry
    /// write or tick sweep retries. Slot stays held.
    TokenContended,
    /// The caller suppressed finalization for the rest of its transaction
    /// (failed-sync replacement attach) and completes later itself.
    FinalizeSuppressed,
    /// No run row exists, so nothing owns a slot.
    RunNotFound,
    /// Run is `restoring` or in an unknown status: restore finalization or the
    /// operator restore/cancel command resolves it. Slot stays held by design.
    RunStatusNotCompletable,
    /// A pending/failed phase gate: a gate pass clears it automatically, an
    /// orphan gate needs operator repair. Gate creation already released the slot.
    BlockingPhaseGate,
    /// Policy is still creating gate dispatches (`phase_gate_grace_until` is in
    /// the future); the window expires within 30s and the tick sweep retries.
    PhaseGateGraceWindow,
    /// An operator-held `user_cancelled` entry: the operator resumes or skips
    /// it, or ends the run. Slot stays held so a resumed entry can reuse it.
    UserCancelledEntry,
    /// `pending`/`dispatched` entries remain; their terminal transitions retry.
    RunnableEntry,
}

impl RunCompletionBlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TokenContended => "token_contended",
            Self::FinalizeSuppressed => "finalize_suppressed",
            Self::RunNotFound => "run_not_found",
            Self::RunStatusNotCompletable => "run_status_not_completable",
            Self::BlockingPhaseGate => "blocking_phase_gate",
            Self::PhaseGateGraceWindow => "phase_gate_grace_window",
            Self::UserCancelledEntry => "user_cancelled_entry",
            Self::RunnableEntry => "runnable_entry",
        }
    }
}

/// Result of one canonical completion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCompletionOutcome {
    /// This transaction flipped the run to `completed`, released its slots and
    /// queued the completion notification.
    Completed,
    /// Readiness failed; nothing was written.
    Blocked(RunCompletionBlockedReason),
    /// The run was already `completed` or `cancelled`; nothing was written and
    /// no notification was queued again.
    AlreadyTerminal,
}

impl RunCompletionOutcome {
    pub fn is_completed(self) -> bool {
        matches!(self, Self::Completed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Blocked(_) => "blocked",
            Self::AlreadyTerminal => "already_terminal",
        }
    }

    pub fn blocked_reason(self) -> Option<RunCompletionBlockedReason> {
        match self {
            Self::Blocked(reason) => Some(reason),
            _ => None,
        }
    }

    /// Log label: the blocked reason when blocked, otherwise the outcome name.
    pub fn describe(self) -> &'static str {
        self.blocked_reason()
            .map(RunCompletionBlockedReason::as_str)
            .unwrap_or(self.as_str())
    }
}

struct RunEntryReadiness {
    runnable: i64,
    user_cancelled: i64,
    total: i64,
}

async fn run_entry_readiness_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<RunEntryReadiness, String> {
    let row = sqlx::query(
        "SELECT COUNT(*) FILTER (WHERE status IN ('pending', 'dispatched')) AS runnable,
                COUNT(*) FILTER (WHERE status = 'user_cancelled') AS user_cancelled,
                COUNT(*) AS total
         FROM auto_queue_entries
         WHERE run_id = $1",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))?;
    let column = |name: &str| -> Result<i64, String> {
        row.try_get::<i64, _>(name).map_err(|error| {
            format!("decode auto-queue entry readiness {name} for run {run_id}: {error}")
        })
    };
    Ok(RunEntryReadiness {
        runnable: column("runnable")?,
        user_cancelled: column("user_cancelled")?,
        total: column("total")?,
    })
}

/// A blocking advisory acquisition here would invert the lock order used by
/// terminal entry writers: `update_entry_status_on_pg_tx` can already hold an
/// entry row while `terminalize_selected_runs_with_pg` acquires the run token
/// before locking entries. The non-blocking acquisition below adds no wait and
/// defers finalization whenever another transaction owns the token.
///
/// When the try-lock succeeds, the token remains held until the caller's
/// transaction commits. Blocking attach, cancel, and explicit-completion
/// participants can therefore wait behind this opportunistic finalizer. That
/// set includes the activate tail through `complete_run_after_activate_on_pg`:
/// JS policy calls are bounded by the bridge deadline, while the HTTP activate
/// path has no corresponding deadline. Avoiding an entry-row/run-token ABBA
/// therefore comes with possible participant delay rather than no waiting in
/// the protocol.
///
/// Moving a blocking acquisition above every entry, card, and run write in the
/// callers of this helper would broaden per-run serialization across the sync,
/// GitHub, phase-gate reconciliation, and dispatch-terminal paths. Those paths
/// therefore retain their existing lock order and use this opportunistic
/// finalizer instead. Its remaining-entry predicate is derived only after the
/// token is acquired, so a previously computed count cannot cross an attach
/// commit protected by the same token.
///
/// Blocking participants take `aq_run:<run_id>` before row locks: cancel and
/// terminalize, force-pause, phase-gate attachment (including its attachment-
/// free branch), consultation attachment, explicit completion, dispatched-
/// entry choke points, done-entry reactivation, retry attachment, campaign
/// handoff append, activate's empty-run completion `complete_run_if_empty`,
/// and the run-scoped reset `reset_run_scoped_with_pg` (#4880).
/// Retry already takes the d1 retry token first and then the run token before its
/// failed-sync and replacement attachment. Later cards/entries/runs/slots
/// ordering is serialized by that first run token for these participants.
/// `maybe_finalize_run_if_ready_pg` is the exception: callers may already hold
/// entry or run rows, so its try-lock is non-blocking and adds no wait edge.
///
/// `lock_phase_gate_state_on_pg_tx` uses PostgreSQL's two-argument advisory
/// key space. It is separate from the one-argument `aq_run:<run_id>` token and
/// does not serialize phase-gate state writes with cancel.
///
/// The only other `completed` writers are the destructive admin owners pinned by
/// `run_completion_contract_tests`: the operator `end` command and the two resets.
pub(crate) async fn finalize_run_if_ready_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<RunCompletionOutcome, String> {
    use RunCompletionBlockedReason as Reason;

    if !try_acquire_run_advisory_xact_lock_on_pg_tx(tx, run_id).await? {
        tracing::info!(run_id = %run_id, "run_finalize_deferred_lock_contended");
        return Ok(RunCompletionOutcome::Blocked(Reason::TokenContended));
    }

    let finalize_suppressed = sqlx::query_scalar::<_, bool>(
        "SELECT COALESCE(
             current_setting('agentdesk.suppress_terminal_entry_finalize', true) = 'on',
             false
         )",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("read terminal-entry finalizer suppression: {error}"))?;
    if finalize_suppressed {
        return Ok(RunCompletionOutcome::Blocked(Reason::FinalizeSuppressed));
    }

    let run_row = sqlx::query(
        "SELECT status, COALESCE(phase_gate_grace_until > NOW(), false) AS in_grace
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue run {run_id} for completion: {error}"))?;
    let Some(run_row) = run_row else {
        return Ok(RunCompletionOutcome::Blocked(Reason::RunNotFound));
    };
    let run_status: String = run_row
        .try_get("status")
        .map_err(|error| format!("decode auto-queue run status for {run_id}: {error}"))?;
    let in_grace: bool = run_row
        .try_get("in_grace")
        .map_err(|error| format!("decode auto-queue grace window for {run_id}: {error}"))?;
    if matches!(run_status.as_str(), "completed" | "cancelled") {
        return Ok(RunCompletionOutcome::AlreadyTerminal);
    }

    if super::phase_gates::run_has_blocking_phase_gate_on_pg_tx(tx, run_id).await? {
        return Ok(RunCompletionOutcome::Blocked(Reason::BlockingPhaseGate));
    }
    if in_grace {
        return Ok(RunCompletionOutcome::Blocked(Reason::PhaseGateGraceWindow));
    }

    let entries = run_entry_readiness_on_pg_tx(tx, run_id).await?;
    if entries.user_cancelled > 0 {
        return Ok(RunCompletionOutcome::Blocked(Reason::UserCancelledEntry));
    }
    if entries.runnable > 0 {
        return Ok(RunCompletionOutcome::Blocked(Reason::RunnableEntry));
    }

    // The status transition is the release authority. In particular, a run in
    // the restore hand-off window must retain its slot until restore finalizes;
    // releasing first and then discovering the status is ineligible creates a
    // restoring-run / unowned-slot split brain.
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tracing::info!(
            run_id = %run_id,
            status = %run_status,
            "run_finalize_refused_status_not_completable"
        );
        return Ok(RunCompletionOutcome::Blocked(
            Reason::RunStatusNotCompletable,
        ));
    }

    release_run_slots_on_pg_tx(tx, run_id)
        .await
        .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;
    // A run that never held an entry has nothing to announce.
    if entries.total > 0 {
        queue_run_completion_notify_on_pg(tx, run_id).await?;
    }
    Ok(RunCompletionOutcome::Completed)
}

/// Boolean view of [`finalize_run_if_ready_on_pg_tx`] for terminal-entry and
/// phase-gate writers that only need to know whether this transaction completed the run.
pub(crate) async fn maybe_finalize_run_if_ready_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    Ok(finalize_run_if_ready_on_pg_tx(tx, run_id)
        .await?
        .is_completed())
}

pub(super) async fn auto_queue_run_review_disabled_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<bool, String> {
    let review_mode = sqlx::query_scalar::<_, Option<String>>(
        "SELECT review_mode FROM auto_queue_runs WHERE id = $1",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("load auto-queue review mode for run {run_id}: {error}"))?
    .flatten();

    Ok(review_mode.as_deref().unwrap_or("enabled") == "disabled")
}

pub async fn pause_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres pause auto-queue run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'paused',
             completed_at = NULL
         WHERE id = $1
           AND status = 'active'",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("pause postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated > 0 {
        release_run_slots_on_pg_tx(&mut tx, run_id)
            .await
            .map_err(|error| {
                format!("release postgres auto-queue slots for paused run {run_id}: {error}")
            })?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres pause auto-queue run {run_id}: {error}"))?;
    Ok(updated > 0)
}

pub async fn resume_run_on_pg(pool: &PgPool, run_id: &str) -> Result<bool, String> {
    // Keep scoped Resume on the same canonical gate predicate as the global
    // Resume route. A pending/failed gate must leave the paused run untouched.
    if super::phase_gates::run_has_blocking_phase_gate_pg(pool, run_id)
        .await
        .map_err(|error| format!("check blocking phase gates for run {run_id}: {error}"))?
    {
        return Ok(false);
    }

    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'active',
             completed_at = NULL
         WHERE id = $1
           AND status = 'paused'",
    )
    .bind(run_id)
    .execute(pool)
    .await
    .map_err(|error| format!("resume postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    Ok(updated > 0)
}

/// Transaction-owning entry point for callers holding no row locks (policy `completeRun`,
/// activate tail): blocking run token, same readiness predicate, commit only on `Completed`.
pub async fn finalize_run_if_ready_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<RunCompletionOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres complete auto-queue run {run_id}: {error}"))?;
    acquire_run_advisory_xact_lock_on_pg_tx(&mut tx, run_id).await?;
    let outcome = finalize_run_if_ready_on_pg_tx(&mut tx, run_id).await?;
    if outcome.is_completed() {
        tx.commit().await.map_err(|error| {
            format!("commit postgres complete auto-queue run {run_id}: {error}")
        })?;
    } else {
        tracing::info!(
            run_id = %run_id,
            outcome = outcome.describe(),
            "complete_run_left_unchanged"
        );
        tx.rollback().await.map_err(|error| {
            format!("rollback refused postgres complete auto-queue run {run_id}: {error}")
        })?;
    }
    Ok(outcome)
}
