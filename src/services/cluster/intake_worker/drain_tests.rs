use super::IntakeWorkerLifecycle;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

fn poll_drain(future: Pin<&mut impl Future<Output = ()>>) -> Poll<()> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn last_tick_completes_both_pending_waiters() {
    let lifecycle = IntakeWorkerLifecycle::default();
    let tick = lifecycle.try_begin_tick().unwrap();
    lifecycle.fence_admission();
    let mut first = Box::pin(lifecycle.wait_until_drained());
    let mut second = Box::pin(lifecycle.wait_until_drained());
    assert!(poll_drain(first.as_mut()).is_pending());
    assert!(poll_drain(second.as_mut()).is_pending());
    drop(tick);
    assert!(poll_drain(first.as_mut()).is_ready(), "first waiter");
    assert!(poll_drain(second.as_mut()).is_ready(), "second waiter");
}

#[test]
fn cancelled_waiter_does_not_block_survivor() {
    let lifecycle = IntakeWorkerLifecycle::default();
    let tick = lifecycle.try_begin_tick().unwrap();
    lifecycle.fence_admission();
    let mut cancelled = Box::pin(lifecycle.wait_until_drained());
    let mut survivor = Box::pin(lifecycle.wait_until_drained());
    assert!(poll_drain(cancelled.as_mut()).is_pending());
    assert!(poll_drain(survivor.as_mut()).is_pending());
    drop(cancelled);
    drop(tick);
    assert!(poll_drain(survivor.as_mut()).is_ready());
}

#[test]
fn late_waiter_completes_without_another_notification() {
    let lifecycle = IntakeWorkerLifecycle::default();
    let tick = lifecycle.try_begin_tick().unwrap();
    lifecycle.fence_admission();
    drop(tick);
    let mut late = Box::pin(lifecycle.wait_until_drained());
    assert!(poll_drain(late.as_mut()).is_ready());
}

#[test]
fn nonfinal_tick_does_not_complete_drain() {
    let lifecycle = IntakeWorkerLifecycle::default();
    let first_tick = lifecycle.try_begin_tick().unwrap();
    let last_tick = lifecycle.try_begin_tick().unwrap();
    lifecycle.fence_admission();
    let mut waiter = Box::pin(lifecycle.wait_until_drained());
    assert!(poll_drain(waiter.as_mut()).is_pending());
    drop(first_tick);
    assert!(poll_drain(waiter.as_mut()).is_pending());
    drop(last_tick);
    assert!(poll_drain(waiter.as_mut()).is_ready());
}

#[test]
fn drain_registers_before_checking_active_ticks() {
    // Lexical order tripwire, not a runtime interleaving proof or Rust parser.
    let source = include_str!("../intake_worker.rs");
    let start = source
        .find("pub(crate) async fn wait_until_drained(")
        .unwrap();
    let end = source[start..]
        .find("\npub(crate) struct IntakeWorkerTickGuard")
        .unwrap();
    let body = &source[start..start + end];
    let register = body
        .find("let notified = self.drained.notified();")
        .unwrap();
    let check = body
        .find("self.active_ticks.load(Ordering::SeqCst)")
        .unwrap();
    assert!(
        register < check,
        "register before checking: broadcast has no permit"
    );
}
