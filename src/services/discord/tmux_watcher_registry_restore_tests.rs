use super::{
    Arc, ChannelId, TmuxWatcherHandle, TmuxWatcherRegistry, tmux_watcher_now_ms,
};

fn live_watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: format!("/tmp/{tmux_session_name}.jsonl"),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(
            std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms()),
        ),
    }
}

// #3105: a LIVE TUI session whose authoritative watcher-handle binding is
// missing (slot evicted by compact/restart/rebind, never re-claimed) must be
// self-healable via an authoritative re-registration so the idle relay can
// route again — instead of dropping every poll forever. This is the
// registry-side half of the fix; it asserts the restore is treated as
// authoritative on lookup and does NOT depend on the dedupe mirror.
#[test]
fn restored_owner_makes_missing_registry_resolve_for_live_session() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
    let channel = ChannelId::new(1_504_468_805_772_902_471);

    // No live watcher handle yet → registry misses (the #3018 drop trigger).
    assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);

    // Authoritative (settings-derived) re-registration repairs the miss.
    assert!(
        registry.restore_owner_channel_for_tmux_session(tmux, channel),
        "first restore must report a change so a single bounded incident is emitted"
    );
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        Some(channel),
        "restored owner must resolve authoritatively from the registry"
    );

    // Re-applying the same binding is a no-op (no repeated per-poll incident).
    assert!(
        !registry.restore_owner_channel_for_tmux_session(tmux, channel),
        "unchanged restore must not re-report a change"
    );
}

#[test]
fn session_rebind_retains_owner_when_called_after_watcher_teardown() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-claude-adk-cc";
    let channel = ChannelId::new(4794);

    let handle = live_watcher_handle(tmux);
    let cancel = handle.cancel.clone();
    registry.insert(channel, handle);
    registry.remove_tmux_session_if_current(tmux, &cancel);
    assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);

    assert!(registry.retain_owner_during_session_rebind(tmux, channel));
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        Some(channel),
        "the owner-only binding must be restorable after teardown until pane death"
    );
}

// A real live watcher handle is the primary authority and must win over (and
// evict) any restored owner-only binding — restored entries can never shadow
// or contradict live truth.
#[test]
fn live_watcher_handle_overrides_and_evicts_restored_owner() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
    let restored_channel = ChannelId::new(111_000_000_000_000);
    let live_channel = ChannelId::new(222_000_000_000_000);

    registry.restore_owner_channel_for_tmux_session(tmux, restored_channel);
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        Some(restored_channel)
    );

    // Claiming a live watcher handle takes over and drops the restored entry.
    registry.insert(live_channel, live_watcher_handle(tmux));
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        Some(live_channel),
        "live watcher handle must win over a restored owner-only binding"
    );

    // Removing the live watcher must NOT resurrect the evicted restored entry.
    registry.remove(&live_channel);
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        None,
        "evicted restored entry must not resurrect after the live watcher is removed"
    );
}

// Restoring an owner while a live watcher already owns the session must be a
// no-op (and clear any leftover restored entry) — never override live truth.
#[test]
fn restore_is_noop_when_live_watcher_owns_session() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-claude-adk-cc";
    let live_channel = ChannelId::new(333_000_000_000_000);

    registry.insert(live_channel, live_watcher_handle(tmux));
    assert!(
        !registry
            .restore_owner_channel_for_tmux_session(tmux, ChannelId::new(444_000_000_000_000)),
        "restore must not report a change when a live watcher owns the session"
    );
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        Some(live_channel),
        "live watcher owner must be unchanged by a restore attempt"
    );
}

// The base and thread-suffixed tmux names are distinct registry keys; a
// restored owner for the thread-suffixed live session resolves on its own
// exact key (the relay resolves the channel from the suffixed name).
#[test]
fn base_and_thread_suffixed_names_resolve_independently() {
    let registry = TmuxWatcherRegistry::new();
    let base = "AgentDesk-claude-adk-cc";
    let suffixed = "AgentDesk-claude-adk-cc-t1504468805772902471";
    let channel = ChannelId::new(1_504_468_805_772_902_471);

    registry.restore_owner_channel_for_tmux_session(suffixed, channel);
    assert_eq!(
        registry.owner_channel_for_tmux_session(suffixed),
        Some(channel)
    );
    assert_eq!(
        registry.owner_channel_for_tmux_session(base),
        None,
        "the base name must not borrow the thread-suffixed session's owner"
    );
}

// Clearing a restored owner (e.g. when the pane is no longer live) must drop
// the binding so a dead session can never resolve.
#[test]
fn clear_restored_owner_drops_binding() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
    let channel = ChannelId::new(1_504_468_805_772_902_471);

    registry.restore_owner_channel_for_tmux_session(tmux, channel);
    registry.clear_restored_owner_for_tmux_session(tmux);
    assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);
}

#[test]
fn cancel_and_remove_channel_if_current_only_rolls_back_matching_claim() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-codex-adk-cdx";
    let channel = ChannelId::new(1_504_468_805_772_902_471);
    let handle = live_watcher_handle(tmux);
    let expected_output_path = handle.output_path.clone();
    let expected_cancel = handle.cancel.clone();
    registry.insert(channel, handle);

    assert!(
        !registry.cancel_and_remove_channel_if_current(
            &channel,
            tmux,
            "/tmp/different.jsonl",
            &expected_cancel
        ),
        "output-path mismatch must not remove a possibly newer watcher"
    );
    assert_eq!(registry.owner_channel_for_tmux_session(tmux), Some(channel));
    assert!(!expected_cancel.load(std::sync::atomic::Ordering::Relaxed));

    assert!(registry.cancel_and_remove_channel_if_current(
        &channel,
        tmux,
        &expected_output_path,
        &expected_cancel
    ));
    assert_eq!(registry.owner_channel_for_tmux_session(tmux), None);
    assert!(expected_cancel.load(std::sync::atomic::Ordering::Relaxed));
}

#[test]
fn tmux_session_is_stale_does_not_fold_cancel_flag_into_heartbeat() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-codex-adk-cdx-fresh-cancel";
    let channel = ChannelId::new(1_504_468_805_772_902_472);
    let handle = live_watcher_handle(tmux);
    let cancel = handle.cancel.clone();
    registry.insert(channel, handle);

    cancel.store(true, std::sync::atomic::Ordering::Relaxed);

    assert_eq!(
        registry.tmux_session_is_stale(tmux),
        Some(false),
        "a fresh heartbeat watcher with an early cancel flag is cancelled, not heartbeat-stale"
    );
    assert_eq!(
        registry.tmux_session_live_for_relay(tmux),
        Some(false),
        "the same cancelled handle is still not relay-live; cancel is evaluated separately"
    );
}
