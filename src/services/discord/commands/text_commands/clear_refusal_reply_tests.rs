//! Text `!clear` through the real dispatcher into a recording Discord: a refused clear answers
//! with the reason `/clear` shows and changes nothing; one that runs answers only its notice.
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use serde_json::Value;
use serenity::{ChannelId, MessageId, UserId};

use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::admin_host_guard::tests::{Recorder, process};
use crate::services::discord::commands::{
    SESSION_CLEARED_RESPONSE, SoftClearNotifyMode, clear_channel_session_state,
};
use crate::services::discord::host_defer_gate::tests::{Case, ScriptedTmux, map_channel, postgres};
use crate::services::discord::host_teardown_gate::test_support::{Stored, channel_key, shared_on};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, Error, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};

const REFUSED: &str = "세션을 초기화하지 못했어요: ";
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

/// One channel of the rig: its session, its stored row's key, its queued input and its process.
struct Channel {
    id: ChannelId,
    tmux_name: String,
    key: String,
    queued: MessageId,
    alive: Arc<AtomicBool>,
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
        Channel {
            id,
            tmux_name,
            key,
            queued,
            alive,
        }
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
        let handled = Box::pin(super::handle_text_command_with_uploads(
            &self.ctx,
            &message,
            &self.data,
            channel.id,
            "!clear",
            &[],
            &mut None,
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

    /// What `/clear` shows for the same channel: the error its poise handler answers with.
    async fn slash_error(&self, channel: &Channel) -> String {
        let (provider, quiet) = (ProviderKind::Claude, SoftClearNotifyMode::Suppress);
        let http = &self.ctx.http;
        let clear =
            clear_channel_session_state(http, &self.shared, &provider, channel.id, "/clear", quiet);
        clear.await.expect_err("the same clear refuses").to_string()
    }

    /// A refused clear left the session, its queue, its process and its transcript alone, and
    /// neither started a turn nor queued its own text.
    async fn assert_untouched(&self, channel: &Channel, what: &str) {
        let core = self.shared.core.lock().await;
        let session = &core.sessions[&channel.id];
        assert_eq!(
            session.session_id.as_deref(),
            Some("sid"),
            "{what}: selector kept"
        );
        assert!(!session.cleared, "{what}: not cleared");
        drop(core);
        let mailbox = self.shared.mailbox(channel.id).snapshot().await;
        assert!(mailbox.cancel_token.is_none(), "{what}: no turn started");
        let queued: Vec<_> = mailbox
            .intervention_queue
            .iter()
            .map(|q| q.message_id)
            .collect();
        assert_eq!(queued, [channel.queued], "{what}: the queue is as it was");
        assert!(
            channel.alive.load(Ordering::SeqCst),
            "{what}: the process is kept"
        );
        assert_eq!(
            self.tmux.take_calls(),
            Vec::<String>::new(),
            "{what}: no tmux call"
        );
        let boundaries: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM channel_session_clear_boundaries WHERE channel_id = $1",
        )
        .bind(channel.id.get().to_string())
        .fetch_one(&self.pool)
        .await
        .unwrap();
        assert_eq!(boundaries, 0, "{what}: no transcript boundary");
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

/// The refused command is handled, answers once with the reason and never the cleared notice.
fn assert_refusal_answered(
    (handled, replies): (Result<bool, Error>, Vec<(u64, String)>),
    what: &str,
) -> String {
    let handled = handled.unwrap_or_else(|error| panic!("{what}: the refusal left as {error}"));
    assert!(handled, "{what}: the command is consumed");
    let cleared = replies
        .iter()
        .any(|(_, text)| text == SESSION_CLEARED_RESPONSE);
    assert!(
        !cleared,
        "{what}: no cleared notice for a refused clear: {replies:?}"
    );
    assert_eq!(replies.len(), 1, "{what}: one answer: {replies:?}");
    let reason = replies[0].1.clone();
    assert!(reason.starts_with(REFUSED), "{what}: {reason}");
    reason
}

// A Herdr-configured channel whose O still reads rotated-away sources refuses `!clear`; the text
// command answers that reason instead of failing silently, and the execution row stays Bound.
#[tokio::test]
async fn a_herdr_refused_text_clear_answers_the_channel_with_its_reason_pg() {
    let rig = Rig::start().await;
    let channel = rig.channel(1_479_671_305_340_101_000, Stored::Hosted).await;
    let id = channel.id.get();
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(id, NODE)]);
    let _switch = crate::services::turn_host::force_switch_for_test(Some(true));
    let stop = std::env::temp_dir().join(format!("adk-text-clear-{}", uuid::Uuid::new_v4()));
    let open = crate::services::herdr_admission::Admission::new(None, Some(stop));
    let _admission = crate::services::herdr_admission::force_for_test(open);
    let runtime_root = crate::config::runtime_root().expect("a runtime root");
    let (store, era) = crate::services::herdr_launch::o_store_for_test(&runtime_root, &[id]);
    let mut seeded = store.open_channel(&era, id).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(id, ClaudeTui)]);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    let _unsettled = crate::services::tui_o::writer::host::force_unsettled_for_test(Some(59));

    let reason = assert_refusal_answered(rig.text_clear(&channel).await, "herdr");

    assert!(
        reason.contains("herdr clear refused: rotation_unsettled(59)"),
        "{reason}"
    );
    rig.assert_untouched(&channel, "herdr").await;
    let state: Option<String> = sqlx::query_scalar(
        "SELECT hosted_execution->>'state' FROM sessions WHERE session_key = $1",
    )
    .bind(&channel.key)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert_eq!(state.as_deref(), Some("bound"), "the execution row is kept");
    assert_eq!(
        reason,
        rig.slash_error(&channel).await,
        "as `/clear` shows it"
    );
    rig.finish(&[&channel]).await;
}

// A tmux channel whose stored host is not a confirmed legacy one refuses `!clear` through main's
// managed-reset guard; the text command answers that reason too.
#[tokio::test]
async fn a_tmux_refused_text_clear_answers_the_channel_with_its_reason_pg() {
    let rig = Rig::start().await;
    let _switch = crate::services::turn_host::force_switch_for_test(Some(false));
    let channel = rig.channel(1_479_671_305_340_102_000, Stored::Future).await;

    let reason = assert_refusal_answered(rig.text_clear(&channel).await, "tmux");

    assert!(reason.contains(&channel.tmux_name), "{reason}");
    rig.assert_untouched(&channel, "tmux").await;
    assert_eq!(
        reason,
        rig.slash_error(&channel).await,
        "as `/clear` shows it"
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
    let notice = SESSION_CLEARED_RESPONSE.to_string();
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
    rig.finish(&[&channel]).await;
}
