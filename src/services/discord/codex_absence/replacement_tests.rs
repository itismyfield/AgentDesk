use super::*;
use crate::services::discord::save_actual_episode_for_tests;

const CHANNEL: u64 = 7_250_101;
const TMUX: &str = "AgentDesk-codex-replacement-unit";

fn token() -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    save_actual_episode_for_tests(CHANNEL, TMUX, 30, &token);
    token
}

#[test]
fn replacement_probe_detects_the_whole_successful_gap_but_not_failure_or_cancel() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = token();
    let probe = Probe::new(CHANNEL, TMUX);
    let guard = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
        .unwrap()
        .unwrap();
    assert!(probe.defer_death());
    assert!(!Probe::new(CHANNEL + 1, TMUX).defer_death());
    guard.complete();
    assert!(
        probe.defer_death(),
        "a negative probe spanning the whole gap must be retried"
    );
    let next = Probe::new(CHANNEL, TMUX);
    assert!(
        !next.defer_death(),
        "a death after spawn completion remains a death"
    );
    let failed = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
        .unwrap()
        .unwrap();
    drop(failed);
    assert!(
        !next.defer_death(),
        "create failure restores ordinary cleanup"
    );
    let cancelled = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
        .unwrap()
        .unwrap();
    token.publish_cancel("replacement test cancellation");
    assert!(!Probe::new(CHANNEL, TMUX).defer_death());
    assert!(Probe::new(CHANNEL, TMUX).cleanup().is_some());
    drop(cancelled);
}

#[test]
fn replacement_begin_and_terminal_cleanup_have_one_winner() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = token();
    let probe = Probe::new(CHANNEL, TMUX);
    let cleanup = probe.cleanup().unwrap();
    assert!(Replacement::begin(Some(CHANNEL), TMUX, Some(&token)).is_err());
    let worker = cleanup.clone();
    drop(cleanup);
    assert!(
        Replacement::begin(Some(CHANNEL), TMUX, Some(&token)).is_err(),
        "blocking cleanup retains admission after parent cancellation"
    );
    drop(worker);
    let guard = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
        .unwrap()
        .unwrap();
    assert!(
        probe.cleanup().is_none(),
        "replacement wins immediately before cleanup admission"
    );
    drop(guard);
    let cleanup = Probe::new(CHANNEL, TMUX).cleanup().unwrap();
    let row = load_inflight_state_read_only_result(&ProviderKind::Codex, CHANNEL)
        .unwrap()
        .unwrap();
    let identity = InflightTurnIdentity::from_state(&row);
    super::super::super::inflight::clear_inflight_state_if_matches_identity(
        &ProviderKind::Codex,
        CHANNEL,
        &identity,
    );
    assert!(
        load_inflight_state_read_only_result(&ProviderKind::Codex, CHANNEL)
            .unwrap()
            .is_none()
    );
    drop(cleanup);
    assert!(
        Replacement::begin(Some(CHANNEL), TMUX, Some(&token)).is_err(),
        "RowAbsent never becomes untracked launch permission"
    );
}

#[test]
fn replacement_requires_its_exact_turn_and_unwinds_on_panic() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = token();
    let stranger = Arc::new(CancelToken::new());
    assert!(Replacement::begin(Some(CHANNEL), TMUX, Some(&stranger)).is_err());
    assert!(Replacement::begin(Some(CHANNEL), "other-pane", Some(&token)).is_err());
    let original = load_inflight_state_read_only_result(&ProviderKind::Codex, CHANNEL)
        .unwrap()
        .unwrap();
    let mut legacy = original.clone();
    legacy.turn_nonce = None;
    super::super::super::inflight::save_inflight_state(&legacy).unwrap();
    assert!(Replacement::begin(Some(CHANNEL), TMUX, Some(&token)).is_err());
    legacy = original.clone();
    legacy.request_owner_user_id = 0;
    super::super::super::inflight::save_inflight_state(&legacy).unwrap();
    assert!(Replacement::begin(Some(CHANNEL), TMUX, Some(&token)).is_err());
    super::super::super::inflight::save_inflight_state(&original).unwrap();
    let probe = Probe::new(CHANNEL, TMUX);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
            .unwrap()
            .unwrap();
        panic!("fixture provider failure");
    }));
    assert!(caught.is_err());
    assert!(!probe.defer_death());
    assert!(probe.cleanup().is_some());
}
