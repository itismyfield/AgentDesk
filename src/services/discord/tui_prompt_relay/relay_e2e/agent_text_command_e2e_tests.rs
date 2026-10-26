//! Agents post through the announce bot and cannot run slash commands: their `!clear`,
//! `!restart` and `!stop` work through intake, a removed name only gets an answer, other `!` is chat.

use std::sync::atomic::Ordering;
use std::time::Duration;

use poise::serenity_prelude as serenity;

use super::stop_command_catch_up_e2e::{assert_no_unhandled, hold_a_turn, recent_snowflake_base};
use super::{RelayE2eHarness, wait_until};
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::host_defer_gate::tests::ScriptedTmux;

/// The announce bot, which agent-to-agent sends post as.
const ANNOUNCE_ID: u64 = 940_487_400_000_021;

async fn harness_with_announce() -> RelayE2eHarness {
    let harness = RelayE2eHarness::start_with_health_registry().await;
    let registry = harness.health_registry.clone().expect("a health registry");
    registry
        .set_utility_bot_user_id_for_tests(UtilityBotRole::Announce, ANNOUNCE_ID)
        .await;
    harness.register_channel_in_role_map();
    harness
}

/// Production intake of `text` posted by the announce bot, run to completion.
async fn from_announce(harness: &RelayE2eHarness, message: u64, text: &str) {
    let mut arrival = super::discord_mock::user_message(message, text);
    arrival.author.id = serenity::UserId::new(ANNOUNCE_ID);
    arrival.author.name = "announce-bot".to_string();
    arrival.author.bot = true;
    harness
        .spawn_message(arrival)
        .await
        .expect("intake task")
        .expect("intake result");
}

async fn session_id(harness: &RelayE2eHarness) -> Option<String> {
    let core = harness.shared.core.lock().await;
    let channel = serenity::ChannelId::new(super::CHANNEL_ID);
    core.sessions.get(&channel)?.session_id.clone()
}

/// Whether `text` reached the provider stub as turn input within 15s.
async fn reaches_provider(harness: &RelayE2eHarness, text: &str) -> bool {
    let root = harness.root.path().to_path_buf();
    let text = text.to_string();
    wait_until(Duration::from_secs(15), move || {
        let inputs = std::fs::read_to_string(root.join(super::PROVIDER_INPUTS_FILE));
        let reached = inputs.is_ok_and(|inputs| inputs.contains(&text));
        Box::pin(async move { reached })
    })
    .await
}

fn replies_to(harness: &RelayE2eHarness, message: u64) -> Vec<String> {
    harness
        .messages()
        .into_iter()
        .filter(|(reply_to, _)| *reply_to == Some(message))
        .map(|(_, text)| text)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_bot_stop_interrupts_the_active_turn() {
    let harness = harness_with_announce().await;
    let base = recent_snowflake_base();
    let (_held, token) = hold_a_turn(&harness, base | 1).await;

    from_announce(&harness, base | 2, "!stop").await;

    assert!(
        token.cancelled.load(Ordering::SeqCst),
        "the agent's stop cancels the turn"
    );
    assert_no_unhandled(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_bot_clear_resets_the_session() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let _tmux = ScriptedTmux::install();
    assert!(
        session_id(&harness).await.is_some(),
        "fixture binds a session"
    );
    let clear = recent_snowflake_base() | 1;

    from_announce(&harness, clear, "!clear").await;

    assert_eq!(
        session_id(&harness).await,
        None,
        "the agent's clear drops the session"
    );
    let replies = replies_to(&harness, clear);
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(
        replies[0].starts_with(crate::services::discord::commands::SESSION_CLEARED_RESPONSE),
        "{replies:?}"
    );
    assert_no_unhandled(&harness);
}

/// The classifier swap keeps `!clear` a handled command: a mention-led `!clear` resets the
/// session and nothing of it reaches chat, the mailbox or the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clear_stays_a_command_and_never_reaches_chat() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let _tmux = ScriptedTmux::install();
    let clear = recent_snowflake_base() | 1;
    let text = format!("<@{}> !clear", super::discord_mock::BOT_ID);

    harness
        .deliver_user_message(clear, &text)
        .await
        .expect("live intake handles the command");

    assert_eq!(session_id(&harness).await, None, "the clear ran");
    let mailbox = harness.mailbox().await;
    assert!(mailbox.cancel_token.is_none(), "no turn started");
    assert!(mailbox.intervention_queue.is_empty(), "nothing queued");
    assert_eq!(
        harness.placeholder_posts(),
        0,
        "no chat turn was dispatched"
    );
    assert_eq!(harness.provider_starts(), 0);
    assert_eq!(replies_to(&harness, clear).len(), 1);
    assert_no_unhandled(&harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_bot_restart_restarts_the_session() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let _tmux = ScriptedTmux::install();
    let restart = recent_snowflake_base() | 1;

    from_announce(&harness, restart, "!restart").await;

    let replies = replies_to(&harness, restart);
    assert_eq!(
        replies.first().map(String::as_str),
        Some("♻ 세션 재시작 중...")
    );
    let result = replies.last().expect("a restart result");
    assert!(
        result.starts_with("♻ 세션 재시작 요청 결과 (provider=claude)."),
        "{replies:?}"
    );
    assert!(
        result.contains("인사말 turn이 시작됐습니다."),
        "{replies:?}"
    );
    let seeded = wait_until(Duration::from_secs(15), {
        let root = harness.root.path().to_path_buf();
        move || {
            let inputs = std::fs::read_to_string(root.join(super::PROVIDER_INPUTS_FILE));
            let seeded = inputs.is_ok_and(|inputs| inputs.contains("세션 재시작 완료"));
            Box::pin(async move { seeded })
        }
    })
    .await;
    assert!(seeded, "the restart's seed turn reaches the provider");
    assert_no_unhandled(&harness);
}

/// A removed command from the agent runs nothing: one answer naming the slash command,
/// no provider turn, and no settings, session or host change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_bot_removed_commands_only_get_an_answer() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let root = harness.root.path().to_path_buf();
    let marker = root.join("shell-ran");
    let shell = format!("!shell touch {}", marker.display());
    let cases = [
        ("!pwd", Some("/pwd")),
        ("!help", Some("/help")),
        ("!health", Some("/health")),
        ("!status", Some("/status")),
        ("!inflight", Some("/inflight")),
        ("!queue all", Some("/queue")),
        ("!usage", Some("/usage")),
        ("!receipt", Some("/usage")),
        ("!skill review", Some("/skill")),
        ("!cc review", Some("/skill")),
        ("!model opus", Some("/model")),
        ("!fast on", Some("/fast")),
        ("!goals on", Some("/goals")),
        ("!allowall off", Some("/allowall")),
        ("!vc join", Some("/voice")),
        ("!metrics", None),
        ("!allowedtools", None),
        ("!sessions", None),
        ("!start /", None),
        ("!down /etc/hosts", None),
        ("!meeting agenda", None),
        ("!deletesession x", None),
        ("!debug", None),
        (shell.as_str(), None),
        ("!allowed +Bash", None),
        ("!deadlock-recover", None),
        ("!machine-flip peer", None),
        ("!stuck-pr-rebase", None),
        ("!adduser 1", None),
        ("!removeuser 1", None),
    ];
    let settings_before = harness.shared.settings.read().await.clone();
    let path = async |harness: &RelayE2eHarness| {
        let core = harness.shared.core.lock().await;
        let session = core
            .sessions
            .get(&serenity::ChannelId::new(super::CHANNEL_ID));
        session.map(|session| (session.current_path.clone(), session.session_id.clone()))
    };
    let session_before = path(&harness).await;
    let base = recent_snowflake_base();
    for (n, (text, replacement)) in cases.iter().enumerate() {
        let message = base | (n as u64 + 1);
        from_announce(&harness, message, text).await;
        let name = text.split_whitespace().next().unwrap();
        let expected = match replacement {
            Some(slash) => format!("`{name}` 텍스트 명령은 없어졌어요. `{slash}` 커맨드를 쓰세요."),
            None => format!("`{name}` 텍스트 명령은 없어졌어요."),
        };
        assert_eq!(replies_to(&harness, message), vec![expected], "{text}");
    }
    assert_eq!(
        harness.provider_starts(),
        0,
        "no removed command became a turn"
    );
    let settings_after = harness.shared.settings.read().await.clone();
    assert_eq!(
        settings_after.allow_all_users,
        settings_before.allow_all_users
    );
    assert_eq!(
        settings_after.allowed_user_ids,
        settings_before.allowed_user_ids
    );
    assert_eq!(path(&harness).await, session_before);
    assert!(!marker.exists(), "no shell ran");
    assert_eq!(
        harness.messages().len(),
        cases.len(),
        "only the answers were posted"
    );
    assert_no_unhandled(&harness);
}

/// `!` text that is not a command word is ordinary chat, from the agent as from anyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_bot_other_bang_text_is_chat() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let message = recent_snowflake_base() | 1;

    from_announce(&harness, message, "!clearall the cache, please").await;

    assert!(
        reaches_provider(&harness, "!clearall the cache, please").await,
        "the text reaches the provider: {:?}",
        harness.messages()
    );
    assert!(
        replies_to(&harness, message).is_empty(),
        "{:?}",
        harness.messages()
    );
    assert_no_unhandled(&harness);
}

/// With no Herdr consent endpoint for the channel, the owner's `!herdr consent retry` is
/// not handled here and goes on as chat instead of being swallowed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unserved_consent_retry_is_chat() {
    let harness = harness_with_announce().await;
    harness.answer_placeholders_immediately();
    let message = recent_snowflake_base() | 1;

    harness
        .deliver_user_message(message, "!herdr consent retry")
        .await
        .expect("live intake");

    assert!(
        reaches_provider(&harness, "!herdr consent retry").await,
        "the retry reaches the provider: {:?}",
        harness.messages()
    );
    assert!(
        replies_to(&harness, message).is_empty(),
        "{:?}",
        harness.messages()
    );
    assert_no_unhandled(&harness);
}
