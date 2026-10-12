//! The policy close clears its exact receipt before anything is released, so a successor never
//! starts over a receipt row; a clear refused there releases nothing.
use super::*;

/// What `consume` held when it reached its clear: the active count and the durable receipt.
type AtClear = std::rc::Rc<std::cell::RefCell<Vec<(usize, Option<bool>)>>>;

/// Observes the clear's boundary; `bump` first saves the same episode's row again there.
fn at_clear(close: &Close, bump: bool) -> AtClear {
    let seen = AtClear::default();
    let (slot, shared) = (seen.clone(), close.shared.clone());
    test_hooks::observe(Some(Box::new(move |step, row: &InflightTurnState| {
        if step != "before_policy_clear" {
            return;
        }
        if bump {
            let mut again = row.clone();
            again.save_generation += 1;
            discord::inflight::save_inflight_state(&again).unwrap();
        }
        let durable =
            discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, row.channel_id);
        let pending = durable
            .and_then(|row| row.herdr_before_start_stop)
            .map(|receipt| receipt.notice_pending());
        let active = shared.restart.global_active.load(Ordering::Relaxed);
        slot.borrow_mut().push((active, pending));
    })));
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
    test_hooks::observe(None);
    assert!(settled);
    assert_eq!(*seen.borrow(), [(1, Some(false))], "cleared before release");
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

/// A same-episode save between the ack and the clear moves the row on, so the exact clear refuses;
/// the turn then stays held with its acked receipt, nothing is submitted, and restart finishes it.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_policy_clear_refused_releases_nothing() {
    let close = Close::open(929, |_| {}).await;
    let seen = at_clear(&close, true);
    let submitted = test_hooks::submitted();
    let settled = close.consume(shown()).await;
    test_hooks::observe(None);
    assert!(!settled);
    assert_eq!(*seen.borrow(), [(1, Some(false))]);
    assert_eq!(test_hooks::submitted(), submitted, "nothing submitted");
    assert!(close.held().await, "nothing released");
    assert_eq!(
        close.receipt_pending(),
        Some(false),
        "the acked receipt stays"
    );
}
