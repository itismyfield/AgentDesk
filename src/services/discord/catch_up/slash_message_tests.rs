use super::*;
use crate::services::discord;

async fn check_slash_sweep(phase2: bool) {
    let root = scoped_runtime_root();
    let shared = discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(8_154_001 + u64::from(phase2));
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
        settings.allowed_bot_ids = vec![INFO_BOT_ID];
    }
    shared
        .known_slash_commands
        .set(HashSet::from(["clear".into()]))
        .unwrap();
    let id = |n, age| message_id_with_age(n, Duration::from_secs(age));
    let bot = id(1, 120);
    let unknown = id(2, 90);
    let longer_name = id(7, 88);
    let spaced = id(8, 85);
    let announce = id(3, 80);
    let ordinary_bot = id(4, 70);
    let command_bot = id(5, 110);
    let clear = id(6, 30);
    let with_args = id(9, 25);
    write_checkpoint(root.path(), &provider, channel, bot.get());
    let messages = vec![
        discord_message(channel, command_bot, CURRENT_BOT_ID, true, "/clear"),
        discord_message(channel, unknown, HUMAN_ID, false, "/unregistered request"),
        discord_message(channel, longer_name, HUMAN_ID, false, "/clearance"),
        discord_message(channel, spaced, HUMAN_ID, false, " /clear"),
        discord_message(channel, announce, ANNOUNCE_BOT_ID, true, "/clear"),
        discord_message(channel, ordinary_bot, INFO_BOT_ID, true, "/clear"),
        discord_message(channel, clear, HUMAN_ID, false, "/clear"),
        discord_message(channel, with_args, HUMAN_ID, false, "/clear args"),
    ];
    let mut history: Vec<_> = messages.iter().rev().cloned().collect();
    history.push(discord_message(
        channel,
        bot,
        CURRENT_BOT_ID,
        true,
        "previous response",
    ));
    let api = TestCatchUpApi::new(Vec::new())
        .with_scripted_fetches(vec![
            Ok(if phase2 { Vec::new() } else { messages }),
            Ok(history.clone()),
            Ok(Vec::new()), // The next After query returns nothing past the checkpoint.
            Ok(history),
        ])
        .with_utility_bot_ids(Some(ANNOUNCE_BOT_ID), Some(NOTIFY_BOT_ID));
    for sweep in 1..=2 {
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
        let queued: Vec<_> = discord::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue
            .iter()
            .map(|row| row.message_id)
            .collect();
        assert_eq!(
            queued,
            vec![unknown, longer_name, spaced, announce],
            "sweep {sweep}: unknown names, leading spaces and announce traffic still recover"
        );
        assert_eq!(
            shared.last_message_ids.get(&channel).map(|id| *id),
            Some(with_args.get()),
            "terminal slash skip advances checkpoint"
        );
        assert!(!shared.catch_up_retry_pending.contains_key(&channel));
        assert!(api.dead_letters.lock().unwrap().is_empty());
    }
    assert_eq!(api.fetch_calls.load(Ordering::Relaxed), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_skips_registered_human_slash_and_recovers_unknown() {
    check_slash_sweep(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_skips_registered_human_slash_and_recovers_unknown() {
    check_slash_sweep(true).await;
}
