use super::*;
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::herdr_launch::tests::retired_durability_tests::SyncFault;

pub(super) fn retired_cache_skips_completed_cleanup_and_unknown_or_failed_fsync_retries() {
    let case = Case::launched("rr-cache");
    let path = case.hold_path();
    let fault = SyncFault::new(&path);
    fault.fail(true);
    let before = case.effects();
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert_eq!(case.cleanups(), 1);
    assert_eq!(
        fault.calls(),
        1,
        "real hold unlink reached the production fsync hook"
    );
    assert!(
        !path.exists(),
        "unlink landed before the injected fsync failure"
    );
    case.pass();
    assert_eq!(case.cleanups(), 2);
    assert_eq!(fault.calls(), 2);
    assert_eq!(
        crate::services::discord::recovery_engine::herdr_reader::reconnect_counts().withheld,
        1,
        "failed Retired cleanup remains withheld in restart health"
    );
    fault.fail(false);
    case.pass();
    assert_eq!(case.cleanups(), 3);
    assert_eq!(
        fault.calls(),
        3,
        "Retired retry must fsync the already absent hold"
    );
    assert_eq!(
        crate::services::discord::recovery_engine::herdr_reader::reconnect_counts().withheld,
        0,
        "completed Retired cleanup clears restart withholding"
    );
    case.pass();
    assert_eq!(case.cleanups(), 3, "completed cache avoids another cleanup");
    assert_eq!(fault.calls(), 3);
    recovery::prune(&[]);
    case.pass();
    assert_eq!(
        case.cleanups(),
        4,
        "unknown process cache re-establishes durable absence once"
    );
    assert_eq!(fault.calls(), 4);
    case.pass();
    assert_eq!(case.cleanups(), 4);
    assert_eq!(fault.calls(), 4);
    crate::services::claude::herdr_turn::hold(&case.nonce).unwrap();
    case.pass();
    assert_eq!(
        case.cleanups(),
        5,
        "a reappearing hold invalidates Completed"
    );
    assert_eq!(fault.calls(), 5);
    assert!(!path.exists());
    assert_eq!(case.effects(), before);
}

#[test]
fn rr_final_current_change_after_second_proof_never_attempts_cas_pg() {
    refuses_change_at(Change::Nonce, "rr-final-current", true);
}

#[test]
fn rr_final_quiet_change_after_second_proof_never_attempts_cas_pg() {
    for (change, tag) in [
        (Change::Active, "rr-final-active"),
        (Change::Inflight, "rr-final-inflight"),
    ] {
        std::thread::spawn(move || refuses_change_at(change, tag, true))
            .join()
            .unwrap();
    }
}

#[test]
fn rr_completed_retired_cache_needs_no_pg_or_transition_pg() {
    let case = Case::launched("rr-steady-completed");
    case.pass();
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    let retired = observation(&case);
    let guard = case
        .shared
        .session_transition_lock(ChannelId::new(CHANNEL))
        .try_lock_owned()
        .unwrap();
    let closed = case.fx.rt.block_on(async {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/closed")
            .unwrap();
        pool.close().await;
        pool
    });
    recovery::CURRENT_READS.with(|count| count.set(0));
    recovery::TRANSITIONS.with(|count| count.set(0));
    let result = case.fx.rt.block_on(recovery::recover_retired(
        &case.shared,
        &closed,
        &retired,
        None,
    ));
    assert_eq!(
        result,
        recovery::Recovery::Retired,
        "Completed exact row bypasses PG and a busy transition"
    );
    assert_eq!(recovery::CURRENT_READS.with(std::cell::Cell::get), 0);
    assert_eq!(recovery::TRANSITIONS.with(std::cell::Cell::get), 0);
    assert_eq!(case.cleanups(), 1);
    assert!(
        case.shared
            .session_transition_lock(ChannelId::new(CHANNEL))
            .try_lock_owned()
            .is_err()
    );
    drop(guard);
}

fn observation(case: &Case) -> crate::db::dispatched_sessions::hosted_execution::HostedObservation {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, load_hosted_execution_pg,
    };
    let key = super::super::super::session_key(&case.fx.owner);
    let HostedLookup::Found(row) = case.fx.rt.block_on(load_hosted_execution_pg(
        &case.fx.pool,
        HostedLookupKey::SessionKey(&key),
    )) else {
        panic!("exact fixture row");
    };
    row
}

fn candidate(
    case: &Case,
    row: &crate::db::dispatched_sessions::hosted_execution::HostedObservation,
) -> Result<Option<recovery::RootReplacedCandidate>, String> {
    let _registry = registry(case);
    crate::services::discord::recovery_engine::herdr_reader::prepare_root_replaced_candidate(row)?
        .observe()
}

fn registry(case: &Case) -> impl Drop {
    let endpoint = crate::config::session_hosts::herdr_endpoint(CHANNEL).unwrap();
    case.fx.rig.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    )
}

#[test]
fn rr_borrowed_transition_recovers_without_releasing_its_callers_guard_pg() {
    let case = Case::launched("rr-borrowed-transition");
    let _registry = registry(&case);
    let row = observation(&case);
    let candidate = candidate(&case, &row)
        .unwrap()
        .expect("exact RootReplaced read");
    let guard = case
        .shared
        .session_transition_lock(ChannelId::new(CHANNEL))
        .try_lock_owned()
        .unwrap();
    let lease =
        SessionTransitionLease::borrow(&case.shared, ChannelId::new(CHANNEL), &guard).unwrap();
    let result = case.fx.rt.block_on(recovery::recover(
        &case.shared,
        &case.fx.pool,
        &candidate,
        Some(&lease),
    ));
    assert_eq!(
        result,
        recovery::Recovery::Retired,
        "an existing exact transition may authorize recovery"
    );
    assert_eq!(case.fx.row(), Some(HostedState::Retired));
    assert!(!case.hold_path().exists());
    assert!(
        case.shared
            .session_transition_lock(ChannelId::new(CHANNEL))
            .try_lock_owned()
            .is_err()
    );
    drop(lease);
    assert!(
        case.shared
            .session_transition_lock(ChannelId::new(CHANNEL))
            .try_lock_owned()
            .is_err()
    );
    drop(guard);
    assert!(SessionTransitionLease::try_acquire(&case.shared, ChannelId::new(CHANNEL)).is_ok());
}

#[test]
fn rr_borrowed_transition_from_another_channel_holds_without_cleanup_pg() {
    let case = Case::launched("rr-wrong-transition");
    let _registry = registry(&case);
    let row = observation(&case);
    let candidate = candidate(&case, &row)
        .unwrap()
        .expect("exact RootReplaced read");
    let guard =
        SessionTransitionLease::try_acquire(&case.shared, ChannelId::new(CHANNEL + 1)).unwrap();
    let result = case.fx.rt.block_on(recovery::recover(
        &case.shared,
        &case.fx.pool,
        &candidate,
        Some(&guard),
    ));
    assert_eq!(
        result,
        recovery::Recovery::Held("borrowed transition differs".into())
    );
    assert_eq!(case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
}

#[test]
fn rr_initial_missing_cannot_construct_a_candidate_or_cleanup_pg() {
    let case = Case::launched("rr-missing-candidate");
    let row = observation(&case);
    let effects = case.effects();
    case.fx.rig.show_panes(&[]);
    assert!(
        candidate(&case, &row).is_err(),
        "Missing is Held before coordinator authorization"
    );
    use crate::services::discord::recovery_engine::host_reconcile::HostReconcile;
    use crate::services::discord::tui_prompt_relay::herdr_source::HerdrSourceAttach;
    assert!(
        recovery::RootReplacedCandidate::from_refusal(
            &row,
            &HerdrSourceAttach::Refused(HostReconcile::Missing)
        )
        .is_none()
    );
    assert_eq!(case.fx.row(), Some(HostedState::Bound));
    assert_eq!(case.cleanups(), 0);
    assert!(case.hold_path().exists());
    assert_eq!(case.effects(), effects);
}
