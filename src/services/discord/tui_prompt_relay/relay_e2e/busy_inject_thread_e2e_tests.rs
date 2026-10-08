//! One message reaching a gated parent and its thread: the parent's injection and the thread's
//! promotion each keep the other off it, and nothing else does.

use super::*;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock::user_message;

/// A thread of the fixture channel the mock answers for.
fn thread_of(rt: &Runtime) -> u64 {
    let thread = fresh_id();
    rt.h.add_thread(thread);
    thread
}

/// Production intake of `message` arriving through `thread`.
async fn deliver_in(rt: &Runtime, thread: u64, message: u64, text: &str) {
    let mut arrival = user_message(message, text);
    arrival.channel_id = ChannelId::new(thread);
    rt.h.spawn_message(arrival).await.unwrap().unwrap();
}

/// What the thread did with `message`: posts in it, and whether its mailbox runs or queues it.
async fn thread_took(rt: &Runtime, thread: u64, message: u64) -> (usize, bool) {
    let posts = rt.h.mock.channel_posts.lock().unwrap().clone();
    let posts = posts
        .iter()
        .filter(|(channel, _)| *channel == thread)
        .count();
    let channel = ChannelId::new(thread);
    let snapshot = crate::services::discord::mailbox_snapshot(&rt.h.shared, channel).await;
    let id = MessageId::new(message);
    let queued = snapshot.intervention_queue.iter();
    let held = snapshot.active_user_message_id == Some(id)
        || queued
            .into_iter()
            .any(|item| item.source_message_ids.contains(&id));
    (posts, held)
}

/// A thread arrival while the parent's paste is in progress is refused promotion, whether the
/// gate lists only the parent or the thread too; the parent's paste completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_arrival_is_refused_while_the_parent_pastes_the_message_pg() {
    for thread_listed in [false, true] {
        let busy = busy().await;
        let rt = &busy.rt;
        let thread = thread_of(rt);
        let _thread_gate = thread_listed.then(|| hook::open_gate(thread));
        busy.pane.set("gate", "");
        let message = fresh_id();
        let parent = rt.h.spawn_user_message(message, "status?");
        let at_gate = busy.pane.path("at_gate");
        let gated = wait_until(WAIT, move || {
            let at_gate = at_gate.clone();
            Box::pin(async move { at_gate.exists() })
        });
        assert!(gated.await, "the parent's paste reached the pane");
        deliver_in(rt, thread, message, "status?").await;
        let promotions = hook::seen(message).promotions;
        let took = thread_took(rt, thread, message).await;
        busy.pane.set("go", "");
        parent.await.unwrap().unwrap();
        let observed = (promotions, took, busy.pane.keys().len());
        assert_eq!(
            observed,
            (vec![false], (0, false), 2),
            "thread listed: {thread_listed}"
        );
    }
}

/// A thread that took the message while the parent's arrival waited ahead of its claim keeps
/// it: the parent pastes nothing and falls back to its own intake, as without injection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parent_arrival_after_the_thread_took_the_message_does_not_paste_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    let thread = thread_of(rt);
    let message = fresh_id();
    let (reached, resume) = hook::park_offer(message);
    let parent = rt.h.spawn_user_message(message, "status?");
    tokio::time::timeout(WAIT, reached.notified())
        .await
        .expect("the parent arrival parked ahead of its claim");
    deliver_in(rt, thread, message, "status?").await;
    let took = thread_took(rt, thread, message).await;
    resume.notify_one();
    parent.await.unwrap().unwrap();
    let observed = (
        hook::seen(message).promotions,
        took,
        busy.pane.keys(),
        hook::seen(message).outcomes,
        rt.queue().await,
    );
    let parent_queue = vec!["status?".to_string()];
    assert_eq!(
        observed,
        (vec![true], (1, true), vec![], vec![], parent_queue)
    );
}

/// A thread arrival of a message the parent injected stops: refused promotion while the dedup
/// entry lives, and at the lookup once it is gone, from memory or from the provider file alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_arrival_of_an_injected_message_never_runs_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    let thread = thread_of(rt);
    let pasted = fresh_id();
    rt.h.deliver_user_message(pasted, "status?").await.unwrap();
    let mut observed = Vec::new();
    deliver_in(rt, thread, pasted, "status?").await;
    observed.push((
        "dedup",
        hook::seen(pasted).promotions,
        thread_took(rt, thread, pasted).await,
    ));
    let (channel, now) = (ChannelId::new(CHANNEL_ID), std::time::Instant::now());
    let remembered = MessageId::new(fresh_id());
    let outcome = InjectionOutcome::Observed;
    disposition::note_terminal(
        &ProviderKind::Claude,
        channel,
        Some(remembered),
        outcome,
        now,
    );
    let recorded = MessageId::new(fresh_id());
    let now_ms = chrono::Utc::now().timestamp_millis();
    let record = disposition::record_terminal;
    record(&ProviderKind::Claude, channel, recorded, outcome, now_ms).unwrap();
    for (case, message) in [("memory", remembered), ("disk", recorded)] {
        deliver_in(rt, thread, message.get(), "status?").await;
        let seen = hook::seen(message.get());
        let took = thread_took(rt, thread, message.get()).await;
        observed.push((case, seen.promotions, (took.0 + seen.offers, took.1)));
    }
    let refused = ("dedup", vec![false], (0, false));
    let stopped = |case| (case, vec![], (0, false));
    assert_eq!(observed, [refused, stopped("memory"), stopped("disk")]);
}

/// A parent owner that died leaves no claim on the message, so the thread's arrival is promoted
/// and the thread takes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_takes_a_message_whose_parent_owner_died_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    let thread = thread_of(rt);
    inject_hook::crash_owner(CHANNEL_ID);
    let message = fresh_id();
    rt.h.deliver_user_message(message, "status?").await.unwrap();
    let source = disposition::test_support::source_entry(&ProviderKind::Claude, message);
    deliver_in(rt, thread, message, "status?").await;
    let observed = (
        hook::seen(message).outcomes,
        source,
        hook::seen(message).promotions,
        thread_took(rt, thread, message).await,
    );
    let died = vec!["OwnerFailed { turn_id: None }".to_string()];
    assert_eq!(observed, (died, None, vec![true], (1, true)));
}
