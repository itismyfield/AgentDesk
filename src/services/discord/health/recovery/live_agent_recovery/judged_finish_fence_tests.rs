//! The recovery fence holds only when its stop freed the channel: a turn admitted after the
//! judged one, or a finish the actor does not answer, leaves the fence closed and the turn kept.

use super::super::stop_judgement::judged_finish_tests::{Channel, claude, file_state, run};
use super::fence_runtime;

#[test]
fn a_fence_does_not_hold_over_a_successor_or_an_unanswered_finish() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let claude = claude();
    run(async {
        let fx = Channel::new(5_340_134_000).await;
        let fence = fence_runtime(&fx.registry, &claude, fx.channel);
        let (fenced, next) = tokio::join!(fence, fx.admit_successor_after_cancel());
        assert!(!fenced, "superseded: the fence does not hold");
        fx.assert_successor_kept(&next, "superseded").await;

        let fx = Channel::new(5_340_134_100).await;
        let row = file_state(&Channel::row_path(fx.channel));
        let fence = fence_runtime(&fx.registry, &claude, fx.channel);
        let unanswered = async {
            fx.wait_judged_cancelled().await;
            fx.shared.mailboxes.insert_unreachable_for_test(fx.channel);
        };
        let (fenced, ()) = tokio::join!(fence, unanswered);
        assert!(!fenced, "unanswered: the fence does not hold");
        fx.assert_runtime_kept("unanswered").await;
        let after = file_state(&Channel::row_path(fx.channel));
        assert_eq!(after, row, "unanswered: the row as it was");
        fx.shared.mailboxes.remove_fixture_for_test(fx.channel);
    });
}
