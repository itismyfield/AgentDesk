use super::*;

pub(crate) fn durable_voice_announcement_pending_key(
    correlation_id: &str,
    semantic_event_id: &str,
) -> String {
    format!("{correlation_id}::{semantic_event_id}")
}

pub(super) fn decode_voice_announcement_value(
    value: serde_json::Value,
) -> Result<VoiceTranscriptAnnouncement, sqlx::Error> {
    serde_json::from_value(value).map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

/// Pre-publish reservation for a readable-only voice transcript announce.
/// The Discord gateway can deliver the create event before the HTTP send call
/// returns a message id, so the intake side can also bind this pending row by
/// the opaque ref embedded in the announce-bot message.
pub(crate) async fn persist_voice_announcement_reservation_durable(
    pool: &PgPool,
    pending_key: &str,
    target_channel_id: ChannelId,
    announce_content: &str,
    announcement: &VoiceTranscriptAnnouncement,
) -> Result<bool, sqlx::Error> {
    let announcement = serde_json::to_value(announcement).map_err(|error| {
        sqlx::Error::Encode(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("serialize voice transcript announcement: {error}"),
        )))
    })?;
    let result = sqlx::query(
        "INSERT INTO voice_transcript_announcement_meta (
             pending_key, target_channel_id, announce_content, announcement
         ) VALUES ($1, $2, $3, $4)
         ON CONFLICT (pending_key) DO UPDATE
         SET target_channel_id = EXCLUDED.target_channel_id,
             announce_content = EXCLUDED.announce_content,
             announcement = EXCLUDED.announcement
         WHERE voice_transcript_announcement_meta.consumed_at IS NULL",
    )
    .bind(pending_key)
    .bind(target_channel_id.get().to_string())
    .bind(announce_content)
    .bind(announcement)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Bind the durable reservation to the Discord message id returned by the
/// announce send. Returns false if the pending row was already consumed or
/// was bound to a different message id by an impossible-looking race.
pub(crate) async fn bind_voice_announcement_durable_message_id(
    pool: &PgPool,
    pending_key: &str,
    message_id: MessageId,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE voice_transcript_announcement_meta
         SET message_id = $2,
             bound_at = COALESCE(bound_at, NOW())
         WHERE pending_key = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $3)
           AND (message_id IS NULL OR message_id = $2)",
    )
    .bind(pending_key)
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Gateway-race recovery: exact pending-key bind for readable-only announce
/// messages carrying an opaque `ADK_VOICE_ANNOUNCE_REF` marker.
pub(crate) async fn bind_pending_voice_announcement_by_key_durable(
    pool: &PgPool,
    pending_key: &str,
    target_channel_id: ChannelId,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "UPDATE voice_transcript_announcement_meta
         SET message_id = $3,
             bound_at = COALESCE(bound_at, NOW())
         WHERE pending_key = $1
           AND target_channel_id = $2
           AND message_id IS NULL
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $4)
         RETURNING announcement",
    )
    .bind(pending_key)
    .bind(target_channel_id.get().to_string())
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value.map(decode_voice_announcement_value).transpose()
}

/// Atomic consume variant for workers that receive a forwarded readable
/// announcement before the posting process successfully binds `message_id`.
#[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
pub(crate) async fn take_pending_voice_announcement_by_key_durable(
    pool: &PgPool,
    pending_key: &str,
    target_channel_id: ChannelId,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "UPDATE voice_transcript_announcement_meta
         SET message_id = $3,
             bound_at = COALESCE(bound_at, NOW()),
             consumed_at = NOW()
         WHERE pending_key = $1
           AND target_channel_id = $2
           AND message_id IS NULL
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $4)
         RETURNING announcement",
    )
    .bind(pending_key)
    .bind(target_channel_id.get().to_string())
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value.map(decode_voice_announcement_value).transpose()
}

pub(crate) async fn cancel_voice_announcement_reservation_durable(
    pool: &PgPool,
    pending_key: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM voice_transcript_announcement_meta
         WHERE pending_key = $1
           AND message_id IS NULL
           AND consumed_at IS NULL",
    )
    .bind(pending_key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(crate) async fn load_voice_announcement_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT announcement
         FROM voice_transcript_announcement_meta
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value.map(decode_voice_announcement_value).transpose()
}

pub(crate) async fn load_consumed_voice_announcement_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT announcement
         FROM voice_transcript_announcement_meta
         WHERE message_id = $1
           AND consumed_at IS NOT NULL
           AND created_at > NOW() - make_interval(secs => $2)",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value.map(decode_voice_announcement_value).transpose()
}

#[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
pub(crate) async fn take_voice_announcement_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "UPDATE voice_transcript_announcement_meta
         SET consumed_at = NOW()
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)
         RETURNING announcement",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value.map(decode_voice_announcement_value).transpose()
}

pub(crate) async fn mark_voice_announcement_durable_consumed(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE voice_transcript_announcement_meta
         SET consumed_at = NOW()
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCEMENT_META_TTL_SECS as f64)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(crate) async fn gc_expired_voice_announcement_meta_pg(
    pool: &PgPool,
    ttl: Duration,
) -> Result<u64, sqlx::Error> {
    let ttl_secs = ttl.as_secs_f64();
    let result = sqlx::query(
        "DELETE FROM voice_transcript_announcement_meta
         WHERE created_at < NOW() - make_interval(secs => $1)",
    )
    .bind(ttl_secs)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

fn durable_pending_message_id(correlation_id: &str) -> String {
    format!("{DURABLE_HANDOFF_PENDING_PREFIX}{correlation_id}")
}

fn is_durable_pending_message_id(message_id: &str) -> bool {
    message_id.starts_with(DURABLE_HANDOFF_PENDING_PREFIX)
}

/// Persist a voice-background handoff marker to the durable side store
/// (#2274). The process-local in-memory store remains the hot read path;
/// this PG row is the durable source of truth that survives a dcserver
/// restart partway through a long background turn.
///
/// `ON CONFLICT … DO UPDATE` deliberately refuses to update rows that were
/// already consumed. A late publish/persist retry must not resurrect a
/// handoff after terminal delivery has claimed it (#2392).
#[allow(dead_code)] // voice runtime wired only when voice config enabled; no target exercises it. See #3034
pub(crate) async fn persist_handoff_durable(
    pool: &PgPool,
    message_id: MessageId,
    meta: &VoiceBackgroundHandoffMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO voice_background_handoff_meta (
             message_id, voice_channel_id, background_channel_id, agent_id
         ) VALUES ($1, $2, $3, $4)
         ON CONFLICT (message_id) DO UPDATE
         SET voice_channel_id = EXCLUDED.voice_channel_id,
             background_channel_id = EXCLUDED.background_channel_id,
             agent_id = EXCLUDED.agent_id
         WHERE voice_background_handoff_meta.consumed_at IS NULL",
    )
    .bind(message_id.get().to_string())
    .bind(meta.voice_channel_id.to_string())
    .bind(meta.background_channel_id.to_string())
    .bind(meta.agent_id.as_ref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Pre-publish durable reservation for a voice-background handoff (#2392).
/// The row is keyed by a synthetic pending id until Discord returns the real
/// message id. If terminal delivery wins the race before bind, it claims this
/// row by parsing the correlation marker embedded in the announce prompt.
pub(crate) async fn persist_handoff_reservation_durable(
    pool: &PgPool,
    correlation_id: &str,
    meta: &VoiceBackgroundHandoffMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO voice_background_handoff_meta (
             message_id, voice_channel_id, background_channel_id, agent_id
         ) VALUES ($1, $2, $3, $4)
         ON CONFLICT (message_id) DO UPDATE
         SET voice_channel_id = EXCLUDED.voice_channel_id,
             background_channel_id = EXCLUDED.background_channel_id,
             agent_id = EXCLUDED.agent_id
         WHERE voice_background_handoff_meta.consumed_at IS NULL",
    )
    .bind(durable_pending_message_id(correlation_id))
    .bind(meta.voice_channel_id.to_string())
    .bind(meta.background_channel_id.to_string())
    .bind(meta.agent_id.as_ref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Promote a pending durable reservation to the actual Discord message id.
/// Returns `false` when the pending row was already consumed by the
/// correlation fallback or was otherwise absent; callers must not insert a
/// fresh actual-message row in that case, or they would resurrect a consumed
/// handoff (#2392).
pub(crate) async fn bind_handoff_durable_message_id(
    pool: &PgPool,
    correlation_id: &str,
    message_id: MessageId,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE voice_background_handoff_meta
         SET message_id = $1
         WHERE message_id = $2
           AND consumed_at IS NULL
           AND expires_at > NOW()",
    )
    .bind(message_id.get().to_string())
    .bind(durable_pending_message_id(correlation_id))
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(crate) async fn cancel_handoff_reservation_durable(
    pool: &PgPool,
    correlation_id: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM voice_background_handoff_meta
         WHERE message_id = $1
           AND consumed_at IS NULL",
    )
    .bind(durable_pending_message_id(correlation_id))
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Non-destructive read used to check whether a marker exists for a given
/// `message_id`. Mirrors `peek_durable` in the announce path.
pub(crate) async fn load_handoff_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceBackgroundHandoffMeta>, sqlx::Error> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT voice_channel_id, background_channel_id, agent_id
         FROM voice_background_handoff_meta
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND expires_at > NOW()",
    )
    .bind(message_id.get().to_string())
    .fetch_optional(pool)
    .await?;
    row.map(|(voice_channel_id, background_channel_id, agent_id)| {
        Ok::<_, sqlx::Error>(VoiceBackgroundHandoffMeta {
            voice_channel_id: voice_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("voice_channel_id not u64: {error}"),
                )))
            })?,
            background_channel_id: background_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("background_channel_id not u64: {error}"),
                )))
            })?,
            agent_id,
            // A row that came from PG is durable by definition.
            local_only_fallback: false,
        })
    })
    .transpose()
}

pub(crate) async fn take_handoff_reservation_durable(
    pool: &PgPool,
    correlation_id: &str,
) -> Result<Option<VoiceBackgroundHandoffMeta>, sqlx::Error> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "UPDATE voice_background_handoff_meta
         SET consumed_at = NOW()
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND expires_at > NOW()
         RETURNING voice_channel_id, background_channel_id, agent_id",
    )
    .bind(durable_pending_message_id(correlation_id))
    .fetch_optional(pool)
    .await?;
    row.map(|(voice_channel_id, background_channel_id, agent_id)| {
        Ok::<_, sqlx::Error>(VoiceBackgroundHandoffMeta {
            voice_channel_id: voice_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("voice_channel_id not u64: {error}"),
                )))
            })?,
            background_channel_id: background_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("background_channel_id not u64: {error}"),
                )))
            })?,
            agent_id,
            local_only_fallback: false,
        })
    })
    .transpose()
}

/// Atomic claim — `UPDATE … SET consumed_at = NOW() RETURNING …` so that
/// two callers racing on the same row cannot both succeed. Concurrent
/// callers (e.g. two terminal-delivery hooks in a clustered deployment)
/// receive `Ok(None)` and MUST abort routing.
///
/// Crash semantics mirror the announce path: the row is marked consumed,
/// not deleted; the GC sweep removes the row after TTL. If a worker
/// crashes after `take_handoff_durable` but before routing, the spoken
/// summary is dropped — that is the conservative choice, matching the
/// fail-safe-drop posture #2236 established.
pub(crate) async fn take_handoff_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceBackgroundHandoffMeta>, sqlx::Error> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "UPDATE voice_background_handoff_meta
         SET consumed_at = NOW()
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND expires_at > NOW()
         RETURNING voice_channel_id, background_channel_id, agent_id",
    )
    .bind(message_id.get().to_string())
    .fetch_optional(pool)
    .await?;
    row.map(|(voice_channel_id, background_channel_id, agent_id)| {
        Ok::<_, sqlx::Error>(VoiceBackgroundHandoffMeta {
            voice_channel_id: voice_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("voice_channel_id not u64: {error}"),
                )))
            })?,
            background_channel_id: background_channel_id.parse().map_err(|error| {
                sqlx::Error::Decode(Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("background_channel_id not u64: {error}"),
                )))
            })?,
            agent_id,
            // A row that came from PG is durable by definition.
            local_only_fallback: false,
        })
    })
    .transpose()
}

/// Boot-time rehydration — copy every live, unconsumed, within-TTL row
/// from the PG side store into the in-memory store so callers on the hot
/// path (synchronous `get_handoff` / `take_handoff`) keep working after a
/// dcserver restart without an async fallback ripple.
///
/// #2274 Codex review finding #3: each rehydrated row carries its
/// PG-recorded age, and the in-memory expiry is set to the REMAINING
/// portion of the durable TTL — never a fresh 24-hour lease. Without
/// this, a row that already lived 23 hours in PG could survive another
/// 24 hours in memory while PG GC deletes the durable source of truth.
///
/// Best-effort: a PG error here is logged and ignored. Subsequent
/// dispatches will still write through and terminal-delivery callers fall
/// back to `take_handoff_durable` directly when the in-memory store
/// misses (see `voice_background_completion_target`).
///
/// Returns the count of rows rehydrated for observability.
pub(crate) async fn rehydrate_handoffs_from_pg(pool: &PgPool) -> Result<u64, sqlx::Error> {
    // `age_secs` is computed in SQL so the truth horizon is PG's clock,
    // not the local process clock — same source of truth used by the
    // load/take/GC paths.
    // `remaining_secs` is computed from `expires_at` so the in-memory
    // lifetime tracks the refreshed durable deadline, not just
    // `created_at`. Rows whose `expires_at` is in the past are already
    // excluded by the `expires_at > NOW()` filter.
    let rows: Vec<(String, String, String, Option<String>, f64)> = sqlx::query_as(
        "SELECT message_id,
                voice_channel_id,
                background_channel_id,
                agent_id,
                EXTRACT(EPOCH FROM (expires_at - NOW()))::float8 AS remaining_secs
         FROM voice_background_handoff_meta
         WHERE consumed_at IS NULL
           AND expires_at > NOW()",
    )
    .fetch_all(pool)
    .await?;
    let store = global_store();
    let mut count: u64 = 0;
    for (message_id, voice_channel_id, background_channel_id, agent_id, remaining_secs) in rows {
        if is_durable_pending_message_id(&message_id) {
            continue;
        }
        let Ok(message_id_u64) = message_id.parse::<u64>() else {
            tracing::warn!(
                message_id,
                "voice_background_handoff_meta rehydrate skipped row with non-u64 message_id"
            );
            continue;
        };
        let Ok(voice_channel_id_u64) = voice_channel_id.parse::<u64>() else {
            tracing::warn!(
                message_id_u64,
                voice_channel_id,
                "voice_background_handoff_meta rehydrate skipped row with non-u64 voice_channel_id"
            );
            continue;
        };
        let Ok(background_channel_id_u64) = background_channel_id.parse::<u64>() else {
            tracing::warn!(
                message_id_u64,
                background_channel_id,
                "voice_background_handoff_meta rehydrate skipped row with non-u64 background_channel_id"
            );
            continue;
        };
        // Clamp to 1 s so the entry exists in memory — the durable claim
        // path is still authoritative and will refuse already-consumed rows.
        let remaining = Duration::from_secs_f64(remaining_secs.max(1.0));
        store.insert_handoff_with_remaining_ttl(
            MessageId::new(message_id_u64),
            VoiceBackgroundHandoffMeta {
                voice_channel_id: voice_channel_id_u64,
                background_channel_id: background_channel_id_u64,
                agent_id,
                // Rehydrated entries are backed by a durable PG row.
                local_only_fallback: false,
            },
            remaining,
        );
        count += 1;
    }
    Ok(count)
}

/// Delete durable rows whose effective TTL has elapsed. The live deadline is
/// stored in `expires_at` (migration 0064), which is refreshed by
/// `refresh_handoff_ttl_durable` when the watchdog deadline is extended.
/// Wired into the leader-only maintenance scheduler.
pub(crate) async fn gc_expired_voice_background_handoff_meta_pg(
    pool: &PgPool,
    _ttl: Duration,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM voice_background_handoff_meta
         WHERE expires_at < NOW()",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Refresh the durable TTL for a handoff row by resetting `expires_at` to
/// `NOW() + DURABLE_HANDOFF_META_TTL_SECS` (#2352).
///
/// Called after a successful watchdog deadline extension so long-running
/// background turns do not lose their PG routing marker when the GC runs.
/// Idempotent: no-op when the row is already consumed or absent.
///
/// Returns `true` when a live row was found and updated.
pub(crate) async fn refresh_handoff_ttl_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE voice_background_handoff_meta
         SET expires_at = NOW() + make_interval(secs => $1)
         WHERE message_id = $2
           AND consumed_at IS NULL",
    )
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
    .bind(message_id.get().to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
