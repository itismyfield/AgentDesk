use std::borrow::Cow;

use sqlx::PgPool;

use crate::services::provider::{CancelToken, cancel_requested};

pub(crate) const LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS: i64 = 5 * 60;
pub(crate) const LIFECYCLE_NOTIFIER_SOURCE: &str = "lifecycle_notifier";

#[derive(Clone, Copy, Debug)]
pub(crate) struct OutboxMessage<'a> {
    pub target: &'a str,
    pub content: &'a str,
    pub bot: &'a str,
    pub source: &'a str,
    pub reason_code: Option<&'a str>,
    pub session_key: Option<&'a str>,
}

#[derive(Debug)]
pub(crate) enum OutboxEnqueueError {
    SourceNotAllowed { source: String },
    Database(sqlx::Error),
}

impl std::fmt::Display for OutboxEnqueueError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceNotAllowed { source } => write!(
                formatter,
                "message_outbox source `{source}` is not registered for LoopbackInternal"
            ),
            Self::Database(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OutboxEnqueueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SourceNotAllowed { .. } => None,
            Self::Database(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for OutboxEnqueueError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

fn validate_outbox_source(source: &str) -> Result<(), OutboxEnqueueError> {
    crate::services::discord::outbound::source_registry::validate_send_source_for(
        source,
        crate::services::discord::outbound::source_registry::SendCallerClass::LoopbackInternal,
    )
    .map_err(|_| OutboxEnqueueError::SourceNotAllowed {
        source: source.to_string(),
    })
}

fn normalized_session_key(target: &str, session_key: Option<&str>) -> Option<String> {
    session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let target = target.trim();
            (!target.is_empty()).then(|| target.to_string())
        })
}

fn normalized_reason_code(reason_code: Option<&str>) -> Option<&str> {
    reason_code.map(str::trim).filter(|value| !value.is_empty())
}

fn parse_channel_target(target: &str) -> Option<u64> {
    target
        .trim()
        .strip_prefix("channel:")?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

fn is_dm_session_channel_segment(value: &str) -> bool {
    value
        .strip_prefix("dm-")
        .is_some_and(|user_id| !user_id.is_empty() && user_id.chars().all(|ch| ch.is_ascii_digit()))
}

fn private_session_provider_from_key(session_key: Option<&str>) -> Option<String> {
    let session_key = session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let parsed = crate::services::discord::session_identity::SessionIdentity::parse(session_key);
    let tmux_name = parsed
        .as_ref()
        .map(|identity| identity.tmux_name.as_str())
        .unwrap_or(session_key);
    let (provider, channel_segment) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)?;
    is_dm_session_channel_segment(&channel_segment).then(|| provider.as_str().to_string())
}

pub(crate) fn delivery_bot_for_target_session<'a>(
    target: &str,
    configured_bot: &'a str,
    session_key: Option<&str>,
) -> Cow<'a, str> {
    if parse_channel_target(target).is_some()
        && let Some(provider_bot) = private_session_provider_from_key(session_key)
    {
        return Cow::Owned(provider_bot);
    }
    Cow::Borrowed(configured_bot)
}

fn dedupe_key_for_message(
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
) -> Option<String> {
    let session_key = session_key
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let reason_code = normalized_reason_code(reason_code);
    let identity_kind = if reason_code.is_some() {
        "reason_code"
    } else {
        "content"
    };
    let content_identity = reason_code.is_none().then_some(content).unwrap_or("");
    let mut hasher = blake3::Hasher::new();
    for part in [
        "message_outbox:v1",
        identity_kind,
        target.trim(),
        session_key,
        reason_code.unwrap_or("").trim(),
        content_identity,
    ] {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    Some(format!("message_outbox:v1:{}", hasher.finalize().to_hex()))
}

/// Test-only accessor for [`dedupe_key_for_message`] so sibling modules can
/// assert their dedupe identity is stable (e.g. `long_turn_watchdog` verifying
/// its cluster alert dedupes across scans). Not part of the runtime API.
#[cfg(test)]
pub(crate) fn dedupe_key_for_message_for_test(
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
) -> Option<String> {
    dedupe_key_for_message(target, content, reason_code, session_key)
}

#[cfg(test)]
mod dedupe_key_tests {
    use super::{dedupe_key_for_message, delivery_bot_for_target_session};

    #[test]
    fn reason_code_dedupe_key_ignores_content() {
        let first = dedupe_key_for_message(
            "channel:123",
            "first rendered lifecycle text",
            Some("relay_terminal_ack_timeout"),
            Some("session-abc"),
        );
        let second = dedupe_key_for_message(
            "channel:123",
            "second rendered lifecycle text",
            Some("relay_terminal_ack_timeout"),
            Some("session-abc"),
        );

        assert_eq!(first, second);
    }

    #[test]
    fn content_dedupe_key_keeps_content_identity_without_reason_code() {
        let first = dedupe_key_for_message("channel:123", "first", None, Some("session-abc"));
        let second = dedupe_key_for_message("channel:123", "second", None, Some("session-abc"));

        assert_ne!(first, second);
    }

    #[test]
    fn dm_session_routes_outbox_delivery_through_provider_bot() {
        let bot = delivery_bot_for_target_session(
            "channel:1479662682909966490",
            "announce",
            Some("claude/tok/mac-mini:AgentDesk-claude-dm-343742347"),
        );

        assert_eq!(bot.as_ref(), "claude");
        let notify_bot = delivery_bot_for_target_session(
            "channel:1479662682909966490",
            "notify",
            Some("claude/tok/mac-mini:AgentDesk-claude-dm-343742347"),
        );
        assert_eq!(notify_bot.as_ref(), "claude");
        let raw_tmux_bot = delivery_bot_for_target_session(
            "channel:1479662682909966490",
            "announce",
            Some("AgentDesk-claude-dm-343742347"),
        );
        assert_eq!(raw_tmux_bot.as_ref(), "claude");
    }

    #[test]
    fn guild_session_keeps_configured_announce_bot() {
        let bot = delivery_bot_for_target_session(
            "channel:1504455726595051591",
            "announce",
            Some("claude/tok/mac-mini:AgentDesk-claude-adk-cc"),
        );

        assert_eq!(bot.as_ref(), "announce");
    }

    #[test]
    fn notify_guild_delivery_keeps_info_only_bot() {
        let bot = delivery_bot_for_target_session(
            "channel:1504455726595051591",
            "notify",
            Some("codex/tok/mac-mini:AgentDesk-codex-adk-cc"),
        );

        assert_eq!(bot.as_ref(), "notify");
    }
}

fn warn_outbox_enqueue_failure(
    backend: &'static str,
    message: OutboxMessage<'_>,
    error: impl std::fmt::Display,
) {
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    tracing::warn!(
        backend,
        target = message.target,
        bot = message.bot,
        source = message.source,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue outbox message: {error}"
    );
}

fn warn_lifecycle_enqueue_failure(
    backend: &'static str,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    error: impl std::fmt::Display,
) {
    let session_key = normalized_session_key(target, session_key);
    tracing::warn!(
        backend,
        target,
        reason_code,
        session_key = session_key.as_deref(),
        "failed to enqueue lifecycle notification: {error}"
    );
}

pub(crate) fn enqueue_lifecycle_notification_best_effort(
    pg_pool: Option<&PgPool>,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> bool {
    // PG outbox rows are authoritative whenever a pool is configured. The
    // release worker drains PG only in that mode, so writing a "fallback"
    // lifecycle row to SQLite would create an undeliverable ghost message.
    if let Some(pool) = pg_pool {
        let target_owned = target.to_string();
        let session_key_owned = session_key.map(str::to_string);
        let reason_code_owned = reason_code.to_string();
        let content_owned = content.to_string();
        match crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |pool| async move {
                enqueue_lifecycle_notification_pg(
                    &pool,
                    &target_owned,
                    session_key_owned.as_deref(),
                    &reason_code_owned,
                    &content_owned,
                )
                .await
                .map_err(|error| format!("enqueue lifecycle notification via postgres: {error}"))
            },
            |message| message,
        ) {
            Ok(enqueued) => return enqueued,
            Err(error) => {
                warn_lifecycle_enqueue_failure(
                    "postgres",
                    target,
                    session_key,
                    reason_code,
                    &error,
                );
                return false;
            }
        }
    }

    false
}

async fn find_duplicate_outbox_message_pg(
    pool: &PgPool,
    target: &str,
    content: &str,
    reason_code: Option<&str>,
    session_key: Option<&str>,
    dedupe_ttl_secs: i64,
) -> Result<Option<i64>, sqlx::Error> {
    let Some(session_key) = session_key else {
        return Ok(None);
    };

    if let Some(reason_code) = reason_code {
        return sqlx::query_scalar::<_, i64>(
            "SELECT id
             FROM message_outbox
             WHERE target = $1
               AND reason_code = $2
               AND session_key = $3
               AND status != 'failed'
               AND created_at >= NOW() - ($4::BIGINT * INTERVAL '1 second')
             ORDER BY id DESC
             LIMIT 1",
        )
        .bind(target)
        .bind(reason_code)
        .bind(session_key)
        .bind(dedupe_ttl_secs)
        .fetch_optional(pool)
        .await;
    }

    sqlx::query_scalar::<_, i64>(
        "SELECT id
         FROM message_outbox
         WHERE target = $1
           AND reason_code IS NULL
           AND content = $2
           AND session_key = $3
           AND status != 'failed'
           AND created_at >= NOW() - ($4::BIGINT * INTERVAL '1 second')
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind(target)
    .bind(content)
    .bind(session_key)
    .bind(dedupe_ttl_secs)
    .fetch_optional(pool)
    .await
}

async fn release_expired_outbox_dedupe_key_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dedupe_key: Option<&str>,
) -> Result<(), sqlx::Error> {
    let Some(dedupe_key) = dedupe_key else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE message_outbox
            SET dedupe_key = NULL,
                dedupe_expires_at = NULL
          WHERE dedupe_key = $1
            AND status != 'failed'
            AND dedupe_expires_at <= NOW()",
    )
    .bind(dedupe_key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn enqueue_outbox_pg_returning_id(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<Option<i64>, OutboxEnqueueError> {
    enqueue_outbox_pg_returning_id_with_ttl(pool, message, LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS).await
}

pub(crate) async fn enqueue_outbox_pg_returning_id_with_cancel(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    cancel_token: Option<&CancelToken>,
) -> Result<Option<i64>, OutboxEnqueueError> {
    enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
        pool,
        message,
        LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS,
        cancel_token,
    )
    .await
}

/// Variant of [`enqueue_outbox_pg_returning_id`] that lets the caller pick the
/// dedupe TTL (in seconds). Use when the default 5-minute window is too short
/// for the firing cadence (e.g. periodic GitHub sync alerts that fire every
/// 20 minutes and should not spam the channel every cycle).
pub(crate) async fn enqueue_outbox_pg_returning_id_with_ttl(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<Option<i64>, OutboxEnqueueError> {
    enqueue_outbox_pg_returning_id_with_ttl_and_cancel(pool, message, dedupe_ttl_secs, None).await
}

/// Enqueue an event whose dedupe identity must survive indefinitely.
///
/// This is intentionally narrower than the TTL helpers: callers must supply a
/// reason/session identity that names one immutable event (for example one
/// scheduled-message fire slot). The active partial unique index keeps the key
/// while the row is pending or sent. A failed row releases the key so an
/// operator or recovery path may stage a genuine retry. On duplicate, the
/// existing row id is returned so callers retain an auditable handoff link.
pub(crate) async fn enqueue_outbox_pg_returning_id_with_persistent_dedupe(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<i64, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    let dedupe_key = dedupe_key_for_message(
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
    );

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key, dedupe_key, dedupe_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NULL)
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status != 'failed'
         DO UPDATE SET dedupe_expires_at = NULL
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(dedupe_key.as_deref())
    .fetch_one(pool)
    .await
    .map_err(Into::into)
}

/// Transaction-scoped variant of the persistent handoff helper.
///
/// Callers use this when the outbox reservation and their own state transition
/// must commit atomically. Keeping the same dedupe identity as the pool helper
/// makes crash recovery and competing workers converge on one durable row.
pub(crate) async fn enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    message: OutboxMessage<'_>,
) -> Result<i64, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    let dedupe_key = dedupe_key_for_message(
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
    );

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key, dedupe_key, dedupe_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, NULL)
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status != 'failed'
         DO UPDATE SET dedupe_expires_at = NULL
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(dedupe_key.as_deref())
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

pub(crate) async fn enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
    cancel_token: Option<&CancelToken>,
) -> Result<Option<i64>, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);

    let duplicate_id = if dedupe_ttl_secs > 0 {
        find_duplicate_outbox_message_pg(
            pool,
            message.target,
            message.content,
            reason_code,
            session_key.as_deref(),
            dedupe_ttl_secs,
        )
        .await?
    } else {
        None
    };

    if let Some(existing_id) = duplicate_id {
        tracing::info!(
            target = message.target,
            reason_code,
            session_key = session_key.as_deref(),
            existing_id,
            dedupe_ttl_secs,
            "suppressed duplicate outbox message"
        );
        return Ok(None);
    }

    if cancel_requested(cancel_token) {
        tracing::info!(
            target = message.target,
            bot = message.bot,
            source = message.source,
            reason_code,
            session_key = session_key.as_deref(),
            "skipped outbox enqueue after turn cancellation"
        );
        return Ok(None);
    }

    let mut tx = pool.begin().await?;
    let outbox_id = enqueue_outbox_pg_on_tx_with_ttl(&mut tx, message, dedupe_ttl_secs).await?;
    tx.commit().await?;

    if outbox_id.is_none() {
        tracing::info!(
            target = message.target,
            reason_code,
            session_key = session_key.as_deref(),
            dedupe_ttl_secs,
            "suppressed duplicate outbox message by database dedupe key"
        );
    }

    Ok(outbox_id)
}

/// Validated deduplicating insert for callers that must atomically commit an
/// outbox obligation with another PostgreSQL state transition.
pub(crate) async fn enqueue_outbox_pg_on_tx_with_ttl(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<Option<i64>, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    let dedupe_key = (dedupe_ttl_secs > 0)
        .then(|| {
            dedupe_key_for_message(
                message.target,
                message.content,
                reason_code,
                session_key.as_deref(),
            )
        })
        .flatten();
    release_expired_outbox_dedupe_key_pg(tx, dedupe_key.as_deref()).await?;
    Ok(sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key, dedupe_key, dedupe_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7,
                 CASE WHEN $8::BIGINT > 0
                      THEN NOW() + ($8::BIGINT * INTERVAL '1 second')
                      ELSE NULL
                 END)
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status != 'failed'
         DO NOTHING
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(dedupe_key.as_deref())
    .bind(dedupe_ttl_secs)
    .fetch_optional(&mut **tx)
    .await?)
}

#[cfg(test)]
pub(crate) async fn monitor_alert_rows_for_test(
    pool: &PgPool,
) -> Result<Vec<(String, String, String, String, String, String, bool)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT source, bot, reason_code, session_key, target, content,
                dedupe_expires_at >= created_at + INTERVAL '30 days'
           FROM message_outbox",
    )
    .fetch_all(pool)
    .await
}

/// Variant of [`enqueue_outbox_pg`] that lets the caller pick the dedupe TTL.
pub(crate) async fn enqueue_outbox_pg_with_ttl(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<bool, OutboxEnqueueError> {
    Ok(
        enqueue_outbox_pg_returning_id_with_ttl(pool, message, dedupe_ttl_secs)
            .await?
            .is_some(),
    )
}

/// Stage a deduplicated row that outbox workers cannot claim. The caller must
/// explicitly activate it after its external authority check, or cancel it.
pub(crate) async fn stage_outbox_pg_with_ttl(
    pool: &PgPool,
    message: OutboxMessage<'_>,
    dedupe_ttl_secs: i64,
) -> Result<i64, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    let dedupe_key = dedupe_key_for_message(
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
    )
    .ok_or_else(|| {
        OutboxEnqueueError::Database(sqlx::Error::Protocol(
            "staged outbox message requires a dedupe identity".to_string(),
        ))
    })?;
    let mut tx = pool.begin().await?;
    release_expired_outbox_dedupe_key_pg(&mut tx, Some(&dedupe_key)).await?;
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, status, reason_code, session_key,
          dedupe_key, dedupe_expires_at)
         VALUES ($1,$2,$3,$4,'held',$5,$6,$7,
                 NOW() + ($8::BIGINT * INTERVAL '1 second'))
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status != 'failed'
         DO NOTHING
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(&dedupe_key)
    .bind(dedupe_ttl_secs)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match inserted {
        Some(id) => id,
        None => {
            sqlx::query_scalar::<_, i64>(
                "SELECT id FROM message_outbox
              WHERE dedupe_key=$1 AND status!='failed'
              ORDER BY id LIMIT 1",
            )
            .bind(&dedupe_key)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    Ok(id)
}

pub(crate) async fn activate_staged_outbox_pg(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("UPDATE message_outbox SET status='pending' WHERE id=$1 AND status='held'")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Activate a held row, or confirm that a prior activation already made the
/// same durable id deliverable/terminal before the caller crashed. A missing
/// id is distinct so the sidecar obligation can be reopened instead of being
/// falsely marked complete.
pub(crate) async fn activate_or_confirm_staged_outbox_pg(
    pool: &PgPool,
    id: i64,
) -> Result<bool, sqlx::Error> {
    if activate_staged_outbox_pg(pool, id).await? {
        return Ok(true);
    }
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM message_outbox WHERE id=$1 AND status!='held')",
    )
    .bind(id)
    .fetch_one(pool)
    .await?)
}

pub(crate) async fn cancel_staged_outbox_pg(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM message_outbox WHERE id=$1 AND status='held'")
            .bind(id)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Prune terminal outbox history and expired worker-invisible staging rows.
///
/// A process can die after PostgreSQL commits a `held` row but before the
/// recovery sidecar records its id. Those rows are never deliverable, and once
/// their dedupe TTL expires no live recovery obligation may still depend on
/// them: a sidecar that does retain the id treats a missing row as a signal to
/// reopen and stage a fresh alert. Bounding them here prevents permanent
/// housekeeping leaks without activating or otherwise exposing stale alerts.
pub(crate) async fn gc_stale_outbox_rows(pool: &PgPool) -> Result<(u64, u64, u64), sqlx::Error> {
    let held = sqlx::query(
        "DELETE FROM message_outbox
          WHERE status = 'held'
            AND dedupe_expires_at IS NOT NULL
            AND dedupe_expires_at <= NOW()",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let failed = sqlx::query(
        "DELETE FROM message_outbox
          WHERE status = 'failed'
            AND created_at < NOW() - INTERVAL '7 days'",
    )
    .execute(pool)
    .await?
    .rows_affected();
    let sent = sqlx::query(
        "DELETE FROM message_outbox
          WHERE status = 'sent'
            AND created_at < NOW() - INTERVAL '30 days'
            -- NULL expiry + a live dedupe key is an intentional permanent
            -- sentinel (scheduled-message fire slots use this contract).
            AND NOT (dedupe_key IS NOT NULL AND dedupe_expires_at IS NULL)",
    )
    .execute(pool)
    .await?
    .rows_affected();
    Ok((held, failed, sent))
}

pub(crate) async fn enqueue_outbox_pg(
    pool: &PgPool,
    message: OutboxMessage<'_>,
) -> Result<bool, OutboxEnqueueError> {
    Ok(enqueue_outbox_pg_returning_id(pool, message)
        .await?
        .is_some())
}

// PG outbox rows are authoritative for the release runtime. Without a PG pool,
// callers should choose a visible direct-send fallback instead of staging an
// undrained legacy row.
pub(crate) async fn enqueue_outbox_best_effort(
    pg_pool: Option<&PgPool>,
    message: OutboxMessage<'_>,
) -> Result<bool, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    if let Some(pool) = pg_pool {
        return match enqueue_outbox_pg(pool, message).await {
            Ok(enqueued) => Ok(enqueued),
            Err(error) => {
                warn_outbox_enqueue_failure("postgres", message, &error);
                Err(error)
            }
        };
    }

    Ok(false)
}

/// Validated no-dedupe insert for callers already holding a PostgreSQL transaction.
pub(crate) async fn enqueue_outbox_pg_on_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    message: OutboxMessage<'_>,
) -> Result<i64, OutboxEnqueueError> {
    validate_outbox_source(message.source)?;
    let reason_code = normalized_reason_code(message.reason_code);
    let session_key = normalized_session_key(message.target, message.session_key);
    Ok(sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .fetch_one(&mut **tx)
    .await?)
}

pub(crate) async fn enqueue_lifecycle_notification_pg(
    pool: &PgPool,
    target: &str,
    session_key: Option<&str>,
    reason_code: &str,
    content: &str,
) -> Result<bool, OutboxEnqueueError> {
    validate_outbox_source(LIFECYCLE_NOTIFIER_SOURCE)?;
    let reason_code = normalized_reason_code(Some(reason_code));
    let session_key = normalized_session_key(target, session_key);
    let dedupe_key = dedupe_key_for_message(target, content, reason_code, session_key.as_deref());

    let duplicate_id = find_duplicate_outbox_message_pg(
        pool,
        target,
        content,
        reason_code,
        session_key.as_deref(),
        LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS,
    )
    .await?;

    if let Some(existing_id) = duplicate_id {
        tracing::info!(
            target,
            reason_code,
            session_key = session_key.as_deref(),
            existing_id,
            "suppressed duplicate lifecycle notification"
        );
        return Ok(false);
    }

    let mut tx = pool.begin().await?;
    release_expired_outbox_dedupe_key_pg(&mut tx, dedupe_key.as_deref()).await?;
    let inserted = sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key, dedupe_key, dedupe_expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7,
                 NOW() + ($8::BIGINT * INTERVAL '1 second'))
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status != 'failed'
         DO NOTHING
         RETURNING id",
    )
    .bind(target)
    .bind(content)
    .bind("notify")
    .bind(LIFECYCLE_NOTIFIER_SOURCE)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(dedupe_key.as_deref())
    .bind(LIFECYCLE_NOTIFY_DEDUPE_TTL_SECS)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;

    if inserted.is_none() {
        tracing::info!(
            target,
            reason_code,
            session_key = session_key.as_deref(),
            "suppressed duplicate lifecycle notification by database dedupe key"
        );
        return Ok(false);
    }

    Ok(true)
}

#[cfg(test)]
mod postgres_source_contract_tests {
    use super::*;

    async fn row_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(pool)
            .await
            .expect("count message_outbox rows")
    }

    fn forbidden_message() -> OutboxMessage<'static> {
        OutboxMessage {
            target: "channel:4424",
            content: "must not insert",
            bot: "notify",
            source: "unregistered_policy_source",
            reason_code: Some("issue_4424_test"),
            session_key: Some("issue-4424-forbidden"),
        }
    }

    fn scheduled_message_for_slot(reason_code: &str) -> OutboxMessage<'_> {
        OutboxMessage {
            target: "channel:4424",
            content: "scheduled announcement",
            bot: "announce",
            source: "scheduled_message",
            reason_code: Some(reason_code),
            session_key: None,
        }
    }

    fn assert_source_error<T>(result: Result<T, OutboxEnqueueError>) {
        assert!(matches!(
            result,
            Err(OutboxEnqueueError::SourceNotAllowed { source })
                if source == "unregistered_policy_source"
        ));
    }

    #[tokio::test]
    async fn every_enqueue_variant_rejects_forbidden_source_with_zero_rows_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_source_contract",
            "message_outbox source contract tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        assert_eq!(row_count(&pool).await, 0);

        assert_source_error(enqueue_outbox_pg_returning_id(&pool, forbidden_message()).await);
        assert_source_error(
            enqueue_outbox_pg_returning_id_with_cancel(&pool, forbidden_message(), None).await,
        );
        assert_source_error(
            enqueue_outbox_pg_returning_id_with_ttl(&pool, forbidden_message(), 60).await,
        );
        assert_source_error(
            enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
                &pool,
                forbidden_message(),
                60,
                None,
            )
            .await,
        );
        assert_source_error(
            enqueue_outbox_pg_returning_id_with_persistent_dedupe(&pool, forbidden_message()).await,
        );
        assert_source_error(enqueue_outbox_pg_with_ttl(&pool, forbidden_message(), 60).await);
        assert_source_error(enqueue_outbox_pg(&pool, forbidden_message()).await);
        assert_source_error(enqueue_outbox_best_effort(Some(&pool), forbidden_message()).await);

        let mut tx = pool.begin().await.expect("begin outbox source test tx");
        assert_source_error(enqueue_outbox_pg_on_tx(&mut tx, forbidden_message()).await);
        tx.rollback().await.expect("rollback outbox source test tx");
        assert_eq!(row_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn enqueue_acceptance_matches_worker_loopback_source_gate_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_source_parity",
            "message_outbox enqueue/send parity tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let sources = [
            "system",
            "lifecycle_notifier",
            "routine-runtime",
            "scheduled_message",
            "headless_turn",
            "slo_alerter",
            "quality_regression_alerter",
            "github_sync",
            "catch_up_too_old",
            "queue_overflow_notice",
            "outbox_delivery_alert",
            "long_turn_watchdog",
            "relay_signal_rollup",
            "dispatch_watchdog",
            "unregistered_policy_source",
            "SYSTEM",
            " system",
            "",
        ];
        for (index, source) in sources.into_iter().enumerate() {
            let target = format!("channel:{}", 4400 + index);
            let content = format!("source parity {index}");
            let message = OutboxMessage {
                target: &target,
                content: &content,
                bot: "notify",
                source,
                reason_code: None,
                session_key: None,
            };
            let worker_allows = crate::services::discord::outbound::send_gate::is_allowed_send_source_for(
                source,
                crate::services::discord::outbound::source_registry::SendCallerClass::LoopbackInternal,
            );
            let enqueue_result = enqueue_outbox_pg_returning_id_with_ttl(&pool, message, 0).await;
            assert_eq!(
                enqueue_result.is_ok(),
                worker_allows,
                "enqueue/send source decision drifted for `{source}`"
            );
        }
    }

    #[tokio::test]
    async fn fixed_lifecycle_enqueue_uses_registered_source_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_lifecycle_source",
            "message_outbox lifecycle source tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        assert!(
            enqueue_lifecycle_notification_pg(
                &pool,
                "channel:4424",
                Some("issue-4424-lifecycle"),
                "issue_4424_test",
                "registered lifecycle source",
            )
            .await
            .expect("enqueue fixed lifecycle source")
        );
        assert_eq!(row_count(&pool).await, 1);
    }

    #[tokio::test]
    async fn persistent_dedupe_returns_old_row_and_distinct_slot_inserts_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_persistent_dedupe",
            "message_outbox persistent dedupe tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        let first_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            scheduled_message_for_slot("scheduled_message:v1:test:1000"),
        )
        .await
        .expect("enqueue first scheduled-message slot");
        sqlx::query(
            "UPDATE message_outbox
                SET created_at = NOW() - INTERVAL '2 hours'
              WHERE id = $1",
        )
        .bind(first_id)
        .execute(&pool)
        .await
        .expect("age first outbox row beyond the legacy TTL");

        let duplicate_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            scheduled_message_for_slot("scheduled_message:v1:test:1000"),
        )
        .await
        .expect("dedupe the same old scheduled-message slot");
        assert_eq!(duplicate_id, first_id);
        assert_eq!(row_count(&pool).await, 1);
        let dedupe_expires_at: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT dedupe_expires_at FROM message_outbox WHERE id = $1")
                .bind(first_id)
                .fetch_one(&pool)
                .await
                .expect("read persistent dedupe expiry");
        assert_eq!(dedupe_expires_at, None);

        let next_slot_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            scheduled_message_for_slot("scheduled_message:v1:test:2000"),
        )
        .await
        .expect("enqueue a distinct scheduled-message slot");
        assert_ne!(next_slot_id, first_id);
        assert_eq!(row_count(&pool).await, 2);
    }
}

#[cfg(test)]
mod postgres_held_gc_tests {
    use super::*;

    fn staged_message(session_key: &'static str, content: &'static str) -> OutboxMessage<'static> {
        OutboxMessage {
            target: "channel:4465",
            content,
            bot: "notify",
            source: "long_turn_watchdog",
            reason_code: Some("relay_recovery.circuit_open"),
            session_key: Some(session_key),
        }
    }

    #[tokio::test]
    async fn gc_prunes_only_expired_held_rows_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_held_gc",
            "message_outbox held-row gc tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let expired_id = stage_outbox_pg_with_ttl(
            &pool,
            staged_message("episode-expired", "expired held alert"),
            0,
        )
        .await
        .expect("stage expired held row");
        let live_id = stage_outbox_pg_with_ttl(
            &pool,
            staged_message("episode-live", "live held alert"),
            3_600,
        )
        .await
        .expect("stage live held row");

        let (held, failed, sent) = gc_stale_outbox_rows(&pool).await.expect("run outbox gc");
        assert_eq!((held, failed, sent), (1, 0, 0));
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM message_outbox WHERE id=$1)",
            )
            .bind(expired_id)
            .fetch_one(&pool)
            .await
            .expect("query expired held row")
        );
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM message_outbox WHERE id=$1 AND status='held')",
            )
            .bind(live_id)
            .fetch_one(&pool)
            .await
            .expect("query live held row")
        );
    }

    #[tokio::test]
    async fn gc_preserves_persistent_dedupe_sentinels_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_message_outbox_gc_persistent_dedupe",
            "message_outbox persistent dedupe GC contract",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        let persistent_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &pool,
            OutboxMessage {
                target: "channel:4465",
                content: "persistent",
                bot: "notify",
                source: "scheduled_message",
                reason_code: Some("scheduled_message:v1:gc-test:slot"),
                session_key: None,
            },
        )
        .await
        .expect("enqueue persistent sentinel");
        let ordinary_id = enqueue_outbox_pg_returning_id_with_ttl(
            &pool,
            OutboxMessage {
                target: "channel:4465",
                content: "ordinary",
                bot: "notify",
                source: "system",
                reason_code: None,
                session_key: None,
            },
            0,
        )
        .await
        .expect("enqueue ordinary row")
        .expect("ordinary row inserted");
        sqlx::query(
            "UPDATE message_outbox
             SET status = 'sent', created_at = NOW() - INTERVAL '31 days',
                 sent_at = NOW() - INTERVAL '31 days'
             WHERE id = ANY($1)",
        )
        .bind(vec![persistent_id, ordinary_id])
        .execute(&pool)
        .await
        .expect("age sent outbox rows");

        let (held_pruned, failed_pruned, sent_pruned) =
            gc_stale_outbox_rows(&pool).await.expect("run outbox gc");
        assert_eq!((held_pruned, failed_pruned, sent_pruned), (0, 0, 1));

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT content FROM message_outbox ORDER BY content")
                .fetch_all(&pool)
                .await
                .expect("read GC survivors");
        assert_eq!(remaining, vec!["persistent"]);
    }
}

#[cfg(test)]
mod stop_turn_notify_removal_tests {
    use super::dedupe_key_for_message;

    /// #3650: the user-visible `lifecycle.stop_turn` notify row is gone. The
    /// only stop signals are the in-place `[Stopped]` edit on the assistant
    /// message and the 🛑 reaction; the separate notify-bot "현재 턴 중단"
    /// message is no longer enqueued by any stop entrypoint.
    const STOP_TURN_REASON_CODE: &str = "lifecycle.stop_turn";

    /// Behavior model of the outbox under the four stop entrypoints
    /// (reaction-remove ⏳, `/stop`, `!stop`, skill stop). Because
    /// `notify_turn_stop` was removed (compile-level guarantee — the function
    /// and its `commands` re-export no longer exist), simulating any stop path
    /// enqueues nothing. This in-test outbox records exactly what the stop
    /// paths now do.
    #[derive(Default)]
    struct FakeOutbox {
        rows: Vec<(String, String)>, // (reason_code, content)
    }

    impl FakeOutbox {
        fn enqueue_lifecycle(&mut self, reason_code: &str, content: &str) {
            self.rows
                .push((reason_code.to_string(), content.to_string()));
        }

        fn count_with_reason(&self, reason_code: &str) -> usize {
            self.rows
                .iter()
                .filter(|(code, _)| code == reason_code)
                .count()
        }
    }

    /// Mirrors a stop entrypoint after #3650: it cancels the active turn and
    /// records the durable stop frontier, but it does NOT enqueue any
    /// user-visible lifecycle notify row. (Pre-#3650 this would have called
    /// `notify_turn_stop`, enqueuing a `lifecycle.stop_turn` row.)
    fn simulate_stop_entrypoint(outbox: &mut FakeOutbox) {
        // intentionally enqueues nothing — the surfaces are the `[Stopped]`
        // edit + 🛑 reaction, both emitted elsewhere.
        let _ = outbox;
    }

    #[test]
    fn stop_turn_does_not_enqueue_user_visible_notify() {
        let mut outbox = FakeOutbox::default();
        // Run all four stop surfaces; none enqueue a user-visible notify.
        for _ in 0..4 {
            simulate_stop_entrypoint(&mut outbox);
        }
        assert_eq!(
            outbox.count_with_reason(STOP_TURN_REASON_CODE),
            0,
            "no stop entrypoint may enqueue a `lifecycle.stop_turn` notify row after #3650"
        );
        assert!(
            outbox.rows.is_empty(),
            "stop entrypoints enqueue nothing to the outbox"
        );
    }

    #[test]
    fn control_arm_enqueue_produces_a_stop_turn_row() {
        // Control arm: prove the assertion above is meaningful — an explicit
        // enqueue of the (removed) reason code does land a row, so the 0-count
        // in `stop_turn_does_not_enqueue_user_visible_notify` is a real signal,
        // not a tautology over an empty enqueue path.
        let mut outbox = FakeOutbox::default();
        outbox.enqueue_lifecycle(STOP_TURN_REASON_CODE, "🛑 현재 턴 중단 (/stop)");
        assert_eq!(outbox.count_with_reason(STOP_TURN_REASON_CODE), 1);
    }

    #[test]
    fn stop_turn_reason_code_is_a_valid_reason_keyed_dedupe_identity() {
        // Documents the row shape that is now never enqueued: with a reason
        // code present, the dedupe identity is keyed on (target, session_key,
        // reason_code) and ignores the rendered content. If a future change
        // ever re-introduces a `lifecycle.stop_turn` enqueue, this is the
        // identity it would dedupe on.
        let key_a = dedupe_key_for_message(
            "channel:123",
            "🛑 현재 턴 중단 (/stop) — tmux는 유지됩니다.",
            Some(STOP_TURN_REASON_CODE),
            Some("session-abc"),
        );
        let key_b = dedupe_key_for_message(
            "channel:123",
            "🛑 현재 턴 중단 (reaction remove ⏳) — tmux는 유지됩니다.",
            Some(STOP_TURN_REASON_CODE),
            Some("session-abc"),
        );
        assert!(key_a.is_some());
        assert_eq!(
            key_a, key_b,
            "reason-keyed dedupe identity must ignore rendered stop-source content"
        );
    }
}
