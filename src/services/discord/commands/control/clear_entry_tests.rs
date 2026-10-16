//! `/clear` and `!clear` with a failing queue file, marker, boundary or stop still answer the
//! cleared notice once with one warning, and leave the channel cleared for every later reader.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serde_json::Value;
use serenity::{ChannelId, MessageId, UserId};

use super::super::SESSION_CLEARED_RESPONSE;
use super::clear_persist_failure_tests::{Trap, trap_boundary_writes};
use crate::services::discord::admin_host_guard::tests::Recorder;
use crate::services::discord::host_defer_gate::tests::{map_channel, postgres};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::test_support::fail_queue_saves;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, QueuePersistenceContext,
    registry_purge::MailboxPurgeOutcome,
};

const OWNER: u64 = 1;
const PROVIDER: ProviderKind = ProviderKind::Gemini;
const FAILED: &str = "⚠️ 일부 저장을 한 번 재시도했지만 완료하지 못했어요. 재시작 후 버린 메시지나 이전 세션이 복원될 수 있어요.";
const LANDED: &str = "⚠️ 일부 저장이 처음에 실패했지만 재시도로 완료했어요.";
const STOP: &str = "⚠️ 실행 중이던 턴의 종료를 확인하지 못했어요.";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry {
    Slash,
    Text,
}

/// One failure a clear meets, the warning it must answer with, and whether a restart may still
/// restore the dequeued input.
#[derive(Clone, Copy, Debug)]
enum Fault {
    Clean,
    CorruptMarker,
    MarkerRemoval { times: usize, after: bool },
    CorruptSnapshot,
    UnreadableSnapshot,
    ExternalMeta,
    Publish(usize),
    Boundary(Trap),
    StopRefused,
}

impl Fault {
    fn warning(self) -> Option<&'static str> {
        match self {
            Self::MarkerRemoval { times: 1, .. } | Self::Publish(1) => Some(LANDED),
            Self::Boundary(Trap::First) => Some(LANDED),
            Self::MarkerRemoval { .. } | Self::Publish(_) | Self::Boundary(_) => Some(FAILED),
            Self::UnreadableSnapshot => Some(FAILED),
            Self::StopRefused => Some(STOP),
            Self::Clean | Self::CorruptMarker | Self::CorruptSnapshot | Self::ExternalMeta => None,
        }
    }
}

const FAULTS: [Fault; 13] = [
    Fault::Clean,
    Fault::CorruptMarker,
    Fault::MarkerRemoval {
        times: 1,
        after: false,
    },
    Fault::MarkerRemoval {
        times: 1,
        after: true,
    },
    Fault::MarkerRemoval {
        times: 2,
        after: false,
    },
    Fault::CorruptSnapshot,
    Fault::UnreadableSnapshot,
    Fault::ExternalMeta,
    Fault::Publish(1),
    Fault::Publish(2),
    Fault::Boundary(Trap::First),
    Fault::Boundary(Trap::Every),
    Fault::StopRefused,
];

/// A Gemini runtime on PostgreSQL whose Discord REST calls land in a recorder.
struct Rig {
    api: Recorder,
    ctx: serenity::Context,
    data: Data,
    pool: sqlx::PgPool,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    gateway: tokio::task::JoinHandle<()>,
    _voice: tempfile::TempDir,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Rig {
    async fn start() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool) = postgres().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.settings.write().await.owner_user_id = Some(OWNER);
        shared.settings.write().await.provider = PROVIDER;
        let api = Recorder::start().await;
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let rest = format!("http://127.0.0.1:{}", api.port);
        let ctx = discord_mock::serenity_context(rest, gateway_url).await;
        let voice = tempfile::tempdir().unwrap();
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.audio.recordings_dir = voice.path().to_path_buf();
        let data = Data {
            shared,
            token: "clear-entry-test".into(),
            provider: PROVIDER,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
            ingress_shadow: None,
        };
        Self {
            api,
            ctx,
            data,
            pool,
            db,
            gateway,
            _voice: voice,
            _root: root,
        }
    }

    fn shared(&self) -> &Arc<SharedData> {
        &self.data.shared
    }

    fn persistence(&self) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&PROVIDER, &self.shared().token_hash, None)
    }

    fn file(&self, channel: ChannelId, extension: &str) -> std::path::PathBuf {
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let name = format!("{}.{extension}", channel.get());
        root.join(PROVIDER.as_str())
            .join(&self.shared().token_hash)
            .join(name)
    }

    async fn key(&self, channel: ChannelId) -> String {
        let key = crate::services::discord::adk_session::build_adk_session_key;
        key(self.shared(), channel, &PROVIDER, None)
            .await
            .expect("session key")
    }

    /// The provider session PostgreSQL would hand the next launch to resume.
    async fn stored_selector(&self, channel: ChannelId) -> Option<String> {
        let query = "SELECT claude_session_id FROM sessions WHERE session_key = $1";
        let query = sqlx::query_scalar::<_, Option<String>>(query).bind(self.key(channel).await);
        query.fetch_one(&self.pool).await.expect("stored selector")
    }

    /// A channel with a provider session, one input dequeued behind its marker, one queued
    /// behind it and an armed catch-up retry; `fault` is then set up.
    async fn channel(&self, n: u64, fault: Fault) -> (ChannelId, Option<Arc<CancelToken>>) {
        let channel = ChannelId::new(1_479_671_305_340_201_000 + 1_000 * n);
        let name = format!("adk-clear-entry-{n}-{}", uuid::Uuid::new_v4().simple());
        map_channel(self.shared(), channel, &name).await;
        let mut core = self.shared().core.lock().await;
        core.sessions.get_mut(&channel).unwrap().session_id = Some("sid".into());
        drop(core);
        let stored = "INSERT INTO sessions (session_key, provider, status, claude_session_id)
                      VALUES ($1, $2, 'idle', 'sid')";
        let stored = sqlx::query(stored)
            .bind(self.key(channel).await)
            .bind(PROVIDER.as_str());
        stored
            .execute(&self.pool)
            .await
            .expect("stored provider session");
        let mailbox = self.shared().mailbox(channel);
        for (offset, author) in [(1, 7), (2, 8)] {
            let input = input(MessageId::new(channel.get() + offset), author);
            assert!(mailbox.enqueue(input, self.persistence()).await.enqueued);
        }
        let mut token = None;
        if matches!(fault, Fault::StopRefused) {
            let active = Arc::new(CancelToken::new());
            // A tmux name whose host marker says Herdr: the stop's host is refused.
            let name = format!("adk-clear-entry-herdr-{n}");
            let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
            active.bind_unmanaged_session_name(&name);
            let owner = UserId::new(OWNER);
            let start = crate::services::discord::mailbox_try_start_turn;
            let id = MessageId::new(channel.get() + 4);
            assert!(start(self.shared(), channel, active.clone(), owner, id).await);
            crate::services::discord::increment_global_active(self.shared(), "other channel");
            crate::services::discord::increment_global_active(self.shared(), "cleared turn");
            token = Some(active);
        } else {
            let taken = mailbox.take_next_soft(self.persistence()).await;
            assert!(taken.intervention.is_some(), "the first input is dequeued");
        }
        let arm = crate::services::discord::catch_up::retry_state::arm_catch_up_retry_for_tests;
        arm(self.shared(), channel, channel.get());
        let (queue, marker) = (self.file(channel, "json"), self.file(channel, "dispatch"));
        let registry = ChannelMailboxRegistry::fail_clear_removals;
        match fault {
            Fault::CorruptMarker => std::fs::write(&marker, "{").unwrap(),
            Fault::MarkerRemoval { times, after } => registry(channel, true, times, after),
            Fault::CorruptSnapshot => std::fs::write(&queue, "{not json").unwrap(),
            Fault::UnreadableSnapshot => {
                std::fs::remove_file(&queue).unwrap();
                std::fs::create_dir(&queue).unwrap();
            }
            Fault::ExternalMeta => {
                let record = r#"{"version":1,"items":[],"terminal_identities":[{"primary":5,
                    "queued_generation":1,"sources":[{"message_id":5,"queued_generation":1}]}]}"#;
                std::fs::write(&queue, record).unwrap();
            }
            Fault::Publish(times) => fail_queue_saves(channel, times),
            Fault::Boundary(trap) => trap_boundary_writes(&self.pool, trap).await,
            Fault::Clean | Fault::StopRefused => {}
        }
        (channel, token)
    }

    /// Runs the clear through `entry` and returns every reply it posted.
    async fn clear(&self, entry: Entry, channel: ChannelId) -> Vec<String> {
        self.api.take();
        let author = UserId::new(OWNER);
        match entry {
            Entry::Slash => {
                let run = super::super::cancel_backstop_tests::invoke_real_slash_action;
                let command = super::cmd_clear();
                let data = &self.data;
                assert!(run(&self.ctx, data, channel, author, command, "clear").await);
            }
            Entry::Text => {
                let mut message = serenity::Message::default();
                message.id = MessageId::new(channel.get() + 3);
                message.channel_id = channel;
                message.author.id = author;
                message.author.name = "clear-entry".into();
                message.content = "!clear".into();
                let handle = super::super::text_commands::handle_text_command_with_uploads;
                let mut permit = None;
                let handled = handle(
                    &self.ctx,
                    &message,
                    &self.data,
                    channel,
                    "!clear",
                    &[],
                    &mut permit,
                );
                assert!(Box::pin(handled).await.expect("the text clear is handled"));
            }
        }
        let replies = self.api.take().into_iter().filter_map(|call| {
            let (_, body) = call.split_once(' ')?.1.split_once(' ')?;
            let body: Value = serde_json::from_str(body).ok()?;
            let content = body["content"]
                .as_str()
                .or(body["data"]["content"].as_str());
            content.map(str::to_owned)
        });
        replies.collect()
    }

    async fn finish(self) {
        self.gateway.abort();
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn input(id: MessageId, author: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: id,
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: vec![id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "queued before the clear".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn run_matrix(entry: Entry) {
    let rig = Rig::start().await;
    for (n, fault) in FAULTS.into_iter().enumerate() {
        let case = format!("{entry:?}/{fault:?}");
        let (channel, token) = rig.channel(n as u64, fault).await;

        let replies = rig.clear(entry, channel).await;

        let expected = match fault.warning() {
            Some(warning) => format!("{SESSION_CLEARED_RESPONSE}\n{warning}"),
            None => SESSION_CLEARED_RESPONSE.to_string(),
        };
        assert_eq!(replies, [expected], "{case}: one notice, one warning line");
        let shared = rig.shared();
        let live = shared.mailbox(channel).snapshot().await;
        assert!(live.intervention_queue.is_empty(), "{case}: queue");
        assert_eq!(live.pending_user_dispatch, None, "{case}: reservation");
        assert!(live.cancel_token.is_none(), "{case}: active anchor");
        assert!(
            !shared.catch_up_retry_pending.contains_key(&channel),
            "{case}: retry"
        );
        let checkpoint = shared.last_message_ids.get(&channel).map(|id| *id);
        assert!(
            checkpoint >= Some(channel.get() + 2),
            "{case}: {checkpoint:?}"
        );
        let core = shared.core.lock().await;
        let session = &core.sessions[&channel];
        assert_eq!(
            (session.session_id.as_deref(), session.cleared),
            (None, true),
            "{case}"
        );
        drop(core);
        assert_eq!(
            rig.stored_selector(channel).await,
            None,
            "{case}: nothing to resume"
        );
        if let Some(token) = token {
            assert!(
                token.cancelled.load(Ordering::SeqCst),
                "{case}: the turn is stopped"
            );
            assert_eq!(
                shared.restart.global_active.load(Ordering::SeqCst),
                1,
                "{case}"
            );
            shared.restart.global_active.store(0, Ordering::SeqCst);
        }
        let removed = shared.mailboxes.remove_idle_entry(channel).await;
        assert_eq!(removed, MailboxPurgeOutcome::Removed, "{case}");
        let successor = shared.mailbox(channel);
        let hydrated = successor
            .hydrate_pending_queue_from_disk(rig.persistence())
            .await;
        assert_eq!(
            hydrated.absorbed, 0,
            "{case}: the successor restores nothing"
        );
        let taken = successor.take_next_soft(rig.persistence()).await;
        assert!(taken.intervention.is_none(), "{case}: nor takes anything");
        if let Fault::ExternalMeta = fault {
            let kept: Value =
                serde_json::from_slice(&std::fs::read(rig.file(channel, "json")).unwrap()).unwrap();
            assert_eq!(
                kept["terminal_identities"][0]["primary"], 5,
                "{case}: submitted sources kept"
            );
        }
        // A queue path that is a directory refuses every write, a new input's too.
        if !matches!(fault, Fault::UnreadableSnapshot) {
            let fresh = input(MessageId::new(channel.get() + 5), 7);
            assert!(
                successor.enqueue(fresh, rig.persistence()).await.enqueued,
                "{case}: new input"
            );
            // A restart reads the files alone; only the discarded inputs count as revived.
            let restart = ChannelMailboxRegistry::default().handle(channel);
            restart
                .hydrate_pending_queue_from_disk(rig.persistence())
                .await;
            let restored = restart.snapshot().await.intervention_queue;
            let discarded = [channel.get() + 1, channel.get() + 2].map(MessageId::new);
            let revived = restored.iter().flat_map(|item| &item.source_message_ids);
            let revived = revived.into_iter().any(|id| discarded.contains(id));
            let allowed = fault.warning() == Some(FAILED);
            assert!(
                !revived || allowed,
                "{case}: only a twice-failed write lets a restart restore"
            );
        }
        if let Fault::Boundary(_) = fault {
            trap_boundary_writes(&rig.pool, Trap::Off).await;
        }
    }
    rig.finish().await;
}

#[tokio::test]
async fn slash_clear_commits_and_warns_once_whatever_fails_pg() {
    run_matrix(Entry::Slash).await;
}

#[tokio::test]
async fn text_clear_commits_and_warns_once_whatever_fails_pg() {
    run_matrix(Entry::Text).await;
}

/// Without a pool the clear still commits through both entries; the skipped boundary is warned.
#[tokio::test]
async fn a_clear_without_a_pool_commits_and_warns_through_both_entries_pg() {
    let rig = Rig::start().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    shared.settings.write().await.owner_user_id = Some(OWNER);
    shared.settings.write().await.provider = PROVIDER;
    let data = Data {
        shared,
        token: rig.data.token.clone(),
        provider: PROVIDER,
        voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&rig.data.voice_config),
        voice_config: rig.data.voice_config.clone(),
        ingress_shadow: None,
    };
    let rig = Rig { data, ..rig };
    for (n, entry) in [Entry::Slash, Entry::Text].into_iter().enumerate() {
        let (channel, _) = rig.channel(20 + n as u64, Fault::Clean).await;
        let replies = rig.clear(entry, channel).await;
        let expected = format!("{SESSION_CLEARED_RESPONSE}\n{FAILED}");
        assert_eq!(replies, [expected], "{entry:?}");
        assert!(
            rig.shared()
                .mailbox(channel)
                .snapshot()
                .await
                .intervention_queue
                .is_empty()
        );
    }
    rig.finish().await;
}
