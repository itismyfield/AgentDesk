use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use poise::serenity_prelude::{ChannelId, MessageId};
use sqlx::PgPool;

use super::prompt::VoiceTranscriptAnnouncement;

const ANNOUNCEMENT_META_TTL: Duration = Duration::from_secs(30);
/// Durable voice transcript announcement metadata can outlive the short
/// process-local TTL because intake may be queued to another process or sit
/// behind an active turn before worker execution.
pub(crate) const DURABLE_ANNOUNCEMENT_META_TTL_SECS: i64 = 24 * 60 * 60;
/// Voice-background handoff markers can outlive the short announce TTL because
/// the background turn they trigger may run for minutes — or, with watchdog
/// extensions, hours — before the terminal-delivery callback consults the
/// marker.
///
/// 24h is generous: `turn_orchestrator::extend_active_watchdog_deadline` does
/// not impose a practical cap on the number of extensions
/// (`count_limit = u32::MAX`, `total_secs_limit = u64::MAX`), so a productive
/// long turn can legitimately exceed the 1-hour default watchdog. Keeping
/// markers alive for a full day prevents the spoken-summary path from
/// silently dropping completions on extended turns (Codex #2274 review
/// finding #2). Anything older than 24h almost certainly represents a
/// turn that crashed or never reached terminal delivery.
const HANDOFF_META_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Durable handoff rows older than this are treated as expired and ignored
/// by the durable load/take helpers. The leader-only GC sweep
/// (`gc_expired_voice_background_handoff_meta_pg`) deletes them. Mirrors
/// the in-memory `HANDOFF_META_TTL` — see that constant for the rationale.
pub(crate) const DURABLE_HANDOFF_META_TTL_SECS: i64 = 24 * 60 * 60;
const DURABLE_HANDOFF_PENDING_PREFIX: &str = "pending:";

#[derive(Debug, Clone)]
struct StoredVoiceTranscriptAnnouncement {
    announcement: VoiceTranscriptAnnouncement,
    expires_at: Instant,
    accepted_replay: bool,
}

/// Typed marker recorded by the voice foreground → background dispatch path
/// (`dispatch_voice_background_handoff`). The turn bridge consults this on
/// terminal delivery to decide whether the spoken summary should be routed
/// into the foreground voice channel.
///
/// This replaces the user-controllable Korean-prefix substring match that
/// `is_voice_background_handoff_prompt` previously used (issue #2236).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceBackgroundHandoffMeta {
    /// Voice channel that originated the handoff (where the spoken summary
    /// should be routed if it is delivered).
    pub voice_channel_id: u64,
    /// Background text channel where the handoff prompt was posted.
    pub background_channel_id: u64,
    /// Agent id from the active voice route. Used by
    /// `voice_channel_for_background` to disambiguate when multiple agents
    /// map onto the same background channel.
    pub agent_id: Option<String>,
    /// Set at dispatch time when the durable PG write failed (or no pool
    /// was available). When `true`, terminal delivery on this node may
    /// fall back to consuming the in-memory marker even though no PG row
    /// exists — restoring the pre-#2274 local-only behaviour under DB
    /// unavailability. Always `false` for markers loaded from PG, since
    /// those rows are themselves the durable source of truth.
    ///
    /// Codex #2274 round-2 finding: without this flag, a transient PG
    /// outage at dispatch would silently drop the spoken summary because
    /// the PG-authoritative claim path would return `Ok(None)` and refuse
    /// to route. The flag scopes the fallback to exactly the case it is
    /// meant to handle (persist failed AT DISPATCH) and never to the case
    /// PG actually consumed a real row (since `forget_handoff` clears the
    /// local copy in that branch).
    pub local_only_fallback: bool,
}

#[derive(Debug, Clone)]
struct StoredVoiceBackgroundHandoffMeta {
    meta: VoiceBackgroundHandoffMeta,
    expires_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceAnnouncementMetaStore {
    entries: RwLock<HashMap<u64, StoredVoiceTranscriptAnnouncement>>,
    handoff_entries: RwLock<HashMap<u64, StoredVoiceBackgroundHandoffMeta>>,
    pending_handoff_entries: RwLock<HashMap<String, StoredVoiceBackgroundHandoffMeta>>,
}

impl VoiceAnnouncementMetaStore {
    pub(crate) fn insert(&self, message_id: MessageId, announcement: VoiceTranscriptAnnouncement) {
        self.insert_with_acceptance(message_id, announcement, false);
    }

    pub(crate) fn insert_accepted_replay(
        &self,
        message_id: MessageId,
        announcement: VoiceTranscriptAnnouncement,
    ) {
        self.insert_with_acceptance(message_id, announcement, true);
    }

    fn insert_with_acceptance(
        &self,
        message_id: MessageId,
        announcement: VoiceTranscriptAnnouncement,
        accepted_replay: bool,
    ) {
        if let Ok(mut entries) = self.entries.write() {
            let now = Instant::now();
            prune_expired_locked(&mut entries, now);
            entries.insert(
                message_id.get(),
                StoredVoiceTranscriptAnnouncement {
                    announcement,
                    expires_at: now + ANNOUNCEMENT_META_TTL,
                    accepted_replay,
                },
            );
        } else {
            tracing::warn!(
                message_id = message_id.get(),
                "voice transcript announcement metadata insert failed because store lock is poisoned"
            );
        }
    }

    pub(crate) fn take(&self, message_id: MessageId) -> Option<VoiceTranscriptAnnouncement> {
        self.take_with_acceptance(message_id)
            .map(|(announcement, _)| announcement)
    }

    pub(crate) fn take_with_acceptance(
        &self,
        message_id: MessageId,
    ) -> Option<(VoiceTranscriptAnnouncement, bool)> {
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => {
                tracing::warn!(
                    message_id = message_id.get(),
                    "voice transcript announcement metadata take failed because store lock is poisoned"
                );
                return None;
            }
        };
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries
            .remove(&message_id.get())
            .map(|stored| (stored.announcement, stored.accepted_replay))
    }

    pub(crate) fn contains(&self, message_id: MessageId) -> bool {
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries.contains_key(&message_id.get())
    }

    pub(crate) fn insert_handoff(&self, message_id: MessageId, meta: VoiceBackgroundHandoffMeta) {
        self.insert_handoff_with_remaining_ttl(message_id, meta, HANDOFF_META_TTL);
    }

    pub(crate) fn reserve_handoff(&self, correlation_id: &str, meta: VoiceBackgroundHandoffMeta) {
        if let Ok(mut entries) = self.pending_handoff_entries.write() {
            let now = Instant::now();
            prune_pending_handoff_expired_locked(&mut entries, now);
            entries.insert(
                correlation_id.to_string(),
                StoredVoiceBackgroundHandoffMeta {
                    meta,
                    expires_at: now + HANDOFF_META_TTL,
                },
            );
        }
    }

    pub(crate) fn bind_handoff_message_id(
        &self,
        correlation_id: &str,
        message_id: MessageId,
    ) -> bool {
        let stored = {
            let Ok(mut pending) = self.pending_handoff_entries.write() else {
                return false;
            };
            let now = Instant::now();
            prune_pending_handoff_expired_locked(&mut pending, now);
            pending.remove(correlation_id)
        };
        let Some(stored) = stored else {
            return false;
        };
        if let Ok(mut entries) = self.handoff_entries.write() {
            let now = Instant::now();
            prune_handoff_expired_locked(&mut entries, now);
            entries.insert(
                message_id.get(),
                StoredVoiceBackgroundHandoffMeta {
                    meta: stored.meta,
                    expires_at: now + HANDOFF_META_TTL,
                },
            );
            true
        } else {
            false
        }
    }

    pub(crate) fn cancel_handoff_reservation(&self, correlation_id: &str) -> bool {
        let Ok(mut entries) = self.pending_handoff_entries.write() else {
            return false;
        };
        let now = Instant::now();
        prune_pending_handoff_expired_locked(&mut entries, now);
        entries.remove(correlation_id).is_some()
    }

    pub(crate) fn mark_handoff_reservation_local_only_fallback(
        &self,
        correlation_id: &str,
    ) -> bool {
        let Ok(mut entries) = self.pending_handoff_entries.write() else {
            return false;
        };
        let now = Instant::now();
        prune_pending_handoff_expired_locked(&mut entries, now);
        if let Some(stored) = entries.get_mut(correlation_id) {
            stored.meta.local_only_fallback = true;
            true
        } else {
            false
        }
    }

    /// Insert with an explicit remaining-lifetime override. Used by
    /// `rehydrate_handoffs_from_pg` (#2274 Codex review finding #3) so a
    /// row that already survived 59 minutes in PG only gets the matching
    /// remaining-TTL in memory — not a fresh 24-hour lease. Without this,
    /// a stale local marker could outlive its durable row and route a
    /// completion summary after PG GC has already deleted the source of
    /// truth.
    pub(crate) fn insert_handoff_with_remaining_ttl(
        &self,
        message_id: MessageId,
        meta: VoiceBackgroundHandoffMeta,
        remaining: Duration,
    ) {
        if let Ok(mut entries) = self.handoff_entries.write() {
            let now = Instant::now();
            prune_handoff_expired_locked(&mut entries, now);
            entries.insert(
                message_id.get(),
                StoredVoiceBackgroundHandoffMeta {
                    meta,
                    expires_at: now + remaining,
                },
            );
        }
    }

    /// Drop a specific marker from the in-memory store without consuming
    /// it. Used to clear stale local state when the durable PG claim is
    /// the authoritative source and reports the row is gone (#2274 Codex
    /// review finding #1).
    pub(crate) fn forget_handoff(&self, message_id: MessageId) {
        if let Ok(mut entries) = self.handoff_entries.write() {
            entries.remove(&message_id.get());
        }
    }

    /// Flip the `local_only_fallback` flag on an in-memory marker. Called
    /// at dispatch time when the durable PG write failed (or no pool was
    /// available), so the terminal-delivery path knows it is safe to fall
    /// back to consuming the local marker without a backing PG row.
    /// Returns true iff a marker existed and was updated.
    ///
    /// Codex #2274 round-2 finding: see the `local_only_fallback` doc
    /// comment on `VoiceBackgroundHandoffMeta`.
    pub(crate) fn mark_handoff_local_only_fallback(&self, message_id: MessageId) -> bool {
        let Ok(mut entries) = self.handoff_entries.write() else {
            return false;
        };
        let now = Instant::now();
        prune_handoff_expired_locked(&mut entries, now);
        if let Some(stored) = entries.get_mut(&message_id.get()) {
            stored.meta.local_only_fallback = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn get_handoff(&self, message_id: MessageId) -> Option<VoiceBackgroundHandoffMeta> {
        let mut entries = self.handoff_entries.write().ok()?;
        let now = Instant::now();
        prune_handoff_expired_locked(&mut entries, now);
        entries
            .get(&message_id.get())
            .map(|stored| stored.meta.clone())
    }

    pub(crate) fn take_handoff(&self, message_id: MessageId) -> Option<VoiceBackgroundHandoffMeta> {
        let mut entries = self.handoff_entries.write().ok()?;
        let now = Instant::now();
        prune_handoff_expired_locked(&mut entries, now);
        entries.remove(&message_id.get()).map(|stored| stored.meta)
    }

    pub(crate) fn get_handoff_reservation(
        &self,
        correlation_id: &str,
    ) -> Option<VoiceBackgroundHandoffMeta> {
        let mut entries = self.pending_handoff_entries.write().ok()?;
        let now = Instant::now();
        prune_pending_handoff_expired_locked(&mut entries, now);
        entries
            .get(correlation_id)
            .map(|stored| stored.meta.clone())
    }

    pub(crate) fn take_handoff_reservation(
        &self,
        correlation_id: &str,
    ) -> Option<VoiceBackgroundHandoffMeta> {
        let mut entries = self.pending_handoff_entries.write().ok()?;
        let now = Instant::now();
        prune_pending_handoff_expired_locked(&mut entries, now);
        entries.remove(correlation_id).map(|stored| stored.meta)
    }

    /// #2266: non-consuming clone of the stored announcement so the intake-gate
    /// busy-channel paths can embed the payload in the queued `Intervention`
    /// WITHOUT draining the store. The active dispatch path still calls
    /// `take()` to consume the entry once the queued turn finally runs and
    /// reinserts the payload — but for the intake-time queue paths the
    /// metadata must travel inside the Intervention because the in-memory
    /// store TTL (30s) is shorter than typical queue dwell times.
    pub(crate) fn peek_clone(&self, message_id: MessageId) -> Option<VoiceTranscriptAnnouncement> {
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => {
                tracing::warn!(
                    message_id = message_id.get(),
                    "voice transcript announcement metadata peek failed because store lock is poisoned"
                );
                return None;
            }
        };
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries
            .get(&message_id.get())
            .map(|stored| stored.announcement.clone())
    }
}

fn prune_handoff_expired_locked(
    entries: &mut HashMap<u64, StoredVoiceBackgroundHandoffMeta>,
    now: Instant,
) {
    entries.retain(|_, stored| stored.expires_at > now);
}

fn prune_pending_handoff_expired_locked(
    entries: &mut HashMap<String, StoredVoiceBackgroundHandoffMeta>,
    now: Instant,
) {
    entries.retain(|_, stored| stored.expires_at > now);
}

fn prune_expired_locked(
    entries: &mut HashMap<u64, StoredVoiceTranscriptAnnouncement>,
    now: Instant,
) {
    entries.retain(|_, stored| stored.expires_at > now);
}

pub(crate) fn global_store() -> &'static VoiceAnnouncementMetaStore {
    static STORE: OnceLock<VoiceAnnouncementMetaStore> = OnceLock::new();
    STORE.get_or_init(VoiceAnnouncementMetaStore::default)
}

pub(crate) fn durable_voice_announcement_pending_key(
    correlation_id: &str,
    semantic_event_id: &str,
) -> String {
    format!("{correlation_id}::{semantic_event_id}")
}

fn decode_voice_announcement_value(
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
           AND created_at > NOW() - make_interval(secs => $3)",
    )
    .bind(message_id.get().to_string())
    .bind(durable_pending_message_id(correlation_id))
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
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
           AND created_at > NOW() - make_interval(secs => $2)",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
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
           AND created_at > NOW() - make_interval(secs => $2)
         RETURNING voice_channel_id, background_channel_id, agent_id",
    )
    .bind(durable_pending_message_id(correlation_id))
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
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
           AND created_at > NOW() - make_interval(secs => $2)
         RETURNING voice_channel_id, background_channel_id, agent_id",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
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
    let rows: Vec<(String, String, String, Option<String>, f64)> = sqlx::query_as(
        "SELECT message_id,
                voice_channel_id,
                background_channel_id,
                agent_id,
                EXTRACT(EPOCH FROM (NOW() - created_at))::float8 AS age_secs
         FROM voice_background_handoff_meta
         WHERE consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $1)",
    )
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
    .fetch_all(pool)
    .await?;
    let store = global_store();
    let mut count: u64 = 0;
    for (message_id, voice_channel_id, background_channel_id, agent_id, age_secs) in rows {
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
        // Compute remaining TTL from PG-reported age. Clamp the lower
        // bound to a single second so the entry exists at all — the
        // durable claim path remains the source of truth and will
        // refuse stale rows even if a barely-alive local entry briefly
        // survives.
        let total_ttl_secs = DURABLE_HANDOFF_META_TTL_SECS as f64;
        let remaining_secs = (total_ttl_secs - age_secs.max(0.0)).max(1.0);
        let remaining = Duration::from_secs_f64(remaining_secs);
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

/// Delete durable rows older than `ttl`. Wired into the leader-only
/// maintenance scheduler so cleanup runs without a new background worker.
pub(crate) async fn gc_expired_voice_background_handoff_meta_pg(
    pool: &PgPool,
    ttl: Duration,
) -> Result<u64, sqlx::Error> {
    let ttl_secs = ttl.as_secs_f64();
    let result = sqlx::query(
        "DELETE FROM voice_background_handoff_meta
         WHERE created_at < NOW() - make_interval(secs => $1)",
    )
    .bind(ttl_secs)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announcement() -> VoiceTranscriptAnnouncement {
        VoiceTranscriptAnnouncement {
            transcript: "상태 알려줘".to_string(),
            user_id: "42".to_string(),
            utterance_id: "utt-1".to_string(),
            language: "ko-KR".to_string(),
            verbose_progress: true,
            started_at: Some("2026-05-16T10:00:00+09:00".to_string()),
            completed_at: Some("2026-05-16T10:00:01+09:00".to_string()),
            samples_written: Some(48_000),
        }
    }

    #[test]
    fn store_is_one_shot() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(123);
        store.insert(message_id, announcement());

        assert_eq!(store.take(message_id).unwrap().utterance_id, "utt-1");
        assert!(store.take(message_id).is_none());
    }

    #[test]
    fn contains_does_not_consume_entry() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(124);
        store.insert(message_id, announcement());

        assert!(store.contains(message_id));
        assert_eq!(store.take(message_id).unwrap().utterance_id, "utt-1");
    }

    #[test]
    fn store_distinguishes_accepted_replay_entries() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(125);
        let replay_message_id = MessageId::new(126);

        store.insert(message_id, announcement());
        store.insert_accepted_replay(replay_message_id, announcement());

        let (_, accepted) = store
            .take_with_acceptance(message_id)
            .expect("normal entry");
        assert!(!accepted);
        let (_, accepted_replay) = store
            .take_with_acceptance(replay_message_id)
            .expect("accepted replay entry");
        assert!(accepted_replay);
    }

    #[test]
    fn handoff_store_round_trips_typed_metadata() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(200);
        let meta = VoiceBackgroundHandoffMeta {
            voice_channel_id: 300,
            background_channel_id: 200,
            agent_id: Some("project-agentdesk".to_string()),
            local_only_fallback: false,
        };

        store.insert_handoff(message_id, meta.clone());
        assert_eq!(store.get_handoff(message_id), Some(meta.clone()));
        // get_handoff does not consume — same call should still return.
        assert_eq!(store.get_handoff(message_id), Some(meta.clone()));
        assert_eq!(store.take_handoff(message_id), Some(meta));
        assert!(store.get_handoff(message_id).is_none());
    }

    #[test]
    fn pending_handoff_reservation_can_win_before_message_bind() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(201);
        let correlation_id = "voice-bg:0123456789abcdef0123456789abcdef";
        let meta = VoiceBackgroundHandoffMeta {
            voice_channel_id: 301,
            background_channel_id: 201,
            agent_id: Some("project-agentdesk".to_string()),
            local_only_fallback: false,
        };

        store.reserve_handoff(correlation_id, meta.clone());
        assert!(store.get_handoff(message_id).is_none());
        assert_eq!(store.take_handoff_reservation(correlation_id), Some(meta));

        assert!(
            !store.bind_handoff_message_id(correlation_id, message_id),
            "late bind must not recreate a consumed pending reservation"
        );
        assert!(store.get_handoff(message_id).is_none());
    }

    #[test]
    fn pending_handoff_reservation_binds_to_message_id() {
        let store = VoiceAnnouncementMetaStore::default();
        let message_id = MessageId::new(202);
        let correlation_id = "voice-bg:abcdef0123456789abcdef0123456789";
        let meta = VoiceBackgroundHandoffMeta {
            voice_channel_id: 302,
            background_channel_id: 202,
            agent_id: None,
            local_only_fallback: false,
        };

        store.reserve_handoff(correlation_id, meta.clone());

        assert!(store.bind_handoff_message_id(correlation_id, message_id));
        assert!(store.get_handoff_reservation(correlation_id).is_none());
        assert_eq!(store.take_handoff(message_id), Some(meta));
    }

    #[test]
    fn handoff_store_returns_none_when_absent() {
        let store = VoiceAnnouncementMetaStore::default();
        assert!(store.get_handoff(MessageId::new(999)).is_none());
        assert!(store.take_handoff(MessageId::new(999)).is_none());
    }

    fn handoff_meta(
        voice: u64,
        background: u64,
        agent: Option<&str>,
    ) -> VoiceBackgroundHandoffMeta {
        VoiceBackgroundHandoffMeta {
            voice_channel_id: voice,
            background_channel_id: background,
            agent_id: agent.map(str::to_string),
            local_only_fallback: false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_binds_pending_by_key_then_consumes_once() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_001);
        let message_id = MessageId::new(82_001);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91001:utt-1", "announce:generation:1");
        let content = "🎙️ \"상태 알려줘\"";
        let expected = announcement();

        persist_voice_announcement_reservation_durable(
            &pool,
            &pending_key,
            channel_id,
            content,
            &expected,
        )
        .await
        .expect("persist durable voice announcement reservation");

        assert!(
            load_voice_announcement_durable(&pool, message_id)
                .await
                .expect("load before bind")
                .is_none(),
            "unbound pending row must not load by message id"
        );

        let bound = bind_pending_voice_announcement_by_key_durable(
            &pool,
            &pending_key,
            channel_id,
            message_id,
        )
        .await
        .expect("bind pending by key")
        .expect("pending row found");
        assert_eq!(bound, expected);

        let loaded = load_voice_announcement_durable(&pool, message_id)
            .await
            .expect("load after bind")
            .expect("bound row loads by message id");
        assert_eq!(loaded, expected);

        assert!(
            bind_pending_voice_announcement_by_key_durable(
                &pool,
                &pending_key,
                channel_id,
                message_id,
            )
            .await
            .expect("second key bind")
            .is_none(),
            "key bind must be one-shot once the row has a message id"
        );

        let taken = take_voice_announcement_durable(&pool, message_id)
            .await
            .expect("take durable voice announcement")
            .expect("first take consumes");
        assert_eq!(taken, expected);
        assert!(
            take_voice_announcement_durable(&pool, message_id)
                .await
                .expect("second take")
                .is_none(),
            "durable voice announcement consume is one-shot"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_bind_by_pending_key_is_idempotent() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_002);
        let message_id = MessageId::new(82_002);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91002:utt-1", "announce:generation:1");
        let content = "🎙️ \"다시 알려줘\"";
        let expected = announcement();

        persist_voice_announcement_reservation_durable(
            &pool,
            &pending_key,
            channel_id,
            content,
            &expected,
        )
        .await
        .expect("persist durable voice announcement reservation");

        assert!(
            bind_voice_announcement_durable_message_id(&pool, &pending_key, message_id)
                .await
                .expect("bind message id"),
            "first bind should succeed"
        );
        assert!(
            bind_voice_announcement_durable_message_id(&pool, &pending_key, message_id)
                .await
                .expect("idempotent rebind"),
            "same message id rebind should be idempotent"
        );
        assert_eq!(
            load_voice_announcement_durable(&pool, message_id)
                .await
                .expect("load after bind"),
            Some(expected)
        );
        assert!(
            mark_voice_announcement_durable_consumed(&pool, message_id)
                .await
                .expect("mark consumed"),
            "bound row should mark consumed"
        );
        assert!(
            !bind_voice_announcement_durable_message_id(
                &pool,
                &pending_key,
                MessageId::new(82_003),
            )
            .await
            .expect("late bind after consume"),
            "consumed row must not bind a new message id"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_voice_announcement_concurrent_takes_yield_exactly_one_claim() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_003);
        let message_id = MessageId::new(82_003);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91003:utt-1", "announce:generation:1");
        let content = "🎙️ \"동시에 처리해줘\"";
        let expected = announcement();

        persist_voice_announcement_reservation_durable(
            &pool,
            &pending_key,
            channel_id,
            content,
            &expected,
        )
        .await
        .expect("persist durable voice announcement reservation");
        assert!(
            bind_voice_announcement_durable_message_id(&pool, &pending_key, message_id)
                .await
                .expect("bind message id")
        );

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let task_a = tokio::spawn(async move {
            take_voice_announcement_durable(&pool_a, message_id)
                .await
                .unwrap()
        });
        let task_b = tokio::spawn(async move {
            take_voice_announcement_durable(&pool_b, message_id)
                .await
                .unwrap()
        });
        let (result_a, result_b) =
            tokio::try_join!(task_a, task_b).expect("join concurrent consumers");
        let winners = [&result_a, &result_b]
            .iter()
            .filter(|result| result.is_some())
            .count();
        assert_eq!(winners, 1, "exactly one atomic durable consumer must win");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_pending_key_disambiguates_same_content() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_004);
        let message_a = MessageId::new(82_004);
        let message_b = MessageId::new(82_005);
        let key_a =
            durable_voice_announcement_pending_key("voice:1:91004:utt-a", "announce:generation:1");
        let key_b =
            durable_voice_announcement_pending_key("voice:1:91004:utt-b", "announce:generation:1");
        let content = "🎙️ \"같은 말\"";
        let mut meta_a = announcement();
        meta_a.utterance_id = "utt-a".to_string();
        let mut meta_b = announcement();
        meta_b.utterance_id = "utt-b".to_string();

        persist_voice_announcement_reservation_durable(&pool, &key_a, channel_id, content, &meta_a)
            .await
            .expect("persist first reservation");
        persist_voice_announcement_reservation_durable(&pool, &key_b, channel_id, content, &meta_b)
            .await
            .expect("persist second reservation");

        assert_eq!(
            bind_pending_voice_announcement_by_key_durable(&pool, &key_b, channel_id, message_b)
                .await
                .expect("bind second by key"),
            Some(meta_b.clone())
        );
        assert_eq!(
            load_voice_announcement_durable(&pool, message_b)
                .await
                .expect("load second message"),
            Some(meta_b)
        );
        assert!(
            load_voice_announcement_durable(&pool, message_a)
                .await
                .expect("first message remains unbound")
                .is_none()
        );
        assert_eq!(
            bind_pending_voice_announcement_by_key_durable(&pool, &key_a, channel_id, message_a)
                .await
                .expect("bind first by key"),
            Some(meta_a)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_pending_key_bind_is_channel_scoped() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_006);
        let wrong_channel_id = ChannelId::new(91_007);
        let message_id = MessageId::new(82_007);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91006:utt-1", "announce:generation:1");
        let content = "🎙️ \"채널 확인\"";
        let expected = announcement();

        assert!(
            persist_voice_announcement_reservation_durable(
                &pool,
                &pending_key,
                channel_id,
                content,
                &expected,
            )
            .await
            .expect("persist durable voice announcement reservation")
        );

        assert!(
            bind_pending_voice_announcement_by_key_durable(
                &pool,
                &pending_key,
                wrong_channel_id,
                message_id,
            )
            .await
            .expect("wrong-channel bind should not error")
            .is_none(),
            "copied/reflected ref in another channel must not bind the pending row"
        );
        assert_eq!(
            bind_pending_voice_announcement_by_key_durable(
                &pool,
                &pending_key,
                channel_id,
                message_id,
            )
            .await
            .expect("correct-channel bind"),
            Some(expected)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_pending_key_take_consumes_without_bind_race() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_008);
        let message_id = MessageId::new(82_008);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91008:utt-1", "announce:generation:1");
        let content = "🎙️ \"바로 처리해\"";
        let expected = announcement();

        assert!(
            persist_voice_announcement_reservation_durable(
                &pool,
                &pending_key,
                channel_id,
                content,
                &expected,
            )
            .await
            .expect("persist durable voice announcement reservation")
        );

        assert_eq!(
            take_pending_voice_announcement_by_key_durable(
                &pool,
                &pending_key,
                channel_id,
                message_id,
            )
            .await
            .expect("take pending by key"),
            Some(expected)
        );
        assert!(
            take_pending_voice_announcement_by_key_durable(
                &pool,
                &pending_key,
                channel_id,
                message_id,
            )
            .await
            .expect("second take pending by key")
            .is_none(),
            "pending-key consume must be one-shot"
        );
        assert!(
            !bind_voice_announcement_durable_message_id(
                &pool,
                &pending_key,
                MessageId::new(82_009),
            )
            .await
            .expect("late bind after pending consume"),
            "late bind must not resurrect a consumed pending row"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_consumed_row_verifies_accepted_replay() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_009);
        let message_id = MessageId::new(82_010);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91009:utt-1", "announce:generation:1");
        let content = "🎙️ \"대기열로 처리해\"";
        let expected = announcement();

        assert!(
            persist_voice_announcement_reservation_durable(
                &pool,
                &pending_key,
                channel_id,
                content,
                &expected,
            )
            .await
            .expect("persist durable voice announcement reservation")
        );
        assert!(
            bind_voice_announcement_durable_message_id(&pool, &pending_key, message_id)
                .await
                .expect("bind message id")
        );
        assert!(
            mark_voice_announcement_durable_consumed(&pool, message_id)
                .await
                .expect("mark accepted queued replay consumed")
        );
        assert!(
            load_voice_announcement_durable(&pool, message_id)
                .await
                .expect("live load after consume")
                .is_none(),
            "live durable lookup must still hide consumed rows"
        );
        assert_eq!(
            load_consumed_voice_announcement_durable(&pool, message_id)
                .await
                .expect("consumed load after queue accept"),
            Some(expected)
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_voice_announcement_persist_does_not_resurrect_consumed_row() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let channel_id = ChannelId::new(91_005);
        let message_id = MessageId::new(82_006);
        let pending_key =
            durable_voice_announcement_pending_key("voice:1:91005:utt-1", "announce:generation:1");
        let content = "🎙️ \"되살리지 마\"";
        let expected = announcement();

        assert!(
            persist_voice_announcement_reservation_durable(
                &pool,
                &pending_key,
                channel_id,
                content,
                &expected,
            )
            .await
            .expect("persist durable voice announcement reservation")
        );
        assert!(
            bind_voice_announcement_durable_message_id(&pool, &pending_key, message_id)
                .await
                .expect("bind message id")
        );
        assert_eq!(
            take_voice_announcement_durable(&pool, message_id)
                .await
                .expect("take once"),
            Some(expected.clone())
        );
        assert!(
            !persist_voice_announcement_reservation_durable(
                &pool,
                &pending_key,
                channel_id,
                content,
                &expected,
            )
            .await
            .expect("late persist after consume must not fail"),
            "late persist must report no live reservation was written"
        );
        assert!(
            take_voice_announcement_durable(&pool, message_id)
                .await
                .expect("take after late persist")
                .is_none(),
            "late persist must not clear consumed_at"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_handoff_round_trips_and_consumes_exactly_once() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_001);
        let expected = handoff_meta(700, 600, Some("project-agentdesk"));

        persist_handoff_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable handoff");

        let loaded = load_handoff_durable(&pool, message_id)
            .await
            .expect("load durable handoff")
            .expect("row visible before consumption");
        assert_eq!(loaded, expected);

        let taken = take_handoff_durable(&pool, message_id)
            .await
            .expect("take durable handoff")
            .expect("first take consumes the row");
        assert_eq!(taken, expected);

        assert!(
            load_handoff_durable(&pool, message_id)
                .await
                .expect("load after consume")
                .is_none(),
            "consumed row must not be visible to load"
        );
        assert!(
            take_handoff_durable(&pool, message_id)
                .await
                .expect("second take")
                .is_none(),
            "second take must report None — claim is one-shot"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_handoff_persist_does_not_resurrect_consumed_row() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_051);
        let expected = handoff_meta(710, 610, Some("project-agentdesk"));
        let replacement = handoff_meta(711, 611, Some("other-agent"));

        persist_handoff_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable handoff");
        take_handoff_durable(&pool, message_id)
            .await
            .expect("take durable handoff")
            .expect("row claimed");

        persist_handoff_durable(&pool, message_id, &replacement)
            .await
            .expect("late persist must not fail");

        assert!(
            take_handoff_durable(&pool, message_id)
                .await
                .expect("take after late persist")
                .is_none(),
            "late persist must not clear consumed_at and resurrect the row"
        );
        let stored_voice_channel_id: String = sqlx::query_scalar(
            "SELECT voice_channel_id
             FROM voice_background_handoff_meta
             WHERE message_id = $1",
        )
        .bind(message_id.get().to_string())
        .fetch_one(&pool)
        .await
        .expect("consumed row remains for GC");
        assert_eq!(
            stored_voice_channel_id,
            expected.voice_channel_id.to_string()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_pending_handoff_claim_before_bind_prevents_resurrection() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_061);
        let correlation_id = "voice-bg:11112222333344445555666677778888";
        let expected = handoff_meta(712, 612, Some("project-agentdesk"));

        persist_handoff_reservation_durable(&pool, correlation_id, &expected)
            .await
            .expect("persist pending durable handoff");

        let claimed = take_handoff_reservation_durable(&pool, correlation_id)
            .await
            .expect("claim pending durable handoff")
            .expect("pending reservation is claimable before bind");
        assert_eq!(claimed, expected);

        assert!(
            !bind_handoff_durable_message_id(&pool, correlation_id, message_id)
                .await
                .expect("late bind after claim"),
            "late bind must report that the pending row was already consumed"
        );
        assert!(
            load_handoff_durable(&pool, message_id)
                .await
                .expect("load actual message id after late bind")
                .is_none(),
            "late bind must not create an actual-message row after correlation claim"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two concurrent terminal-delivery callers race to consume the same
    /// durable handoff. Exactly one must win.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_handoff_concurrent_consumers_yield_exactly_one_claim() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_101);
        let expected = handoff_meta(701, 601, Some("project-agentdesk"));

        persist_handoff_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable handoff");

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let task_a =
            tokio::spawn(async move { take_handoff_durable(&pool_a, message_id).await.unwrap() });
        let task_b =
            tokio::spawn(async move { take_handoff_durable(&pool_b, message_id).await.unwrap() });
        let (result_a, result_b) =
            tokio::try_join!(task_a, task_b).expect("join concurrent consumers");
        let winners = [&result_a, &result_b]
            .iter()
            .filter(|r| r.is_some())
            .count();
        assert_eq!(winners, 1, "exactly one consumer must win the atomic claim");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rehydrate_copies_live_rows_into_in_memory_store() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_201);
        let expected = handoff_meta(702, 602, Some("project-agentdesk"));

        persist_handoff_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable handoff");

        let count = rehydrate_handoffs_from_pg(&pool)
            .await
            .expect("rehydrate succeeds");
        assert!(
            count >= 1,
            "rehydrate must include the persisted row (got {count})"
        );
        assert_eq!(global_store().get_handoff(message_id), Some(expected));

        // Drain the in-memory store entry to keep test isolation tight.
        let _ = global_store().take_handoff(message_id);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gc_removes_rows_older_than_ttl() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(81_301);
        let expected = handoff_meta(703, 603, None);

        persist_handoff_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable handoff");

        // Backdate created_at past the GC TTL so the GC sweep deletes it.
        sqlx::query(
            "UPDATE voice_background_handoff_meta
             SET created_at = NOW() - make_interval(secs => $1)
             WHERE message_id = $2",
        )
        .bind((DURABLE_HANDOFF_META_TTL_SECS + 60) as f64)
        .bind(message_id.get().to_string())
        .execute(&pool)
        .await
        .expect("backdate row for gc test");

        let deleted = gc_expired_voice_background_handoff_meta_pg(
            &pool,
            Duration::from_secs(DURABLE_HANDOFF_META_TTL_SECS as u64),
        )
        .await
        .expect("gc sweep");
        assert!(
            deleted >= 1,
            "gc must delete the backdated row (got {deleted})"
        );

        assert!(
            load_handoff_durable(&pool, message_id)
                .await
                .expect("load after gc")
                .is_none(),
            "post-gc load must observe no row"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
