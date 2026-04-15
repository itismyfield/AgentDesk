use crate::db::Db;
use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Kanban ops ────────────────────────────────────────────────────
//
// agentdesk.kanban.setStatus(cardId, newStatus, force?) — updates card status
// and fires appropriate hooks (OnCardTransition, OnCardTerminal, OnReviewEnter).
// This replaces direct SQL UPDATEs in policies to ensure hooks always fire.

pub(super) fn register_kanban_ops<'js>(ctx: &Ctx<'js>, db: Db) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;
    let kanban_obj = Object::new(ctx.clone())?;

    let db_set = db.clone();
    kanban_obj.set(
        "__setStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, new_status: String, force: Option<bool>| -> String {
                let conn = match db_set.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
                };
                let force = force.unwrap_or(false);

                // Get current status
                let old_status: String = match conn.query_row(
                    "SELECT status FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |row| row.get(0),
                ) {
                    Ok(s) => s,
                    Err(_) => return r#"{"error":"card not found"}"#.to_string(),
                };

                if old_status == new_status {
                    return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
                }

                // Pipeline-driven guard and clock fields (#106 P5)
                // Resolve effective pipeline for this card (repo + agent overrides)
                crate::pipeline::ensure_loaded();
                let repo_id: Option<String> = conn
                    .query_row("SELECT repo_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| {
                        r.get(0)
                    })
                    .ok()
                    .flatten();
                let agent_id: Option<String> = conn
                    .query_row(
                        "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                        [&card_id],
                        |r| r.get(0),
                    )
                    .ok()
                    .flatten();
                let effective =
                    crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
                let pipeline = &effective;

                // Guard: prevent reverting terminal cards
                if pipeline.is_terminal(&old_status) && old_status != new_status {
                    return format!(
                        r#"{{"error":"cannot revert terminal card from {} to {}"}}"#,
                        old_status, new_status
                    );
                }

                // #228: Enforce review_verdict_pass gate on transitions to terminal states.
                // Only this specific gate is checked — other gates (has_active_dispatch,
                // review_rework) are used legitimately by policies and must not be blocked here.
                if pipeline.is_terminal(&new_status)
                    && !force
                    && let Some(t) = pipeline.find_transition(&old_status, &new_status)
                {
                    let needs_review_pass = t.gates.iter().any(|g| {
                        pipeline
                            .gates
                            .get(g.as_str())
                            .is_some_and(|gc| gc.check.as_deref() == Some("review_verdict_pass"))
                    });
                    if needs_review_pass {
                        // Mirror kanban.rs:112-125 — check the LATEST completed review
                        // dispatch only, not any historical pass. A card with pass R1
                        // then rework R2 must not skip the current review round.
                        let latest_verdict: Option<String> = conn
                            .query_row(
                                "SELECT json_extract(result, '$.verdict') FROM task_dispatches \
                                 WHERE kanban_card_id = ?1 AND dispatch_type = 'review' \
                                 AND status = 'completed' \
                                 ORDER BY updated_at DESC LIMIT 1",
                                [&card_id],
                                |row| row.get(0),
                            )
                            .ok()
                            .flatten();
                        let has_pass =
                            matches!(latest_verdict.as_deref(), Some("pass") | Some("approved"));
                        if !has_pass {
                            return format!(
                                r#"{{"error":"gate blocked: review_verdict_pass — no review pass verdict","from":"{}","to":"{}"}}"#,
                                old_status, new_status
                            );
                        }
                    }
                }

                // Clock fields from pipeline config
                let clock_extra = match pipeline.clock_for_state(&new_status) {
                    Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                        format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                    }
                    Some(clock) => format!(", {} = datetime('now')", clock.set),
                    None => String::new(),
                };
                // Terminal cleanup: clear review-related fields
                let terminal_cleanup = if pipeline.is_terminal(&new_status) {
                    ", review_status = NULL, suggestion_pending_at = NULL, review_entered_at = NULL, awaiting_dod_at = NULL, blocked_reason = NULL, review_round = NULL, deferred_dod_json = NULL"
                } else {
                    ""
                };
                let extra = format!("{clock_extra}{terminal_cleanup}");
                let sql = format!(
                    "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now'){} WHERE id = ?2",
                    extra
                );
                if let Err(e) = conn.execute(&sql, rusqlite::params![new_status, card_id]) {
                    return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
                }

                // Also update auto_queue_entries if terminal
                if pipeline.is_terminal(&new_status) {
                    sync_auto_queue_terminal_on_conn(&conn, &card_id);
                }

                // #117/#158: Sync canonical review state via unified entrypoint
                let has_hooks = pipeline
                    .hooks_for_state(&new_status)
                    .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
                let is_review_enter = pipeline
                    .hooks_for_state(&new_status)
                    .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
                if pipeline.is_terminal(&new_status) || !has_hooks {
                    review_state_sync_on_conn(
                        &conn,
                        &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string(),
                    );
                } else if is_review_enter {
                    review_state_sync_on_conn(
                        &conn,
                        &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
                    );
                }

                format!(
                    r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}"}}"#,
                    old_status, new_status, card_id
                )
            },
        )?,
    )?;

    let db_reopen = db.clone();
    kanban_obj.set(
        "__reopenRaw",
        Function::new(ctx.clone(), move |card_id: String, new_status: String| -> String {
            let conn = match db_reopen.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"DB lock: {}"}}"#, e),
            };

            let old_status: String = match conn.query_row(
                "SELECT status FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            ) {
                Ok(s) => s,
                Err(_) => return r#"{"error":"card not found"}"#.to_string(),
            };

            crate::pipeline::ensure_loaded();
            let repo_id: Option<String> = conn
                .query_row("SELECT repo_id FROM kanban_cards WHERE id = ?1", [&card_id], |r| {
                    r.get(0)
                })
                .ok()
                .flatten();
            let agent_id: Option<String> = conn
                .query_row(
                    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1",
                    [&card_id],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let effective =
                crate::pipeline::resolve_for_card(&conn, repo_id.as_deref(), agent_id.as_deref());
            let pipeline = &effective;

            if !pipeline.is_terminal(&old_status) {
                return format!(
                    r#"{{"error":"reopen requires terminal card (current: {})"}}"#,
                    old_status
                );
            }
            if pipeline.is_terminal(&new_status) {
                return format!(
                    r#"{{"error":"reopen target must be non-terminal (target: {})"}}"#,
                    new_status
                );
            }
            if old_status == new_status {
                return format!(r#"{{"ok":true,"changed":false,"status":"{}"}}"#, new_status);
            }

            let clock_extra = match pipeline.clock_for_state(&new_status) {
                Some(clock) if clock.mode.as_deref() == Some("coalesce") => {
                    format!(", {} = COALESCE({}, datetime('now'))", clock.set, clock.set)
                }
                Some(clock) => format!(", {} = datetime('now')", clock.set),
                None => String::new(),
            };

            let sql = format!(
                "UPDATE kanban_cards SET status = ?1, completed_at = NULL, updated_at = datetime('now'){} WHERE id = ?2",
                clock_extra
            );
            if let Err(e) = conn.execute(&sql, rusqlite::params![new_status, card_id]) {
                return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
            }

            let entry_ids: Vec<String> = conn
                .prepare(
                    "SELECT id FROM auto_queue_entries
                     WHERE kanban_card_id = ?1 AND status = 'done'",
                )
                .ok()
                .and_then(|mut stmt| {
                    stmt.query_map([&card_id], |row| row.get::<_, String>(0))
                        .ok()
                        .map(|rows| rows.filter_map(|row| row.ok()).collect())
                })
                .unwrap_or_default();
            for entry_id in entry_ids {
                if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
                    &conn,
                    &entry_id,
                    crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                    "js_reopen",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                ) {
                    return format!(r#"{{"error":"{}"}}"#, error);
                }
            }

            crate::kanban::correct_tn_to_fn_on_reopen(&db_reopen, &card_id);

            let has_hooks = pipeline
                .hooks_for_state(&new_status)
                .is_some_and(|h| !h.on_enter.is_empty() || !h.on_exit.is_empty());
            let is_review_enter = pipeline
                .hooks_for_state(&new_status)
                .is_some_and(|h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));
            if !has_hooks {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "idle"}).to_string(),
                );
            } else if is_review_enter {
                review_state_sync_on_conn(
                    &conn,
                    &serde_json::json!({"card_id": card_id, "state": "reviewing"}).to_string(),
                );
            }

            format!(
                r#"{{"ok":true,"changed":true,"from":"{}","to":"{}","card_id":"{}","reopened":true}}"#,
                old_status, new_status, card_id
            )
        })?,
    )?;

    let db_get = db.clone();
    kanban_obj.set(
        "__getCardRaw",
        Function::new(ctx.clone(), move |card_id: String| -> String {
            let conn = match db_get.separate_conn() {
                Ok(c) => c,
                Err(e) => return format!(r#"{{"error":"{}"}}"#, e),
            };
            match conn.query_row(
                "SELECT id, status, assigned_agent_id, title, review_status, review_round, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| {
                    Ok(serde_json::json!({
                        "id": row.get::<_, String>(0)?,
                        "status": row.get::<_, String>(1)?,
                        "assigned_agent_id": row.get::<_, Option<String>>(2)?,
                        "title": row.get::<_, Option<String>>(3)?,
                        "review_status": row.get::<_, Option<String>>(4)?,
                        "review_round": row.get::<_, Option<i64>>(5)?,
                        "latest_dispatch_id": row.get::<_, Option<String>>(6)?,
                    }))
                },
            ) {
                Ok(card) => card.to_string(),
                Err(_) => r#"{"error":"card not found"}"#.to_string(),
            }
        })?,
    )?;

    // #155: setReviewStatus — controlled path for review_status + clock updates.
    // Replaces direct SQL UPDATEs so the ExecuteSQL guard can block bare review_status writes.
    let db_review = db.clone();
    kanban_obj.set(
        "__setReviewStatusRaw",
        Function::new(
            ctx.clone(),
            move |card_id: String, opts_json: String| -> String {
                let opts: serde_json::Value = match serde_json::from_str(&opts_json) {
                    Ok(v) => v,
                    Err(e) => return format!(r#"{{"error":"bad opts: {}"}}"#, e),
                };
                let conn = match db_review.separate_conn() {
                    Ok(c) => c,
                    Err(e) => return format!(r#"{{"error":"DB: {}"}}"#, e),
                };

                // Build dynamic SET clause
                let mut sets = vec!["updated_at = datetime('now')".to_string()];
                let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![];

                if let Some(rs) = opts.get("review_status") {
                    if rs.is_null() {
                        sets.push("review_status = NULL".to_string());
                    } else if let Some(s) = rs.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("review_status = ?{}", params.len()));
                    }
                }
                if let Some(v) = opts.get("suggestion_pending_at") {
                    if v.is_null() {
                        sets.push("suggestion_pending_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("suggestion_pending_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("review_entered_at") {
                    if v.is_null() {
                        sets.push("review_entered_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("review_entered_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("awaiting_dod_at") {
                    if v.is_null() {
                        sets.push("awaiting_dod_at = NULL".to_string());
                    } else if v.as_str() == Some("now") {
                        sets.push("awaiting_dod_at = datetime('now')".to_string());
                    }
                }
                if let Some(v) = opts.get("blocked_reason") {
                    if v.is_null() {
                        sets.push("blocked_reason = NULL".to_string());
                    } else if let Some(s) = v.as_str() {
                        params.push(Box::new(s.to_string()));
                        sets.push(format!("blocked_reason = ?{}", params.len()));
                    }
                }

                // Optional terminal guard: only update if status != terminal
                let where_clause = if let Some(excl) = opts.get("exclude_status") {
                    if let Some(s) = excl.as_str() {
                        params.push(Box::new(s.to_string()));
                        params.push(Box::new(card_id.clone()));
                        format!(
                            "WHERE id = ?{} AND status != ?{}",
                            params.len(),
                            params.len() - 1
                        )
                    } else {
                        params.push(Box::new(card_id.clone()));
                        format!("WHERE id = ?{}", params.len())
                    }
                } else {
                    params.push(Box::new(card_id.clone()));
                    format!("WHERE id = ?{}", params.len())
                };

                let sql = format!(
                    "UPDATE kanban_cards SET {} {}",
                    sets.join(", "),
                    where_clause
                );
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params.iter().map(|p| p.as_ref()).collect();
                if let Err(e) = conn.execute(&sql, param_refs.as_slice()) {
                    return format!(r#"{{"error":"UPDATE: {}"}}"#, e);
                }

                // #117/#158: Sync card_review_state via unified entrypoint
                if let Some(rs) = opts.get("review_status") {
                    let review_state = if rs.is_null() {
                        Some("idle")
                    } else {
                        rs.as_str()
                    };
                    if let Some(s) = review_state {
                        review_state_sync_on_conn(
                            &conn,
                            &serde_json::json!({"card_id": card_id, "state": s}).to_string(),
                        );
                    }
                }

                r#"{"ok":true}"#.to_string()
            },
        )?,
    )?;

    ad.set("kanban", kanban_obj)?;

    // JS wrapper that parses JSON and accumulates transitions for post-hook processing.
    // setStatus only updates the DB — transition hooks (OnCardTransition, OnReviewEnter,
    // OnCardTerminal) cannot fire from within a hook because the engine is not reentrant.
    // Instead, transitions are collected in __pendingTransitions and the Rust caller
    // processes them after the hook returns via drain_pending_transitions().
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var raw = agentdesk.kanban.__setStatusRaw;
            var reopenRaw = agentdesk.kanban.__reopenRaw;
            var getRaw = agentdesk.kanban.__getCardRaw;
            agentdesk.kanban.__pendingTransitions = [];
            agentdesk.kanban.setStatus = function(cardId, newStatus, force) {
                var result = JSON.parse(raw(cardId, newStatus, !!force));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[setStatus] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[setStatus] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.reopen = function(cardId, newStatus) {
                var result = JSON.parse(reopenRaw(cardId, newStatus));
                if (result.error) throw new Error(result.error);
                if (result.changed) {
                    agentdesk.kanban.__pendingTransitions.push({
                        card_id: result.card_id,
                        from: result.from,
                        to: result.to
                    });
                    agentdesk.log.info("[reopen] " + result.card_id + " " + result.from + " -> " + result.to + " (pendingLen=" + agentdesk.kanban.__pendingTransitions.length + ")");
                } else {
                    agentdesk.log.info("[reopen] " + cardId + " -> " + newStatus + " (no-change)");
                }
                return result;
            };
            agentdesk.kanban.getCard = function(cardId) {
                var result = JSON.parse(getRaw(cardId));
                if (result.error) return null;
                return result;
            };
            var reviewRaw = agentdesk.kanban.__setReviewStatusRaw;
            agentdesk.kanban.setReviewStatus = function(cardId, reviewStatus, opts) {
                var o = opts || {};
                o.review_status = reviewStatus;
                var result = JSON.parse(reviewRaw(cardId, JSON.stringify(o)));
                if (result.error) throw new Error(result.error);
                return result;
            };
        })();
    "#,
    )?;

    Ok(())
}

/// Rust implementation of card_review_state sync (#158).
/// Single entrypoint for all review-state mutations.
/// Used by both the JS bridge and Rust route handlers.
pub(super) fn review_state_sync(db: &Db, json_str: &str) -> String {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => return format!(r#"{{"error":"db error: {}"}}"#, e),
    };
    review_state_sync_on_conn(&conn, json_str)
}

/// Best-effort auto-queue cleanup for terminal cards.
///
/// When a card finishes, its active dispatch entry should become `done` and any
/// stale pending copies in active or paused runs should be skipped so they do
/// not block other runs.
pub(super) fn sync_auto_queue_terminal_on_conn(conn: &rusqlite::Connection, card_id: &str) {
    let dispatched_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1 AND status = 'dispatched'",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in dispatched_ids {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_DONE,
            "card_terminal",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to mark entry {} done during terminal sync: {}",
                entry_id,
                error
            );
        }
    }

    let pending_ids: Vec<String> = conn
        .prepare(
            "SELECT id FROM auto_queue_entries
             WHERE kanban_card_id = ?1
               AND status = 'pending'
               AND run_id IN (
                   SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused')
               )",
        )
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([card_id], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|row| row.ok()).collect())
        })
        .unwrap_or_default();
    for entry_id in pending_ids {
        if let Err(error) = crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "card_terminal_pending_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        ) {
            tracing::warn!(
                "[auto-queue] failed to skip pending entry {} during terminal sync: {}",
                entry_id,
                error
            );
        }
    }
}

/// Skip live auto-queue entries for a card after PMD explicitly backs the card out.
///
/// Only active/paused runs are touched. Generated or future runs stay intact so
/// PMD can intentionally re-queue the card later after fixing prerequisites.
pub(super) fn skip_live_auto_queue_entries_for_card_on_conn(
    conn: &rusqlite::Connection,
    card_id: &str,
) -> rusqlite::Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id FROM auto_queue_entries
         WHERE kanban_card_id = ?1
           AND status IN ('pending', 'dispatched')
           AND run_id IN (SELECT id FROM auto_queue_runs WHERE status IN ('active', 'paused'))",
    )?;
    let entry_ids: Vec<String> = stmt
        .query_map([card_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut changed = 0usize;
    for entry_id in entry_ids {
        if crate::db::auto_queue::update_entry_status_on_conn(
            conn,
            &entry_id,
            crate::db::auto_queue::ENTRY_STATUS_SKIPPED,
            "force_transition_cleanup",
            &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
        )
        .map_err(|error| match error {
            crate::db::auto_queue::EntryStatusUpdateError::Sql(sql) => sql,
            other => rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                other.to_string(),
            ))),
        })?
        .changed
        {
            changed += 1;
        }
    }

    Ok(changed)
}

/// Same as `review_state_sync` but operates on an already-acquired connection.
/// Use this inside transactions or when a lock is already held (#158).
pub(super) fn review_state_sync_on_conn(conn: &rusqlite::Connection, json_str: &str) -> String {
    let params: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => return format!(r#"{{"error":"invalid JSON: {}"}}"#, e),
    };

    let card_id = params["card_id"].as_str().unwrap_or("");
    let state = params["state"].as_str().unwrap_or("");
    if card_id.is_empty() || state.is_empty() {
        return r#"{"error":"card_id and state are required"}"#.to_string();
    }

    // Special case: clear_verdict only NULLs last_verdict without changing state
    if state == "clear_verdict" {
        let result = conn.execute(
            "UPDATE card_review_state SET last_verdict = NULL, updated_at = datetime('now') WHERE card_id = ?1",
            rusqlite::params![card_id],
        );
        return match result {
            Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
            Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
        };
    }

    // Build dynamic SET clause based on provided fields
    let review_round = params["review_round"].as_i64();
    let last_verdict = params["last_verdict"].as_str();
    let last_decision = params["last_decision"].as_str();
    let pending_dispatch_id = params["pending_dispatch_id"].as_str();
    let approach_change_round = params["approach_change_round"].as_i64();
    let session_reset_round = params["session_reset_round"].as_i64();
    let review_entered_at = params["review_entered_at"].as_str();

    // UPSERT: INSERT OR REPLACE with all fields
    let result = conn.execute(
        "INSERT INTO card_review_state (card_id, state, review_round, last_verdict, last_decision, pending_dispatch_id, approach_change_round, session_reset_round, review_entered_at, updated_at) \
         VALUES (?1, ?2, COALESCE(?3, 0), ?4, ?5, ?6, ?7, ?8, COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE NULL END), datetime('now')) \
         ON CONFLICT(card_id) DO UPDATE SET \
         state = ?2, \
         review_round = COALESCE(?3, review_round), \
         last_verdict = COALESCE(?4, last_verdict), \
         last_decision = COALESCE(?5, last_decision), \
         pending_dispatch_id = CASE \
             WHEN ?6 IS NOT NULL THEN ?6 \
             WHEN ?2 = 'suggestion_pending' THEN pending_dispatch_id \
             ELSE NULL \
         END, \
         approach_change_round = COALESCE(?7, approach_change_round), \
         session_reset_round = COALESCE(?8, session_reset_round), \
         review_entered_at = COALESCE(?9, CASE WHEN ?2 = 'reviewing' THEN datetime('now') ELSE review_entered_at END), \
         updated_at = datetime('now')",
        rusqlite::params![
            card_id,
            state,
            review_round,
            last_verdict,
            last_decision,
            pending_dispatch_id,
            approach_change_round,
            session_reset_round,
            review_entered_at,
        ],
    );

    match result {
        Ok(n) => format!(r#"{{"ok":true,"rows_affected":{n}}}"#),
        Err(e) => format!(r#"{{"error":"sql error: {}"}}"#, e),
    }
}
