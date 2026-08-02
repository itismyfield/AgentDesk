use sqlx::{PgPool, Row as SqlxRow};

use super::entries::{ENTRY_STATUS_DONE, ENTRY_STATUS_USER_CANCELLED};
use super::slots::release_run_slots_on_pg_tx;

#[derive(Debug, Clone, PartialEq, Eq)]
/// Why canonical run completion declined to write `completed`.
///
/// Every new variant must document all three parts of its ownership contract:
/// the resolver (and whether it is automatic or manual), the signal emitted
/// when resolution does not happen, and whether the run's slot is retained or
/// has already been released by the surrounding lifecycle.
pub enum RunCompletionBlockedReason {
    /// Resolution: a valid gate is cleared automatically by terminal-dispatch
    /// reconciliation; an orphan gate has no automatic resolver and requires
    /// the operator HTTP repair route. Signal: readiness returns this typed
    /// reason, while failed-gate handling and operator repair emit their
    /// existing alerts/audit summary; no tick invokes orphan repair. Slot:
    /// readiness itself changes nothing, but normal gate creation pauses the
    /// run before gate dispatch creation, and that pause releases its slot.
    BlockingPhaseGate,
    /// Resolution: the bounded completion tick retries after the DB comparison
    /// says the grace deadline has passed. Signal: readiness returns this typed
    /// reason until then. Slot: readiness preserves current ownership. A tick
    /// racing the pre-gate continuation leaves the slot held until that
    /// continuation clears the deadline or pauses; gate creation then releases
    /// the slot through pause.
    /// The deadline is written from application `Date.now()` but compared with
    /// DB `NOW()`, so finite clock skew can extend the otherwise bounded wait.
    PhaseGateGraceWindow,
    /// Resolution: the bounded tick automatically changes drained, gate-free,
    /// grace-expired `user_cancelled` entries to `skipped`, then re-enters the
    /// canonical writer. Signal: readiness returns this typed reason and tick
    /// transition failures reach policy error logging. Slot: any assigned slot
    /// remains held until the automatic transition lets completion release it.
    UserCancelledEntry,
    /// Resolution: the bounded dispatch tick automatically advances pending
    /// work on `active` runs; a final-phase block first resumes a paused run.
    /// Signal: readiness returns this typed reason and a failed final-phase
    /// resume emits a warning. Slot: readiness preserves current ownership.
    /// `generated`/`pending` runs are not scanned by that tick, but are still
    /// pre-activation and therefore do not own an assigned slot.
    RunnableEntry,
    /// Resolution: none is automatic. In particular, if the process crashes
    /// after `apply_restore_state_changes_pg` commits `restoring`
    /// (`fsm.rs:292-313`) but before `finalize_restore_run_pg`
    /// (`fsm.rs:398-437`), only an operator restore retry exits the state.
    /// Signal: an explicit readiness call returns this typed reason, but the
    /// stuck `restoring` state has no periodic sweep or alert. Slot: retained
    /// deliberately across restore. This crash window is pre-existing debt,
    /// not introduced by #4881.
    RunStatusNotCompletable,
    /// Resolution: the `onCardTerminal` policy hook automatically performs the
    /// continuation that may create a phase gate or re-enter finalization.
    /// Signal: the terminal-entry writer returns this typed reason while that
    /// continuation is pending. Slot: readiness retains it; continuation keeps
    /// it while dispatching more work, or releases it by completion/gate pause.
    PolicyContinuationPending,
    /// Resolution: none applies because no run row exists. Signal: the caller
    /// receives this typed reason and must treat the identifier as stale or
    /// invalid. Slot: readiness neither acquires nor releases a slot for the
    /// missing run.
    RunNotFound,
}

impl RunCompletionBlockedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BlockingPhaseGate => "blocking_phase_gate",
            Self::PhaseGateGraceWindow => "phase_gate_grace_window",
            Self::UserCancelledEntry => "user_cancelled_entry",
            Self::RunnableEntry => "runnable_entry",
            Self::RunStatusNotCompletable => "run_status_not_completable",
            Self::PolicyContinuationPending => "policy_continuation_pending",
            Self::RunNotFound => "run_not_found",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunReadinessOutcome {
    Completed,
    Blocked(RunCompletionBlockedReason),
    AlreadyTerminal,
}

impl RunReadinessOutcome {
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    pub fn outcome_name(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Blocked(_) => "blocked",
            Self::AlreadyTerminal => "already_terminal",
        }
    }

    pub fn blocked_reason(&self) -> Option<&'static str> {
        match self {
            Self::Blocked(reason) => Some(reason.as_str()),
            Self::Completed | Self::AlreadyTerminal => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForceRunCompletionCommand<'a> {
    pub run_id: &'a str,
    pub operator: &'a str,
    pub source: &'a str,
}

async fn queue_run_completion_notify_on_pg(
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
) -> Result<RunReadinessOutcome, String> {
    if new_status == ENTRY_STATUS_DONE {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::PolicyContinuationPending,
        ));
    }
    // #815 + #4881 r2: never finalize directly on `user_cancelled`. Preserve
    // the operator recovery window; the bounded tick later normalizes a
    // drained cancellation to `skipped` and re-enters readiness.
    if new_status == ENTRY_STATUS_USER_CANCELLED {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::UserCancelledEntry,
        ));
    }

    maybe_finalize_run_if_ready_pg(tx, run_id).await
}

/// The policy sweep's `LIMIT 50` caps writer invocations, not SQL statements.
/// `pg_stat_statements` measurements excluding transaction control recorded
/// 801 statements for the old unfiltered 200-writer scenario and at least 401
/// for one prefilter plus 50 ready writers. There is no fixed SQL upper bound:
/// notification routing varies with agent/fallback channel lookup, entry-count
/// lookup, and one outbox insert per resolved target.
pub(crate) async fn maybe_finalize_run_if_ready_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<RunReadinessOutcome, String> {
    let run = sqlx::query(
        "SELECT status,
                phase_gate_grace_until IS NOT NULL
                    AND phase_gate_grace_until > NOW() AS within_phase_gate_grace
         FROM auto_queue_runs
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| format!("lock auto-queue run {run_id} for readiness: {error}"))?;
    let Some(run) = run else {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::RunNotFound,
        ));
    };
    let status: String = run
        .try_get("status")
        .map_err(|error| format!("decode readiness status for run {run_id}: {error}"))?;
    if matches!(status.as_str(), "completed" | "cancelled" | "failed") {
        return Ok(RunReadinessOutcome::AlreadyTerminal);
    }
    if !matches!(
        status.as_str(),
        "active" | "paused" | "generated" | "pending"
    ) {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::RunStatusNotCompletable,
        ));
    }

    if super::phase_gates::run_has_blocking_phase_gate_on_pg_tx(tx, run_id).await? {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::BlockingPhaseGate,
        ));
    }
    let within_phase_gate_grace: bool = run
        .try_get("within_phase_gate_grace")
        .map_err(|error| format!("decode phase-gate grace readiness for run {run_id}: {error}"))?;
    if within_phase_gate_grace {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::PhaseGateGraceWindow,
        ));
    }
    let has_user_cancelled = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
             SELECT 1 FROM auto_queue_entries
             WHERE run_id = $1 AND status = 'user_cancelled'
         )",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("check user-cancelled entries for run {run_id}: {error}"))?;
    if has_user_cancelled {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::UserCancelledEntry,
        ));
    }

    let remaining = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(run_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| format!("count remaining auto-queue entries for run {run_id}: {error}"))?;
    if remaining > 0 {
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::RunnableEntry,
        ));
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
           AND status = $2",
    )
    .bind(run_id)
    .bind(&status)
    .execute(&mut **tx)
    .await
    .map_err(|error| format!("complete auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        return Ok(RunReadinessOutcome::AlreadyTerminal);
    }

    release_run_slots_on_pg_tx(tx, run_id)
        .await
        .map_err(|error| format!("release auto-queue slots for run {run_id}: {error}"))?;
    queue_run_completion_notify_on_pg(tx, run_id).await?;
    Ok(RunReadinessOutcome::Completed)
}

pub async fn finalize_run_if_ready_on_pg(
    pool: &PgPool,
    run_id: &str,
) -> Result<RunReadinessOutcome, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres readiness for run {run_id}: {error}"))?;
    let outcome = maybe_finalize_run_if_ready_pg(&mut tx, run_id).await?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres readiness for run {run_id}: {error}"))?;
    Ok(outcome)
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

pub async fn force_complete_run_on_pg(
    pool: &PgPool,
    command: &ForceRunCompletionCommand<'_>,
) -> Result<RunReadinessOutcome, String> {
    let run_id = command.run_id.trim();
    let operator = command.operator.trim();
    let source = command.source.trim();
    if run_id.is_empty() || operator.is_empty() || source.is_empty() {
        return Err("force completion requires run_id, operator, and source".to_string());
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin postgres complete auto-queue run {run_id}: {error}"))?;
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM auto_queue_runs WHERE id = $1 FOR UPDATE",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| format!("lock force-completed run {run_id}: {error}"))?;
    let Some(status) = status else {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback missing force-completed run {run_id}: {error}"))?;
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::RunNotFound,
        ));
    };
    if matches!(status.as_str(), "completed" | "cancelled" | "failed") {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback terminal force-completed run {run_id}: {error}"))?;
        return Ok(RunReadinessOutcome::AlreadyTerminal);
    }
    if !matches!(
        status.as_str(),
        "active" | "paused" | "generated" | "pending"
    ) {
        tx.rollback().await.map_err(|error| {
            format!("rollback ineligible force-completed run {run_id}: {error}")
        })?;
        return Ok(RunReadinessOutcome::Blocked(
            RunCompletionBlockedReason::RunStatusNotCompletable,
        ));
    }
    // #2048 F17 + #4881 r2: only this explicit force command may bulk-drop
    // valid pending/failed phase gates. The non-force repair path may delete
    // only orphan rows that have no corresponding dispatch and therefore can
    // never reconcile. Force keeps the broad delete + slot release atomic
    // with the status flip.
    sqlx::query("DELETE FROM auto_queue_phase_gates WHERE run_id = $1")
        .bind(run_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("delete phase gates for completed run {run_id}: {error}"))?;
    let updated = sqlx::query(
        "UPDATE auto_queue_runs
         SET status = 'completed',
             completed_at = NOW()
         WHERE id = $1
           AND status IN ('active', 'paused', 'generated', 'pending')",
    )
    .bind(run_id)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("complete postgres auto-queue run {run_id}: {error}"))?
    .rows_affected();
    if updated == 0 {
        tx.rollback().await.map_err(|error| {
            format!("rollback stale postgres complete auto-queue run {run_id}: {error}")
        })?;
        return Ok(RunReadinessOutcome::AlreadyTerminal);
    }

    release_run_slots_on_pg_tx(&mut tx, run_id)
        .await
        .map_err(|error| format!("release slots for completed run {run_id}: {error}"))?;

    queue_run_completion_notify_on_pg(&mut tx, run_id).await?;
    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('auto_queue_run', $1, $2, $3)",
    )
    .bind(run_id)
    .bind(format!("force_complete:{source}"))
    .bind(operator)
    .execute(&mut *tx)
    .await
    .map_err(|error| format!("audit force completion for run {run_id}: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit postgres complete auto-queue run {run_id}: {error}"))?;
    Ok(RunReadinessOutcome::Completed)
}
