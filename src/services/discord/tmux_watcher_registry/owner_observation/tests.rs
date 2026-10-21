use std::cell::Cell;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};

use super::super::{ChannelId, TmuxWatcherHandle, TmuxWatcherRegistry, lock_tmux_watcher_registry};
use super::{
    TMUX_WATCHER_REGISTRY_LOCK, TmuxWatcherRegistryGuard, owner_observation_epoch,
    owner_publication_fence_held_for_tests, with_owner_publication,
};

fn registry() -> TmuxWatcherRegistry {
    TmuxWatcherRegistry::new_with_coords(Arc::new(dashmap::DashMap::new()))
}

fn handle(name: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: name.into(),
        output_path: format!("unused-{name}.jsonl"),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(1)),
    }
}

fn publish_after_contention<R>(
    expected: u64,
    mut publish: impl FnMut(&TmuxWatcherRegistryGuard) -> R,
) -> Result<R, &'static str> {
    for _ in 0..10_000 {
        match with_owner_publication(expected, |guard| publish(guard)) {
            Err("registry_busy") => std::thread::yield_now(),
            result => return result,
        }
    }
    panic!("owner registry did not release its synchronous guard");
}

#[test]
fn paired_insert_same_owner_replacement_and_remove_advance_global_epoch() {
    let registry = registry();
    let channel = ChannelId::new(6_694_101);
    let name = "owner-epoch-paired";
    let guard = lock_tmux_watcher_registry();
    let before = owner_observation_epoch().unwrap();
    let initial = handle(name);
    let original_cancel = initial.cancel.clone();
    assert!(registry.insert_locked(&guard, channel, initial).is_none());
    let inserted = owner_observation_epoch().unwrap();
    assert!(inserted > before);

    let replacement = handle(name);
    let replacement_cancel = replacement.cancel.clone();
    let displaced = registry
        .insert_locked(&guard, channel, replacement)
        .unwrap();
    assert!(Arc::ptr_eq(&displaced.cancel, &original_cancel));
    assert!(Arc::ptr_eq(
        &registry.get(&channel).unwrap().cancel,
        &replacement_cancel
    ));
    assert!(!Arc::ptr_eq(&original_cancel, &replacement_cancel));
    assert_eq!(registry.owner_channel_for_tmux_session(name), Some(channel));
    assert!(owner_observation_epoch().unwrap() > inserted);

    let before_remove = owner_observation_epoch().unwrap();
    let (_, removed) = registry.remove_locked(&guard, &channel).unwrap();
    assert!(!removed.cancel.load(Ordering::Relaxed));
    assert_eq!(registry.owner_channel_for_tmux_session(name), None);
    let removed_epoch = owner_observation_epoch().unwrap();
    assert!(removed_epoch > before_remove);
    assert!(registry.remove_locked(&guard, &channel).is_none());
    assert_eq!(owner_observation_epoch(), Some(removed_epoch));
}

#[test]
fn restored_clear_locked_is_reentrant_safe_and_clear_restore_aba_rejects_callback() {
    let registry = registry();
    let channel = ChannelId::new(6_694_102);
    let name = "owner-epoch-restored";
    assert!(registry.restore_owner_channel_for_tmux_session(name, channel));
    let pinned = owner_observation_epoch().unwrap();
    {
        let guard = lock_tmux_watcher_registry();
        registry.clear_restored_owner_locked(&guard, name);
        assert_eq!(registry.owner_channel_for_tmux_session(name), None);
        let cleared = owner_observation_epoch().unwrap();
        assert!(cleared > pinned);
        registry.clear_restored_owner_locked(&guard, name);
        assert_eq!(owner_observation_epoch(), Some(cleared));
    }
    assert!(registry.restore_owner_channel_for_tmux_session(name, channel));
    assert_eq!(registry.owner_channel_for_tmux_session(name), Some(channel));
    let calls = Cell::new(0);
    assert_eq!(
        publish_after_contention(pinned, |_| calls.set(calls.get() + 1)),
        Err("owner_epoch")
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn restored_restore_retain_and_wrapper_clear_mutations_advance_epoch() {
    let registry = registry();
    let name = "owner-epoch-rebind";
    let first = ChannelId::new(6_694_103);
    let second = ChannelId::new(6_694_104);
    let before_restore = owner_observation_epoch().unwrap();
    assert!(registry.restore_owner_channel_for_tmux_session(name, first));
    assert!(owner_observation_epoch().unwrap() > before_restore);
    let before_retain = owner_observation_epoch().unwrap();
    assert!(registry.retain_owner_during_session_rebind(name, second));
    assert_eq!(registry.owner_channel_for_tmux_session(name), Some(second));
    assert!(owner_observation_epoch().unwrap() > before_retain);
    let before_clear = owner_observation_epoch().unwrap();
    registry.clear_restored_owner_for_tmux_session(name);
    assert_eq!(registry.owner_channel_for_tmux_session(name), None);
    assert!(owner_observation_epoch().unwrap() > before_clear);

    registry.insert(first, handle(name));
    assert!(registry.retain_owner_during_session_rebind(name, first));
    assert!(registry.restored_owner_by_tmux_session.contains_key(name));
    let before_shadow_clear = owner_observation_epoch().unwrap();
    assert!(!registry.restore_owner_channel_for_tmux_session(name, second));
    assert_eq!(registry.owner_channel_for_tmux_session(name), Some(first));
    assert!(!registry.restored_owner_by_tmux_session.contains_key(name));
    assert!(owner_observation_epoch().unwrap() > before_shadow_clear);
}

#[test]
fn unrelated_registry_owner_mutation_invalidates_old_publication() {
    let first = registry();
    let unrelated = registry();
    let channel = ChannelId::new(6_694_105);
    let name = "owner-epoch-unchanged";
    first.restore_owner_channel_for_tmux_session(name, channel);
    let pinned = owner_observation_epoch().unwrap();
    unrelated.restore_owner_channel_for_tmux_session("unrelated-name", ChannelId::new(6_694_106));
    assert_eq!(first.owner_channel_for_tmux_session(name), Some(channel));
    let calls = Cell::new(0);
    assert_eq!(
        publish_after_contention(pinned, |_| calls.set(calls.get() + 1)),
        Err("owner_epoch")
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn publication_callback_holds_guard_and_tls_until_return() {
    let poisoned = std::panic::catch_unwind(|| {
        let _guard = lock_tmux_watcher_registry();
        panic!("interrupted registry writer");
    });
    assert!(poisoned.is_err());
    let registry = registry();
    let calls = Cell::new(0);
    assert!(!owner_publication_fence_held_for_tests());
    let mut published = false;
    for _ in 0..10_000 {
        let pinned = owner_observation_epoch().unwrap();
        match with_owner_publication(pinned, |guard| {
            calls.set(calls.get() + 1);
            assert!(owner_publication_fence_held_for_tests());
            assert!(TMUX_WATCHER_REGISTRY_LOCK.try_lock().is_err());
            assert_eq!(with_owner_publication(pinned, |_| ()), Err("registry_busy"));
            registry.clear_restored_owner_locked(guard, "absent-noop");
            assert_eq!(owner_observation_epoch(), Some(pinned));
        }) {
            Ok(()) => {
                published = true;
                break;
            }
            Err("owner_epoch" | "registry_busy") => std::thread::yield_now(),
            result => panic!("unexpected publication result: {result:?}"),
        }
    }
    assert!(published, "no uncontended owner publication was reached");
    assert_eq!(calls.get(), 1);
    assert!(!owner_publication_fence_held_for_tests());
}

#[test]
fn busy_registry_guard_never_enters_tls_or_calls_publication() {
    let guard = lock_tmux_watcher_registry();
    let pinned = owner_observation_epoch().unwrap();
    let calls = Cell::new(0);
    assert_eq!(
        with_owner_publication(pinned, |_| calls.set(calls.get() + 1)),
        Err("registry_busy")
    );
    assert_eq!(calls.get(), 0);
    assert!(!owner_publication_fence_held_for_tests());
    drop(guard);
}
