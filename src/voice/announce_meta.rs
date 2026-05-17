use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use poise::serenity_prelude::MessageId;
use sqlx::PgPool;

use super::prompt::VoiceTranscriptAnnouncement;

const ANNOUNCEMENT_META_TTL: Duration = Duration::from_secs(30);
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

#[derive(Debug, Clone)]
struct StoredVoiceTranscriptAnnouncement {
    announcement: VoiceTranscriptAnnouncement,
    expires_at: Instant,
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

/// In-memory "pending" reservation for the persist-before-publish flow
/// (#2392). Keyed by `correlation_id` (deterministic from
/// `(guild_id, voice_channel_id, utterance_id, generation)`), it carries
/// the meta payload until `bind_pending_to_message_id` promotes it into
/// the committed `handoff_entries` map after publish returns. The
/// `expires_at` matches `HANDOFF_META_TTL` so a reservation that never
/// gets bound (publish error, dispatcher crash) eventually disappears.
#[derive(Debug, Clone)]
struct PendingHandoffReservation {
    meta: VoiceBackgroundHandoffMeta,
    expires_at: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct VoiceAnnouncementMetaStore {
    entries: RwLock<HashMap<u64, StoredVoiceTranscriptAnnouncement>>,
    handoff_entries: RwLock<HashMap<u64, StoredVoiceBackgroundHandoffMeta>>,
    /// #2392 — reservations made BEFORE publish so the durable record
    /// exists before any caller can observe the announce-bot MESSAGE_CREATE
    /// webhook. Promoted to `handoff_entries` by `bind_pending_to_message_id`
    /// or removed by `cancel_pending_reservation` on publish failure.
    pending_handoff_entries: RwLock<HashMap<String, PendingHandoffReservation>>,
}

impl VoiceAnnouncementMetaStore {
    pub(crate) fn insert(&self, message_id: MessageId, announcement: VoiceTranscriptAnnouncement) {
        if let Ok(mut entries) = self.entries.write() {
            let now = Instant::now();
            prune_expired_locked(&mut entries, now);
            entries.insert(
                message_id.get(),
                StoredVoiceTranscriptAnnouncement {
                    announcement,
                    expires_at: now + ANNOUNCEMENT_META_TTL,
                },
            );
        }
    }

    pub(crate) fn take(&self, message_id: MessageId) -> Option<VoiceTranscriptAnnouncement> {
        let mut entries = self.entries.write().ok()?;
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries
            .remove(&message_id.get())
            .map(|stored| stored.announcement)
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

    /// #2392 — reserve a pending handoff record keyed by `correlation_id`
    /// BEFORE publish. Returns `Err(())` when a reservation already exists
    /// for `correlation_id` — silent overwrite was flagged by Codex
    /// review against PR #2446 (HIGH-3). Successful reservation must be
    /// followed by either `bind_pending_to_message_id` (publish succeeded)
    /// or `cancel_pending_reservation` (publish failed). A leaked
    /// reservation eventually evaporates via the same TTL as a committed
    /// entry.
    pub(crate) fn reserve_pending_handoff(
        &self,
        correlation_id: &str,
        meta: VoiceBackgroundHandoffMeta,
    ) -> Result<(), ()> {
        let mut entries = self.pending_handoff_entries.write().map_err(|_| ())?;
        let now = Instant::now();
        prune_pending_expired_locked(&mut entries, now);
        if entries.contains_key(correlation_id) {
            return Err(());
        }
        entries.insert(
            correlation_id.to_string(),
            PendingHandoffReservation {
                meta,
                expires_at: now + HANDOFF_META_TTL,
            },
        );
        Ok(())
    }

    /// #2392 — promote a pending reservation into the committed
    /// `message_id`-keyed marker map. Returns the meta payload if a
    /// pending reservation existed; otherwise returns `None`. Callers
    /// MUST treat `None` as a programmer error (or a TTL expiry between
    /// reserve and bind) and emit a warning.
    ///
    /// This method takes the pending lock and the committed lock in a
    /// deterministic order (pending → committed) inside a single call so
    /// that `get_handoff` / `take_handoff`, which only look at the
    /// committed map, observe the marker atomically once the publish has
    /// returned and the dispatcher is in the bind step. The pending map
    /// is intentionally invisible to those readers — terminal-delivery
    /// readers look up by `message_id`, which only exists after publish.
    pub(crate) fn bind_pending_to_message_id(
        &self,
        correlation_id: &str,
        message_id: MessageId,
    ) -> Option<VoiceBackgroundHandoffMeta> {
        let mut pending = self.pending_handoff_entries.write().ok()?;
        let now = Instant::now();
        prune_pending_expired_locked(&mut pending, now);
        let reservation = pending.remove(correlation_id)?;
        let remaining = reservation
            .expires_at
            .checked_duration_since(now)
            .unwrap_or(Duration::from_secs(1));
        drop(pending);

        let mut entries = self.handoff_entries.write().ok()?;
        prune_handoff_expired_locked(&mut entries, now);
        entries.insert(
            message_id.get(),
            StoredVoiceBackgroundHandoffMeta {
                meta: reservation.meta.clone(),
                expires_at: now + remaining,
            },
        );
        Some(reservation.meta)
    }

    /// #2392 — drop a pending reservation that will never be bound
    /// (publish error before message_id is available, or dispatcher
    /// chose to abort). Idempotent.
    pub(crate) fn cancel_pending_reservation(&self, correlation_id: &str) {
        if let Ok(mut pending) = self.pending_handoff_entries.write() {
            pending.remove(correlation_id);
        }
    }

    /// #2392 test/observability helper — true iff a pending reservation
    /// exists for `correlation_id`. Used by the unit tests that verify
    /// the lifecycle (reserve → bind | cancel) leaves no leak.
    #[cfg(test)]
    pub(crate) fn pending_contains(&self, correlation_id: &str) -> bool {
        self.pending_handoff_entries
            .read()
            .map(|guard| guard.contains_key(correlation_id))
            .unwrap_or(false)
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

    /// #2266: non-consuming clone of the stored announcement so the intake-gate
    /// busy-channel paths can embed the payload in the queued `Intervention`
    /// WITHOUT draining the store. The active dispatch path still calls
    /// `take()` to consume the entry once the queued turn finally runs and
    /// reinserts the payload — but for the intake-time queue paths the
    /// metadata must travel inside the Intervention because the in-memory
    /// store TTL (30s) is shorter than typical queue dwell times.
    pub(crate) fn peek_clone(&self, message_id: MessageId) -> Option<VoiceTranscriptAnnouncement> {
        let mut entries = self.entries.write().ok()?;
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

fn prune_pending_expired_locked(
    entries: &mut HashMap<String, PendingHandoffReservation>,
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

/// #2392 — insert a pending handoff row to the durable PG store keyed by
/// `correlation_id` BEFORE publish. `message_id` is left NULL until
/// `bind_handoff_message_id_durable` runs after publish returns. The
/// `voice_background_handoff_meta_correlation_id_unique` partial unique
/// index rejects double reservations at the schema level (Codex HIGH-3
/// against PR #2446).
///
/// Returns Ok on a successful insert. Returns Err on a duplicate
/// correlation_id (caller must NOT proceed with publish) or on a PG
/// transport error (caller must NOT proceed with publish — fail-closed,
/// #2355 HIGH-2).
pub(crate) async fn reserve_handoff_durable(
    pool: &PgPool,
    correlation_id: &str,
    meta: &VoiceBackgroundHandoffMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO voice_background_handoff_meta (
             correlation_id, message_id,
             voice_channel_id, background_channel_id, agent_id
         ) VALUES ($1, NULL, $2, $3, $4)",
    )
    .bind(correlation_id)
    .bind(meta.voice_channel_id.to_string())
    .bind(meta.background_channel_id.to_string())
    .bind(meta.agent_id.as_ref())
    .execute(pool)
    .await?;
    Ok(())
}

/// #2392 — promote a pending reservation in PG into a committed
/// message_id-keyed marker after publish returns. The UPDATE is keyed by
/// `correlation_id` and explicitly guards `message_id IS NULL` so a
/// retry after a transient transport blip cannot accidentally rebind to
/// a different message_id (which would orphan the original publish in
/// Discord without a marker).
///
/// Returns Ok(()) on a successful bind. Returns Ok with no-op semantics
/// (zero rows affected) if the pending row was already consumed or never
/// existed — the caller logs and abandons routing. Returns Err on PG
/// transport error so the caller can decide between local-only fallback
/// (#2355 design) and dropping the handoff.
pub(crate) async fn bind_handoff_message_id_durable(
    pool: &PgPool,
    correlation_id: &str,
    message_id: MessageId,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE voice_background_handoff_meta
         SET message_id = $2, updated_at = NOW()
         WHERE correlation_id = $1 AND message_id IS NULL",
    )
    .bind(correlation_id)
    .bind(message_id.get().to_string())
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// #2392 — delete a pending PG reservation when publish failed. Keyed by
/// (`correlation_id`, `message_id IS NULL`) so a row that has already
/// been bound cannot be accidentally erased by a late cleanup attempt.
pub(crate) async fn cancel_pending_handoff_durable(
    pool: &PgPool,
    correlation_id: &str,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "DELETE FROM voice_background_handoff_meta
         WHERE correlation_id = $1 AND message_id IS NULL",
    )
    .bind(correlation_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Persist a voice-background handoff marker to the durable side store
/// (#2274). The process-local in-memory store remains the hot read path;
/// this PG row is the durable source of truth that survives a dcserver
/// restart partway through a long background turn.
///
/// `ON CONFLICT … DO UPDATE` resets `consumed_at` to NULL so retries from
/// a re-dispatched handoff path can reuse the same `message_id`.
///
/// #2392: this is the legacy direct-insert variant. New voice dispatch
/// sites MUST use the 3-phase `reserve_handoff_durable` → publish →
/// `bind_handoff_message_id_durable` flow; the voice dispatcher refuses
/// dispatch entirely when guild_id or PG pool is missing (no race-prone
/// fallback survives). This entry point remains in use by the turn
/// bridge for inbound markers (#2236 reverse-binding tests) and by
/// existing test scaffolding.
pub(crate) async fn persist_handoff_durable(
    pool: &PgPool,
    message_id: MessageId,
    meta: &VoiceBackgroundHandoffMeta,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO voice_background_handoff_meta (
             message_id, voice_channel_id, background_channel_id, agent_id
         ) VALUES ($1, $2, $3, $4)
         ON CONFLICT (message_id) WHERE message_id IS NOT NULL DO UPDATE
         SET voice_channel_id = EXCLUDED.voice_channel_id,
             background_channel_id = EXCLUDED.background_channel_id,
             agent_id = EXCLUDED.agent_id,
             consumed_at = NULL",
    )
    .bind(message_id.get().to_string())
    .bind(meta.voice_channel_id.to_string())
    .bind(meta.background_channel_id.to_string())
    .bind(meta.agent_id.as_ref())
    .execute(pool)
    .await?;
    Ok(())
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

    #[test]
    fn reserve_then_bind_promotes_to_committed() {
        let store = VoiceAnnouncementMetaStore::default();
        let correlation_id = "voice:1:2:utt-abc";
        let meta = handoff_meta(900, 800, Some("agent"));

        store
            .reserve_pending_handoff(correlation_id, meta.clone())
            .expect("first reservation succeeds");
        assert!(
            store.pending_contains(correlation_id),
            "pending entry must exist after reserve"
        );
        // get_handoff/take_handoff intentionally do NOT see pending entries.
        assert!(
            store.get_handoff(MessageId::new(7)).is_none(),
            "committed map must not surface pending entries — readers key on message_id"
        );

        let bound = store
            .bind_pending_to_message_id(correlation_id, MessageId::new(7))
            .expect("bind promotes pending → committed");
        assert_eq!(bound, meta);
        assert!(
            !store.pending_contains(correlation_id),
            "pending entry must be removed after bind"
        );
        assert_eq!(
            store.get_handoff(MessageId::new(7)),
            Some(meta),
            "committed entry must be visible to terminal-delivery readers"
        );
    }

    #[test]
    fn reserve_rejects_duplicate_correlation_id() {
        let store = VoiceAnnouncementMetaStore::default();
        let correlation_id = "voice:1:2:utt-abc";
        store
            .reserve_pending_handoff(correlation_id, handoff_meta(1, 2, None))
            .expect("first reservation succeeds");
        // #2392 — Codex HIGH-3 against PR #2446: silent overwrite of a
        // pending reservation must be rejected so the second caller
        // does not blow away the first dispatcher's in-flight state.
        assert!(
            store
                .reserve_pending_handoff(correlation_id, handoff_meta(3, 4, None))
                .is_err(),
            "duplicate reservation must fail-closed"
        );
    }

    #[test]
    fn cancel_pending_reservation_clears_entry() {
        let store = VoiceAnnouncementMetaStore::default();
        let correlation_id = "voice:1:2:utt-abc";
        store
            .reserve_pending_handoff(correlation_id, handoff_meta(1, 2, None))
            .expect("reserve");
        store.cancel_pending_reservation(correlation_id);
        assert!(
            !store.pending_contains(correlation_id),
            "cancel_pending_reservation must clear the entry so dispatch can retry"
        );
        // A subsequent reservation now succeeds because the slot is empty.
        store
            .reserve_pending_handoff(correlation_id, handoff_meta(3, 4, None))
            .expect("post-cancel re-reserve succeeds");
    }

    #[test]
    fn bind_with_no_reservation_returns_none() {
        let store = VoiceAnnouncementMetaStore::default();
        assert!(
            store
                .bind_pending_to_message_id("voice:0:0:none", MessageId::new(123))
                .is_none(),
            "bind without a prior reservation must report None — callers log and abandon"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reserve_pending_durable_rejects_duplicate_correlation_id() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let correlation_id = "voice:1000:2000:dup-test";
        let meta = handoff_meta(1234, 5678, Some("agent"));

        reserve_handoff_durable(&pool, correlation_id, &meta)
            .await
            .expect("first reservation");
        let second = reserve_handoff_durable(&pool, correlation_id, &meta).await;
        assert!(
            second.is_err(),
            "second reservation with the same correlation_id must fail — Codex HIGH-3 vs PR #2446"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_durable_promotes_pending_to_message_id() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let correlation_id = "voice:1000:2000:bind-test";
        let meta = handoff_meta(1234, 5678, Some("agent"));

        reserve_handoff_durable(&pool, correlation_id, &meta)
            .await
            .expect("reserve");

        // Pending row must NOT be visible to a take_handoff_durable
        // call keyed by message_id — the partial filter `message_id = $1`
        // naturally excludes the NULL row, but assert explicitly so a
        // future schema change cannot regress this.
        let pre_bind = take_handoff_durable(&pool, MessageId::new(99_999))
            .await
            .expect("take pre-bind");
        assert!(
            pre_bind.is_none(),
            "pending row must not be consumable before bind"
        );

        let rows = bind_handoff_message_id_durable(&pool, correlation_id, MessageId::new(99_999))
            .await
            .expect("bind durable");
        assert_eq!(rows, 1, "bind must affect exactly one row");

        let claimed = take_handoff_durable(&pool, MessageId::new(99_999))
            .await
            .expect("take post-bind")
            .expect("bound row must be claimable");
        assert_eq!(claimed, meta);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_pending_durable_removes_only_unbound_rows() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let unbound = "voice:1000:2000:cancel-unbound";
        let bound = "voice:1000:2000:cancel-bound";
        let meta = handoff_meta(11, 22, None);

        reserve_handoff_durable(&pool, unbound, &meta)
            .await
            .expect("reserve unbound");
        reserve_handoff_durable(&pool, bound, &meta)
            .await
            .expect("reserve bound");
        bind_handoff_message_id_durable(&pool, bound, MessageId::new(42))
            .await
            .expect("bind bound");

        let deleted = cancel_pending_handoff_durable(&pool, unbound)
            .await
            .expect("cancel unbound");
        assert_eq!(deleted, 1, "unbound row must be cancellable");

        // Bound row must survive — cancel guards on `message_id IS NULL`.
        let deleted_bound = cancel_pending_handoff_durable(&pool, bound)
            .await
            .expect("cancel bound");
        assert_eq!(
            deleted_bound, 0,
            "bound rows must NOT be deletable via the pending-cancel path"
        );
        let still_there = take_handoff_durable(&pool, MessageId::new(42))
            .await
            .expect("take bound");
        assert!(
            still_there.is_some(),
            "bound row must remain claimable after a misdirected pending-cancel"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// #2392 — race regression: concurrently spawn two dispatchers that
    /// reserve different correlation_ids and verify both reservations
    /// land cleanly with no cross-contamination. Also asserts that two
    /// tasks racing on the SAME correlation_id resolve to exactly one
    /// winning reservation (Codex HIGH-3 was an interleaving check).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_with_distinct_correlation_ids_both_succeed() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let meta = handoff_meta(11, 22, None);

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let meta_a = meta.clone();
        let meta_b = meta.clone();
        let task_a = tokio::spawn(async move {
            reserve_handoff_durable(&pool_a, "voice:1:2:concurrent-a", &meta_a).await
        });
        let task_b = tokio::spawn(async move {
            reserve_handoff_durable(&pool_b, "voice:1:2:concurrent-b", &meta_b).await
        });
        let (res_a, res_b) = tokio::try_join!(task_a, task_b).expect("join");
        assert!(res_a.is_ok() && res_b.is_ok(), "distinct ids must both win");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_reservations_with_same_correlation_id_collide_to_exactly_one() {
        let pg_db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let meta = handoff_meta(11, 22, None);
        let cid = "voice:1:2:race";

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let meta_a = meta.clone();
        let meta_b = meta.clone();
        let task_a =
            tokio::spawn(async move { reserve_handoff_durable(&pool_a, cid, &meta_a).await });
        let task_b =
            tokio::spawn(async move { reserve_handoff_durable(&pool_b, cid, &meta_b).await });
        let (res_a, res_b) = tokio::try_join!(task_a, task_b).expect("join");
        let winners = [&res_a, &res_b].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            winners, 1,
            "exactly one of two concurrent reservations on the same correlation_id must win"
        );

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
