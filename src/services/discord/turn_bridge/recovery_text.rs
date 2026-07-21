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
use serenity::all::{ChannelId, MessageId};

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

fn store_kv_meta_value_pg(pg_pool: &sqlx::PgPool, key: &str, history: &str) -> Result<(), String> {
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

fn take_kv_meta_value_pg(pg_pool: &sqlx::PgPool, key: &str) -> Option<String> {
    let key = key.to_string();
    crate::utils::async_bridge::block_on_pg_result(
        pg_pool,
        move |pool| async move { take_kv_meta_value_pg_async(&pool, &key).await },
        |message| message,
    )
    .ok()
    .flatten()
}

async fn take_kv_meta_value_pg_async(
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

fn take_kv_meta_value_runtime_pg(key: &str) -> Option<String> {
    let key = key.to_string();
    crate::utils::async_bridge::block_on_result(
        async move {
            let config =
                crate::config::load().map_err(|error| format!("load runtime config: {error}"))?;
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Ok(None);
            };

            take_kv_meta_value_pg_async(&pool, &key).await
        },
        |message| message,
    )
    .ok()
    .flatten()
}

fn store_session_retry_context_kv_only(
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
        Err(err) if direct_runtime_context_unavailable(&err) => {
            if let Some(pg_pool) = pg_pool {
                store_kv_meta_value_pg(pg_pool, &key, history)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }?;

    Ok(())
}

fn store_session_retry_context_impl(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
    session_key: Option<&str>,
) -> Result<(), String> {
    let history = history.trim();
    if history.is_empty() {
        return Ok(());
    }
    store_session_retry_context_kv_only(pg_pool, channel_id, history)?;

    if let Some(pg_pool) = pg_pool {
        insert_recovery_audit_pg(pg_pool, channel_id, session_key, history)?;
    }

    Ok(())
}

/// Store the session retry (recovery) context together with its recovery
/// audit record (keyed by `session_key`).
///
/// #3418 D1: the lifecycle notification that used to fire here
/// (`lifecycle.recovery_context` → "📋 최근 Discord 메시지를 복원
/// 컨텍스트로 저장했습니다…") was pure duplication of the status panel's
/// inline `(최근 대화 N개를 읽어들였습니다)` suffix (rendered from
/// `recovery_message_count`, independent of any notification). The notify
/// enqueue was dropped; the audit-bearing store path is retained so the
/// prompt manifest sha256 validation keeps working.
pub(in crate::services::discord) fn store_session_retry_context_with_audit(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
    session_key: Option<&str>,
) -> Result<(), String> {
    store_session_retry_context_impl(pg_pool, channel_id, history, session_key)
}

pub(in crate::services::discord) fn restore_session_retry_context_after_take(
    pg_pool: Option<&sqlx::PgPool>,
    channel_id: u64,
    history: &str,
) -> Result<(), String> {
    // Blind upsert race: `intake_turn.rs:2201` releases the mailbox before
    // the put-back call at `intake_turn.rs:2339`, so a same-channel turn could
    // store a fresh context in that tiny gap and this stale snapshot would
    // clobber it. Accepted risk: the window is negligible, both payloads are
    // equivalent recent-message snapshots, and no locking should be added here.
    store_session_retry_context_kv_only(pg_pool, channel_id, history)
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
            .and_then(|pg_pool| take_kv_meta_value_pg(pg_pool, &key))
            .or_else(|| take_kv_meta_value_runtime_pg(&key)),
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

// ---------------------------------------------------------------------------
// #4307 PR-B: voluntary tool_feedback reminder stash → next-turn injection.
//
// The reminder is generated at turn end in `completion_postlude.rs` (when a
// turn ran `recall` but skipped the voluntary `tool_feedback`). Turn N stashes
// it here; turn N+1's intake takes it and injects it into the next prompt.
// Same kv_meta storage layer (`internal_api::set_kv_value` with a direct-PG
// fallback) and the same one-shot take/put-back contract as the session-retry
// recovery context above — reusing the generic `*_kv_meta_value_pg` helpers —
// but a distinct key so the two channels are independent (a turn may stash a
// reminder without a retry context, and vice versa). No recovery-audit row:
// reminders are lightweight and not part of the prompt-manifest sha256 chain.
//
// The key carries a provider axis (codex dual-review r1): on a channel shared
// by multiple provider bots, provider B's intake must not take (or its stash
// overwrite) provider A's reminder — the pending search_event_ids belong to
// A's memento session. Deliberately NOT token-scoped: a token rotation would
// orphan the stash, while the provider axis is stable across rotations.
fn voluntary_feedback_reminder_key(provider: &ProviderKind, channel_id: u64) -> String {
    format!(
        "voluntary_feedback_reminder:{}:{channel_id}",
        provider.as_str()
    )
}

pub(in crate::services::discord) fn store_voluntary_feedback_reminder(
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    reminder: &str,
) -> Result<(), String> {
    let reminder = reminder.trim();
    if reminder.is_empty() {
        return Ok(());
    }

    let key = voluntary_feedback_reminder_key(provider, channel_id);
    match super::super::internal_api::set_kv_value(&key, reminder) {
        Ok(()) => Ok(()),
        Err(err) if direct_runtime_context_unavailable(&err) => {
            if let Some(pg_pool) = pg_pool {
                store_kv_meta_value_pg(pg_pool, &key, reminder)
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

/// One-shot take: reads and deletes the stashed reminder so it is injected into
/// exactly one turn (no duplicate injection on turn N+2 unless a fresh reminder
/// is stashed). Mirrors `take_session_retry_context_for_turn_with_audit`'s
/// KV-then-PG-fallback take, minus the audit stamping.
pub(in crate::services::discord) fn take_voluntary_feedback_reminder(
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<String> {
    let key = voluntary_feedback_reminder_key(provider, channel_id);
    let reminder = match super::super::internal_api::take_kv_value(&key) {
        Ok(Some(reminder)) => Some(reminder),
        Ok(None) => None,
        Err(err) if direct_runtime_context_unavailable(&err) => pg_pool
            .and_then(|pg_pool| take_kv_meta_value_pg(pg_pool, &key))
            .or_else(|| take_kv_meta_value_runtime_pg(&key)),
        Err(_) => None,
    }?;
    let reminder = reminder.trim().to_string();
    if reminder.is_empty() {
        None
    } else {
        Some(reminder)
    }
}

/// Put the stashed reminder back after a turn took it but failed to establish
/// (TUI-busy enqueue refusal). Blind upsert, matching
/// `restore_session_retry_context_after_take`'s accepted-race semantics.
pub(in crate::services::discord) fn restore_voluntary_feedback_reminder_after_take(
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    reminder: &str,
) -> Result<(), String> {
    store_voluntary_feedback_reminder(pg_pool, provider, channel_id, reminder)
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
        super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None).await;
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
        restore_session_retry_context_after_take, restore_voluntary_feedback_reminder_after_take,
        store_session_retry_context_with_audit, store_voluntary_feedback_reminder,
        take_session_retry_context_for_turn_with_audit, take_voluntary_feedback_reminder,
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

    /// #3418 D1 regression lock: storing the session recovery context must
    /// NOT enqueue a lifecycle notification. The removed notify (reason_code
    /// built below) was pure duplication of the status panel's inline
    /// recovery suffix, which is rendered from `recovery_message_count`
    /// (SessionStrategyDetails) and is entirely independent of any
    /// notification. Backends are PG-only in practice; the `Db` compatibility
    /// handle is disabled, so this guards the wiring at the source level rather
    /// than via a DB fixture.
    ///
    /// The forbidden literals are assembled at runtime from fragments so the
    /// test source itself never contains them verbatim (otherwise the
    /// `include_str!` scan would match its own assertion text).
    #[test]
    fn recovery_context_store_does_not_enqueue_lifecycle_notification() {
        let module_src = include_str!("recovery_text.rs");

        // reason_code: "lifecycle." + "recovery_context"
        let reason_code = format!("\"{}{}\"", "lifecycle.", "recovery_context");
        assert!(
            !module_src.contains(&reason_code),
            "recovery context store must not enqueue the lifecycle recovery_context \
             notification (duplicate of the status panel recovery suffix)"
        );

        // user-facing notify body fragment (Korean, assembled from parts)
        let notify_phrase = format!("{}{}", "복원 컨텍스트로 ", "저장했습니다");
        assert!(
            !module_src.contains(&notify_phrase),
            "recovery context store must not enqueue the duplicate recovery-context \
             user notification body"
        );
    }

    #[tokio::test]
    async fn session_retry_context_restore_after_take_round_trips_raw_context_pg() {
        let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_recovery_text",
            "recovery text session retry context round trip",
        )
        .await;
        let pool = pg_db.connect_and_migrate().await;

        let channel_id = 9_000_000_000_004_148;
        let original_context =
            "alice: first recovered message\nbot: retry context survives\nalice: final message";
        store_session_retry_context_with_audit(
            Some(&pool),
            channel_id,
            original_context,
            Some("session-retry-round-trip"),
        )
        .expect("store session retry context");

        let first_take =
            take_session_retry_context_for_turn_with_audit(Some(&pool), channel_id, None)
                .expect("first take returns stored context");
        assert_eq!(first_take.raw_context, original_context);

        restore_session_retry_context_after_take(Some(&pool), channel_id, &first_take.raw_context)
            .expect("restore session retry context after take");

        let second_take =
            take_session_retry_context_for_turn_with_audit(Some(&pool), channel_id, None)
                .expect("second take returns restored context");
        assert_eq!(second_take.raw_context, original_context);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #4307 PR-B: the voluntary tool_feedback reminder must survive the
    /// stash → take → put-back → take round trip (independent kv_meta key), and
    /// a one-shot take must consume it so it is not re-injected on a later turn.
    #[tokio::test]
    async fn voluntary_feedback_reminder_stash_take_and_put_back_round_trips_pg() {
        use crate::services::provider::ProviderKind;

        let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_reminder_text",
            "recovery text voluntary feedback reminder round trip",
        )
        .await;
        let pool = pg_db.connect_and_migrate().await;

        let provider = ProviderKind::Claude;
        let channel_id = 9_000_000_000_004_307;
        let reminder = "이번 턴 recall 2건 중 tool_feedback 0/2. 다음 search_event_ids에 대해 \
             tool_feedback(search_event_id, relevant, sufficient)을 평가 후 턴 종료: [se-1, se-2]";

        // No reminder stashed → take yields nothing.
        assert!(
            take_voluntary_feedback_reminder(Some(&pool), &provider, channel_id).is_none(),
            "take without a stash must be None"
        );

        store_voluntary_feedback_reminder(Some(&pool), &provider, channel_id, reminder)
            .expect("stash voluntary feedback reminder");

        let first_take = take_voluntary_feedback_reminder(Some(&pool), &provider, channel_id)
            .expect("first take returns the stashed reminder");
        assert_eq!(first_take, reminder);

        // One-shot: the stash is consumed by the successful take.
        assert!(
            take_voluntary_feedback_reminder(Some(&pool), &provider, channel_id).is_none(),
            "reminder must be consumed by the take (no duplicate injection)"
        );

        // Put-back after a take that failed to establish a turn.
        restore_voluntary_feedback_reminder_after_take(
            Some(&pool),
            &provider,
            channel_id,
            &first_take,
        )
        .expect("put the reminder back after a refused turn");
        let second_take = take_voluntary_feedback_reminder(Some(&pool), &provider, channel_id)
            .expect("second take returns the restored reminder");
        assert_eq!(second_take, reminder);

        pool.close().await;
        pg_db.drop().await;
    }

    /// #4307 PR-B rework (codex dual-review r1): on a channel shared by
    /// multiple provider bots, another provider's intake must neither consume
    /// nor clobber a reminder stashed by the originating provider — the key is
    /// provider-scoped.
    #[tokio::test]
    async fn voluntary_feedback_reminder_is_not_consumed_by_other_provider_pg() {
        use crate::services::provider::ProviderKind;

        let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_reminder_xprov",
            "recovery text voluntary feedback reminder provider scoping",
        )
        .await;
        let pool = pg_db.connect_and_migrate().await;

        let claude = ProviderKind::Claude;
        let codex = ProviderKind::Codex;
        let channel_id = 9_000_000_000_004_308;
        let claude_reminder = "claude pending tool_feedback: [se-claude-1]";
        let codex_reminder = "codex pending tool_feedback: [se-codex-1]";

        store_voluntary_feedback_reminder(Some(&pool), &claude, channel_id, claude_reminder)
            .expect("stash claude reminder");

        // Another provider's take on the same channel must not consume it.
        assert!(
            take_voluntary_feedback_reminder(Some(&pool), &codex, channel_id).is_none(),
            "codex take must not consume claude's reminder"
        );

        // Another provider's stash must not overwrite it either.
        store_voluntary_feedback_reminder(Some(&pool), &codex, channel_id, codex_reminder)
            .expect("stash codex reminder");
        assert_eq!(
            take_voluntary_feedback_reminder(Some(&pool), &claude, channel_id).as_deref(),
            Some(claude_reminder),
            "claude's reminder survives the codex stash on the same channel"
        );
        assert_eq!(
            take_voluntary_feedback_reminder(Some(&pool), &codex, channel_id).as_deref(),
            Some(codex_reminder),
            "codex's reminder is stored independently"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}

/// #2452 H6: dedup guard for `auto_retry_with_history`. Kept at module
/// scope so the explicit-release path
/// (`TurnGateway::schedule_retry_with_history_with_completion` →
/// `release_retry_pending`) can drop the entry as soon as the matching
/// retry-turn finishes scheduling. The previous design used a hardcoded
/// 30s sleep on a detached task; that gave a 30s lockout window even
/// when scheduling succeeded in <100ms.
use std::sync::LazyLock;
static RETRY_PENDING: LazyLock<dashmap::DashSet<u64>> = LazyLock::new(dashmap::DashSet::new);

/// #2452 H6: explicit release for the auto-retry dedup lockout. Callers
/// using `schedule_retry_with_history_with_completion` invoke this once
/// the retry-spawn future has resolved (or the 120s safety net fires).
pub(in crate::services::discord) fn release_retry_pending(channel_id: ChannelId) {
    RETRY_PENDING.remove(&channel_id.get());
}

/// #2452 H6: test-only helper to inspect the dedup set without exposing
/// its internals broadly.
#[cfg(test)]
pub(in crate::services::discord) fn retry_pending_contains(channel_id: ChannelId) -> bool {
    RETRY_PENDING.contains(&channel_id.get())
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

    // Dedup guard: prevent turn_bridge + watcher from both firing
    // auto-retry for the same channel simultaneously.
    if !RETRY_PENDING.insert(channel_id.get()) {
        tracing::warn!("  [{ts}] ⏭ auto-retry: skipped (dedup) for channel {channel_id}");
        return;
    }
    // #2452 H6 graduation: the lockout release is now driven by an
    // explicit completion oneshot wired through `TurnGateway::
    // schedule_retry_with_history_with_completion`. The 30s sleep that
    // used to live here remains only as a 120s safety net on the
    // explicit-completion path below; this fire-and-forget legacy
    // wrapper (no caller-visible completion) keeps the original
    // sleep-based release so the public surface stays compatible for
    // callers that don't care about prompt release.
    let ch_id = channel_id.get();
    super::super::task_supervisor::spawn_observed("auto_retry_pending_release", async move {
        // Capped at 30s for back-compat — callers wanting prompt release
        // should use the `_with_completion` variant.
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
            super::super::adk_session::build_adk_session_key(shared, channel_id, provider, None)
                .await
                .unwrap_or_else(|| format!("channel:{}", channel_id.get()));
        let stored = store_session_retry_context_with_audit(
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

#[cfg(test)]
mod retry_pending_tests {
    use super::{RETRY_PENDING, release_retry_pending, retry_pending_contains};
    use serenity::all::ChannelId;

    /// #2452 H6 acceptance — first probe is the spec line "Test: simulate
    /// retry that resolves completion_rx -> lockout released immediately".
    /// We exercise the release path directly because the surrounding
    /// scheduling future requires a live Discord HTTP / SharedData fixture that
    /// is outside the default test suite.
    #[test]
    fn release_retry_pending_removes_dedup_entry() {
        // Use an arbitrary channel id unlikely to collide with other tests
        // (the static set is process-global). We insert first to model the
        // pre-existing lockout, then assert release drops it.
        let channel = ChannelId::new(900_000_000_000_002_452);
        RETRY_PENDING.insert(channel.get());
        assert!(retry_pending_contains(channel));

        release_retry_pending(channel);

        assert!(!retry_pending_contains(channel));
    }

    /// #2452 H6: releasing a not-pending channel must be a safe no-op.
    /// Re-running the release path after the 120s safety net would
    /// otherwise double-remove on the dashmap.
    #[test]
    fn release_retry_pending_is_idempotent() {
        let channel = ChannelId::new(900_000_000_000_002_453);
        // Ensure not present.
        RETRY_PENDING.remove(&channel.get());
        assert!(!retry_pending_contains(channel));

        release_retry_pending(channel);
        release_retry_pending(channel);

        assert!(!retry_pending_contains(channel));
    }
}
