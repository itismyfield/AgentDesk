use anyhow::Result;
use serde_json::json;

use crate::db::Db;
use crate::engine::PolicyEngine;

/// Core dispatch creation: DB operations only, no hooks fired.
///
/// - Inserts a record into `task_dispatches`
/// - Updates `kanban_cards.latest_dispatch_id` and sets status to "requested" (non-review)
/// - Returns `(dispatch_id, old_card_status)`
///
/// Caller is responsible for firing hooks after this returns.
pub fn create_dispatch_core(
    db: &Db,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String)> {
    let dispatch_id = uuid::Uuid::new_v4().to_string();

    // For review dispatches, inject reviewed_commit (HEAD) and provider info
    let context_str = if dispatch_type == "review" {
        let mut ctx_val = if context.is_object() {
            context.clone()
        } else {
            json!({})
        };
        if let Some(obj) = ctx_val.as_object_mut() {
            if !obj.contains_key("reviewed_commit") {
                let repo_dir = match std::env::var("AGENTDESK_REPO_DIR") {
                    Ok(d) => d,
                    Err(_) => dirs::home_dir()
                        .ok_or_else(|| {
                            anyhow::anyhow!("HOME directory not found; set AGENTDESK_REPO_DIR")
                        })?
                        .join("AgentDesk")
                        .to_string_lossy()
                        .into_owned(),
                };
                if let Some(commit) = crate::services::platform::git_head_commit(&repo_dir) {
                    obj.insert("reviewed_commit".to_string(), json!(commit));
                }
            }
            // Inject from_provider/target_provider for cross-provider review validation
            if !obj.contains_key("from_provider") || !obj.contains_key("target_provider") {
                if let Ok(conn) = db.separate_conn() {
                    if let Ok((ch, alt)) = conn.query_row(
                        "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?1",
                        [to_agent_id],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                            ))
                        },
                    ) {
                        if !obj.contains_key("from_provider") {
                            if let Some(fp) = ch.as_deref().and_then(provider_from_channel_suffix) {
                                obj.insert("from_provider".to_string(), json!(fp));
                            }
                        }
                        if !obj.contains_key("target_provider") {
                            if let Some(tp) = alt.as_deref().and_then(provider_from_channel_suffix)
                            {
                                obj.insert("target_provider".to_string(), json!(tp));
                            }
                        }
                    }
                }
            }
        }
        serde_json::to_string(&ctx_val)?
    } else {
        serde_json::to_string(context)?
    };

    // Use separate_conn to avoid blocking request handlers while
    // engine/onTick holds the main DB Mutex via QuickJS.
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;

    // Get current card status for the transition hook
    let old_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| row.get(0),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    // Guard: prevent ALL dispatches for terminal cards (pipeline-driven).
    // No dispatch type should reopen a completed card.
    crate::pipeline::ensure_loaded();
    let is_terminal = crate::pipeline::try_get()
        .map(|p| p.is_terminal(&old_status))
        .unwrap_or(old_status == "done");
    if is_terminal {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type,
            kanban_card_id,
            old_status
        ));
    }

    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework";

    // #116: Cancel any existing pending review-decision for this card before creating a new one.
    // Enforces the invariant: at most 1 pending/dispatched review-decision per card.
    if dispatch_type == "review-decision" {
        let cancelled = conn.execute(
            "UPDATE task_dispatches SET status = 'cancelled', result = '{\"reason\":\"superseded_by_new_review_decision\"}', updated_at = datetime('now') \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
            [kanban_card_id],
        ).unwrap_or(0);
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled,
                kanban_card_id
            );
        }
    }

    // Insert dispatch.
    // #116: For review-decision, the partial unique index idx_single_active_review_decision
    // prevents concurrent race conditions from creating duplicates at the DB level.
    if let Err(e) = conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, datetime('now'), datetime('now'))",
        rusqlite::params![dispatch_id, kanban_card_id, to_agent_id, dispatch_type, title, context_str],
    ) {
        if dispatch_type == "review-decision"
            && e.to_string().contains("UNIQUE constraint failed")
        {
            return Err(anyhow::anyhow!(
                "review-decision already exists for card {} (concurrent race prevented by DB constraint)",
                kanban_card_id
            ));
        }
        return Err(e.into());
    }

    // Update kanban card — rework/review dispatches keep current status
    if is_review_type {
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, kanban_card_id],
        )?;
    } else {
        // Pipeline-driven: resolve the dispatch kickoff state (first gated target from dispatchable)
        crate::pipeline::ensure_loaded();
        let kickoff_state = crate::pipeline::try_get()
            .and_then(|p| {
                let dispatchable = p.dispatchable_states();
                p.transitions.iter().find(|t| {
                    t.transition_type == crate::pipeline::TransitionType::Gated
                        && dispatchable.contains(&t.from.as_str())
                }).map(|t| t.to.clone())
            })
            .unwrap_or_else(|| "requested".to_string());
        // Build clock SQL from pipeline config
        let clock_sql = crate::pipeline::try_get()
            .and_then(|p| p.clock_for_state(&kickoff_state))
            .map(|c| format!(", {} = datetime('now')", c.set))
            .unwrap_or_else(|| ", requested_at = datetime('now')".to_string());
        let sql = format!(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, status = ?2{clock_sql}, updated_at = datetime('now') WHERE id = ?3"
        );
        conn.execute(&sql, rusqlite::params![dispatch_id, kickoff_state, kanban_card_id])?;
    }

    Ok((dispatch_id, old_status))
}

/// Like `create_dispatch_core` but uses a pre-assigned dispatch ID (#121 intent model).
/// Called by the intent executor when processing CreateDispatch intents.
pub fn create_dispatch_core_with_id(
    db: &Db,
    dispatch_id: &str,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<(String, String)> {
    // For review dispatches, inject reviewed_commit (HEAD) and provider info
    let context_str = if dispatch_type == "review" {
        let mut ctx_val = if context.is_object() {
            context.clone()
        } else {
            json!({})
        };
        if let Some(obj) = ctx_val.as_object_mut() {
            if !obj.contains_key("reviewed_commit") {
                let repo_dir = match std::env::var("AGENTDESK_REPO_DIR") {
                    Ok(d) => d,
                    Err(_) => dirs::home_dir()
                        .ok_or_else(|| {
                            anyhow::anyhow!("HOME directory not found; set AGENTDESK_REPO_DIR")
                        })?
                        .join("AgentDesk")
                        .to_string_lossy()
                        .into_owned(),
                };
                if let Some(commit) = crate::services::platform::git_head_commit(&repo_dir) {
                    obj.insert("reviewed_commit".to_string(), json!(commit));
                }
            }
            if !obj.contains_key("from_provider") || !obj.contains_key("target_provider") {
                if let Ok(conn) = db.separate_conn() {
                    if let Ok((ch, alt)) = conn.query_row(
                        "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?1",
                        [to_agent_id],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                            ))
                        },
                    ) {
                        if !obj.contains_key("from_provider") {
                            if let Some(fp) = ch.as_deref().and_then(provider_from_channel_suffix) {
                                obj.insert("from_provider".to_string(), json!(fp));
                            }
                        }
                        if !obj.contains_key("target_provider") {
                            if let Some(tp) =
                                alt.as_deref().and_then(provider_from_channel_suffix)
                            {
                                obj.insert("target_provider".to_string(), json!(tp));
                            }
                        }
                    }
                }
            }
        }
        serde_json::to_string(&ctx_val)?
    } else {
        serde_json::to_string(context)?
    };

    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB conn error: {e}"))?;

    let old_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [kanban_card_id],
            |row| row.get(0),
        )
        .map_err(|e| anyhow::anyhow!("Card not found: {e}"))?;

    crate::pipeline::ensure_loaded();
    let is_terminal = crate::pipeline::try_get()
        .map(|p| p.is_terminal(&old_status))
        .unwrap_or(old_status == "done");
    if is_terminal {
        return Err(anyhow::anyhow!(
            "Cannot create {} dispatch for terminal card {} (status: {}) — cannot revert terminal card",
            dispatch_type, kanban_card_id, old_status
        ));
    }

    let is_review_type = dispatch_type == "review"
        || dispatch_type == "review-decision"
        || dispatch_type == "rework";

    if dispatch_type == "review-decision" {
        let cancelled = conn.execute(
            "UPDATE task_dispatches SET status = 'cancelled', result = '{\"reason\":\"superseded_by_new_review_decision\"}', updated_at = datetime('now') \
             WHERE kanban_card_id = ?1 AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
            [kanban_card_id],
        ).unwrap_or(0);
        if cancelled > 0 {
            tracing::info!(
                "[dispatch] Cancelled {} stale review-decision(s) for card {} before creating new one",
                cancelled, kanban_card_id
            );
        }
    }

    if let Err(e) = conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, datetime('now'), datetime('now'))",
        rusqlite::params![dispatch_id, kanban_card_id, to_agent_id, dispatch_type, title, context_str],
    ) {
        if dispatch_type == "review-decision"
            && e.to_string().contains("UNIQUE constraint failed")
        {
            return Err(anyhow::anyhow!(
                "review-decision already exists for card {} (concurrent race prevented by DB constraint)",
                kanban_card_id
            ));
        }
        return Err(e.into());
    }

    if is_review_type {
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, kanban_card_id],
        )?;
    } else {
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, status = 'requested', requested_at = datetime('now'), updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, kanban_card_id],
        )?;
    }

    Ok((dispatch_id.to_string(), old_status))
}

/// Create a new dispatch for a kanban card.
///
/// - Delegates DB work to `create_dispatch_core`
/// - Fires `OnCardTransition` hook (old_status -> requested)
///
/// Returns the full dispatch row as JSON.
pub fn create_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    kanban_card_id: &str,
    to_agent_id: &str,
    dispatch_type: &str,
    title: &str,
    context: &serde_json::Value,
) -> Result<serde_json::Value> {
    let (dispatch_id, old_status) = create_dispatch_core(
        db,
        kanban_card_id,
        to_agent_id,
        dispatch_type,
        title,
        context,
    )?;

    // Read back the dispatch
    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;
    let dispatch = query_dispatch_row(&conn, &dispatch_id)?;
    drop(conn);

    // Fire pipeline-defined on_enter hooks for the kickoff state (#134).
    // Resolve kickoff state from pipeline (same logic as create_dispatch_core).
    crate::pipeline::ensure_loaded();
    let kickoff = crate::pipeline::try_get()
        .and_then(|p| {
            let d = p.dispatchable_states();
            p.transitions.iter().find(|t| {
                t.transition_type == crate::pipeline::TransitionType::Gated
                    && d.contains(&t.from.as_str())
            }).map(|t| t.to.as_str())
        })
        .unwrap_or("requested");
    crate::kanban::fire_state_hooks(db, engine, kanban_card_id, &old_status, kickoff);

    Ok(dispatch)
}

/// Complete a dispatch, setting its status to "completed" with the given result.
/// Fires `OnDispatchCompleted` hook.
pub fn complete_dispatch(
    db: &Db,
    engine: &PolicyEngine,
    dispatch_id: &str,
    result: &serde_json::Value,
) -> Result<serde_json::Value> {
    let result_str = serde_json::to_string(result)?;

    let conn = db
        .separate_conn()
        .map_err(|e| anyhow::anyhow!("DB lock error: {e}"))?;

    let changed = conn.execute(
        "UPDATE task_dispatches SET status = 'completed', result = ?1, updated_at = datetime('now') \
         WHERE id = ?2 AND status IN ('pending', 'dispatched')",
        rusqlite::params![result_str, dispatch_id],
    )?;

    if changed == 0 {
        // Either not found, already completed, or cancelled — skip hook firing
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⏭ complete_dispatch: {dispatch_id} already completed/cancelled, skipping hooks"
            );
            let dispatch = query_dispatch_row(&conn, dispatch_id)?;
            drop(conn);
            return Ok(dispatch);
        }
        return Err(anyhow::anyhow!("Dispatch not found: {dispatch_id}"));
    }

    let dispatch = query_dispatch_row(&conn, dispatch_id)?;

    let kanban_card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok();

    // Capture card status BEFORE hooks fire (used for audit/logging if needed)
    let _old_status: String = kanban_card_id
        .as_ref()
        .and_then(|cid| {
            conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or_default();

    // Capture max rowid before hooks fire — any dispatches created by hooks
    // (JS agentdesk.dispatch.create()) will have a higher rowid.
    let pre_hook_max_rowid: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM task_dispatches",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    drop(conn);

    // Fire event hooks for dispatch completion (#134 — pipeline-defined events)
    crate::kanban::fire_event_hooks(
        engine,
        "on_dispatch_completed",
        "OnDispatchCompleted",
        json!({
            "dispatch_id": dispatch_id,
            "kanban_card_id": kanban_card_id,
            "result": result,
        }),
    );

    // After OnDispatchCompleted, policies may have changed card status via setStatus().
    // Drain pending transitions and fire follow-up hooks (OnReviewEnter, etc.).
    // Loop because transition hooks may generate further transitions.
    // Note: fire_transition_hooks already calls notify_new_dispatches_after_hooks
    // for dispatches created during each transition's hooks.
    loop {
        let transitions = engine.drain_pending_transitions();
        if transitions.is_empty() {
            break;
        }
        for (card_id, old_s, new_s) in &transitions {
            crate::kanban::fire_transition_hooks(db, engine, card_id, old_s, new_s);
        }
    }

    // After all hooks and transitions drained, check for dispatches created by
    // OnDispatchCompleted hooks (e.g. pipeline.js, review-automation.js, timeouts.js)
    // that were NOT covered by fire_transition_hooks' notify_new_dispatches_after_hooks.
    // These are dispatches created outside any card transition context.
    notify_hook_created_dispatches(db, pre_hook_max_rowid);

    // #139: Safety net — if card transitioned to review but OnReviewEnter failed
    // to create a review dispatch (engine lock contention, JS error, etc.),
    // re-fire OnReviewEnter to guarantee review dispatch creation.
    {
        let needs_review_dispatch = db
            .lock()
            .ok()
            .map(|conn| {
                let card_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM kanban_cards WHERE id = ?1",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .ok();
                let has_review_dispatch: bool = conn
                    .query_row(
                        "SELECT COUNT(*) > 0 FROM task_dispatches \
                         WHERE kanban_card_id = ?1 AND dispatch_type IN ('review', 'review-decision') \
                         AND status IN ('pending', 'dispatched')",
                        [&kanban_card_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
                // Pipeline-driven: check if current state has OnReviewEnter hook
                let is_review_state = card_status.as_deref().map_or(false, |s| {
                    crate::pipeline::try_get()
                        .and_then(|p| p.hooks_for_state(s))
                        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"))
                });
                is_review_state && !has_review_dispatch
            })
            .unwrap_or(false);

        if needs_review_dispatch {
            let cid = kanban_card_id.as_deref().unwrap_or("unknown");
            tracing::warn!(
                "[dispatch] Card {} in review-like state but no review dispatch — re-firing OnReviewEnter (#139)",
                cid
            );
            let _ = engine.try_fire_hook_by_name(
                "OnReviewEnter",
                json!({ "card_id": cid }),
            );
            // Drain any transitions from the re-fired hook
            loop {
                let transitions = engine.drain_pending_transitions();
                if transitions.is_empty() {
                    break;
                }
                for (cid, old_s, new_s) in &transitions {
                    crate::kanban::fire_transition_hooks(db, engine, cid, old_s, new_s);
                }
            }
            notify_hook_created_dispatches(db, pre_hook_max_rowid);
        }
    }

    Ok(dispatch)
}

/// Send Discord notifications for any pending dispatches created after `pre_hook_max_rowid`.
/// Uses the `dispatch_notified` dedup guard in `send_dispatch_to_discord` to avoid
/// double-notifying dispatches already handled by `notify_new_dispatches_after_hooks`.
pub(crate) fn notify_hook_created_dispatches(db: &Db, pre_hook_max_rowid: i64) {
    let dispatches: Vec<(String, String, String, String)> = db
        .separate_conn()
        .ok()
        .map(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT td.id, td.to_agent_id, td.kanban_card_id, kc.title \
                     FROM task_dispatches td \
                     JOIN kanban_cards kc ON td.kanban_card_id = kc.id \
                     WHERE td.status = 'pending' \
                       AND td.rowid > ?1 \
                       AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id)",
                )
                .ok();
            stmt.as_mut()
                .and_then(|s| {
                    s.query_map(rusqlite::params![pre_hook_max_rowid], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })
                    .ok()
                })
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    if dispatches.is_empty() {
        return;
    }

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let db_clone = db.clone();
        for (dispatch_id, agent_id, card_id, title) in dispatches {
            let db_c = db_clone.clone();
            handle.spawn(async move {
                crate::server::routes::dispatches::send_dispatch_to_discord(
                    &db_c,
                    &agent_id,
                    &title,
                    &card_id,
                    &dispatch_id,
                )
                .await;
            });
        }
    }
}

/// Read a single dispatch row as JSON.
pub fn query_dispatch_row(
    conn: &rusqlite::Connection,
    dispatch_id: &str,
) -> Result<serde_json::Value> {
    conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at, COALESCE(retry_count, 0)
         FROM task_dispatches WHERE id = ?1",
        [dispatch_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "from_agent_id": row.get::<_, Option<String>>(2)?,
                "to_agent_id": row.get::<_, Option<String>>(3)?,
                "dispatch_type": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "title": row.get::<_, Option<String>>(6)?,
                "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
                "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
                "chain_depth": row.get::<_, i64>(10)?,
                "created_at": row.get::<_, String>(11)?,
                "updated_at": row.get::<_, String>(12)?,
                "retry_count": row.get::<_, i64>(13)?,
            }))
        },
    )
    .map_err(|e| anyhow::anyhow!("Dispatch query error: {e}"))
}

/// Check whether a dispatch belongs to an active unified-thread auto-queue run.
///
/// Returns `true` when:
/// - The dispatch's kanban card is part of an active/paused auto-queue run
/// - That run has `unified_thread_id IS NOT NULL`
/// - The run still has pending or dispatched entries remaining
///
/// When `true`, callers should **not** tear down the tmux session because the
/// same thread will be reused for subsequent queue entries.
///
/// Uses a standalone `rusqlite::Connection` opened from the runtime DB path
/// to avoid lock contention with the main `Db` mutex.
pub fn is_unified_thread_active(dispatch_id: &str) -> bool {
    let root = match crate::cli::agentdesk_runtime_root() {
        Some(r) => r,
        None => return false,
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let result: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 \
             FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             WHERE e.run_id = ( \
                 SELECT e2.run_id FROM auto_queue_entries e2 \
                 JOIN task_dispatches td ON td.kanban_card_id = e2.kanban_card_id \
                 WHERE td.id = ?1 LIMIT 1 \
             ) \
             AND r.status IN ('active', 'paused') \
             AND e.status IN ('pending', 'dispatched') \
             AND r.unified_thread_id IS NOT NULL",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    result
}

/// Check whether a thread channel belongs to an active unified-thread auto-queue run.
///
/// Looks up `auto_queue_runs` by `unified_thread_channel_id` matching the
/// given Discord channel ID. Returns `true` when a matching active/paused run
/// still has pending or dispatched entries.
pub fn is_unified_thread_channel_active(channel_id: u64) -> bool {
    let root = match crate::cli::agentdesk_runtime_root() {
        Some(r) => r,
        None => return false,
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let conn = match rusqlite::Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let channel_str = channel_id.to_string();
    let result: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 \
             FROM auto_queue_entries e \
             JOIN auto_queue_runs r ON e.run_id = r.id \
             WHERE r.unified_thread_channel_id = ?1 \
             AND r.status IN ('active', 'paused') \
             AND e.status IN ('pending', 'dispatched') \
             AND r.unified_thread_id IS NOT NULL",
            [&channel_str],
            |row| row.get(0),
        )
        .unwrap_or(false);
    result
}

/// Check whether a channel name (from tmux session parsing) belongs to an active
/// unified-thread auto-queue run. Extracts the thread channel ID from the
/// `-t{15+digit}` suffix in the channel name.
pub fn is_unified_thread_channel_name_active(channel_name: &str) -> bool {
    // Extract thread channel ID from channel name suffix (-t{15+digits})
    let thread_channel_id: u64 = match channel_name.rfind("-t") {
        Some(pos) => {
            let suffix = &channel_name[pos + 2..];
            if suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit()) {
                suffix.parse().unwrap_or(0)
            } else {
                return false;
            }
        }
        None => return false,
    };
    if thread_channel_id == 0 {
        return false;
    }
    is_unified_thread_channel_active(thread_channel_id)
}

/// Determine provider from a Discord channel name suffix.
fn provider_from_channel_suffix(channel: &str) -> Option<&'static str> {
    if channel.ends_with("-cc") {
        Some("claude")
    } else if channel.ends_with("-cdx") {
        Some("codex")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn seed_card(db: &Db, card_id: &str, status: &str) {
        let conn = db.separate_conn().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    #[test]
    fn create_dispatch_inserts_and_updates_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-1", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-1",
            "agent-1",
            "implementation",
            "Do the thing",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-1");
        assert_eq!(dispatch["to_agent_id"], "agent-1");
        assert_eq!(dispatch["dispatch_type"], "implementation");
        assert_eq!(dispatch["title"], "Do the thing");

        // Card should be updated
        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "requested");
        assert_eq!(latest_dispatch_id, dispatch["id"].as_str().unwrap());
    }

    #[test]
    fn create_dispatch_for_nonexistent_card_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = create_dispatch(
            &db,
            &engine,
            "nonexistent",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        );
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_updates_status() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-2", "ready");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-2",
            "agent-1",
            "implementation",
            "title",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        let completed =
            complete_dispatch(&db, &engine, &dispatch_id, &json!({"output": "done"})).unwrap();

        assert_eq!(completed["status"], "completed");
    }

    #[test]
    fn complete_dispatch_nonexistent_fails() {
        let db = test_db();
        let engine = test_engine(&db);

        let result = complete_dispatch(&db, &engine, "nonexistent", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn complete_dispatch_skips_cancelled() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-cancel", "review");

        let dispatch = create_dispatch(
            &db,
            &engine,
            "card-cancel",
            "agent-1",
            "review-decision",
            "Decision",
            &json!({}),
        )
        .unwrap();
        let dispatch_id = dispatch["id"].as_str().unwrap().to_string();

        // Simulate dismiss: cancel the dispatch
        {
            let conn = db.separate_conn().unwrap();
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled' WHERE id = ?1",
                [&dispatch_id],
            )
            .unwrap();
        }

        // Delayed completion attempt should NOT re-complete the cancelled dispatch
        let result = complete_dispatch(&db, &engine, &dispatch_id, &json!({"verdict": "pass"}));
        // Should return Ok (dispatch found) but status should remain cancelled
        assert!(result.is_ok());
        let returned = result.unwrap();
        assert_eq!(
            returned["status"], "cancelled",
            "cancelled dispatch must not be re-completed"
        );
    }

    #[test]
    fn create_review_dispatch_for_done_card_rejected() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-done", "done");

        for dispatch_type in &["review", "review-decision", "rework"] {
            let result = create_dispatch(
                &db,
                &engine,
                "card-done",
                "agent-1",
                dispatch_type,
                "Should fail",
                &json!({}),
            );
            assert!(
                result.is_err(),
                "{} dispatch should not be created for done card",
                dispatch_type
            );
        }

        // All dispatch types for done cards should be rejected
        let result = create_dispatch(
            &db,
            &engine,
            "card-done",
            "agent-1",
            "implementation",
            "Reopen work",
            &json!({}),
        );
        assert!(
            result.is_err(),
            "implementation dispatch should be rejected for done card"
        );
    }

    #[test]
    fn create_dispatch_core_shares_invariants_with_create_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-core", "ready");

        // create_dispatch_core returns (dispatch_id, old_status)
        let (dispatch_id, old_status) = create_dispatch_core(
            &db,
            "card-core",
            "agent-1",
            "implementation",
            "Core dispatch",
            &json!({"key": "value"}),
        )
        .unwrap();

        assert_eq!(old_status, "ready");

        let conn = db.separate_conn().unwrap();
        let (card_status, latest_dispatch_id): (String, String) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-core'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(card_status, "requested");
        assert_eq!(latest_dispatch_id, dispatch_id);

        // Dispatch row exists
        let dispatch = query_dispatch_row(&conn, &dispatch_id).unwrap();
        assert_eq!(dispatch["status"], "pending");
        assert_eq!(dispatch["kanban_card_id"], "card-core");
        drop(conn);

        // create_dispatch delegates to core — verify same invariants
        seed_card(&db, "card-full", "ready");
        let full_dispatch = create_dispatch(
            &db,
            &engine,
            "card-full",
            "agent-1",
            "implementation",
            "Full dispatch",
            &json!({}),
        )
        .unwrap();
        assert_eq!(full_dispatch["status"], "pending");
    }

    #[test]
    fn create_dispatch_core_rejects_done_card() {
        let db = test_db();
        seed_card(&db, "card-done-core", "done");

        let result = create_dispatch_core(
            &db,
            "card-done-core",
            "agent-1",
            "implementation",
            "Should fail",
            &json!({}),
        );
        assert!(result.is_err(), "core should reject done card dispatch");
    }

    #[test]
    fn concurrent_dispatches_for_different_cards_have_distinct_ids() {
        // Regression: concurrent dispatches from different cards must not share
        // dispatch IDs or card state — each must be independently routable.
        let db = test_db();
        let engine = test_engine(&db);
        seed_card(&db, "card-a", "ready");
        seed_card(&db, "card-b", "ready");

        let dispatch_a = create_dispatch(
            &db,
            &engine,
            "card-a",
            "agent-1",
            "implementation",
            "Task A",
            &json!({}),
        )
        .unwrap();

        let dispatch_b = create_dispatch(
            &db,
            &engine,
            "card-b",
            "agent-2",
            "implementation",
            "Task B",
            &json!({}),
        )
        .unwrap();

        let id_a = dispatch_a["id"].as_str().unwrap();
        let id_b = dispatch_b["id"].as_str().unwrap();
        assert_ne!(id_a, id_b, "dispatch IDs must be unique");
        assert_eq!(dispatch_a["kanban_card_id"], "card-a");
        assert_eq!(dispatch_b["kanban_card_id"], "card-b");

        // Each card's latest_dispatch_id points to its own dispatch
        let conn = db.separate_conn().unwrap();
        let latest_a: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_b: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-b'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latest_a, id_a);
        assert_eq!(latest_b, id_b);
        assert_ne!(latest_a, latest_b, "card dispatch IDs must not cross");
    }
}
