use super::*;

thread_local! {
    static HOOKS: std::cell::RefCell<Vec<(&'static str, Box<dyn Fn()>)>> = Default::default();
}

/// Runs the test's hook for `point`, if one is registered on this thread.
pub(super) async fn hook(point: &'static str) {
    HOOKS.with(|hooks| {
        for (at, run) in hooks.borrow().iter() {
            if *at == point {
                run();
            }
        }
    });
}
