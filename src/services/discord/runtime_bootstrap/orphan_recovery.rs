use super::super::*;
use sqlx::Row as SqlxRow;

pub(super) static STARTUP_THREAD_MAP_VALIDATION_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(super) fn spawn_startup_thread_map_validation(pg_pool: Option<sqlx::PgPool>, token: String) {
    tokio::spawn(async move {
        let (checked, cleared) =
            crate::services::dispatches::discord_delivery::validate_channel_thread_maps_on_startup_with_backends(
                pg_pool.as_ref(),
                &token,
            )
            .await;
        if checked > 0 || cleared > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
            );
        }
    });
}

/// Remove the retired durable handoff tree without parsing or executing it.
/// Current recovery paths no longer write these JSON records, so any files here
/// are legacy residue and must not affect boot behavior.
pub(super) fn purge_legacy_durable_handoffs() {
    let Some(root) = crate::services::discord::runtime_store::legacy_discord_handoff_root() else {
        return;
    };
    if !root.exists() {
        return;
    }
    match std::fs::remove_dir_all(&root) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Removed retired durable handoff directory: {}",
                root.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to remove retired durable handoff directory {}: {error}",
                root.display()
            );
        }
    }
}

/// #164: Re-deliver orphan pending dispatches after dcserver restart.
///
/// After a restart, dispatches in `pending` status may have been Discord-notified
/// but the in-memory intervention_queue was lost. Or the notification was interrupted
/// mid-flight. This function identifies truly orphan dispatches and re-delivers them.
///
/// **Safety**:
/// - Process-global once guard via `std::sync::Once` — safe across multiple provider instances
/// - Startup boot timestamp from dcserver.pid mtime — not wall clock
/// - Newer-dispatch check uses rowid (monotonic) instead of created_at (second-granularity)
/// - Five AND conditions must ALL be met before re-delivery (see issue #164)
pub(super) async fn recover_orphan_pending_dispatches(shared: &Arc<SharedData>) {
    // Process-global once guard: prevents duplicate execution when multiple
    // provider instances (Claude + Codex) call this from their own setup paths.
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut should_run = false;
    ONCE.call_once(|| should_run = true);
    if !should_run {
        return;
    }
    recover_orphan_pending_dispatches_once(shared.pg_pool.as_ref()).await;
}

async fn recover_orphan_pending_dispatches_once(pg_pool: Option<&sqlx::PgPool>) {
    clear_stale_session_dispatch_links(pg_pool).await;

    // Boot timestamp from dcserver.pid mtime — represents actual process start,
    // not a wall-clock offset that could mis-classify old pending dispatches.
    let boot_time: String = {
        let pid_path =
            crate::cli::agentdesk_runtime_root().map(|r| r.join("runtime").join("dcserver.pid"));
        let mtime = pid_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        match mtime {
            Some(t) => {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            }
            None => {
                // No pid file — cannot determine boot time safely, skip recovery
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ #164: No dcserver.pid — skipping orphan dispatch recovery"
                );
                return;
            }
        }
    };

    // Query orphan pending dispatches with all 5 safety conditions:
    // 1. status = 'pending'
    // 2. card is assigned to the dispatch target agent
    // 3. agent has NO working session (idle)
    // 4. created_at < boot_time (pre-restart, using pid mtime)
    // 5. no newer dispatch for the same card (using rowid for monotonic ordering,
    //    avoids same-second ambiguity with created_at)
    let orphans: Vec<(String, String, String, String, String)> = if let Some(pool) = pg_pool {
        match sqlx::query(
            "SELECT d.id, d.to_agent_id, d.kanban_card_id, d.title, d.dispatch_type
               FROM task_dispatches d
               JOIN kanban_cards kc ON kc.id = d.kanban_card_id
              WHERE d.status = 'pending'
                AND d.created_at < $1::timestamptz
                AND kc.assigned_agent_id = d.to_agent_id
                AND NOT EXISTS (
                    SELECT 1 FROM sessions s
                     WHERE s.agent_id = d.to_agent_id
                       AND s.status IN ('turn_active', 'working')
                )
                AND NOT EXISTS (
                    SELECT 1 FROM task_dispatches d2
                     WHERE d2.kanban_card_id = d.kanban_card_id
                       AND d2.status NOT IN ('cancelled', 'failed')
                       AND d2.created_at > d.created_at
                )",
        )
        .bind(&boot_time)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| {
                    Some((
                        row.try_get::<String, _>("id").ok()?,
                        row.try_get::<String, _>("to_agent_id").ok()?,
                        row.try_get::<String, _>("kanban_card_id").ok()?,
                        row.try_get::<String, _>("title").ok()?,
                        row.try_get::<String, _>("dispatch_type").ok()?,
                    ))
                })
                .collect(),
            Err(error) => {
                tracing::warn!(
                    "[dispatch-recovery] failed to query postgres orphan dispatches: {error}"
                );
                return;
            }
        }
    } else {
        return;
    };

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🔄 #164: Found {} orphan pending dispatch(es) to re-deliver",
        orphans.len()
    );

    let mut delivered = 0usize;
    for (dispatch_id, agent_id, card_id, _title, dtype) in &orphans {
        if dispatch_target_held(pg_pool, dispatch_id, agent_id, card_id, dtype).await {
            tracing::info!(
                dispatch_id = %dispatch_id,
                "orphan dispatch recovery left a protected channel's dispatch in place"
            );
            continue;
        }
        // Clear any existing dispatch_notified marker — the 5-condition query already
        // validated this dispatch is truly orphan, so the marker (if any) is stale.
        {
            let dispatch_notified_key = format!("dispatch_notified:{dispatch_id}");
            if crate::services::discord::internal_api::delete_kv_value(&dispatch_notified_key)
                .is_err()
            {
                if let Some(pool) = pg_pool {
                    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                        .bind(&dispatch_notified_key)
                        .execute(pool)
                        .await
                        .ok();
                }
            }
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}]   ↻ Recovering {dtype} dispatch {id} → {agent} (card {card})",
            id = &dispatch_id[..8],
            agent = agent_id,
            card = &card_id[..8.min(card_id.len())],
        );

        let recovery_result = if let Some(pool) = pg_pool {
            crate::db::dispatches::outbox::requeue_dispatch_notify_pg(pool, dispatch_id)
                .await
                .map(|queued| {
                    if !queued {
                        tracing::info!(
                            "  [{}]   · Skipped orphan recovery for {id} (dispatch not requeueable)",
                            chrono::Local::now().format("%H:%M:%S"),
                            id = &dispatch_id[..8],
                        );
                    }
                    queued
                })
        } else {
            continue;
        };
        match recovery_result {
            Ok(true) => {
                delivered += 1;
            }
            Ok(false) => {}
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}]   ⚠ Recovery delivery failed for {id}: {e}",
                    id = &dispatch_id[..8],
                );
            }
        }
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] ✓ #164: Re-delivered {delivered}/{} orphan dispatch(es)",
        orphans.len()
    );
}

/// Whether the dispatch's delivery channel, resolved as its notify would, is protected,
/// or cannot be placed while any channel is protected.
async fn dispatch_target_held(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    agent_id: &str,
    card_id: &str,
    dispatch_type: &str,
) -> bool {
    use crate::services::discord::input_runtime::fence::{self, BootTarget};
    let Some(pool) = pg_pool.filter(|_| fence::any_protected()) else {
        return false;
    };
    let context = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await;
    let channel = match context {
        Ok(Some(context)) => crate::db::dispatches::resolve_dispatch_delivery_channel_pg(
            pool,
            agent_id,
            card_id,
            Some(dispatch_type),
            context.as_deref(),
        )
        .await
        .ok()
        .flatten()
        .and_then(|channel| channel.parse::<u64>().ok()),
        _ => None,
    };
    fence::boot_skip(channel.map_or(BootTarget::Unknown, BootTarget::AnyProvider))
}

/// Session ids the hygiene pass leaves linked: a protected channel's, and any that no
/// channel column places while a channel is protected. `None` holds the whole pass.
async fn hygiene_kept_sessions(pool: &sqlx::PgPool) -> Option<Vec<i64>> {
    use crate::services::discord::input_runtime::fence::{self, BootTarget};
    if !fence::any_protected() {
        return Some(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT id, channel_id, thread_channel_id FROM sessions
          WHERE active_dispatch_id IS NOT NULL",
    )
    .fetch_all(pool)
    .await;
    let Ok(rows) = rows else {
        return (!fence::boot_skip(BootTarget::Unknown)).then(Vec::new);
    };
    let held = |row: &sqlx::postgres::PgRow| {
        let channel = |column| {
            let value = row.try_get::<Option<String>, _>(column).ok().flatten();
            value.and_then(|value| value.parse::<u64>().ok())
        };
        let channels = [channel("thread_channel_id"), channel("channel_id")];
        if channels.iter().all(Option::is_none) {
            return fence::boot_skip(BootTarget::Unknown);
        }
        let mut channels = channels.into_iter().flatten();
        channels.any(|channel| fence::boot_skip(BootTarget::AnyProvider(channel)))
    };
    let kept = rows.iter().filter(|row| held(row));
    Some(
        kept.filter_map(|row| row.try_get::<i64, _>("id").ok())
            .collect(),
    )
}

async fn clear_stale_session_dispatch_links(pg_pool: Option<&sqlx::PgPool>) {
    let Some(pool) = pg_pool else {
        return;
    };
    let Some(kept) = hygiene_kept_sessions(pool).await else {
        return;
    };
    // With nothing kept both statements run exactly as before.
    let keep = if kept.is_empty() {
        ""
    } else {
        "AND NOT (s.id = ANY($1))"
    };

    let sql = format!(
        "UPDATE sessions s
            SET status = CASE
                    WHEN s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                    ELSE s.status
                END,
                active_dispatch_id = NULL,
                session_info = 'Cleared stale terminal dispatch link',
                last_heartbeat = NOW()
           FROM task_dispatches d
          WHERE s.active_dispatch_id = d.id
            AND d.status IN ('completed', 'failed', 'cancelled')
            {keep}
      RETURNING s.session_key, d.id AS dispatch_id, d.status AS dispatch_status",
    );
    let query = sqlx::query(&sql);
    let query = if kept.is_empty() {
        query
    } else {
        query.bind(kept.as_slice())
    };
    match query.fetch_all(pool).await {
        Ok(rows) => {
            if !rows.is_empty() {
                let sample = rows
                    .iter()
                    .take(5)
                    .filter_map(|row| {
                        let session_key = row.try_get::<String, _>("session_key").ok()?;
                        let dispatch_id = row.try_get::<String, _>("dispatch_id").ok()?;
                        let dispatch_status = row.try_get::<String, _>("dispatch_status").ok()?;
                        Some(format!("{session_key}:{dispatch_id}:{dispatch_status}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!(
                    cleared = rows.len(),
                    sample = %sample,
                    "cleared stale terminal active_dispatch_id links during startup recovery"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to clear stale terminal active_dispatch_id links during startup recovery"
            );
        }
    }

    let sql = format!(
        "WITH stale AS (
             SELECT s.session_key
               FROM sessions s
              WHERE s.active_dispatch_id IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM task_dispatches d WHERE d.id = s.active_dispatch_id
                )
                {keep}
         )
         UPDATE sessions s
            SET status = CASE
                    WHEN s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                    ELSE s.status
                END,
                active_dispatch_id = NULL,
                session_info = 'Cleared missing dispatch link',
                last_heartbeat = NOW()
           FROM stale
          WHERE s.session_key = stale.session_key
      RETURNING s.session_key",
    );
    let query = sqlx::query(&sql);
    let query = if kept.is_empty() {
        query
    } else {
        query.bind(kept.as_slice())
    };
    match query.fetch_all(pool).await {
        Ok(rows) => {
            if !rows.is_empty() {
                let sample = rows
                    .iter()
                    .take(5)
                    .filter_map(|row| row.try_get::<String, _>("session_key").ok())
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!(
                    cleared = rows.len(),
                    sample = %sample,
                    "cleared missing active_dispatch_id links during startup recovery"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to clear missing active_dispatch_id links during startup recovery"
            );
        }
    }
}

#[cfg(test)]
#[path = "orphan_recovery_fence_tests.rs"]
mod fence_tests;

pub(super) fn should_skip_agent_runtime_launch(token: &str) -> Option<String> {
    let bot = agentdesk_config::find_discord_bot_by_token(token)?;
    let agent_bot_names = agentdesk_config::collect_agent_bot_names();
    if !agent_bot_names.is_empty() && !agent_bot_names.contains(&bot.name) {
        return Some(bot.name);
    }
    None
}
