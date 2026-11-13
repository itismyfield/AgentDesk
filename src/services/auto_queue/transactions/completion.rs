//! Canonical run completion for the engine, GitHub sync, routes and entry/gate writers.
//! The DB prepares the completing writes on the caller's tx; this module stages the notice there.

use crate::db::auto_queue::completion_write::{
    PreparedCompletion, RunCompletionNoticeFacts, RunCompletionPrepared,
    load_run_completion_notice_facts_on_pg_tx,
};
use crate::db::auto_queue::{
    RunCompletionOutcome, acquire_run_advisory_xact_lock_on_pg_tx, prepare_run_completion_on_pg_tx,
};
use sqlx::PgPool;

/// Completes the run when ready and stages its notice on `tx`; the caller owns commit.
pub(crate) async fn finalize_run_if_ready_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<RunCompletionOutcome, String> {
    match prepare_run_completion_on_pg_tx(tx, run_id).await? {
        RunCompletionPrepared::Unchanged(outcome) => Ok(outcome),
        RunCompletionPrepared::Completed(mut prepared) => {
            if prepared.has_entries() {
                stage_completion_notice(&mut prepared).await?;
            }
            Ok(RunCompletionOutcome::Completed)
        }
    }
}

async fn stage_completion_notice(prepared: &mut PreparedCompletion<'_, '_>) -> Result<(), String> {
    let Some(facts) = prepared.notice_facts().await? else {
        return Ok(());
    };
    let run_id = prepared.run_id().to_string();
    enqueue_completion_notices(prepared.transaction(), &run_id, facts).await
}

/// Notice for a run the caller already completed on `tx` (operator end).
pub(crate) async fn queue_run_completion_notify_on_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
) -> Result<(), String> {
    let Some(facts) = load_run_completion_notice_facts_on_pg_tx(tx, run_id).await? else {
        return Ok(());
    };
    enqueue_completion_notices(tx, run_id, facts).await
}

async fn enqueue_completion_notices(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    run_id: &str,
    facts: RunCompletionNoticeFacts,
) -> Result<(), String> {
    let repo_label = facts
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(global)");
    let short_run_id = &run_id[..8.min(run_id.len())];
    let entry_count = facts.entry_count;
    let content = format!("자동큐 완료: {repo_label} / run {short_run_id} / {entry_count}개");

    for channel_id in facts.targets {
        let target = format!("channel:{channel_id}");
        crate::services::message_outbox::enqueue_session_notice_pg_on_tx(
            tx,
            crate::domain::outbound::outbox::OutboxMessage {
                target: &target,
                content: &content,
                bot: crate::domain::bot_role::UtilityBotRole::Notify.alias(),
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

/// Transaction-owning entry point for callers holding no row locks (policy `completeRun`,
/// activate tail): blocking run token, same readiness predicate, commit only on `Completed`.
pub(crate) async fn finalize_run_if_ready_pg(
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

#[cfg(test)]
#[path = "completion_tests.rs"]
mod tests;
