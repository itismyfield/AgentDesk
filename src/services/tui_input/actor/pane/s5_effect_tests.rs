use super::*;
use crate::test_support::input_owner::{Scope, fixture};

#[test]
fn s5_b0_actor_worker_observes_owner_and_refuses_late_unscoped_paste() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Fixture::new();
    let mut pane = rig.pane();
    let (_root, attempt, guard) = fixture("tmux", &pane.session);
    {
        let _scope = guard.scope(Scope::owner(attempt.clone()));
        assert_eq!(
            pane.with_composer(|pane| pane.submit("frame")),
            Some(SendOutcome::Sent)
        );
    }
    guard.assert_owner_payload(&attempt, b"frame", 1);
    // The fixture does not render a fresh composer after Enter.
    fs::write(
        rig.0.path().join("screen"),
        "────────────────────\n❯ \n────────────────────\n",
    )
    .unwrap();
    assert!(matches!(
        pane.with_composer(|pane| pane.submit("late fallback")),
        Some(SendOutcome::NotSent(_))
    ));
    assert_eq!(rig.calls("paste-buffer"), 1);
    assert_eq!(guard.counts().attempted_legacy, 1);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            guard.assert_owner_payload(&attempt, b"frame", 1);
        }))
        .is_err()
    );
}
