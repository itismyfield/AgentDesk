//! A `!` text command this bot already replied to is never replayed into the
//! provider queue; unanswered commands and every other input recover as before.

use super::*;
use crate::services::discord;

const DISPATCH: &str = "DISPATCH:1f3c2b1a-0000-4000-8000-000000000000";

fn reply_to(channel_id: ChannelId, id: MessageId, to: MessageId, text: &str) -> serenity::Message {
    reply_from(CURRENT_BOT_ID, channel_id, id, to, text)
}

fn reply_from(
    author_id: u64,
    channel_id: ChannelId,
    id: MessageId,
    to: MessageId,
    text: &str,
) -> serenity::Message {
    let mut reply = discord_message(channel_id, id, author_id, true, text);
    reply.message_reference = Some(serenity::MessageReference::from((channel_id, to)));
    reply
}

async fn queued_sources(shared: &discord::SharedData, channel_id: ChannelId) -> Vec<MessageId> {
    discord::mailbox_snapshot(shared, channel_id)
        .await
        .intervention_queue
        .iter()
        .flat_map(|intervention| intervention.source_message_ids.clone())
        .collect()
}

async fn authorize(shared: &discord::SharedData) {
    let mut settings = shared.settings.write().await;
    settings.owner_user_id = Some(OWNER_ID);
    settings.allow_all_users = true;
    settings.allowed_bot_ids = vec![INFO_BOT_ID];
}

/// Replied `!clear`/`!stop`/mention-led `!pwd` were consumed live; a reply to another
/// message or from another bot is no evidence for `!status`.
#[tokio::test(flavor = "current_thread")]
async fn sweep_skips_replied_commands_and_recovers_everything_else() {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(4_655_301);
    let id = |sequence: u64, age: u64| message_id_with_age(sequence, Duration::from_secs(age));
    let (clear, cleared) = (id(1, 90), id(2, 88));
    let (stop, stopped) = (id(3, 80), id(4, 79));
    let (status, other_bot_answer) = (id(5, 70), id(12, 69));
    let (plain, plain_answer) = (id(6, 60), id(7, 59));
    let (mentioned, pwd_answer) = (id(8, 50), id(9, 49));
    let (slash, dispatch) = (id(10, 40), id(11, 30));
    write_checkpoint(root.path(), &provider, channel_id, clear.get() - 1);

    let history = vec![
        discord_message(channel_id, clear, ANNOUNCE_BOT_ID, true, "!clear [E2E]"),
        reply_to(channel_id, cleared, clear, "세션을 초기화했어요."),
        discord_message(channel_id, stop, HUMAN_ID, false, "!stop"),
        reply_to(channel_id, stopped, stop, "중지할 진행 중인 작업이 없어요."),
        discord_message(channel_id, status, HUMAN_ID, false, "!status"),
        reply_from(
            INFO_BOT_ID,
            channel_id,
            other_bot_answer,
            status,
            "다른 봇의 답",
        ),
        discord_message(channel_id, plain, HUMAN_ID, false, "계속 진행해"),
        reply_to(channel_id, plain_answer, plain, "진행할게요."),
        discord_message(channel_id, mentioned, HUMAN_ID, false, "<@9001> !pwd"),
        reply_to(channel_id, pwd_answer, mentioned, "`/tmp/ws`"),
        discord_message(channel_id, slash, HUMAN_ID, false, "/model gpt"),
        discord_message(channel_id, dispatch, INFO_BOT_ID, true, DISPATCH),
    ];
    let newest_first = history.iter().rev().cloned().collect();
    let (api, _outbox) = TestCatchUpApi::new(history);
    let api = api
        .with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID))
        .with_phase2_messages(newest_first);

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(
        queued_sources(&shared, channel_id).await,
        vec![status, plain, slash, dispatch],
        "replied commands are consumed; unanswered commands and other input recover"
    );
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(dispatch.get())
    );
    assert!(!shared.catch_up_retry_pending.contains_key(&channel_id));
}

/// Without this bot's identity a reply cannot be recognized, so the command
/// is neither replayed nor skipped: the frontier stops before it and retries.
#[tokio::test(flavor = "current_thread")]
async fn sweep_defers_a_command_when_own_identity_is_unknown() {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    authorize(&shared).await;
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(4_655_302);
    let id = |sequence: u64, age: u64| message_id_with_age(sequence, Duration::from_secs(age));
    let (plain, clear, cleared, later) = (id(1, 90), id(2, 80), id(3, 79), id(4, 70));
    write_checkpoint(root.path(), &provider, channel_id, plain.get() - 1);

    let (mut api, _outbox) = TestCatchUpApi::new(vec![
        discord_message(channel_id, plain, HUMAN_ID, false, "먼저 이것"),
        discord_message(channel_id, clear, HUMAN_ID, false, "!clear"),
        reply_to(channel_id, cleared, clear, "세션을 초기화했어요."),
        discord_message(channel_id, later, HUMAN_ID, false, "그 다음"),
    ]);
    api.current_user_id = None;

    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;

    assert_eq!(queued_sources(&shared, channel_id).await, vec![plain]);
    assert_eq!(
        shared.last_message_ids.get(&channel_id).map(|id| *id),
        Some(plain.get()),
        "the checkpoint must stay before the undecided command"
    );
    let pending = shared
        .catch_up_retry_pending
        .get(&channel_id)
        .map(|state| state.checkpoint);
    assert_eq!(pending, Some(plain.get()));
}
