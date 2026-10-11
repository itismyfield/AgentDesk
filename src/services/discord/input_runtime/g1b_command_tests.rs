//! At the producer: only the namespace verdict disarms a live notice, and the handle a
//! release carried keeps re-arming the barrier after the handback settles.
use super::*;
use crate::services::discord::input_runtime::fence::Closing;
use crate::services::discord::input_runtime::fence::modes::{order_barrier, settle_order_barrier};
use crate::services::discord::input_runtime::supervisor::ordering::g1b_tests::exhaust_dirty;
use crate::services::discord::input_runtime::supervisor::ordering::{
    self, AdmissionOrder, EXHAUSTED,
};
use crate::services::provider::ProviderKind;

// Answers every live notice from `order` the way the supervisor loop does.
fn serve(
    mut order: AdmissionOrder,
) -> (
    mpsc::Sender<SupervisorCmd>,
    tokio::task::JoinHandle<AdmissionOrder>,
) {
    let (sender, mut receiver) = mpsc::channel(1);
    let service = tokio::spawn(async move {
        while let Some(SupervisorCmd::PendingSource { sources, reply }) = receiver.recv().await {
            reply.send(order.admit_pending(&sources)).unwrap();
        }
        order
    });
    (sender, service)
}

#[tokio::test]
async fn g1b_hm1_generation_refusal_keeps_pending_guard() {
    let mut order = AdmissionOrder::new(ProviderKind::Claude, 6_326_170, 7, 0);
    let overflow = order.pending_overflow();
    exhaust_dirty(&mut order);
    assert_eq!(order.admit_pending(&[11]).err(), Some(EXHAUSTED));
    let (sender, service) = serve(order);
    // The namespace verdict took nothing and settles the notice without a dirty mark.
    let external = crate::services::tui_input::input_key::EXTERNAL_KEY_BASE;
    let refused = pending_source(&sender, &overflow, vec![11, external]).await;
    assert!(matches!(refused, Err(Deferred::Order)));
    assert_eq!(overflow.generation(), 0);
    // Exhaustion took nothing either, so the same handle keeps the catch-up obligation.
    let exhausted = pending_source(&sender, &overflow, vec![12]).await;
    assert!(matches!(exhausted, Err(Deferred::Order)));
    assert_eq!(overflow.generation(), 1);
    drop(sender);
    let order = service.await.unwrap();
    assert!(order.pending_snapshot().sources.is_empty());
    assert!(!order.offer_ready());
}

#[tokio::test]
async fn g1b_release_keeps_old_producer_overflow() {
    let channel = 6_326_171;
    let original = AdmissionOrder::new(ProviderKind::Claude, channel, 7, 0);
    let overflow = original.pending_overflow();
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    closing.begin_handback().unwrap();
    (closing.release_after_handback_with_pending(&original.pending_snapshot())).unwrap();
    drop(original);
    let ticket = ordering::handback_fetch_ticket(&ProviderKind::Claude, channel).unwrap();
    let cap = ordering::handback_begin_scan(ticket, vec![], 0, true).unwrap();
    let proof = ordering::handback_complete(cap).unwrap();
    assert_eq!(settle_order_barrier(proof), Ok(true));
    assert!(!order_barrier(&ProviderKind::Claude, channel));
    // Only the earlier producer's guard fails: its receiver is already gone.
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let lost = pending_source(&sender, &overflow, vec![11]).await;
    assert!(matches!(lost, Err(Deferred::SupervisorLost)));
    assert!(order_barrier(&ProviderKind::Claude, channel));
}
