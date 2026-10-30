use super::*;
use crate::services::discord::gateway::GatewayFuture;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Mutex;
use std::task::Poll;
use std::time::Duration;

type Sent = Result<ReplaceLongMessageOutcome, String>;

struct Gateway {
    row: InflightTurnState,
    replies: Mutex<VecDeque<Sent>>,
    calls: Mutex<Vec<(tokio::time::Instant, serde_json::Value)>>,
    fail_ack: bool,
}

impl Gateway {
    fn new(close: &Close, replies: impl IntoIterator<Item = Sent>, fail_ack: bool) -> Self {
        let key = TurnKey::new(close.channel(), close.row.user_msg_id, 17)
            .with_episode_nonce(close.row.turn_nonce.as_deref());
        close.shared.turn_finalizer.register_start(
            key,
            ProviderKind::Claude,
            RelayOwnerKind::None,
            &close.shared,
        );
        Self {
            row: close.row.clone(),
            replies: Mutex::new(replies.into_iter().collect()),
            calls: Mutex::default(),
            fail_ack,
        }
    }

    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl TurnGateway for Gateway {
    fn replace_message_with_outcome<'a>(
        &'a self,
        channel: ChannelId,
        anchor: MessageId,
        text: &'a str,
    ) -> GatewayFuture<'a, Sent> {
        Box::pin(async move {
            assert_eq!(
                (channel.get(), anchor.get(), text),
                (
                    self.row.channel_id,
                    self.row.current_msg_id,
                    ADMITTED_ABORT_NOTICE
                )
            );
            let durable = discord::inflight::load_inflight_state_read_only(
                &ProviderKind::Claude,
                channel.get(),
            )
            .unwrap();
            assert!(
                durable
                    .herdr_before_start_stop
                    .as_ref()
                    .unwrap()
                    .supported()
            );
            let root = discord::inflight::inflight_runtime_root().unwrap();
            let path =
                discord::inflight::inflight_state_path(&root, &ProviderKind::Claude, channel.get());
            assert!(discord::inflight::try_lock_inflight_state_path(&path).is_ok());
            let receipt = serde_json::to_value(durable.herdr_before_start_stop).unwrap();
            let mut calls = self.calls.lock().unwrap();
            calls.push((tokio::time::Instant::now(), receipt));
            if self.fail_ack && calls.len() == 1 {
                InflightTurnState::arm_before_start_receipt_fault(channel.get(), false);
            }
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err("notice unavailable".into()))
        })
    }

    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async { panic!("unexpected standalone send") })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { panic!("unexpected standalone edit") })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async { panic!("unexpected turn retry") })
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { panic!("unexpected dispatch") })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        false
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

struct Boundaries {
    retries: Rc<Cell<usize>>,
    receipts: Rc<RefCell<Vec<serde_json::Value>>>,
}

impl Boundaries {
    fn new() -> Self {
        let retries = Rc::new(Cell::new(0));
        let receipts = Rc::new(RefCell::new(Vec::new()));
        let (count, seen) = (retries.clone(), receipts.clone());
        test_hooks::observe(Some(Box::new(move |step, row| {
            if step == "before_notice_retry" {
                count.set(count.get() + 1);
            }
            if step == "after_receipt" {
                seen.borrow_mut()
                    .push(serde_json::to_value(&row.herdr_before_start_stop).unwrap());
            }
        })));
        Self { retries, receipts }
    }
}

impl Drop for Boundaries {
    fn drop(&mut self) {
        test_hooks::observe(None);
    }
}

// Poll the production future without letting paused time auto-advance past the rendezvous.
async fn before_retry<F: Future<Output = bool>>(
    mut future: Pin<&mut F>,
    boundaries: &Boundaries,
    count: usize,
) {
    let started = std::time::Instant::now();
    loop {
        std::future::poll_fn(|cx| {
            assert!(
                future.as_mut().poll(cx).is_pending(),
                "bridge ended before retry"
            );
            Poll::Ready(())
        })
        .await;
        if boundaries.retries.get() >= count {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "retry boundary not reached"
        );
        tokio::task::yield_now().await;
    }
}

async fn exit(close: &Close, gateway: &Gateway) -> bool {
    exit_held(
        &close.shared,
        gateway,
        &close.actor,
        &close.row,
        close.anchor().unwrap_or_else(|| {
            crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0)
        }),
        (false, false, false),
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn p10_r1_notice_retries_the_same_receipt_then_closes_once() {
    let close = Close::open(5_340_910, |_| {}).await;
    let gateway = Gateway::new(&close, [Err("transient".into()), shown().await], false);
    let boundaries = Boundaries::new();
    let submitted = test_hooks::submitted();
    let mut events = close.events();
    tokio::time::pause();
    let future = exit(&close, &gateway);
    tokio::pin!(future);
    before_retry(future.as_mut(), &boundaries, 1).await;
    assert_eq!(gateway.count(), 1);
    assert!(close.held().await);
    let queued = close.other_turn_and_queue().await;
    let stopped = close
        .shared
        .mailbox(close.channel())
        .admit_herdr_user_stop_if_current(close.actor.clone(), "stop again".into())
        .await;
    assert!(stopped.already_stopping);
    tokio::time::advance(Duration::from_millis(999)).await;
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(gateway.count(), 1, "the backoff has not elapsed");
    tokio::time::advance(Duration::from_millis(2)).await;
    assert!(future.await);
    {
        let calls = gateway.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[1].0.duration_since(calls[0].0) >= Duration::from_secs(1));
        assert_eq!(calls[0].1, calls[1].1, "one durable admission");
    }
    assert_eq!(test_hooks::submitted(), submitted + 1);
    assert_eq!(close.released(&mut events), 1);
    assert_eq!(close.active(), 1, "only this turn's +1 is debited");
    assert_eq!(close.queued().await, queued);
    assert!(close.durable().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn p10_r1_ack_retry_reuses_the_sent_notice() {
    for (index, sent) in [
        shown().await,
        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
            edit_error: "anchor gone".into(),
            replacement_anchor: Some(MessageId::new(9_001)),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let close = Close::open(5_340_911 + index as u64, |_| {}).await;
        let gateway = Gateway::new(&close, [sent.clone(), sent], true);
        let boundaries = Boundaries::new();
        let submitted = test_hooks::submitted();
        let mut events = close.events();
        tokio::time::pause();
        let future = exit(&close, &gateway);
        tokio::pin!(future);
        before_retry(future.as_mut(), &boundaries, 1).await;
        assert!(InflightTurnState::before_start_receipt_faults_consumed(
            close.row.channel_id
        ));
        assert_eq!(close.receipt_pending(), Some(true));
        assert_eq!(gateway.count(), 1);
        tokio::time::advance(Duration::from_millis(1001)).await;
        assert!(future.await);
        assert_eq!(gateway.count(), 1, "a shown notice needs only another ACK");
        let receipts = boundaries.receipts.borrow();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0], receipts[1]);
        assert_eq!(test_hooks::submitted(), submitted + 1);
        assert_eq!(close.released(&mut events), 1);
        assert_eq!(close.active(), 0);
        assert!(close.durable().is_none());
        tokio::time::resume();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn p10_r1_notice_exhaustion_keeps_receipt_for_restart() {
    let close = Close::open(5_340_913, |_| {}).await;
    let gateway = Gateway::new(&close, [], false);
    let boundaries = Boundaries::new();
    let submitted = test_hooks::submitted();
    tokio::time::pause();
    let future = exit(&close, &gateway);
    tokio::pin!(future);
    before_retry(future.as_mut(), &boundaries, 1).await;
    let receipt_bytes = close.bytes();
    tokio::time::advance(Duration::from_millis(1001)).await;
    before_retry(future.as_mut(), &boundaries, 2).await;
    assert_eq!(gateway.count(), 2);
    tokio::time::advance(Duration::from_millis(3001)).await;
    assert!(future.await);
    assert_eq!(gateway.count(), 3);
    assert_eq!(close.bytes(), receipt_bytes);
    assert!(close.held().await);
    assert_eq!(close.receipt_pending(), Some(true));
    assert_eq!(test_hooks::submitted(), submitted);
    tokio::time::resume();
    discord::mailbox_clear_channel(&close.shared, &ProviderKind::Claude, close.channel()).await;
    assert!(
        close
            .shared
            .mailbox(close.channel())
            .snapshot()
            .await
            .cancel_token
            .is_none()
    );
    let mut next_boot = discord::make_shared_data_for_tests_with_storage(None);
    Arc::get_mut(&mut next_boot)
        .unwrap()
        .restart
        .current_generation = 18;
    let discord = discord::recovery_engine::o_cut_recorder::start(close.row.channel_id).await;
    discord::recovery_engine::restore_inflight_turns(
        &discord.http,
        &next_boot,
        &ProviderKind::Claude,
    )
    .await;
    assert_eq!(discord.contents(), [ADMITTED_ABORT_NOTICE]);
    assert!(
        close.durable().is_none(),
        "restart settles the retained receipt"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn p10_r1_retry_stops_when_the_captured_actor_changes() {
    let close = Close::open(5_340_914, |_| {}).await;
    let gateway = Gateway::new(&close, [Err("transient".into()), shown().await], false);
    let boundaries = Boundaries::new();
    let submitted = test_hooks::submitted();
    tokio::time::pause();
    let future = exit(&close, &gateway);
    tokio::pin!(future);
    before_retry(future.as_mut(), &boundaries, 1).await;
    let bytes = close.bytes();
    discord::mailbox_clear_channel(&close.shared, &ProviderKind::Claude, close.channel()).await;
    let successor = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &close.shared,
            close.channel(),
            successor.clone(),
            UserId::new(2),
            MessageId::new(close.row.user_msg_id + 10),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    // Keep the receipt row unchanged so admission cannot mask a missing actor check.
    tokio::time::advance(Duration::from_millis(1001)).await;
    assert!(future.await);
    assert_eq!(gateway.count(), 1, "a successor gets no stale notice");
    assert_eq!(test_hooks::submitted(), submitted);
    assert_eq!(close.bytes(), bytes);
    let current = close
        .shared
        .mailbox(close.channel())
        .snapshot()
        .await
        .cancel_token
        .unwrap();
    assert!(Arc::ptr_eq(&current, &successor));
    assert!(!successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(close.active(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn p10_r1_delivered_reentry_never_retransmits() {
    for (index, moved) in [false, true].into_iter().enumerate() {
        let close = Close::open(5_340_915 + index as u64, |_| {}).await;
        close.acked().await;
        if moved {
            let mut durable = close.durable().unwrap();
            durable.current_msg_id += 10;
            discord::inflight::save_inflight_state(&durable).unwrap();
        }
        let gateway = Gateway::new(&close, [], false);
        let before = close.bytes();
        let submitted = test_hooks::submitted();
        assert!(exit(&close, &gateway).await);
        assert_eq!(gateway.count(), 0);
        assert_eq!(test_hooks::submitted(), submitted + usize::from(!moved));
        if moved {
            assert_eq!(close.bytes(), before);
            assert!(close.held().await);
        } else {
            assert!(close.durable().is_none());
            assert_eq!(close.active(), 0);
        }
    }
}
