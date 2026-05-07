//! Postgres transition orchestration for kanban cards.

use super::github_sync::github_sync_on_transition_pg;
use super::hooks::fire_dynamic_hooks;
use super::review_tuning::record_true_negative_if_pass_with_backends;
use super::state_machine::resolve_pipeline_with_pg;
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn completed_dispatch_only_does_not_authorize_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-completed", "requested").await;
        seed_dispatch_pg(&pool, "card-completed", "completed").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-completed",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed dispatch should NOT authorize transition"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("active dispatch"),
            "error should mention active dispatch"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn pending_dispatch_authorizes_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-pending", "requested").await;
        seed_dispatch_pg(&pool, "card-pending", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pending",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "pending dispatch should authorize transition"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn dispatched_status_authorizes_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-dispatched", "requested").await;
        seed_dispatch_pg(&pool, "card-dispatched", "dispatched").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-dispatched",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "dispatched status should authorize transition"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn no_dispatch_blocks_non_free_transition_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-none", "requested").await;
        // No dispatch at all

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-none",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(result.is_err(), "no dispatch should block transition");
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn free_transition_works_without_dispatch_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-free", "backlog").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-free",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "backlog → ready should work without dispatch"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn force_overrides_dispatch_check() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force", "requested").await;
        // No dispatch, but force=true

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-force",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "force=true should bypass dispatch check");
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn stale_completed_review_verdict_does_not_open_current_done_gate() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-stale-review-pass", "review").await;

        sqlx::query(
            "UPDATE kanban_cards
             SET review_entered_at = NOW()
             WHERE id = 'card-stale-review-pass'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-stale-pass', 'card-stale-review-pass', 'agent-1', 'review', 'completed',
                'stale pass', $1::jsonb,
                NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-stale-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_err(),
            "completed review verdicts from older rounds must not satisfy the current review_passed gate"
        );

        let status: String = sqlx::query_scalar(
            "SELECT status FROM kanban_cards WHERE id = 'card-stale-review-pass'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "review",
            "stale review verdict must leave the card in review"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn legacy_review_without_review_entered_at_keeps_latest_pass_behavior() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-legacy-review-pass", "review").await;

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at, completed_at
             ) VALUES (
                'review-legacy-pass', 'card-legacy-review-pass', 'agent-1', 'review', 'completed',
                'legacy pass', $1::jsonb,
                NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
             )",
        )
        .bind(json!({"verdict": "pass"}).to_string())
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-legacy-review-pass",
            "done",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await;
        assert!(
            result.is_ok(),
            "cards without review_entered_at must preserve the legacy pass verdict behavior"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn transition_status_with_on_conn_rolls_back_on_cleanup_error_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());
        seed_card_pg(&pool, "card-force-rollback", "requested").await;
        seed_dispatch_pg(&pool, "card-force-rollback", "pending").await;

        let result = transition_status_with_opts_and_allowed_cleanup_pg_only(
            &pool,
            &engine,
            "card-force-rollback",
            "in_progress",
            "pmd",
            crate::engine::transition::ForceIntent::OperatorOverride,
            AllowedOnConnMutation::TestOnlyRollbackGuard,
        )
        .await;
        assert!(result.is_err(), "cleanup failure must abort the transition");

        let status: String =
            sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-force-rollback'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status, "requested",
            "cleanup failure must roll back the card status change"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// Regression test for #274: status transitions fire custom state hooks
    /// through try_fire_hook_by_name(), and dispatch.create() in that path must
    /// return with the dispatch row + notify outbox already materialized.
    #[tokio::test]
    async fn transition_status_custom_on_enter_hook_materializes_dispatch_outbox_pg() {
        let dir = TempDir::new().unwrap();
        let worktree_path_json =
            serde_json::to_string(dir.path().to_string_lossy().as_ref()).unwrap();
        let hook_source = r#"
            var policy = {
                name: "ready-enter-hook",
                priority: 1,
                onCustomReadyEnter: function(payload) {
                    agentdesk.dispatch.create(
                        payload.card_id,
                        "agent-1",
                        "implementation",
                        "Ready Hook Dispatch",
                        {
                            worktree_path: __WORKTREE_PATH__,
                            worktree_branch: "test-ready-hook"
                        }
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#
        .replace("__WORKTREE_PATH__", &worktree_path_json);
        std::fs::write(dir.path().join("ready-enter-hook.js"), hook_source).unwrap();

        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg_and_dir(pool.clone(), dir.path());
        seed_card_pg(&pool, "card-ready-hook", "backlog").await;

        sqlx::query("UPDATE agents SET pipeline_config = $1::jsonb WHERE id = 'agent-1'")
            .bind(
                json!({
                    "hooks": {
                        "ready": {
                            "on_enter": ["onCustomReadyEnter"],
                            "on_exit": []
                        }
                    }
                })
                .to_string(),
            )
            .execute(&pool)
            .await
            .unwrap();

        transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-ready-hook",
            "ready",
            "system",
            crate::engine::transition::ForceIntent::None,
        )
        .await
        .unwrap();

        let (dispatch_id, title): (String, String) = sqlx::query_as(
            "SELECT id, title FROM task_dispatches WHERE kanban_card_id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("custom ready on_enter hook should create a dispatch");
        assert_eq!(title, "Ready Hook Dispatch");

        let notify_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
        )
        .bind(&dispatch_id)
        .fetch_one(&pool)
        .await
        .expect("dispatch outbox query should succeed");
        assert_eq!(
            notify_count, 1,
            "custom transition hook dispatch must enqueue exactly one notify outbox row"
        );

        let (card_status, latest_dispatch_id): (String, String) = sqlx::query_as(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-ready-hook'",
        )
        .fetch_one(&pool)
        .await
        .expect("card should be updated by dispatch.create()");
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch_id);
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #110: Rust transition_status marks auto_queue_entries as done,
    /// and this single update is sufficient (no JS triple-update).
    #[tokio::test]
    async fn transition_to_done_marks_auto_queue_entry_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        // Seed cards for the queue
        seed_card_pg(&pool, "card-q1", "review").await;
        seed_card_pg(&pool, "card-q2", "ready").await;
        seed_dispatch_pg(&pool, "card-q1", "pending").await;
        let (_run_id, entry_a, _entry_b) = seed_auto_queue_run_pg(&pool, "agent-1").await;

        // Transition card-q1 to done
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-q1",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        // Verify: entry_a should be 'done' (set by Rust transition_status)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = $1")
                .bind(&entry_a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "done",
            "Rust must mark auto_queue_entry as done"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    #[tokio::test]
    async fn run_completion_waits_for_phase_gate_then_enqueues_notify_to_main_channel() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let db = test_db();
        let engine = test_engine_with_pg(pool.clone());

        seed_card_with_repo_pg(&pool, "card-notify", "review", "repo-1").await;
        seed_dispatch_pg(&pool, "card-notify", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread, unified_thread_id, thread_group_count, created_at
             )
             VALUES ($1, $2, $3, 'active', TRUE, $4::jsonb, 1, NOW())",
        )
        .bind("run-notify")
        .bind("repo-1")
        .bind("agent-1")
        .bind(r#"{"123":"thread-999"}"#)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, created_at
             )
             VALUES ($1, $2, $3, $4, 'dispatched', $5, 1, NOW())",
        )
        .bind("entry-notify")
        .bind("run-notify")
        .bind("card-notify")
        .bind("agent-1")
        .bind("dispatch-card-notify-pending")
        .execute(&pool)
        .await
        .unwrap();

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-notify",
            "done",
            "review",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await;
        assert!(result.is_ok(), "transition to done should succeed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            run_status, "paused",
            "single-phase terminal completion must pause for a phase gate"
        );

        let phase_gate_dispatch_id: String = sqlx::query_scalar(
            "SELECT id FROM task_dispatches
             WHERE kanban_card_id = 'card-notify' AND dispatch_type = 'phase-gate'
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let queued_notifications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            queued_notifications, 0,
            "completion notify must wait for the phase gate to pass"
        );

        let completed = crate::dispatch::complete_dispatch(
            &db,
            &engine,
            &phase_gate_dispatch_id,
            &json!({
                "verdict": "phase_gate_passed",
                "summary": "phase gate approved"
            }),
        )
        .expect("phase gate completion should succeed");
        assert_eq!(completed["status"], "completed");

        let run_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-notify'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(run_status, "completed");

        let (target, bot, content): (String, String, String) = sqlx::query_as(
            "SELECT target, bot, content FROM message_outbox ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(bot, "notify");
        assert!(
            content.contains("자동큐 완료: repo-1 / run run-noti / 1개"),
            "notify message should summarize the completed run"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #110: non-terminal manual recovery transitions must not complete auto-queue entries.

    /// #110: non-terminal manual recovery transitions must not complete auto-queue entries.
    #[tokio::test]
    async fn requested_force_transition_does_not_complete_auto_queue_entry_pg() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-pd", "review").await;
        seed_dispatch_pg(&pool, "card-pd", "pending").await;

        sqlx::query(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at)
             VALUES ('run-pd', 'active', 'agent-1', NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
             )
             VALUES ('entry-pd', 'run-pd', 'card-pd', 'agent-1', 'dispatched', 1, NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Transition to requested (NOT done)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-pd",
            "requested",
            "pm-gate",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        // Verify: entry should still be 'dispatched' (not done)
        let entry_status: String =
            sqlx::query_scalar("SELECT status FROM auto_queue_entries WHERE id = 'entry-pd'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            entry_status, "dispatched",
            "requested must NOT mark auto_queue_entry as done"
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// #128: started_at must reset on every in_progress re-entry (rework/resume).
    /// YAML pipeline uses `mode: coalesce` for in_progress clock, which preserves
    /// the original started_at on rework re-entry. This prevents losing the original
    /// start timestamp. Timeouts.js handles rework re-entry by checking the current
    /// dispatch's created_at rather than started_at.

    /// #128: started_at must reset on every in_progress re-entry (rework/resume).
    /// YAML pipeline uses `mode: coalesce` for in_progress clock, which preserves
    /// the original started_at on rework re-entry. This prevents losing the original
    /// start timestamp. Timeouts.js handles rework re-entry by checking the current
    /// dispatch's created_at rather than started_at.
    #[tokio::test]
    async fn started_at_coalesces_on_in_progress_reentry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-1', 'Agent 1', '123', '456')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, assigned_agent_id, started_at, created_at, updated_at
             )
             VALUES ('card-rework', 'Test', 'review', 'agent-1', NOW() - INTERVAL '3 hours', NOW(), NOW())",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Add dispatch to authorize transition
        seed_dispatch_pg(&pool, "card-rework", "pending").await;

        // Transition back to in_progress (simulates rework)
        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-rework",
            "in_progress",
            "pm-decision",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok(), "rework transition should succeed");

        // Verify started_at was PRESERVED (coalesce mode: original timestamp kept)
        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-rework'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds > 3500,
            "started_at should be preserved (coalesce mode), but was only {} seconds ago",
            age_seconds
        );
        pg_db.close_pool_and_drop(pool).await;
    }

    /// When started_at is NULL (first-time entry), coalesce mode sets it to now.
    #[tokio::test]
    async fn started_at_set_on_first_in_progress_entry() {
        let pg_db = KanbanPgDatabase::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let engine = test_engine_with_pg(pool.clone());

        seed_card_pg(&pool, "card-first", "requested").await;

        seed_dispatch_pg(&pool, "card-first", "pending").await;

        let result = transition_status_with_opts_pg_only(
            &pool,
            &engine,
            "card-first",
            "in_progress",
            "system",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await;
        assert!(result.is_ok());

        let age_seconds: i64 = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM (NOW() - started_at))::bigint
             FROM kanban_cards
             WHERE id = 'card-first'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            age_seconds < 60,
            "started_at should be set to now on first entry, but was {} seconds ago",
            age_seconds
        );
        pg_db.close_pool_and_drop(pool).await;
    }
}
