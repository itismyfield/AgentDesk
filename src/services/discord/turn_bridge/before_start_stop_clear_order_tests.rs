//! The policy close's mailbox step clears its exact receipt before anything is released, so a
//! successor never starts over a receipt row; a clear refused there releases nothing.
use super::*;
use crate::services::turn_orchestrator::policy_close::test_hooks as step;

/// What the close's mailbox step held when it reached its clear: the active count and whether
/// the durable receipt's notice was still pending.
type AtClear = Arc<std::sync::Mutex<Vec<(usize, Option<bool>)>>>;

/// Observes the next request's clear boundary; `bump` first saves the same episode's row there.
fn at_clear(close: &Close, bump: bool) -> AtClear {
    let seen = AtClear::default();
    let (slot, shared, row) = (seen.clone(), close.shared.clone(), close.row.clone());
    step::arm(Box::new(move |stage| {
        if stage != "before_clear" {
            return;
        }
        let durable =
            discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id);
        if bump {
            let mut again = durable.clone().unwrap();
            again.save_generation += 1;
            discord::inflight::save_inflight_state(&again).unwrap();
        }
        let pending = durable
            .and_then(|row| row.herdr_before_start_stop)
            .map(|receipt| receipt.notice_pending());
        let active = shared.restart.global_active.load(Ordering::Relaxed);
        slot.lock().unwrap().push((active, pending));
    }));
    seen
}

/// The acked receipt clears while the turn is still counted and held; only then is it released,
/// and the next turn on the channel starts and saves its own row.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_policy_clear_precedes_release_and_frees_the_successor() {
    let close = Close::open(928, |_| {}).await;
    let seen = at_clear(&close, false);
    let submitted = test_hooks::submitted();
    let settled = close.consume(shown()).await;
    assert!(settled);
    assert_eq!(
        *seen.lock().unwrap(),
        [(1, Some(false))],
        "cleared before release"
    );
    assert_eq!(test_hooks::submitted(), submitted + 1);
    assert!(close.durable().is_none());
    assert_eq!(
        close.shared.restart.global_active.load(Ordering::Relaxed),
        0
    );
    let next = Arc::new(CancelToken::new());
    let mut row = close.row.clone();
    row.turn_nonce = next.turn_nonce().map(str::to_owned);
    row.user_msg_id += 10;
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            ChannelId::new(928),
            next.clone(),
            UserId::new(2),
            MessageId::new(row.user_msg_id),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    discord::inflight::save_inflight_state(&row).expect("the successor saves its own row");
    assert_eq!(close.durable().unwrap().turn_nonce, row.turn_nonce);
}

/// A same-episode save before the step's clear makes the exact clear refuse; the one submitted
/// close releases and announces nothing, leaving the acked receipt held for restart.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_policy_clear_refused_releases_nothing() {
    let close = Close::open(929, |_| {}).await;
    let seen = at_clear(&close, true);
    let submitted = test_hooks::submitted();
    let mut events =
        crate::services::discord::turn_completion_events::subscribe_turn_completion_events(
            &close.shared,
        );
    let settled = close.consume(shown()).await;
    assert!(!settled);
    assert_eq!(*seen.lock().unwrap(), [(1, Some(false))]);
    assert_eq!(
        test_hooks::submitted(),
        submitted + 1,
        "the policy close is submitted"
    );
    assert!(events.try_recv().is_err(), "no release event");
    assert!(close.held().await, "nothing released");
    assert_eq!(
        close.receipt_pending(),
        Some(false),
        "the acked receipt stays"
    );
}
