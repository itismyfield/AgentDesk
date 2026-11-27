//! Test hook a slot reset runs for each verified channel before the managed-reset recheck.

type VerifiedClearRecheckHook = (u64, Box<dyn FnOnce()>);

thread_local! {
    static BEFORE_VERIFIED_CLEAR_RECHECK: std::cell::RefCell<Option<VerifiedClearRecheckHook>> =
        Default::default();
}

pub(crate) fn before_verified_clear_recheck_for_tests(
    channel: u64,
    action: impl FnOnce() + 'static,
) {
    BEFORE_VERIFIED_CLEAR_RECHECK
        .with(|cell| *cell.borrow_mut() = Some((channel, Box::new(action))));
}

pub(crate) fn run_before_verified_clear_recheck_for_tests(channel: u64) {
    let action = BEFORE_VERIFIED_CLEAR_RECHECK.with(|cell| {
        let mut pending = cell.borrow_mut();
        pending
            .as_ref()
            .is_some_and(|(expected, _)| *expected == channel)
            .then(|| pending.take().unwrap().1)
    });
    if let Some(action) = action {
        action();
    }
}
