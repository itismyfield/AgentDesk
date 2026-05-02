use std::sync::Arc;

use crate::services::discord::SharedData;
use crate::services::observability::recovery_audit::{
    RecoveryAuditDraft, RecoveryAuditRecord, insert_recovery_audit_record,
    mark_recovery_audit_consumed,
};
use crate::services::observability::turn_lifecycle::{
    RecoveryContextDetails, RecoveryDetails, TurnEvent, TurnLifecycleEmit, emit_turn_lifecycle,
};
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use serenity::all::{ChannelId, MessageId};

const SESSION_RETRY_CONTEXT_LIMIT: usize = 10;
const SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT: usize = 300;
const DISCORD_RECENT_MESSAGES_RECOVERY_SOURCE: &str = "discord_recent_messages";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct SessionRetryContext {
    pub(in crate::services::discord) raw_context: String,
    pub(in crate::services::discord) audit_record: Option<RecoveryAuditRecord>,
}

fn session_retry_context_key(channel_id: u64) -> String {
    format!("session_retry_context:{channel_id}")
}

fn direct_runtime_context_unavailable(error: &str) -> bool {
    error.contains("direct runtime API context is unavailable")
        || error.contains("direct runtime pg context is unavailable")
}

fn store_session_retry_context_sqlite(
    sqlite: &crate::db::Db,
    key: &str,
    history: &str,
) -> Result<(), String> {
    let conn = sqlite
        .lock()
        .map_err(|err| format!("db lock failed: {err}"))?;
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        [key, history],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

fn store_session_retry_context_pg(
    pg_pool: &sqlx::PgPool,
    key: &str,
    history: &str,
) -> Result<(), String> {
    let key = key.to_string();
    let history = history.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value",
            )
            .bind(&key)
            .bind(&history)
            .execute(&pool)
            .await
            .map_err(|error| format!("store retry context {key}: {error}"))?;
            Ok(())
        },
        |message| message,
    )
}

fn insert_recovery_audit_pg(
    pg_pool: &sqlx::PgPool,
    channel_id: u64,
    session_key: Option<&str>,
    history: &str,
) -> Result<(), String> {
    let draft = RecoveryAuditDraft::discord_recent(
        channel_id.to_string(),
        session_key.map(str::to_string),
        history.to_string(),
        SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT,
    );
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            insert_recovery_audit_record(&pool, draft)
                .await
                .map_err(|error| format!("insert recovery audit record: {error}"))?;
            Ok(())
        },
        |message| message,
    )
}

fn take_session_retry_context_sqlite(sqlite: &crate::db::Db, key: &str) -> Option<String> {
    let conn = sqlite.lock().ok()?;
    let history = conn
        .query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()?;
    let _ = conn.execute("DELETE FROM kv_meta WHERE key = ?1", [key]);
    let history = history.trim().to_string();
    if history.is_empty() {
        None
    } else {
        Some(history)
    }
}

fn take_session_retry_context_pg(pg_pool: &sqlx::PgPool, key: &str) -> Option<String> {
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move { take_session_retry_context_pg_async(&pool, &key).await },
        |message| message,
    )
    .ok()
    .flatten()
}

async fn take_session_retry_context_pg_async(
    pool: &sqlx::PgPool,
    key: &str,
) -> Result<Option<String>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| format!("begin retry-context tx for {key}: {error}"))?;
    let history =
        sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1 LIMIT 1")
            .bind(key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load retry context {key}: {error}"))?;
    if history.is_some() {
        sqlx::query("DELETE FROM kv_meta WHERE key = $1")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("delete retry context {key}: {error}"))?;
    }
    tx.commit()
        .await
        .map_err(|error| format!("commit retry-context tx for {key}: {error}"))?;
    Ok(history.and_then(|history| {
        let history = history.trim().to_string();
        if history.is_empty() {
            None
        } else {
            Some(history)
        }
    }))
}

fn take_session_retry_context_runtime_pg(key: &str) -> Option<String> {
    let key = key.to_string();
    crate::utils::async_bridge::block_on_result(
        async move {
            let config =
                crate::config::load().map_err(|error| format!("load runtime config: {error}"))?;
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };

            take_session_retry_context_pg_async(&pool, &key).await
        },
        |message| message,
    )
    .ok()
    .flatten()
}

pub(in crate::services::discord) fn build_session_retry_context_from_history(
    history: &[HistoryItem],
) -> Option<String> {
    let lines = history
        .iter()
        .filter_map(|item| {
            let label = match item.item_type {
                HistoryType::User => "User",
                HistoryType::Assistant | HistoryType::Error => "Assistant",
                HistoryType::System | HistoryType::ToolUse | HistoryType::ToolResult => {
                    return None;
                }
            };
            let content = item.content.trim();
            if content.is_empty() {
                return None;
            }
            let excerpt = content
                .chars()
                .take(SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT)
                .collect::<String>();
            Some(format!("{label}: {excerpt}"))
        })
        .rev()
        .take(SESSION_RETRY_CONTEXT_LIMIT)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn store_session_retry_context_impl(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
    session_key: Option<&str>,
) -> Result<(), String> {
    let history = history.trim();
    if history.is_empty() {
        return Ok(());
    }

    let key = session_retry_context_key(channel_id);
    match super::super::internal_api::set_kv_value(&key, history) {
        Ok(()) => Ok(()),
        Err(err) if direct_runtime_context_unavailable(&err) => {
            if let Some(pg_pool) = pg_pool {
                store_session_retry_context_pg(pg_pool, &key, history)
            } else if let Some(db) = db {
                store_session_retry_context_sqlite(db, &key, history)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }?;

    if let Some(pg_pool) = pg_pool {
        insert_recovery_audit_pg(pg_pool, channel_id, session_key, history)?;
    }

    Ok(())
}

pub(in crate::services::discord) fn store_session_retry_context(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
) -> Result<(), String> {
    store_session_retry_context_impl(db, pg_pool, channel_id, history, None)
}

pub(in crate::services::discord) fn store_session_retry_context_with_notify(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
    session_key: Option<&str>,
) -> Result<bool, String> {
    store_session_retry_context_impl(db, pg_pool, channel_id, history, session_key)?;
    let sqlite_runtime_db = if pg_pool.is_some() { None } else { db };
    Ok(
        crate::services::message_outbox::enqueue_lifecycle_notification_best_effort(
            sqlite_runtime_db,
            pg_pool,
            &format!("channel:{channel_id}"),
            session_key,
            "lifecycle.recovery_context",
            "📋 최근 Discord 메시지를 복원 컨텍스트로 저장했습니다. 다음 턴에 자동 주입합니다.",
        ),
    )
}

fn mark_recovery_audit_consumed_pg(
    pg_pool: &sqlx::PgPool,
    channel_id: u64,
    turn_id: &str,
) -> Result<Option<RecoveryAuditRecord>, String> {
    let channel_id = channel_id.to_string();
    let turn_id = turn_id.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move {
            let record = mark_recovery_audit_consumed(&pool, &channel_id, &turn_id)
                .await
                .map_err(|error| format!("mark recovery audit consumed: {error}"))?;
            Ok(record)
        },
        |message| message,
    )
}

pub(in crate::services::discord) fn take_session_retry_context_for_turn_with_audit(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    consumed_by_turn_id: Option<&str>,
) -> Option<SessionRetryContext> {
    let key = session_retry_context_key(channel_id);
    let history = match super::super::internal_api::take_kv_value(&key) {
        Ok(Some(history)) => {
            let history = history.trim().to_string();
            if history.is_empty() {
                None
            } else {
                Some(history)
            }
        }
        Ok(None) => None,
        Err(err) if direct_runtime_context_unavailable(&err) => pg_pool
            .and_then(|pg_pool| take_session_retry_context_pg(pg_pool, &key))
            .or_else(|| take_session_retry_context_runtime_pg(&key))
            .or_else(|| db.and_then(|db| take_session_retry_context_sqlite(db, &key))),
        Err(_) => None,
    };

    let history = history?;
    let mut audit_record = None;
    if let (Some(pg_pool), Some(turn_id)) = (pg_pool, consumed_by_turn_id) {
        match mark_recovery_audit_consumed_pg(pg_pool, channel_id, turn_id) {
            Ok(record) => {
                audit_record = record;
            }
            Err(error) => {
                tracing::warn!("failed to stamp recovery audit consumed_by_turn_id: {error}");
            }
        }
    }

    Some(SessionRetryContext {
        raw_context: history,
        audit_record,
    })
}

pub(in crate::services::discord) fn take_session_retry_context_for_turn(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    consumed_by_turn_id: Option<&str>,
) -> Option<String> {
    take_session_retry_context_for_turn_with_audit(db, pg_pool, channel_id, consumed_by_turn_id)
        .map(|context| context.raw_context)
}

pub(in crate::services::discord) fn take_session_retry_context(
    db: Option<&crate::db::Db>,
    channel_id: u64,
) -> Option<String> {
    take_session_retry_context_for_turn(db, None, channel_id, None)
}

fn build_discord_recent_recovery_context_from_parts<'a>(
    messages: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<(String, usize)> {
    let mut lines = Vec::new();
    for (author, content) in messages {
        let content = content
            .chars()
            .take(SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT)
            .collect::<String>();
        if !content.trim().is_empty() {
            lines.push(format!("{author}: {content}"));
        }
    }

    if lines.is_empty() {
        None
    } else {
        let message_count = lines.len();
        Some((lines.join("\n"), message_count))
    }
}

async fn emit_session_resume_failed_with_recovery(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
    reason: String,
    recovery: Option<RecoveryContextDetails>,
) {
    let Some(pg_pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let session_key =
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider).await;
    let recovery_action = if recovery.is_some() {
        "fresh_session_with_discord_history_context"
    } else {
        "fresh_session_without_recovery_context"
    };
    let turn_id = format!("discord:{}:{}", channel_id.get(), user_message_id.get());
    let mut emit = TurnLifecycleEmit::new(
        turn_id,
        channel_id.get().to_string(),
        TurnEvent::SessionResumeFailedWithRecovery(RecoveryDetails {
            reason: reason.clone(),
            recovery_action: recovery_action.to_string(),
            previous_session_key: session_key.clone(),
            recovered_session_key: None,
            recovery,
        }),
        format!("session resume failed; recovery decision: {recovery_action}; reason: {reason}"),
    );
    if let Some(session_key) = session_key {
        emit = emit.session_key(session_key);
    }
    if let Err(error) = emit_turn_lifecycle(pg_pool, emit).await {
        tracing::warn!(
            "failed to emit session resume recovery lifecycle event for channel {}: {error}",
            channel_id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_discord_recent_recovery_context_from_parts, direct_runtime_context_unavailable,
    };

    #[test]
    fn direct_runtime_context_unavailable_matches_api_and_pg_errors() {
        assert!(direct_runtime_context_unavailable(
            "direct runtime API context is unavailable"
        ));
        assert!(direct_runtime_context_unavailable(
            "direct runtime pg context is unavailable"
        ));
        assert!(!direct_runtime_context_unavailable("other runtime error"));
    }

    #[test]
    fn discord_recent_recovery_context_preserves_existing_format_and_limits() {
        let long = "x".repeat(305);
        let built = build_discord_recent_recovery_context_from_parts([
            ("alice", "hello"),
            ("bot", ""),
            ("bob", long.as_str()),
        ])
        .expect("non-empty context");

        assert_eq!(built.1, 2);
        assert_eq!(built.0, format!("alice: hello\nbob: {}", "x".repeat(300)));
    }
}

/// Auto-retry a failed resume by fetching recent Discord history,
/// storing it in kv_meta for the router to inject into the LLM prompt,
/// and queueing the original message as an internal intervention.
/// Discord only sees the next provider reply — the full history is LLM-only.
pub(in crate::services::discord) async fn auto_retry_with_history(
    http: &serenity::http::Http,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
    user_text: &str,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    // Dedup guard: use a static set to prevent turn_bridge + watcher from
    // both firing auto-retry for the same channel simultaneously.
    use std::sync::LazyLock;
    static RETRY_PENDING: LazyLock<dashmap::DashSet<u64>> =
        LazyLock::new(|| dashmap::DashSet::new());
    if !RETRY_PENDING.insert(channel_id.get()) {
        tracing::warn!("  [{ts}] ⏭ auto-retry: skipped (dedup) for channel {channel_id}");
        return;
    }
    // Clean up guard after 30 seconds (allow future retries)
    let ch_id = channel_id.get();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        RETRY_PENDING.remove(&ch_id);
    });

    tracing::warn!("  [{ts}] ↻ auto-retry: fetching last 10 messages for channel {channel_id}");

    // Fetch last 10 messages from Discord
    let recovery_context = match channel_id
        .messages(http, serenity::builder::GetMessages::new().limit(10))
        .await
    {
        Ok(msgs) => build_discord_recent_recovery_context_from_parts(
            msgs.iter()
                .rev()
                .map(|msg| (msg.author.name.as_str(), msg.content.as_str())),
        )
        .map(|(history, message_count)| (history, message_count, None::<String>))
        .or_else(|| {
            Some((
                String::new(),
                0,
                Some("discord recent messages were empty".to_string()),
            ))
        }),
        Err(e) => {
            tracing::warn!("  [{ts}] ⚠ auto-retry: failed to fetch history: {e}");
            Some((
                String::new(),
                0,
                Some(format!("failed to fetch Discord history: {e}")),
            ))
        }
    };

    // Store history in kv_meta for the router to inject into LLM prompt.
    // Key: session_retry_context:{channel_id} — consumed on next turn start.
    if let Some((ref hist, message_count, ref skip_reason)) = recovery_context
        && skip_reason.is_none()
    {
        let session_key =
            super::super::adk_session::build_adk_session_key(shared, channel_id, provider)
                .await
                .unwrap_or_else(|| format!("channel:{}", channel_id.get()));
        let stored = store_session_retry_context_with_notify(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            channel_id.get(),
            hist,
            Some(session_key.as_str()),
        );
        match stored {
            Ok(_) => {
                emit_session_resume_failed_with_recovery(
                    shared,
                    provider,
                    channel_id,
                    user_message_id,
                    "provider-native resume failed; Discord recovery context built".to_string(),
                    Some(RecoveryContextDetails {
                        source: DISCORD_RECENT_MESSAGES_RECOVERY_SOURCE.to_string(),
                        message_count,
                        max_chars: SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT,
                    }),
                )
                .await;
            }
            Err(error) => {
                emit_session_resume_failed_with_recovery(
                    shared,
                    provider,
                    channel_id,
                    user_message_id,
                    format!("failed to store Discord recovery context: {error}"),
                    None,
                )
                .await;
            }
        }
    } else if let Some((_, _, Some(reason))) = recovery_context {
        emit_session_resume_failed_with_recovery(
            shared,
            provider,
            channel_id,
            user_message_id,
            reason,
            None,
        )
        .await;
    }

    // Discord message: short notice only — history stays LLM-side
    let retry_content = format!(
        "[이전 대화 복원 — 세션이 만료되어 최근 대화를 컨텍스트로 제공합니다]\n\n{}",
        user_text
    );
    let enqueued = super::super::enqueue_internal_followup(
        shared,
        provider,
        channel_id,
        user_message_id,
        retry_content,
        "auto-retry with history",
    )
    .await;
    if !enqueued {
        tracing::warn!("  [{ts}] ⏭ auto-retry: follow-up deduped for channel {channel_id}");
    }
}
