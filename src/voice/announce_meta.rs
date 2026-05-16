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
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    message_id = message_id.get(),
                    error = %error,
                    "failed to insert voice transcript announcement metadata; local store lock is poisoned"
                );
                return;
            }
        };
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

pub(crate) async fn persist_durable(
    pool: &PgPool,
    message_id: MessageId,
    announcement: &VoiceTranscriptAnnouncement,
) -> Result<(), sqlx::Error> {
    let announcement =
        serde_json::to_value(announcement).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    sqlx::query(
        "INSERT INTO voice_transcript_announce_meta (message_id, announcement)
         VALUES ($1, $2)
         ON CONFLICT (message_id) DO UPDATE
         SET announcement = EXCLUDED.announcement,
             consumed_at = NULL",
    )
    .bind(message_id.get().to_string())
    .bind(announcement)
    .execute(pool)
    .await?;
    Ok(())
}

/// Durable rows older than this are treated as expired and ignored by
/// `load_durable` / `peek_durable` / `take_durable`. The GC sweep
/// (`gc_expired_voice_announce_meta_pg`) deletes them on the leader.
///
/// 10 minutes is conservative: announce → intake hand-off normally
/// completes within seconds, so anything older almost certainly
/// represents a producer that wrote the row but never matched the
/// MESSAGE_CREATE event (#2209).
pub(crate) const DURABLE_ANNOUNCE_META_TTL_SECS: i64 = 600;

pub(crate) async fn load_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT announcement
         FROM voice_transcript_announce_meta
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCE_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

/// Non-destructive read used by intake before validation/barge-in
/// succeed. Pairs with [`consume_durable`] which is only invoked after
/// the announcement has been successfully handed off to the provider
/// — so a worker crash between peek and consume leaves the row intact
/// for the next intake attempt (#2209 review finding #2).
pub(crate) async fn peek_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    load_durable(pool, message_id).await
}

/// Atomic claim used to commit a voice-announce intake to dispatch.
///
/// Returns `Ok(Some(announcement))` exactly once per row — the winning
/// caller. Concurrent callers (e.g. two intake handlers that both
/// `peek_durable` the same row before either claims) receive
/// `Ok(None)` and MUST abort dispatch with a structured warn.
///
/// Implemented as `UPDATE … SET consumed_at = NOW() WHERE
/// consumed_at IS NULL RETURNING announcement`, which Postgres treats
/// as an atomic compare-and-swap on the row. Rows older than
/// `DURABLE_ANNOUNCE_META_TTL_SECS` are not claimable.
///
/// Note on crash semantics: the row is not deleted, only marked
/// `consumed_at`. The GC sweep deletes any row whose `created_at`
/// is older than TTL. If a worker crashes after a successful claim
/// but before dispatch, the turn is lost — that is the conservative
/// choice: dispatching a turn twice is worse than dropping it.
pub(crate) async fn consume_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "UPDATE voice_transcript_announce_meta
         SET consumed_at = NOW()
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)
         RETURNING announcement",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCE_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

/// Legacy destructive read retained for tests and any caller that
/// explicitly wants peek+consume to be a single atomic op. Production
/// intake now uses [`peek_durable`] + [`consume_durable`] so a worker
/// crash between the two leaves the row recoverable (#2209 finding #2).
pub(crate) async fn take_durable(
    pool: &PgPool,
    message_id: MessageId,
) -> Result<Option<VoiceTranscriptAnnouncement>, sqlx::Error> {
    let value: Option<serde_json::Value> = sqlx::query_scalar(
        "DELETE FROM voice_transcript_announce_meta
         WHERE message_id = $1
           AND consumed_at IS NULL
           AND created_at > NOW() - make_interval(secs => $2)
         RETURNING announcement",
    )
    .bind(message_id.get().to_string())
    .bind(DURABLE_ANNOUNCE_META_TTL_SECS as f64)
    .fetch_optional(pool)
    .await?;
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| sqlx::Error::Decode(Box::new(error)))
}

/// Delete durable rows older than `ttl`. Intended to be wired into
/// the leader-only maintenance scheduler so cleanup runs without a
/// new background worker. See `src/server/maintenance.rs`.
pub(crate) async fn gc_expired_voice_announce_meta_pg(
    pool: &PgPool,
    ttl: std::time::Duration,
) -> Result<u64, sqlx::Error> {
    let ttl_secs = ttl.as_secs_f64();
    let result = sqlx::query(
        "DELETE FROM voice_transcript_announce_meta
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
    use crate::db::auto_queue::test_support::TestPostgresDb;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_store_reconstructs_and_consumes_announcement() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(12_345);
        let expected = announcement();

        persist_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable metadata");

        let loaded = load_durable(&pool, message_id)
            .await
            .expect("load durable metadata")
            .expect("metadata present before consumption");
        assert_eq!(loaded, expected);

        let taken = take_durable(&pool, message_id)
            .await
            .expect("take durable metadata")
            .expect("metadata consumed exactly once");
        assert_eq!(taken, expected);
        assert!(
            load_durable(&pool, message_id)
                .await
                .expect("load after consumption")
                .is_none()
        );
        assert!(
            take_durable(&pool, message_id)
                .await
                .expect("second take")
                .is_none()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Simulates the worker-crash window: an intake `peek_durable`
    /// observes the row, then the worker panics before reaching
    /// `consume_durable`. The row must survive so a retry can recover.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn peek_then_panic_before_consume_leaves_row_recoverable() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(98_001);
        let expected = announcement();

        persist_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable metadata");

        // Peek emulates the intake handler reading the row before
        // validation succeeds.
        let peeked = peek_durable(&pool, message_id)
            .await
            .expect("peek durable metadata")
            .expect("metadata visible after persist");
        assert_eq!(peeked, expected);

        // Simulate a worker panic / handler return before consume.
        // (no consume_durable call)

        // A follow-up intake attempt must still see the row.
        let peeked_again = peek_durable(&pool, message_id)
            .await
            .expect("peek after panic")
            .expect("row should survive a peek-without-consume");
        assert_eq!(peeked_again, expected);

        // After a successful retry, consume returns the announcement
        // exactly once.
        let claimed = consume_durable(&pool, message_id)
            .await
            .expect("consume durable metadata")
            .expect("claim should succeed");
        assert_eq!(claimed, expected);
        assert!(
            peek_durable(&pool, message_id)
                .await
                .expect("peek after consume")
                .is_none(),
            "consumed row must not be visible to subsequent peeks"
        );
        assert!(
            consume_durable(&pool, message_id)
                .await
                .expect("second consume")
                .is_none(),
            "second consume must report no row — claim is one-shot"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    /// Two concurrent intakes peek the same row, then both race to
    /// claim. Exactly one must win (Some(announcement)); the other
    /// must observe `None` and abort dispatch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_consumers_yield_exactly_one_claim() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let message_id = MessageId::new(98_301);
        let expected = announcement();

        persist_durable(&pool, message_id, &expected)
            .await
            .expect("persist durable metadata");

        let pool_a = pool.clone();
        let pool_b = pool.clone();
        let task_a = tokio::spawn(async move {
            let _peek = peek_durable(&pool_a, message_id).await.unwrap();
            consume_durable(&pool_a, message_id).await.unwrap()
        });
        let task_b = tokio::spawn(async move {
            let _peek = peek_durable(&pool_b, message_id).await.unwrap();
            consume_durable(&pool_b, message_id).await.unwrap()
        });
        let (result_a, result_b) =
            tokio::try_join!(task_a, task_b).expect("join concurrent consumers");
        let winners = [&result_a, &result_b]
            .iter()
            .filter(|r| r.is_some())
            .count();
        assert_eq!(
            winners, 1,
            "exactly one consumer must win the atomic claim"
        );
        let winner = result_a.as_ref().or(result_b.as_ref()).unwrap();
        assert_eq!(winner, &expected);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gc_expired_voice_announce_meta_deletes_rows_older_than_ttl() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let fresh_id = MessageId::new(98_101);
        let stale_id = MessageId::new(98_102);

        persist_durable(&pool, fresh_id, &announcement())
            .await
            .expect("persist fresh metadata");
        persist_durable(&pool, stale_id, &announcement())
            .await
            .expect("persist stale metadata");

        // Backdate the stale row beyond the GC horizon.
        sqlx::query(
            "UPDATE voice_transcript_announce_meta
             SET created_at = NOW() - INTERVAL '1 hour'
             WHERE message_id = $1",
        )
        .bind(stale_id.get().to_string())
        .execute(&pool)
        .await
        .expect("backdate stale row");

        let deleted = gc_expired_voice_announce_meta_pg(&pool, std::time::Duration::from_secs(600))
            .await
            .expect("gc expired rows");
        assert_eq!(deleted, 1, "exactly the stale row should be deleted");

        assert!(
            peek_durable(&pool, fresh_id)
                .await
                .expect("peek fresh")
                .is_some(),
            "fresh row should survive GC"
        );
        assert!(
            peek_durable(&pool, stale_id)
                .await
                .expect("peek stale")
                .is_none(),
            "stale row should be gone after GC"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ttl_filter_hides_rows_older_than_ttl_from_peek_and_take() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let stale_id = MessageId::new(98_201);

        persist_durable(&pool, stale_id, &announcement())
            .await
            .expect("persist stale metadata");

        // Backdate beyond the runtime TTL so peek/take should ignore it.
        sqlx::query(
            "UPDATE voice_transcript_announce_meta
             SET created_at = NOW() - make_interval(secs => $1)
             WHERE message_id = $2",
        )
        .bind((DURABLE_ANNOUNCE_META_TTL_SECS + 60) as f64)
        .bind(stale_id.get().to_string())
        .execute(&pool)
        .await
        .expect("backdate row past TTL");

        assert!(
            load_durable(&pool, stale_id)
                .await
                .expect("load past TTL")
                .is_none(),
            "load_durable must ignore rows older than TTL"
        );
        assert!(
            peek_durable(&pool, stale_id)
                .await
                .expect("peek past TTL")
                .is_none(),
            "peek_durable must ignore rows older than TTL"
        );
        assert!(
            take_durable(&pool, stale_id)
                .await
                .expect("take past TTL")
                .is_none(),
            "take_durable must ignore rows older than TTL"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
