//! #4453 regression coverage for phase-1 catch-up classification order.
//!
//! These tests use the production classifier and sweep seam. In particular,
//! the mixed-page tests pin the checkpoint at the contiguous settled frontier:
//! terminal skips advance it, while a recoverable message blocked by capacity
//! remains strictly beyond it for the retry.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use super::{
    CatchUpClassification, CatchUpDeps, CatchUpDiscordApi, CatchUpMessageView, ChannelId,
    MessageId, ProviderKind, RuntimeChannelBindingStatus, advance_catch_up_settled_frontier,
    catch_up_too_old_drop, catch_up_too_old_notice, classify_catch_up_message, run_catch_up_sweep,
};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, MAX_INTERVENTIONS_PER_CHANNEL,
};

const CURRENT_BOT_ID: u64 = 9_001;
const INFO_BOT_ID: u64 = 1_481_522_187_197_218_816;
const HUMAN_ID: u64 = 343_742_347_365_974_026;

fn view(author_id: u64, author_is_bot: bool, age_secs: i64, text: &str) -> CatchUpMessageView {
    CatchUpMessageView {
        message_id: 1_504_813_049_431_724_053,
        author_id,
        author_is_bot,
        is_processable_kind: true,
        age_secs,
        trimmed_text: text.trim().to_string(),
    }
}

fn classify(view: &CatchUpMessageView) -> CatchUpClassification {
    classify_catch_up_message(view, Some(CURRENT_BOT_ID), &HashSet::new(), 300, &[], None)
}

#[test]
fn aged_disallowed_bot_is_not_allowed_without_dlq_or_notice_drop() {
    let message = view(
        INFO_BOT_ID,
        true,
        3_600,
        "✅ Task completed: informational echo",
    );
    let outcome = classify(&message);

    assert_eq!(
        outcome,
        CatchUpClassification::NotAllowed,
        "sender eligibility must win before age; this bot was never a replay candidate"
    );
    assert!(
        catch_up_too_old_drop(outcome, message.author_id, &message.trimmed_text).is_none(),
        "NotAllowed must not enter the shared TooOld DLQ/notice side-effect gate"
    );
}

#[test]
fn aged_empty_message_is_empty_without_dlq_or_notice_drop() {
    let message = view(HUMAN_ID, false, 3_600, "   \n\t");
    let outcome = classify(&message);

    assert_eq!(
        outcome,
        CatchUpClassification::Empty,
        "empty content must be terminal before the age gate"
    );
    assert!(
        catch_up_too_old_drop(outcome, message.author_id, &message.trimmed_text).is_none(),
        "Empty must not enter the shared TooOld DLQ/notice side-effect gate"
    );
}

#[test]
fn aged_allowed_human_is_too_old_and_advances_the_settled_frontier() {
    let message = view(HUMAN_ID, false, 3_600, "계속 진행해");
    let outcome = classify(&message);
    assert_eq!(outcome, CatchUpClassification::TooOld);

    let drop = catch_up_too_old_drop(outcome, message.author_id, &message.trimmed_text)
        .expect("processable stale human enters the TooOld DLQ/notice gate");
    let notice = catch_up_too_old_notice(&[drop]).expect("one TooOld drop produces a notice");
    assert!(notice.contains("계속 진행해"));
    assert_eq!(
        advance_catch_up_settled_frontier(None, message.message_id),
        Some(message.message_id),
        "TooOld is permanently settled and must retire from later scans"
    );
}

struct ScopedRuntimeRoot {
    _lock: std::sync::MutexGuard<'static, ()>,
    temp: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl ScopedRuntimeRoot {
    fn path(&self) -> &std::path::Path {
        self.temp.path()
    }
}

impl Drop for ScopedRuntimeRoot {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn scoped_runtime_root() -> ScopedRuntimeRoot {
    let lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    let temp = tempfile::tempdir().expect("create catch-up test runtime root");
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
    }
    ScopedRuntimeRoot {
        _lock: lock,
        temp,
        previous,
    }
}

fn message_id_with_age(sequence: u64, age: Duration) -> MessageId {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let age_ms = i64::try_from(age.as_millis()).expect("test age fits in i64 millis");
    let timestamp_ms = chrono::Utc::now().timestamp_millis() - age_ms;
    let discord_ms = u64::try_from(timestamp_ms - DISCORD_EPOCH_MS)
        .expect("test timestamp must be after Discord epoch");
    MessageId::new((discord_ms << 22) | sequence)
}

fn discord_message(
    channel_id: ChannelId,
    message_id: MessageId,
    author_id: u64,
    author_is_bot: bool,
    text: &str,
) -> serenity::Message {
    let mut author = serenity::User::default();
    author.id = serenity::UserId::new(author_id);
    author.name = format!("user-{author_id}");
    author.bot = author_is_bot;

    let mut message = serenity::Message::default();
    message.id = message_id;
    message.channel_id = channel_id;
    message.author = author;
    message.content = text.to_string();
    message.timestamp = message_id.created_at();
    message
}

fn write_checkpoint(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: ChannelId,
    checkpoint: u64,
) {
    let dir = root
        .join("runtime")
        .join("last_message")
        .join(provider.as_str());
    std::fs::create_dir_all(&dir).expect("create last-message provider dir");
    std::fs::write(
        dir.join(format!("{}.txt", channel_id.get())),
        checkpoint.to_string(),
    )
    .expect("write last-message checkpoint");
}

struct TestCatchUpApi {
    messages: Vec<serenity::Message>,
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for TestCatchUpApi {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        Ok(Some(CURRENT_BOT_ID))
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        _channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        RuntimeChannelBindingStatus::Owned
    }

    async fn fetch_messages(
        &self,
        _channel_id: ChannelId,
        _request: serenity::builder::GetMessages,
    ) -> Result<Vec<serenity::Message>, String> {
        Ok(self.messages.clone())
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        _shared: &Arc<super::SharedData>,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) {
    }
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_advances_through_mixed_terminal_aged_page() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_001);
    let bot_id = message_id_with_age(1, Duration::from_secs(420));
    let empty_id = message_id_with_age(2, Duration::from_secs(410));
    let human_id = message_id_with_age(3, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get() - 1);

    let api = TestCatchUpApi {
        messages: vec![
            discord_message(
                channel_id,
                bot_id,
                INFO_BOT_ID,
                true,
                "✅ Task completed: informational echo",
            ),
            discord_message(channel_id, empty_id, HUMAN_ID, false, "   "),
            discord_message(channel_id, human_id, HUMAN_ID, false, "계속 진행해"),
        ],
    };
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(human_id.get()),
        "NotAllowed, Empty, and TooOld are one contiguous settled prefix"
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "none of the three terminal aged classifications may enqueue"
    );
}

fn queued_intervention(message_id: MessageId, index: usize) -> Intervention {
    Intervention {
        author_id: serenity::UserId::new(HUMAN_ID),
        author_is_bot: false,
        message_id,
        queued_generation: super::runtime_store::load_generation(),
        source_message_ids: vec![message_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("already queued {index}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_checkpoint_stops_before_capacity_blocked_human() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_002);
    let bot_id = message_id_with_age(1, Duration::from_secs(360));
    let human_id = message_id_with_age(2, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get() - 1);

    for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
        let queued_id = MessageId::new(8_000_000_000_000_000_000 + index as u64);
        let outcome = super::super::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            queued_intervention(queued_id, index),
        )
        .await;
        assert!(super::catch_up_enqueue_accepted(&outcome));
    }

    let api = TestCatchUpApi {
        messages: vec![
            discord_message(channel_id, bot_id, INFO_BOT_ID, true, "terminal-input echo"),
            discord_message(channel_id, human_id, HUMAN_ID, false, "새 작업"),
        ],
    };
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(bot_id.get()),
        "the terminal bot skip settles, but the capacity-blocked human must not"
    );
    let retry = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .expect("blocked human remains recoverable through a retry");
    assert_eq!(retry.checkpoint, bot_id.get());
    assert!(retry.checkpoint < human_id.get());
    assert!(
        !super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .iter()
            .any(|queued| queued.message_id == human_id),
        "capacity-blocked human must remain beyond the settled checkpoint"
    );
}
