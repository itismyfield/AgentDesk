//! Tmux probe-failure readiness witness that joins the KE turn-liveness authority, moved out of KRX
//! platform tmux.

use crate::services::platform::tmux::timeout_tests::{PathOverride, write_fake_tmux};

/// Witness for the shared authority join in
/// `services::tmux_turn_liveness::independent_tmux_readiness`: a probe that
/// *failed* is not evidence of absence, so it must widen into
/// `LiveOrAmbiguous` instead of collapsing into `Missing`.
///
/// The test above pins only the classifier half, and the reconciler and
/// zombie-release suites inject an already-built `LiveOrAmbiguous`; nothing
/// covered the edge joining them. Rewriting that one arm to `Missing` kept
/// the whole suite green while handing both destructive consumers -- the
/// stale-turn reconciler's idle UPDATE and the zombie foreground mailbox
/// release -- a false confirmation of absence on transport, socket,
/// permission and timeout errors.
#[test]
fn probe_failure_widens_to_live_or_ambiguous_readiness() {
    use crate::services::provider::ProviderKind;
    use crate::services::tmux_turn_liveness::{
        IndependentTmuxReadiness, independent_tmux_readiness,
    };

    let temp = tempfile::TempDir::new().expect("temp dir");
    write_fake_tmux(
        temp.path(),
        "case \"$FAKE_TMUX_MODE\" in missing) echo \"can't find session: test\" >&2; exit 1;; failed) echo 'connect failed: No such file or directory' >&2; exit 1;; *) exit 0;; esac",
    );
    let _path = PathOverride::prepend_for_test(temp.path());

    let readiness =
        |name: &str| independent_tmux_readiness(name, &ProviderKind::Claude, None, None, None);

    // Confirmed absence is the only spawn-backed answer allowed to report
    // `Missing`, so the two arms stay observably distinct.
    unsafe { std::env::set_var("FAKE_TMUX_MODE", "missing") };
    assert_eq!(
        readiness("AgentDesk-timeout-fixture"),
        IndependentTmuxReadiness::Missing,
        "a confirmed missing session must still read as Missing"
    );

    // An unexpected tmux failure classifies as `ProbeFailed`; it must not
    // reach destructive callers as absence.
    unsafe { std::env::set_var("FAKE_TMUX_MODE", "failed") };
    let probe_failed = readiness("AgentDesk-timeout-fixture");
    unsafe { std::env::remove_var("FAKE_TMUX_MODE") };
    assert_eq!(
        probe_failed,
        IndependentTmuxReadiness::LiveOrAmbiguous,
        "an unexpected tmux failure is unknown liveness, not confirmed absence"
    );

    // The blank-name guard reaches `ProbeFailed` without spawning tmux at
    // all, so the join is pinned independently of the stderr classifier.
    assert_eq!(
        readiness("   "),
        IndependentTmuxReadiness::LiveOrAmbiguous,
        "a session name that cannot be probed is unknown liveness"
    );

    // State the consequence in the consumers' own terms: both destructive
    // paths key off `Missing`/`ReadyForInput`, so a probe failure must be
    // neither.
    assert_ne!(
        probe_failed,
        IndependentTmuxReadiness::Missing,
        "probe failure must not authorize the reconciler's idle UPDATE"
    );
    assert_ne!(
        probe_failed,
        IndependentTmuxReadiness::ReadyForInput,
        "probe failure must not authorize zombie mailbox release"
    );
}
