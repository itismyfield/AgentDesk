use super::{SessionTransitionBusy, SessionTransitionLocks};
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;
use tokio::sync::OwnedMutexGuard;

#[test]
fn a_channel_keeps_one_lock_while_a_handle_or_guard_lives() {
    let locks = SessionTransitionLocks::default();
    let channel = ChannelId::new(7_353_001);
    let first = locks.lock(channel);
    assert!(Arc::ptr_eq(&first, &locks.lock(channel)));
    assert!(!Arc::ptr_eq(&first, &locks.lock(ChannelId::new(7_353_002))));

    let guard = Arc::clone(&first).try_lock_owned().unwrap();
    drop(first);
    assert!(Arc::ptr_eq(
        OwnedMutexGuard::mutex(&guard),
        &locks.lock(channel)
    ));
    assert!(locks.lock(channel).try_lock_owned().is_err());
    drop(guard);
    assert!(locks.lock(channel).try_lock_owned().is_ok());
}

#[test]
fn growth_prunes_dead_entries_and_keeps_a_held_lock_registered() {
    let locks = SessionTransitionLocks::default();
    let held = ChannelId::new(7_353_011);
    let dead = ChannelId::new(7_353_012);
    let guard = locks.lock(held).try_lock_owned().unwrap();
    drop(locks.lock(dead));
    assert!(locks.has_entry(dead));

    let _grown = locks.lock(ChannelId::new(7_353_013));
    assert!(!locks.has_entry(dead));
    assert!(locks.has_entry(held));
    assert!(Arc::ptr_eq(
        OwnedMutexGuard::mutex(&guard),
        &locks.lock(held)
    ));
    assert!(locks.lock(held).try_lock_owned().is_err());
}

#[test]
fn two_registries_never_share_a_channel_lock() {
    let ours = SessionTransitionLocks::default();
    let theirs = SessionTransitionLocks::default();
    let channel = ChannelId::new(7_353_021);
    let guard = ours.lock(channel).try_lock_owned().unwrap();
    assert!(!Arc::ptr_eq(
        OwnedMutexGuard::mutex(&guard),
        &theirs.lock(channel)
    ));
    assert!(theirs.lock(channel).try_lock_owned().is_ok());
    assert!(ours.lock(channel).try_lock_owned().is_err());
}

#[test]
fn a_dead_entry_is_replaced_in_place_without_pruning_the_others() {
    let locks = SessionTransitionLocks::default();
    let reused = ChannelId::new(7_353_031);
    let idle = ChannelId::new(7_353_032);
    let first = locks.lock(reused);
    drop(locks.lock(idle));
    let stale = Arc::downgrade(&first);
    drop(first);
    assert!(stale.upgrade().is_none());

    let second = locks.lock(reused);
    assert!(Arc::ptr_eq(&second, &locks.lock(reused)));
    assert!(
        locks.has_entry(idle),
        "replacing an existing channel's dead entry must not scan the registry"
    );
}

#[tokio::test(start_paused = true)]
async fn acquire_reports_busy_after_the_wait_window_and_succeeds_once_free() {
    let locks = SessionTransitionLocks::default();
    let channel = ChannelId::new(7_353_041);
    let held = locks.acquire(channel).await.unwrap();
    assert!(matches!(
        locks.acquire(channel).await,
        Err(SessionTransitionBusy)
    ));
    drop(held);
    assert!(locks.acquire(channel).await.is_ok());
}
