//! #4453 regression coverage for phase-1 catch-up classification order.
//!
//! These tests use the production classifier and sweep seam. In particular,
//! the mixed-page tests pin the checkpoint at the contiguous settled frontier:
//! terminal skips advance it, while a recoverable message blocked by capacity
//! remains strictly beyond it for the retry.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;

use super::{
    CATCH_UP_RETRY_DEFERRED_REARM_LIMIT, CatchUpClassification, CatchUpClassificationDecision,
    CatchUpDeps, CatchUpDiscordApi, CatchUpMessageView, CatchUpTooOldOutboxRequest, ChannelId,
    MessageId, ProviderKind, RuntimeChannelBindingStatus, advance_catch_up_settled_frontier,
    catch_up_too_old_drop, catch_up_too_old_notice, classify_catch_up_message,
    classify_catch_up_message_with_utility_resolution, run_catch_up_sweep,
};
use crate::services::discord::health::UtilityBotUserIdResolution;
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, MAX_INTERVENTIONS_PER_CHANNEL,
};

const CURRENT_BOT_ID: u64 = 9_001;
const INFO_BOT_ID: u64 = 1_481_522_187_197_218_816;
const ANNOUNCE_BOT_ID: u64 = 1_481_522_187_197_218_817;
const NOTIFY_BOT_ID: u64 = 1_481_522_187_197_218_818;
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
    classify_catch_up_message(
        view,
        Some(CURRENT_BOT_ID),
        &HashSet::new(),
        300,
        &[],
        None,
        None,
    )
}

fn classify_with_resolutions(
    view: &CatchUpMessageView,
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
) -> CatchUpClassificationDecision {
    classify_catch_up_message_with_utility_resolution(
        view,
        Some(CURRENT_BOT_ID),
        &HashSet::new(),
        300,
        &[],
        announce_resolution,
        notify_resolution,
    )
}

#[test]
fn unconfigured_utility_id_is_stable_absence_not_a_retry_loop() {
    let human = view(HUMAN_ID, false, 60, "계속 진행해");
    assert_eq!(
        classify_with_resolutions(
            &human,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::Recover)
    );

    let ordinary_bot = view(INFO_BOT_ID, true, 60, "informational status only");
    assert_eq!(
        classify_with_resolutions(
            &ordinary_bot,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::NotAllowed),
        "a deliberately absent utility identity must not defer every ordinary bot forever"
    );
}

#[test]
fn unavailable_utility_id_defers_only_when_sender_semantics_can_change() {
    let markerless_bot = view(
        INFO_BOT_ID,
        true,
        60,
        "PM triage: inspect the stalled workflow",
    );
    assert_eq!(
        classify_with_resolutions(
            &markerless_bot,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::UtilityIdentityUnavailable,
        "an unresolved announce identity can turn NotAllowed into Recover"
    );

    let false_flag_human_shape = view(HUMAN_ID, false, 60, "계속 진행해");
    assert_eq!(
        classify_with_resolutions(
            &false_flag_human_shape,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unavailable,
        ),
        CatchUpClassificationDecision::UtilityIdentityUnavailable,
        "an unresolved notify identity can turn a false-flag Recover into NotAllowed"
    );

    let stale_false_flag = view(HUMAN_ID, false, 3_600, "진짜 사용자 요청");
    assert_eq!(
        classify_with_resolutions(
            &stale_false_flag,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::UtilityIdentityUnavailable,
        "same TooOld enum still defers when announce identity changes the user-facing resend surface"
    );

    let legacy_card = view(
        INFO_BOT_ID,
        true,
        60,
        "📋 **새 이슈 #42** — fix the thing\n> 상태: 🟡 open",
    );
    assert_eq!(
        classify_with_resolutions(
            &legacy_card,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::NotAllowed),
        "legacy announce cards are suppressed with or without the identity and must not retry forever"
    );

    let ordinary_bot = view(INFO_BOT_ID, true, 60, "informational status only");
    assert_eq!(
        classify_with_resolutions(
            &ordinary_bot,
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Unavailable,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::NotAllowed),
        "a plain bot message is NotAllowed even if it is notify, so lookup failure is immaterial"
    );
}

#[test]
fn notify_identity_is_terminal_before_age_even_when_discord_bot_flag_is_false() {
    for (label, age_secs) in [("stale", 3_600), ("fresh", 60)] {
        let message = view(
            NOTIFY_BOT_ID,
            false,
            age_secs,
            "✅ Task completed: informational echo",
        );
        let outcome = classify_catch_up_message(
            &message,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            300,
            &[NOTIFY_BOT_ID],
            Some(NOTIFY_BOT_ID),
            Some(NOTIFY_BOT_ID),
        );
        assert_eq!(
            outcome,
            CatchUpClassification::NotAllowed,
            "{label} notify output must never become a recoverable turn or TooOld evidence"
        );
        assert!(
            catch_up_too_old_drop(
                outcome,
                message.author_id,
                message.author_is_bot,
                &[NOTIFY_BOT_ID],
                Some(NOTIFY_BOT_ID),
                Some(NOTIFY_BOT_ID),
                &message.trimmed_text,
            )
            .is_none(),
            "{label} notify output must not enter the DLQ/notice side-effect gate"
        );
    }

    for (label, author_id, allowed_bot_ids, announce_bot_id, text) in [
        (
            "announce",
            ANNOUNCE_BOT_ID,
            Vec::new(),
            Some(ANNOUNCE_BOT_ID),
            "PM triage: inspect the stalled workflow",
        ),
        (
            "allowed",
            INFO_BOT_ID,
            vec![INFO_BOT_ID],
            None,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
        ),
    ] {
        let stale = view(author_id, false, 3_600, text);
        let fresh = view(author_id, false, 60, text);
        assert_eq!(
            classify_catch_up_message(
                &stale,
                Some(CURRENT_BOT_ID),
                &HashSet::new(),
                300,
                &allowed_bot_ids,
                announce_bot_id,
                Some(NOTIFY_BOT_ID),
            ),
            CatchUpClassification::TooOld,
            "false-flag {label} identity must retain stale eligible-trigger semantics"
        );
        assert_eq!(
            classify_catch_up_message(
                &fresh,
                Some(CURRENT_BOT_ID),
                &HashSet::new(),
                300,
                &allowed_bot_ids,
                announce_bot_id,
                Some(NOTIFY_BOT_ID),
            ),
            CatchUpClassification::Recover,
            "false-flag {label} identity must retain fresh eligible-trigger semantics"
        );
    }
}

#[test]
fn aged_task_notify_and_system_messages_never_become_actionable_too_old() {
    let task = view(
        INFO_BOT_ID,
        true,
        3_600,
        "✅ Task completed: informational echo",
    );
    let notify = view(
        INFO_BOT_ID,
        true,
        3_600,
        "⚠️ 스톨 의심: 정상 작업 중이면 무시하세요",
    );
    let mut system = view(INFO_BOT_ID, true, 3_600, "thread-created system event");
    system.is_processable_kind = false;

    for (label, message, expected) in [
        ("task", task, CatchUpClassification::NotAllowed),
        ("notify", notify, CatchUpClassification::NotAllowed),
        ("system", system, CatchUpClassification::SystemKind),
    ] {
        let outcome = classify(&message);
        assert_eq!(
            outcome, expected,
            "{label} classification must win before the age gate"
        );
        assert!(
            catch_up_too_old_drop(
                outcome,
                message.author_id,
                message.author_is_bot,
                &[],
                None,
                None,
                &message.trimmed_text,
            )
            .is_none(),
            "{label} must not enter the actionable TooOld notice gate"
        );
    }
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
        catch_up_too_old_drop(
            outcome,
            message.author_id,
            message.author_is_bot,
            &[],
            None,
            None,
            &message.trimmed_text,
        )
        .is_none(),
        "Empty must not enter the shared TooOld DLQ/notice side-effect gate"
    );
}

#[test]
fn aged_allowed_human_is_too_old_and_advances_the_settled_frontier() {
    let message = view(HUMAN_ID, false, 3_600, "계속 진행해");
    let outcome = classify(&message);
    assert_eq!(outcome, CatchUpClassification::TooOld);

    let drop = catch_up_too_old_drop(
        outcome,
        message.author_id,
        message.author_is_bot,
        &[],
        None,
        None,
        &message.trimmed_text,
    )
    .expect("processable stale human enters the TooOld DLQ/notice gate");
    let notice = catch_up_too_old_notice(&[drop]).expect("one TooOld drop produces a notice");
    assert!(notice.contains("계속 진행해"));
    assert!(notice.contains("1건"));
    assert_eq!(
        advance_catch_up_settled_frontier(None, message.message_id),
        Some(message.message_id),
        "TooOld is permanently settled and must retire from later scans"
    );
}

#[test]
fn aged_announce_bot_settles_without_a_human_resend_notice() {
    let message = view(
        INFO_BOT_ID,
        true,
        3_600,
        "PM triage: inspect the stalled workflow",
    );
    let outcome = classify_catch_up_message(
        &message,
        Some(CURRENT_BOT_ID),
        &HashSet::new(),
        300,
        &[],
        Some(INFO_BOT_ID),
        None,
    );
    assert_eq!(
        outcome,
        CatchUpClassification::TooOld,
        "an announce-authored trigger is eligible but unsafe to replay after the age limit"
    );

    assert!(
        catch_up_too_old_drop(
            outcome,
            message.author_id,
            message.author_is_bot,
            &[],
            Some(INFO_BOT_ID),
            None,
            &message.trimmed_text,
        )
        .is_none(),
        "a human cannot resend an announce-bot trigger, so it must not construct an actionable drop"
    );
    assert_eq!(
        advance_catch_up_settled_frontier(None, message.message_id),
        Some(message.message_id),
        "the terminal bot trigger must still advance the contiguous settled frontier"
    );

    let fresh = view(
        INFO_BOT_ID,
        true,
        60,
        "PM triage: inspect the stalled workflow",
    );
    assert_eq!(
        classify_catch_up_message(
            &fresh,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            300,
            &[],
            Some(INFO_BOT_ID),
            None,
        ),
        CatchUpClassification::Recover,
        "the human-only notice gate must not suppress a fresh announce trigger"
    );
}

#[test]
fn aged_marker_authorized_bot_settles_without_notice_but_fresh_trigger_recovers() {
    let stale = view(
        INFO_BOT_ID,
        true,
        3_600,
        "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
    );
    let stale_outcome = classify_catch_up_message(
        &stale,
        Some(CURRENT_BOT_ID),
        &HashSet::new(),
        300,
        &[INFO_BOT_ID],
        None,
        None,
    );
    assert_eq!(stale_outcome, CatchUpClassification::TooOld);
    assert!(
        catch_up_too_old_drop(
            stale_outcome,
            stale.author_id,
            stale.author_is_bot,
            &[INFO_BOT_ID],
            None,
            None,
            &stale.trimmed_text,
        )
        .is_none(),
        "an allowed automation trigger is internal evidence, never a human resend candidate"
    );

    let fresh = view(
        INFO_BOT_ID,
        true,
        60,
        "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
    );
    assert_eq!(
        classify_catch_up_message(
            &fresh,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            300,
            &[INFO_BOT_ID],
            None,
            None,
        ),
        CatchUpClassification::Recover,
        "fresh marker-authorized automation remains a valid turn trigger"
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
    let path = checkpoint_path(root, provider, channel_id);
    std::fs::create_dir_all(path.parent().expect("last-message provider dir"))
        .expect("create last-message provider dir");
    std::fs::write(path, checkpoint.to_string()).expect("write last-message checkpoint");
}

fn checkpoint_path(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: ChannelId,
) -> std::path::PathBuf {
    root.join("runtime")
        .join("last_message")
        .join(provider.as_str())
        .join(format!("{}.txt", channel_id.get()))
}

fn write_role_map(root: &std::path::Path, provider: &ProviderKind, channel_id: ChannelId) {
    let config_dir = root.join("config");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("role_map.json"),
        format!(
            r#"{{
  "byChannelId": {{
    "{}": {{
      "roleId": "adk-cc",
      "promptFile": "prompt.md",
      "provider": "{}"
    }}
  }}
}}"#,
            channel_id.get(),
            provider.as_str(),
        ),
    )
    .expect("write role map");
}

struct TestCatchUpApi {
    messages: Vec<serenity::Message>,
    phase2_messages: Option<Vec<serenity::Message>>,
    scripted_fetches: Option<Mutex<VecDeque<Result<Vec<serenity::Message>, String>>>>,
    fetch_calls: AtomicUsize,
    outbox: Arc<Mutex<Vec<CatchUpTooOldOutboxRequest>>>,
    dead_letters: Arc<Mutex<Vec<crate::db::relay_dead_letter::RelayDeadLetterRecord>>>,
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
}

impl TestCatchUpApi {
    fn new(
        messages: Vec<serenity::Message>,
    ) -> (Self, Arc<Mutex<Vec<CatchUpTooOldOutboxRequest>>>) {
        let outbox = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                messages,
                phase2_messages: None,
                scripted_fetches: None,
                fetch_calls: AtomicUsize::new(0),
                outbox: Arc::clone(&outbox),
                dead_letters: Arc::new(Mutex::new(Vec::new())),
                announce_resolution: UtilityBotUserIdResolution::Unconfigured,
                notify_resolution: UtilityBotUserIdResolution::Unconfigured,
            },
            outbox,
        )
    }

    fn with_utility_bot_ids(
        mut self,
        announce_bot_id: Option<u64>,
        notify_bot_id: Option<u64>,
    ) -> Self {
        self.announce_resolution = announce_bot_id.map_or(
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Resolved,
        );
        self.notify_resolution = notify_bot_id.map_or(
            UtilityBotUserIdResolution::Unconfigured,
            UtilityBotUserIdResolution::Resolved,
        );
        self
    }

    fn with_utility_bot_resolutions(
        mut self,
        announce_resolution: UtilityBotUserIdResolution,
        notify_resolution: UtilityBotUserIdResolution,
    ) -> Self {
        self.announce_resolution = announce_resolution;
        self.notify_resolution = notify_resolution;
        self
    }

    fn with_phase2_messages(mut self, messages: Vec<serenity::Message>) -> Self {
        self.phase2_messages = Some(messages);
        self
    }

    fn with_scripted_fetches(
        mut self,
        fetches: Vec<Result<Vec<serenity::Message>, String>>,
    ) -> Self {
        self.scripted_fetches = Some(Mutex::new(fetches.into()));
        self
    }

    fn with_outbox(mut self, outbox: Arc<Mutex<Vec<CatchUpTooOldOutboxRequest>>>) -> Self {
        self.outbox = outbox;
        self
    }
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
        let call = self.fetch_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(fetches) = &self.scripted_fetches {
            return fetches
                .lock()
                .expect("scripted fetch lock")
                .pop_front()
                .expect("scripted fetch response");
        }
        Ok(if call > 0 {
            self.phase2_messages
                .as_ref()
                .unwrap_or(&self.messages)
                .clone()
        } else {
            self.messages.clone()
        })
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        _shared: &Arc<super::SharedData>,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) {
    }

    fn enqueue_too_old_notice(
        &self,
        _pool: Option<sqlx::PgPool>,
        request: CatchUpTooOldOutboxRequest,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let mut outbox = self.outbox.lock().expect("outbox capture lock");
        if !outbox.iter().any(|existing| {
            existing.target == request.target
                && existing.content == request.content
                && existing.reason_code == request.reason_code
                && existing.session_key == request.session_key
        }) {
            outbox.push(request);
        }
        None
    }

    fn record_too_old_dead_letter(
        &self,
        _pool: Option<&sqlx::PgPool>,
        record: crate::db::relay_dead_letter::RelayDeadLetterRecord,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .push(record);
        None
    }

    async fn utility_bot_user_ids(
        &self,
        _shared: &super::SharedData,
    ) -> (UtilityBotUserIdResolution, UtilityBotUserIdResolution) {
        (self.announce_resolution, self.notify_resolution)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn production_two_scan_retries_unavailable_announce_then_recovers() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_007);
    let announce_message_id = message_id_with_age(1, Duration::from_secs(30));
    let initial_checkpoint = announce_message_id.get() - 1;
    write_checkpoint(root.path(), &provider, channel_id, initial_checkpoint);
    shared.settings.write().await.allowed_bot_ids = vec![ANNOUNCE_BOT_ID];
    let message = discord_message(
        channel_id,
        announce_message_id,
        ANNOUNCE_BOT_ID,
        true,
        "PM triage: inspect the stalled workflow",
    );

    let (first_api, first_outbox) = TestCatchUpApi::new(vec![message.clone()]);
    let first_api = first_api.with_utility_bot_resolutions(
        UtilityBotUserIdResolution::Unavailable,
        UtilityBotUserIdResolution::Unconfigured,
    );
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;

    assert!(
        shared
            .last_message_ids
            .get(&channel_id)
            .is_none_or(|checkpoint| *checkpoint < announce_message_id.get()),
        "an ambiguous markerless announce message must remain beyond the durable frontier"
    );
    let first_retry = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .expect("identity uncertainty must preserve a bounded retry");
    assert_eq!(first_retry.checkpoint, initial_checkpoint);
    drop(first_retry);
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "the unavailable scan must neither lose nor prematurely enqueue the message"
    );
    assert!(first_outbox.lock().expect("outbox capture lock").is_empty());

    let pending_retry_channels = HashSet::from([channel_id]);
    let (second_api, second_outbox) = TestCatchUpApi::new(vec![message]);
    let second_api = second_api.with_utility_bot_resolutions(
        UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
        UtilityBotUserIdResolution::Unconfigured,
    );
    run_catch_up_sweep(
        CatchUpDeps::new(&second_api, &shared, &provider)
            .with_pending_retry_channels(&pending_retry_channels),
    )
    .await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect::<Vec<_>>(),
        vec![announce_message_id],
        "the resolved scan must recover the exact markerless announce trigger"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(announce_message_id.get())
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        second_outbox
            .lock()
            .expect("outbox capture lock")
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn production_two_scan_retries_unavailable_notify_then_settles_silently() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_009);
    let notify_message_id = message_id_with_age(1, Duration::from_secs(30));
    let initial_checkpoint = notify_message_id.get() - 1;
    write_checkpoint(root.path(), &provider, channel_id, initial_checkpoint);
    shared.settings.write().await.allowed_bot_ids = vec![NOTIFY_BOT_ID];
    let message = discord_message(
        channel_id,
        notify_message_id,
        NOTIFY_BOT_ID,
        false,
        "DISPATCH:false-flag-notify-overlap",
    );

    let (first_api, _) = TestCatchUpApi::new(vec![message.clone()]);
    let first_api = first_api.with_utility_bot_resolutions(
        UtilityBotUserIdResolution::Unconfigured,
        UtilityBotUserIdResolution::Unavailable,
    );
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;
    assert!(
        shared
            .last_message_ids
            .get(&channel_id)
            .is_none_or(|checkpoint| *checkpoint < notify_message_id.get())
    );
    assert!(shared.catch_up_retry_pending.contains_key(&channel_id));

    let pending_retry_channels = HashSet::from([channel_id]);
    let (second_api, second_outbox) = TestCatchUpApi::new(vec![message]);
    let second_api = second_api.with_utility_bot_resolutions(
        UtilityBotUserIdResolution::Unconfigured,
        UtilityBotUserIdResolution::Resolved(NOTIFY_BOT_ID),
    );
    run_catch_up_sweep(
        CatchUpDeps::new(&second_api, &shared, &provider)
            .with_pending_retry_channels(&pending_retry_channels),
    )
    .await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(notify_message_id.get()),
        "resolved notify output is a stable terminal skip and can settle"
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "notify output must never become a turn"
    );
    assert!(
        second_outbox
            .lock()
            .expect("outbox capture lock")
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn unavailable_identity_retry_cap_never_settles_ambiguous_trigger() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_008);
    let message_id = message_id_with_age(1, Duration::from_secs(30));
    let initial_checkpoint = message_id.get() - 1;
    write_checkpoint(root.path(), &provider, channel_id, initial_checkpoint);
    shared.settings.write().await.allowed_bot_ids = vec![ANNOUNCE_BOT_ID];
    let message = discord_message(
        channel_id,
        message_id,
        ANNOUNCE_BOT_ID,
        true,
        "PM triage: preserve me while identity lookup is down",
    );

    // One initial arm plus exactly the configured number of carried retries
    // exhausts the tight retry chain. The cap clears the in-memory arm/backoff;
    // it must never convert uncertainty into a settled checkpoint. A later
    // periodic catch-up therefore starts again from the same durable cursor.
    for _ in 0..=CATCH_UP_RETRY_DEFERRED_REARM_LIMIT {
        let pending_retry_channels = shared
            .catch_up_retry_pending
            .contains_key(&channel_id)
            .then(|| HashSet::from([channel_id]))
            .unwrap_or_default();
        let (api, _) = TestCatchUpApi::new(vec![message.clone()]);
        let api = api.with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        );
        run_catch_up_sweep(
            CatchUpDeps::new(&api, &shared, &provider)
                .with_pending_retry_channels(&pending_retry_channels),
        )
        .await;
    }

    assert!(
        !shared.catch_up_retry_pending.contains_key(&channel_id),
        "the bounded retry chain must stop after its configured budget"
    );
    assert!(
        shared
            .last_message_ids
            .get(&channel_id)
            .is_none_or(|checkpoint| *checkpoint < message_id.get()),
        "retry exhaustion must preserve the ambiguous trigger beyond the durable frontier"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn recent_partial_page_failure_preserves_gap_then_recovers_older_human() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_010);
    write_role_map(root.path(), &provider, channel_id);

    let newest_terminal_id = message_id_with_age(3, Duration::from_secs(30));
    let buried_human_id = message_id_with_age(2, Duration::from_secs(120));
    let age_boundary_bot_id = message_id_with_age(1, Duration::from_secs(360));
    let newest_terminal = discord_message(
        channel_id,
        newest_terminal_id,
        INFO_BOT_ID,
        true,
        "informational terminal-only page",
    );
    let buried_human = discord_message(
        channel_id,
        buried_human_id,
        HUMAN_ID,
        false,
        "page 2 user request",
    );
    let age_boundary_bot = discord_message(
        channel_id,
        age_boundary_bot_id,
        INFO_BOT_ID,
        true,
        "older non-actionable boundary",
    );

    let (first_api, first_outbox) = TestCatchUpApi::new(Vec::new());
    let first_api = first_api.with_scripted_fetches(vec![
        Ok(vec![newest_terminal.clone()]),
        Err("transient page 2 failure".to_string()),
        Ok(Vec::new()), // mutation-only phase-2 fallback; normally left unused
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;

    assert!(
        shared.last_message_ids.get(&channel_id).is_none(),
        "a newer terminal-only partial page must not create a durable frontier past the unknown gap"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "an incomplete Recent batch is retried as a whole rather than partially committed"
    );
    assert!(first_outbox.lock().expect("outbox capture lock").is_empty());
    assert!(
        first_api
            .dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .is_empty()
    );
    assert_eq!(
        first_api.fetch_calls.load(Ordering::Relaxed),
        2,
        "phase 2 must not bypass an incomplete Recent lower gap"
    );

    let (second_api, _) = TestCatchUpApi::new(Vec::new());
    let second_api = second_api.with_scripted_fetches(vec![
        Ok(vec![newest_terminal]),
        Ok(vec![buried_human, age_boundary_bot]),
        Ok(Vec::new()), // phase-2 backstop
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&second_api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect::<Vec<_>>(),
        vec![buried_human_id],
        "the next complete Recent scan must recover the human hidden behind the failed page"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(newest_terminal_id.get()),
        "only the complete oldest-first batch may advance the settled frontier"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn recent_initial_fetch_failure_blocks_phase2_then_recovers_whole_gap() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_015);
    write_role_map(root.path(), &provider, channel_id);

    let bot_response_id = message_id_with_age(1, Duration::from_secs(180));
    let older_human_id = message_id_with_age(2, Duration::from_secs(120));
    let newer_human_id = message_id_with_age(3, Duration::from_secs(30));
    let older_human = discord_message(
        channel_id,
        older_human_id,
        HUMAN_ID,
        false,
        "older user request below the failed Recent page",
    );
    let newer_human = discord_message(
        channel_id,
        newer_human_id,
        HUMAN_ID,
        false,
        "newer unanswered user request",
    );
    let bot_response = discord_message(
        channel_id,
        bot_response_id,
        CURRENT_BOT_ID,
        true,
        "previous bot response",
    );

    // If the initial Recent failure is not marked incomplete, phase 2 consumes
    // the second response, enqueues `newer_human`, and persists its id across
    // the unknown lower gap. The correct path stops after the first fetch.
    let (first_api, first_outbox) = TestCatchUpApi::new(Vec::new());
    let first_api = first_api.with_scripted_fetches(vec![
        Err("transient initial Recent fetch failure".to_string()),
        Ok(vec![newer_human.clone(), bot_response]),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;

    assert_eq!(
        first_api.fetch_calls.load(Ordering::Relaxed),
        1,
        "an unread unbounded Recent gap must block the entire channel's phase-2 scan"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "the failed sweep must not enqueue the newer phase-2 item"
    );
    assert!(shared.last_message_ids.get(&channel_id).is_none());
    assert!(
        !checkpoint_path(root.path(), &provider, channel_id).exists(),
        "the failed sweep must not create a durable frontier"
    );
    assert!(first_outbox.lock().expect("outbox capture lock").is_empty());
    assert!(
        first_api
            .dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .is_empty()
    );

    // A later complete Recent sweep starts from the still-open lower bound and
    // recovers both messages chronologically instead of only the newer one.
    let (second_api, second_outbox) = TestCatchUpApi::new(Vec::new());
    let second_api = second_api.with_scripted_fetches(vec![
        Ok(vec![newer_human, older_human]),
        Ok(Vec::new()),
        Ok(Vec::new()),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&second_api, &shared, &provider)).await;

    let recovered_mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        super::super::recovery_known_message_ids(&recovered_mailbox),
        HashSet::from([older_human_id.get(), newer_human_id.get()]),
        "the next complete sweep must recover the whole previously unknown gap, including ids merged into one intervention"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(newer_human_id.get())
    );
    assert!(
        second_outbox
            .lock()
            .expect("outbox capture lock")
            .is_empty()
    );
    assert!(
        second_api
            .dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_advances_through_mixed_terminal_aged_page() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_001);
    let task_id = message_id_with_age(1, Duration::from_secs(450));
    let notify_id = message_id_with_age(2, Duration::from_secs(440));
    let system_id = message_id_with_age(3, Duration::from_secs(430));
    let empty_id = message_id_with_age(4, Duration::from_secs(410));
    let human_id = message_id_with_age(5, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel_id, task_id.get() - 1);

    let mut system = discord_message(
        channel_id,
        system_id,
        INFO_BOT_ID,
        true,
        "thread-created system event",
    );
    system.kind = serenity::MessageType::PinsAdd;

    let (api, outbox) = TestCatchUpApi::new(vec![
        discord_message(
            channel_id,
            task_id,
            INFO_BOT_ID,
            true,
            "✅ Task completed: informational echo",
        ),
        discord_message(
            channel_id,
            notify_id,
            INFO_BOT_ID,
            true,
            "⚠️ 스톨 의심: 정상 작업 중이면 무시하세요",
        ),
        system,
        discord_message(channel_id, empty_id, HUMAN_ID, false, "   "),
        discord_message(channel_id, human_id, HUMAN_ID, false, "계속 진행해"),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(human_id.get()),
        "task/notify/SystemKind/Empty and human TooOld are one contiguous settled prefix"
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "none of the five terminal aged classifications may enqueue"
    );
    assert_eq!(
        *outbox.lock().expect("outbox capture lock"),
        vec![CatchUpTooOldOutboxRequest {
            target: format!("channel:{channel_id}"),
            content: format!(
                "⚠️ 재시작 공백으로 1건이 5분 초과로 미처리되었습니다. 필요하면 다시 보내주세요:\n• `{HUMAN_ID}`: 계속 진행해"
            ),
            bot: "notify",
            source: "catch_up_too_old",
            reason_code: "catch_up.too_old",
            session_key: format!("catch_up_too_old:{channel_id}:{}", human_id.get()),
        }],
        "production sweep must construct the exact deduplicating outbox contract without a PG pool"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_uses_semantic_utility_identity_when_bot_flag_is_false() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_003);
    let announce_id = message_id_with_age(1, Duration::from_secs(430));
    let allowed_id = message_id_with_age(2, Duration::from_secs(420));
    let notify_id = message_id_with_age(3, Duration::from_secs(410));
    let human_id = message_id_with_age(4, Duration::from_secs(400));
    let fresh_notify_id = message_id_with_age(5, Duration::from_secs(60));
    write_checkpoint(root.path(), &provider, channel_id, announce_id.get() - 1);
    shared.settings.write().await.allowed_bot_ids = vec![INFO_BOT_ID];

    let (api, outbox) = TestCatchUpApi::new(vec![
        discord_message(
            channel_id,
            announce_id,
            ANNOUNCE_BOT_ID,
            false,
            "PM triage: inspect the stalled workflow",
        ),
        discord_message(
            channel_id,
            allowed_id,
            INFO_BOT_ID,
            false,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
        ),
        discord_message(
            channel_id,
            notify_id,
            NOTIFY_BOT_ID,
            false,
            "✅ Task completed: informational echo",
        ),
        discord_message(channel_id, human_id, HUMAN_ID, false, "진짜 사용자 요청"),
        discord_message(
            channel_id,
            fresh_notify_id,
            NOTIFY_BOT_ID,
            false,
            "⚠️ fresh notify output",
        ),
    ]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(fresh_notify_id.get()),
        "all terminal inputs settle regardless of Discord's bot flag"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
    let outbox = outbox.lock().expect("outbox capture lock");
    assert_eq!(outbox.len(), 1, "known utility identities must stay silent");
    assert_eq!(
        outbox[0].session_key,
        format!("catch_up_too_old:{channel_id}:{}", human_id.get())
    );
    assert!(outbox[0].content.contains("진짜 사용자 요청"));
    assert!(!outbox[0].content.contains("PM triage"));
    assert!(!outbox[0].content.contains("DISPATCH:"));
    assert!(!outbox[0].content.contains("Task completed"));
    let dead_letters: Vec<_> = api
        .dead_letters
        .lock()
        .expect("dead-letter capture lock")
        .iter()
        .map(|record| {
            (
                record.kind.clone(),
                record.channel_id.clone(),
                record.author_id.clone(),
                record.message_id.clone(),
                record.content.clone(),
                record.reason.clone(),
            )
        })
        .collect();
    assert_eq!(
        dead_letters,
        vec![
            (
                "catch_up_too_old".to_string(),
                channel_id.to_string(),
                Some(ANNOUNCE_BOT_ID.to_string()),
                Some(announce_id.get().to_string()),
                "PM triage: inspect the stalled workflow".to_string(),
                "age_secs=430 > max_age_secs=300".to_string(),
            ),
            (
                "catch_up_too_old".to_string(),
                channel_id.to_string(),
                Some(INFO_BOT_ID.to_string()),
                Some(allowed_id.get().to_string()),
                "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000".to_string(),
                "age_secs=420 > max_age_secs=300".to_string(),
            ),
            (
                "catch_up_too_old".to_string(),
                channel_id.to_string(),
                Some(HUMAN_ID.to_string()),
                Some(human_id.get().to_string()),
                "진짜 사용자 요청".to_string(),
                "age_secs=400 > max_age_secs=300".to_string(),
            ),
        ],
        "every expected DLQ field must remain exact, while false-flag notify output never enters DLQ"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn production_phase2_notify_overlap_is_blocked_before_recovery() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_006);
    let notify_id = message_id_with_age(3, Duration::from_secs(10));
    let allowed_id = message_id_with_age(2, Duration::from_secs(20));
    let bot_response_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        bot_response_id.get() - 1,
    );
    shared.settings.write().await.allowed_bot_ids = vec![NOTIFY_BOT_ID, INFO_BOT_ID];

    let (api, outbox) = TestCatchUpApi::new(Vec::new());
    let api = api
        .with_utility_bot_ids(Some(NOTIFY_BOT_ID), Some(NOTIFY_BOT_ID))
        .with_phase2_messages(vec![
            discord_message(
                channel_id,
                notify_id,
                NOTIFY_BOT_ID,
                false,
                "DISPATCH:notify-overlaps-allowed-and-announce",
            ),
            discord_message(
                channel_id,
                allowed_id,
                INFO_BOT_ID,
                false,
                "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
            ),
            discord_message(
                channel_id,
                bot_response_id,
                CURRENT_BOT_ID,
                true,
                "previous bot response",
            ),
        ]);

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    let recovered_ids: Vec<_> = mailbox
        .intervention_queue
        .iter()
        .map(|intervention| intervention.message_id)
        .collect();
    assert_eq!(
        recovered_ids,
        vec![allowed_id],
        "phase2 must retain false-flag allowed automation but block notify even when its ID is simultaneously allowed and announce"
    );
    assert!(
        !recovered_ids.contains(&notify_id),
        "notify semantic identity must win before phase2 recovery"
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
    assert!(
        api.dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .is_empty(),
        "phase2 never turns notify output into TooOld evidence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_aged_input_does_not_retry_when_utility_identity_is_unavailable() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_011);
    let bot_response_id = message_id_with_age(1, Duration::from_secs(800));
    let aged_human_id = message_id_with_age(2, Duration::from_secs(700));
    write_checkpoint(root.path(), &provider, channel_id, bot_response_id.get());

    let (api, _) = TestCatchUpApi::new(Vec::new());
    let api = api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unavailable,
        )
        .with_phase2_messages(vec![
            discord_message(
                channel_id,
                aged_human_id,
                HUMAN_ID,
                false,
                "stale unanswered request",
            ),
            discord_message(
                channel_id,
                bot_response_id,
                CURRENT_BOT_ID,
                true,
                "older bot response",
            ),
        ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        !shared.catch_up_retry_pending.contains_key(&channel_id),
        "phase2 age is identity-independent and must not start an unavailable-id retry chain"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
    assert_eq!(api.fetch_calls.load(Ordering::Relaxed), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_checkpointed_input_does_not_retry_when_utility_identity_is_unavailable() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_012);
    let bot_response_id = message_id_with_age(1, Duration::from_secs(60));
    let checkpointed_human_id = message_id_with_age(2, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        checkpointed_human_id.get(),
    );
    shared
        .last_message_ids
        .insert(channel_id, checkpointed_human_id.get());

    let (api, _) = TestCatchUpApi::new(Vec::new());
    let api = api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unavailable,
        )
        .with_phase2_messages(vec![
            discord_message(
                channel_id,
                checkpointed_human_id,
                HUMAN_ID,
                false,
                "already checkpointed request",
            ),
            discord_message(
                channel_id,
                bot_response_id,
                CURRENT_BOT_ID,
                true,
                "previous bot response",
            ),
        ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        !shared.catch_up_retry_pending.contains_key(&channel_id),
        "a saved phase2 checkpoint must settle before utility counterfactuals"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
    assert_eq!(api.fetch_calls.load(Ordering::Relaxed), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_fresh_announce_unavailable_then_resolved_recovers_eventually() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_013);
    let bot_response_id = message_id_with_age(1, Duration::from_secs(60));
    let announce_message_id = message_id_with_age(2, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel_id, bot_response_id.get());
    let phase2_messages = vec![
        discord_message(
            channel_id,
            announce_message_id,
            ANNOUNCE_BOT_ID,
            true,
            "PM triage: recover this markerless trigger",
        ),
        discord_message(
            channel_id,
            bot_response_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ];

    let (first_api, _) = TestCatchUpApi::new(Vec::new());
    let first_api = first_api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        )
        .with_phase2_messages(phase2_messages.clone());
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;
    assert!(shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty(),
        "fresh identity-dependent work must wait while announce identity is unavailable"
    );

    let pending_retry_channels = HashSet::from([channel_id]);
    let (second_api, _) = TestCatchUpApi::new(Vec::new());
    let second_api = second_api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
            UtilityBotUserIdResolution::Unconfigured,
        )
        .with_phase2_messages(phase2_messages);
    run_catch_up_sweep(
        CatchUpDeps::new(&second_api, &shared, &provider)
            .with_pending_retry_channels(&pending_retry_channels),
    )
    .await;

    assert_eq!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect::<Vec<_>>(),
        vec![announce_message_id]
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_false_flag_announce_unavailable_preserves_then_recovers_exact_message() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_014);
    let bot_response_id = message_id_with_age(1, Duration::from_secs(60));
    let announce_message_id = message_id_with_age(2, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel_id, bot_response_id.get());
    let phase2_messages = vec![
        discord_message(
            channel_id,
            announce_message_id,
            ANNOUNCE_BOT_ID,
            false,
            "PM triage: recover false-flag announce",
        ),
        discord_message(
            channel_id,
            bot_response_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ];

    let (first_api, _) = TestCatchUpApi::new(Vec::new());
    let first_api = first_api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        )
        .with_phase2_messages(phase2_messages.clone());
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;

    let retry = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .expect("unresolved announce authorization bypass must arm a retry");
    assert_eq!(
        retry.checkpoint,
        bot_response_id.get(),
        "the retry must preserve the durable frontier before the ambiguous message"
    );
    drop(retry);
    assert_eq!(
        std::fs::read_to_string(checkpoint_path(root.path(), &provider, channel_id))
            .expect("read preserved durable checkpoint")
            .trim(),
        bot_response_id.get().to_string(),
        "the unavailable scan must not advance the durable checkpoint"
    );
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );

    let pending_retry_channels = HashSet::from([channel_id]);
    let (second_api, _) = TestCatchUpApi::new(Vec::new());
    let second_api = second_api
        .with_utility_bot_resolutions(
            UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
            UtilityBotUserIdResolution::Unconfigured,
        )
        .with_phase2_messages(phase2_messages);
    run_catch_up_sweep(
        CatchUpDeps::new(&second_api, &shared, &provider)
            .with_pending_retry_channels(&pending_retry_channels),
    )
    .await;

    assert_eq!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .iter()
            .map(|intervention| (intervention.message_id, intervention.text.as_str()))
            .collect::<Vec<_>>(),
        vec![(
            announce_message_id,
            "PM triage: recover false-flag announce"
        )]
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(announce_message_id.get())
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
}

#[tokio::test(flavor = "current_thread")]
async fn production_sweep_outbox_contract_dedupes_same_batch_and_separates_new_human() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_004);
    let first_id = message_id_with_age(1, Duration::from_secs(410));
    let second_id = message_id_with_age(2, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel_id, first_id.get() - 1);

    let (first_api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        first_id,
        HUMAN_ID,
        false,
        "첫 사용자 요청",
    )]);
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;
    run_catch_up_sweep(CatchUpDeps::new(&first_api, &shared, &provider)).await;

    let (second_api, _) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        second_id,
        HUMAN_ID,
        false,
        "새 사용자 요청",
    )]);
    let second_api = second_api.with_outbox(Arc::clone(&outbox));
    run_catch_up_sweep(CatchUpDeps::new(&second_api, &shared, &provider)).await;

    let outbox = outbox.lock().expect("outbox capture lock");
    assert_eq!(
        outbox.len(),
        2,
        "same batch dedupes and a new human batch separates"
    );
    for (request, id, snippet) in [
        (&outbox[0], first_id, "첫 사용자 요청"),
        (&outbox[1], second_id, "새 사용자 요청"),
    ] {
        assert_eq!(request.target, format!("channel:{channel_id}"));
        assert!(request.content.contains(snippet));
        assert_eq!(request.bot, "notify");
        assert_eq!(request.source, "catch_up_too_old");
        assert_eq!(request.reason_code, "catch_up.too_old");
        assert_eq!(
            request.session_key,
            format!("catch_up_too_old:{channel_id}:{}", id.get())
        );
    }
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
    shared.settings.write().await.allowed_bot_ids = vec![INFO_BOT_ID];

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

    let (api, outbox) = TestCatchUpApi::new(vec![
        discord_message(
            channel_id,
            bot_id,
            INFO_BOT_ID,
            true,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
        ),
        discord_message(channel_id, human_id, HUMAN_ID, false, "새 작업"),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(bot_id.get()),
        "the aged eligible bot settles silently, but the capacity-blocked human must not"
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
    assert!(
        outbox.lock().expect("outbox capture lock").is_empty(),
        "an aged bot before a capacity-blocked human must settle without a resend notice"
    );
}
