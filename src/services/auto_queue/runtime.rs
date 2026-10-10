use std::sync::Arc;

use axum::http::StatusCode;
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::auto_queue::slot_predicate::{
    DispatchSlotPolarity, active_dispatch_on_slot_predicate,
};
use crate::services::discord::admin_host_guard::{ManagedResetVerdict, managed_reset_verdict};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::host_teardown_gate::row_host_refusal;
use crate::services::discord::session_identity::tmux_name_from_session_key;

const SLOT_THREAD_RESET_SESSION_INFO: &str = "Slot thread reset";

/// The statuses a slot reset idles; the host check reads the same rows.
const SLOT_RESET_STATUSES: [&str; 5] = [
    "turn_active",
    "awaiting_bg",
    "awaiting_user",
    "working",
    "idle",
];

#[derive(Debug, Clone)]
struct RuntimeSlotClearTarget {
    provider_name: String,
    thread_channel_id: u64,
    session_key: Option<String>,
}

/// The row a thread's runtime clear is chosen from, as stored, whatever its status.
#[derive(Debug, Clone)]
struct SlotThreadRow {
    thread_channel_id: u64,
    provider: Option<String>,
    session_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SlotClearTarget {
    thread_channel_ids: Vec<u64>,
    /// Kept even when no runtime target can be built from it, so the host check still reads it.
    selected_rows: Vec<SlotThreadRow>,
    runtime_targets: Vec<RuntimeSlotClearTarget>,
}

fn parse_slot_thread_channel_ids_from_value(value: &serde_json::Value) -> Vec<u64> {
    let mut thread_channel_ids = value
        .as_object()
        .map(|map| {
            map.values()
                .filter_map(|value| {
                    value
                        .as_str()
                        .and_then(|raw| raw.trim().parse::<u64>().ok())
                        .or_else(|| value.as_u64())
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    thread_channel_ids.sort_unstable();
    thread_channel_ids.dedup();
    thread_channel_ids
}

async fn build_slot_clear_target_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<SlotClearTarget, String> {
    let raw_map = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT COALESCE(thread_id_map, '{}'::jsonb)
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind(agent_id)
    .bind(slot_index)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres slot map for {agent_id}:{slot_index}: {error}"))?
    .flatten()
    .unwrap_or_else(|| serde_json::json!({}));

    let thread_channel_ids = parse_slot_thread_channel_ids_from_value(&raw_map);
    let mut selected_rows = Vec::with_capacity(thread_channel_ids.len());
    let mut runtime_targets = Vec::with_capacity(thread_channel_ids.len());

    for thread_channel_id in &thread_channel_ids {
        let row = sqlx::query(
            "SELECT provider, session_key
             FROM sessions
             WHERE thread_channel_id = $1
             ORDER BY CASE status WHEN 'turn_active' THEN 0 WHEN 'working' THEN 0 WHEN 'awaiting_bg' THEN 1 WHEN 'awaiting_user' THEN 2 WHEN 'idle' THEN 3 ELSE 4 END,
                      COALESCE(last_heartbeat, created_at) DESC,
                      id DESC
             LIMIT 1",
        )
        .bind(thread_channel_id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(|error| {
            format!(
                "load postgres slot runtime target for {agent_id}:{slot_index}:{thread_channel_id}: {error}"
            )
        })?;
        let Some(row) = row else {
            continue;
        };
        let session_key = row
            .try_get::<Option<String>, _>("session_key")
            .ok()
            .flatten();
        let provider = row.try_get::<Option<String>, _>("provider").ok().flatten();
        selected_rows.push(SlotThreadRow {
            thread_channel_id: *thread_channel_id,
            provider: provider.clone(),
            session_key: session_key.clone(),
        });
        let provider_name = provider
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                session_key.as_deref().and_then(|key| {
                    tmux_name_from_session_key(key).and_then(|tmux_name| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(
                            &tmux_name,
                        )
                        .map(|(provider, _)| provider.as_str().to_string())
                    })
                })
            });
        let Some(provider_name) = provider_name else {
            continue;
        };
        runtime_targets.push(RuntimeSlotClearTarget {
            provider_name,
            thread_channel_id: *thread_channel_id,
            session_key,
        });
    }

    Ok(SlotClearTarget {
        thread_channel_ids,
        selected_rows,
        runtime_targets,
    })
}

pub async fn clear_slot_sessions_pg(
    pool: &PgPool,
    thread_channel_ids: &[u64],
) -> Result<usize, String> {
    let mut thread_channel_ids = thread_channel_ids
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>();
    thread_channel_ids.sort_unstable();
    thread_channel_ids.dedup();
    if thread_channel_ids.is_empty() {
        return Ok(0);
    }

    let thread_count = thread_channel_ids.len();
    sqlx::query(
        "UPDATE sessions
         SET status = 'idle',
             active_dispatch_id = NULL,
             session_info = $1,
             claude_session_id = NULL,
             tokens = 0,
             last_heartbeat = NOW()
         WHERE thread_channel_id = ANY($2::TEXT[])
           AND status = ANY($3::TEXT[])
           AND NOT replay_session_held(current_replay_receipt_id, active_dispatch_id)",
    )
    .bind(SLOT_THREAD_RESET_SESSION_INFO)
    .bind(&thread_channel_ids)
    .bind(&SLOT_RESET_STATUSES[..])
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| format!("clear postgres slot sessions for {thread_count} thread(s): {error}"))
}

pub async fn clear_slot_threads_for_slot_pg(
    health_registry: Option<Arc<HealthRegistry>>,
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<usize, String> {
    let target = build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    let registry = health_registry.as_deref();
    let (safe_to_clear_thread_ids, verdicts) =
        filter_safe_slot_thread_reset_targets(pool, registry, &target).await?;
    #[cfg(test)]
    for channel in &safe_to_clear_thread_ids {
        crate::services::discord::admin_host_guard::run_before_verified_clear_recheck_for_tests(
            *channel,
        );
    }
    let cleared = clear_slot_sessions_pg(pool, &safe_to_clear_thread_ids).await?;

    if health_registry.is_some() {
        #[cfg(test)]
        let slot_threads = target.thread_channel_ids.clone();
        tokio::spawn(async move {
            for verdict in verdicts {
                let thread = verdict.channel_id().get();
                // Runs on the target approved before the sessions update; nothing is judged again.
                let reset = verdict.apply().await;
                #[cfg(test)]
                RUNTIME_CLEARS
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push((thread, Some(reset)));
                #[cfg(not(test))]
                let _ = (thread, reset);
            }
            #[cfg(test)]
            RUNTIME_CLEARS_DONE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(slot_threads);
        });
    }

    Ok(cleared)
}

pub async fn slot_has_active_dispatch_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
    exclude_entry_id: Option<&str>,
) -> Result<bool, String> {
    let exclude_id = exclude_dispatch_id.unwrap_or("");
    let exclude_entry_id = exclude_entry_id.unwrap_or("");
    // #2048 F5 + F8 / #3040: paused/cancelled-run entries no longer block —
    // their dispatches are being cancelled. review / review-decision /
    // create-pr dispatches only block when a live session is attached. The
    // task_dispatches half of this check shares the single SQL builder
    // (`active_dispatch_on_slot_predicate`) with claim.rs and slots.rs so
    // slot allocation and slot reset can never disagree.
    let auto_queue_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
         FROM auto_queue_entries e
         LEFT JOIN auto_queue_runs r ON r.id = e.run_id
         WHERE e.agent_id = $1
           AND e.slot_index = $2
           AND e.status = 'dispatched'
           AND COALESCE(e.dispatch_id, '') != $3
           AND e.id != $4
           AND COALESCE(r.status, 'active') NOT IN ('paused', 'cancelled')",
    )
    .bind(agent_id)
    .bind(slot_index)
    .bind(exclude_id)
    .bind(exclude_entry_id)
    .fetch_one(pool)
    .await
    .map_err(|error| {
        format!("load postgres active slot entries for {agent_id}:{slot_index}: {error}")
    })?;
    if auto_queue_active > 0 {
        return Ok(true);
    }

    let active_dispatch_exists = active_dispatch_on_slot_predicate(
        "$1",
        "$2",
        DispatchSlotPolarity::Exists,
        Some("d.id != $3"),
    );
    let dispatch_query = format!("SELECT {active_dispatch_exists}");
    sqlx::query_scalar::<_, bool>(&dispatch_query)
        .bind(agent_id)
        .bind(slot_index)
        .bind(exclude_id)
        .fetch_one(pool)
        .await
        .map_err(|error| {
            format!("load postgres active dispatches for {agent_id}:{slot_index}: {error}")
        })
}

pub async fn reset_slot_thread_bindings_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<(usize, usize, usize), String> {
    reset_slot_thread_bindings_excluding_pg(pool, agent_id, slot_index, None, None).await
}

pub async fn reset_slot_thread_bindings_excluding_pg(
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
    exclude_dispatch_id: Option<&str>,
    exclude_entry_id: Option<&str>,
) -> Result<(usize, usize, usize), String> {
    if slot_has_active_dispatch_excluding_pg(
        pool,
        agent_id,
        slot_index,
        exclude_dispatch_id,
        exclude_entry_id,
    )
    .await?
    {
        return Err(format!(
            "slot {slot_index} for agent {agent_id} has active dispatch"
        ));
    }

    let target = build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    let (safe_to_clear_thread_ids, _) =
        filter_safe_slot_thread_reset_targets(pool, None, &target).await?;
    let archived_threads = archive_slot_threads(&safe_to_clear_thread_ids).await?;
    let cleared_sessions = clear_slot_sessions_pg(pool, &safe_to_clear_thread_ids).await?;
    let cleared_bindings = if safe_to_clear_thread_ids.len() == target.thread_channel_ids.len() {
        sqlx::query(
            "UPDATE auto_queue_slots
             SET thread_id_map = '{}'::jsonb,
                 updated_at = NOW()
             WHERE agent_id = $1 AND slot_index = $2",
        )
        .bind(agent_id)
        .bind(slot_index)
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres slot bindings for {agent_id}:{slot_index}: {error}")
        })?
        .rows_affected() as usize
    } else {
        tracing::warn!(
            "[auto-queue] preserving slot thread bindings for {agent_id}:{slot_index}: a thread reset was deferred or refused"
        );
        0
    };

    Ok((archived_threads, cleared_sessions, cleared_bindings))
}

async fn archive_slot_threads(thread_channel_ids: &[u64]) -> Result<usize, String> {
    if thread_channel_ids.is_empty() {
        return Ok(0);
    }

    // #2048 F16: missing announce token → graceful skip (slot reset is
    // best-effort; environments that wire tokens under different names
    // should still be able to reset slot session state). The session/clear
    // path already covers the data side; archive is the optional Discord
    // side-effect.
    let Some(token) = crate::credential::read_bot_token(
        crate::services::discord::bot_role::UtilityBotRole::Announce.alias(),
    ) else {
        tracing::warn!(
            "[auto-queue] skipping archive_slot_threads: no announce bot token configured"
        );
        return Ok(0);
    };
    let client = reqwest::Client::new();
    let mut archived = 0usize;

    for thread_channel_id in thread_channel_ids {
        let thread_url = format!("https://discord.com/api/v10/channels/{thread_channel_id}");
        match client
            .patch(&thread_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND => {
                archived += 1;
            }
            Ok(resp) if resp.status().is_client_error() => {
                // #2048 F16: 4xx is non-retryable and usually means the thread
                // is already archived / permission changed / rate-limited;
                // skip rather than fail the whole slot reset. The data-side
                // session clear has already completed.
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    thread_channel_id,
                    %status,
                    %body,
                    "[auto-queue] skipping archive_slot_threads on 4xx"
                );
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!(
                    "failed to archive slot thread {thread_channel_id}: {status} {body}"
                ));
            }
            Err(err) => {
                return Err(format!(
                    "failed to archive slot thread {thread_channel_id}: {err}"
                ));
            }
        }
    }

    Ok(archived)
}

/// The threads a reset may change and, with a registry, the runtime-clear verdict of each; every
/// check only reads and finishes before the caller's first write.
async fn filter_safe_slot_thread_reset_targets(
    pool: &PgPool,
    registry: Option<&HealthRegistry>,
    target: &SlotClearTarget,
) -> Result<(Vec<u64>, Vec<ManagedResetVerdict>), String> {
    let (mut safe_to_reset, mut verdicts) = (Vec::new(), Vec::new());
    for thread_channel_id in &target.thread_channel_ids {
        let thread_id = thread_channel_id.to_string();
        let selected = target
            .selected_rows
            .iter()
            .find(|row| row.thread_channel_id == *thread_channel_id);
        // A Herdr-configured thread is kept whatever its rows say, with or without a registry.
        let configured = crate::services::discord::admin_host_guard::configured_refusal;
        let host = match configured(*thread_channel_id) {
            Some(reason) => Some(reason),
            None => slot_thread_host_refusal(pool, &thread_id, selected).await,
        };
        if let Some(reason) = host {
            tracing::warn!(
                "[auto-queue] skipping slot thread reset for {thread_channel_id}: {reason}"
            );
            continue;
        }
        let runtime_target = (target.runtime_targets.iter())
            .find(|runtime| runtime.thread_channel_id == *thread_channel_id);
        let verdict = match (registry, runtime_target) {
            (Some(registry), Some(runtime)) => {
                let channel = poise::serenity_prelude::ChannelId::new(*thread_channel_id);
                let key = runtime.session_key.as_deref();
                managed_reset_verdict(registry, &runtime.provider_name, channel, key).await
            }
            _ => None,
        };
        if let Some(reason) = verdict.as_ref().and_then(ManagedResetVerdict::refusal) {
            tracing::warn!(
                "[auto-queue] skipping slot thread reset for {thread_channel_id}: {reason}"
            );
            continue;
        }
        match crate::services::discord::should_defer_thread_archive_pg(Some(pool), &thread_id).await
        {
            Ok(true) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active turn or fresh inflight still present"
                );
            }
            Ok(false) => {
                safe_to_reset.push(*thread_channel_id);
                verdicts.extend(verdict);
            }
            Err(err) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active-check failed: {err}"
                );
            }
        }
    }
    Ok((safe_to_reset, verdicts))
}

/// Why a slot thread may not be reset: every row the reset idles and the row its runtime
/// clear is chosen from must each be a confirmed legacy tmux session.
async fn slot_thread_host_refusal(
    pool: &PgPool,
    thread_id: &str,
    selected: Option<&SlotThreadRow>,
) -> Option<String> {
    let rows = sqlx::query_as::<_, (Option<String>, Option<String>, bool)>(
        "SELECT provider, session_key,
                replay_session_held(current_replay_receipt_id, active_dispatch_id)
         FROM sessions
         WHERE thread_channel_id = $1
           AND status = ANY($2::TEXT[])",
    )
    .bind(thread_id)
    .bind(&SLOT_RESET_STATUSES[..])
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => return Some(format!("read the thread's sessions: {error}")),
    };
    // A session whose started attempt is held keeps its provider session and runtime.
    if rows.iter().any(|(_, _, held)| *held) {
        return Some("a session holds a started attempt whose replay is held".to_string());
    }
    let rows = rows.into_iter().map(|(provider, key, _)| (provider, key));
    let selected = selected.map(|row| (row.provider.clone(), row.session_key.clone()));
    for (provider, session_key) in rows.chain(selected) {
        let Some(key) = session_key else {
            return Some("a session row holds no session key".to_string());
        };
        let Some(name) = tmux_name_from_session_key(&key) else {
            return Some(format!("`{key}` names no tmux session"));
        };
        let (provider, caller) = (provider.as_deref(), "auto_queue_slot_reset");
        let refused = row_host_refusal(pool, provider, Some(thread_id), &key, &name, caller).await;
        if refused.is_some() {
            return refused;
        }
    }
    None
}

/// Each spawned slot-thread runtime clear, in order, for the tests that wait on it.
#[cfg(test)]
pub(crate) static RUNTIME_CLEARS: std::sync::Mutex<
    Vec<(
        u64,
        Option<crate::services::discord::admin_host_guard::ManagedReset>,
    )>,
> = std::sync::Mutex::new(Vec::new());

/// The slot's thread ids once each spawned runtime-clear loop has finished.
#[cfg(test)]
pub(crate) static RUNTIME_CLEARS_DONE: std::sync::Mutex<Vec<Vec<u64>>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
#[path = "runtime/clear_slot_sessions_pg_tests.rs"]
mod clear_slot_sessions_pg_tests;

#[cfg(test)]
#[path = "runtime/slot_reset_host_pg_tests.rs"]
pub(crate) mod slot_reset_host_pg_tests;

#[cfg(test)]
#[cfg(unix)]
#[path = "runtime/verified_reset_pg_tests.rs"]
pub(crate) mod verified_reset_pg_tests;
