//! A preserve stop of a streaming turn whose host marker cannot be read, through the real
//! watcher loop: the stop is kept before any write, so the watcher still delivers the tail.

use super::*;

const T0: &str = "ADK-P6ASB T0 delivered before the watcher attached";
const HEAD: &str = "ADK-P6ASB T1 head streamed before the stop";
const TAIL: &str = "ADK-P6ASB T1 tail written after the stop was kept";

// A /turns cancel of that turn is kept and cancels nothing: the watcher posts the tail exactly
// once, with no missing bytes, and kills nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_stop_leaves_the_watcher_to_deliver_the_tail_once() {
    let test = "a_kept_stop_leaves_the_watcher_to_deliver_the_tail_once";
    if !isolated_in("judged_stop_harness_tests", test, &[]) {
        return;
    }
    let seed = format!("{}{}{}", user("T0"), said(T0), stop());
    let mut h = Harness::new(80, &seed).await;
    let f = seed.len() as u64;
    h.commit(0, f);
    h.row_at(f);
    h.spawn(f);
    h.append(format!("{}{}", user("T1"), said(HEAD)).as_bytes());
    h.until("streaming preview", |h| h.showing(HEAD)).await;
    let marker = crate::services::tmux_common::session_temp_path(&h.tmux, "host_kind");
    std::fs::create_dir_all(&marker).unwrap();
    let token = Arc::new(crate::services::provider::CancelToken::new());
    token.bind_claude_tmux_session(&h.tmux);
    let user_msg = serenity::MessageId::new(h.channel.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    let author = serenity::UserId::new(7);
    assert!(start(&h.shared, h.channel, token.clone(), author, user_msg).await);
    let registry = crate::services::discord::health::HealthRegistry::new();
    registry
        .register("claude".to_string(), h.shared.clone())
        .await;
    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(CLAUDE),
        channel_id: Some(h.channel),
        tmux_name: h.tmux.clone(),
    };

    let stop_turn = crate::services::turn_lifecycle::stop_turn_preserving_queue;
    let result = stop_turn(Some(&registry), &target, "p6asb harness").await;
    assert!(result.host_guard_kept());
    assert!(!token.cancelled.load(Ordering::SeqCst));
    h.append(format!("{}{}", said(TAIL), stop()).as_bytes());
    h.drained("tail frame").await;

    let observed = h.observe(&[TAIL]);
    assert_eq!(
        observed.copies.iter().map(Vec::len).collect::<Vec<_>>(),
        [1]
    );
    assert_eq!(observed.missing_bytes, 0);
    assert_eq!(h.kills(), 0);
}
