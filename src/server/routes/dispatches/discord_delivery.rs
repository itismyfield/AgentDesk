use super::outbox::{format_dispatch_message, prefix_dispatch_message, use_counter_model_channel};
use super::resolve_channel_alias;
use super::thread_reuse::{
    clear_thread_for_channel, get_thread_for_channel, set_thread_for_channel, try_reuse_thread,
};
use crate::db::agents::{
    resolve_agent_dispatch_channel_on_conn, resolve_agent_primary_channel_on_conn,
};

/// Send a dispatch notification to the target agent's Discord channel.
/// Message format: `DISPATCH:<dispatch_id> - <title>\n<issue_url>`
/// The `DISPATCH:<uuid>` prefix is required for the dcserver to link the
/// resulting Claude session back to the kanban card (via `parse_dispatch_id`).
pub(crate) async fn send_dispatch_to_discord(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    // Two-phase delivery guard (prevents duplicates across all callers):
    // 1. Check dispatch_notified (confirmed prior delivery) → skip if present
    // 2. Claim dispatch_reserving (atomic lock) → skip if another path holds it
    // 3. Send to Discord
    // 4. On success: release reserving, commit notified
    // 5. On failure: release reserving, return Err
    // Boot recovery clears stale reserving markers on startup.
    {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for delivery guard".into()),
        };
        // Already confirmed delivered?
        let notified = conn
            .query_row(
                "SELECT 1 FROM kv_meta WHERE key = ?1",
                [&format!("dispatch_notified:{dispatch_id}")],
                |_| Ok(()),
            )
            .is_ok();
        if notified {
            return Ok(()); // Confirmed prior delivery — idempotent skip
        }
        // Atomic reservation claim
        let claimed = conn
            .execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_reserving:{dispatch_id}"), dispatch_id],
            )
            .unwrap_or(0)
            > 0;
        if !claimed {
            return Ok(()); // Another path is actively delivering — skip
        }
    }

    // Wrap the actual send so we can always release the reservation
    let send_result =
        send_dispatch_to_discord_inner(db, agent_id, title, card_id, dispatch_id).await;

    // Release reservation and commit notified marker on success
    if let Ok(conn) = db.lock() {
        conn.execute(
            "DELETE FROM kv_meta WHERE key = ?1",
            [&format!("dispatch_reserving:{dispatch_id}")],
        )
        .ok();
        if send_result.is_ok() {
            conn.execute(
                "INSERT OR IGNORE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("dispatch_notified:{dispatch_id}"), dispatch_id],
            )
            .ok();
        }
    }

    send_result
}

/// Inner function: performs the actual Discord send without reservation logic.
async fn send_dispatch_to_discord_inner(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) -> Result<(), String> {
    // Determine dispatch type to choose the right channel
    let dispatch_type: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for dispatch type query".into()),
        };
        conn.query_row(
            "SELECT dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    // For review dispatches, use the alternate channel (counter-model)
    let use_alt = use_counter_model_channel(dispatch_type.as_deref());

    // #145: Check if this dispatch is in a unified thread auto-queue run (dispatch_id path)
    // #218: Check unified run by dispatch_id first, then card_id for review/rework
    let is_unified_run: bool = db
        .lock()
        .ok()
        .and_then(|conn| {
            let by_dispatch: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM auto_queue_runs r \
                     JOIN auto_queue_entries e ON e.run_id = r.id \
                     WHERE e.dispatch_id = ?1 AND r.unified_thread = 1 AND r.status = 'active'",
                    [dispatch_id],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if by_dispatch {
                return Some(true);
            }
            conn.query_row(
                "SELECT COUNT(*) > 0 FROM auto_queue_runs r \
                 JOIN auto_queue_entries e ON e.run_id = r.id \
                 WHERE e.kanban_card_id = ?1 AND r.unified_thread = 1 AND r.status = 'active'",
                [card_id],
                |row| row.get(0),
            )
            .ok()
        })
        .unwrap_or(false);
    // Each channel (primary/alt) gets its own unified thread — don't override use_alt

    // Look up agent's discord channel
    let channel_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for channel lookup".into()),
        };
        resolve_agent_dispatch_channel_on_conn(&conn, agent_id, dispatch_type.as_deref())
            .ok()
            .flatten()
    };

    let channel_id = match channel_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::warn!(
                "[dispatch] No discord_channel_id for agent {agent_id}, skipping message"
            );
            return Err(format!("no discord channel for agent {agent_id}"));
        }
    };

    // Parse channel ID as u64, or resolve alias via role_map.json
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            // Try resolving channel name alias from role_map.json
            match resolve_channel_alias(&channel_id) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        "[dispatch] Cannot resolve channel '{channel_id}' for agent {agent_id}"
                    );
                    return Err(format!(
                        "cannot resolve channel '{channel_id}' for agent {agent_id}"
                    ));
                }
            }
        }
    };

    // Look up the issue URL and number for context
    let (issue_url, issue_number): (Option<String>, Option<i64>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for issue lookup".into()),
        };
        conn.query_row(
            "SELECT github_issue_url, github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_default()
    };

    // For review dispatches, look up reviewed commit SHA, branch, and target provider from context
    let (reviewed_commit, target_provider, review_branch): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = if use_alt {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for context query".into()),
        };
        let ctx: Option<String> = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let ctx_val: serde_json::Value = ctx
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::json!({}));
        (
            ctx_val
                .get("reviewed_commit")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ctx_val
                .get("target_provider")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            ctx_val
                .get("branch")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        )
    } else {
        (None, None, None)
    };

    // Read dispatch context for reason/source info
    let dispatch_context: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    let message = format_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        use_alt,
        reviewed_commit.as_deref(),
        target_provider.as_deref(),
        review_branch.as_deref(),
        dispatch_type.as_deref(),
        dispatch_context.as_deref(),
    );

    // Send via Discord HTTP API using the announce bot
    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => {
            tracing::warn!(
                "[dispatch] No announce bot token (missing credential/announce_bot_token)"
            );
            return Err("no announce bot token".into());
        }
    };

    // ── Thread reuse: check if card already has an active thread ──
    let client = reqwest::Client::new();
    let dispatch_type_label = dispatch_type.as_deref().unwrap_or("implementation");
    let message = prefix_dispatch_message(dispatch_type_label, &message);

    // #145/#140: Look up per-channel unified thread via dispatch_id path
    // #140: For parallel runs (thread_group_count > 1), threads are grouped:
    //   unified_thread_id = {"0": {"channel_id": "thread_id"}, "1": {"channel_id": "thread_id"}}
    // For non-parallel (thread_group_count == 1), flat format is preserved:
    //   unified_thread_id = {"channel_id": "thread_id"}
    let mut unified_thread_id: Option<String> = db.lock().ok().and_then(|conn| {
        // Get both the map JSON and the entry's thread_group + run's group count
        // #218: Try dispatch_id first, then fall back to card_id for review/rework
        // dispatches that aren't directly linked to auto_queue_entries.
        let row: Option<(String, i64, i64)> = conn
            .query_row(
                "SELECT r.unified_thread_id, COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                 FROM auto_queue_runs r \
                 JOIN auto_queue_entries e ON e.run_id = r.id \
                 WHERE e.dispatch_id = ?1 AND r.unified_thread = 1 AND r.status = 'active' \
                 AND r.unified_thread_id IS NOT NULL",
                [dispatch_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .ok()
            .or_else(|| {
                // #218: Fallback — match by card_id for review/rework dispatches
                conn.query_row(
                    "SELECT r.unified_thread_id, COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                     FROM auto_queue_runs r \
                     JOIN auto_queue_entries e ON e.run_id = r.id \
                     WHERE e.kanban_card_id = ?1 AND r.unified_thread = 1 AND r.status = 'active' \
                     AND r.unified_thread_id IS NOT NULL",
                    [card_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                )
                .ok()
            });
        row.and_then(|(json_str, thread_group, group_count)| {
            if let Ok(map) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if !map.is_object() {
                    return None;
                }
                if group_count > 1 {
                    // Parallel: nested format {"group_num": {"channel_id": "thread_id"}}
                    map.get(&thread_group.to_string())
                        .and_then(|group_map| group_map.get(&channel_id_num.to_string()))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    // Non-parallel: flat format {"channel_id": "thread_id"}
                    map.get(&channel_id_num.to_string())
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                }
            } else {
                None
            }
        })
    });

    // Try to reuse existing thread for this card (channel-specific)
    let existing_thread_id: Option<String> = if unified_thread_id.is_some() {
        unified_thread_id.clone()
    } else {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for thread lookup".into()),
        };
        get_thread_for_channel(&conn, card_id, channel_id_num)
    };

    if let Some(ref existing_tid) = existing_thread_id {
        // Try to unarchive and reuse the existing thread
        if let Some(reused) = try_reuse_thread(
            &client,
            &token,
            existing_tid,
            channel_id_num,
            dispatch_type_label,
            &message,
            dispatch_id,
            card_id,
            db,
        )
        .await
        {
            if reused {
                return Ok(());
            }
        }
    }

    // #145/#140: If unified thread reuse failed, remove this channel from JSON map (dispatch_id path)
    // #140: Handle nested parallel format {"group_num": {"channel_id": "thread_id"}}
    if unified_thread_id.is_some() {
        if let Ok(conn) = db.lock() {
            let row_data: Option<(String, i64, i64)> = conn
                .query_row(
                    "SELECT COALESCE(r.unified_thread_id, '{}'), COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                     FROM auto_queue_runs r \
                     JOIN auto_queue_entries e ON e.run_id = r.id \
                     WHERE e.dispatch_id = ?1 AND r.status IN ('active', 'paused')",
                    [dispatch_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                )
                .ok();
            if let Some((existing, thread_group, group_count)) = row_data {
                if let Ok(mut map) = serde_json::from_str::<serde_json::Value>(&existing) {
                    let ch_key = channel_id_num.to_string();
                    if group_count > 1 {
                        // Parallel: nested format — remove from group sub-map
                        let group_key = thread_group.to_string();
                        if let Some(group_map) =
                            map.get_mut(&group_key).and_then(|v| v.as_object_mut())
                        {
                            group_map.remove(&ch_key);
                        }
                    } else {
                        // Non-parallel: flat format
                        if let Some(obj) = map.as_object_mut() {
                            obj.remove(&ch_key);
                        }
                    }
                    conn.execute(
                        "UPDATE auto_queue_runs SET unified_thread_id = ?1 \
                         WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?2) \
                         AND status IN ('active', 'paused')",
                        rusqlite::params![map.to_string(), dispatch_id],
                    )
                    .ok();
                }
            }
        }
        unified_thread_id = None; // Reset local so new thread creation saves to run below
    }

    // No existing thread or reuse failed — create a new thread
    // #137/#140: For unified thread, build name from queued issue numbers
    // #140: For parallel runs, only show issues in the same thread_group
    let thread_name = if unified_thread_id.is_none() {
        // First dispatch in unified run — check if we should use a combined name
        let unified_issues: Option<String> = db
            .lock()
            .ok()
            .and_then(|conn| {
                // Check if this card is in a unified run and get thread_group info
                // #218: Try dispatch_id first, then card_id for review/rework
                let entry_info: Option<(String, i64, i64)> = conn
                    .query_row(
                        "SELECT e.run_id, COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                         FROM auto_queue_entries e \
                         JOIN auto_queue_runs r ON e.run_id = r.id \
                         WHERE r.unified_thread = 1 AND e.dispatch_id = ?1",
                        [dispatch_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                    )
                    .ok()
                    .or_else(|| {
                        conn.query_row(
                            "SELECT e.run_id, COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                             FROM auto_queue_entries e \
                             JOIN auto_queue_runs r ON e.run_id = r.id \
                             WHERE r.unified_thread = 1 AND e.kanban_card_id = ?1 AND r.status = 'active'",
                            [card_id],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
                        )
                        .ok()
                    });
                let (run_id, thread_group, group_count) = entry_info?;

                // Build issue list — for parallel runs, only show group's issues
                let group_filter = if group_count > 1 {
                    format!(" AND COALESCE(e.thread_group, 0) = {}", thread_group)
                } else {
                    String::new()
                };
                let sql = format!(
                    "SELECT kc.github_issue_number FROM auto_queue_entries e \
                     JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
                     WHERE e.run_id = ?1{} AND kc.github_issue_number IS NOT NULL \
                     ORDER BY e.priority_rank ASC",
                    group_filter
                );
                let mut stmt = conn.prepare(&sql).ok()?;
                let current_issue: Option<i64> = conn
                    .query_row(
                        "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .ok();
                let nums: Vec<String> = stmt
                    .query_map([&run_id], |row| row.get::<_, i64>(0))
                    .ok()?
                    .filter_map(|r| r.ok())
                    .map(|n| {
                        if Some(n) == current_issue {
                            format!("▸{}", n)
                        } else {
                            format!("#{}", n)
                        }
                    })
                    .collect();
                if nums.is_empty() {
                    None
                } else {
                    // For parallel runs, prefix with group number
                    if group_count > 1 {
                        Some(format!("G{} {}", thread_group, nums.join(" ")))
                    } else {
                        Some(nums.join(" "))
                    }
                }
            });

        if let Some(name) = unified_issues {
            // Discord thread name max 100 chars
            name.chars().take(100).collect()
        } else if let Some(num) = issue_number {
            let short: String = title.chars().take(90).collect();
            format!("#{} {}", num, short)
        } else {
            title.chars().take(100).collect()
        }
    } else if let Some(num) = issue_number {
        let short: String = title.chars().take(90).collect();
        format!("#{} {}", num, short)
    } else {
        title.chars().take(100).collect()
    };

    let thread_url = format!(
        "https://discord.com/api/v10/channels/{}/threads",
        channel_id_num
    );
    let thread_resp = client
        .post(&thread_url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({
            "name": thread_name,
            "type": 11, // PUBLIC_THREAD
            "auto_archive_duration": 1440, // 24h
        }))
        .send()
        .await;

    match thread_resp {
        Ok(tr) if tr.status().is_success() => {
            if let Ok(thread_body) = tr.json::<serde_json::Value>().await {
                let thread_id = thread_body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !thread_id.is_empty() {
                    // Send dispatch message into the thread BEFORE persisting thread_id.
                    // If the POST fails, we don't save thread_id so that
                    // [I-0] recovery sends to the channel and future dispatches won't
                    // reuse an empty thread.
                    let thread_msg_url = format!(
                        "https://discord.com/api/v10/channels/{}/messages",
                        thread_id
                    );
                    let thread_msg_ok = client
                        .post(&thread_msg_url)
                        .header("Authorization", format!("Bot {}", token))
                        .json(&serde_json::json!({"content": message}))
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false);
                    if thread_msg_ok {
                        // Persist thread_id on success
                        if let Ok(conn) = db.lock() {
                            conn.execute(
                                "UPDATE task_dispatches SET thread_id = ?1 WHERE id = ?2",
                                rusqlite::params![thread_id, dispatch_id],
                            )
                            .ok();
                            set_thread_for_channel(&conn, card_id, channel_id_num, thread_id);
                            // #141/#140: Store unified thread per channel in JSON map
                            // Save when: no existing thread for this channel (unified_thread_id is None)
                            // AND this card belongs to a unified run
                            if unified_thread_id.is_none() && is_unified_run {
                                // #140/#218: Get thread_group and group_count for this entry
                                // Try dispatch_id first, fall back to card_id for review/rework
                                let (entry_group, group_count): (i64, i64) = conn
                                    .query_row(
                                        "SELECT COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                                         FROM auto_queue_entries e \
                                         JOIN auto_queue_runs r ON e.run_id = r.id \
                                         WHERE e.dispatch_id = ?1",
                                        [dispatch_id],
                                        |row| Ok((row.get(0)?, row.get(1)?)),
                                    )
                                    .or_else(|_| {
                                        conn.query_row(
                                            "SELECT COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                                             FROM auto_queue_entries e \
                                             JOIN auto_queue_runs r ON e.run_id = r.id \
                                             WHERE e.kanban_card_id = ?1 AND r.unified_thread = 1 AND r.status = 'active'",
                                            [card_id],
                                            |row| Ok((row.get(0)?, row.get(1)?)),
                                        )
                                    })
                                    .unwrap_or((0, 1));

                                // Read existing map (try dispatch_id then card_id)
                                let existing: String = conn
                                    .query_row(
                                        "SELECT COALESCE(unified_thread_id, '{}') FROM auto_queue_runs \
                                         WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?1) \
                                         AND status IN ('active', 'paused')",
                                        [dispatch_id],
                                        |row| row.get(0),
                                    )
                                    .or_else(|_| {
                                        conn.query_row(
                                            "SELECT COALESCE(r.unified_thread_id, '{}') FROM auto_queue_runs r \
                                             JOIN auto_queue_entries e ON e.run_id = r.id \
                                             WHERE e.kanban_card_id = ?1 AND r.unified_thread = 1 AND r.status = 'active'",
                                            [card_id],
                                            |row| row.get(0),
                                        )
                                    })
                                    .unwrap_or_else(|_| "{}".to_string());

                                let mut map: serde_json::Value =
                                    serde_json::from_str::<serde_json::Value>(&existing)
                                        .ok()
                                        .filter(|v: &serde_json::Value| v.is_object())
                                        .unwrap_or_else(|| serde_json::json!({}));

                                if group_count > 1 {
                                    // #140: Parallel — nested format {"group_num": {"channel_id": "thread_id"}}
                                    let group_key = entry_group.to_string();
                                    if !map.get(&group_key).map(|v| v.is_object()).unwrap_or(false)
                                    {
                                        map[group_key.clone()] = serde_json::json!({});
                                    }
                                    map[group_key][channel_id_num.to_string()] =
                                        serde_json::json!(thread_id);
                                } else {
                                    // Non-parallel: flat format {"channel_id": "thread_id"}
                                    map[channel_id_num.to_string()] = serde_json::json!(thread_id);
                                }

                                // #218: Try dispatch_id first, then card_id for review/rework
                                let map_str = map.to_string();
                                let updated = conn.execute(
                                    "UPDATE auto_queue_runs SET unified_thread_id = ?1, unified_thread_channel_id = ?2 \
                                     WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE dispatch_id = ?3) \
                                     AND status IN ('active', 'paused')",
                                    rusqlite::params![map_str, thread_id, dispatch_id],
                                )
                                .unwrap_or(0);
                                if updated == 0 {
                                    conn.execute(
                                        "UPDATE auto_queue_runs SET unified_thread_id = ?1, unified_thread_channel_id = ?2 \
                                         WHERE id IN (SELECT run_id FROM auto_queue_entries WHERE kanban_card_id = ?3) \
                                         AND unified_thread = 1 AND status IN ('active', 'paused')",
                                        rusqlite::params![map_str, thread_id, card_id],
                                    )
                                    .ok();
                                }
                            }
                        }
                        tracing::info!(
                            "[dispatch] Created thread {thread_id} and sent dispatch {dispatch_id} to {agent_id}"
                        );
                        return Ok(());
                    } else {
                        tracing::warn!(
                            "[dispatch] Thread message POST failed for dispatch {dispatch_id}"
                        );
                        return Err(format!(
                            "thread message POST failed for dispatch {dispatch_id}"
                        ));
                    }
                }
            }
            // thread_body parse failed or thread_id empty
            return Err("thread created but response parsing failed".into());
        }
        Ok(tr) => {
            // Thread creation failed — fall back to sending directly to the channel
            let status = tr.status();
            tracing::warn!(
                "[dispatch] Thread creation failed ({status}), falling back to channel message"
            );
            let url = format!(
                "https://discord.com/api/v10/channels/{}/messages",
                channel_id_num
            );
            match client
                .post(&url)
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    tracing::info!(
                        "[dispatch] Sent fallback message to {agent_id} (channel {channel_id})"
                    );
                    return Ok(());
                }
                Ok(r) => {
                    let st = r.status();
                    let body = r.text().await.unwrap_or_default();
                    tracing::warn!("[dispatch] Discord API error {st}: {body}");
                    return Err(format!("discord API error {st}: {body}"));
                }
                Err(e) => {
                    tracing::warn!("[dispatch] Request failed: {e}");
                    return Err(format!("discord request failed: {e}"));
                }
            }
        }
        Err(e) => {
            tracing::warn!("[dispatch] Thread creation request failed: {e}");
            return Err(format!("thread creation request failed: {e}"));
        }
    }
}

/// Handle primary-channel followup after a counter-model review completes.
/// pass/unknown verdicts send an immediate message; improve/rework/reject
/// create a review-decision dispatch whose notify row is delivered by outbox.
pub(super) async fn send_review_result_to_primary(
    db: &crate::db::Db,
    card_id: &str,
    verdict: &str,
) -> Result<(), String> {
    // Look up card info
    let (agent_id, title, issue_url): (String, String, Option<String>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for card lookup".into()),
        };
        let result = conn.query_row(
            "SELECT kc.assigned_agent_id, kc.title, kc.github_issue_url \
             FROM kanban_cards kc \
             WHERE kc.id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(r) => r,
            Err(_) => return Err(format!("card {card_id} not found or missing agent")),
        }
    };
    let channel_id = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for primary channel lookup".into()),
        };
        resolve_agent_primary_channel_on_conn(&conn, &agent_id)
            .ok()
            .flatten()
            .ok_or_else(|| format!("agent {agent_id} missing primary discord channel"))?
    };

    // For improve/rework/reject: create a review-decision dispatch via the
    // authoritative path and let the outbox worker deliver the message.
    if verdict != "pass" && verdict != "approved" && verdict != "unknown" {
        // #118: If approach-change already created a rework dispatch (review_status = rework_pending),
        // skip creating the review-decision dispatch to avoid double dispatch.
        {
            let skip = db
                .lock()
                .ok()
                .and_then(|conn| {
                    conn.query_row(
                        "SELECT review_status FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .ok()
                    .flatten()
                })
                .map(|s| s == "rework_pending")
                .unwrap_or(false);
            if skip {
                tracing::info!(
                    "[review-followup] #118 skipping review-decision for {card_id} — approach-change rework already dispatched"
                );
                return Ok(());
            }
        }

        return match crate::dispatch::create_dispatch_core(
            db,
            card_id,
            &agent_id,
            "review-decision",
            &format!("[리뷰 검토] {title}"),
            &serde_json::json!({"verdict": verdict}),
        ) {
            Ok((id, _old_status, _reused)) => {
                if let Ok(conn) = db.lock() {
                    crate::engine::ops::review_state_sync_on_conn(
                        &conn,
                        &serde_json::json!({
                            "card_id": card_id,
                            "state": "suggestion_pending",
                            "pending_dispatch_id": id,
                            "last_verdict": verdict,
                        })
                        .to_string(),
                    );
                }
                tracing::info!(
                    "[review-followup] enqueued review-decision dispatch {} for card {}",
                    id,
                    card_id
                );
                Ok(())
            }
            Err(e) => {
                tracing::warn!(
                    "[review-followup] skipping review-decision dispatch for card {card_id}: {e}"
                );
                Err(format!(
                    "create_dispatch_core failed for review-decision: {e}"
                ))
            }
        };
    }

    // Resolve channel ID (may be a name alias)
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => match resolve_channel_alias(&channel_id) {
            Some(n) => n,
            None => return Err(format!("cannot resolve channel alias '{channel_id}'")),
        },
    };

    let token = match crate::credential::read_bot_token("announce") {
        Some(t) => t,
        None => return Err("no announce bot token".into()),
    };
    let client = reqwest::Client::new();

    // #218: Check unified_thread_id first for auto-queue dispatches
    let active_thread_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for thread lookup".into()),
        };
        // Try unified thread first: find active auto-queue run for this card
        // #218 R2: Handle both flat and nested (parallel) unified_thread_id formats
        let unified: Option<String> = conn
            .query_row(
                "SELECT r.unified_thread_id, COALESCE(e.thread_group, 0), COALESCE(r.thread_group_count, 1) \
                 FROM auto_queue_runs r \
                 JOIN auto_queue_entries e ON e.run_id = r.id \
                 WHERE e.kanban_card_id = ?1 AND r.unified_thread = 1 AND r.status IN ('active', 'paused') \
                 AND r.unified_thread_id IS NOT NULL",
                [card_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
            )
            .ok()
            .and_then(|(json_str, thread_group, group_count)| {
                let map: serde_json::Value = serde_json::from_str(&json_str).ok()?;
                let ch_key = channel_id_num.to_string();
                if group_count > 1 {
                    // Parallel: nested format {"group_num": {"channel_id": "thread_id"}}
                    map.get(&thread_group.to_string())
                        .and_then(|group_map| group_map.get(&ch_key))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    // Non-parallel: flat format {"channel_id": "thread_id"}
                    map.get(&ch_key)
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                }
            });
        // Fall back to card's channel_thread_map
        unified.or_else(|| get_thread_for_channel(&conn, card_id, channel_id_num))
    };
    // Use resolved numeric channel ID for Discord API calls
    let channel_id = channel_id_num.to_string();

    // Determine target: existing thread from primary channel (if valid) or main channel.
    let target_channel = if let Some(ref tid) = active_thread_id {
        let info_url = format!("https://discord.com/api/v10/channels/{}", tid);
        let valid = match client
            .get(&info_url)
            .header("Authorization", format!("Bot {}", &token))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    let locked = body
                        .get("thread_metadata")
                        .and_then(|m| m.get("locked"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    !locked
                } else {
                    false
                }
            }
            _ => false,
        };
        if valid {
            // Unarchive if needed — check result and fallback to channel on failure
            let unarchive_ok = match client
                .patch(&info_url)
                .header("Authorization", format!("Bot {}", &token))
                .json(&serde_json::json!({"archived": false}))
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => true,
                Ok(r) => {
                    tracing::warn!(
                        "[review] Failed to unarchive thread {tid}: HTTP {}",
                        r.status()
                    );
                    false
                }
                Err(e) => {
                    tracing::warn!("[review] Failed to unarchive thread {tid}: {e}");
                    false
                }
            };
            if unarchive_ok {
                tid.clone()
            } else {
                // Unarchive failed — clear stale channel-thread mapping and fall back to channel
                if let Ok(conn) = db.lock() {
                    clear_thread_for_channel(&conn, card_id, channel_id_num);
                }
                channel_id.clone()
            }
        } else {
            // Thread is locked or inaccessible — clear stale channel-thread mapping and fall back to channel
            if let Ok(conn) = db.lock() {
                clear_thread_for_channel(&conn, card_id, channel_id_num);
            }
            channel_id.clone()
        }
    } else {
        channel_id.clone()
    };
    // For pass/approved verdict, just send a simple notification (no action needed).
    // #116: accept is NOT a counter-model verdict — it's a review-decision action.
    if verdict == "pass" || verdict == "approved" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!("✅ [리뷰 통과] {title} — done으로 이동{url_line}");

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        match client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                return Err(format!(
                    "discord API error {} for pass notification",
                    r.status()
                ));
            }
            Err(e) => return Err(format!("discord request failed for pass notification: {e}")),
        }
    }

    // For unknown verdict (e.g. session idle auto-completed without verdict submission),
    // notify the original agent to check GitHub comments and decide.
    if verdict == "unknown" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!(
            "⚠️ [리뷰 verdict 미제출] {title}\n\
             ⛔ 코드 리뷰 금지 — 이것은 리뷰 결과 확인 요청입니다\n\
             카운터모델이 verdict를 제출하지 않고 세션이 종료됐습니다.\n\
             GitHub 이슈 코멘트를 확인하고 리뷰 내용이 있으면 반영해주세요.{url_line}"
        );
        let message = prefix_dispatch_message("review-decision", &message);

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            target_channel
        );
        match client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await
        {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) => {
                return Err(format!(
                    "discord API error {} for unknown-verdict notification",
                    r.status()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "discord request failed for unknown-verdict notification: {e}"
                ));
            }
        }
    }

    unreachable!("explicit review verdicts should return earlier");
}
