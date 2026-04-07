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

fn transition_status_with_opts_inner<F>(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force: bool,
    on_conn_after_intents: F,
) -> Result<TransitionResult>
where
    F: FnOnce(&rusqlite::Connection) -> Result<()>,
{
    use crate::engine::transition::{
        self, CardState, GateSnapshot, TransitionContext, TransitionOutcome,
    };

    // ── 1. Assemble TransitionContext (DB reads) ──
    let conn = db.lock().map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (
        old_status,
        review_status,
        latest_dispatch_id,
        card_repo_id,
        card_agent_id,
        review_entered_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, review_status, latest_dispatch_id, repo_id, assigned_agent_id, review_entered_at \
             FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
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

    // Review verdict gate: check the latest completed review dispatch for this card.
    // When the card has a current review_entered_at clock, ignore verdicts from
    // earlier rounds. Legacy cards without review_entered_at keep the old behavior.
    let latest_review_verdict: Option<String> = conn
        .query_row(
            "SELECT json_extract(result, '$.verdict') FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review' AND status = 'completed' \
               AND (?2 IS NULL OR datetime(COALESCE(completed_at, updated_at)) >= datetime(?2)) \
             ORDER BY datetime(COALESCE(completed_at, updated_at)) DESC, id DESC LIMIT 1",
            rusqlite::params![card_id, review_entered_at.as_deref()],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let review_verdict_pass = matches!(
        latest_review_verdict.as_deref(),
        Some("pass") | Some("approved")
    );
    let review_verdict_rework = matches!(
        latest_review_verdict.as_deref(),
        Some("rework") | Some("improve") | Some("reject")
    );

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
            review_verdict_pass,
            review_verdict_rework,
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
        on_conn_after_intents(&conn)?;
        Ok(())
    })();
    if let Err(e) = exec_result {
        conn.execute_batch("ROLLBACK").ok();
        return Err(e);
    }
    conn.execute_batch("COMMIT")?;

    drop(conn);

    // ── 5. Post-transition side-effects (hooks, GitHub sync) ──
    if effective.is_terminal(new_status) {
        sync_terminal_card_state(db, card_id);
    }
    github_sync_on_transition(db, &effective, card_id, new_status);
    fire_dynamic_hooks(engine, &effective, card_id, &old_status, new_status);

    if effective.is_terminal(new_status) && record_true_negative_if_pass(db, card_id) {
        crate::server::routes::review_verdict::spawn_aggregate_if_needed(db);
    }

    Ok(TransitionResult {
        changed: true,
        from: old_status,
        to: new_status.to_string(),
    })
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
    transition_status_with_opts_inner(db, engine, card_id, new_status, source, force, |_conn| {
        Ok(())
    })
}

/// Full transition with an extra DB mutation executed inside the same
/// transaction as the canonical transition intents.
pub fn transition_status_with_opts_and_on_conn<F>(
    db: &Db,
    engine: &PolicyEngine,
    card_id: &str,
    new_status: &str,
    source: &str,
    force: bool,
    on_conn_after_intents: F,
) -> Result<TransitionResult>
where
    F: FnOnce(&rusqlite::Connection) -> Result<()>,
{
    transition_status_with_opts_inner(
        db,
        engine,
        card_id,
        new_status,
        source,
        force,
        on_conn_after_intents,
    )
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
            review_verdict_pass: false,
            review_verdict_rework: false,
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

const TERMINAL_DISPATCH_CLEANUP_REASON: &str = "auto_cancelled_on_terminal_card";

fn sync_terminal_card_state(db: &Db, card_id: &str) {
    let Ok(conn) = db.lock() else {
        return;
    };

    conn.execute(
        "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') \
         WHERE kanban_card_id = ?1 AND status = 'dispatched'",
        [card_id],
    )
    .ok();

    let pending_followups: Vec<String> = conn
        .prepare(
            "SELECT id FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type IN ('review-decision', 'rework') \
             AND status IN ('pending', 'dispatched')",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();

    let mut cancelled = 0usize;
    for dispatch_id in pending_followups {
        cancelled += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_conn(
            &conn,
            &dispatch_id,
            Some(TERMINAL_DISPATCH_CLEANUP_REASON),
        )
        .unwrap_or(0);
    }

    if cancelled > 0 {
        tracing::info!(
            "[kanban] Cancelled {} pending terminal follow-up dispatch(es) for card {}",
            cancelled,
            card_id
        );
    }
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
pub fn fire_enter_hooks(db: &Db, engine: &PolicyEngine, card_id: &str, state: &str) {
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
            sync_terminal_card_state(db, card_id);
        }

        github_sync_on_transition(db, pipeline, card_id, to);
        fire_dynamic_hooks(engine, pipeline, card_id, from, to);

        // #119: Record true_negative for cards that passed review and reached terminal state
        if pipeline.is_terminal(to) && record_true_negative_if_pass(db, card_id) {
            crate::server::routes::review_verdict::spawn_aggregate_if_needed(db);
        }
    }

    drain_hook_side_effects(db, engine);
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
        let _ = crate::github::run_gh(&["issue", "close", &num.to_string(), "--repo", &repo]);
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::run_gh(&[
            "issue",
            "comment",
            &num.to_string(),
            "--repo",
            &repo,
            "--body",
            comment,
        ]);
    }
}

/// Cooldown period for violation alerts (30 minutes).
const VIOLATION_COOLDOWN_SECS: i64 = 1800;

/// Send a violation alert to the PMD/kanban-manager channel via announce bot.
/// Applies dedup cooldown (30 min per card+from+to) and suppresses API-source alerts.
fn notify_pmd_violation(
    conn: &rusqlite::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    reason: &str,
) {
    // #200: API callers already receive error responses — no need to alert PMD.
    if source == "api" {
        tracing::debug!(
            "[kanban] Suppressing violation alert for api source: {from} → {to} card {card_id}"
        );
        return;
    }

    // #200: Dedup cooldown — skip if same card+from+to was alerted within 30 min.
    let cooldown_key = format!("violation_sent:{card_id}:{from}:{to}");
    let recently_sent: bool = conn
        .query_row(
            "SELECT 1 FROM kv_meta WHERE key = ?1 \
             AND (expires_at IS NULL OR expires_at > datetime('now'))",
            [&cooldown_key],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if recently_sent {
        tracing::debug!(
            "[kanban] Violation alert cooldown active for {from} → {to} card {card_id}"
        );
        return;
    }
    // Record cooldown with TTL
    let _ = conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) \
         VALUES (?1, ?2, datetime('now', '+' || ?3 || ' seconds'))",
        rusqlite::params![cooldown_key, "1", VIOLATION_COOLDOWN_SECS.to_string()],
    );

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
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
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

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
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

    fn seed_dispatch_with_type(
        db: &Db,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
        dispatch_status: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Typed Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, dispatch_type, dispatch_status],
        )
        .unwrap();
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

    #[test]
    fn stale_completed_review_verdict_does_not_open_current_done_gate() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-stale-review-pass", "review");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET review_entered_at = datetime('now')
                 WHERE id = 'card-stale-review-pass'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                    created_at, updated_at, completed_at
                 ) VALUES (
                    'review-stale-pass', 'card-stale-review-pass', 'agent-1', 'review', 'completed',
                    'stale pass', ?1,
                    datetime('now', '-30 minutes'), datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
                 )",
                rusqlite::params![json!({"verdict": "pass"}).to_string()],
            )
            .unwrap();
        }

        let result = transition_status(&db, &engine, "card-stale-review-pass", "done");
        assert!(
            result.is_err(),
            "completed review verdicts from older rounds must not satisfy the current review_passed gate"
        );

        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-stale-review-pass'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            status, "review",
            "stale review verdict must leave the card in review"
        );
    }

    #[test]
    fn legacy_review_without_review_entered_at_keeps_latest_pass_behavior() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-legacy-review-pass", "review");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                    created_at, updated_at, completed_at
                 ) VALUES (
                    'review-legacy-pass', 'card-legacy-review-pass', 'agent-1', 'review', 'completed',
                    'legacy pass', ?1,
                    datetime('now', '-10 minutes'), datetime('now', '-5 minutes'), datetime('now', '-5 minutes')
                 )",
                rusqlite::params![json!({"verdict": "pass"}).to_string()],
            )
            .unwrap();
        }

        let result = transition_status(&db, &engine, "card-legacy-review-pass", "done");
        assert!(
            result.is_ok(),
            "cards without review_entered_at must preserve the legacy pass verdict behavior"
        );
    }

    #[test]
    fn transition_status_with_on_conn_rolls_back_on_cleanup_error() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-force-rollback", "requested");
        seed_dispatch(&db, "card-force-rollback", "pending");

        let result = transition_status_with_opts_and_on_conn(
            &db,
            &engine,
            "card-force-rollback",
            "in_progress",
            "pmd",
            true,
            |_conn| Err(anyhow::anyhow!("cleanup failed")),
        );
        assert!(result.is_err(), "cleanup failure must abort the transition");

        let status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-force-rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            status, "requested",
            "cleanup failure must roll back the card status change"
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

    /// Regression test for #274: transition_status() fires custom state hooks
    /// through try_fire_hook_by_name(), and dispatch.create() in that path must
    /// return with the dispatch row + notify outbox already materialized.
    #[test]
    fn transition_status_custom_on_enter_hook_materializes_dispatch_outbox() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("ready-enter-hook.js"),
            r#"
            var policy = {
                name: "ready-enter-hook",
                priority: 1,
                onCustomReadyEnter: function(payload) {
                    agentdesk.dispatch.create(
                        payload.card_id,
                        "agent-1",
                        "implementation",
                        "Ready Hook Dispatch"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-ready-hook", "backlog");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE agents SET pipeline_config = ?1 WHERE id = 'agent-1'",
                [json!({
                    "hooks": {
                        "ready": {
                            "on_enter": ["onCustomReadyEnter"],
                            "on_exit": []
                        }
                    }
                })
                .to_string()],
            )
            .unwrap();
        }

        transition_status(&db, &engine, "card-ready-hook", "ready").unwrap();

        let conn = db.lock().unwrap();
        let (dispatch_id, title): (String, String) = conn
            .query_row(
                "SELECT id, title FROM task_dispatches WHERE kanban_card_id = 'card-ready-hook'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("custom ready on_enter hook should create a dispatch");
        assert_eq!(title, "Ready Hook Dispatch");

        let notify_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
                [&dispatch_id],
                |row| row.get(0),
            )
            .expect("dispatch outbox query should succeed");
        assert_eq!(
            notify_count, 1,
            "custom transition hook dispatch must enqueue exactly one notify outbox row"
        );

        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-ready-hook'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("card should be updated by dispatch.create()");
        assert_eq!(card_status, "in_progress");
        assert_eq!(latest_dispatch_id, dispatch_id);
    }

    /// Regression guard for the known-hook path: try_fire_hook_by_name() must
    /// return with dispatch.create() side-effects already visible, even without
    /// an extra drain_hook_side_effects() call at the caller.
    #[test]
    fn try_fire_hook_drains_dispatch_intents_without_explicit_drain() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-intent.js"),
            r#"
            var policy = {
                name: "tick-intent",
                priority: 1,
                onTick1min: function() {
                    agentdesk.dispatch.create(
                        "card-intent-test",
                        "agent-1",
                        "implementation",
                        "Intent Drain Test"
                    );
                }
            };
            agentdesk.registerPolicy(policy);
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());
        seed_card(&db, "card-intent-test", "requested");

        // Fire tick hook — do NOT call drain_hook_side_effects afterwards.
        // The intent should still be drained by try_fire_hook's internal drain.
        engine
            .try_fire_hook_by_name("OnTick1min", json!({}))
            .unwrap();

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-intent-test' AND dispatch_type = 'implementation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "#202: tick hook dispatch intent must be persisted by try_fire_hook's internal drain"
        );
    }

    #[test]
    fn fire_transition_hooks_terminal_cleanup_cancels_review_followups_with_reason() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-terminal-cleanup", "review");
        seed_dispatch_with_type(
            &db,
            "dispatch-rd-cleanup",
            "card-terminal-cleanup",
            "review-decision",
            "pending",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-rw-cleanup",
            "card-terminal-cleanup",
            "rework",
            "dispatched",
        );
        seed_dispatch_with_type(
            &db,
            "dispatch-impl-keep",
            "card-terminal-cleanup",
            "implementation",
            "pending",
        );

        fire_transition_hooks(&db, &engine, "card-terminal-cleanup", "review", "done");

        let conn = db.lock().unwrap();
        let (rd_status, rd_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rd-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (rw_status, rw_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, json_extract(result, '$.reason') FROM task_dispatches WHERE id = 'dispatch-rw-cleanup'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let impl_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-impl-keep'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(rd_status, "cancelled");
        assert_eq!(rd_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(rw_status, "cancelled");
        assert_eq!(rw_reason.as_deref(), Some(TERMINAL_DISPATCH_CLEANUP_REASON));
        assert_eq!(
            impl_status, "pending",
            "terminal cleanup must not cancel unrelated pending work dispatches"
        );
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
                unified_thread INTEGER DEFAULT 0,
                unified_thread_id TEXT,
                unified_thread_channel_id TEXT,
                max_concurrent_threads INTEGER DEFAULT 1,
                max_concurrent_per_agent INTEGER DEFAULT 1,
                thread_group_count INTEGER DEFAULT 1,
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
                dispatch_id     TEXT,
                thread_group    INTEGER DEFAULT 0,
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

    #[cfg(unix)]
    #[test]
    fn deploy_pipeline_uses_card_scoped_worktree_instead_of_latest_session_cwd() {
        let _env_guard = crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let temp = TempDir::new().unwrap();
        let policies_dir = temp.path().join("policies");
        fs::create_dir_all(&policies_dir).unwrap();
        fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("policies")
                .join("deploy-pipeline.js"),
            policies_dir.join("deploy-pipeline.js"),
        )
        .unwrap();

        let fake_bin_dir = temp.path().join("bin");
        fs::create_dir_all(&fake_bin_dir).unwrap();
        let tmux_log = temp.path().join("tmux.log");
        write_executable_script(
            &fake_bin_dir.join("tmux"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = \"has-session\" ]; then\n  echo \"missing\" >&2\n  exit 1\nfi\nexit 0\n",
                tmux_log.display()
            ),
        );

        let original_path = std::env::var("PATH").unwrap_or_default();
        let _path_guard = EnvVarGuard::set(
            "PATH",
            &format!("{}:{}", fake_bin_dir.display(), original_path),
        );

        let db = test_db();
        let engine = test_engine_with_dir(&db, &policies_dir);
        let card_id = format!("dep{:05}-card", std::process::id() % 100000);
        seed_card_with_repo(&db, &card_id, "review", "repo-1");

        let correct_worktree = temp.path().join("worktrees").join("card-301");
        let wrong_worktree = temp.path().join("worktrees").join("other-card");
        fs::create_dir_all(&correct_worktree).unwrap();
        fs::create_dir_all(&wrong_worktree).unwrap();

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, provider)
                 VALUES ('repo-1', 'dev-deploy', 1, 'review_pass', 'self')",
                [],
            )
            .unwrap();
            let stage_id = conn.last_insert_rowid();
            conn.execute(
                "UPDATE kanban_cards
                 SET pipeline_stage_id = ?1, blocked_reason = 'deploy:waiting', updated_at = datetime('now')
                 WHERE id = ?2",
                rusqlite::params![stage_id, card_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result, created_at, updated_at
                 ) VALUES (
                    'dispatch-card-deploy-301', ?1, 'agent-1', 'implementation', 'completed',
                    'Implementation Done', ?2, '{}', datetime('now'), datetime('now')
                 )",
                rusqlite::params![card_id, serde_json::json!({
                    "worktree_path": correct_worktree.display().to_string(),
                    "worktree_branch": "feat/301-correct-worktree"
                })
                .to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (
                    session_key, agent_id, provider, status, cwd, last_heartbeat
                 ) VALUES (
                    'session-card-other', 'agent-1', 'codex', 'connected', ?1, datetime('now')
                 )",
                [wrong_worktree.display().to_string()],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("onTick30s", json!({}))
            .unwrap();

        let blocked_reason: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            blocked_reason.starts_with("deploy:deploying:adk-deploy-"),
            "deploy queue should transition card into deploying state"
        );

        let tmux_invocations = fs::read_to_string(&tmux_log).unwrap();
        println!("[test] deploy tmux invocations:\n{tmux_invocations}");
        assert!(
            tmux_invocations.contains(&correct_worktree.display().to_string()),
            "deploy command must use card-scoped worktree path from dispatch context"
        );
        assert!(
            !tmux_invocations.contains(&wrong_worktree.display().to_string()),
            "deploy command must ignore latest session cwd from another card"
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

    #[test]
    fn run_completion_enqueues_notify_to_main_channel() {
        let db = test_db();
        ensure_auto_queue_tables(&db);
        let engine = test_engine(&db);

        seed_card_with_repo(&db, "card-notify", "review", "repo-1");
        seed_dispatch(&db, "card-notify", "pending");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (
                    id, repo, agent_id, status, unified_thread, unified_thread_id, thread_group_count, created_at
                 ) VALUES (?1, ?2, ?3, 'active', 1, ?4, 1, datetime('now'))",
                rusqlite::params![
                    "run-notify",
                    "repo-1",
                    "agent-1",
                    r#"{"123":"thread-999"}"#
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank
                 ) VALUES (?1, ?2, ?3, ?4, 'dispatched', ?5, 1)",
                rusqlite::params![
                    "entry-notify",
                    "run-notify",
                    "card-notify",
                    "agent-1",
                    "dispatch-card-notify-pending"
                ],
            )
            .unwrap();
        }

        let result =
            transition_status_with_opts(&db, &engine, "card-notify", "done", "review", true);
        assert!(result.is_ok(), "transition to done should succeed");

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-notify'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_status, "completed");

        let (target, bot, content): (String, String, String) = conn
            .query_row(
                "SELECT target, bot, content FROM message_outbox ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(bot, "notify");
        assert!(
            content.contains("자동큐 완료: repo-1 / run run-noti / 1개"),
            "notify message should summarize the completed run"
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
