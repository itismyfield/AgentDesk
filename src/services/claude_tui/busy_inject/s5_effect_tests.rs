use super::*;
use crate::test_support::input_owner::{Op, Scope, fixture};

#[test]
fn s5_b0_pinned_stash_applied_counts_the_actual_payload_once() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-pinned-applied");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    let (outcome, proof, starts) = rig.run(&FAST, false, usize::MAX, false, |_| {}, || {});
    assert_eq!((outcome, proof, starts), (Outcome::Injected, None, 1));
    guard.assert_owner_payload(&attempt, TEXT.as_bytes(), 1);
    assert!(rig.pinned_writes());
    let counts = guard.counts();
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Paste && event.written)
            .count(),
        1
    );
    assert_eq!(
        counts
            .events
            .iter()
            .filter(|event| event.op == Op::Enter && event.written)
            .count(),
        1
    );
    wait("s5-pinned-enter", || {
        events(&rig.renderer).iter().any(|event| event == "Enter")
    });
    assert_eq!(fs::read_to_string(rig.renderer.join("body")).unwrap(), TEXT);
}

#[test]
fn s5_b0_pinned_stash_held_does_not_count_a_successful_shell_exit() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let rig = Rig::new("s5-pinned-held");
    let (_root, attempt, guard) = fixture("tmux", &rig.name);
    let swap = rig.swap();
    let _scope = guard.scope(Scope::owner(attempt.clone()));
    let (outcome, proof, _) = rig.run(
        &FAST,
        false,
        usize::MAX,
        false,
        |_| {},
        move || swap.replace(),
    );
    assert_eq!(
        (outcome, proof),
        (Outcome::Failed(WriteFailure::Paste), None)
    );
    assert!(rig.pinned_writes());
    let counts = guard.counts();
    assert_eq!(counts.attempted_legacy, 0);
    assert!(counts.owner_content.is_empty());
    assert!(
        counts
            .events
            .iter()
            .any(|event| event.op == Op::Paste && event.accepted && !event.written)
    );
    assert!(rig.events().is_empty());
}
