use std::sync::Arc;

use axum::http::StatusCode;
use sqlx::{PgPool, Row as SqlxRow};

use crate::db::auto_queue::slot_predicate::{
    DispatchSlotPolarity, active_dispatch_on_slot_predicate,
};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::session_identity::tmux_name_from_session_key;

#[derive(Debug, Clone)]
struct RuntimeSlotClearTarget {
    provider_name: String,
    thread_channel_id: u64,
    session_key: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct SlotClearTarget {
    thread_channel_ids: Vec<u64>,
    runtime_targets: Vec<RuntimeSlotClearTarget>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SlotRuntimeClearOutcome {
    Cleared(usize),
    DeferredResumeTransition,
    FailedRuntimePersistence,
}

fn slot_runtime_clear_permits_persistence(
    outcomes: impl IntoIterator<Item = crate::services::discord::health::RuntimeChannelClearResult>,
) -> SlotRuntimeClearOutcome {
    let mut cleared = 0usize;
    for outcome in outcomes {
        match outcome {
            crate::services::discord::health::RuntimeChannelClearResult::Cleared
            | crate::services::discord::health::RuntimeChannelClearResult::Unavailable => {
                cleared += 1;
            }
            crate::services::discord::health::RuntimeChannelClearResult::DeferredResumeTransition => {
                return SlotRuntimeClearOutcome::DeferredResumeTransition;
            }
            crate::services::discord::health::RuntimeChannelClearResult::PersistenceFailed => {
                return SlotRuntimeClearOutcome::FailedRuntimePersistence;
            }
        }
    }
    SlotRuntimeClearOutcome::Cleared(cleared)
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
        let provider_name = row
            .try_get::<Option<String>, _>("provider")
            .ok()
            .flatten()
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
        runtime_targets,
    })
}

pub async fn clear_slot_sessions_pg(
    pool: &PgPool,
    thread_channel_ids: &[u64],
) -> Result<usize, String> {
    let mut cleared_sessions = 0usize;
    for thread_channel_id in thread_channel_ids {
        let result = sqlx::query(
            "UPDATE sessions
             SET status = 'idle',
                 active_dispatch_id = NULL,
                 session_info = $1,
                 claude_session_id = NULL,
                 tokens = 0,
                 last_heartbeat = NOW()
             WHERE thread_channel_id = $2
               AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle')",
        )
        .bind("Slot thread reset")
        .bind(thread_channel_id.to_string())
        .execute(pool)
        .await
        .map_err(|error| {
            format!("clear postgres slot sessions for {thread_channel_id}: {error}")
        })?;
        cleared_sessions += result.rows_affected() as usize;
    }
    Ok(cleared_sessions)
}

async fn abort_prepared_slot_runtime_clears(
    prepared_targets: Vec<crate::services::discord::health::PreparedRuntimeChannelClear>,
    reason: &str,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for prepared in prepared_targets {
        if let Err(error) =
            crate::services::discord::health::abort_prepared_provider_channel_runtime_clear(
                prepared,
            )
            .await
        {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "abort prepared slot runtime clear after {reason}: {}",
            errors.join("; ")
        ))
    }
}

pub async fn clear_slot_threads_for_slot_pg(
    health_registry: Option<Arc<HealthRegistry>>,
    pool: &PgPool,
    agent_id: &str,
    slot_index: i64,
) -> Result<SlotRuntimeClearOutcome, String> {
    let target = build_slot_clear_target_pg(pool, agent_id, slot_index).await?;
    let safe_to_clear_thread_ids =
        filter_safe_slot_thread_reset_targets(pool, &target.thread_channel_ids).await?;

    if let Some(registry) = health_registry {
        let safe_to_clear: std::collections::HashSet<u64> =
            safe_to_clear_thread_ids.iter().copied().collect();
        let runtime_targets = target
            .runtime_targets
            .iter()
            .filter(|target| safe_to_clear.contains(&target.thread_channel_id))
            .collect::<Vec<_>>();
        let transition_id = uuid::Uuid::new_v4();
        let mut prepared_targets = Vec::with_capacity(runtime_targets.len());
        for runtime_target in runtime_targets {
            let prepared =
                crate::services::discord::health::prepare_provider_channel_runtime_clear(
                    &registry,
                    &runtime_target.provider_name,
                    poise::serenity_prelude::ChannelId::new(runtime_target.thread_channel_id),
                    runtime_target.session_key.as_deref(),
                    transition_id,
                )
                .await;
            match prepared {
                crate::services::discord::health::PrepareRuntimeChannelClearResult::Prepared(
                    prepared,
                ) => prepared_targets.push(prepared),
                crate::services::discord::health::PrepareRuntimeChannelClearResult::Unavailable => {}
                crate::services::discord::health::PrepareRuntimeChannelClearResult::DeferredResumeTransition => {
                    abort_prepared_slot_runtime_clears(prepared_targets, "deferred target").await?;
                    return Ok(SlotRuntimeClearOutcome::DeferredResumeTransition);
                }
                crate::services::discord::health::PrepareRuntimeChannelClearResult::PersistenceFailed => {
                    abort_prepared_slot_runtime_clears(
                        prepared_targets,
                        "persistence failure",
                    )
                    .await?;
                    return Ok(SlotRuntimeClearOutcome::FailedRuntimePersistence);
                }
            }
        }
        for prepared in prepared_targets {
            let outcome =
                crate::services::discord::health::commit_prepared_provider_channel_runtime_clear(
                    prepared,
                )
                .await;
            debug_assert_eq!(
                outcome,
                crate::services::discord::health::RuntimeChannelClearResult::Cleared
            );
            if outcome != crate::services::discord::health::RuntimeChannelClearResult::Cleared {
                return Err(format!(
                    "prepared slot runtime clear commit violated infallible contract: {outcome:?}"
                ));
            }
        }
    }

    let cleared = clear_slot_sessions_pg(pool, &safe_to_clear_thread_ids).await?;
    Ok(SlotRuntimeClearOutcome::Cleared(cleared))
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
    let safe_to_clear_thread_ids =
        filter_safe_slot_thread_reset_targets(pool, &target.thread_channel_ids).await?;
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
            "[auto-queue] preserving slot thread bindings for {agent_id}:{slot_index}: active thread archive was deferred"
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

async fn filter_safe_slot_thread_reset_targets(
    pool: &PgPool,
    thread_channel_ids: &[u64],
) -> Result<Vec<u64>, String> {
    let mut safe_to_reset = Vec::new();
    for thread_channel_id in thread_channel_ids {
        let thread_id = thread_channel_id.to_string();
        match crate::services::discord::should_defer_thread_archive_pg(Some(pool), &thread_id).await
        {
            Ok(true) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active turn or fresh inflight still present"
                );
            }
            Ok(false) => safe_to_reset.push(*thread_channel_id),
            Err(err) => {
                tracing::warn!(
                    "[auto-queue] skipping slot thread reset for {thread_channel_id}: active-check failed: {err}"
                );
            }
        }
    }
    Ok(safe_to_reset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::discord::{self, health::RuntimeChannelClearResult};
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::{
        BeginResumeTransitionResult, EndResumeTransitionResult,
    };
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};

    async fn seed_two_target_slot(pool: &PgPool, first: ChannelId, second: ChannelId) {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-two-target', 'repo', 'agent-two-target', 'active')",
        )
        .execute(pool)
        .await
        .expect("seed run");
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id)
             VALUES ('agent-two-target', 'Agent Two Target', 'claude', '123')",
        )
        .execute(pool)
        .await
        .expect("seed agent");
        sqlx::query(
            "INSERT INTO auto_queue_slots
                (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map)
             VALUES ('agent-two-target', 0, 'run-two-target', 0, $1::jsonb)",
        )
        .bind(
            serde_json::json!({
                "first": first.get().to_string(),
                "second": second.get().to_string(),
            })
            .to_string(),
        )
        .execute(pool)
        .await
        .expect("seed slot");
        for (channel, session_key) in [(first, "two-target-first"), (second, "two-target-second")] {
            sqlx::query(
                "INSERT INTO sessions
                    (session_key, agent_id, provider, status, session_info, tokens,
                     thread_channel_id, claude_session_id, active_dispatch_id, last_heartbeat)
                 VALUES ($1, 'agent-two-target', 'claude', 'idle', 'preserve', 321,
                         $2, 'provider-session', 'dispatch', NOW())",
            )
            .bind(session_key)
            .bind(channel.get().to_string())
            .execute(pool)
            .await
            .expect("seed session");
        }
    }

    async fn assert_two_target_authority_preserved(
        pool: &PgPool,
        shared: &Arc<discord::SharedData>,
        targets: [(ChannelId, &Arc<CancelToken>); 2],
    ) {
        for (channel, expected_token) in targets {
            let session =
                sqlx::query_as::<_, (String, String, i64, Option<String>, Option<String>)>(
                    "SELECT status, session_info, tokens, claude_session_id, active_dispatch_id
                 FROM sessions WHERE thread_channel_id = $1",
                )
                .bind(channel.get().to_string())
                .fetch_one(pool)
                .await
                .expect("load preserved session");
            assert_eq!(session.0, "idle");
            assert_eq!(session.1, "preserve");
            assert_eq!(session.2, 321);
            assert_eq!(session.3.as_deref(), Some("provider-session"));
            assert_eq!(session.4.as_deref(), Some("dispatch"));
            let snapshot = discord::mailbox_handle_for_tests(shared, channel)
                .snapshot()
                .await;
            assert!(
                snapshot
                    .cancel_token
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, expected_token)),
                "later target refusal must preserve every runtime token"
            );
        }
    }

    #[tokio::test]
    async fn later_target_refusal_preserves_all_slot_runtime_and_pg_authority() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate_with_max_connections(8).await;
        let first = ChannelId::new(4_916_601);
        let second = ChannelId::new(4_916_602);
        seed_two_target_slot(&pool, first, second).await;

        let shared = discord::make_shared_data_for_tests();
        let registry = Arc::new(HealthRegistry::new());
        registry
            .register_for_tests("claude".to_string(), shared.clone())
            .await;
        let first_token = Arc::new(CancelToken::new());
        let second_token = Arc::new(CancelToken::new());
        for (channel, token, message) in [
            (first, first_token.clone(), 4_916_611),
            (second, second_token.clone(), 4_916_612),
        ] {
            assert!(
                discord::mailbox_handle_for_tests(&shared, channel)
                    .try_start_turn(token, UserId::new(7), MessageId::new(message))
                    .await
            );
        }
        let blocking_key = match discord::mailbox_handle_for_tests(&shared, second)
            .begin_resume_transition(uuid::Uuid::new_v4())
            .await
        {
            BeginResumeTransitionResult::Begun(key) => key,
            other => panic!("reserve second target: {other:?}"),
        };

        assert_eq!(
            clear_slot_threads_for_slot_pg(Some(registry), &pool, "agent-two-target", 0)
                .await
                .expect("defer two-target clear"),
            SlotRuntimeClearOutcome::DeferredResumeTransition
        );
        assert_two_target_authority_preserved(
            &pool,
            &shared,
            [(first, &first_token), (second, &second_token)],
        )
        .await;
        assert!(matches!(
            discord::mailbox_handle_for_tests(&shared, second)
                .abort_resume_transition(blocking_key)
                .await,
            EndResumeTransitionResult::Applied(_)
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn resume_transition_refusal_blocks_slot_persistence() {
        assert_eq!(
            slot_runtime_clear_permits_persistence([
                RuntimeChannelClearResult::Cleared,
                RuntimeChannelClearResult::DeferredResumeTransition,
                RuntimeChannelClearResult::Unavailable,
            ]),
            SlotRuntimeClearOutcome::DeferredResumeTransition,
            "one reserved runtime must keep the whole slot out of the persistence-clear phase"
        );
    }

    #[test]
    fn runtime_persistence_failure_blocks_slot_persistence() {
        assert_eq!(
            slot_runtime_clear_permits_persistence([
                RuntimeChannelClearResult::Cleared,
                RuntimeChannelClearResult::PersistenceFailed,
            ]),
            SlotRuntimeClearOutcome::FailedRuntimePersistence,
            "runtime mailbox persistence failure must keep the PostgreSQL session authoritative"
        );
    }

    #[test]
    fn unavailable_runtime_does_not_block_pg_only_cleanup() {
        assert_eq!(
            slot_runtime_clear_permits_persistence([
                RuntimeChannelClearResult::Unavailable,
                RuntimeChannelClearResult::Cleared,
            ]),
            SlotRuntimeClearOutcome::Cleared(2)
        );
    }
}
