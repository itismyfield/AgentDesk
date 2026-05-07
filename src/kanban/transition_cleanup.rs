//! Cleanup policies and transactional side effects for kanban transitions.

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::terminal_cleanup::strip_stale_worktree_metadata_from_dispatches_on_conn;
use super::terminal_cleanup::strip_stale_worktree_metadata_from_dispatches_on_pg_tx;
use anyhow::Result;
use serde_json::json;
use sqlx::Row as SqlxRow;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PgTransitionCleanupCounts {
    pub cancelled_dispatches: usize,
    pub skipped_auto_queue_entries: usize,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedOnConnMutation {
    ForceTransitionRevertCleanup,
    ForceTransitionTerminalCleanup,
    TestOnlyRollbackGuard,
    TestOnlyManualInterventionCleanup,
}

impl AllowedOnConnMutation {
    pub(super) fn audit_value(self) -> &'static str {
        match self {
            Self::ForceTransitionRevertCleanup => "force_transition_revert_cleanup",
            Self::ForceTransitionTerminalCleanup => "force_transition_terminal_cleanup",
            Self::TestOnlyRollbackGuard => "test_only_rollback_guard",
            Self::TestOnlyManualInterventionCleanup => "test_only_manual_intervention_cleanup",
        }
    }

    pub(super) fn rationale(self) -> &'static str {
        match self {
            Self::ForceTransitionRevertCleanup => {
                "same transaction required to clear review and dispatch residue while rewinding status"
            }
            Self::ForceTransitionTerminalCleanup => {
                "same transaction required to cancel stale dispatches before terminal status commits"
            }
            Self::TestOnlyRollbackGuard => {
                "test-only rollback probe for transition + cleanup atomicity"
            }
            Self::TestOnlyManualInterventionCleanup => {
                "test-only cleanup for escalation-cooldown clearing assertions"
            }
        }
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_escalation_alert_state_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "DELETE FROM kv_meta WHERE key IN (?1, ?2)",
        sqlite_test::params![
            format!("pm_pending:{card_id}"),
            format!("pm_decision_sent:{card_id}")
        ],
    )?;
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn cleanup_force_transition_revert_fields_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE kanban_cards \
         SET latest_dispatch_id = NULL, review_status = NULL, \
             review_round = 0, review_notes = NULL, suggestion_pending_at = NULL, \
             review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, \
             updated_at = datetime('now') \
         WHERE id = ?1",
        [card_id],
    )?;
    conn.execute(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            ?1, 0, 'idle', NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, datetime('now')
         )
         ON CONFLICT(card_id) DO UPDATE SET
            review_round = 0,
            state = 'idle',
            pending_dispatch_id = NULL,
            last_verdict = NULL,
            last_decision = NULL,
            decided_by = NULL,
            decided_at = NULL,
            approach_change_round = NULL,
            session_reset_round = NULL,
            review_entered_at = NULL,
            updated_at = datetime('now')",
        [card_id],
    )?;
    clear_escalation_alert_state_on_conn(conn, card_id)?;
    strip_stale_worktree_metadata_from_dispatches_on_conn(conn, card_id)?;
    Ok(())
}

pub(super) async fn clear_escalation_alert_state_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM kv_meta WHERE key = ANY($1)")
        .bind(vec![
            format!("pm_pending:{card_id}"),
            format!("pm_decision_sent:{card_id}"),
        ])
        .execute(&mut **tx)
        .await
        .map_err(|error| {
            anyhow::anyhow!("clear postgres escalation state for {card_id}: {error}")
        })?;
    Ok(())
}

async fn skip_live_auto_queue_entries_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<usize> {
    let rows = sqlx::query(
        "SELECT id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres auto-queue entries for {card_id}: {error}"))?;

    let mut changed = 0usize;
    for row in rows {
        let entry_id: String = row.try_get("id").map_err(|error| {
            anyhow::anyhow!("decode postgres auto-queue entry for {card_id}: {error}")
        })?;
        let result = crate::db::auto_queue::update_entry_status_on_pg_tx(
            tx,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("skip postgres auto-queue entry {entry_id}: {error}"))?;
        if result.changed {
            changed += 1;
        }
    }

    Ok(changed)
}

#[allow(dead_code)]
async fn count_live_auto_queue_entries_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<usize> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("count postgres live auto-queue entries for {card_id}: {error}")
    })?;
    Ok(count.max(0) as usize)
}

async fn clear_force_transition_terminalized_links_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE auto_queue_entries
         SET dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = COALESCE(completed_at, NOW())
         WHERE kanban_card_id = $1
           AND status = 'skipped'
           AND dispatch_id IS NOT NULL
           AND run_id IN (
               SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
           )",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "clear postgres force-transition terminalized auto-queue links for {card_id}: {error}"
        )
    })?;
    Ok(())
}

async fn cancel_active_dispatches_for_card_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<PgTransitionCleanupCounts> {
    let rows = sqlx::query(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres live dispatches for {card_id}: {error}"))?;
    let dispatch_ids: Vec<String> = rows
        .into_iter()
        .map(|row| row.try_get("id"))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            anyhow::anyhow!("decode postgres live dispatch id for {card_id}: {error}")
        })?;

    if dispatch_ids.is_empty() {
        return Ok(PgTransitionCleanupCounts::default());
    }

    sqlx::query(
        "UPDATE sessions
         SET status = CASE WHEN status IN ('turn_active', 'working') THEN 'idle' ELSE status END,
             active_dispatch_id = NULL
         WHERE active_dispatch_id = ANY($1)",
    )
    .bind(&dispatch_ids)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("clear postgres live session dispatches for {card_id}: {error}")
    })?;

    let cancel_payload = reason
        .map(|value| json!({ "reason": value, "completion_source": "force_transition" }))
        .unwrap_or_else(|| json!({ "completion_source": "force_transition" }));
    let mut counts = PgTransitionCleanupCounts::default();
    for dispatch_id in dispatch_ids {
        let rows_affected = sqlx::query(
            "UPDATE task_dispatches
             SET status = 'cancelled',
                 updated_at = NOW(),
                 completed_at = COALESCE(completed_at, NOW()),
                 result = COALESCE(result, CAST($2 AS jsonb)::text)
             WHERE id = $1
               AND status IN ('pending', 'dispatched')",
        )
        .bind(&dispatch_id)
        .bind(cancel_payload.to_string())
        .execute(&mut **tx)
        .await
        .map_err(|error| anyhow::anyhow!("cancel postgres dispatch {dispatch_id}: {error}"))?
        .rows_affected();
        counts.cancelled_dispatches += rows_affected as usize;

        // Route the force-skip through the shared entry transition helper so
        // PG bookkeeping mirrors SQLite: transition rows are recorded and
        // single-entry runs can finalize. Preserve the dispatch link afterward
        // for abandoned-dispatch lineage.
        counts.skipped_auto_queue_entries += crate::db::auto_queue::sync_dispatch_terminal_entries_on_pg_tx(
            tx,
            &dispatch_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            true,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "mark postgres live auto-queue entry skipped during force-transition cancel {dispatch_id}: {error}"
            )
        })?;
    }

    Ok(counts)
}

async fn cleanup_force_transition_revert_fields_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
) -> anyhow::Result<()> {
    use crate::engine::transition::TransitionIntent;

    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &TransitionIntent::SetLatestDispatchId {
            card_id: card_id.to_string(),
            dispatch_id: None,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    crate::engine::transition_executor_pg::execute_pg_transition_intent(
        tx,
        &TransitionIntent::SetReviewStatus {
            card_id: card_id.to_string(),
            review_status: None,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;

    sqlx::query(
        "UPDATE kanban_cards
         SET review_round = 0,
             review_notes = NULL,
             suggestion_pending_at = NULL,
             review_entered_at = NULL,
             awaiting_dod_at = NULL,
             blocked_reason = NULL,
             updated_at = NOW()
         WHERE id = $1",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| {
        anyhow::anyhow!("reset postgres kanban cleanup fields for {card_id}: {error}")
    })?;

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            $1, 0, 'idle', NULL, NULL, NULL,
            NULL, NULL, NULL, NULL, NULL, NOW()
         )
         ON CONFLICT(card_id) DO UPDATE SET
            review_round = 0,
            state = 'idle',
            pending_dispatch_id = NULL,
            last_verdict = NULL,
            last_decision = NULL,
            decided_by = NULL,
            decided_at = NULL,
            approach_change_round = NULL,
            session_reset_round = NULL,
            review_entered_at = NULL,
            updated_at = NOW()",
    )
    .bind(card_id)
    .execute(&mut **tx)
    .await
    .map_err(|error| anyhow::anyhow!("reset postgres review state for {card_id}: {error}"))?;

    clear_escalation_alert_state_on_pg_tx(tx, card_id).await?;
    strip_stale_worktree_metadata_from_dispatches_on_pg_tx(tx, card_id).await?;
    Ok(())
}

pub(super) async fn execute_allowed_cleanup_on_pg_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    card_id: &str,
    new_status: &str,
    on_pg_policy: AllowedOnConnMutation,
) -> Result<PgTransitionCleanupCounts> {
    let mut counts = PgTransitionCleanupCounts::default();

    match on_pg_policy {
        AllowedOnConnMutation::ForceTransitionRevertCleanup => {
            let reason = format!("force-transition to {new_status}");
            // Model 2: generic cancel keeps the dispatch pointer for
            // provenance. Force-transition cleanup is the explicit terminal
            // cleanup path, so it preserves the detailed cancel bookkeeping
            // and then clears any skipped links that cancel's side-effect left.
            let cancelled_counts =
                cancel_active_dispatches_for_card_on_pg_tx(tx, card_id, Some(&reason)).await?;
            counts.cancelled_dispatches = cancelled_counts.cancelled_dispatches;
            counts.skipped_auto_queue_entries = cancelled_counts.skipped_auto_queue_entries;
            counts.skipped_auto_queue_entries +=
                skip_live_auto_queue_entries_for_card_on_pg_tx(tx, card_id).await?;
            clear_force_transition_terminalized_links_on_pg_tx(tx, card_id).await?;
            cleanup_force_transition_revert_fields_on_pg_tx(tx, card_id).await?;
        }
        AllowedOnConnMutation::ForceTransitionTerminalCleanup => {
            counts.cancelled_dispatches =
                crate::engine::transition_executor_pg::cancel_live_dispatches_for_terminal_card_pg(
                    tx, card_id,
                )
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        AllowedOnConnMutation::TestOnlyRollbackGuard => {
            return Err(anyhow::anyhow!("cleanup failed"));
        }
        AllowedOnConnMutation::TestOnlyManualInterventionCleanup => {
            clear_escalation_alert_state_on_pg_tx(tx, card_id).await?;
        }
    }

    Ok(counts)
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;

    /// #800: `reset_full=true` reopens must scrub recorded worktree metadata
    /// from `task_dispatches.context` / `task_dispatches.result` so a follow-up
    /// `latest_completed_work_dispatch_target` call cannot silently re-inject
    /// the stale path into the new dispatch context.
    #[test]
    fn cleanup_force_transition_revert_fields_strips_dispatch_worktree_metadata() {
        let db = test_db();
        seed_card(&db, "card-800-strip-wt", "in_progress");

        let conn = db.lock().unwrap();
        // Two dispatches on the same card, one completed implementation that
        // recorded both context-side and result-side wt metadata, plus a
        // pending dispatch with only context-side wt metadata. We assert that
        // ALL wt-locating keys are removed but unrelated fields survive.
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-completed', 'card-800-strip-wt', 'agent-1', 'implementation', 'completed',
                'Old impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-stale",
                    "worktree_branch": "wt/800-old",
                    "preserve_me": "context_value"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-stale",
                    "completed_branch": "wt/800-old",
                    "completed_commit": "deadbeefcafebabe",
                    "preserve_me_too": "result_value"
                })
                .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-pending', 'card-800-strip-wt', 'agent-1', 'implementation', 'pending',
                'New impl', ?1, NULL, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-also-stale",
                    "worktree_branch": "wt/800-also-old",
                    "title_hint": "redispatch"
                })
                .to_string(),
            ],
        )
        .unwrap();
        // A second card's dispatch must be untouched by the scoped cleanup.
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-800-other', 'Other', 'in_progress', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
             ) VALUES (
                'd-800-other-card', 'card-800-other', 'agent-1', 'implementation', 'completed',
                'Other impl', ?1, ?2, datetime('now'), datetime('now')
             )",
            sqlite_test::params![
                serde_json::json!({
                    "worktree_path": "/tmp/agentdesk-800-other-keep",
                    "worktree_branch": "wt/800-other-keep"
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": "/tmp/agentdesk-800-other-keep",
                    "completed_branch": "wt/800-other-keep"
                })
                .to_string(),
            ],
        )
        .unwrap();

        cleanup_force_transition_revert_fields_on_conn(&conn, "card-800-strip-wt").unwrap();

        // Helper to read a dispatch JSON column back as a serde value.
        let load_json = |dispatch_id: &str, column: &str| -> Option<serde_json::Value> {
            let raw: Option<String> = conn
                .query_row(
                    &format!("SELECT {column} FROM task_dispatches WHERE id = ?1"),
                    [dispatch_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();
            raw.and_then(|s| serde_json::from_str(&s).ok())
        };

        let completed_ctx = load_json("d-800-completed", "context").unwrap();
        assert!(
            completed_ctx.get("worktree_path").is_none(),
            "context.worktree_path must be removed, got {completed_ctx:?}"
        );
        assert!(
            completed_ctx.get("worktree_branch").is_none(),
            "context.worktree_branch must be removed, got {completed_ctx:?}"
        );
        assert_eq!(
            completed_ctx["preserve_me"].as_str(),
            Some("context_value"),
            "unrelated context fields must be preserved"
        );

        let completed_result = load_json("d-800-completed", "result").unwrap();
        assert!(
            completed_result.get("completed_worktree_path").is_none(),
            "result.completed_worktree_path must be removed, got {completed_result:?}"
        );
        assert!(
            completed_result.get("completed_branch").is_none(),
            "result.completed_branch must be removed, got {completed_result:?}"
        );
        assert_eq!(
            completed_result["completed_commit"].as_str(),
            Some("deadbeefcafebabe"),
            "completion evidence (completed_commit) must be preserved as audit history"
        );
        assert_eq!(
            completed_result["preserve_me_too"].as_str(),
            Some("result_value")
        );

        let pending_ctx = load_json("d-800-pending", "context").unwrap();
        assert!(pending_ctx.get("worktree_path").is_none());
        assert!(pending_ctx.get("worktree_branch").is_none());
        assert_eq!(pending_ctx["title_hint"].as_str(), Some("redispatch"));

        // Other card untouched — both wt-locating keys must still be present.
        let other_ctx = load_json("d-800-other-card", "context").unwrap();
        assert_eq!(
            other_ctx["worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep"),
            "cleanup must be card-scoped and not touch unrelated cards"
        );
        let other_result = load_json("d-800-other-card", "result").unwrap();
        assert_eq!(
            other_result["completed_worktree_path"].as_str(),
            Some("/tmp/agentdesk-800-other-keep")
        );
    }
}
