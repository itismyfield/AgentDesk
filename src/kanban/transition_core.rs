//! Postgres transition orchestration for kanban cards.

use super::github_sync::github_sync_on_transition_pg;
use super::hooks::fire_dynamic_hooks;
use super::state_machine::{record_true_negative_if_pass_with_backends, resolve_pipeline_with_pg};
use super::terminal_cleanup::cleanup_terminal_managed_worktrees_pg;
use super::transition_cleanup::{
    AllowedOnConnMutation, PgTransitionCleanupCounts, clear_escalation_alert_state_on_pg_tx,
    execute_allowed_cleanup_on_pg_tx,
};
use crate::db::Db;
use crate::engine::PolicyEngine;
use anyhow::Result;
use sqlx::Row as SqlxRow;

async fn transition_status_with_opts_pg_inner(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: Option<AllowedOnConnMutation>,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    let row = sqlx::query(
        "SELECT
            status,
            review_status,
            latest_dispatch_id,
            repo_id,
            assigned_agent_id,
            review_entered_at::text AS review_entered_at,
            blocked_reason
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres card {card_id}: {error}"))?
    .ok_or_else(|| anyhow::anyhow!("card not found: {card_id}"))?;

    let old_status: String = row
        .try_get("status")
        .map_err(|error| anyhow::anyhow!("decode status for {card_id}: {error}"))?;
    let review_status: Option<String> = row
        .try_get("review_status")
        .map_err(|error| anyhow::anyhow!("decode review_status for {card_id}: {error}"))?;
    let latest_dispatch_id: Option<String> = row
        .try_get("latest_dispatch_id")
        .map_err(|error| anyhow::anyhow!("decode latest_dispatch_id for {card_id}: {error}"))?;
    let card_repo_id: Option<String> = row
        .try_get("repo_id")
        .map_err(|error| anyhow::anyhow!("decode repo_id for {card_id}: {error}"))?;
    let card_agent_id: Option<String> = row
        .try_get("assigned_agent_id")
        .map_err(|error| anyhow::anyhow!("decode assigned_agent_id for {card_id}: {error}"))?;
    let review_entered_at: Option<String> = row
        .try_get("review_entered_at")
        .map_err(|error| anyhow::anyhow!("decode review_entered_at for {card_id}: {error}"))?;
    let blocked_reason: Option<String> = row
        .try_get("blocked_reason")
        .map_err(|error| anyhow::anyhow!("decode blocked_reason for {card_id}: {error}"))?;

    if old_status == new_status {
        return Ok((
            TransitionResult {
                changed: false,
                from: old_status,
                to: new_status.to_string(),
            },
            PgTransitionCleanupCounts::default(),
        ));
    }

    crate::pipeline::ensure_loaded();
    let effective =
        resolve_pipeline_with_pg(pg_pool, card_repo_id.as_deref(), card_agent_id.as_deref())
            .await?;

    let has_active_dispatch = sqlx::query_scalar::<_, bool>(
        "SELECT COUNT(*) > 0
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_one(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load active dispatch gate for {card_id}: {error}"))?;

    let latest_review_verdict = sqlx::query_scalar::<_, Option<String>>(
        "SELECT result::jsonb ->> 'verdict'
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status = 'completed'
           AND ($2::timestamptz IS NULL OR COALESCE(completed_at, updated_at) >= $2::timestamptz)
         ORDER BY COALESCE(completed_at, updated_at) DESC, id DESC
         LIMIT 1",
    )
    .bind(card_id)
    .bind(review_entered_at.as_deref())
    .fetch_optional(pg_pool)
    .await
    .map_err(|error| anyhow::anyhow!("load latest review verdict for {card_id}: {error}"))?
    .flatten();

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.clone(),
            review_status: review_status.clone(),
            latest_dispatch_id: latest_dispatch_id.clone(),
        },
        pipeline: effective.clone(),
        gates: GateSnapshot {
            has_active_dispatch,
            review_verdict_pass: matches!(
                latest_review_verdict.as_deref(),
                Some("pass") | Some("approved")
            ),
            review_verdict_rework: matches!(
                latest_review_verdict.as_deref(),
                Some("rework") | Some("improve") | Some("reject")
            ),
        },
    };

    let decision = transition::decide_status_transition_with_caller(
        &ctx,
        new_status,
        source,
        force_intent,
        "kanban::transition_status_with_opts_pg",
    );

    if let TransitionOutcome::Blocked(ref reason) = decision.outcome {
        let mut tx = pg_pool
            .begin()
            .await
            .map_err(|error| anyhow::anyhow!("begin blocked postgres transition tx: {error}"))?;
        for intent in &decision.intents {
            crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        tx.commit()
            .await
            .map_err(|error| anyhow::anyhow!("commit blocked postgres transition tx: {error}"))?;
        tracing::warn!(
            "[kanban] Blocked postgres transition {} → {} for card {} (source: {}): {}",
            old_status,
            new_status,
            card_id,
            source,
            reason
        );
        return Err(anyhow::anyhow!("{}", reason));
    }

    if decision.outcome == TransitionOutcome::NoOp {
        return Ok((
            TransitionResult {
                changed: false,
                from: old_status,
                to: new_status.to_string(),
            },
            PgTransitionCleanupCounts::default(),
        ));
    }

    let old_manual_intervention = crate::manual_intervention::requires_manual_intervention(
        review_status.as_deref(),
        blocked_reason.as_deref(),
    );

    let mut tx = pg_pool
        .begin()
        .await
        .map_err(|error| anyhow::anyhow!("begin postgres transition tx: {error}"))?;

    for intent in &decision.intents {
        crate::engine::transition_executor_pg::execute_pg_transition_intent(&mut tx, intent)
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }

    let cleanup_counts = if let Some(policy) = on_pg_policy {
        tracing::debug!(
            card_id,
            source,
            on_pg_policy = policy.audit_value(),
            rationale = policy.rationale(),
            "[kanban] executing allowlisted postgres cleanup after transition intents"
        );
        execute_allowed_cleanup_on_pg_tx(&mut tx, card_id, new_status, policy).await?
    } else {
        let mut counts = PgTransitionCleanupCounts::default();
        if effective.is_terminal(new_status) {
            counts.cancelled_dispatches =
                crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
                    &mut tx, card_id,
                )
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        counts
    };

    let new_state_row = sqlx::query(
        "SELECT review_status, blocked_reason
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| anyhow::anyhow!("reload postgres card state for {card_id}: {error}"))?;
    let new_review_status: Option<String> = new_state_row
        .try_get("review_status")
        .map_err(|error| anyhow::anyhow!("decode new review_status for {card_id}: {error}"))?;
    let new_blocked_reason: Option<String> = new_state_row
        .try_get("blocked_reason")
        .map_err(|error| anyhow::anyhow!("decode new blocked_reason for {card_id}: {error}"))?;

    let new_manual_intervention = crate::manual_intervention::requires_manual_intervention(
        new_review_status.as_deref(),
        new_blocked_reason.as_deref(),
    );
    if old_manual_intervention && !new_manual_intervention {
        clear_escalation_alert_state_on_pg_tx(&mut tx, card_id).await?;
    }

    tx.commit()
        .await
        .map_err(|error| anyhow::anyhow!("commit postgres transition tx: {error}"))?;

    if effective.is_terminal(new_status) {
        match cleanup_terminal_managed_worktrees_pg(pg_pool, card_id).await {
            Ok(summary) => {
                if summary.removed > 0
                    || summary.skipped_dirty > 0
                    || summary.skipped_unmerged > 0
                    || summary.skipped_unmanaged > 0
                    || summary.failed > 0
                {
                    tracing::info!(
                        "[kanban] terminal managed worktree cleanup for {}: removed={}, dirty={}, unmerged={}, unmanaged={}, failed={}",
                        card_id,
                        summary.removed,
                        summary.skipped_dirty,
                        summary.skipped_unmerged,
                        summary.skipped_unmanaged,
                        summary.failed
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    "[kanban] terminal managed worktree cleanup failed for {}: {}",
                    card_id,
                    error
                );
            }
        }
    }

    github_sync_on_transition_pg(pg_pool, &effective, card_id, new_status).await;
    fire_dynamic_hooks(
        engine,
        &effective,
        card_id,
        &old_status,
        new_status,
        Some(source),
    );

    if effective.is_terminal(new_status)
        && record_true_negative_if_pass_with_backends(db, Some(pg_pool), card_id)
    {
        crate::server::routes::review_verdict::spawn_aggregate_if_needed_with_pg(Some(
            pg_pool.clone(),
        ));
    }

    Ok((
        TransitionResult {
            changed: true,
            from: old_status,
            to: new_status.to_string(),
        },
        cleanup_counts,
    ))
}

pub async fn transition_status_with_opts_pg_only(
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> Result<TransitionResult> {
    transition_status_with_opts_pg_inner(
        None,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        None,
    )
    .await
    .map(|(result, _)| result)
}

pub async fn transition_status_with_opts_pg(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
) -> Result<TransitionResult> {
    transition_status_with_opts_pg_inner(
        db,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        None,
    )
    .await
    .map(|(result, _)| result)
}

/// #1444: run the same `ForceTransitionRevertCleanup` cleanup that
/// `transition_status_with_opts_and_allowed_cleanup_pg_only` would have
/// applied, but without going through the FSM. The route handler uses this
/// when the FSM short-circuits with `NoOp` (e.g. `force=true` ready→ready
/// recovery) so the cleanup still runs and the documented force-recovery
/// path actually clears `latest_dispatch_id`, skipped queue entries, and
/// session bindings instead of leaving them stale.
pub async fn force_transition_revert_cleanup_pg_only(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
    new_status: &str,
) -> Result<PgTransitionCleanupCounts> {
    let mut tx = pg_pool
        .begin()
        .await
        .map_err(|error| anyhow::anyhow!("begin force-transition revert cleanup tx: {error}"))?;
    let counts = execute_allowed_cleanup_on_pg_tx(
        &mut tx,
        card_id,
        new_status,
        AllowedOnConnMutation::ForceTransitionRevertCleanup,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| anyhow::anyhow!("commit force-transition revert cleanup tx: {error}"))?;
    Ok(counts)
}

pub async fn transition_status_with_opts_and_allowed_cleanup_pg_only(
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    transition_status_with_opts_pg_inner(
        None,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        Some(on_pg_policy),
    )
    .await
}

pub async fn transition_status_with_opts_and_allowed_cleanup_pg(
    db: Option<&Db>,
    pg_pool: &sqlx::PgPool,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force_intent: crate::engine::transition::ForceIntent,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<(TransitionResult, PgTransitionCleanupCounts)> {
    transition_status_with_opts_pg_inner(
        db,
        pg_pool,
        engine,
        card_id,
        new_status,
        source,
        force_intent,
        Some(on_pg_policy),
    )
    .await
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct TransitionResult {
    pub changed: bool,
    pub from: String,
    pub to: String,
}
