use super::{IntakeWorkerLifecycle, IntakeWorkerTickGuard};
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

thread_local! {
    static ENDS_BETWEEN_WAITER_STEPS: RefCell<Option<IntakeWorkerTickGuard<'static>>> =
        const { RefCell::new(None) };
}

/// Ends the armed tick between the waiter's registration and its count check.
pub(super) fn between_waiter_steps() {
    let tick = ENDS_BETWEEN_WAITER_STEPS.with(|slot| slot.borrow_mut().take());
    drop(tick);
}

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
fn last_tick_ending_inside_the_waiter_steps_completes_the_drain() {
    let lifecycle: &'static IntakeWorkerLifecycle = Box::leak(Box::default());
    let tick = lifecycle.try_begin_tick().unwrap();
    lifecycle.fence_admission();
    ENDS_BETWEEN_WAITER_STEPS.with(|slot| *slot.borrow_mut() = Some(tick));
    let mut waiter = Box::pin(lifecycle.wait_until_drained());
    let first = poll_drain(waiter.as_mut());
    assert!(
        ENDS_BETWEEN_WAITER_STEPS.with(|slot| slot.borrow().is_none()),
        "the waiter must reach the step boundary that ends the last tick"
    );
    assert!(
        first.is_ready() || poll_drain(waiter.as_mut()).is_ready(),
        "a last tick ending between the waiter steps must not strand the drain"
    );
}
