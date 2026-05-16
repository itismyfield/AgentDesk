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
/// the background turn they trigger may run for minutes before the terminal
/// delivery callback consults the marker. Keep generously long so legitimate
/// long-running background turns still find the marker.
const HANDOFF_META_TTL: Duration = Duration::from_secs(60 * 60);

/// Durable handoff rows older than this are treated as expired and ignored
/// by the durable load/take helpers. The leader-only GC sweep
/// (`gc_expired_voice_background_handoff_meta_pg`) deletes them.
///
/// 1 hour is conservative: voice → background → terminal-delivery normally
/// completes within minutes, so anything older almost certainly represents
/// a turn that crashed or never reached terminal delivery. Mirrors the
/// in-memory `HANDOFF_META_TTL`.
pub(crate) const DURABLE_HANDOFF_META_TTL_SECS: i64 = 60 * 60;

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
        if let Ok(mut entries) = self.handoff_entries.write() {
            let now = Instant::now();
            prune_handoff_expired_locked(&mut entries, now);
            entries.insert(
                message_id.get(),
                StoredVoiceBackgroundHandoffMeta {
                    meta,
                    expires_at: now + HANDOFF_META_TTL,
                },
            );
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

/// Persist a voice-background handoff marker to the durable side store
/// (#2274). The process-local in-memory store remains the hot read path;
/// this PG row is the durable source of truth that survives a dcserver
/// restart partway through a long background turn.
///
/// `ON CONFLICT … DO UPDATE` resets `consumed_at` to NULL so retries from
/// a re-dispatched handoff path can reuse the same `message_id`.
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
        })
    })
    .transpose()
}

/// Boot-time rehydration — copy every live, unconsumed, within-TTL row
/// from the PG side store into the in-memory store so callers on the hot
/// path (synchronous `get_handoff` / `take_handoff`) keep working after a
/// dcserver restart without an async fallback ripple.
///
/// Best-effort: a PG error here is logged and ignored. Subsequent
/// dispatches will still write through and terminal-delivery callers fall
/// back to `take_handoff_durable` directly when the in-memory store
/// misses (see `voice_background_completion_target`).
///
/// Returns the count of rows rehydrated for observability.
pub(crate) async fn rehydrate_handoffs_from_pg(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT message_id, voice_channel_id, background_channel_id, agent_id
         FROM voice_background_handoff_meta
         WHERE consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $1)",
    )
    .bind(DURABLE_HANDOFF_META_TTL_SECS as f64)
    .fetch_all(pool)
    .await?;
    let store = global_store();
    let mut count: u64 = 0;
    for (message_id, voice_channel_id, background_channel_id, agent_id) in rows {
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
        store.insert_handoff(
            MessageId::new(message_id_u64),
            VoiceBackgroundHandoffMeta {
                voice_channel_id: voice_channel_id_u64,
                background_channel_id: background_channel_id_u64,
                agent_id,
            },
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
