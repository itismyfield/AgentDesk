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

use super::api::{CatchUpFetchCursor, CatchUpFetchRequest};
use super::settled_frontier::SettledFrontier;
use super::{
    CATCH_UP_RETRY_DEFERRED_REARM_LIMIT, CatchUpClassification, CatchUpClassificationDecision,
    CatchUpDeps, CatchUpDiscordApi, CatchUpMessageView, CatchUpTooOldOutboxRequest, ChannelId,
    MessageId, ProviderKind, RuntimeChannelBindingStatus, catch_up_intervention_text,
    catch_up_source_generation, catch_up_too_old_drop, catch_up_too_old_notice,
    classify_catch_up_message, classify_catch_up_message_with_utility_resolution,
    run_catch_up_sweep,
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
const UNAUTHORIZED_HUMAN_ID: u64 = 343_742_347_365_974_027;
/// #6059: a configured owner distinct from every author these tests send, so a
/// fixture that needs an authorized human keeps the owner precondition without
/// authorizing the author through ownership.
const OWNER_ID: u64 = 343_742_347_365_974_030;

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
    classify_with_resolutions_for_author(view, announce_resolution, notify_resolution, true)
}

/// #6042: the authorization bit is now a classifier input for both catch-up
/// phases, so the sender-ordering tests above keep their pre-#6042 meaning by
/// passing an authorized author; only the gate's own tests pass `false`.
fn classify_with_resolutions_for_author(
    view: &CatchUpMessageView,
    announce_resolution: UtilityBotUserIdResolution,
    notify_resolution: UtilityBotUserIdResolution,
    author_is_authorized: bool,
) -> CatchUpClassificationDecision {
    classify_catch_up_message_with_utility_resolution(
        view,
        Some(CURRENT_BOT_ID),
        &HashSet::new(),
        &HashSet::new(),
        300,
        &[],
        announce_resolution,
        notify_resolution,
        author_is_authorized,
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
fn false_flag_non_allowlist_announce_dispatch_is_not_cancel_preserved() {
    let message = view(
        ANNOUNCE_BOT_ID,
        false,
        60,
        "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
    );
    assert_eq!(
        classify_with_resolutions(
            &message,
            UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::Recover),
    );

    let source = catch_up_source_generation(
        MessageId::new(message.message_id),
        42,
        message.author_id,
        message.author_is_bot,
        &[],
        UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
    );
    assert!(
        !source.preserve_on_cancel,
        "resolved announce identity must override a false Discord bot flag without an allowlist fixture"
    );
}

#[test]
fn unavailable_announce_identity_during_recover_is_not_cancel_preserved() {
    let message = view(HUMAN_ID, false, 60, "계속 진행해");
    assert_eq!(
        classify_with_resolutions(
            &message,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::Recover),
        "announce ambiguity does not defer when both identity alternatives recover"
    );

    let source = catch_up_source_generation(
        MessageId::new(message.message_id),
        43,
        message.author_id,
        message.author_is_bot,
        &[],
        UtilityBotUserIdResolution::Unavailable,
    );
    assert!(
        !source.preserve_on_cancel,
        "an unavailable announce identity is not positive proof of a genuine human"
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
fn catch_up_human_visible_marker_recovers_and_strips_but_bot_marker_is_rejected() {
    let visible_marker = "[origin=operational_alert]please resume my work";
    let human = view(HUMAN_ID, false, 60, visible_marker);
    let bot = view(ANNOUNCE_BOT_ID, true, 60, visible_marker);

    assert_eq!(
        classify_catch_up_message(
            &human,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &HashSet::new(),
            300,
            &[ANNOUNCE_BOT_ID],
            Some(ANNOUNCE_BOT_ID),
            Some(NOTIFY_BOT_ID),
        ),
        CatchUpClassification::Recover,
        "a human's visible provenance literal must not discard catch-up input"
    );
    assert_eq!(
        catch_up_intervention_text(visible_marker, false),
        "please resume my work",
        "catch-up must strip the visible provenance literal before enqueueing human text"
    );
    assert_eq!(
        catch_up_intervention_text(visible_marker, true),
        visible_marker,
        "bot text must retain its marker until the sender policy rejects it"
    );
    assert_eq!(
        classify_catch_up_message(
            &bot,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &HashSet::new(),
            300,
            &[ANNOUNCE_BOT_ID],
            Some(ANNOUNCE_BOT_ID),
            Some(NOTIFY_BOT_ID),
        ),
        CatchUpClassification::NotAllowed,
        "the same marker must continue to reject an announce-bot sender"
    );
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

/// A lone skipped message with no membership arm, as the sweep records it.
fn settled_alone(message_id: u64, outcome: CatchUpClassification) -> Option<u64> {
    let mut frontier = SettledFrontier::default();
    frontier.record_skipped(message_id, outcome, None);
    frontier.newest()
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
        settled_alone(message.message_id, outcome),
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
        settled_alone(message.message_id, outcome),
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
        _request: CatchUpFetchRequest,
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
async fn phase1_false_flag_allowed_dispatch_is_not_cancel_preserved() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_247_001);
    let dispatch_message_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        dispatch_message_id.get() - 1,
    );
    shared.settings.write().await.allowed_bot_ids = vec![INFO_BOT_ID];

    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        dispatch_message_id,
        INFO_BOT_ID,
        false,
        "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(mailbox.intervention_queue.len(), 1);
    assert_eq!(
        mailbox.intervention_queue[0].message_id,
        dispatch_message_id
    );
    assert!(
        !mailbox.intervention_queue[0].preserve_on_cancel(),
        "a bot=false allowed DISPATCH must remain unmarked so cancel drops it like origin/main"
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
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
    assert!(!mailbox.intervention_queue[0].preserve_on_cancel());
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
    // #6042: phase 1 now gates on `author_is_authorized`, and
    // `user_is_authorized` is false for every id under default test settings.
    // Without this the fresh human message classifies `NotAllowed` and the
    // sweep never reaches the behaviour this test exists to pin.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

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
    assert!(mailbox.intervention_queue[0].preserve_on_cancel());
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
    // #6042: phase 1 now gates on `author_is_authorized`, and
    // `user_is_authorized` is false for every id under default test settings.
    // Without this the fresh human message classifies `NotAllowed` and the
    // sweep never reaches the behaviour this test exists to pin.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

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
        super::super::recovery_known_arms_and_ids(&recovered_mailbox).1,
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
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }

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
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }

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
        !mailbox.intervention_queue[0].preserve_on_cancel(),
        "a bot=false allowed DISPATCH remains automation and must retain origin/main cancel-drop behavior"
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
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }

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
        queued_generation: super::runtime_store::process_generation(),
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

/// #5996 / contract I20: phase 2 finds this message only in
/// `intervention_queue`. That membership says the message was accepted for a
/// turn, never that a turn took it, so the skip is right and the checkpoint
/// advance is not: the advance rides `phase2_retry_after_checkpoint` into the
/// retry state and the retry scan then fetches only past it, so a queue entry
/// that is dropped before it drains is never seen again.
#[tokio::test(flavor = "current_thread")]
async fn queue_membership_alone_does_not_advance_the_phase2_checkpoint() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_016);
    // Both catch-up phases pass `author_is_authorized` into
    // `classify_catch_up_message_with_utility_resolution` — #6042 merged the
    // phase-2 gate into that shared path — and `user_is_authorized` is false
    // for every id under default test settings. Without this the fresh human
    // message classifies `NotAllowed`, phase 2 skips it before the capacity
    // gate, and the sweep never reaches the checkpoint decision this test
    // exists to pin.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let queued_id = message_id_with_age(2, Duration::from_secs(120));
    let fresh_id = message_id_with_age(3, Duration::from_secs(30));
    // Registers the channel and puts phase 1 in `After(..)` mode, so phase 1
    // takes fetch call 0 (the empty list) and phase 2 takes call 1. Phase 2's
    // own starting checkpoint comes from the in-memory `last_message_ids`,
    // which an empty phase-1 page leaves untouched.
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get());

    // Fill to capacity with `queued_id` among the entries: phase 2 then skips
    // it as a duplicate and defers on `fresh_id`, and that defer is what
    // publishes the phase-2 checkpoint where this test can read it.
    for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
        let id = if index == 0 {
            queued_id
        } else {
            MessageId::new(8_100_000_000_000_000_000 + index as u64)
        };
        let outcome = super::super::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            queued_intervention(id, index),
        )
        .await;
        assert!(super::catch_up_enqueue_accepted(&outcome));
    }

    let (api, outbox) = TestCatchUpApi::new(Vec::new());
    let api = api.with_phase2_messages(vec![
        discord_message(
            channel_id,
            fresh_id,
            HUMAN_ID,
            false,
            "newer unanswered request",
        ),
        discord_message(
            channel_id,
            queued_id,
            HUMAN_ID,
            false,
            "queued but never dispatched",
        ),
        discord_message(
            channel_id,
            bot_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    // This proves phase 2 issued its own request. It does NOT prove the sweep
    // reached the message loop: the empty-page bail and the "no bot response
    // found" bail both run after this counter moves. The retry assertion below
    // is what pins that the loop ran and stopped where it should.
    assert!(
        api.fetch_calls.load(Ordering::Relaxed) >= 2,
        "phase 2 must have run its own fetch"
    );
    let retry = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .expect("the capacity-blocked fresh message must stay recoverable");
    assert_eq!(
        retry.checkpoint,
        bot_id.get(),
        "queue membership is not evidence of dispatch and must not move the checkpoint"
    );
    assert!(
        retry.checkpoint < queued_id.get(),
        "a checkpoint at or past the queued message forecloses its recovery"
    );
    assert!(
        shared.last_message_ids.get(&channel_id).is_none(),
        "a scan that recovered nothing must not establish a durable frontier"
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
}

/// The other half of the #5996 split: `active_user_message_id` names the
/// message `try_start_turn` stamped onto the slot a turn holds, which IS the
/// dispatch evidence I20 asks for. Removing the advance outright instead of
/// grading it would wedge this scan on a message no rescan can help.
#[tokio::test(flavor = "current_thread")]
async fn an_active_turn_still_advances_the_phase2_checkpoint() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_453_017);
    // Same reason as the sibling test above: phase 2 gates on
    // `author_is_authorized`, so the fresh human message that must reach the
    // capacity gate is otherwise classified `NotAllowed` and skipped.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let active_id = message_id_with_age(2, Duration::from_secs(120));
    let fresh_id = message_id_with_age(3, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get());

    let started = super::super::mailbox_try_start_turn(
        &shared,
        channel_id,
        Arc::new(crate::services::provider::CancelToken::new()),
        serenity::UserId::new(HUMAN_ID),
        active_id,
    )
    .await;
    assert!(started, "the active turn must claim the slot");

    for index in 0..MAX_INTERVENTIONS_PER_CHANNEL {
        let outcome = super::super::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel_id,
            queued_intervention(
                MessageId::new(8_200_000_000_000_000_000 + index as u64),
                index,
            ),
        )
        .await;
        assert!(super::catch_up_enqueue_accepted(&outcome));
    }

    let (api, _outbox) = TestCatchUpApi::new(Vec::new());
    let api = api.with_phase2_messages(vec![
        discord_message(
            channel_id,
            fresh_id,
            HUMAN_ID,
            false,
            "newer unanswered request",
        ),
        discord_message(
            channel_id,
            active_id,
            HUMAN_ID,
            false,
            "the turn currently running",
        ),
        discord_message(
            channel_id,
            bot_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    // This proves phase 2 issued its own request. It does NOT prove the sweep
    // reached the message loop: the empty-page bail and the "no bot response
    // found" bail both run after this counter moves. The retry assertion below
    // is what pins that the loop ran and stopped where it should.
    assert!(
        api.fetch_calls.load(Ordering::Relaxed) >= 2,
        "phase 2 must have run its own fetch"
    );
    let retry = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .expect("the capacity-blocked fresh message must stay recoverable");
    assert_eq!(
        retry.checkpoint,
        active_id.get(),
        "a turn took this message, so the checkpoint is earned and must move"
    );
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
    // #6042: phase 1 now gates on `author_is_authorized`, and
    // `user_is_authorized` is false for every id under default test settings.
    // Without this the fresh human message classifies `NotAllowed` and the
    // sweep never reaches the behaviour this test exists to pin.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

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

// ---------------------------------------------------------------------------
// #4564 durable completed-turn ledger: an already-answered inbound message must
// never be re-flagged "restart-gap TooOld", while a genuinely-undelivered one
// still ages out (no silent loss). The Settled branch is a NEW guard, so each
// test below fails by ASSERT (not a compile error) under the named mutation.
// ---------------------------------------------------------------------------

/// Test 2: an aged inbound message WITH a confirmed terminal delivery on the
/// ledger classifies `Settled`, pre-empting the age gate.
///
/// MUTATION: deleting the `settled_ids.contains(..)` branch in
/// `classify_catch_up_message` makes this return `TooOld` — caught here by the
/// assert, since the `Settled` variant still exists (used by stats + tests) so
/// removing the branch is not a compile error.
#[test]
fn aged_message_on_the_ledger_is_settled_not_too_old() {
    let message = view(HUMAN_ID, false, 3600, "아까 그거 다 됐어?");
    let mut settled = HashSet::new();
    settled.insert(message.message_id);
    assert_eq!(
        classify_catch_up_message(
            &message,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &settled,
            300,
            &[],
            None,
            None,
        ),
        CatchUpClassification::Settled,
        "an aged message on the completed-turn ledger must be Settled, not TooOld"
    );
}

/// Test 3 (#4260 non-regression): a genuine downtime message NOT on the ledger
/// still ages out to `TooOld`.
///
/// MUTATION: widening the `Settled` match to key on channel/age alone (instead
/// of `settled_ids` membership) would wrongly settle this un-answered message —
/// the assert catches it.
#[test]
fn aged_message_absent_from_the_ledger_is_too_old() {
    let message = view(HUMAN_ID, false, 3600, "이거 아직 처리 안 됐지?");
    let mut settled = HashSet::new();
    settled.insert(message.message_id + 1); // a DIFFERENT turn is settled
    assert_eq!(
        classify_catch_up_message(
            &message,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &settled,
            300,
            &[],
            None,
            None,
        ),
        CatchUpClassification::TooOld,
        "a message whose id is not on the ledger must fall through to TooOld"
    );
}

/// Test 5 (P1 silent-loss guard — why #4600 was closed): a `DeliveredCommit`
/// written to the delivery-record frontier must NOT be treated as "settled".
/// ONLY a completed-turn LEDGER append settles. This simulates a crash after the
/// frontier write but before the ledger append: the consult set stays empty and
/// the row still ages to TooOld/DLQ (no false suppression, no silent loss).
// .generation marker 는 Unix 전용 tmux wrapper 만 쓰고 non-unix 의 generation 은 0(불신)이라 generation 에 묶인 durable frontier 경로가 Windows 에는 없다.
#[cfg(unix)]
#[test]
fn delivery_frontier_without_ledger_append_is_not_settled() {
    let _root = scoped_runtime_root();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_564_005);
    let message = view(HUMAN_ID, false, 3600, "크래시 직전에 배달된 답변");
    let tmux_session_name = "AgentDesk-claude-ledger-frontier-only";
    let generation_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap()).unwrap();
    std::fs::write(&generation_path, "test-generation").unwrap();
    let generation_mtime_ns =
        crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
            tmux_session_name,
        );

    // The delivery committed (frontier written)...
    crate::services::discord::outbound::delivery_record::write_delivered_frontier(
        &provider,
        channel_id.get(),
        tmux_session_name,
        crate::services::discord::outbound::delivery_record::DeliveredCommit {
            range: (0, 128),
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: Some(message.message_id),
            panel_channel_id: Some(channel_id.get()),
        },
    )
    .expect("write delivery frontier");

    // ...but the ledger was never appended, so the consult set is empty.
    let settled = crate::services::discord::outbound::completed_turn_ledger::settled_user_msg_ids(
        &provider,
        channel_id.get(),
    );
    assert!(
        !settled.contains(&message.message_id),
        "a delivery frontier must never leak into the settled set (#4600 P1)"
    );
    assert_eq!(
        classify_catch_up_message(
            &message,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &settled,
            300,
            &[],
            None,
            None,
        ),
        CatchUpClassification::TooOld,
        "ledger absence must fall through to TooOld/DLQ (no silent loss)"
    );
}

/// Test 1 (end-to-end): after a restart, an already-answered aged human message
/// on the completed-turn ledger must NOT raise the false restart-gap notice.
///
/// MUTATION: dropping the `settled_ids` consult in the sweep re-flags the
/// message `TooOld`, and the aggregate notice lands in the outbox — the assert
/// on an empty outbox catches it.
#[tokio::test(flavor = "current_thread")]
async fn ledger_suppresses_the_restart_gap_notice_for_an_answered_message() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_564_001);
    let answered = discord_message(
        channel_id,
        message_id_with_age(1, Duration::from_secs(3600)),
        HUMAN_ID,
        false,
        "아까 그거 다 됐어?",
    );
    write_checkpoint(root.path(), &provider, channel_id, answered.id.get() - 1);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }

    // The turn reached terminal delivery before the restart → on the ledger.
    crate::services::discord::outbound::completed_turn_ledger::append_completed_turn(
        &provider,
        channel_id.get(),
        answered.id.get(),
    );

    let (api, outbox) = TestCatchUpApi::new(vec![answered]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        outbox.lock().expect("outbox capture lock").is_empty(),
        "an answered message on the ledger must not raise a restart-gap notice"
    );
}

// ---------------------------------------------------------------------------
// #6042: catch-up phase 1 had no author authorization gate. Phase 2 already
// refused unauthorized authors, so a message that arrived while the relay was
// down could start a turn that the same message could never have started while
// the relay was up. These tests pin the gate BEHAVIOURALLY — queue membership
// and checkpoint motion — rather than on the classifier's return value, so
// cutting the wiring at the phase-1 call site cannot leave them green.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn phase1_unauthorized_human_is_not_enqueued() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_201);
    let human_message_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        human_message_id.get() - 1,
    );

    // Default test settings authorize nobody: `allow_all_users` is false,
    // `owner_user_id` is None, `allowed_user_ids` is empty.
    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        human_message_id,
        HUMAN_ID,
        false,
        "미인가 사용자의 복구 요청",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert!(
        !mailbox
            .intervention_queue
            .iter()
            .any(|intervention| intervention.message_id == human_message_id),
        "an unauthorized author's message must not become recovery work in phase 1"
    );
    assert!(
        mailbox.intervention_queue.is_empty(),
        "the refused message is the only message in the scan"
    );
    // The refusal is terminal, not a deferral: the settled frontier must move
    // past the message so the next sweep does not rescan it forever.
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(human_message_id.get()),
        "a terminally refused message must retire on the durable frontier"
    );
    assert!(
        !shared.catch_up_retry_pending.contains_key(&channel_id),
        "authorization refusal is not an ambiguity and must not arm a retry"
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_authorized_human_is_enqueued() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_202);
    let human_message_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        human_message_id.get() - 1,
    );
    // The other pole of the gate: an authorized author keeps the pre-#6042
    // behaviour exactly. Without this test an inverted gate stays green.
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }

    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        human_message_id,
        HUMAN_ID,
        false,
        "인가된 사용자의 복구 요청",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect::<Vec<_>>(),
        vec![human_message_id],
        "an authorized author must still be recovered exactly as before #6042"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(human_message_id.get())
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_announce_bot_bypasses_authorization() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_203);
    let announce_message_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        announce_message_id.get() - 1,
    );

    // `allowed_bot_ids` stays empty and `allow_all_users` stays false: the only
    // thing that can carry this message past the gate is the announce identity.
    // Automation is authorized by its configured role, never by
    // `user_is_authorized`, so the gate must consult allowance FIRST.
    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        announce_message_id,
        ANNOUNCE_BOT_ID,
        true,
        "PM triage: inspect the stalled workflow",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert_eq!(
        mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect::<Vec<_>>(),
        vec![announce_message_id],
        "the announce identity must start turns without any per-user authorization"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(announce_message_id.get())
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
}

/// The sweep tests above cannot see the difference between `NotAllowed` and any
/// other non-`Recover` outcome: both `continue` and both advance the settled
/// frontier. Only a direct classifier assertion pins which outcome the gate
/// produces, which is what keeps the stats breakdown honest.
#[test]
fn phase1_classification_returns_not_allowed_for_unauthorized_human() {
    let human = view(HUMAN_ID, false, 30, "미인가 사용자의 복구 요청");
    assert_eq!(
        classify_with_resolutions_for_author(
            &human,
            UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID),
            UtilityBotUserIdResolution::Resolved(NOTIFY_BOT_ID),
            false,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::NotAllowed),
        "an unauthorized human must classify exactly NotAllowed, not some other terminal skip"
    );
}

/// #6042 left phase 2 unchanged in behaviour, but the repo pinned that
/// behaviour nowhere: before this test, replacing the phase-2 call site's
/// authorization argument with `true` broke nothing.
#[tokio::test(flavor = "current_thread")]
async fn phase2_unauthorized_human_is_not_enqueued() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_204);
    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let fresh_id = message_id_with_age(2, Duration::from_secs(30));
    // Registers the channel and puts phase 1 in `After(..)` mode, so phase 1
    // takes fetch call 0 (the empty list) and phase 2 takes call 1.
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get());

    let (api, outbox) = TestCatchUpApi::new(Vec::new());
    let api = api.with_phase2_messages(vec![
        discord_message(
            channel_id,
            fresh_id,
            HUMAN_ID,
            false,
            "미인가 사용자의 phase 2 요청",
        ),
        discord_message(
            channel_id,
            bot_id,
            CURRENT_BOT_ID,
            true,
            "previous bot response",
        ),
    ]);

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        api.fetch_calls.load(Ordering::Relaxed) >= 2,
        "phase 2 must have run its own fetch"
    );
    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    assert!(
        mailbox.intervention_queue.is_empty(),
        "phase 2 must keep refusing an unauthorized author"
    );
    assert!(outbox.lock().expect("outbox capture lock").is_empty());
}

// #6059: live intake refuses every human while `owner_user_id` is unset, even
// in allow-all mode or for an allow-listed id; catch-up used to accept them.
// Each scenario runs the ownerless refusal and an owner-only control on fresh
// state, so the refusal cannot pass because the sweep never classified the
// message, and the control cannot fail because the refusal moved a checkpoint.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum OwnerlessGrant {
    AllowAll,
    Listed,
}

async fn grant_human(shared: &super::SharedData, grant: OwnerlessGrant, owner: Option<u64>) {
    let mut settings = shared.settings.write().await;
    settings.owner_user_id = owner;
    match grant {
        OwnerlessGrant::AllowAll => settings.allow_all_users = true,
        OwnerlessGrant::Listed => settings.allowed_user_ids = vec![HUMAN_ID],
    }
}

struct OwnerGateSweep {
    human_message_id: MessageId,
    queued: Vec<MessageId>,
    last_message_id: Option<u64>,
    retry_pending: bool,
    outbox_empty: bool,
}

async fn phase1_owner_gate_sweep(
    channel_id: ChannelId,
    grant: OwnerlessGrant,
    owner: Option<u64>,
) -> OwnerGateSweep {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let human_message_id = message_id_with_age(1, Duration::from_secs(30));
    write_checkpoint(
        root.path(),
        &provider,
        channel_id,
        human_message_id.get() - 1,
    );
    grant_human(&shared, grant, owner).await;

    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        human_message_id,
        HUMAN_ID,
        false,
        "owner 미설정 구성의 복구 요청",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    let outbox_empty = outbox.lock().expect("outbox capture lock").is_empty();
    OwnerGateSweep {
        human_message_id,
        queued: mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect(),
        last_message_id: shared.last_message_ids.get(&channel_id).map(|id| *id),
        retry_pending: shared.catch_up_retry_pending.contains_key(&channel_id),
        outbox_empty,
    }
}

async fn phase2_owner_gate_sweep(
    channel_id: ChannelId,
    grant: OwnerlessGrant,
    owner: Option<u64>,
) -> OwnerGateSweep {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let bot_id = message_id_with_age(1, Duration::from_secs(300));
    let human_message_id = message_id_with_age(2, Duration::from_secs(30));
    // Phase 1 takes fetch call 0 (the empty list) and phase 2 takes call 1.
    write_checkpoint(root.path(), &provider, channel_id, bot_id.get());
    grant_human(&shared, grant, owner).await;

    let (api, outbox) = TestCatchUpApi::new(Vec::new());
    let api = api
        .with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID))
        .with_phase2_messages(vec![
            discord_message(
                channel_id,
                human_message_id,
                HUMAN_ID,
                false,
                "owner 미설정 구성의 phase 2 요청",
            ),
            discord_message(
                channel_id,
                bot_id,
                CURRENT_BOT_ID,
                true,
                "previous bot response",
            ),
        ]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    assert!(
        api.fetch_calls.load(Ordering::Relaxed) >= 2,
        "phase 2 must have run its own fetch"
    );

    let mailbox = super::super::mailbox_snapshot(&shared, channel_id).await;
    let outbox_empty = outbox.lock().expect("outbox capture lock").is_empty();
    OwnerGateSweep {
        human_message_id,
        queued: mailbox
            .intervention_queue
            .iter()
            .map(|intervention| intervention.message_id)
            .collect(),
        last_message_id: shared.last_message_ids.get(&channel_id).map(|id| *id),
        retry_pending: shared.catch_up_retry_pending.contains_key(&channel_id),
        outbox_empty,
    }
}

async fn assert_phase1_owner_gate(grant: OwnerlessGrant, channel_id: u64) {
    let refused = phase1_owner_gate_sweep(ChannelId::new(channel_id), grant, None).await;
    assert!(
        refused.queued.is_empty(),
        "{grant:?} without an owner must not become phase-1 recovery work, got {:?}",
        refused.queued
    );
    // Same terminal refusal contract as `phase1_unauthorized_human_is_not_enqueued`.
    assert_eq!(
        refused.last_message_id,
        Some(refused.human_message_id.get()),
        "{grant:?}: a terminally refused message must retire on the settled frontier"
    );
    assert!(
        !refused.retry_pending,
        "{grant:?}: authorization refusal must not arm a retry"
    );
    assert!(refused.outbox_empty, "{grant:?}: refusal must not notify");

    let control =
        phase1_owner_gate_sweep(ChannelId::new(channel_id + 1), grant, Some(OWNER_ID)).await;
    assert_eq!(
        control.queued,
        vec![control.human_message_id],
        "{grant:?} with only the owner added must recover the same message in phase 1"
    );
    assert!(control.outbox_empty);
}

async fn assert_phase2_owner_gate(grant: OwnerlessGrant, channel_id: u64) {
    let refused = phase2_owner_gate_sweep(ChannelId::new(channel_id), grant, None).await;
    assert!(
        refused.queued.is_empty(),
        "{grant:?} without an owner must not become phase-2 recovery work, got {:?}",
        refused.queued
    );
    assert!(refused.outbox_empty, "{grant:?}: refusal must not notify");

    let control =
        phase2_owner_gate_sweep(ChannelId::new(channel_id + 1), grant, Some(OWNER_ID)).await;
    assert_eq!(
        control.queued,
        vec![control.human_message_id],
        "{grant:?} with only the owner added must recover the same message in phase 2"
    );
    assert!(control.outbox_empty);
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_ownerless_allow_all_human_is_not_enqueued() {
    assert_phase1_owner_gate(OwnerlessGrant::AllowAll, 4_605_901).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_ownerless_listed_human_is_not_enqueued() {
    assert_phase1_owner_gate(OwnerlessGrant::Listed, 4_605_903).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_ownerless_allow_all_human_is_not_enqueued() {
    assert_phase2_owner_gate(OwnerlessGrant::AllowAll, 4_605_905).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_ownerless_listed_human_is_not_enqueued() {
    assert_phase2_owner_gate(OwnerlessGrant::Listed, 4_605_907).await;
}

// Unauthorized aged humans get neither the TooOld resend notice (which echoes
// author id + snippet) nor a DLQ record.

#[tokio::test(flavor = "current_thread")]
async fn phase1_unauthorized_human_too_old_is_neither_noticed_nor_dead_lettered() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_205);
    let aged_id = message_id_with_age(1, Duration::from_secs(3_600));
    write_checkpoint(root.path(), &provider, channel_id, aged_id.get() - 1);

    // Default settings authorize nobody.
    let (api, outbox) = TestCatchUpApi::new(vec![discord_message(
        channel_id,
        aged_id,
        UNAUTHORIZED_HUMAN_ID,
        false,
        "미인가 사용자의 오래된 요청",
    )]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        outbox.lock().expect("outbox capture lock").is_empty(),
        "an unauthorized author's id and content must not be echoed into the channel"
    );
    assert!(
        api.dead_letters
            .lock()
            .expect("dead-letter capture lock")
            .is_empty(),
        "an unauthorized author's content must not be persisted to the DLQ"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(aged_id.get()),
        "the refusal is terminal and still retires on the durable frontier"
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
    assert!(
        super::super::mailbox_snapshot(&shared, channel_id)
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_authorized_human_too_old_keeps_notice_and_dead_letter_per_author() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_206);
    let authorized_id = message_id_with_age(1, Duration::from_secs(3_600));
    let unauthorized_id = message_id_with_age(2, Duration::from_secs(3_500));
    write_checkpoint(root.path(), &provider, channel_id, authorized_id.get() - 1);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allowed_user_ids = vec![HUMAN_ID];
    }

    let (api, outbox) = TestCatchUpApi::new(vec![
        discord_message(channel_id, authorized_id, HUMAN_ID, false, "인가된 요청"),
        discord_message(
            channel_id,
            unauthorized_id,
            UNAUTHORIZED_HUMAN_ID,
            false,
            "미인가 요청",
        ),
    ]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        *outbox.lock().expect("outbox capture lock"),
        vec![CatchUpTooOldOutboxRequest {
            target: format!("channel:{channel_id}"),
            content: format!(
                "⚠️ 재시작 공백으로 1건이 5분 초과로 미처리되었습니다. 필요하면 다시 보내주세요:\n• `{HUMAN_ID}`: 인가된 요청"
            ),
            bot: "notify",
            source: "catch_up_too_old",
            reason_code: "catch_up.too_old",
            session_key: format!("catch_up_too_old:{channel_id}:{}", authorized_id.get()),
        }],
        "only the authorized author enters the notice, and owns the batch key"
    );
    let dead_letters: Vec<_> = api
        .dead_letters
        .lock()
        .expect("dead-letter capture lock")
        .iter()
        .map(|record| (record.author_id.clone(), record.content.clone()))
        .collect();
    assert_eq!(
        dead_letters,
        vec![(Some(HUMAN_ID.to_string()), "인가된 요청".to_string())],
        "only the authorized author's TooOld is dead-lettered"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(unauthorized_id.get()),
        "both aged messages settle as one contiguous prefix"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_allowed_automation_too_old_is_dead_lettered_without_authorization() {
    let root = scoped_runtime_root();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(4_604_207);
    let announce_id = message_id_with_age(1, Duration::from_secs(3_600));
    let allowed_id = message_id_with_age(2, Duration::from_secs(3_500));
    write_checkpoint(root.path(), &provider, channel_id, announce_id.get() - 1);
    // No user is authorized; automation is allowed only by its configured role.
    shared.settings.write().await.allowed_bot_ids = vec![INFO_BOT_ID];

    let (api, outbox) = TestCatchUpApi::new(vec![
        discord_message(
            channel_id,
            announce_id,
            ANNOUNCE_BOT_ID,
            true,
            "PM triage: inspect the stalled workflow",
        ),
        discord_message(
            channel_id,
            allowed_id,
            INFO_BOT_ID,
            true,
            "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000",
        ),
    ]);
    let api = api.with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert!(
        outbox.lock().expect("outbox capture lock").is_empty(),
        "automation TooOld stays internal evidence"
    );
    let dead_letters: Vec<_> = api
        .dead_letters
        .lock()
        .expect("dead-letter capture lock")
        .iter()
        .map(|record| record.author_id.clone())
        .collect();
    assert_eq!(
        dead_letters,
        vec![
            Some(ANNOUNCE_BOT_ID.to_string()),
            Some(INFO_BOT_ID.to_string())
        ],
        "allowed automation keeps its TooOld DLQ evidence without user authorization"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(allowed_id.get())
    );
}

/// Notify-only unavailability settles (the refusal is identity-independent);
/// announce unavailability still defers because announce bypasses auth.
#[test]
fn aged_unauthorized_human_classifies_not_allowed_across_identity_states() {
    let aged = view(
        UNAUTHORIZED_HUMAN_ID,
        false,
        3_600,
        "미인가 사용자의 오래된 요청",
    );
    let resolved = UtilityBotUserIdResolution::Resolved(ANNOUNCE_BOT_ID);
    let not_allowed = CatchUpClassificationDecision::Determinate(CatchUpClassification::NotAllowed);
    assert_eq!(
        classify_with_resolutions_for_author(
            &aged,
            resolved,
            UtilityBotUserIdResolution::Resolved(NOTIFY_BOT_ID),
            false,
        ),
        not_allowed
    );
    assert_eq!(
        classify_with_resolutions_for_author(
            &aged,
            resolved,
            UtilityBotUserIdResolution::Unavailable,
            false,
        ),
        not_allowed
    );
    assert_eq!(
        classify_with_resolutions_for_author(
            &aged,
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
            false,
        ),
        CatchUpClassificationDecision::UtilityIdentityUnavailable
    );
    assert_eq!(
        classify_with_resolutions_for_author(
            &aged,
            resolved,
            UtilityBotUserIdResolution::Resolved(NOTIFY_BOT_ID),
            true,
        ),
        CatchUpClassificationDecision::Determinate(CatchUpClassification::TooOld)
    );
}

#[path = "frontier_sweep_tests.rs"]
mod frontier_sweep_tests;
