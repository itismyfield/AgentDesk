//! Text `!clear` through the real dispatcher into a recording Discord: the clear runs whatever
//! host the channel's session has, and answers its notice once, with a warning line at most.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use serde_json::Value;
use serenity::{ChannelId, MessageId, UserId};

use crate::db::session_transcripts::{ChannelClearFence, observe_channel_clear_fence_once};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::admin_host_guard::tests::{Recorder, process};
use crate::services::discord::commands::SESSION_CLEARED_RESPONSE;
use crate::services::discord::host_defer_gate::tests::{Case, ScriptedTmux, map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, Error, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};

const NODE: &str = "mac-mini";

/// A Claude runtime on PostgreSQL whose Discord REST calls land in a recorder, over a scripted tmux.
struct Rig {
    api: Recorder,
    ctx: serenity::Context,
    data: Data,
    shared: Arc<SharedData>,
    pool: sqlx::PgPool,
    db: crate::db::auto_queue::test_support::TestPostgresDb,
    tmux: ScriptedTmux,
    gateway: tokio::task::JoinHandle<()>,
    _voice: tempfile::TempDir,
    _root: crate::config::TestRuntimeRootGuard,
}

/// One channel of the rig: its session, its queued input, its process and its transcript fence.
struct Channel {
    id: ChannelId,
    tmux_name: String,
    alive: Arc<AtomicBool>,
    fence: ChannelClearFence,
}

impl Rig {
    async fn start() -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let tmux = ScriptedTmux::install();
        let (db, pool) = postgres().await;
        let shared = shared_on(&pool).await;
        let api = Recorder::start().await;
        let (_, gateway_url, gateway) =
            discord_mock::start(discord_mock::DiscordMockState::new()).await;
        let rest = format!("http://127.0.0.1:{}", api.port);
        let ctx = discord_mock::serenity_context(rest, gateway_url).await;
        let voice = tempfile::tempdir().unwrap();
        let mut voice_config = crate::voice::VoiceConfig::default();
        voice_config.audio.recordings_dir = voice.path().to_path_buf();
        voice_config.keep_recordings = true;
        let data = Data {
            shared: shared.clone(),
            token: "text-clear-test".into(),
            provider: ProviderKind::Claude,
            voice_receiver: crate::voice::VoiceReceiver::from_voice_config(&voice_config),
            voice_config,
        };
        Self {
            api,
            ctx,
            data,
            shared,
            pool,
            db,
            tmux,
            gateway,
            _voice: voice,
            _root: root,
        }
    }

    /// Maps `id` to a session with a provider selector, one queued input, a live process and the
    /// `stored` sessions row.
    async fn channel(&self, id: u64, stored: Stored) -> Channel {
        let id = ChannelId::new(id);
        let name = format!("adk-text-clear-{}", uuid::Uuid::new_v4().simple());
        let tmux_name = ProviderKind::Claude.build_tmux_session_name(&name);
        let key = channel_key(&self.shared, &tmux_name);
        map_channel(&self.shared, id, &name).await;
        let mut core = self.shared.core.lock().await;
        core.sessions.get_mut(&id).unwrap().session_id = Some("sid".into());
        drop(core);
        Case::Stored(stored)
            .seed(&self.pool, &key, &tmux_name, id.get())
            .await;
        let queued = MessageId::new(id.get() + 1);
        let persistence =
            QueuePersistenceContext::new(&ProviderKind::Claude, &self.shared.token_hash, None);
        let mailbox = self.shared.mailbox(id);
        let enqueued = mailbox.enqueue(input(queued), persistence).await;
        assert!(enqueued.enqueued, "the fixture input is queued");
        let alive = process(&tmux_name, 64_900);
        self.tmux.take_calls();
        let fence = self.clear_fence(id).await;
        Channel {
            id,
            tmux_name,
            alive,
            fence,
        }
    }

    /// The channel's transcript clear generation as PostgreSQL holds it now.
    async fn clear_fence(&self, channel: ChannelId) -> ChannelClearFence {
        let slot = tokio::sync::OnceCell::new();
        let channel = channel.get().to_string();
        observe_channel_clear_fence_once(&slot, Some(&self.pool), &channel).await;
        slot.into_inner().expect("an observed fence")
    }

    /// Sends `!clear` on `channel` through the dispatcher: its result and the replies it posted,
    /// each as `(replied-to message, content)`. Any other Discord request fails the test.
    async fn text_clear(&self, channel: &Channel) -> (Result<bool, Error>, Vec<(u64, String)>) {
        let mut message = serenity::Message::default();
        message.id = MessageId::new(channel.id.get() + 2);
        message.channel_id = channel.id;
        message.author.id = UserId::new(1);
        message.author.name = "clear-test".into();
        message.content = "!clear".into();
        self.api.take();
        let handled = Box::pin(super::handle_text_command(
            &self.ctx, &message, &self.data, channel.id, "!clear",
        ))
        .await;
        let posted = format!("POST /api/v10/channels/{}/messages ", channel.id.get());
        let replies = self.api.take().into_iter().map(|call| {
            let body = call.strip_prefix(&posted);
            let body = body.unwrap_or_else(|| panic!("an unexpected Discord request: {call}"));
            let body: Value = serde_json::from_str(body).expect("a JSON message body");
            let target = body["message_reference"]["message_id"].as_str();
            let target = target.and_then(|id| id.parse::<u64>().ok());
            assert_eq!(target, Some(message.id.get()), "a reply to the command");
            (
                message.id.get(),
                body["content"].as_str().unwrap().to_owned(),
            )
        });
        (handled, replies.collect())
    }

    async fn finish(self, channels: &[&Channel]) {
        for channel in channels {
            crate::services::session_backend::remove_process_session(&channel.tmux_name);
        }
        self.gateway.abort();
        self.pool.close().await;
        self.db.drop().await;
    }
}

fn input(id: MessageId) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
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

/// The fixture's one queued input.
const WAITING: &str = "대기 메시지 1건을 비웠어요.";

/// The clear ran: one reply, the notice with `warning` under it, and the session, its queue and
/// its transcript boundary are reset.
async fn assert_cleared(
    rig: &Rig,
    channel: &Channel,
    (handled, replies): (Result<bool, Error>, Vec<(u64, String)>),
    warning: Option<&str>,
) {
    assert!(handled.expect("the clear ran"), "the command is consumed");
    let notice = format!("{SESSION_CLEARED_RESPONSE}\n{WAITING}");
    let notice = match warning {
        Some(warning) => format!("{notice}\n⚠️ {warning}"),
        None => notice,
    };
    assert_eq!(replies, [(channel.id.get() + 2, notice)], "one notice");
    let core = rig.shared.core.lock().await;
    let session = &core.sessions[&channel.id];
    assert_eq!(
        (session.session_id.as_deref(), session.cleared),
        (None, true)
    );
    drop(core);
    let mailbox = rig.shared.mailbox(channel.id).snapshot().await;
    assert!(
        mailbox.intervention_queue.is_empty(),
        "the queue is discarded"
    );
    assert_ne!(
        rig.clear_fence(channel.id).await,
        channel.fence,
        "the boundary moved"
    );
}

// A Herdr-configured channel whose O still reads rotated-away sources used to refuse `!clear`;
// the clear now runs, leaves tmux alone and warns that its Herdr end was not confirmed.
#[tokio::test]
async fn a_text_clear_of_an_unsettled_herdr_channel_runs_and_warns_pg() {
    let rig = Rig::start().await;
    let channel = rig.channel(1_479_671_305_340_101_000, Stored::Hosted).await;
    let id = channel.id.get();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(id, NODE)]);
    let _switch = crate::services::turn_host::force_switch_for_test(Some(true));
    let stop = std::env::temp_dir().join(format!("adk-text-clear-{}", uuid::Uuid::new_v4()));
    let open = crate::services::herdr_admission::Admission::new(true, Some(stop));
    let _admission = crate::services::herdr_admission::force_for_test(open);
    let runtime_root = crate::config::runtime_root().expect("a runtime root");
    let (store, era) = crate::services::herdr_launch::o_store_for_test(&runtime_root, &[id]);
    let mut seeded = store.open_channel(&era, id).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(id, ClaudeTui)]);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _unsettled = crate::services::tui_o::writer::host::force_unsettled_for_test(Some(59));

    let cleared = rig.text_clear(&channel).await;

    let unconfirmed = "이전 세션 프로세스의 종료를 확인하지 못했어요.";
    assert_cleared(&rig, &channel, cleared, Some(unconfirmed)).await;
    assert!(
        channel.alive.load(Ordering::SeqCst),
        "no tmux reset on a Herdr channel"
    );
    assert_eq!(rig.tmux.take_calls(), Vec::<String>::new(), "no tmux call");
    rig.finish(&[&channel]).await;
}

// A tmux channel whose stored host is not a confirmed legacy one used to refuse `!clear`; the
// clear now runs and ends the tmux session.
#[tokio::test]
async fn a_text_clear_of_an_unconfirmed_tmux_host_runs_and_ends_the_session_pg() {
    let rig = Rig::start().await;
    let _switch = crate::services::turn_host::force_switch_for_test(Some(false));
    let channel = rig.channel(1_479_671_305_340_102_000, Stored::Future).await;

    let cleared = rig.text_clear(&channel).await;

    assert_cleared(&rig, &channel, cleared, None).await;
    assert!(
        !channel.alive.load(Ordering::SeqCst),
        "the session's process ends"
    );
    rig.finish(&[&channel]).await;
}

// A clear that runs still answers the cleared notice exactly once and nothing else.
#[tokio::test]
async fn a_text_clear_that_runs_answers_only_the_cleared_notice_pg() {
    let rig = Rig::start().await;
    let _switch = crate::services::turn_host::force_switch_for_test(Some(false));
    let channel = rig.channel(1_479_671_305_340_103_000, Stored::Legacy).await;

    let (handled, replies) = rig.text_clear(&channel).await;

    assert!(handled.expect("the clear ran"), "the command is consumed");
    let notice = format!("{SESSION_CLEARED_RESPONSE}\n{WAITING}");
    assert_eq!(
        replies,
        [(channel.id.get() + 2, notice)],
        "one cleared notice"
    );
    let core = rig.shared.core.lock().await;
    let session = &core.sessions[&channel.id];
    assert_eq!(
        (session.session_id.as_deref(), session.cleared),
        (None, true)
    );
    drop(core);
    assert!(
        !channel.alive.load(Ordering::SeqCst),
        "the managed reset ran"
    );
    let fence = rig.clear_fence(channel.id).await;
    assert_ne!(fence, channel.fence, "the transcript boundary moved");
    rig.finish(&[&channel]).await;
}

#[path = "clear_http_overlap_tests.rs"]
mod http_overlap;
