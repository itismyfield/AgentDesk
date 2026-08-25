use super::{
    Arc, ChannelId, DeliveryLeaseCell, DeliveryLeaseKey, TmuxWatcherHandle, TmuxWatcherRegistry,
    tmux_watcher_now_ms,
};
use super::tmux_watcher_registry::TerminalDeliveryFence;

/// #5071 relay-tail S4 r2 (P1-2 ②): the delivery conjunct is no longer optional
/// on the fenced view, so every identity-fence test has to supply one. These
/// tests are about the IDENTITY conjuncts, so they pin a fresh `Unleased` cell
/// under a key nothing ever acquires — the delivery conjunct always permits and
/// cannot be what decides the run.
fn permissive_delivery_fence(channel_id: ChannelId, site: &'static str) -> TerminalDeliveryFence {
    TerminalDeliveryFence::capture(
        Arc::new(DeliveryLeaseCell::new(channel_id)),
        DeliveryLeaseKey::new(channel_id, 1, 5_071_000_001, None, None),
        site,
    )
}

fn live_watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
    TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: format!("/tmp/{tmux_session_name}.jsonl"),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(tmux_watcher_now_ms())),
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
    assert!(registry.remove_tmux_session_if_current(tmux, &cancel).is_some());
    assert!(!cancel.load(std::sync::atomic::Ordering::Relaxed));
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
        !registry.restore_owner_channel_for_tmux_session(tmux, ChannelId::new(444_000_000_000_000)),
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

// #5071 T3-A1 r2, from the r2 reviewer's counterexample.
//
// THIS TEST IS NOT A DEFECT REPRODUCTION. It fixes the DECLARED limit of the
// value CAS documented on `WatcherIdentityFence`: nothing in the registry keeps
// a row generation, so an A -> B -> A readmission that restores every pinned
// value — the same owner channel, the same output path, the very same cancel
// `Arc`, and an untouched `.spawn_nonce` — compares equal to a row that never
// moved, and `Enforce` permits the removal. The assertions below are the CURRENT
// behaviour on purpose. If this behaviour ever changes, the declaration on
// `WatcherIdentityFence` must be rewritten in the same commit.
//
// Reachability is a separate question, answered in that same declaration: every
// production registry writer mints a fresh cancel `Arc`, so only a test can hand
// the registry back the pinned pointer.
//
// `cfg(unix)`: off unix there is no `.spawn_nonce` marker store, so the nonce
// conjunct is permanently `Unknown` and `Enforce` denies unconditionally.
#[cfg(unix)]
#[test]
fn value_cas_declared_non_guarantee_readmitted_identical_row_passes_enforce() {
    use super::tmux_watcher_registry::WatcherIdentityFence;
    use crate::config::ExecutionIdentityMode;

    const SITE: &str = "t3a1_readmitted_row_declared_limit";

    let root = tempfile::tempdir().expect("isolated runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());

    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-5071-t3a1-readmitted-row";
    let owner = ChannelId::new(5_071_000_000_000_000_001);
    let usurper = ChannelId::new(5_071_000_000_000_000_002);

    // Row A, plus the single spawn whose marker the fence re-reads. The session
    // is never respawned, so the nonce conjunct matches throughout and cannot be
    // what decides this run.
    let handle = live_watcher_handle(tmux);
    let pinned_output_path = handle.output_path.clone();
    let pinned_cancel = handle.cancel.clone();
    registry.insert(owner, handle);
    super::write_spawn_nonce(tmux).expect("spawn nonce");

    // The complete T3-R2 pin, captured the way the TUI call site captures it.
    let fence = WatcherIdentityFence::capture(ExecutionIdentityMode::Enforce, SITE, tmux)
        .with_pinned_binding(owner, &pinned_output_path);

    // A -> B: a genuinely different row takes the session key — another owner,
    // another output path, its own cancel `Arc`.
    let mut replacement = live_watcher_handle(tmux);
    replacement.output_path = format!("/tmp/{tmux}-replacement.jsonl");
    let replacement_cancel = replacement.cancel.clone();
    registry.insert(usurper, replacement);
    assert!(
        !Arc::ptr_eq(&replacement_cancel, &pinned_cancel),
        "the replacement must carry its own cancel pointer"
    );

    // Control: the fence really is enforcing at this point. Pinning B's pointer
    // gets past `Arc::ptr_eq` and is then refused by the owner/output conjunct.
    let control = WatcherIdentityFence::capture(ExecutionIdentityMode::Enforce, SITE, tmux)
        .with_pinned_binding(owner, &pinned_output_path);
    assert!(
        registry
            .under_identity_fence(control)
            .with_terminal_delivery_fence(permissive_delivery_fence(owner, SITE))
            .remove_tmux_session_if_current(tmux, &replacement_cancel)
            .is_none(),
        "Enforce must refuse while the live row genuinely differs from the pin"
    );

    // B -> A: the row is restored value for value, including the SAME cancel
    // `Arc`. No production writer does this; see `WatcherIdentityFence`.
    let mut readmitted = live_watcher_handle(tmux);
    readmitted.output_path = pinned_output_path.clone();
    readmitted.cancel = pinned_cancel.clone();
    registry.insert(owner, readmitted);

    assert!(
        registry
            .under_identity_fence(fence)
            .with_terminal_delivery_fence(permissive_delivery_fence(owner, SITE))
            .remove_tmux_session_if_current(tmux, &pinned_cancel)
            .is_some(),
        "declared limit: a value CAS cannot see the A -> B -> A replacement history"
    );
    assert_eq!(
        registry.owner_channel_for_tmux_session(tmux),
        None,
        "the readmitted row is removed, which is exactly the limit being declared"
    );
}

// #5399: a destruction decision routed through the fence used to cost two
// `.spawn_nonce` reads in EVERY mode — the up-front capture plus a re-read
// inside the registry lock — and `Legacy` threw both answers away. This drives a
// whole Legacy decision over a session whose marker exists and is readable, and
// asserts the read tally for that session never moves while the removal still
// happens. Removing either short-circuit (`WatcherIdentityFence::capture` or the
// top of `destruction_permitted_under_identity`) fails the tally assertion.
//
// The `Enforce` half is the control: the same decision on a second session still
// makes both reads and still permits, so the short-circuits did not silence the
// modes that consume the comparison.
//
// `cfg(unix)`: off unix there is no marker store to read or skip reading.
#[cfg(unix)]
#[test]
fn legacy_decision_reads_no_spawn_nonce_marker_and_still_removes() {
    use super::tmux::execution_identity::spawn_nonce_reads_for;
    use super::tmux_watcher_registry::WatcherIdentityFence;
    use crate::config::ExecutionIdentityMode;

    const SITE: &str = "t3a1_legacy_dead_io";

    let root = tempfile::tempdir().expect("isolated runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());

    for (mode, tmux, expected_reads) in [
        (
            ExecutionIdentityMode::Legacy,
            "AgentDesk-5399-dead-io-legacy",
            0,
        ),
        (
            ExecutionIdentityMode::Enforce,
            "AgentDesk-5399-dead-io-enforce",
            2,
        ),
    ] {
        let registry = TmuxWatcherRegistry::new();
        let owner = ChannelId::new(5_399_000_000_000_000_001);

        let handle = live_watcher_handle(tmux);
        let pinned_output_path = handle.output_path.clone();
        let pinned_cancel = handle.cancel.clone();
        registry.insert(owner, handle);
        // A readable marker, so a read that happens is visible as a tally move
        // and a read that is skipped cannot be excused by an absent marker.
        super::write_spawn_nonce(tmux).expect("spawn nonce");
        let baseline = spawn_nonce_reads_for(tmux);

        let fence = WatcherIdentityFence::capture(mode, SITE, tmux)
            .with_pinned_binding(owner, &pinned_output_path);
        assert!(
            registry
                .under_identity_fence(fence)
                .with_terminal_delivery_fence(permissive_delivery_fence(owner, SITE))
                .remove_tmux_session_if_current(tmux, &pinned_cancel)
                .is_some(),
            "{mode:?} must still permit an untouched row: nothing about the CAS answer changed"
        );
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            None,
            "the permitted removal must actually drop the row"
        );
        assert_eq!(
            spawn_nonce_reads_for(tmux) - baseline,
            expected_reads,
            "{mode:?} must make exactly {expected_reads} marker read(s) across capture and CAS"
        );
    }
}

// #5071 T4-B0 (#4987 S0): `channel_binding` is the only ChannelId-keyed view of
// a watcher slot, and until this slice it returned the owner channel and the
// session name only — so #4986's second transcript coordinate, which the handle
// has always held, had no route out of the registry. These two fix that route:
// a binding that resolves the channel but drops the path, or that reports a
// path the handle no longer has, fails here.
#[test]
fn channel_binding_exposes_the_live_watcher_output_path() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-5071-t4b0-binding-path";
    let channel = ChannelId::new(5_071_000_000_000_000_040);

    let mut handle = live_watcher_handle(tmux);
    handle.output_path = format!("/tmp/{tmux}-native.jsonl");
    let native_output_path = handle.output_path.clone();
    registry.insert(channel, handle);

    let binding = registry
        .channel_binding(&channel)
        .expect("a live watcher slot must resolve a binding for its owner channel");
    assert_eq!(binding.tmux_session_name, tmux);
    assert_eq!(
        binding.output_path.as_deref(),
        Some(native_output_path.as_str()),
        "the binding must carry the handle's transcript, not None"
    );
    assert_eq!(
        binding.output_path,
        registry.watcher_output_path(tmux),
        "the binding and the session-keyed accessor must never disagree"
    );
}

// The wrapper -> provider-native promotion is the state #4986 was observed in,
// and it is reached by replacing the handle. A binding that cached the first
// path would keep reporting the wrapper file after the watcher left it.
#[test]
fn channel_binding_output_path_follows_the_native_transcript_handoff() {
    let registry = TmuxWatcherRegistry::new();
    let tmux = "AgentDesk-5071-t4b0-binding-handoff";
    let channel = ChannelId::new(5_071_000_000_000_000_041);

    let mut wrapper = live_watcher_handle(tmux);
    wrapper.output_path = format!("/tmp/{tmux}-wrapper.jsonl");
    let wrapper_output_path = wrapper.output_path.clone();
    registry.insert(channel, wrapper);
    assert_eq!(
        registry
            .channel_binding(&channel)
            .and_then(|binding| binding.output_path)
            .as_deref(),
        Some(wrapper_output_path.as_str())
    );

    let mut native = live_watcher_handle(tmux);
    native.output_path = format!("/tmp/{tmux}-native.jsonl");
    let native_output_path = native.output_path.clone();
    registry.insert(channel, native);

    assert_eq!(
        registry
            .channel_binding(&channel)
            .and_then(|binding| binding.output_path)
            .as_deref(),
        Some(native_output_path.as_str()),
        "the binding must track the live handle, not the transcript it started on"
    );

    // Removing the slot removes the coordinate with it: the binding never
    // outlives the handle it read the path from.
    let cancel = registry
        .get(&channel)
        .map(|entry| entry.cancel.clone())
        .expect("the live handle must still be registered");
    registry.remove_tmux_session_if_current(tmux, &cancel);
    assert!(registry.channel_binding(&channel).is_none());
}

// #5071 relay-tail S4 r2 (P1-1): the delivery conjunct's judgment and the
// destruction it authorizes must be ATOMIC against a concurrent acquire of the
// very key it judged.
//
// The r1 shape could not be. `TerminalDeliveryFence::permits_destruction` read
// a `LeaseSnapshot` through `DeliveryLeaseCell::read`, which drops the cell's
// payload mutex on the way out, and returned a `bool`; the registry lock the
// CAS core holds is a different lock and fences nothing on the cell. So a
// Sink/Bridge `try_acquire` under the SAME key could win between "no live
// holder" and `remove`, and the watcher was torn down under a delivery that had
// just legitimately started.
//
// The property asserted here is what closing that window looks like from the
// racing acquirer's side: by the time its `try_acquire` returns, the removal it
// raced must ALREADY be committed. The hook fires in exactly the r1 window —
// after the conjunct permitted, before the registry mutation — and holds it
// open for far longer than any real interleaving would, so under the r1 shape
// the acquirer wins immediately and still sees the row registered.
#[test]
fn delivery_fence_judgment_and_destruction_are_atomic_against_a_racing_acquire() {
    use super::tmux_watcher_registry::{
        WatcherIdentityFence, set_delivery_fence_permitted_hook_for_tests,
    };
    use crate::config::ExecutionIdentityMode;
    use std::sync::atomic::{AtomicBool, Ordering};

    const SITE: &str = "s4_r2_delivery_fence_atomicity";
    // Long enough that a lost race is a loud failure rather than a flake: the
    // acquirer would have to be descheduled for the whole window to look atomic
    // by accident.
    const HELD_OPEN: std::time::Duration = std::time::Duration::from_millis(300);

    let registry = Arc::new(TmuxWatcherRegistry::new());
    let tmux = "AgentDesk-5071-s4r2-toctou";
    let owner = ChannelId::new(5_071_000_000_000_000_042);

    let handle = live_watcher_handle(tmux);
    let pinned_cancel = handle.cancel.clone();
    registry.insert(owner, handle);

    // Unleased at the judgment, so the conjunct permits and the removal is the
    // expected outcome. The race is about WHEN, not whether.
    let lease = Arc::new(DeliveryLeaseCell::new(owner));
    let contended_key = DeliveryLeaseKey::new(owner, 1, 5_071_000_042, None, None);

    let judged = Arc::new(AtomicBool::new(false));
    let racer = std::thread::spawn({
        let lease = Arc::clone(&lease);
        let registry = Arc::clone(&registry);
        let key = contended_key.clone();
        let judged = Arc::clone(&judged);
        let tmux = tmux.to_string();
        move || {
            while !judged.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            let acquired = lease.try_acquire(
                key,
                super::LeaseHolder::Sink,
                0,
                128,
                super::lease_now_ms().saturating_add(super::DELIVERY_LEASE_DEADLINE_MS),
            );
            // Read the registry the instant the lease is ours. Under r1 the row
            // is still there; under r2 the removal already happened.
            (acquired, registry.owner_channel_for_tmux_session(&tmux))
        }
    });

    let hook_judged = Arc::clone(&judged);
    let _hook = set_delivery_fence_permitted_hook_for_tests(Arc::new(move |site| {
        if site != SITE {
            return;
        }
        hook_judged.store(true, Ordering::Release);
        std::thread::sleep(HELD_OPEN);
    }));

    // `Legacy` reads no `.spawn_nonce` marker and pins no binding, so the
    // identity conjunct cannot be what decides this run.
    let identity_fence = WatcherIdentityFence::capture(ExecutionIdentityMode::Legacy, SITE, tmux);
    let delivery_fence =
        TerminalDeliveryFence::capture(Arc::clone(&lease), contended_key.clone(), SITE);
    let removed = registry
        .under_identity_fence(identity_fence)
        .with_terminal_delivery_fence(delivery_fence)
        .remove_tmux_session_if_current(tmux, &pinned_cancel);
    assert!(
        removed.is_some(),
        "the cell was Unleased at the judgment, so the conjunct must permit"
    );

    let (acquired, owner_seen_by_racer) = racer.join().expect("racing acquirer thread");
    assert!(
        acquired,
        "the fence must not consume or block the lease itself; the racer takes it \
         once the destruction is done"
    );
    assert_eq!(
        owner_seen_by_racer, None,
        "a delivery lease taken under the judged key must never become live while \
         the destruction it was judged against is still pending"
    );
}
