//! Phase 2's newest-bot-post boundary against the live intake checkpoint, through the real sweep.

use super::*;

/// An input received but never recorded by intake (lost across a restart) is recovered even
/// when a later bot post, such as a queued placeholder for an earlier input, follows it.
#[tokio::test(flavor = "current_thread")]
async fn a_bot_post_does_not_hide_an_input_intake_never_recorded() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_608_005);
    let (checkpoint, m) = (id(1, 400), id(2, 120));
    let (a, b, placeholder) = (id(3, 60), id(4, 59), id(5, 58));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    fx.shared.last_message_ids.insert(channel_id, a.get());
    let queued = "큐에 들어갔습니다";
    let api = StrictApi::new(&fx.shared)
        .with_history(
            channel_id,
            vec![own_reply(channel_id, checkpoint), human(channel_id, m)],
        )
        .arriving_at(1, human(channel_id, a))
        .arriving_at(1, human(channel_id, b))
        .arriving_at(
            1,
            discord_message(channel_id, placeholder, CURRENT_BOT_ID, true, queued),
        );

    fx.sweep(&api).await;

    let log = api.fetch_log();
    assert_eq!(log[0].2, [m.get()], "phase 1 did not read B");
    assert_eq!(log[1].1, CatchUpFetchRequest::new(20));
    assert_eq!(log[1].2[..2], [placeholder.get(), b.get()]);
    assert_eq!(api.enqueue_log(), [(b.get(), true, None)], "only B, once");
    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    let queued_ids: Vec<_> = (snapshot.intervention_queue.iter())
        .map(|item| item.message_id)
        .collect();
    assert_eq!(queued_ids, [m, b]);
}

/// An input intake already recorded stays answered behind the bot reply, even when a phase-1
/// barrier lowers phase 2's scan checkpoint below it, so a restart does not replay it.
#[tokio::test(flavor = "current_thread")]
async fn an_input_intake_recorded_stays_hidden_behind_the_bot_reply() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_608_006);
    let (old, m, x, reply) = (id(1, 400), id(2, 120), id(3, 60), id(4, 58));
    fx.seed_checkpoint(channel_id, old);
    fx.queue(channel_id, m).await;
    fx.shared.last_message_ids.insert(channel_id, x.get());
    let api = StrictApi::new(&fx.shared)
        .with_history(
            channel_id,
            vec![own_reply(channel_id, old), human(channel_id, m)],
        )
        .arriving_at(1, human(channel_id, x))
        .arriving_at(1, own_reply(channel_id, reply));

    fx.sweep(&api).await;

    let log = api.fetch_log();
    assert_eq!(log[0].2, [m.get()], "phase 1 holds M as its barrier");
    assert_eq!(log[1].2, [reply.get(), x.get(), m.get(), old.get()]);
    assert!(
        api.enqueue_log().is_empty(),
        "X is at the intake checkpoint"
    );
    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    let queued_ids: Vec<_> = (snapshot.intervention_queue.iter())
        .map(|item| item.message_id)
        .collect();
    assert_eq!(queued_ids, [m]);
}
