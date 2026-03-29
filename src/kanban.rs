//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through `transition_status()`.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.
//!
//! ## Pipeline-Driven Transitions (#106 P5)
//!
//! All transition rules, gates, hooks, clocks, and timeouts are defined in
//! `policies/default-pipeline.yaml`. No hardcoded state names exist in this module.
//! See the YAML file for the complete state machine specification.
//!
//! Custom pipelines can override the default via repo or agent-level overrides
//! (3-level inheritance: default → repo → agent).

use crate::db::Db;
use crate::engine::PolicyEngine;
use anyhow::Result;
use serde_json::json;

/// Transition a kanban card to a new status.
///
/// This is the ONLY correct way to change a card's status.
/// It handles:
/// 1. Dispatch validation (C: dispatch required for non-free transitions)
/// 2. DB UPDATE with appropriate timestamp fields
/// 3. Audit logging (D: all transitions logged)
/// 4. OnCardTransition hook
/// 5. OnReviewEnter hook (when → review)
/// 6. OnCardTerminal hook (when → done)
/// 7. auto_queue_entries sync (when → done)
///
/// `source`: who initiated the transition (e.g., "api", "policy", "pmd")
/// `force`: PMD-only override to bypass dispatch validation
pub fn transition_status(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
) -> Result<TransitionResult> {
    transition_status_with_opts(db, engine, card_id, new_status, "system", false)
}

/// Full transition with source tracking and force override.
///
/// #155: Uses the TransitionDecision + Executor pattern.
/// 1. Assemble TransitionContext from DB
/// 2. Call decide_status_transition (pure function — no I/O)
/// 3. Execute decision intents via Executor
/// 4. Fire post-transition hooks (GitHub sync, policy hooks)
pub fn transition_status_with_opts(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force: bool,
) -> Result<TransitionResult> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    // ── 1. Assemble TransitionContext (DB reads) ──
    let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (old_status, review_status, latest_dispatch_id, card_repo_id, card_agent_id): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, review_status, latest_dispatch_id, repo_id, assigned_agent_id \
             FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(|_| anyhow::anyhow!("card not found: {card_id}"))?;

    if old_status == new_status {
        return Ok(TransitionResult {
            changed: false,
            from: old_status,
            to: new_status.to_string(),
        });
    }

    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());

    // Pre-evaluate gate checks (DB queries done before calling pure function)
    let has_active_dispatch: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
            [card_id],
            |row| row.get(0),
        )
        .unwrap_or(false);

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.clone(),
            review_status,
            latest_dispatch_id,
        },
        pipeline: effective.clone(),
        gates: GateSnapshot {
            has_active_dispatch,
        },
    };

    // ── 2. Pure decision (no I/O) ──
    let decision = transition::decide_status_transition(&ctx, new_status, source, force);

    // ── 3. Handle blocked decisions ──
    if let TransitionOutcome::Blocked(ref reason) = decision.outcome {
        // Execute audit log intents for blocked decisions
        for intent in &decision.intents {
            if let transition::TransitionIntent::AuditLog {
                card_id: aid,
                from,
                to,
                source: src,
                message,
            } = intent
            {
                log_audit(&conn, aid, from, to, src, message);
            }
        }
        tracing::warn!(
            "[kanban] Blocked transition {} → {} for card {} (source: {}): {}",
            old_status,
            new_status,
            card_id,
            source,
            reason
        );
        notify_pmd_violation(&conn, card_id, &old_status, new_status, source, reason);
        return Err(anyhow::anyhow!("{}", reason));
    }

    if decision.outcome == TransitionOutcome::NoOp {
        return Ok(TransitionResult {
            changed: false,
            from: old_status,
            to: new_status.to_string(),
        });
    }

    // ── 4. Execute intents atomically (DB writes, still holding lock) ──
    conn.execute_batch("BEGIN")?;
    let exec_result = (|| -> anyhow::Result<()> {
        for intent in &decision.intents {
            transition::execute_intent_on_conn(&conn, intent)?;
        }
        Ok(())
    })();
    if let Err(e) = exec_result {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    conn.execute_batch("COMMIT")?;

    drop(conn);

    // ── 5. Post-transition side-effects (hooks, GitHub sync) ──
    github_sync_on_transition(db, &effective, card_id, new_status);
    fire_dynamic_hooks(engine, &effective, card_id, &old_status, new_status);

    // Drain intents pushed by hooks (e.g. OnReviewEnter → dispatch.create).
    // Without this, dispatch_outbox notifications are never queued and the
    // dispatch sits pending until the 2-min recovery sweep picks it up.
    drain_hook_side_effects(db, engine);

    if effective.is_terminal(new_status) && record_true_negative_if_pass(db, card_id) {
        crate::server::routes::review_verdict::spawn_aggregate_if_needed(db);
    }

    Ok(TransitionResult {
        changed: true,
        from: old_status,
        to: new_status.to_string(),
    })
}

/// Transition a card through the full reducer (decision → intents → execute)
/// but skip post-transition side-effects (policy hooks, GitHub sync).
///
/// Used by auto-queue's silent free-walk to move non-dispatchable cards
/// (e.g. backlog) to a dispatchable state without firing kanban-rules hooks
/// that would create side-dispatches.
///
/// Unlike raw SQL, this preserves all canonical invariants:
/// ApplyClock, AuditLog, SyncReviewState, SyncAutoQueue.
pub fn transition_status_no_hooks(
    db: &Db,
    card_id: &str,
    new_status: &str,
    source: &str,
) -> Result<TransitionResult> {
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (old_status, review_status, latest_dispatch_id, card_repo_id, card_agent_id): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, review_status, latest_dispatch_id, repo_id, assigned_agent_id \
             FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .map_err(|_| anyhow::anyhow!("card not found: {card_id}"))?;

    if old_status == new_status {
        return Ok(TransitionResult {
            changed: false,
            from: old_status,
            to: new_status.to_string(),
        });
    }

    crate::pipeline::ensure_loaded();
    let effective =
        crate::pipeline::resolve_for_card(&conn, card_repo_id.as_deref(), card_agent_id.as_deref());

    let has_active_dispatch: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND status IN ('pending', 'dispatched')",
            [card_id],
            |row| row.get(0),
        )
        .unwrap_or(false);

    let ctx = TransitionContext {
        card: CardState {
            id: card_id.to_string(),
            status: old_status.clone(),
            review_status,
            latest_dispatch_id,
        },
        pipeline: effective,
        gates: GateSnapshot {
            has_active_dispatch,
        },
    };

    let decision = transition::decide_status_transition(&ctx, new_status, source, true);

    if let TransitionOutcome::Blocked(ref reason) = decision.outcome {
        return Err(anyhow::anyhow!("{}", reason));
    }
    if decision.outcome == TransitionOutcome::NoOp {
        return Ok(TransitionResult {
            changed: false,
            from: old_status,
            to: new_status.to_string(),
        });
    }

    conn.execute_batch("BEGIN")?;
    let exec_result = (|| -> anyhow::Result<()> {
        for intent in &decision.intents {
            transition::execute_intent_on_conn(&conn, intent)?;
        }
        Ok(())
    })();
    if let Err(e) = exec_result {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    conn.execute_batch("COMMIT")?;

    // Intentionally skip: fire_dynamic_hooks, github_sync, drain_hook_side_effects

    Ok(TransitionResult {
        changed: true,
        from: old_status,
        to: new_status.to_string(),
    })
}

#[derive(Debug)]
pub struct TransitionResult {
    pub changed: bool,
    pub from: String,
    pub to: String,
}

/// Fire hooks dynamically based on the effective pipeline's hooks section (#106 P5).
///
/// All hook bindings come from the YAML pipeline definition.
/// States without hook bindings simply fire no hooks.
fn fire_dynamic_hooks(
    engine: &PolicyEngine,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    old_status: &str,
    new_status: &str,
) {
    let payload = json!({
        "card_id": card_id,
        "from": old_status,
        "to": new_status,
        "status": new_status,
    });

    // Fire on_exit hooks for the state being LEFT
    if let Some(bindings) = pipeline.hooks_for_state(old_status) {
        for hook_name in &bindings.on_exit {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // Fire on_enter hooks for the state being ENTERED
    if let Some(bindings) = pipeline.hooks_for_state(new_status) {
        for hook_name in &bindings.on_enter {
            let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
        }
    }
    // No fallback — YAML is the sole source of truth for hook bindings.
}

/// Drain deferred side-effects produced while hooks were executing.
///
/// Hooks cannot re-enter the engine, so transition requests and dispatch
/// creations are accumulated for post-hook replay.
pub fn drain_hook_side_effects(db: &Db, engine: &PolicyEngine) {
    loop {
        let intent_result = engine.drain_pending_intents();
        let mut transitions = intent_result.transitions;
        transitions.extend(engine.drain_pending_transitions());

        // #108: Immediately notify dispatches created by JS policy intents.
        // Without this, dispatches created outside fire_transition_hooks
        // (e.g. timeouts.js, review-automation.js) would sit pending until
        // the [I-0] 2min recovery sweep picked them up.
        for cd in &intent_result.created_dispatches {
            let title: String = db
                .separate_conn()
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT title FROM kanban_cards WHERE id = ?1",
                        [&cd.card_id],
                        |row| row.get(0),
                    )
                    .ok()
                })
                .unwrap_or_default();
            crate::server::routes::dispatches::queue_dispatch_notify(
                db,
                &cd.dispatch_id,
                &cd.agent_id,
                &cd.card_id,
                &title,
            );
        }

        if transitions.is_empty() {
            break;
        }

        for (card_id, old_status, new_status) in &transitions {
            fire_transition_hooks(db, engine, card_id, old_status, new_status);
        }
    }
}

/// Fire pipeline-defined event hooks for a lifecycle event (#134).
///
/// Looks up the `events` section of the effective pipeline and fires each
/// hook name via `try_fire_hook_by_name`. Falls back to firing the default
/// hook name if no pipeline config or no event binding is found.
pub fn fire_event_hooks(
    db: &Db,
    engine: &PolicyEngine,
    event: &str,
    default_hook: &str,
    payload: serde_json::Value,
) {
    crate::pipeline::ensure_loaded();
    let hooks: Vec<String> = crate::pipeline::try_get()
        .and_then(|p| p.event_hooks(event).cloned())
        .unwrap_or_else(|| vec![default_hook.to_string()]);
    for hook_name in &hooks {
        let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
    }
    // Event hook callers already own transition draining; only materialize
    // deferred dispatch intents here so follow-up notification queries can see them.
    let _ = db;
    let _ = engine.drain_pending_intents();
}

/// Fire only the pipeline-defined on_enter/on_exit hooks for a transition.
///
/// Unlike `fire_transition_hooks`, this does NOT perform side-effects
/// (audit log, GitHub sync, terminal-state sync, dispatch notifications).
/// Use this when callers already handle those concerns separately
/// (e.g. dispatch creation, route handlers).
pub fn fire_state_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    if from == to {
        return;
    }
    crate::pipeline::ensure_loaded();
    let effective = db.lock().ok().map(|conn| {
        let repo_id: Option<String> = conn
            .query_row(
                "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref())
    });
    if let Some(ref pipeline) = effective {
        fire_dynamic_hooks(engine, pipeline, card_id, from, to);
    }
    drain_hook_side_effects(db, engine);
}

/// Fire only the on_enter hooks for a specific state, without requiring a transition.
///
/// Used when re-entering the same state (e.g., restarting review from awaiting_dod)
/// where `fire_state_hooks` would no-op because from == to.
///
/// #108: Also detects dispatches created by on_enter hooks (e.g., OnReviewEnter
/// creating a review dispatch during dispute) and queues Discord notifications.
/// Without this, callers had to manually re-query the DB to find new dispatches,
/// which introduced race conditions.
pub fn fire_enter_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, state: &str) {
    // Capture pre-hook dispatch ID to detect new dispatches created by hooks (#108)
    let pre_dispatch_id: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    crate::pipeline::ensure_loaded();
    let effective = db.lock().ok().map(|conn| {
        let repo_id: Option<String> = conn
            .query_row(
                "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref())
    });
    if let Some(ref pipeline) = effective {
        if let Some(bindings) = pipeline.hooks_for_state(state) {
            let payload = json!({
                "card_id": card_id,
                "from": state,
                "to": state,
                "status": state,
            });
            for hook_name in &bindings.on_enter {
                let _ = engine.try_fire_hook_by_name(hook_name, payload.clone());
            }
        }
    }
    drain_hook_side_effects(db, engine);

    // #108: After all hooks, check if new dispatches were created and queue
    // Discord notifications. Same pattern as fire_transition_hooks.
    notify_new_dispatches_after_hooks(db, card_id, pre_dispatch_id.as_deref());
}

/// Fire hooks for a status transition that already happened in the DB.
/// Use this when the DB UPDATE was done elsewhere (e.g., update_card with mixed fields).
pub fn fire_transition_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, from: &str, to: &str) {
    if from == to {
        return;
    }

    // Audit log
    if let Ok(conn) = db.lock() {
        log_audit(&conn, card_id, from, to, "hook", "OK");
    }

    // Capture pre-hook dispatch ID to detect new dispatches created by hooks
    let pre_dispatch_id: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    // Resolve effective pipeline for this card (#135)
    crate::pipeline::ensure_loaded();
    let effective = db.lock().ok().map(|conn| {
        let repo_id: Option<String> = conn
            .query_row(
                "SELECT repo_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let agent_id: Option<String> = conn
            .query_row(
                "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                [card_id],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref())
    });

    if let Some(ref pipeline) = effective {
        // Sync auto_queue_entries + GitHub on terminal status
        if pipeline.is_terminal(to) {
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
                     WHERE kanban_card_id = ?1 AND status = 'dispatched'",
                    [card_id],
                )
                .ok();
            }
        }

        github_sync_on_transition(db, pipeline, card_id, to);
        fire_dynamic_hooks(engine, pipeline, card_id, from, to);

        // #119: Record true_negative for cards that passed review and reached terminal state
        if pipeline.is_terminal(to) && record_true_negative_if_pass(db, card_id) {
            crate::server::routes::review_verdict::spawn_aggregate_if_needed(db);
        }
    }

    drain_hook_side_effects(db, engine);

    // After all hooks, check if a new dispatch was created (by onCardTerminal, onReviewEnter, etc.)
    // and send Discord notification. This handles auto-queue's next dispatch creation.
    notify_new_dispatches_after_hooks(db, card_id, pre_dispatch_id.as_deref());
}

/// Check if hooks created new dispatches and send Discord notifications.
///
/// Uses rowid-based ordering to find dispatches created during hook execution.
/// Cross-card misroute is prevented by using each dispatch's own kanban_card_id
/// for routing (not the triggering card's ID). The card_id filter is intentionally
/// NOT applied because hooks (e.g. OnCardTerminal → auto-queue) legitimately
/// create dispatches for OTHER cards.
fn notify_new_dispatches_after_hooks(db: &Db, card_id: &str, pre_dispatch_id: Option<&str>) {
    // Query ALL pending dispatches inserted after the pre-hook snapshot (by rowid).
    // Uses rowid comparison instead of timestamp — SQLite datetime('now') has only
    // second-level resolution, so dispatches created in the same second would be missed.
    // Rowid is monotonically increasing and survives same-second inserts.
    //
    // No card_id filter: hooks like OnCardTerminal can create dispatches for different
    // cards (e.g. auto-queue dispatching the next ready card). Each dispatch's own
    // kanban_card_id is used for Discord routing below, preventing cross-card misroute.
    let pending_dispatches: Vec<(String, String, String, String)> = db
        .lock()
        .ok()
        .map(|conn| {
            if let Some(pre_id) = pre_dispatch_id {
                // Find any pending dispatches inserted after the pre-hook dispatch (by rowid)
                let mut stmt = conn
                    .prepare(
                        "SELECT td.id, td.to_agent_id, td.kanban_card_id, kc.title \
                         FROM task_dispatches td \
                         JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                         WHERE td.status = 'pending' \
                           AND td.rowid > (SELECT rowid FROM task_dispatches WHERE id = ?1)",
                    )
                    .ok();
                stmt.as_mut()
                    .and_then(|s| {
                        s.query_map(rusqlite::params![pre_id], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        })
                        .ok()
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            } else {
                // No pre-hook dispatch — find any pending dispatch for this card
                // that matches the current latest_dispatch_id
                let latest_id: Option<String> = conn
                    .query_row(
                        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();
                let Some(lid) = latest_id else {
                    return Vec::new();
                };
                let mut stmt = conn
                    .prepare(
                        "SELECT td.id, td.to_agent_id, td.kanban_card_id, kc.title \
                         FROM task_dispatches td \
                         JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                         WHERE td.id = ?1 AND td.status = 'pending'",
                    )
                    .ok();
                stmt.as_mut()
                    .and_then(|s| {
                        s.query_map([&lid], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                        })
                        .ok()
                    })
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            }
        })
        .unwrap_or_default();

    if pending_dispatches.is_empty() {
        return;
    }

    // #144: Queue via dispatch outbox instead of tokio::spawn.
    // Each dispatch uses its own kanban_card_id for correct thread/issue routing.
    for (dispatch_id, agent_id, dispatch_card_id, title) in pending_dispatches {
        crate::server::routes::dispatches::queue_dispatch_notify(
            db,
            &dispatch_id,
            &agent_id,
            &dispatch_card_id,
            &title,
        );
    }
}

/// Sync GitHub issue state when kanban card transitions (pipeline-driven).
/// Terminal states → close issue. States with OnReviewEnter hook → comment.
fn github_sync_on_transition(
    db: &Db,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let info: Option<(String, Option<i64>)> = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT COALESCE(github_issue_url, ''), github_issue_number FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .ok()
        });

    let Some((issue_url, issue_number)) = info else {
        return;
    };
    if issue_url.is_empty() {
        return;
    }

    let repo = match issue_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.find("/issues/").map(|i| &s[..i]))
    {
        Some(r) => r.to_string(),
        None => return,
    };
    let Some(num) = issue_number else { return };

    if is_terminal {
        let _ = std::process::Command::new("gh")
            .args(["issue", "close", &num.to_string(), "--repo", &repo])
            .output();
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = std::process::Command::new("gh")
            .args([
                "issue",
                "comment",
                &num.to_string(),
                "--repo",
                &repo,
                "--body",
                comment,
            ])
            .output();
    }
}

/// Send a violation alert to the PMD/kanban-manager channel via announce bot.
fn notify_pmd_violation(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    reason: &str,
) {
    // Look up card title for the notification
    let title: String = conn
        .query_row(
            "SELECT COALESCE(title, id) FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| card_id.to_string());

    // Read kanban_manager_channel_id from kv_meta
    let km_channel: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(km_channel) = km_channel else {
        tracing::debug!(
            "[kanban] No kanban_manager_channel_id configured, skipping violation alert"
        );
        return;
    };
    let Some(channel_num) = km_channel.parse::<u64>().ok() else {
        tracing::warn!("[kanban] Invalid kanban_manager_channel_id: {km_channel}");
        return;
    };

    let message = format!(
        "⚠️ **칸반 위반 감지**\n\n\
         카드: {title}\n\
         시도: {from} → {to}\n\
         차단 사유: {reason}\n\
         호출자: {source}\n\
         카드 ID: {card_id}"
    );

    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            tracing::debug!("[kanban] No announce bot token, skipping violation alert");
            return;
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let client = reqwest::Client::new();
            let _ = client
                .post(format!(
                    "https://discord.com/api/v10/channels/{channel_num}/messages"
                ))
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await;
        });
    }
}

/// Log a kanban state transition to audit_logs table.
fn log_audit(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![card_id, from, to, source, result],
    )
    .ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', ?1, ?2, ?3)",
        rusqlite::params![card_id, format!("{from}->{to} ({result})"), source],
    )
    .ok();
}

/// #119: When a card reaches done after a review pass verdict, record a true_negative
/// tuning outcome. This confirms the review was correct in not finding issues.
/// Returns true if a TN was actually inserted.
fn record_true_negative_if_pass(db: &Db, card_id: &str) -> bool {
    if let Ok(conn) = db.lock() {
        // Check if the card's last review verdict was "pass" or "approved"
        let last_verdict: Option<String> = conn
            .query_row(
                "SELECT last_verdict FROM card_review_state WHERE card_id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        match last_verdict.as_deref() {
            Some("pass") | Some("approved") => {
                let review_round: Option<i64> = conn
                    .query_row(
                        "SELECT review_round FROM card_review_state WHERE card_id = ?1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .ok();

                // Carry forward finding_categories from the review dispatch that found issues.
                // The most recent review dispatch is typically the pass/approved one with
                // empty items, so we walk backwards to find one with actual findings.
                // This ensures that if TN is later corrected to FN on reopen, categories
                // are already present.
                let finding_cats: Option<String> = conn
                    .prepare(
                        "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' ORDER BY td.rowid DESC",
                    )
                    .ok()
                    .and_then(|mut stmt| {
                        let rows = stmt
                            .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                            .ok()?;
                        for row_result in rows {
                            if let Ok(Some(result_str)) = row_result {
                                if let Ok(v) =
                                    serde_json::from_str::<serde_json::Value>(&result_str)
                                {
                                    if let Some(items) = v["items"].as_array() {
                                        let cats: Vec<String> = items
                                            .iter()
                                            .filter_map(|it| {
                                                it["category"].as_str().map(|s| s.to_string())
                                            })
                                            .collect();
                                        if !cats.is_empty() {
                                            return serde_json::to_string(&cats).ok();
                                        }
                                    }
                                }
                            }
                        }
                        None
                    });

                let inserted = conn.execute(
                    "INSERT INTO review_tuning_outcomes \
                     (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) \
                     VALUES (?1, NULL, ?2, ?3, 'done', 'true_negative', ?4)",
                    rusqlite::params![card_id, review_round, last_verdict.as_deref().unwrap_or("pass"), finding_cats],
                )
                .map(|n| n > 0)
                .unwrap_or(false);
                if inserted {
                    tracing::info!(
                        "[review-tuning] #119 recorded true_negative: card={card_id} (pass → done)"
                    );
                }
                return inserted;
            }
            _ => {} // No review or non-pass verdict — nothing to record
        }
    }
    false
}

/// #119: When a card is reopened after reaching done with a pass verdict,
/// correct any true_negative outcomes to false_negative — the review missed a real bug.
///
/// Also backfills finding_categories if the TN record had empty categories.
/// TN is typically recorded using categories from the last completed review dispatch,
/// which is the pass/approved dispatch with empty items. On reopen we look for the
/// most recent review dispatch that actually reported findings (non-empty items array)
/// to carry those categories forward into the FN record.
pub fn correct_tn_to_fn_on_reopen(db: &Db, card_id: &str) {
    if let Ok(conn) = db.lock() {
        // Only correct the most recent TN (latest review_round) to avoid
        // corrupting historical TN records from earlier rounds
        let updated = conn
            .execute(
                "UPDATE review_tuning_outcomes SET outcome = 'false_negative' \
                 WHERE card_id = ?1 AND outcome = 'true_negative' \
                 AND review_round = (SELECT MAX(review_round) FROM review_tuning_outcomes WHERE card_id = ?1 AND outcome = 'true_negative')",
                [card_id],
            )
            .unwrap_or(0);
        if updated > 0 {
            tracing::info!(
                "[review-tuning] #119 corrected {updated} true_negative → false_negative: card={card_id} (reopen, latest round only)"
            );

            // Backfill finding_categories if empty. The TN was recorded using the
            // last review dispatch (the pass/approved one with empty items). Look
            // for an earlier review dispatch that actually found issues.
            let needs_backfill: bool = conn
                .query_row(
                    "SELECT finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]' \
                     FROM review_tuning_outcomes \
                     WHERE card_id = ?1 AND outcome = 'false_negative' \
                     ORDER BY rowid DESC LIMIT 1",
                    [card_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if needs_backfill {
                // Walk through review dispatches (most recent first) to find
                // one with a non-empty items array containing categories
                let finding_cats: Option<String> = conn
                    .prepare(
                        "SELECT td.result FROM task_dispatches td \
                         WHERE td.kanban_card_id = ?1 AND td.dispatch_type = 'review' \
                         AND td.status = 'completed' \
                         ORDER BY td.rowid DESC",
                    )
                    .ok()
                    .and_then(|mut stmt| {
                        let rows = stmt
                            .query_map([card_id], |row| row.get::<_, Option<String>>(0))
                            .ok()?;
                        for row_result in rows {
                            if let Ok(Some(result_str)) = row_result {
                                if let Ok(v) =
                                    serde_json::from_str::<serde_json::Value>(&result_str)
                                {
                                    if let Some(items) = v["items"].as_array() {
                                        if !items.is_empty() {
                                            let cats: Vec<String> = items
                                                .iter()
                                                .filter_map(|it| {
                                                    it["category"].as_str().map(|s| s.to_string())
                                                })
                                                .collect();
                                            if !cats.is_empty() {
                                                return serde_json::to_string(&cats).ok();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        None
                    });

                if let Some(ref cats) = finding_cats {
                    let backfilled = conn
                        .execute(
                            "UPDATE review_tuning_outcomes SET finding_categories = ?1 \
                             WHERE card_id = ?2 AND outcome = 'false_negative' \
                             AND (finding_categories IS NULL OR finding_categories = '' OR finding_categories = '[]')",
                            rusqlite::params![cats, card_id],
                        )
                        .unwrap_or(0);
                    if backfilled > 0 {
                        tracing::info!(
                            "[review-tuning] #119 backfilled {backfilled} FN finding_categories: card={card_id} categories={cats}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn test_engine_with_dir(db: &Db, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).ok(); // ignore if already exists
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        ).unwrap();
    }

    fn seed_dispatch(db: &Db, card_id: &str, dispatch_status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, ?2, 'agent-1', 'implementation', ?3, 'Test Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![format!("dispatch-{}-{}", card_id, dispatch_status), card_id, dispatch_status],
        ).unwrap();
    }

    #[test]
    fn completed_dispatch_only_does_not_authorize_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-completed", "requested");
        seed_dispatch(&db, "card-completed", "completed");

        let result = transition_status(&db, &engine, "card-completed", "in_progress");
        assert!(
            result.is_err(),
            "completed dispatch should NOT authorize transition"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("active dispatch"),
            "error should mention active dispatch"
        );
    }

    #[test]
    fn pending_dispatch_authorizes_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-pending", "requested");
        seed_dispatch(&db, "card-pending", "pending");

        let result = transition_status(&db, &engine, "card-pending", "in_progress");
        assert!(
            result.is_ok(),
            "pending dispatch should authorize transition"
        );
    }

    #[test]
    fn dispatched_status_authorizes_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-dispatched", "requested");
        seed_dispatch(&db, "card-dispatched", "dispatched");

        let result = transition_status(&db, &engine, "card-dispatched", "in_progress");
        assert!(
            result.is_ok(),
            "dispatched status should authorize transition"
        );
    }

    #[test]
    fn no_dispatch_blocks_non_free_transition() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-none", "requested");
        // No dispatch at all

        let result = transition_status(&db, &engine, "card-none", "in_progress");
        assert!(result.is_err(), "no dispatch should block transition");
    }

    #[test]
    fn free_transition_works_without_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-free", "backlog");

        let result = transition_status(&db, &engine, "card-free", "ready");
        assert!(
            result.is_ok(),
            "backlog → ready should work without dispatch"
        );
    }

    #[test]
    fn force_overrides_dispatch_check() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-force", "requested");
        // No dispatch, but force=true

        let result =
            transition_status_with_opts(&db, &engine, "card-force", "in_progress", "pmd", true);
        assert!(result.is_ok(), "force=true should bypass dispatch check");
    }

    /// Regression: same-second dispatch creation must still be detected by rowid comparison.
    /// Previously used `created_at >` which has only second-level resolution and missed
    /// dispatches created in the same wall-clock second.
    #[test]
    fn notify_query_detects_same_second_dispatch_via_rowid() {
        let db = test_db();
        seed_card(&db, "card-notify", "in_progress");

        // Insert pre-hook dispatch (simulates the dispatch that existed before hooks ran)
        let pre_dispatch_id = "dispatch-pre";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES (?1, 'card-notify', 'agent-1', 'implementation', 'dispatched', 'Pre', datetime('now'), datetime('now'))",
                [pre_dispatch_id],
            ).unwrap();
        }

        // Insert hook-created dispatch in the SAME second (same datetime('now'))
        let new_dispatch_id = "dispatch-new";
        {
            let conn = db.lock().unwrap();
            // Use the exact same timestamp to simulate same-second creation
            let pre_ts: String = conn
                .query_row(
                    "SELECT created_at FROM task_dispatches WHERE id = ?1",
                    [pre_dispatch_id],
                    |row| row.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES (?1, 'card-notify', 'agent-1', 'review', 'pending', 'New', ?2, ?2)",
                rusqlite::params![new_dispatch_id, pre_ts],
            ).unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = 'card-notify'",
                [new_dispatch_id],
            )
            .unwrap();
        }

        // Verify: the rowid-based query used by notify_new_dispatches_after_hooks
        // finds the new dispatch even though created_at is identical.
        // No card_id filter — hooks can create dispatches for any card.
        let found: Vec<String> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT td.id FROM task_dispatches td \
                 JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                 WHERE td.status = 'pending' \
                   AND td.rowid > (SELECT rowid FROM task_dispatches WHERE id = ?1)",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![pre_dispatch_id], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };

        assert_eq!(found.len(), 1, "must find exactly 1 new dispatch");
        assert_eq!(found[0], new_dispatch_id);

        // Counter-check: the old timestamp-based approach would fail here
        let found_by_ts: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM task_dispatches td \
                 WHERE td.status = 'pending' \
                   AND td.created_at > (SELECT created_at FROM task_dispatches WHERE id = ?1)",
                [pre_dispatch_id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            found_by_ts, 0,
            "timestamp-based query misses same-second dispatch (proving rowid fix is necessary)"
        );
    }

    /// Regression: cross-card dispatches created by hooks (e.g. auto-queue) must be
    /// found by the notification query AND each dispatch must carry its own card_id
    /// so that send_dispatch_to_discord routes to the correct thread/issue.
    #[test]
    fn notify_query_finds_cross_card_dispatch_with_correct_card_id() {
        let db = test_db();
        seed_card(&db, "card-x", "in_progress");
        seed_card(&db, "card-y", "ready");

        // Pre-hook dispatch for card-x (the card going through transition)
        let pre_id = "dispatch-x-pre";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES (?1, 'card-x', 'agent-1', 'implementation', 'dispatched', 'X-Pre', datetime('now'), datetime('now'))",
                [pre_id],
            ).unwrap();
        }

        // Hook creates dispatch for card-y (auto-queue dispatching next card)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('dispatch-y-new', 'card-y', 'agent-1', 'implementation', 'pending', 'Y-New', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // The rowid-based query (no card_id filter) must find card-y's dispatch
        let found: Vec<(String, String)> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.kanban_card_id FROM task_dispatches td \
                 JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                 WHERE td.status = 'pending' \
                   AND td.rowid > (SELECT rowid FROM task_dispatches WHERE id = ?1)",
                )
                .unwrap();
            stmt.query_map(rusqlite::params![pre_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
        };

        assert_eq!(found.len(), 1, "must find cross-card dispatch");
        assert_eq!(found[0].0, "dispatch-y-new");
        // Critical: the dispatch carries card-y's ID, not card-x's
        assert_eq!(
            found[0].1, "card-y",
            "dispatch must carry its own card_id for correct routing"
        );
    }

    #[test]
    fn drain_hook_side_effects_materializes_tick_dispatch_intents() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-dispatch.js"),
            r#"
            var policy = {
                name: "tick-dispatch",
                priority: 1,
                onTick30s: function() {
                    agentdesk.dispatch.create(
                        "card-tick",
                        "agent-1",
                        "rework",
                        "Tick Rework"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-tick", "requested");

        engine
            .try_fire_hook_by_name("onTick30s", json!({}))
            .unwrap();
        drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-tick' AND dispatch_type = 'rework'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "tick hook dispatch intent should be persisted");
    }

    // ── Pipeline / auto-queue regression tests (#110) ──────────────

    /// Ensure auto_queue tables exist (created lazily by auto_queue routes, not main migration)
    fn ensure_auto_queue_tables(db: &Db) {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                id          TEXT PRIMARY KEY,
                repo        TEXT,
                agent_id    TEXT,
                status      TEXT DEFAULT 'active',
                ai_model    TEXT,
                ai_rationale TEXT,
                timeout_minutes INTEGER DEFAULT 120,
                created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
                completed_at DATETIME
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entries (
                id              TEXT PRIMARY KEY,
                run_id          TEXT REFERENCES auto_queue_runs(id),
                kanban_card_id  TEXT REFERENCES kanban_cards(id),
                agent_id        TEXT,
                priority_rank   INTEGER DEFAULT 0,
                reason          TEXT,
                status          TEXT DEFAULT 'pending',
                created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
                dispatched_at   DATETIME,
                completed_at    DATETIME
            );",
        )
        .unwrap();
    }

    fn seed_card_with_repo(db: &Db, card_id: &str, status: &str, repo_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
            [],
        ).ok();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at)
             VALUES (?1, 'Test Card', ?2, 'agent-1', ?3, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status, repo_id],
        ).unwrap();
    }

    /// Insert 2 pipeline stages (INTEGER AUTOINCREMENT id) and return their ids.
    fn seed_pipeline_stages(db: &Db, repo_id: &str) -> (i64, i64) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
             VALUES (?1, 'Build', 1, 'ready')",
            [repo_id],
        )
        .unwrap();
        let stage1 = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
             VALUES (?1, 'Deploy', 2, 'review_pass')",
            [repo_id],
        )
        .unwrap();
        let stage2 = conn.last_insert_rowid();
        (stage1, stage2)
    }

    fn seed_auto_queue_run(db: &Db, agent_id: &str) -> (String, String, String) {
        ensure_auto_queue_tables(db);
        let conn = db.lock().unwrap();
        let run_id = "run-1";
        let entry_a = "entry-a";
        let entry_b = "entry-b";
        conn.execute(
            "INSERT INTO auto_queue_runs (id, status, agent_id, created_at) VALUES (?1, 'active', ?2, datetime('now'))",
            rusqlite::params![run_id, agent_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
             VALUES (?1, ?2, 'card-q1', ?3, 'dispatched', 1)",
            rusqlite::params![entry_a, run_id, agent_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
             VALUES (?1, ?2, 'card-q2', ?3, 'pending', 2)",
            rusqlite::params![entry_b, run_id, agent_id],
        ).unwrap();
        (run_id.to_string(), entry_a.to_string(), entry_b.to_string())
    }

    /// #110: Pipeline stage should NOT advance on implementation dispatch completion alone.
    /// The onDispatchCompleted in pipeline.js is now a no-op — advancement happens
    /// only through review-automation processVerdict after review passes.
    #[test]
    fn pipeline_no_auto_advance_on_dispatch_complete() {
        let db = test_db();
        let engine = test_engine(&db);

        seed_card_with_repo(&db, "card-pipe", "in_progress", "repo-1");
        let (stage1, _stage2) = seed_pipeline_stages(&db, "repo-1");

        // Assign pipeline stage (use integer id)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET pipeline_stage_id = ?1 WHERE id = 'card-pipe'",
                [stage1],
            )
            .unwrap();
        }

        // Create and complete an implementation dispatch
        seed_dispatch(&db, "card-pipe", "pending");
        let dispatch_id = "dispatch-card-pipe-pending";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'completed', result = '{}' WHERE id = ?1",
                [dispatch_id],
            )
            .unwrap();
        }

        // Fire OnDispatchCompleted — should NOT create a new dispatch for stage-2
        let _ = engine
            .try_fire_hook_by_name("OnDispatchCompleted", json!({ "dispatch_id": dispatch_id }));

        // Verify: pipeline_stage_id should still be stage-1 (not advanced)
        // pipeline_stage_id is TEXT, pipeline_stages.id is INTEGER AUTOINCREMENT
        let stage_id: Option<String> = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-pipe'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            stage_id.as_deref(),
            Some(stage1.to_string().as_str()),
            "pipeline_stage_id must NOT advance on dispatch completion alone"
        );

        // Verify: no new pending dispatch was created for stage-2
        let new_dispatches: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-pipe' AND status = 'pending'",
                [],
                |row| row.get(0),
            ).unwrap()
        };
        assert_eq!(
            new_dispatches, 0,
            "no new dispatch should be created by pipeline.js onDispatchCompleted"
        );
    }

    /// #110: Rust transition_status marks auto_queue_entries as done,
    /// and this single update is sufficient (no JS triple-update).
    #[test]
    fn transition_to_done_marks_auto_queue_entry() {
        let db = test_db();
        ensure_auto_queue_tables(&db);
        let engine = test_engine(&db);

        // Seed cards for the queue
        seed_card(&db, "card-q1", "review");
        seed_card(&db, "card-q2", "ready");
        seed_dispatch(&db, "card-q1", "pending");
        let (_run_id, entry_a, _entry_b) = seed_auto_queue_run(&db, "agent-1");

        // Transition card-q1 to done
        let result = transition_status_with_opts(&db, &engine, "card-q1", "done", "review", true);
        assert!(result.is_ok(), "transition to done should succeed");

        // Verify: entry_a should be 'done' (set by Rust transition_status)
        let entry_status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM auto_queue_entries WHERE id = ?1",
                [&entry_a],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            entry_status, "done",
            "Rust must mark auto_queue_entry as done"
        );
    }

    /// #110: review → done → auto-queue should not conflict with pending_decision.
    /// When card goes to pending_decision, auto-queue entry should NOT be marked done.
    #[test]
    fn pending_decision_does_not_complete_auto_queue_entry() {
        let db = test_db();
        ensure_auto_queue_tables(&db);
        let engine = test_engine(&db);

        seed_card(&db, "card-pd", "review");
        seed_dispatch(&db, "card-pd", "pending");

        // Create auto-queue entry
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, status, agent_id, created_at) VALUES ('run-pd', 'active', 'agent-1', datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
                 VALUES ('entry-pd', 'run-pd', 'card-pd', 'agent-1', 'dispatched', 1)",
                [],
            ).unwrap();
        }

        // Transition to pending_decision (NOT done)
        let result = transition_status_with_opts(
            &db,
            &engine,
            "card-pd",
            "pending_decision",
            "pm-gate",
            true,
        );
        assert!(result.is_ok());

        // Verify: entry should still be 'dispatched' (not done)
        let entry_status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-pd'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            entry_status, "dispatched",
            "pending_decision must NOT mark auto_queue_entry as done"
        );
    }

    /// #128: started_at must reset on every in_progress re-entry (rework/resume).
    /// YAML pipeline uses `mode: coalesce` for in_progress clock, which preserves
    /// the original started_at on rework re-entry. This prevents losing the original
    /// start timestamp. Timeouts.js handles rework re-entry by checking the current
    /// dispatch's created_at rather than started_at.
    #[test]
    fn started_at_coalesces_on_in_progress_reentry() {
        let db = test_db();
        let engine = test_engine(&db);

        // Create card in review with an old started_at (simulates work done 3h ago)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).ok();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, started_at, created_at, updated_at)
                 VALUES ('card-rework', 'Test', 'review', 'agent-1', datetime('now', '-3 hours'), datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // Add dispatch to authorize transition
        seed_dispatch(&db, "card-rework", "pending");

        // Transition back to in_progress (simulates rework)
        let result = transition_status_with_opts(
            &db,
            &engine,
            "card-rework",
            "in_progress",
            "pm-decision",
            true,
        );
        assert!(result.is_ok(), "rework transition should succeed");

        // Verify started_at was PRESERVED (coalesce mode: original timestamp kept)
        let age_seconds: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT CAST((julianday('now') - julianday(started_at)) * 86400 AS INTEGER) FROM kanban_cards WHERE id = 'card-rework'",
                [],
                |row| row.get(0),
            ).unwrap()
        };
        assert!(
            age_seconds > 3500,
            "started_at should be preserved (coalesce mode), but was only {} seconds ago",
            age_seconds
        );
    }

    /// When started_at is NULL (first-time entry), coalesce mode sets it to now.
    #[test]
    fn started_at_set_on_first_in_progress_entry() {
        let db = test_db();
        let engine = test_engine(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            ).ok();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-first', 'Test', 'requested', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        seed_dispatch(&db, "card-first", "pending");

        let result =
            transition_status_with_opts(&db, &engine, "card-first", "in_progress", "system", true);
        assert!(result.is_ok());

        let age_seconds: i64 = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT CAST((julianday('now') - julianday(started_at)) * 86400 AS INTEGER) FROM kanban_cards WHERE id = 'card-first'",
                [],
                |row| row.get(0),
            ).unwrap()
        };
        assert!(
            age_seconds < 60,
            "started_at should be set to now on first entry, but was {} seconds ago",
            age_seconds
        );
    }
}
