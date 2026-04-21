use std::sync::Arc;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::ui::ai_screen::{HistoryItem, HistoryType};
use serenity::all::{ChannelId, MessageId};

const SESSION_RETRY_CONTEXT_LIMIT: usize = 10;
const SESSION_RETRY_CONTEXT_ITEM_CHAR_LIMIT: usize = 300;

fn session_retry_context_key(channel_id: u64) -> String {
    format!("session_retry_context:{channel_id}")
}

fn direct_api_context_unavailable(error: &str) -> bool {
    error.contains("direct runtime API context is unavailable")
}

fn store_session_retry_context_sqlite(
    db: &crate::db::Db,
    key: &str,
    history: &str,
) -> Result<(), String> {
    let conn = db.lock().map_err(|err| format!("db lock failed: {err}"))?;
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

fn take_session_retry_context_sqlite(db: &crate::db::Db, key: &str) -> Option<String> {
    let conn = db.lock().ok()?;
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

pub(in crate::services::discord) fn store_session_retry_context(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
) -> Result<(), String> {
    let history = history.trim();
    if history.is_empty() {
        return Ok(());
    }

    let key = session_retry_context_key(channel_id);
    match super::super::internal_api::set_kv_value(&key, history) {
        Ok(()) => Ok(()),
        Err(err) if direct_api_context_unavailable(&err) => {
            if let Some(pg_pool) = pg_pool {
                store_session_retry_context_pg(pg_pool, &key, history)
            } else if let Some(db) = db {
                store_session_retry_context_sqlite(db, &key, history)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

pub(in crate::services::discord) fn store_session_retry_context_with_notify(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
    session_key: Option<&str>,
) -> Result<bool, String> {
    store_session_retry_context(db, pg_pool, channel_id, history)?;
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

pub(in crate::services::discord) fn take_session_retry_context(
    db: Option<&crate::db::Db>,
    channel_id: u64,
) -> Option<String> {
    let key = session_retry_context_key(channel_id);
    match super::super::internal_api::take_kv_value(&key) {
        Ok(Some(history)) => {
            let history = history.trim().to_string();
            if history.is_empty() {
                None
            } else {
                Some(history)
            }
        }
        Ok(None) => None,
        Err(err) if direct_api_context_unavailable(&err) => {
            db.and_then(|db| take_session_retry_context_sqlite(db, &key))
        }
        Err(_) => None,
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
    let history = match channel_id
        .messages(http, serenity::builder::GetMessages::new().limit(10))
        .await
    {
        Ok(msgs) => {
            let mut lines = Vec::new();
            for msg in msgs.iter().rev() {
                let author = &msg.author.name;
                let content = msg.content.chars().take(300).collect::<String>();
                if !content.trim().is_empty() {
                    lines.push(format!("{}: {}", author, content));
                }
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Err(e) => {
            tracing::warn!("  [{ts}] ⚠ auto-retry: failed to fetch history: {e}");
            None
        }
    };

    // Store history in kv_meta for the router to inject into LLM prompt.
    // Key: session_retry_context:{channel_id} — consumed on next turn start.
    if let Some(ref hist) = history {
        let session_key =
            super::super::adk_session::build_adk_session_key(shared, channel_id, provider)
                .await
                .unwrap_or_else(|| format!("channel:{}", channel_id.get()));
        let _ = store_session_retry_context_with_notify(
            shared.db.as_ref(),
            shared.pg_pool.as_ref(),
            channel_id.get(),
            hist,
            Some(session_key.as_str()),
        );
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
