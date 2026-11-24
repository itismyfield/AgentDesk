use crate::services::discord::make_shared_data_for_tests;
use crate::services::discord::session_transition::*;
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

#[test]
fn transition_lease_excludes_a_second_claim_until_it_is_dropped() {
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(4_794_903);
    let lease = SessionTransitionLease::try_acquire(&shared, channel).unwrap();
    assert!(lease.matches(&shared, channel));
    assert!(SessionTransitionLease::try_acquire(&shared, channel).is_err());
    drop(lease);
    assert!(SessionTransitionLease::try_acquire(&shared, channel).is_ok());
}

#[test]
fn borrowed_transition_requires_the_same_channel_and_shared_owner() {
    let shared = make_shared_data_for_tests();
    let other = make_shared_data_for_tests();
    let channel = ChannelId::new(4_794_904);
    let guard = shared
        .session_transition_lock(channel)
        .try_lock_owned()
        .unwrap();
    let lease = SessionTransitionLease::borrow(&shared, channel, &guard).unwrap();
    assert!(lease.matches(&shared, channel));
    assert!(!lease.matches(&other, channel));
    assert!(!lease.matches(&shared, ChannelId::new(4_794_905)));
    assert!(SessionTransitionLease::borrow(&other, channel, &guard).is_err());
    assert!(SessionTransitionLease::borrow(&shared, ChannelId::new(4_794_905), &guard).is_err());
    drop(lease);
    assert!(SessionTransitionLease::try_acquire(&shared, channel).is_err());
    drop(guard);
    assert!(SessionTransitionLease::try_acquire(&shared, channel).is_ok());
}

#[tokio::test(start_paused = true)]
async fn transition_acquisition_times_out_after_contract_window() {
    let shared = make_shared_data_for_tests();
    let channel_id = ChannelId::new(4_794_899);
    let held = shared
        .session_transition_lock(channel_id)
        .lock_owned()
        .await;
    let waiting_shared = Arc::clone(&shared);
    let waiter =
        tokio::spawn(async move { waiting_shared.acquire_session_transition(channel_id).await });

    tokio::task::yield_now().await;
    assert!(!waiter.is_finished());
    tokio::time::advance(SESSION_TRANSITION_LOCK_WAIT_TIMEOUT).await;
    tokio::task::yield_now().await;
    assert!(
        waiter.is_finished(),
        "transition wait must end at the configured three-second boundary"
    );
    assert!(matches!(waiter.await.unwrap(), Err(SessionTransitionBusy)));
    drop(held);
}

#[test]
fn inactive_channel_locks_are_pruned_only_on_vacant_insertion() {
    let shared = make_shared_data_for_tests();
    let old_channel = ChannelId::new(4_794_900);
    let live_channel = ChannelId::new(4_794_901);
    let new_channel = ChannelId::new(4_794_902);

    let old = shared.session_transition_lock(old_channel);
    let live = shared.session_transition_lock(live_channel);
    drop(old);

    let same_live = shared.session_transition_lock(live_channel);
    assert!(Arc::ptr_eq(&live, &same_live));
    assert!(
        shared.session_transition_locks.contains_key(&old_channel),
        "the O(1) existing-channel path must not scan and prune the registry"
    );

    let _new = shared.session_transition_lock(new_channel);
    assert!(!shared.session_transition_locks.contains_key(&old_channel));
    assert!(shared.session_transition_locks.contains_key(&live_channel));
    assert!(shared.session_transition_locks.contains_key(&new_channel));
}
