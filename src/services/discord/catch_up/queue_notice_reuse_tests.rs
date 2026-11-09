//! A recovered message that already has this bot's queue-hold notice reply in the channel, which a
//! node on the same bot account may have posted, gets no second notice; any other recovery does.
use super::*;
use crate::services::discord::queue_io::input_ingress::Hold;

fn notice(channel_id: ChannelId, message_id: MessageId, to: MessageId) -> serenity::Message {
    let text = Hold::RestartRecovery.notice();
    let mut notice = discord_message(channel_id, message_id, CURRENT_BOT_ID, true, &text);
    notice.message_reference = Some(serenity::MessageReference::from((channel_id, to)));
    notice
}

/// One real sweep into an adopted owner while restart recovery holds arrivals: the REST posts it
/// made and what it enqueued. `seen` is visible to phase 1; `later` arrives for phase 2 only.
async fn sweep(
    channel_id: ChannelId,
    checkpoint: MessageId,
    seen: Vec<serenity::Message>,
    later: Vec<serenity::Message>,
) -> (usize, Vec<EnqueueRecord>, Vec<u64>) {
    let fx = Fixture::new().await;
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    fx.seed_checkpoint(channel_id, checkpoint);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&fx.shared.token_hash)
        .join(format!("{channel_id}.json"));
    let handle = fx.shared.mailbox(channel_id);
    handle
        .v2_adopt(path, fx.shared.token_hash.clone())
        .await
        .unwrap();
    let restart = &fx.shared.restart.reconcile_done;
    restart.store(false, std::sync::atomic::Ordering::SeqCst);
    let rest = crate::services::discord::shared_state::test_rest::recording_mock;
    let (log, _rest) = rest(7_800_100, channel_id.get()).await;
    let history = std::iter::once(own_reply(channel_id, checkpoint)).chain(seen);
    let mut api = StrictApi::new(&fx.shared).with_history(channel_id, history.collect());
    for message in later {
        api = api.arriving_at(1, message);
    }
    fx.sweep(&api).await;
    let posts = (log.lock().unwrap().iter())
        .filter(|(method, _)| method == "POST")
        .count();
    let phase1 = api.fetch_log()[0].2.clone();
    (posts, api.enqueue_log(), phase1)
}

#[tokio::test(flavor = "current_thread")]
async fn phase_one_recovery_reuses_this_bots_notice_reply_to_the_message() {
    let channel_id = ChannelId::new(7_800_101);
    let (checkpoint, q, posted) = (id(1, 400), id(2, 120), id(3, 100));
    let seen = vec![human(channel_id, q), notice(channel_id, posted, q)];
    let (posts, enqueued, phase1) = sweep(channel_id, checkpoint, seen, vec![]).await;
    assert_eq!(phase1, [posted.get(), q.get()], "phase 1 read both");
    assert_eq!(
        enqueued,
        [(q.get(), true, None)],
        "phase 1 recovers it once"
    );
    assert_eq!(posts, 0, "its notice is already in the channel");
}

#[tokio::test(flavor = "current_thread")]
async fn phase_one_recovery_without_a_notice_reply_posts_one() {
    let channel_id = ChannelId::new(7_800_102);
    let (checkpoint, q) = (id(1, 400), id(2, 120));
    let seen = vec![human(channel_id, q)];
    let (posts, enqueued, _) = sweep(channel_id, checkpoint, seen, vec![]).await;
    assert_eq!(enqueued, [(q.get(), true, None)]);
    assert_eq!(posts, 1, "the held arrival is acknowledged once");
}

#[tokio::test(flavor = "current_thread")]
async fn a_notice_reply_to_another_message_does_not_silence_the_recovered_one() {
    let channel_id = ChannelId::new(7_800_103);
    let (checkpoint, q, posted) = (id(1, 400), id(2, 120), id(3, 100));
    let seen = vec![human(channel_id, q), notice(channel_id, posted, checkpoint)];
    let (posts, enqueued, _) = sweep(channel_id, checkpoint, seen, vec![]).await;
    assert_eq!(enqueued, [(q.get(), true, None)]);
    assert_eq!(posts, 1, "the notice answers another message");
}

/// Phase 2 reads notice replies from its whole page, past the newest-bot-post slice boundary.
#[tokio::test(flavor = "current_thread")]
async fn phase_two_recovery_reuses_only_the_notice_reply_to_the_message() {
    for (seed, reply_to_q, expected) in [(7_800_104, true, 0), (7_800_105, false, 1)] {
        let channel_id = ChannelId::new(seed);
        let (checkpoint, q, posted) = (id(1, 400), id(2, 120), id(3, 100));
        let to = if reply_to_q { q } else { checkpoint };
        let later = vec![human(channel_id, q), notice(channel_id, posted, to)];
        let (posts, enqueued, phase1) = sweep(channel_id, checkpoint, vec![], later).await;
        assert!(phase1.is_empty(), "phase 1 did not read it");
        assert_eq!(
            enqueued,
            [(q.get(), true, None)],
            "phase 2 recovers it once"
        );
        assert_eq!(posts, expected, "notice reply to the message: {reply_to_q}");
    }
}
