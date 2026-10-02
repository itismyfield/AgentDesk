use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
    time::{Duration, Instant},
};

use poise::serenity_prelude::{ChannelId, MessageId};
use sqlx::PgPool;

use super::prompt::VoiceTranscriptAnnouncement;

const ANNOUNCEMENT_META_TTL: Duration = Duration::from_secs(30);
/// Durable announcement rows outlive the local TTL: intake may be queued to another
/// process or wait behind an active turn.
pub(crate) const DURABLE_ANNOUNCEMENT_META_TTL_SECS: i64 = 24 * 60 * 60;
/// Keep voice routing metadata available through long background turns.
const HANDOFF_META_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const DURABLE_HANDOFF_META_TTL_SECS: i64 = 24 * 60 * 60;
const DURABLE_HANDOFF_PENDING_PREFIX: &str = "pending:";

#[derive(Debug, Clone)]
struct StoredVoiceTranscriptAnnouncement {
    announcement: VoiceTranscriptAnnouncement,
    expires_at: Instant,
    accepted_replay: bool,
}

/// Typed marker stamped by `dispatch_voice_background_handoff`; terminal delivery
/// reads it to decide whether the spoken summary goes to the voice channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoiceBackgroundHandoffMeta {
    /// Voice channel that originated the handoff and receives the spoken summary.
    pub voice_channel_id: u64,
    /// Background text channel where the handoff prompt was posted.
    pub background_channel_id: u64,
    /// Agent from the active voice route; lets `voice_channel_for_background`
    /// disambiguate agents that share a background channel.
    pub agent_id: Option<String>,
    /// Legacy flag set by older dispatch code; terminal delivery still honours it.
    /// Always `false` for markers loaded from PG, which are the source of truth.
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

    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn contains(&self, message_id: MessageId) -> bool {
        let mut entries = match self.entries.write() {
            Ok(entries) => entries,
            Err(_) => return false,
        };
        let now = Instant::now();
        prune_expired_locked(&mut entries, now);
        entries.contains_key(&message_id.get())
    }

    #[cfg(test)]
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

    /// Insert with an explicit lifetime; `rehydrate_handoffs_from_pg` passes the row's
    /// remaining PG TTL so the local marker never outlives its durable row.
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

    /// Drop a local marker without returning it, e.g. once the authoritative PG claim
    /// has settled the outcome.
    pub(crate) fn forget_handoff(&self, message_id: MessageId) {
        if let Ok(mut entries) = self.handoff_entries.write() {
            entries.remove(&message_id.get());
        }
    }

    /// Test helper: set `local_only_fallback` on a stored marker. True iff it existed.
    #[cfg(test)]
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

    /// Non-consuming read, so intake-gate queue paths can carry the payload inside the
    /// queued `Intervention`: the 30s local TTL is shorter than typical queue dwell.
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

mod durable;
pub(crate) use durable::*;

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
            control_channel_id: None,
            stt_mode: None,
            stt_latency_ms: None,
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

        // The GC sweep keys on `expires_at`, so expire the row directly.
        sqlx::query(
            "UPDATE voice_background_handoff_meta
             SET expires_at = NOW() - INTERVAL '1 second'
             WHERE message_id = $1",
        )
        .bind(message_id.get().to_string())
        .execute(&pool)
        .await
        .expect("expire row for gc test");

        let deleted = gc_expired_voice_background_handoff_meta_pg(
            &pool,
            Duration::from_secs(DURABLE_HANDOFF_META_TTL_SECS as u64),
        )
        .await
        .expect("gc sweep");
        assert!(
            deleted >= 1,
            "gc must delete the expired row (got {deleted})"
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
