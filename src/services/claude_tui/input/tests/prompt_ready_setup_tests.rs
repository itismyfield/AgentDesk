use super::*;
use std::cell::RefCell;

thread_local! {
    static SETUP_NOTIFY: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
}

pub(in super::super) fn notify_after_registration() {
    if let Some(notify) = SETUP_NOTIFY.with(|slot| slot.borrow_mut().take()) {
        notify.notify_waiters();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_prompt_ready_event_observes_notify_fired_during_setup() {
    let notify = Arc::new(Notify::new());
    let trigger = notify.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        // Fire after subscription and before select polling, on this blocking thread.
        SETUP_NOTIFY.with(|slot| assert!(slot.replace(Some(trigger)).is_none()));
        let outcome = wait_for_prompt_ready_event(notify, Duration::from_secs(2));
        SETUP_NOTIFY.with(|slot| {
            assert!(
                slot.borrow().is_none(),
                "setup notification hook was not consumed"
            );
        });
        outcome
    })
    .await
    .expect("blocking task panicked");

    assert_eq!(outcome, HookFastPathOutcome::Ready);
}
