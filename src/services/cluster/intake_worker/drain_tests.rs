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

const INTAKE_WORKER: &str = include_str!("../intake_worker.rs");
const WAIT: &str = "pub(crate) async fn wait_until_drained(";
const REGISTER: &str = "let notified = self.drained.notified();";
const CHECK: &str = "self.active_ticks.load(Ordering::SeqCst)";

/// Lexical order tripwire, not a runtime interleaving proof or Rust parser: in the code
/// mask of the real `wait_until_drained`, registration precedes the active-tick check.
fn drain_order_gaps(source: &str) -> Vec<String> {
    let mask = crate::test_support::rust_code_mask(source).expect("intake worker lexes");
    let Some(span) = crate::test_support::braced_item_span(&mask, WAIT) else {
        return vec![format!("missing {WAIT}")];
    };
    let body = &mask[span];
    match (body.find(REGISTER), body.find(CHECK)) {
        (Some(register), Some(check)) if register < check => Vec::new(),
        found => vec![format!(
            "register before checking: broadcast has no permit {found:?}"
        )],
    }
}

#[test]
fn drain_registers_before_checking_active_ticks() {
    assert_eq!(drain_order_gaps(INTAKE_WORKER), Vec::<String>::new());
}

#[test]
fn drain_order_guard_rejects_commented_and_reordered_steps() {
    for needle in [WAIT, REGISTER, CHECK] {
        for forgery in crate::test_support::CommentForgery::ALL {
            let forged = crate::test_support::comment_out_code(INTAKE_WORKER, needle, forgery);
            assert!(
                !drain_order_gaps(&forged).is_empty(),
                "{forgery:?} commented `{needle}` still passes"
            );
        }
    }
    let register = INTAKE_WORKER.find(REGISTER).expect("registration");
    let check = register + INTAKE_WORKER[register..].find(CHECK).expect("tick check");
    let swapped = [
        &INTAKE_WORKER[..register],
        CHECK,
        &INTAKE_WORKER[register + REGISTER.len()..check],
        REGISTER,
        &INTAKE_WORKER[check + CHECK.len()..],
    ]
    .concat();
    assert!(
        !drain_order_gaps(&swapped).is_empty(),
        "reordered drain passes"
    );
}
