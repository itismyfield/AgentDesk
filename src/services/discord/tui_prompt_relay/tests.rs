use super::injected_prompt_policy::slash_command_control_kind;
use super::observed_prompt_decision::is_local_only_slash_command_prompt;
use super::*;
use crate::services::discord::gateway::TurnGateway;

fn compact_command_name_first_stub() -> &'static str {
    "<command-name>/compact</command-name>\n            <command-message>compact</command-message>\n            <command-args></command-args>"
}

/// Scoped env-var override for inflight persistence tests. `AGENTDESK_ROOT_DIR`
/// is process-global, so serialize it with the shared test env lock.
struct EnvRootGuard {
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvRootGuard {
    fn set(path: &std::path::Path) -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for EnvRootGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}

// ====================================================================
// #3154 P2-2 (c) — the KEY residual risk: prove there is NO relay GAP
// (not merely no duplicate) on the deferred synthetic-start path.
//
// The relay of a turn's output must come from EXACTLY ONE owner. The
// deferred path has two participants:
//   * the OBSERVER's BridgeAdapter idle-response tail, and
//   * the deferred worker's claimed (watcher) owner.
// If the observer skips but the worker never adopts the watcher owner, the
// output is dropped (a GAP). If both run, it relays twice (a DUPLICATE).
// These tests pin BOTH production decisions against the REAL lease store.
// ====================================================================

/// Deferred ⇒ the observer must STAND DOWN (skip its bridge tail). RED if
/// `observer_should_spawn_bridge_tail` stops honoring `deferred`.
#[test]
fn deferred_observer_skips_bridge_tail() {
    // Lease still reads as BridgeAdapter (pre-claim) — the dangerous case:
    // without the deferred guard the observer WOULD spawn a second relay.
    assert!(
        !observer_should_spawn_bridge_tail(true, ExternalInputRelayOwner::BridgeAdapter),
        "deferred path: the observer must NOT spawn its own bridge tail \
             (the worker owns the relay handoff) — else DUPLICATE relay"
    );
    // Non-deferred + BridgeAdapter owner ⇒ observer relays (the normal path).
    assert!(observer_should_spawn_bridge_tail(
        false,
        ExternalInputRelayOwner::BridgeAdapter
    ));
    // Non-deferred but a watcher already owns it ⇒ observer stands down.
    assert!(!observer_should_spawn_bridge_tail(
        false,
        ExternalInputRelayOwner::TmuxWatcher
    ));
}

/// The no-GAP invariant end-to-end against the REAL lease store. When the
/// synthetic start is deferred and the worker's claim resolves to the tmux
/// WATCHER, the adoption re-records the lease as watcher-owned. We then prove
/// EXACTLY ONE relayer remains:
///   (1) the observer stands down (deferred), AND
///   (2) a watcher relayer exists (the persisted lease is watcher-owned, so
///       the bridge-tail guard reads it and the watcher is the relay owner).
/// Together: not zero (no GAP) and not two (no duplicate).
#[test]
fn deferred_claim_adopts_watcher_owner_exactly_one_relayer_no_gap() {
    let provider = "claude";
    let tmux = "tmux-3154-p2-2-c";
    let channel_id: u64 = 770_000_000_000_001;

    // Worker rehydrates the lease as BridgeAdapter (the persisted pre-claim
    // owner), records it, then the claim resolves to the WATCHER.
    let mut lease = ExternalInputRelayLease::unassigned(Some(channel_id));
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    let lease =
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease(provider, tmux, lease);

    let claimed_owner = ExternalInputRelayOwner::TmuxWatcher;
    let claimed = true;

    // PRODUCTION decision: should we adopt the claimed owner? (Mirrors the
    // real inline + deferred adoption call sites.)
    assert!(
        claim_should_adopt_relay_owner(claimed, lease.relay_owner, claimed_owner),
        "a successful claim that flips the owner MUST adopt — RED if adoption \
             is skipped, which would leave a stale BridgeAdapter lease and the \
             observer/bridge tail would relay a SECOND copy"
    );

    // Perform the adoption exactly as the deferred worker does: re-record the
    // lease with the claimed owner into the REAL store.
    let mut adopted = lease.clone();
    adopted.relay_owner = claimed_owner;
    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(provider, tmux, adopted);

    // (2) The persisted lease now reads as watcher-owned: a relayer EXISTS.
    let stored =
        crate::services::tui_prompt_dedupe::external_input_relay_lease(provider, tmux, channel_id)
            .expect("lease present after adoption");
    assert_eq!(
        stored.relay_owner,
        ExternalInputRelayOwner::TmuxWatcher,
        "after adoption the watcher owns the relay — a relayer EXISTS (no GAP)"
    );

    // (1) With the watcher owning the lease, the observer stands down whether
    // or not we re-check the deferred flag — so the watcher is the SOLE
    // relayer. Count relayers explicitly: observer(0) + watcher(1) == 1.
    let observer_relays = observer_should_spawn_bridge_tail(true, stored.relay_owner);
    let watcher_relays = matches!(stored.relay_owner, ExternalInputRelayOwner::TmuxWatcher);
    let relayer_count = u8::from(observer_relays) + u8::from(watcher_relays);
    assert_eq!(
        relayer_count, 1,
        "EXACTLY ONE relayer on the deferred path: not zero (no GAP) and not \
             two (no duplicate). RED if adoption is dropped (relayer_count==0, GAP) \
             or if the observer ignores `deferred` (relayer_count==2, duplicate)."
    );

    // Hygiene: clear the test lease.
    let _ = crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
        provider, tmux, channel_id,
    );
}

/// #3154 P1 (BridgeAdapter-GAP) — the PARALLEL no-GAP invariant for the OTHER
/// resolved owner. When the deferred claim resolves to the BridgeAdapter (NO
/// watcher will relay this turn), there must STILL be exactly one relayer: the
/// worker spawns the bridge tail. We count relayers explicitly:
///   * observer(0) — stood down on the deferred path, AND
///   * watcher(0) — the resolved owner is the BridgeAdapter, not the watcher, SO
///   * worker bridge tail(1) — `deferred_claim_requires_bridge_tail_relayer` fires.
///
/// RED before this fix: the worker never spawned a bridge tail for the
/// BridgeAdapter owner, so observer(0) + watcher(0) + worker(0) == 0 == GAP.
/// Neutralizing the new branch the OTHER direction (forcing the worker to spawn
/// for the WATCHER owner) is covered by the watcher test below staying at 1.
#[test]
fn deferred_claim_resolves_bridge_owner_exactly_one_relayer_no_gap() {
    // Deferred ⇒ the observer stands down regardless of owner (it cannot know
    // the resolved owner pre-claim and hands the decision to the worker).
    let observer_relays =
        observer_should_spawn_bridge_tail(true, ExternalInputRelayOwner::BridgeAdapter);
    assert!(
        !observer_relays,
        "deferred path: the observer always stands down (the worker owns the \
             post-claim bridge-tail decision)"
    );

    // The claim resolved to the BridgeAdapter: the watcher will NOT relay.
    let resolved_owner = ExternalInputRelayOwner::BridgeAdapter;
    let watcher_relays = matches!(resolved_owner, ExternalInputRelayOwner::TmuxWatcher);
    assert!(!watcher_relays, "BridgeAdapter owner ⇒ no watcher relayer");

    // PRODUCTION decision: the worker MUST spawn its bridge tail for the
    // BridgeAdapter owner — this is the GAP fix.
    let worker_bridge_tail = deferred_claim_requires_bridge_tail_relayer(resolved_owner);
    assert!(
        worker_bridge_tail,
        "BridgeAdapter-owned deferred claim MUST get a worker bridge tail — \
             RED before this fix (worker spawned nothing ⇒ relayer_count == 0 == GAP)"
    );

    let relayer_count =
        u8::from(observer_relays) + u8::from(watcher_relays) + u8::from(worker_bridge_tail);
    assert_eq!(
        relayer_count, 1,
        "EXACTLY ONE relayer on the deferred BridgeAdapter path: not zero (no \
             GAP) and not two (no duplicate). RED if the worker bridge tail is \
             dropped (count == 0, GAP) or if the observer also relays (count == 2)."
    );
}

/// #3154 P1 (BridgeAdapter-GAP) — the symmetric guard: when the deferred claim
/// resolves to the WATCHER, the worker must NOT spawn a bridge tail (the watcher
/// is the sole relayer). This pins the owner-kind-awareness in the OTHER
/// direction: neutralizing the branch so the worker spawns unconditionally would
/// push the watcher path to relayer_count == 2 (DUPLICATE) and turn this RED.
#[test]
fn deferred_claim_resolves_watcher_owner_worker_bridge_tail_stands_down() {
    let resolved_owner = ExternalInputRelayOwner::TmuxWatcher;
    let observer_relays = observer_should_spawn_bridge_tail(true, resolved_owner);
    let watcher_relays = matches!(resolved_owner, ExternalInputRelayOwner::TmuxWatcher);
    let worker_bridge_tail = deferred_claim_requires_bridge_tail_relayer(resolved_owner);
    assert!(
        !worker_bridge_tail,
        "watcher-owned deferred claim MUST NOT get a worker bridge tail — else \
             DUPLICATE relay (the watcher already relays)"
    );
    let relayer_count =
        u8::from(observer_relays) + u8::from(watcher_relays) + u8::from(worker_bridge_tail);
    assert_eq!(
        relayer_count, 1,
        "EXACTLY ONE relayer on the deferred watcher path (the watcher); RED if \
             the worker also spawns a bridge tail (count == 2, DUPLICATE)."
    );
}

/// Adoption must NOT fire when the claim FAILED — a false claim leaves the
/// owner untouched (the worker retries; nothing relays yet, by design).
#[test]
fn failed_claim_does_not_adopt_owner() {
    assert!(
        !claim_should_adopt_relay_owner(
            false,
            ExternalInputRelayOwner::BridgeAdapter,
            ExternalInputRelayOwner::TmuxWatcher,
        ),
        "a failed claim must not re-record a watcher owner (the turn was not \
             actually claimed) — RED if adoption ignores the `claimed` flag"
    );
    // No-op when the owner did not change.
    assert!(!claim_should_adopt_relay_owner(
        true,
        ExternalInputRelayOwner::TmuxWatcher,
        ExternalInputRelayOwner::TmuxWatcher,
    ));
}

/// #3296 (RED-3): the ABORT cleanup hook records a durable aborted-anchor
/// marker and NO LONGER applies any reaction itself — the old #3282 path
/// swapped `⏳ → ⚠` here, branding answered messages as failures. RED on
/// the pre-#3296 code: no marker module/store exists and a `⚠` is added.
/// codex r2 reverses the r1 tail: with the foreign row gone at the record
/// instant the marker must pin the worker's LAST-VIEW identity and stay
/// UNCOVERED unless a commit tombstone proves the deletion was a commit —
/// RED on the r1 code (row-absence alone pre-covered the marker, false-✅
/// ing force-cleared unanswered anchors).
/// (Reaction-op accounting lives in `tui_direct_abort_marker`'s own tests;
/// this hook performs Discord IO only through that module, never directly.)
#[test]
fn abort_cleanup_records_marker_and_keeps_hourglass() {
    // Durable BASE-root injection via the marker module's THREAD-LOCAL
    // test seam (never the process-global `AGENTDESK_ROOT_DIR` env —
    // mutating it races env-reading tests that hold no lock, e.g. the
    // pending-start worker tests' `persist()`). The current-thread
    // `block_on` below keeps the cleanup future on this thread so the
    // override resolves inside it.
    struct RootReset;
    impl Drop for RootReset {
        fn drop(&mut self) {
            super::super::tui_direct_abort_marker::set_test_root_override(None);
        }
    }
    let _root_reset = RootReset;
    let temp = tempfile::tempdir().unwrap();
    super::super::tui_direct_abort_marker::set_test_root_override(Some(temp.path().to_path_buf()));

    let shared = super::super::make_shared_data_for_tests();
    let record = super::super::tui_direct_pending_start::TuiDirectPendingStart {
        provider: "claude".to_string(),
        channel_id: 4242,
        tmux_session_name: "tmux-4242".to_string(),
        prompt_text: "/loop tick".to_string(),
        anchor_message_id: 777_001,
        lease_relay_owner: "bridge_adapter".to_string(),
        lease_runtime_kind: Some("claude_tui".to_string()),
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: super::super::tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
    };
    let cleanup = pending_start_abort_cleanup_fn();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // No inflight row exists for this channel in the test env (the row
    // vanished post-final-view); the worker's last-view identity is what
    // the marker must pin (codex r2).
    let last_view = Some((888_777_u64, "2026-06-10 12:00:00".to_string()));
    rt.block_on(cleanup(&shared, &record, last_view.clone()));

    let markers = super::super::tui_direct_abort_marker::load_for_channel("claude", 4242);
    assert_eq!(
        markers.len(),
        1,
        "the ABORT hook must persist exactly one durable aborted-anchor \
             marker — RED on the old ⚠-swap path (no marker store existed)"
    );
    assert_eq!(
        markers[0].anchor_message_id, 777_001,
        "identity-pinned (I4)"
    );
    assert_eq!(
        markers[0].foreign_user_msg_id,
        Some(888_777),
        "row gone at the record instant ⇒ the LAST-VIEW identity is pinned \
             (codex r2) — RED if None (the marker would be sweep-only)"
    );
    assert_eq!(
        markers[0].covered_at_ms, None,
        "no commit tombstone ⇒ UNCOVERED (codex r2 — RED on the r1 \
             pre-covered promotion: bare row-absence is not commit evidence)"
    );

    // With a commit tombstone matching the last-view identity, the same
    // row-gone abort records COVERED (evidence-backed — the deletion WAS
    // the prior owner's terminal commit).
    super::super::tui_direct_abort_marker::record_commit_tombstone_at(
        55_000,
        "claude",
        "tmux-4242",
        4242,
        888_777,
        "2026-06-10 12:00:00",
    );
    let record_b = super::super::tui_direct_pending_start::TuiDirectPendingStart {
        anchor_message_id: 777_002,
        ..record.clone()
    };
    rt.block_on(cleanup(&shared, &record_b, last_view));
    let covered = super::super::tui_direct_abort_marker::load_for_channel("claude", 4242)
        .into_iter()
        .find(|m| m.anchor_message_id == 777_002)
        .expect("second marker recorded");
    assert_eq!(
        covered.covered_at_ms,
        Some(55_000),
        "matching tombstone at record time ⇒ evidence-backed cover (r2)"
    );

    // Zero anchor id (I5): nothing recorded, nothing panics.
    let zero = super::super::tui_direct_pending_start::TuiDirectPendingStart {
        anchor_message_id: 0,
        ..record
    };
    rt.block_on(cleanup(&shared, &zero, None));
    assert_eq!(
        super::super::tui_direct_abort_marker::load_for_channel("claude", 4242).len(),
        2,
        "a zero anchor id must never be recorded (I5)"
    );
}

// #3018: the tmux_watchers registry is the SINGLE authority for
// tmux-session→channel resolution. When the registry has a mapping it wins
// outright (the dedupe mirror is never consulted as a reverse authority).
#[test]
fn registry_is_authoritative_for_owner_channel_resolution() {
    let registry_channel = ChannelId::new(123_000_000_000_000);

    // Registry hit, no mirror.
    assert_eq!(
        resolve_owner_channel_authoritatively("tmux-a", Some(registry_channel), None),
        Some(registry_channel),
    );

    // Registry hit takes precedence even when the mirror disagrees.
    assert_eq!(
        resolve_owner_channel_authoritatively(
            "tmux-a",
            Some(registry_channel),
            Some(999_000_000_000_000),
        ),
        Some(registry_channel),
        "registry must win over a disagreeing dedupe mirror"
    );
}

// #3018: a registry miss while the dedupe mirror still holds a mapping is
// observable drift. The resolver must NOT fall back to the mirror; it
// returns None (the warn drift alert is emitted as a side effect).
#[test]
fn registry_miss_but_dedupe_hit_drops_and_does_not_use_mirror() {
    assert_eq!(
        resolve_owner_channel_authoritatively("tmux-drift", None, Some(456_000_000_000_000),),
        None,
        "dedupe mirror must never act as a reverse routing authority"
    );

    // Both miss → None.
    assert_eq!(
        resolve_owner_channel_authoritatively("tmux-empty", None, None),
        None,
    );
}

// #3656: the owner-resolution chokepoint must arm repair on the first drift drop.
#[cfg(unix)]
#[tokio::test]
async fn owner_channel_chokepoint_triggers_drift_repair_on_drift_drop() {
    let shared = super::super::make_shared_data_for_tests();
    let tmux = "AgentDesk-claude-drift-chokepoint-fix3656";
    let owner = ChannelId::new(1_504_468_805_772_903_656);

    crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        None,
        "registry miss + mirror hit still drops rather than routing from the mirror"
    );
    assert!(
        super::super::idle_relay_drift::repair_attempt_recorded_for_tests(tmux),
        "the chokepoint must arm a repair attempt on the first drift drop"
    );

    crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
}

// #3105: a LIVE TUI session where the dedupe mirror holds a channel but the
// `tmux_watchers` registry is missing must NOT be permanently dropped. The
// fix self-heals by promoting the authoritative (settings-derived) channel
// into the registry — NOT by routing from the mirror. This end-to-end relay
// test asserts: (1) before repair the resolver drops (registry single
// authority); (2) the dedupe mirror alone is never used as the routing
// owner; (3) after an authoritative registry restore the relay routes again.
#[test]
fn live_session_relay_self_heals_via_authoritative_registry_not_mirror() {
    let shared = super::super::make_shared_data_for_tests();
    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471";
    let owner = ChannelId::new(1_504_468_805_772_902_471);

    // The dedupe mirror has a mapping (live TUI session), but the
    // authoritative registry misses (slot evicted by compact/restart/rebind).
    crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
    assert_eq!(
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux),
        Some(owner.get()),
        "precondition: dedupe mirror holds the live session's channel"
    );

    // (1)+(2): the mirror alone must never be used as the delivery owner —
    // the resolver drops (the #3018 single-authority rule stays intact).
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        None,
        "registry miss + dedupe mirror hit must drop, never route from the mirror"
    );

    // (3): an authoritative registry restore (what the rehydrate loop does
    // from the settings-derived channel) makes the live session route again.
    let repaired = shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(tmux, owner);
    assert!(
        repaired,
        "first restore reports a change (single bounded incident)"
    );
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        Some(owner),
        "after authoritative re-registration the live session must route again"
    );

    // Cleanup shared global dedupe state for cross-test isolation.
    crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
}

// #3306: a drift-triggered self-heal (the new path for ROUTINE sessions that
// have NO settings binding) must promote a durable channel via the SAME
// authoritative `restore_owner_channel_for_tmux_session` registry path the
// #3105 rehydrate uses — proving the registry stays the single authority and
// the resolver routes again after the drift WARN drop. The decision-core
// tests (`idle_relay_drift`) prove WHICH durable source is chosen and the
// mis-delivery guards; this end-to-end test pins the registry promotion +
// resolver hand-off.
//
// #3356: gated to unix to match the `test_watcher_handle` helper (def at
// `#[cfg(unix)]` below) it consumes — every other caller of that helper is
// already `#[cfg(unix)]`, so this restores def/usage cfg symmetry and lets
// the windows leg compile (the helper does not exist on windows).
#[cfg(unix)]
#[test]
fn drift_triggered_restore_makes_routine_session_route_again() {
    let shared = super::super::make_shared_data_for_tests();
    // A routine tmux name that matches no settings channel binding (the
    // exact class that drifts permanently before #3306).
    let tmux = "AgentDesk-claude-routine-token-daily-report---token-manager";
    let owner = ChannelId::new(1_512_635_194_124_013_681);

    // Drift precondition: mirror holds a mapping, registry misses ⇒ drop.
    crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner.get());
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        None,
        "registry miss + mirror hit must drop (drift), never route from mirror"
    );

    // The drift repair (durable source promotion) re-registers the owner via
    // the authoritative restore path — exactly what `attempt_drift_repair`
    // does on a passing `RepairDecision::Promote`.
    let repaired = shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(tmux, owner);
    assert!(repaired, "first drift-triggered restore reports a change");
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        Some(owner),
        "after the drift-triggered authoritative restore the session routes again"
    );

    // Live truth wins: a real watcher claim for the session must own it
    // authoritatively, so a subsequent restore no-ops (the restored entry
    // can never shadow a live watcher).
    shared.tmux_watchers.insert(
        owner,
        test_watcher_handle(tmux, Path::new("/tmp/nope.jsonl")),
    );
    assert!(
        !shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(tmux, owner),
        "restore must no-op while a live watcher owns the session (live truth wins)"
    );

    // Cleanup shared global state for cross-test isolation.
    shared.tmux_watchers.remove(&owner);
    crate::services::tui_prompt_dedupe::evict_dead_tmux_mirror(tmux);
}

// #3105 (codex P1 sub-case B): a DEAD/orphaned tmux session (pane gone, not
// present on this host) whose dedupe mirror still holds a stale ClaudeTui
// runtime binding + channel mapping must NOT spam the per-poll drift/skip
// WARN forever. After the rehydrate pass evicts the mirror, the next idle
// relay iteration finds NO runtime binding to iterate and NO channel mapping
// to drift on — proving the 0.5s spam is stopped. A unique, never-created
// session name guarantees `tmux_session_has_live_pane` is false.
#[cfg(unix)]
#[test]
fn dead_orphaned_session_mirror_is_evicted_and_stops_drift_spam() {
    let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let shared = super::super::make_shared_data_for_tests();
    // A session that does not exist on this host (pane gone / orphaned).
    let tmux = "AgentDesk-claude-adk-cc-t1504468805772902471-DEAD-ORPHAN-fix3105";
    let owner = 1_504_468_805_772_902_471u64;

    // Seed the stale dedupe mirror exactly as a dead/orphaned session leaves it:
    // a ClaudeTui runtime binding (what the relay loop iterates) and a
    // last-seen channel mapping (what the drift-alert resolver reads).
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/claude-transcript-dead-orphan.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    crate::services::tui_prompt_dedupe::register_tmux_channel(tmux, owner);

    // Preconditions: the relay loop WOULD iterate this binding and drift.
    assert!(
        crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
            RuntimeHandoffKind::ClaudeTui
        )
        .iter()
        .any(|(name, _)| name == tmux),
        "precondition: dead session's binding is in the relay loop's iteration set"
    );
    assert_eq!(
        owner_channel_for_tmux_session(&shared, &ProviderKind::Claude, tmux),
        None,
        "precondition: registry misses + mirror hit == the drift the relay loop hits"
    );
    // The session is genuinely dead/orphaned (no live pane, no live watcher).
    assert!(
        claude_tui_session_is_dead_orphaned(&shared, tmux),
        "precondition: a never-created session is dead/orphaned"
    );

    // The rehydrate pass runs the eviction. (rehydrate_existing_claude_tui_bindings
    // calls evict_dead_orphaned_claude_tui_mirrors first; we call it directly
    // so the assertion does not depend on a live tmux binary for list-sessions.)
    evict_dead_orphaned_claude_tui_mirrors(&shared);

    // After eviction: the relay loop iterates an EMPTY set for this session,
    // and the drift-alert resolver finds NO mapping → no drift/skip WARN.
    assert!(
        !crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
            RuntimeHandoffKind::ClaudeTui
        )
        .iter()
        .any(|(name, _)| name == tmux),
        "the stale runtime binding must be evicted so the relay loop no longer iterates it"
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::owner_channel_for_tmux_session(tmux),
        None,
        "the stale channel mirror must be evicted so no drift WARN can fire"
    );

    // Idempotent on the next pass (the ~0.5s repeat): no binding, no work,
    // no second incident — proving the spam is bounded to one line.
    evict_dead_orphaned_claude_tui_mirrors(&shared);
    assert!(
        !crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
            RuntimeHandoffKind::ClaudeTui
        )
        .iter()
        .any(|(name, _)| name == tmux),
        "subsequent iterations stay clean (0.5s spam stopped)"
    );
}

// #3105 (codex P1 sub-case A guard): a LIVE thread-suffixed session whose
// authoritative registry entry was evicted must NOT be treated as
// dead/orphaned — its mirror must survive so the live self-heal path can
// re-register the authoritative owner. We assert the dead-orphaned predicate
// is gated on pane-liveness: with a live watcher handle present the predicate
// is false even though the registry owner map is otherwise empty.
#[cfg(unix)]
#[test]
fn live_session_with_watcher_handle_is_not_dead_orphaned() {
    let shared = super::super::make_shared_data_for_tests();
    let tmux = "AgentDesk-claude-adk-cc-LIVE-fix3105";
    let owner = ChannelId::new(1_504_468_805_772_902_471);

    // A live watcher handle owns the session → it is NOT dead/orphaned even
    // though the host has no real tmux pane for this synthetic name.
    let dir = std::env::temp_dir();
    let output_path = dir.join("claude-live-fix3105.jsonl");
    shared
        .tmux_watchers
        .insert(owner, test_watcher_handle(tmux, &output_path));
    assert!(
        shared.tmux_watchers.has_live_watcher_handle(tmux),
        "precondition: a live watcher handle owns the session"
    );
    assert!(
        !claude_tui_session_is_dead_orphaned(&shared, tmux),
        "a session with a live watcher handle must never be tombstoned as dead/orphaned"
    );
}

// #3105 (codex P2): a LIVE session with no watcher handle whose FIRST pane
// probe flakes (reads not-live) but whose subsequent probes report live must
// NOT be classified dead/orphaned. A single transient negative read can never
// trigger the destructive eviction, so the live session keeps its mirror and
// self-heal path. We drive the pure predicate with a scripted probe sequence
// [false, true] so the flake is deterministic (no real tmux needed).
#[cfg(unix)]
#[test]
fn transient_pane_flake_on_live_session_is_not_dead_orphaned() {
    use std::cell::RefCell;

    // First probe flakes (not live), second probe reports live.
    let live_reads = RefCell::new(vec![false, true].into_iter());
    let is_dead = pane_is_confirmed_dead_orphaned(
        || live_reads.borrow_mut().next().unwrap_or(true),
        // session_exists must NOT even be consulted once a live pane is seen.
        || panic!("session_exists must not be probed once a live pane is observed"),
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        None,
    );
    assert!(
        !is_dead,
        "a single flaky negative pane read followed by a live read must NOT be dead/orphaned"
    );
}

// #3105 (codex P2 / sub-case B regression): a genuinely-gone session reads no
// live pane on EVERY sample AND the hard has-session check confirms it does
// not exist → it is still classified dead/orphaned, so the per-poll WARN spam
// is still stopped. The retries must not make the real dead session immortal.
#[cfg(unix)]
#[test]
fn genuinely_gone_session_is_still_dead_orphaned_after_retries() {
    use std::cell::Cell;

    let probe_count = Cell::new(0usize);
    let is_dead = pane_is_confirmed_dead_orphaned(
        || {
            probe_count.set(probe_count.get() + 1);
            false // never a live pane
        },
        || false, // hard has-session: session truly gone
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        None,
    );
    assert!(
        is_dead,
        "a session with no live pane across all samples AND no has-session must still evict"
    );
    assert_eq!(
        probe_count.get(),
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        "all configured samples must be taken before declaring a session dead"
    );
}

// #3105 (codex P2): the weakest-signal guard. Even when every soft pane probe
// reports dead, if the hard `tmux has-session` check still finds the session
// present on this host (a transient pane read with the session very much
// alive), it must NOT be evicted — "no live pane" alone is never sufficient
// when there is no watcher handle.
#[cfg(unix)]
#[test]
fn confirmed_existing_session_is_not_dead_even_if_pane_probes_flake() {
    let is_dead = pane_is_confirmed_dead_orphaned(
        || false, // soft pane probe: reads dead on every sample
        || true,  // hard has-session: the session IS present on this host
        DEAD_ORPHANED_PANE_PROBE_SAMPLES,
        None,
    );
    assert!(
        !is_dead,
        "a session still present per has-session must not be evicted on soft pane reads alone"
    );
}

#[test]
fn formats_ssh_direct_prompt_notification() {
    let output = format_ssh_direct_prompt_notification("claude", "AgentDesk-claude-a", "hi");

    assert!(output.contains("터미널에 직접 주입된 입력"));
    assert!(output.contains("(tmux : `AgentDesk-claude-a`)"));
    assert!(output.contains("```text\nhi\n```"));
}

#[test]
fn formats_ssh_direct_prompt_notification_with_truncation() {
    let prompt = "x".repeat(SSH_DIRECT_PROMPT_PREVIEW_LIMIT + 20);
    let output = format_ssh_direct_prompt_notification("codex", "AgentDesk-codex-a", &prompt);

    assert!(output.contains("터미널에 직접 주입된 입력"));
    assert!(output.contains("(tmux : `AgentDesk-codex-a`)"));
    assert!(output.contains("..."));
    assert!(output.len() < prompt.len() + 120);
}

#[test]
fn formats_ssh_direct_prompt_notification_escapes_code_fence() {
    let output = format_ssh_direct_prompt_notification("codex", "tmux`name", "a ``` fence");

    assert!(output.contains("(tmux : `tmux'name`)"));
    assert!(output.contains("a ` ` ` fence"));
}

#[test]
fn formats_ssh_direct_prompt_notification_strips_terminal_controls() {
    let output = format_ssh_direct_prompt_notification(
        "claude",
        "AgentDesk-claude-a",
        "\u{15}\u{1b}[31mhello\u{1b}[0m\n\tworld",
    );

    assert!(output.contains("hello\n\tworld"));
    assert!(!output.contains('\u{15}'));
    assert!(!output.contains('\u{1b}'));
}

// U-4 Bare control bytes (BEL, FF, DEL, C1 NEXT LINE) in the SSH-direct
// notification path must be silently dropped — they would otherwise
// disrupt Discord rendering or terminal mirrors that re-paste the text.
// Newline, carriage return, and tab are preserved by design.
#[test]
fn notification_strip_drops_bare_control_bytes_but_keeps_whitespace() {
    let raw = "\u{07}ring\u{0c}page\u{7f}del\u{85}c1\n\tkeep";

    let output = format_ssh_direct_prompt_notification("claude", "tmux-1", raw);

    for forbidden in ['\u{07}', '\u{0c}', '\u{7f}', '\u{85}'] {
        assert!(
            !output.contains(forbidden),
            "control byte {:?} leaked into notification: {:?}",
            forbidden,
            output
        );
    }
    assert!(output.contains("ringpagedelc1\n\tkeep"));
}

// #3100: a human typing directly into the TUI is a real active turn.
#[test]
fn classify_injected_prompt_human_direct_input() {
    assert_eq!(
        classify_injected_prompt("please review PR #1234"),
        InjectedPromptClass::HumanTuiDirect,
    );
    assert!(classify_injected_prompt("hi").is_human_active_turn());
}

// #4567: a `<task-notification>` is a status/card event, not positive
// human-input provenance; it must not claim an external user-turn lifecycle.
#[test]
fn task_notification_lifecycle_is_not_an_external_turn() {
    let decision = relay_observed_prompt_injected_prompt_decision(
        "<task-notification><status>killed</status></task-notification>",
    );
    assert_eq!(
        decision.injected_class,
        InjectedPromptClass::TaskNotificationEvent
    );
    assert!(
        !decision.starts_external_turn_lifecycle(),
        "killed task status must not claim synthetic ownership"
    );
}

#[tokio::test]
async fn task_notification_status_only_preserves_existing_turn_request_anchor() {
    let shared = super::super::make_shared_data_for_tests();
    let channel_id = ChannelId::new(940_000_000_004_567);
    let existing_anchor = 940_000_000_004_568;
    let tmux = "AgentDesk-claude-4567-status-only-anchor";
    shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(tmux, channel_id);
    shared
        .ui
        .placeholder_live_events
        .set_turn_request_anchor(channel_id, Some(existing_anchor));
    let prompt = ObservedTuiPrompt {
        provider: ProviderKind::Claude.as_str().to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: "<task-notification><status>killed</status></task-notification>".to_string(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };

    relay_observed_prompt(&shared, prompt).await;

    assert_eq!(
        shared
            .ui
            .placeholder_live_events
            .request_user_msg_id_for_test(channel_id),
        Some(existing_anchor),
        "status-only task notifications must not clear or replace the existing Discord turn request anchor",
    );
}

#[test]
fn classify_injected_prompt_task_notification_event() {
    let bare = "<task-notification><status>completed</status><task_id>codex-background-event</task_id></task-notification>";
    assert_eq!(
        classify_injected_prompt(bare),
        InjectedPromptClass::TaskNotificationEvent,
    );
    // Tolerates a leading terminal-control prefix some injectors prepend.
    assert_eq!(
        classify_injected_prompt(
            "\u{1b}[0m<task-notification><status>completed</status></task-notification>"
        ),
        InjectedPromptClass::TaskNotificationEvent,
    );
    // An attribute-form opening tag is still recognised.
    assert_eq!(
        classify_injected_prompt("<task-notification kind=\"background\"></task-notification>"),
        InjectedPromptClass::TaskNotificationEvent,
    );
    assert_eq!(
        classify_injected_prompt("<task-notification\nkind=\"background\"></task-notification>"),
        InjectedPromptClass::TaskNotificationEvent,
        "task-notification detection must accept newline attribute boundaries",
    );
    assert_eq!(
        classify_injected_prompt("<task-notification\tkind=\"background\"></task-notification>"),
        InjectedPromptClass::TaskNotificationEvent,
        "task-notification detection must accept tab attribute boundaries",
    );
    let wrapped = format!("터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n{bare}\n```");
    assert_eq!(
        classify_injected_prompt(&wrapped),
        InjectedPromptClass::TaskNotificationEvent,
        "wrapped task notification must classify after peeling the direct-injection wrapper",
    );
    let quoted = "please inspect this log line:\n\
<task-notification><status>completed</status></task-notification>";
    assert_eq!(
        classify_injected_prompt(quoted),
        InjectedPromptClass::HumanTuiDirect,
        "a human quote of the tag mid-body must stay a normal direct prompt",
    );
    assert_eq!(
        classify_injected_prompt("<task-notification-extra></task-notification-extra>"),
        InjectedPromptClass::HumanTuiDirect,
        "task-notification detection must honor tag boundaries",
    );
    assert!(!classify_injected_prompt(bare).is_human_active_turn());
}

// #3716: Codex subagent completions arrive as start-anchored
// `<subagent_notification>{json}</subagent_notification>` envelopes. They are
// neutral machine events, not human direct prompts.
#[test]
fn classify_injected_prompt_subagent_notification_event() {
    let unwrapped = r#"<subagent_notification>
{"agent_path":"/tmp/agent","status":{"completed":"Read-only review complete."}}
</subagent_notification>"#;
    assert_eq!(
        classify_injected_prompt(unwrapped),
        InjectedPromptClass::SubagentNotificationEvent,
    );
    assert!(
        super::injected_prompt_policy::is_start_anchored_subagent_notification(unwrapped),
        "bare subagent_notification must pass the start-anchored detector"
    );

    let wrapped = "터미널에 직접 주입된 입력 (tmux : `AgentDesk-codex-adk-cdx`):\n```text\n\
<subagent_notification>\n{\"status\":{\"completed\":\"done\"}}\n</subagent_notification>\n```";
    assert_eq!(
        classify_injected_prompt(wrapped),
        InjectedPromptClass::SubagentNotificationEvent,
    );
    assert!(
        super::injected_prompt_policy::is_start_anchored_subagent_notification(wrapped),
        "wrapped subagent_notification must pass after peeling the direct-injection wrapper"
    );

    let newline_attr = "<subagent_notification\nkind=\"worker\">{\"status\":{\"completed\":\"done\"}}</subagent_notification>";
    assert_eq!(
        classify_injected_prompt(newline_attr),
        InjectedPromptClass::SubagentNotificationEvent,
        "subagent_notification detection must accept newline attribute boundaries",
    );
    let tab_attr = "<subagent_notification\tkind=\"worker\">{\"status\":{\"completed\":\"done\"}}</subagent_notification>";
    assert_eq!(
        classify_injected_prompt(tab_attr),
        InjectedPromptClass::SubagentNotificationEvent,
        "subagent_notification detection must accept tab attribute boundaries",
    );
    assert_eq!(
        classify_injected_prompt("<subagent_notification_extra>{}</subagent_notification_extra>"),
        InjectedPromptClass::HumanTuiDirect,
        "subagent_notification detection must honor tag boundaries",
    );

    let quoted = "please inspect this log line:\n<subagent_notification>{\"status\":{\"completed\":\"x\"}}</subagent_notification>";
    assert_eq!(
        classify_injected_prompt(quoted),
        InjectedPromptClass::HumanTuiDirect,
        "a human quote of the envelope mid-body must stay a normal direct prompt",
    );
    assert!(
        !super::injected_prompt_policy::is_start_anchored_subagent_notification(quoted),
        "mid-body quote must not pass the start-anchored detector"
    );
}

#[test]
fn subagent_notification_card_completed_hides_raw_envelope() {
    let prompt = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Make /api/docs route-derived.\n2. Resolve generated-doc drift policy."}}
</subagent_notification>"#;

    let output = format_subagent_notification_card("AgentDesk-codex-adk-cdx", prompt);

    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Read-only review complete."));
    assert!(output.contains("1. Make /api/docs route-derived."));
    assert!(output.contains("(tmux : `AgentDesk-codex-adk-cdx`)"));
    assert!(!output.contains("터미널에 직접 주입된 입력"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private-agent"));
    assert!(!output.contains("{\""));
}

#[test]
fn subagent_notification_card_failed_hides_raw_envelope() {
    let prompt = r#"<subagent_notification>{"status":{"failed":"Review failed: missing test coverage."}}</subagent_notification>"#;

    let output = format_subagent_notification_card("tmux`name", prompt);

    assert!(output.contains("Subagent failed"));
    assert!(output.contains("Review failed: missing test coverage."));
    assert!(output.contains("(tmux : `tmux'name`)"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("{\"status\""));
}

#[test]
fn subagent_notification_card_truncates_long_report() {
    let long_report = "x".repeat(1_200);
    let prompt = format!(
        "<subagent_notification>{{\"status\":{{\"completed\":\"{long_report}\"}}}}</subagent_notification>"
    );

    let output = format_subagent_notification_card("sess", &prompt);

    assert!(output.contains("Subagent completed"));
    assert!(output.contains("..."));
    assert!(output.len() < prompt.len());
}

#[test]
fn subagent_notification_card_malformed_payload_omits_raw_payload() {
    let prompt =
        "<subagent_notification>{not-json agent_path=/tmp/private}</subagent_notification>";

    let output = format_subagent_notification_card("sess", prompt);

    assert!(output.contains("Subagent notification"));
    assert!(output.contains("malformed payload omitted"));
    assert!(!output.contains("not-json"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private"));
    assert!(!output.contains("<subagent_notification>"));
}

// #3393/#3730: the live-panel terminal BRIDGE and system-event classifier are
// both gated on a START-ANCHORED check. A human direct prompt that QUOTES a
// notification (embedding a LIVE tool-use-id) must stay a human prompt and must
// not push terminal StatusEvents — so a quoted id cannot false-close a real
// running slot. A bare real-shape record (incl. a leading injection-wrapper
// round-trip) still bridges.
#[test]
fn bridge_guard_is_start_anchored_not_contains() {
    // Human prompt quoting a notification mid-message, with a LIVE tool-use-id
    // that matches a real running slot: NOT start-anchored → no bridge.
    let quoted = "please re-run this, it printed:\n\
            <task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>";
    assert!(
        !is_start_anchored_task_notification(quoted),
        "a mid-message quoted notification must NOT pass the bridge guard"
    );
    assert_eq!(
        classify_injected_prompt(quoted),
        InjectedPromptClass::HumanTuiDirect,
        "a mid-message quoted notification must not classify as a system event",
    );

    // A bare, start-anchored real-shape record bridges.
    let bare = "<task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>";
    assert!(
        is_start_anchored_task_notification(bare),
        "a bare start-anchored notification must pass the bridge guard"
    );

    // Leading terminal-control prefix is tolerated (stripped by the pipeline).
    let ansi_prefixed = "\u{1b}[0m<task-notification><status>completed</status>\
            </task-notification>";
    assert!(
        is_start_anchored_task_notification(ansi_prefixed),
        "an ANSI-prefixed notification must still pass the bridge guard"
    );

    // SSH-direct injection-wrapper round-trip variant: the wrapper line + code
    // fence are peeled by `strip_leading_injection_wrapper`, leaving the tag
    // start-anchored → still bridges (mirrors the #3153 wrapper coverage).
    let wrapped = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
            <task-notification><tool-use-id>toolu_live_slot</tool-use-id>\
            <status>completed</status>\
            <summary>Agent \"x\" completed</summary></task-notification>\n```";
    assert!(
        is_start_anchored_task_notification(wrapped),
        "an injection-wrapper round-trip notification must pass the bridge guard"
    );
}

// #3100: a compact/system continuation prologue is NOT a human request and
// must classify away from the active-turn lifecycle.
#[test]
fn classify_injected_prompt_system_continuation() {
    assert_eq!(
        classify_injected_prompt(
            "This session is being continued from a previous conversation that ran out of context... Summary:"
        ),
        InjectedPromptClass::SystemContinuation,
    );
    assert_eq!(
        classify_injected_prompt("Please continue the conversation from where we left it off"),
        InjectedPromptClass::SystemContinuation,
    );
    assert!(
        !classify_injected_prompt("This session is being continued from a previous conversation")
            .is_human_active_turn()
    );
}

// #3153: a MACHINE slash-command control echo must classify as
// SlashCommandControl — the raw `/loop …` ScheduleWakeup echo (HALF A of the
// double-post), the Claude Code expanded `<command-*>` wrapper (HALF B), the
// raw `/compact` echo (whole-token, incl. bare no-arg), and the
// `<local-command-stdout>Compacted` stdout line. #3178 (codex fix): a machine
// slash turn is now a FULL active turn (NOT suppressed) so concurrent input
// queues; it gets a kind-only anchor + ⏳ + synthetic inflight + ✅, and the
// near-simultaneous duplicate half is collapsed by the 2s dedupe gate.
#[test]
fn classify_injected_prompt_slash_command_control() {
    // HALF A — raw /loop echo.
    assert_eq!(
        classify_injected_prompt("/loop 5m /foo"),
        InjectedPromptClass::SlashCommandControl,
    );
    // HALF B — Claude Code expanded <command-*> wrapper.
    let wrapper = "<command-message>loop is running…</command-message>\
                       <command-name>/loop</command-name><command-args>5m /foo</command-args>";
    assert_eq!(
        classify_injected_prompt(wrapper),
        InjectedPromptClass::SlashCommandControl,
    );
    // Raw /compact echo with args.
    assert_eq!(
        classify_injected_prompt("/compact focus on the relay"),
        InjectedPromptClass::SlashCommandControl,
    );
    // Bare no-arg /compact (whole-token, EOS).
    assert_eq!(
        classify_injected_prompt("/compact"),
        InjectedPromptClass::SlashCommandControl,
    );
    // /compact command stdout line.
    assert_eq!(
        classify_injected_prompt("<local-command-stdout>Compacted (12.3k tokens)"),
        InjectedPromptClass::SlashCommandControl,
    );
    // /model command stdout line: local command output is a machine echo, not
    // a human TUI-direct prompt that should mint a synthetic turn.
    assert_eq!(
        classify_injected_prompt(
            "<local-command-stdout>Set model to Fable 5</local-command-stdout>"
        ),
        InjectedPromptClass::SlashCommandControl,
    );
    let command_name_first = compact_command_name_first_stub();
    assert_eq!(command_name_first.chars().count(), 134);
    assert_eq!(
        classify_injected_prompt(command_name_first),
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(slash_command_control_kind(command_name_first), "/compact");
    assert!(is_local_only_slash_command_prompt(command_name_first));

    // #3178 (codex fix): a machine slash turn is a FULL active turn — NOT
    // suppressed from the user-turn lifecycle (so concurrent input queues),
    // yet it is not a HUMAN active turn (no raw render), and it still delivers
    // assistant output via the bridge tail.
    let ctrl = InjectedPromptClass::SlashCommandControl;
    assert!(
        !ctrl.suppresses_user_turn_lifecycle(),
        "a machine slash turn must NOT suppress the active-turn lifecycle (it claims a full active turn so concurrent input queues)"
    );
    assert!(!ctrl.is_human_active_turn());
    assert!(ctrl.still_delivers_assistant_output());
}

// #3153 double-echo + envelope coverage: a /loop or wrapper echo that
// round-trips through the SSH-direct injection envelope is still anchored
// (strip_leading_injection_wrapper peels the leading wrapper before the
// starts_with anchors) and still classifies as SlashCommandControl.
#[test]
fn classify_injected_prompt_wrapped_slash_command_control() {
    let wrapped_loop = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n/loop 5m /foo\n```";
    assert_eq!(
        classify_injected_prompt(wrapped_loop),
        InjectedPromptClass::SlashCommandControl,
    );
    let wrapped_wrapper = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
                               <command-message>/loop</command-message>\n```";
    assert_eq!(
        classify_injected_prompt(wrapped_wrapper),
        InjectedPromptClass::SlashCommandControl,
    );
    let caveat_wrapped = format!(
        "<local-command-caveat>local commands are synthetic</local-command-caveat>\n{}",
        compact_command_name_first_stub(),
    );
    assert_eq!(
        classify_injected_prompt(&caveat_wrapped),
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(slash_command_control_kind(&caveat_wrapped), "/compact");
    assert!(is_local_only_slash_command_prompt(&caveat_wrapped));

    let caveat_only = "<local-command-caveat>local commands are synthetic</local-command-caveat>";
    assert_eq!(
        classify_injected_prompt(caveat_only),
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(slash_command_control_kind(caveat_only), "slash");
    assert!(is_local_only_slash_command_prompt(caveat_only));
}

// #3153 FALSE-POSITIVE GUARD: a human merely quoting "/loop" / "/compact"
// mid-message must NOT be misclassified — detection is START-ANCHORED, and
// "/compactX" (no whole-token boundary) must also stay a human turn.
#[test]
fn classify_injected_prompt_human_quote_of_slash_is_not_control() {
    let human = "Why does /loop keep appearing in my logs?";
    assert_eq!(
        classify_injected_prompt(human),
        InjectedPromptClass::HumanTuiDirect,
        "a human quoting /loop mid-message must stay a human turn",
    );
    assert!(classify_injected_prompt(human).is_human_active_turn());

    // "/compactfoo" is not the whole `/compact` token → human turn.
    assert_eq!(
        classify_injected_prompt("/compactfoo do the thing"),
        InjectedPromptClass::HumanTuiDirect,
    );

    // A human leading line that merely opens with the wrapped envelope but
    // whose body is a plain request stays a human turn.
    let wrapped_human =
        "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\nplease /loop later maybe\n```";
    assert_eq!(
        classify_injected_prompt(wrapped_human),
        InjectedPromptClass::HumanTuiDirect,
    );
    let quoted_command_name =
        "Why did the transcript include <command-name>/compact</command-name>?";
    assert_eq!(
        classify_injected_prompt(quoted_command_name),
        InjectedPromptClass::HumanTuiDirect,
    );
    let quoted_caveat =
        "The log contains <local-command-caveat>x</local-command-caveat> before XML.";
    assert_eq!(
        classify_injected_prompt(quoted_caveat),
        InjectedPromptClass::HumanTuiDirect,
    );
}

// #3305: a local-completing pass-through command's `<command-*>` echo (or its
// raw `/compact` echo) must be detected as local-only so the idle relay skips
// the synthetic-turn lifecycle (no ⏳ anchor, no inflight) while still posting
// the kind-only guidance note. Covers all four pass-throughs across the
// wrapper, the SSH-direct envelope round-trip, the leading terminal-control
// prefix, the `/compact` raw echo, and the `Compacted` stdout.
#[test]
fn local_only_slash_prompt_detects_passthrough_command_xml() {
    for name in ["/effort", "/compact", "/cost", "/context"] {
        let wrapper = format!(
            "<command-message>{name} is running…</command-message>\n\
                 <command-name>{name}</command-name>\n<command-args></command-args>"
        );
        assert!(
            is_local_only_slash_command_prompt(&wrapper),
            "expanded wrapper for {name} must be local-only",
        );
        // SSH-direct envelope round-trip of the wrapper.
        let wrapped = format!("터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n{wrapper}\n```");
        assert!(
            is_local_only_slash_command_prompt(&wrapped),
            "envelope-wrapped {name} wrapper must still be local-only",
        );
        // Leading terminal-control prefix before the wrapper.
        let with_controls = format!("\u{1b}[2K\r{wrapper}");
        assert!(
            is_local_only_slash_command_prompt(&with_controls),
            "terminal-control-prefixed {name} wrapper must still be local-only",
        );
    }
    // `<command-name>` with an argument body — the first token (`/effort`) is
    // the canonical kind, so it still matches the allow-list.
    let effort_with_args =
        "<command-message>x</command-message>\n<command-name>/effort high</command-name>";
    assert!(is_local_only_slash_command_prompt(effort_with_args));
    // Raw `/compact` echo (no wrapper) and the `Compacted` stdout line.
    assert!(is_local_only_slash_command_prompt("/compact"));
    assert!(is_local_only_slash_command_prompt(
        "/compact focus on the relay"
    ));
    assert!(is_local_only_slash_command_prompt(
        "<local-command-stdout>Compacted (12.3k tokens)"
    ));
}

// #3305 (REQUIRED REGRESSION GUARD): `/loop` STARTS a real model turn, so it
// must remain a full active turn (#3178) — classified as SlashCommandControl
// yet NOT local-only. This double-assertion pins /loop on the anchor+⏳+
// synthetic-inflight path at the classification level so the local-only skip
// can never over-suppress it.
#[test]
fn local_only_slash_prompt_preserves_loop_wakeup_lifecycle() {
    let raw = "/loop 5m /foo";
    let wrapper = "<command-message>loop is running…</command-message>\n\
                       <command-name>/loop</command-name>\n<command-args>5m /foo</command-args>";
    for form in [raw, wrapper] {
        assert_eq!(
            classify_injected_prompt(form),
            InjectedPromptClass::SlashCommandControl,
            "/loop stays a SlashCommandControl active turn",
        );
        assert!(
            !is_local_only_slash_command_prompt(form),
            "/loop must NOT be local-only (it starts a model turn) — over-suppression guard",
        );
    }
}

#[test]
fn command_message_skill_wakeup_preserves_assistant_relay_lifecycle() {
    let wakeup = "<command-message>agentdesk-issue-pipeline</command-message>\n\
                  <command-args>issue 4041 --continue autonomous pipeline</command-args>";

    assert_eq!(
        classify_injected_prompt(wakeup),
        InjectedPromptClass::SlashCommandControl,
        "machine command-message wakeups should hide the raw echo without becoming human text",
    );
    assert_eq!(
        slash_command_control_kind(wakeup),
        "agentdesk-issue-pipeline",
        "command-message-only wakeups need a stable non-fallback kind for lifecycle dedupe",
    );

    let decision = relay_observed_prompt_injected_prompt_decision(wakeup);
    assert_eq!(
        decision.injected_class,
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(
        decision.slash_command_kind.as_deref(),
        Some("agentdesk-issue-pipeline"),
    );
    assert!(
        !decision.local_only_slash,
        "a skill wakeup starts a model turn and must not take the local-only echo path",
    );
    assert!(
        !decision.injected_class.suppresses_user_turn_lifecycle(),
        "wakeup turns still need anchor/reaction/synthetic ownership",
    );
    assert!(
        decision.injected_class.still_delivers_assistant_output(),
        "wakeup assistant prose must stay relayable even when the injected echo is sanitized",
    );
}

// #3305: non-command text, the system-continuation banner, task notifications,
// a token-boundary near-miss, and an UNLISTED command must all be rejected so
// the local-only skip never fires for a real turn (fail-safe = lifecycle kept).
#[test]
fn local_only_slash_prompt_rejects_non_command_text() {
    // A plain human prompt.
    assert!(!is_local_only_slash_command_prompt(
        "please summarize the relay design"
    ));
    // SystemContinuation banner wins (handled by its own neutral-note path).
    assert!(!is_local_only_slash_command_prompt(
        "This session is being continued from a previous conversation…"
    ));
    // Task-notification tag is not a slash control echo.
    assert!(!is_local_only_slash_command_prompt(
        "<task-notification>done</task-notification>"
    ));
    // Token-boundary near-miss: `/compactX` is not the whole `/compact` token.
    assert!(!is_local_only_slash_command_prompt(
        "/compactX do the thing"
    ));
    // An UNLISTED command's wrapper — `/loop` is a SlashCommandControl but is
    // NOT on the allow-list (it starts a model turn), so lifecycle is preserved
    // (fail-safe default).
    let loop_wrapper = "<command-message>x</command-message>\n<command-name>/loop</command-name>";
    assert!(matches!(
        classify_injected_prompt(loop_wrapper),
        InjectedPromptClass::SlashCommandControl
    ));
    assert!(!is_local_only_slash_command_prompt(loop_wrapper));
    // #3500: `/model` IS a SlashCommandControl AND local-only (Claude-native,
    // changes the model with no model turn) — lifecycle is SKIPPED so it does
    // not strand a synthetic inflight that queues the next real message.
    let model_wrapper = "<command-message>x</command-message>\n<command-name>/model</command-name>";
    assert!(matches!(
        classify_injected_prompt(model_wrapper),
        InjectedPromptClass::SlashCommandControl
    ));
    assert!(is_local_only_slash_command_prompt(model_wrapper));
}

// #4033: `/model` writes two adjacent transcript user entries. The
// `<command-name>` half was already local-only (#3500); the stdout half must be
// local-only too, or the relay mints a fake synthetic inflight and queues the
// next real user message behind it.
#[test]
fn local_only_slash_prompt_skips_model_two_half_transcript() {
    let command_name_half =
        "<command-message>x</command-message>\n<command-name>/model</command-name>";
    let stdout_half = "<local-command-stdout>Set model to Fable 5</local-command-stdout>";

    for (half, expected_kind) in [
        (command_name_half, "/model"),
        (stdout_half, "local-command-stdout"),
    ] {
        assert_eq!(
            classify_injected_prompt(half),
            InjectedPromptClass::SlashCommandControl,
            "both /model transcript halves must be machine slash-control echoes",
        );
        assert_eq!(
            slash_command_control_kind(half),
            expected_kind,
            "each /model transcript half keeps the production dedupe kind it really carries",
        );
        assert!(
            is_local_only_slash_command_prompt(half),
            "both /model transcript halves must skip the turn lifecycle",
        );
        assert!(
            !classify_injected_prompt(half).is_human_active_turn(),
            "neither /model transcript half is human TUI-direct input",
        );
    }
}

#[test]
fn relay_prompt_decision_skips_model_two_half_transcript() {
    let command_name_half =
        "<command-message>x</command-message>\n<command-name>/model</command-name>";
    let stdout_half = "<local-command-stdout>Set model to Fable 5</local-command-stdout>";

    let command_decision = relay_observed_prompt_injected_prompt_decision(command_name_half);
    assert_eq!(
        command_decision.injected_class,
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(
        command_decision.slash_command_kind.as_deref(),
        Some("/model")
    );
    assert!(command_decision.local_only_slash);

    let stdout_decision = relay_observed_prompt_injected_prompt_decision(stdout_half);
    assert_eq!(
        stdout_decision.injected_class,
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(
        stdout_decision.slash_command_kind.as_deref(),
        Some("local-command-stdout"),
    );
    assert!(stdout_decision.local_only_slash);

    assert_ne!(
        command_decision.slash_command_kind, stdout_decision.slash_command_kind,
        "the two /model transcript halves intentionally carry different dedupe keys; lifecycle skip must not rely on dedupe",
    );
}

// #3178: the machine slash-command control trigger now resolves to a stable
// command KIND that BOTH the raw `/loop` echo and the expanded `<command-*>`
// wrapper for the SAME command map to (so the #3153 double-post collapses to
// one dedupe entry), and the note shows ONLY that kind — never the raw body.
#[test]
fn slash_command_control_kind_is_stable_across_double_post_halves() {
    // HALF A (raw echo) and HALF B (expanded wrapper) for /loop share a kind.
    assert_eq!(slash_command_control_kind("/loop 5m /foo"), "/loop");
    let wrapper = "<command-message>loop is running…</command-message>\
                       <command-name>/loop</command-name><command-args>5m /foo</command-args>";
    assert_eq!(slash_command_control_kind(wrapper), "/loop");

    // /compact forms (raw echo, bare no-arg, Compacted stdout) share a kind.
    assert_eq!(
        slash_command_control_kind("/compact focus on the relay"),
        "/compact",
    );
    assert_eq!(slash_command_control_kind("/compact"), "/compact");
    assert_eq!(
        slash_command_control_kind("<local-command-stdout>Compacted (12.3k tokens)"),
        "/compact",
    );
    assert_eq!(
        slash_command_control_kind(
            "<local-command-stdout>Compacted 12 messages</local-command-stdout>"
        ),
        "/compact",
    );

    // A round-tripped SSH-direct envelope still resolves to the same kind.
    let wrapped_loop = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n/loop 5m /foo\n```";
    assert_eq!(slash_command_control_kind(wrapped_loop), "/loop");
}

// The note always names the command KIND + tmux session and marks the injection
// non-active. `/loop` carries its directive body, generic stdout carries a short
// output preview, and `/compact`/`Compacted …` stdout stays kind-only.
#[test]
fn slash_command_control_note_loop_and_generic_stdout_show_body() {
    let loop_note =
        format_slash_command_control_note("sess-a", "/loop", "/loop 290s relay check directive");
    assert!(
        loop_note.contains("/loop"),
        "note must name the command kind"
    );
    assert!(loop_note.contains("sess-a"));
    assert!(loop_note.contains("활성 턴 아님"), "must mark non-active");
    assert!(
        loop_note.contains("290s relay check directive"),
        "the /loop note MUST carry the directive body",
    );

    // The expanded wrapper half exposes only the <command-args> block, never
    // the trailing skill markdown the wrapper appends.
    let wrapped = format_slash_command_control_note(
        "sess-a",
        "/loop",
        "<command-name>/loop</command-name>\n<command-args>watch the relay</command-args>\n# /loop — schedule\nSKILL BODY LEAK",
    );
    assert!(
        wrapped.contains("watch the relay"),
        "the /loop note MUST carry the wrapped directive body",
    );
    assert!(
        !wrapped.contains("SKILL BODY LEAK"),
        "the /loop note must NOT leak the trailing skill markdown",
    );

    // An UNTERMINATED wrapper (no closing </command-args>) must NOT spill the
    // trailing skill markdown — the closing tag is required, so it falls back
    // to kind-only rather than rendering the whole tail.
    let unterminated = format_slash_command_control_note(
        "sess-a",
        "/loop",
        "<command-name>/loop</command-name>\n<command-args>watch the relay\n# /loop — schedule\nSKILL BODY LEAK",
    );
    assert!(
        !unterminated.contains("SKILL BODY LEAK"),
        "unterminated wrapper must NOT leak the trailing skill markdown",
    );
    assert!(
        !unterminated.contains("```"),
        "unterminated wrapper falls back to the kind-only header",
    );

    // A bodyless /loop gracefully degrades to the kind-only header.
    let bare = format_slash_command_control_note("sess-a", "/loop", "/loop");
    assert!(bare.contains("/loop") && bare.contains("활성 턴 아님"));
    assert!(!bare.contains("```"), "bodyless /loop has no preview block");

    let compact_note = format_slash_command_control_note(
        "sess-a",
        "/compact",
        "<local-command-stdout>Compacted 12 messages</local-command-stdout>",
    );
    assert!(compact_note.contains("/compact"));
    assert!(
        !compact_note.contains("Compacted"),
        "note must NOT leak the compact stdout body",
    );

    let stdout_note = format_slash_command_control_note(
        "sess-a",
        "local-command-stdout",
        "<local-command-stdout>Set model to Fable 5</local-command-stdout>",
    );
    assert!(stdout_note.contains("머신 슬래시 명령"));
    assert!(
        stdout_note.contains("Set model to Fable 5"),
        "generic stdout notes must include a short body preview",
    );
    assert!(stdout_note.contains("```text"));
}

// #4033 regression guard: broad `<local-command-stdout>...</local-command-stdout>`
// detection must not disturb the legacy /compact stdout path. Compacted stdout
// still resolves to `/compact`, remains local-only, and keeps the body hidden.
#[test]
fn local_command_stdout_compacted_path_stays_kind_only() {
    let compact_stdout = "<local-command-stdout>Compacted 12 messages</local-command-stdout>";
    assert_eq!(
        classify_injected_prompt(compact_stdout),
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(slash_command_control_kind(compact_stdout), "/compact");
    assert!(is_local_only_slash_command_prompt(compact_stdout));

    let compact_note = format_slash_command_control_note("sess-a", "/compact", compact_stdout);
    assert!(compact_note.contains("/compact"));
    assert!(
        !compact_note.contains("Compacted"),
        "legacy /compact stdout note must stay kind-only",
    );
}

#[test]
fn wrapped_local_command_stdout_is_local_only_slash_control() {
    let wrapped_stdout = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
        <local-command-stdout>Set model to Fable 5</local-command-stdout>\n```";

    assert_eq!(
        classify_injected_prompt(wrapped_stdout),
        InjectedPromptClass::SlashCommandControl,
        "a fence-wrapped stdout echo must classify as machine slash control",
    );
    assert_eq!(
        slash_command_control_kind(wrapped_stdout),
        "local-command-stdout",
    );
    assert!(is_local_only_slash_command_prompt(wrapped_stdout));

    let decision = relay_observed_prompt_injected_prompt_decision(wrapped_stdout);
    assert_eq!(
        decision.injected_class,
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(
        decision.slash_command_kind.as_deref(),
        Some("local-command-stdout"),
    );
    assert!(
        decision.local_only_slash,
        "local-only stdout returns before anchor/reaction/synthetic ownership",
    );
    assert!(!decision.injected_class.is_human_active_turn());
}

#[test]
fn wrapped_compacted_stdout_keeps_prefix_only_kind() {
    let wrapped_compacted = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
        <local-command-stdout>Compacted 12 messages\n```";

    assert_eq!(
        classify_injected_prompt(wrapped_compacted),
        InjectedPromptClass::SlashCommandControl,
        "open-fenced Compacted stdout must keep the legacy prefix-only match",
    );
    assert_eq!(slash_command_control_kind(wrapped_compacted), "/compact");
    assert!(is_local_only_slash_command_prompt(wrapped_compacted));

    let decision = relay_observed_prompt_injected_prompt_decision(wrapped_compacted);
    assert_eq!(decision.slash_command_kind.as_deref(), Some("/compact"));
    assert!(decision.local_only_slash);
}

#[test]
fn wrapped_local_command_stdout_with_appended_user_text_stays_human() {
    let wrapped_with_user_text = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\
        <local-command-stdout>Set model to Fable 5</local-command-stdout>\n```\n\
        이 출력 설명해줘";

    assert_eq!(
        classify_injected_prompt(wrapped_with_user_text),
        InjectedPromptClass::HumanTuiDirect,
        "human text appended after the stdout echo must not be over-suppressed",
    );
    assert!(!is_local_only_slash_command_prompt(wrapped_with_user_text));

    let decision = relay_observed_prompt_injected_prompt_decision(wrapped_with_user_text);
    assert_eq!(decision.injected_class, InjectedPromptClass::HumanTuiDirect,);
    assert_eq!(decision.slash_command_kind, None);
    assert!(!decision.local_only_slash);
}

#[test]
fn local_command_stdout_negative_cases_stay_human_direct() {
    let trailing_text = "<local-command-stdout>x</local-command-stdout>\n이 출력 설명해줘";
    let open_tag_only = "<local-command-stdout>Set model to Fable 5";
    let mid_body_tag = "please explain this transcript:\n\
                        <local-command-stdout>x</local-command-stdout>";

    for (prompt, label) in [
        (
            trailing_text,
            "trailing user text after the closing tag is a human prompt",
        ),
        (
            open_tag_only,
            "open-only non-Compacted stdout is incomplete scanner input",
        ),
        (mid_body_tag, "mid-body stdout tags are quoted human text"),
    ] {
        assert_eq!(
            classify_injected_prompt(prompt),
            InjectedPromptClass::HumanTuiDirect,
            "{label}",
        );
        assert!(
            !is_local_only_slash_command_prompt(prompt),
            "{label}: local-only slash detection must stay start-anchored",
        );
        let decision = relay_observed_prompt_injected_prompt_decision(prompt);
        assert_eq!(
            decision.injected_class,
            InjectedPromptClass::HumanTuiDirect,
            "{label}: relay decision must agree with the helper classifier",
        );
        assert_eq!(decision.slash_command_kind, None, "{label}");
        assert!(!decision.local_only_slash, "{label}");
    }

    let rendered = format_ssh_direct_prompt_notification("claude", "sess-mid-body", mid_body_tag);
    assert!(
        rendered.contains("<local-command-stdout>x</local-command-stdout>"),
        "a mid-body stdout tag should remain part of the human prompt preview",
    );
}

#[test]
fn local_command_stdout_body_command_name_does_not_hijack_kind() {
    let stdout_with_embedded_command_name = "<local-command-stdout>Set model to Fable 5\n\
                                            <command-name>/loop</command-name>\n\
                                            </local-command-stdout>";

    assert_eq!(
        classify_injected_prompt(stdout_with_embedded_command_name),
        InjectedPromptClass::SlashCommandControl,
    );
    assert_eq!(
        slash_command_control_kind(stdout_with_embedded_command_name),
        "local-command-stdout",
        "a command-name string embedded inside stdout body must not hijack the kind",
    );
    assert!(is_local_only_slash_command_prompt(
        stdout_with_embedded_command_name
    ));

    let decision =
        relay_observed_prompt_injected_prompt_decision(stdout_with_embedded_command_name);
    assert_eq!(
        decision.slash_command_kind.as_deref(),
        Some("local-command-stdout"),
    );
    assert!(decision.local_only_slash);

    let note = format_slash_command_control_note(
        "sess-a",
        "local-command-stdout",
        stdout_with_embedded_command_name,
    );
    assert!(note.contains("머신 슬래시 명령"));
    assert!(!note.contains("자동 점검(/loop)"));
    assert!(note.contains("Set model to Fable 5"));
}

// #3178 CORE (codex fix): the same trigger (a /loop double-post: raw echo +
// expanded wrapper, both mapping to kind "/loop" for the same tmux session)
// creates the active turn exactly ONCE — the first sighting proceeds, the
// immediate second (within the 2s window) is dropped BEFORE any lease/anchor.
// A DIFFERENT command kind (/loop vs /compact) in the same session is NOT
// collapsed (the kind is the real command name), and a different session is
// never deduped.
#[test]
fn slash_command_control_turn_dedupes_double_post_but_not_distinct_commands() {
    // Unique session names so this test cannot collide with the shared
    // process-global dedupe map across parallel test runs.
    let sess = format!("dedupe-sess-{:p}", &0u8 as *const u8);

    // HALF A — first sighting proceeds to claim the active turn.
    assert!(slash_command_control_turn_is_first_sighting(&sess, "/loop"));
    // HALF B — same (session, kind) within the 2s window → NOT a first
    // sighting (dropped before any lease/anchor; the first turn is preserved).
    assert!(!slash_command_control_turn_is_first_sighting(
        &sess, "/loop"
    ));
    // And again — still deduped.
    assert!(!slash_command_control_turn_is_first_sighting(
        &sess, "/loop"
    ));

    // A DIFFERENT command kind in the same session is a DISTINCT turn —
    // /compact must NOT collapse into the in-window /loop entry.
    assert!(slash_command_control_turn_is_first_sighting(
        &sess, "/compact"
    ));
    // But its own repeat within the window is deduped.
    assert!(!slash_command_control_turn_is_first_sighting(
        &sess, "/compact"
    ));

    // Two DIFFERENT unknown commands in the same session within the window
    // are distinct turns (no single "slash" collapse) — the codex P2 fix.
    assert!(slash_command_control_turn_is_first_sighting(&sess, "/foo"));
    assert!(slash_command_control_turn_is_first_sighting(&sess, "/bar"));

    // A DIFFERENT session with the same kind is a distinct turn.
    let other = format!("{sess}-other");
    assert!(slash_command_control_turn_is_first_sighting(
        &other, "/loop"
    ));
}

fn local_control_prompt(tmux: &str, body: &str, entry_id: &str) -> ObservedTuiPrompt {
    ObservedTuiPrompt {
        provider: "claude".to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: body.to_string(),
        source_event_id: Some(entry_id.to_string()),
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    }
}

#[test]
fn local_slash_control_note_emission_is_wired_through_prepare_gate() {
    let relay_source = include_str!("../tui_prompt_relay.rs");
    assert!(
        relay_source.contains(
            "let Some(note) = prepare_local_only_slash_control_note(&prompt, kind) else {"
        ),
        "relay_observed_prompt must consume the gated note outcome before channel delivery"
    );
}

#[test]
fn local_slash_control_note_path_dedupes_and_seals_dropped_half() {
    let sess = format!("local-control-note-path-{:p}", &0u8 as *const u8);
    let raw = local_control_prompt(&sess, "/compact", "compact-half-a");
    let envelope = local_control_prompt(&sess, compact_command_name_first_stub(), "compact-half-b");

    assert!(
        prepare_local_only_slash_control_note(&raw, "/compact").is_some(),
        "the first transcript half must render one Discord marker"
    );
    assert!(
        prepare_local_only_slash_control_note(&envelope, "/compact").is_none(),
        "the near-simultaneous envelope half must not render a second marker"
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
            "claude",
            &sess,
            compact_command_name_first_stub(),
            Some("compact-half-b"),
            chrono::Utc::now(),
        ),
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedReplayedEntry,
        "the dropped half must be sealed against watermark-reset replay"
    );

    let loop_prompt = local_control_prompt(&sess, "/loop", "loop-entry");
    assert!(
        prepare_local_only_slash_control_note(&loop_prompt, "/loop").is_some(),
        "different command kinds must remain independent"
    );
}

#[test]
fn local_slash_control_note_allows_same_kind_after_window() {
    let sess = format!("local-control-note-window-{:p}", &0u8 as *const u8);
    let first = local_control_prompt(&sess, "/compact", "compact-window-a");
    assert!(prepare_local_only_slash_control_note(&first, "/compact").is_some());

    let key = format!("{sess}\u{0}/compact");
    SLASH_COMMAND_CONTROL_LAST_POSTED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(
            key,
            std::time::Instant::now() - SLASH_COMMAND_CONTROL_DEDUPE_WINDOW,
        );

    let later = local_control_prompt(&sess, "/compact", "compact-window-b");
    assert!(
        prepare_local_only_slash_control_note(&later, "/compact").is_some(),
        "the local note gate must allow a genuine command after the dedupe window"
    );
}

#[test]
fn local_compact_still_bypasses_the_external_replay_classifier() {
    let sess = format!("local-compact-external-gate-{:p}", &0u8 as *const u8);
    let compact = relay_observed_prompt_injected_prompt_decision("/compact");
    assert!(compact.local_only_slash);
    assert!(
        !slash_command_control_turn_is_duplicate_external_replay(&compact, &sess),
        "local controls must remain outside the external replay classifier"
    );

    let loop_control = relay_observed_prompt_injected_prompt_decision("/loop 5m inspect status");
    assert!(!loop_control.local_only_slash);
    assert!(
        !slash_command_control_turn_is_duplicate_external_replay(&loop_control, &sess),
        "the first external slash control proceeds"
    );
    assert!(
        slash_command_control_turn_is_duplicate_external_replay(&loop_control, &sess),
        "the existing external /loop raw-wrapper replay guard remains intact"
    );
}

// #3178 (codex P2 fix): the kind is the REAL command name, so two distinct
// unknown `<command-message>` wrappers do NOT collapse into a single "slash"
// kind (which would wrongly dedupe genuinely different commands).
#[test]
fn slash_command_control_kind_distinguishes_distinct_unknown_commands() {
    let foo = "<command-message>foo running</command-message>\
                   <command-name>/foo</command-name>";
    let bar = "<command-message>bar running</command-message>\
                   <command-name>/bar</command-name>";
    assert_eq!(slash_command_control_kind(foo), "/foo");
    assert_eq!(slash_command_control_kind(bar), "/bar");
    assert_ne!(
        slash_command_control_kind(foo),
        slash_command_control_kind(bar),
        "distinct unknown commands must NOT collapse to one kind"
    );

    let skill = "<command-message>agentdesk-issue-pipeline</command-message>\n\
                 <command-args>issue 4041</command-args>";
    assert_eq!(
        slash_command_control_kind(skill),
        "agentdesk-issue-pipeline"
    );
    assert_ne!(
        slash_command_control_kind(skill),
        slash_command_control_kind("<local-command-caveat>x</local-command-caveat>"),
        "a local-only caveat fallback must not consume a command-message wakeup dedupe key",
    );
}

// #3153 regression guard: the compact CONTINUATION banner must STILL classify
// as SystemContinuation (precedence — the continuation check runs before the
// slash-command-control check, and the banner opening is textually disjoint
// from the /compact echo / Compacted stdout anchors).
#[test]
fn classify_injected_prompt_continuation_still_wins_over_slash_control() {
    assert_eq!(
        classify_injected_prompt(
            "This session is being continued from a previous conversation that ran out of context... Summary:"
        ),
        InjectedPromptClass::SystemContinuation,
    );
}

// #3100: the system-continuation predicate is the most specific signal and
// must win even if the continuation summary embeds a `<task-notification>`.
#[test]
fn classify_injected_prompt_continuation_wins_over_embedded_task_tag() {
    let mixed = "This session is being continued from a previous conversation.\nSummary: \
                     the agent ran <task-notification><status>completed</status></task-notification>";
    assert_eq!(
        classify_injected_prompt(mixed),
        InjectedPromptClass::SystemContinuation,
    );
}

// #4082: a SystemContinuation is a provider-injected session note, not a turn.
// It suppresses the user-turn lifecycle and the external-output bridge tail so
// it cannot strand the mailbox behind a phantom synthetic inflight after
// `/compact`.
#[test]
fn system_continuation_suppresses_external_turn_lifecycle() {
    let cont = InjectedPromptClass::SystemContinuation;
    assert!(
        cont.suppresses_user_turn_lifecycle(),
        "SystemContinuation must drop the ⏳/user-turn lifecycle"
    );
    assert!(
        !cont.still_delivers_assistant_output(),
        "SystemContinuation must not spawn a bridge tail or claim a synthetic external turn"
    );
    assert!(!cont.is_human_active_turn());

    // A subagent_notification is a terminal machine-event card, not a model
    // prologue: it suppresses the user-turn lifecycle and does not keep the
    // provider-output bridge tail.
    let subagent = InjectedPromptClass::SubagentNotificationEvent;
    assert!(subagent.suppresses_user_turn_lifecycle());
    assert!(!subagent.still_delivers_assistant_output());
    assert!(!subagent.is_human_active_turn());

    let human = InjectedPromptClass::HumanTuiDirect;
    assert!(!human.suppresses_user_turn_lifecycle());
    assert!(human.still_delivers_assistant_output());

    let task = InjectedPromptClass::TaskNotificationEvent;
    assert!(
        task.suppresses_user_turn_lifecycle(),
        "task lifecycle records must never claim a user-turn lifecycle"
    );
    assert!(
        !task.still_delivers_assistant_output(),
        "task lifecycle records must not spawn an output bridge tail"
    );
    assert!(!task.is_human_active_turn());
}

// #3100 codex re-review (P2): a human message that merely *quotes* the
// continuation banner inside a normal request must NOT be mis-classified as
// SystemContinuation — otherwise the human silently loses their `⏳`/turn.
// Detection is anchored to start-of-prompt, so an embedded quote never trips.
#[test]
fn classify_injected_prompt_human_quote_of_banner_is_not_continuation() {
    let human = "Can you check why \"This session is being continued from a previous \
                     conversation\" keeps showing up in my logs? Please continue the \
                     conversation from where we left it off was also printed.";
    assert_eq!(
        classify_injected_prompt(human),
        InjectedPromptClass::HumanTuiDirect,
        "a human quoting the banner mid-message must stay a human turn",
    );
    assert!(classify_injected_prompt(human).is_human_active_turn());
}

// #3100 codex re-review (P2): a real machine-injected continuation banner is
// the WHOLE prompt body and starts with the canonical opening — even with a
// leading terminal-control prefix or leading whitespace the injector may
// prepend, it must still classify as SystemContinuation (no false negative).
#[test]
fn classify_injected_prompt_real_injection_with_leading_controls_is_continuation() {
    let injected = "\u{1b}[2K\u{1b}[0m  \n\tThis session is being continued from a previous \
                        conversation that ran out of context.\nAnalysis:\n... summary body ...";
    assert_eq!(
        classify_injected_prompt(injected),
        InjectedPromptClass::SystemContinuation,
        "a real banner with leading controls/whitespace must classify as continuation",
    );
}

// #3100 codex P2: a real machine-injected continuation banner can arrive
// WRAPPED with the SSH-direct injection envelope (the
// `터미널에 직접 주입된 입력 (tmux : <session>):` line + a ```text fence) when a
// previously-rendered notification round-trips back into the terminal and is
// re-observed. After stripping the wrapper the banner body still starts with
// the canonical opening, so it MUST classify as SystemContinuation — otherwise
// it falls into the active-turn handler and wrongly gains a ⏳/anchor/synthetic
// turn (the exact #3100 path this PR claims to fix).
#[test]
fn classify_injected_prompt_wrapped_continuation_is_continuation() {
    // Wrapper + ```text fence, exactly as `format_ssh_direct_prompt_notification`
    // renders it.
    let wrapped = "터미널에 직접 주입된 입력 (tmux : `AgentDesk-claude-adk-cc`):\n```text\n\
                       This session is being continued from a previous conversation that ran out \
                       of context.\nAnalysis: ... summary body ...\n```";
    assert_eq!(
        classify_injected_prompt(wrapped),
        InjectedPromptClass::SystemContinuation,
        "a wrapped continuation banner must classify as SystemContinuation",
    );
    assert!(!classify_injected_prompt(wrapped).is_human_active_turn());

    // Wrapper without a ```text fence (banner body directly on the next line).
    let wrapped_no_fence = "터미널에 직접 주입된 입력 (tmux : `s`):\n\
                                Please continue the conversation from where we left it off";
    assert_eq!(
        classify_injected_prompt(wrapped_no_fence),
        InjectedPromptClass::SystemContinuation,
    );

    // Wrapper + leading control codes the injector may prepend before the body.
    let wrapped_with_controls = "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\n\u{1b}[2K  \
                                     This session is being continued from a previous conversation.";
    assert_eq!(
        classify_injected_prompt(wrapped_with_controls),
        InjectedPromptClass::SystemContinuation,
    );
}

// #3730: provider-session reuse prompts are machine/system injections. They
// compact repeated authoritative Discord/role/tool instructions and must render
// as neutral continuation notes, not raw direct-input blocks.
#[test]
fn classify_injected_prompt_provider_session_reuse_is_continuation() {
    let resumed = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n[User: 0hbujang] ok";
    assert_eq!(
        classify_injected_prompt(resumed),
        InjectedPromptClass::SystemContinuation,
    );

    let fresh_fork = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already issued to this \
role in the current dcserver lifetime still apply. Treat only this turn's user request, \
reply context, uploaded files, and memory recall below as new actionable input.\n\n[User: x]";
    assert_eq!(
        classify_injected_prompt(fresh_fork),
        InjectedPromptClass::SystemContinuation,
    );

    let wrapped = format!(
        "터미널에 직접 주입된 입력 (tmux : `AgentDesk-codex-adk-cdx`):\n```text\n{resumed}\n```"
    );
    assert_eq!(
        classify_injected_prompt(&wrapped),
        InjectedPromptClass::SystemContinuation,
        "wrapped provider reuse marker must classify after peeling the direct-injection wrapper",
    );

    let truncated_prologue = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply.\n\nThis is a human question about the marker.";
    assert_eq!(
        classify_injected_prompt(truncated_prologue),
        InjectedPromptClass::HumanTuiDirect,
        "provider reuse detection must require the full generated prologue, not just the first sentence",
    );
}

#[test]
fn classify_injected_prompt_provider_session_reuse_subagent_stays_subagent_event() {
    let resumed = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    assert_eq!(
        classify_injected_prompt(resumed),
        InjectedPromptClass::SubagentNotificationEvent,
        "provider-session reuse wrapping must not downgrade subagent notifications to generic continuation events",
    );
    let output = format_subagent_notification_card("AgentDesk-codex-adk-cdx", resumed);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Review complete."));
    assert!(!output.contains("[Provider Session Reuse]"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private"));
}

#[test]
fn classify_provider_reuse_user_prefixed_subagent_stays_subagent_event_3777() {
    let resumed = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    assert_eq!(
        classify_injected_prompt(resumed),
        InjectedPromptClass::SubagentNotificationEvent,
        "provider-session reuse user prefix must not hide subagent machine events",
    );
    let output = format_subagent_notification_card("AgentDesk-codex-adk-cdx", resumed);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Review complete."));
    assert!(!output.contains("[User:"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
}

#[test]
fn classify_provider_reuse_chrome_then_user_prefixed_subagent_3818() {
    let resumed = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    assert_eq!(
        classify_injected_prompt(resumed),
        InjectedPromptClass::SubagentNotificationEvent,
        "provider reuse + TUI chrome + user prefix must still render as a subagent card, not a raw direct prompt",
    );
    let output = format_subagent_notification_card("AgentDesk-codex-adk-cdx", resumed);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Review complete."));
    assert!(!output.contains("No response requested."));
    assert!(!output.contains("[User:"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
}

// #3100 codex P2: stripping the wrapper is anchored to the START. A human
// message whose body merely contains/quotes the wrapper marker (not as the
// leading line) must NOT be unwrapped and must stay a human turn.
#[test]
fn classify_injected_prompt_wrapper_quoted_mid_body_is_not_continuation() {
    let human = "Why does \"터미널에 직접 주입된 입력 (tmux : `s`):\" appear, then \
                     This session is being continued from a previous conversation in my logs?";
    assert_eq!(
        classify_injected_prompt(human),
        InjectedPromptClass::HumanTuiDirect,
        "a human quoting the wrapper mid-body must stay a human turn",
    );
    assert!(classify_injected_prompt(human).is_human_active_turn());

    // A leading wrapper line whose body is NOT a continuation banner stays a
    // human turn (the wrapper alone must not force a continuation verdict).
    let wrapped_human =
        "터미널에 직접 주입된 입력 (tmux : `s`):\n```text\nplease review PR #1234\n```";
    assert_eq!(
        classify_injected_prompt(wrapped_human),
        InjectedPromptClass::HumanTuiDirect,
    );

    let quoted_reuse = "Why did the prompt include [Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this Codex thread still apply?";
    assert_eq!(
        classify_injected_prompt(quoted_reuse),
        InjectedPromptClass::HumanTuiDirect,
        "provider reuse detection must stay start-anchored to avoid swallowing human questions",
    );
}

// #3100: the neutral session note must not present the system continuation
// as "터미널에 직접 주입된 입력" (an active-turn marker).
#[test]
fn system_continuation_note_is_neutral_not_active_turn() {
    let prompt = "This session is being continued from a previous conversation. Summary: ...";
    let note = format_system_continuation_note("AgentDesk-claude-adk-cc", prompt);
    assert!(!note.contains("터미널에 직접 주입된 입력"));
    assert_eq!(
        note,
        "🧩 Session continued (compact/resume) · tmux: `AgentDesk-claude-adk-cc`"
    );
    assert!(!note.contains("```text"));
    assert!(!note.contains("Summary:"));
}

#[cfg(unix)]
fn test_watcher_handle(
    tmux_session_name: &str,
    output_path: &Path,
) -> super::super::TmuxWatcherHandle {
    super::super::TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: output_path.display().to_string(),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::tmux_watcher_now_ms(),
        )),
    }
}

#[cfg(unix)]
#[test]
fn external_input_owner_uses_resolved_claude_transcript_before_session_bound_owner() {
    let dir = tempfile::tempdir().expect("temp dir");
    let stale_binding_path = dir.path().join("stale-binding.jsonl");
    let resolved_fresh_path = dir.path().join("resolved-fresh.jsonl");
    let tmux_session_name = "AgentDesk-claude-stale-binding-owner";
    let watchers = super::super::TmuxWatcherRegistry::new();
    watchers.insert(
        ChannelId::new(940_000_000_000_001),
        test_watcher_handle(tmux_session_name, &stale_binding_path),
    );

    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&stale_binding_path),
            true,
        ),
        ExternalInputRelayOwner::BridgeAdapter,
        "TUI-direct external turns have no synthetic inflight, so bridge tail owns response delivery"
    );
    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&resolved_fresh_path),
            true,
        ),
        ExternalInputRelayOwner::BridgeAdapter,
        "a heartbeat-fresh watcher may not own output for a different resolved transcript"
    );
}

#[cfg(unix)]
#[test]
fn external_input_owner_selects_one_relay_path_per_observed_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output_path = dir.path().join("output.jsonl");
    let other_path = dir.path().join("other.jsonl");
    let tmux_session_name = "AgentDesk-codex-owner-split";
    let watchers = super::super::TmuxWatcherRegistry::new();

    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&output_path),
            true,
        ),
        ExternalInputRelayOwner::BridgeAdapter
    );

    watchers.insert(
        ChannelId::new(940_000_000_000_002),
        test_watcher_handle(tmux_session_name, &output_path),
    );
    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&output_path),
            true,
        ),
        ExternalInputRelayOwner::BridgeAdapter
    );
    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&output_path),
            false,
        ),
        ExternalInputRelayOwner::TmuxWatcher
    );
    assert_eq!(
        external_input_relay_owner_for_watchers(
            &watchers,
            tmux_session_name,
            Some(&other_path),
            true,
        ),
        ExternalInputRelayOwner::BridgeAdapter
    );
}

#[cfg(unix)]
#[test]
fn bridge_tail_resolution_bypasses_watcher_suppression_for_session_bound_external_turn() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript_path = dir.path().join("claude-transcript.jsonl");
    std::fs::write(&transcript_path, "").expect("write transcript");
    let tmux_session_name = "AgentDesk-claude-session-bound-direct-input";
    let channel_id = ChannelId::new(940_000_000_000_006);
    let shared = super::super::make_shared_data_for_tests();
    shared.tmux_watchers.insert(
        channel_id,
        test_watcher_handle(tmux_session_name, &transcript_path),
    );
    let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("claude-transcript".to_string()),
        last_offset: 0,
        relay_last_offset: None,
    };

    assert_eq!(
        resolve_idle_relay_transcript(&shared, tmux_session_name, channel_id, &binding, false,),
        Some(transcript_path.clone()),
        "BridgeAdapter-owned direct input must tail even when the watcher covers the transcript"
    );
    assert_eq!(
        resolve_idle_relay_transcript(&shared, tmux_session_name, channel_id, &binding, true,),
        None,
        "legacy watcher-owned mode may still suppress the bridge tail to avoid duplicates"
    );
}

#[cfg(unix)]
#[test]
fn bridge_adapter_tails_only_bridge_owned_external_turns() {
    assert!(bridge_adapter_owns_external_turn(
        ExternalInputRelayOwner::BridgeAdapter
    ));
    assert!(!bridge_adapter_owns_external_turn(
        ExternalInputRelayOwner::SessionBoundRelay
    ));
    assert!(!bridge_adapter_owns_external_turn(
        ExternalInputRelayOwner::TmuxWatcher
    ));
    assert!(!bridge_adapter_owns_external_turn(
        ExternalInputRelayOwner::TuiPromptRelay
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn claude_inflight_drain_wait_allows_transient_previous_turn() {
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_ref = probes.clone();

    assert!(
        wait_for_transient_state_to_clear(
            Duration::from_millis(50),
            Duration::from_millis(1),
            move || probe_ref.fetch_add(1, Ordering::SeqCst) < 2,
        )
        .await,
        "a short-lived previous inflight should not make the direct-input bridge tail give up"
    );
    assert!(
        probes.load(Ordering::SeqCst) >= 3,
        "the helper should re-check until the transient state clears"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn claude_inflight_drain_wait_times_out_when_previous_turn_stays_active() {
    let probes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let probe_ref = probes.clone();

    assert!(
        !wait_for_transient_state_to_clear(
            Duration::from_millis(5),
            Duration::from_millis(1),
            move || {
                probe_ref.fetch_add(1, Ordering::SeqCst);
                true
            },
        )
        .await,
        "a persistent previous inflight should keep the guarded skip behavior"
    );
    assert!(
        probes.load(Ordering::SeqCst) >= 2,
        "timeout branch should poll instead of making a single stale decision"
    );
}

#[cfg(unix)]
#[test]
fn bridge_adapter_tail_guard_clears_only_current_external_lease() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-codex-bridge-guard";
    let channel_id = ChannelId::new(940_000_000_000_003);
    let original = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:codex:940000000000003:bridge-guard:1".to_string()),
        session_key: Some("host:AgentDesk-codex-bridge-guard".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::CodexTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // Capture the RECORDED lease (with its stamped generation) — the guard must
    // hold the exact stored identity to clear it on drop.
    let recorded_original = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Codex.as_str(),
        tmux,
        original.clone(),
    );

    {
        let _guard = TuiDirectExternalInputLeaseGuard::new(
            ProviderKind::Codex,
            tmux,
            channel_id,
            &recorded_original,
        );
    }
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            channel_id.get(),
        )
        .is_none()
    );

    let newer = ExternalInputRelayLease {
        turn_id: Some("external:codex:940000000000003:bridge-guard:2".to_string()),
        ..original.clone()
    };
    let recorded_original = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Codex.as_str(),
        tmux,
        original.clone(),
    );
    let recorded_newer;
    {
        let _guard = TuiDirectExternalInputLeaseGuard::new(
            ProviderKind::Codex,
            tmux,
            channel_id,
            &recorded_original,
        );
        recorded_newer = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            newer.clone(),
        );
    }
    assert_eq!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Codex.as_str(),
            tmux,
            channel_id.get(),
        ),
        Some(recorded_newer.clone()),
        "the old guard's drop must NOT clobber the newer lease (clear-by-identity)"
    );
    assert!(
        crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
            ProviderKind::Codex.as_str(),
            tmux,
            channel_id.get(),
            &recorded_newer,
        )
    );
}

#[cfg(unix)]
#[test]
fn claude_bridge_lease_clears_when_tail_dedup_skips_spawn() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-claude-bridge-dedup-skip";
    let channel_id = ChannelId::new(940_000_000_000_004);
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:940000000000004:dedup-skip:2".to_string()),
        session_key: Some("host:AgentDesk-claude-bridge-dedup-skip".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    {
        let mut active = CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        active.remove(tmux);
        active.insert(tmux.to_string());
    }
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        lease,
    );

    let spawned = spawn_claude_idle_response_tail_once(
        super::super::make_shared_data_for_tests(),
        tmux.to_string(),
        channel_id,
        PathBuf::from("/tmp/unused-claude-bridge-dedup-skip.jsonl"),
        0,
        "direct input while another tail is active".to_string(),
        lease.clone(),
    );
    assert!(
        !spawned,
        "active tail dedup should reject the second Claude tail"
    );
    assert!(clear_external_input_bridge_lease_if_current(
        &ProviderKind::Claude,
        tmux,
        channel_id,
        &lease,
    ));
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        )
        .is_none(),
        "a dedup-skipped Claude BridgeAdapter lease must not block session-bound delivery until TTL"
    );
    CLAUDE_IDLE_RESPONSE_TAILS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(tmux);
}

// SAFETY (await_holding_lock): `tui_prompt_dedupe::TEST_LOCK` is a std Mutex
// held across awaits to serialize tests that share the prompt-dedupe global
// state; the hold is required for serialization. Test-only.
#[allow(clippy::await_holding_lock)]
#[cfg(unix)]
#[tokio::test]
async fn claude_bridge_lease_guard_cleans_no_binding_precondition_skip() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-claude-bridge-no-binding";
    let channel_id = ChannelId::new(940_000_000_000_005);
    let prompt = ObservedTuiPrompt {
        provider: ProviderKind::Claude.as_str().to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: "direct input without runtime binding".to_string(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:940000000000005:no-binding:1".to_string()),
        session_key: Some("host:AgentDesk-claude-bridge-no-binding".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        lease,
    );

    let spawned;
    {
        let mut guard =
            TuiDirectExternalInputLeaseGuard::new(ProviderKind::Claude, tmux, channel_id, &lease);
        spawned = maybe_spawn_claude_idle_response_tail(
            super::super::make_shared_data_for_tests(),
            channel_id,
            &prompt,
            &lease,
            None,
            None,
        )
        .await;
        if spawned {
            guard.disarm();
        }
    }

    assert!(
        !spawned,
        "missing runtime binding is a pre-tail precondition skip"
    );
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        )
        .is_none(),
        "precondition skips before a tail guard exists must clear the recorded BridgeAdapter lease"
    );
}

// #3075 codex P1 #2: a `<task-notification>` edit-repeat records a fresh
// external-input turn lease (record_observed_external_turn_lease) but then
// early-returns before the normal bridge-tail / lease-guard cleanup. The
// repeat path must clear exactly the lease it recorded so a dangling
// non-Unassigned lease cannot make session-bound delivery skip a legitimate
// bridge-tail delivery.
#[test]
fn task_notification_repeat_clears_its_recorded_external_lease() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-task-card-repeat-lease";
    let channel_id = ChannelId::new(950_000_000_000_001);
    let prompt = ObservedTuiPrompt {
            provider: ProviderKind::Claude.as_str().to_string(),
            tmux_session_name: tmux.to_string(),
        prompt: "<task-notification><task-id>repeat-x</task-id><status>completed</status></task-notification>".to_string(),
        source_event_id: None,
            observed_at: chrono::Utc::now(),
            external_input_lease_generation:
                crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation:
                crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        };
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:950000000000001:repeat:1".to_string()),
        session_key: Some("host:AgentDesk-task-card-repeat-lease".to_string()),
        // A BridgeAdapter (non-Unassigned) lease is exactly what would block
        // session-bound delivery if left dangling.
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        lease,
    );
    // Sanity: the lease is present and would block delivery.
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        )
    );

    // The repeat early-return clears exactly its recorded lease.
    assert!(clear_observed_external_turn_lease_if_current(
        &prompt, channel_id, &lease,
    ));
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        )
        .is_none(),
        "a task-notification edit-repeat must not leave a stale lease that blocks bridge-tail delivery"
    );
}

// #3075 codex P1 #2: the exact-match guard must NOT clobber a newer turn's
// lease that reused the same provider/session/channel after the repeat
// recorded its lease.
#[test]
fn task_notification_repeat_lease_clear_preserves_newer_turn() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-task-card-repeat-newer";
    let channel_id = ChannelId::new(950_000_000_000_002);
    let prompt = ObservedTuiPrompt {
        provider: ProviderKind::Claude.as_str().to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: "<task-notification><task-id>repeat-y</task-id></task-notification>".to_string(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };
    let repeat_lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:950000000000002:repeat:1".to_string()),
        session_key: Some("host:AgentDesk-task-card-repeat-newer".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let newer_lease = ExternalInputRelayLease {
        turn_id: Some("external:claude:950000000000002:repeat:2".to_string()),
        ..repeat_lease.clone()
    };
    let recorded_repeat = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        repeat_lease.clone(),
    );
    // A newer turn overwrites the lease before the repeat's cleanup runs.
    let recorded_newer = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        newer_lease.clone(),
    );
    assert_ne!(
        recorded_repeat.generation, recorded_newer.generation,
        "each recorded lease must get a distinct generation",
    );

    // The repeat's exact-match clear is a no-op against the newer lease.
    assert!(!clear_observed_external_turn_lease_if_current(
        &prompt,
        channel_id,
        &recorded_repeat,
    ));
    assert_eq!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        ),
        Some(recorded_newer),
        "exact-match clear must preserve a newer turn's lease",
    );
}

// #3089 A6b r2 [High]/#3998 S1-f2: the codex external-input bridge frame
// builder emits `OutputOffset` so the bridge has a real ordered range for the
// unconditional A5 controller route.

// ====================================================================
// #3256: stream-through of operator external-input prose. These tests pin
// the SINGLE-bridge-turn invariant (one terminal Done = one finalize) and
// prove that a LONG/multi-block response relays PROGRESSIVELY (more than one
// Text frame forwarded before the terminal Done) while a SHORT response
// still yields one finalized card — all WITHIN one bridge turn.
//
// These tests FAIL on the old code path: `bridge_adapter_stream_messages`
// always collapsed the whole response into a single `[Text{full}, Done]`,
// so the bridge only ever saw ONE Text frame regardless of how many prose
// blocks the turn produced (the bug). The stream-through forwards each
// reader frame, so multiple Text frames reach the bridge before Done.
// ====================================================================

#[cfg(unix)]
fn drain_forwarded_idle_stream(
    prefix: Vec<StreamMessage>,
    rest: Vec<StreamMessage>,
) -> (Vec<StreamMessage>, usize) {
    let (reader_tx, reader_rx) = mpsc::channel();
    for message in rest {
        reader_tx.send(message).unwrap();
    }
    drop(reader_tx);
    let (bridge_tx, bridge_rx) = mpsc::channel();
    let text_frames = forward_idle_stream_into_bridge(prefix, reader_rx, bridge_tx);
    let forwarded: Vec<StreamMessage> = bridge_rx.into_iter().collect();
    (forwarded, text_frames)
}

#[cfg(unix)]
#[test]
fn idle_stream_long_response_relays_progressively_within_one_bridge_turn() {
    // A long autonomous turn produces multiple prose blocks interleaved with
    // tool use, then a terminal result. The stream-through must forward each
    // prose block as its own Text frame (progressive relay) and finalize on
    // EXACTLY ONE Done.
    let prefix = vec![StreamMessage::Text {
        content: "first prose block\n".to_string(),
    }];
    let rest = vec![
        StreamMessage::ToolUse {
            name: "Bash".to_string(),
            input: "ls".to_string(),
            tool_use_id: Some("t1".to_string()),
        },
        StreamMessage::OutputOffset { offset: 128 },
        StreamMessage::Text {
            content: "second prose block\n".to_string(),
        },
        StreamMessage::Text {
            content: "third prose block\n".to_string(),
        },
        // Real result line, then the reader's synthetic empty completion.
        StreamMessage::Done {
            result: "first prose block\nsecond prose block\nthird prose block".to_string(),
            session_id: Some("sess-9".to_string()),
        },
        StreamMessage::Done {
            result: String::new(),
            session_id: Some("sess-9".to_string()),
        },
    ];
    let (forwarded, text_frames) = drain_forwarded_idle_stream(prefix, rest);

    assert!(
        text_frames > 1,
        "long response must relay MORE THAN ONE Text frame before turn-done (got {text_frames})"
    );
    let done_count = forwarded
        .iter()
        .filter(|m| matches!(m, StreamMessage::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "exactly one terminal Done must reach the bridge (single finalize)"
    );
    // The terminal Done must be the LAST frame and carry the authoritative
    // result; the trailing synthetic empty Done was dropped.
    assert!(matches!(
        forwarded.last(),
        Some(StreamMessage::Done { result, .. }) if result.contains("third prose block")
    ));
}

#[cfg(unix)]
#[test]
fn idle_stream_short_response_produces_one_finalized_card() {
    // A short turn: one prose block then the result. Equivalent to the old
    // collect-then-send path — one card, one finalize.
    let prefix = vec![StreamMessage::Text {
        content: "quick answer".to_string(),
    }];
    let rest = vec![
        StreamMessage::Done {
            result: "quick answer".to_string(),
            session_id: Some("sess-1".to_string()),
        },
        StreamMessage::Done {
            result: String::new(),
            session_id: Some("sess-1".to_string()),
        },
    ];
    let (forwarded, text_frames) = drain_forwarded_idle_stream(prefix, rest);

    assert_eq!(
        text_frames, 1,
        "short response forwards exactly one Text frame"
    );
    assert_eq!(
        forwarded
            .iter()
            .filter(|m| matches!(m, StreamMessage::Done { .. }))
            .count(),
        1,
        "short response finalizes exactly once"
    );
}

#[cfg(unix)]
#[test]
fn idle_stream_finalizes_exactly_once_even_without_reader_done() {
    // Defensive: if the reader stream closes WITHOUT a terminal Done (e.g.
    // session died mid-stream), a synthetic Done is appended so the bridge
    // still finalizes — exactly once, never zero, never twice.
    let prefix = vec![StreamMessage::Text {
        content: "partial work".to_string(),
    }];
    let (forwarded, _) = drain_forwarded_idle_stream(prefix, Vec::new());

    let done_count = forwarded
        .iter()
        .filter(|m| matches!(m, StreamMessage::Done { .. }))
        .count();
    assert_eq!(
        done_count, 1,
        "missing reader Done must yield exactly one synthetic Done"
    );
    assert!(matches!(forwarded.last(), Some(StreamMessage::Done { .. })));
}

#[cfg(unix)]
#[test]
fn idle_stream_strips_leading_chrome_from_first_text_only() {
    // The old compose path stripped leading TUI chrome; the stream-through
    // must strip it from the FIRST Text frame so the live card never flashes
    // it, while leaving later prose untouched.
    let prefix = vec![StreamMessage::Text {
        content: "No response requested.\nreal prose".to_string(),
    }];
    let rest = vec![
        StreamMessage::Text {
            content: "\nNo response requested. (literal later)".to_string(),
        },
        StreamMessage::Done {
            result: String::new(),
            session_id: None,
        },
    ];
    let (forwarded, _) = drain_forwarded_idle_stream(prefix, rest);

    assert!(matches!(
        &forwarded[0],
        StreamMessage::Text { content } if content.trim() == "real prose"
    ));
    // Later Text frames are NOT chrome-stripped.
    assert!(matches!(
        &forwarded[1],
        StreamMessage::Text { content } if content.contains("(literal later)")
    ));
}

#[cfg(unix)]
#[test]
fn idle_stream_content_classifier_ignores_pure_control_and_empty_done() {
    // Empty / control-only frames are NOT content: a turn yielding only
    // these takes the no-card empty path (preserving today's behavior).
    assert!(!idle_stream_message_is_content(
        &StreamMessage::OutputOffset { offset: 10 }
    ));
    assert!(!idle_stream_message_is_content(&StreamMessage::Done {
        result: String::new(),
        session_id: None,
    }));
    assert!(!idle_stream_message_is_content(&StreamMessage::Text {
        content: "   \n".to_string(),
    }));
    // Real prose, an authoritative terminal body, and a transport error all
    // count as content.
    assert!(idle_stream_message_is_content(&StreamMessage::Text {
        content: "prose".to_string(),
    }));
    assert!(idle_stream_message_is_content(&StreamMessage::Done {
        result: "final body".to_string(),
        session_id: None,
    }));
    assert!(idle_stream_message_is_content(&StreamMessage::Error {
        message: "boom".to_string(),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
    }));
    // #3256 parity: a Text/Done body that is ONLY leading TUI chrome must NOT
    // count as content — otherwise a "No response requested." turn would now
    // spawn a placeholder card the old path never produced.
    assert!(!idle_stream_message_is_content(&StreamMessage::Text {
        content: "No response requested.".to_string(),
    }));
    assert!(!idle_stream_message_is_content(&StreamMessage::Text {
        content: "Continue from where you left off.".to_string(),
    }));
    assert!(!idle_stream_message_is_content(&StreamMessage::Done {
        result: "No response requested.".to_string(),
        session_id: None,
    }));
    // Chrome FOLLOWED by real prose is still content (only leading chrome is
    // stripped).
    assert!(idle_stream_message_is_content(&StreamMessage::Text {
        content: "No response requested.\nactual prose".to_string(),
    }));
}

#[cfg(unix)]
#[test]
fn idle_stream_commit_offset_only_on_successful_delivery() {
    // The stream-through commits the runtime-binding offset only when the
    // single bridge turn delivered successfully — matching the dedupe
    // contract vs. committed_relay_offset (the start-offset clamp in
    // `spawn_claude_idle_response_tail_once` handles the read side).
    assert!(tui_idle_tail_stream_should_commit_runtime_binding_offset(
        true
    ));
    assert!(!tui_idle_tail_stream_should_commit_runtime_binding_offset(
        false
    ));
}

#[cfg(unix)]
#[test]
fn bridge_adapter_inflight_marks_external_input_as_bridge_owned() {
    let output_path = PathBuf::from("/tmp/adk-bridge-adapter.jsonl");
    let lease = ExternalInputRelayLease {
        channel_id: Some(42),
        turn_id: Some("external:codex:42:tmux:1".to_string()),
        session_key: Some("token:AgentDesk-codex-owner-split".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::CodexTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let state = build_tui_direct_bridge_inflight_state(
        ProviderKind::Codex,
        ChannelId::new(42),
        MessageId::new(101),
        MessageId::new(202),
        "typed in TUI",
        "AgentDesk-codex-owner-split",
        &output_path,
        333,
        &lease,
    );

    assert_eq!(state.turn_source, TurnSource::ExternalInput);
    assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::None);
    assert_eq!(state.user_msg_id, 101);
    assert_eq!(state.current_msg_id, 202);
    assert_eq!(state.user_text, "typed in TUI");
    assert_eq!(state.session_key.as_deref(), lease.session_key.as_deref());
    assert_eq!(state.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
    assert_eq!(state.turn_start_offset, Some(333));
}

#[cfg(unix)]
#[test]
fn drain_wait_does_not_block_on_own_synthetic_inflight() {
    // #3176: the idle-tail drain-wait must treat THIS turn's own TUI-direct
    // synthetic inflight as non-blocking. If it waited on it, it would
    // self-deadlock (we created it; it never drains) and permanently skip the
    // relay. The discrimination is `tui_direct_synthetic_inflight_matches`:
    // ExternalInput + same tmux session => our own => non-blocking.
    let output_path = PathBuf::from("/tmp/adk-selfblock.jsonl");
    let lease = ExternalInputRelayLease {
        channel_id: Some(7),
        turn_id: Some("external:claude:7:tmux:1".to_string()),
        session_key: Some("token:AgentDesk-claude-self".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let state = build_tui_direct_bridge_inflight_state(
        ProviderKind::Claude,
        ChannelId::new(7),
        MessageId::new(11),
        MessageId::new(22),
        "typed in TUI",
        "AgentDesk-claude-self",
        &output_path,
        0,
        &lease,
    );

    // state.user_msg_id == 11 (the anchor id for this turn).
    // Our own synthetic for THIS turn (matching anchor id) => non-blocking.
    assert!(
        inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", Some(11)),
        "own synthetic (same session + matching anchor id) must be non-blocking"
    );
    // A PREVIOUS same-session TUI turn (different anchor id) => still blocks,
    // even though it is also ExternalInput on the same session. This is the
    // precision codex required: do not skip a genuinely distinct previous turn.
    assert!(
        !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", Some(999)),
        "a different turn's inflight (anchor id mismatch) must stay blocking"
    );
    // This turn created no synthetic (system-continuation / slash) => anchor None
    // => any present inflight is a previous turn and still blocks.
    assert!(
        !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-self", None),
        "no current synthetic (anchor None) must keep any inflight blocking"
    );
    // A different tmux session is never ours.
    assert!(
        !inflight_is_current_turn_synthetic(Some(&state), "AgentDesk-claude-other", Some(11)),
        "an inflight for a different tmux session must stay blocking"
    );
    // No inflight at all => nothing to wait on (not ours either).
    assert!(!inflight_is_current_turn_synthetic(
        None,
        "AgentDesk-claude-self",
        Some(11)
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_watcher_inflight_marks_existing_tui_turn_without_prompt_resubmit() {
    let output_path = PathBuf::from("/tmp/adk-tui-direct-watcher.jsonl");
    let lease = ExternalInputRelayLease {
        channel_id: Some(42),
        turn_id: Some("external:codex:42:tmux:2".to_string()),
        session_key: Some("token:AgentDesk-codex-owner-split".to_string()),
        relay_owner: ExternalInputRelayOwner::TmuxWatcher,
        runtime_kind: Some(RuntimeHandoffKind::CodexTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let state = build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        ChannelId::new(42),
        MessageId::new(101),
        None,
        "typed in TUI",
        "AgentDesk-codex-owner-split",
        Some(&output_path),
        333,
        &lease,
        RelayOwnerKind::Watcher,
    );

    assert_eq!(state.turn_source, TurnSource::ExternalInput);
    assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::Watcher);
    assert_eq!(
        state.request_owner_user_id,
        TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
    );
    assert_eq!(state.user_msg_id, 101);
    assert_eq!(state.current_msg_id, 0);
    assert_eq!(state.user_text, "typed in TUI");
    assert_eq!(state.output_path.as_deref(), output_path.to_str());
    assert_eq!(state.input_fifo_path, None);
}

#[cfg(unix)]
#[test]
fn codex_external_input_relay_output_path_uses_rollout_not_wrapper() {
    let shared = super::super::make_shared_data_for_tests();
    let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: "/tmp/live-codex-rollout.jsonl".to_string(),
        relay_output_path: Some("/tmp/stale-wrapper-output.jsonl".to_string()),
        input_fifo_path: None,
        session_id: Some("codex-session".to_string()),
        last_offset: 123,
        relay_last_offset: Some(0),
    };

    assert_eq!(
        external_input_relay_output_path(
            &shared,
            ProviderKind::Codex.as_str(),
            "AgentDesk-codex-rollout-path",
            ChannelId::new(42),
            Some(&binding),
        ),
        Some(PathBuf::from("/tmp/live-codex-rollout.jsonl")),
        "Codex TUI response relay must tail the rollout file, not the wrapper jsonl"
    );
}

#[cfg(unix)]
#[test]
fn codex_external_input_binding_refreshes_from_live_rollout_marker() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    let tmux_session_name = "AgentDesk-codex-marker-refresh";
    let rollout_path = temp.path().join("rollout.jsonl");
    std::fs::write(
        &rollout_path,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-marker-session\"}}\n",
    )
    .expect("write rollout");
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        tmux_session_name,
        &rollout_path,
        Some("codex-marker-session"),
    )
    .expect("write marker");
    let stale_binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::CodexTui,
        output_path: temp
            .path()
            .join("missing-wrapper.jsonl")
            .display()
            .to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: None,
        last_offset: 0,
        relay_last_offset: None,
    };

    let refreshed = external_input_relay_binding(
        ProviderKind::Codex.as_str(),
        tmux_session_name,
        ChannelId::new(43),
        Some(stale_binding),
    )
    .expect("binding refresh");

    assert_eq!(refreshed.output_path, rollout_path.display().to_string());
    assert_eq!(
        refreshed.session_id.as_deref(),
        Some("codex-marker-session")
    );
}

#[cfg(unix)]
#[test]
fn codex_ownerless_external_input_undelivered_turn_needs_rollout_repair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _env = EnvRootGuard::set(dir.path());
    let rollout_path = dir.path().join("rollout.jsonl");
    std::fs::write(
        &rollout_path,
        "{\"type\":\"session_meta\",\"payload\":{}}\n",
    )
    .expect("write rollout");
    let missing_output_path = dir.path().join("missing-wrapper.jsonl");
    let lease = ExternalInputRelayLease::unassigned(Some(44));
    let tmux_session_name = "AgentDesk-codex-repair-predicate";
    let mut state = build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        ChannelId::new(44),
        MessageId::new(444),
        None,
        "typed in TUI",
        tmux_session_name,
        Some(&missing_output_path),
        0,
        &lease,
        RelayOwnerKind::None,
    );
    state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);

    assert!(
        codex_ownerless_external_input_inflight_needs_rollout_recovery(&state, tmux_session_name,)
    );

    state.output_path = Some(rollout_path.display().to_string());
    assert!(
        codex_ownerless_external_input_inflight_needs_rollout_recovery(&state, tmux_session_name,)
    );

    state.output_path = Some(missing_output_path.display().to_string());
    state.current_msg_id = 123;
    assert!(
        !codex_ownerless_external_input_inflight_needs_rollout_recovery(&state, tmux_session_name,)
    );
}

#[cfg(unix)]
#[test]
fn synthetic_watcher_claim_requires_live_watcher_covering_output() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output_path = dir.path().join("output.jsonl");
    let other_path = dir.path().join("other.jsonl");
    let tmux_session_name = "AgentDesk-codex-synthetic-owner";
    let watchers = super::super::TmuxWatcherRegistry::new();

    assert!(!tui_direct_watcher_can_own_output(
        &watchers,
        tmux_session_name,
        Some(&output_path),
    ));

    watchers.insert(
        ChannelId::new(940_000_000_000_007),
        test_watcher_handle(tmux_session_name, &output_path),
    );
    assert!(tui_direct_watcher_can_own_output(
        &watchers,
        tmux_session_name,
        Some(&output_path),
    ));
    assert!(!tui_direct_watcher_can_own_output(
        &watchers,
        tmux_session_name,
        Some(&other_path),
    ));
}

/// #3876 (codex rework): the birth-site relay-owner decision for a TUI-direct /
/// warm-followup synthetic inflight is gated on a LIVE per-session producer, NOT
/// the global session-bound flag. `SessionBoundRelay` (sink commits) only when
/// the watcher cannot own AND session-bound delivery is enabled AND a live
/// producer exists; otherwise `BridgeAdapter` so the watcher-independent
/// transcript-direct bridge tail stays the deliverer (regression guard against
/// the producer-starve answer-loss).
#[test]
fn synthetic_relay_owner_gates_session_bound_on_live_producer() {
    use super::synthetic_start::tui_direct_synthetic_relay_owner;

    // (c) Live watcher owns the output → watcher relays the body (unchanged),
    // regardless of session-bound / producer signals.
    assert_eq!(
        tui_direct_synthetic_relay_owner(true, true, true),
        ExternalInputRelayOwner::TmuxWatcher,
    );
    assert_eq!(
        tui_direct_synthetic_relay_owner(true, true, false),
        ExternalInputRelayOwner::TmuxWatcher,
    );
    assert_eq!(
        tui_direct_synthetic_relay_owner(true, false, false),
        ExternalInputRelayOwner::TmuxWatcher,
    );
    // (b) THE FIX (demoed watcher-alive-path-mismatch case): watcher cannot own +
    // session-bound enabled + a LIVE producer exists (the sink can actually
    // commit) → SessionBoundRelay.
    assert_eq!(
        tui_direct_synthetic_relay_owner(false, true, true),
        ExternalInputRelayOwner::SessionBoundRelay,
    );
    // (a) REGRESSION GUARD (watcher-detached / STALL-WATCHDOG force-clean): watcher
    // cannot own + NO live producer → BridgeAdapter, so the watcher-independent
    // transcript-direct bridge tail still delivers (a SessionBoundRelay stamp here
    // would starve the sink AND stand the tail down → answer loss).
    assert_eq!(
        tui_direct_synthetic_relay_owner(false, true, false),
        ExternalInputRelayOwner::BridgeAdapter,
    );
    // Session-bound delivery disabled → legacy bridge-tail path regardless of producer.
    assert_eq!(
        tui_direct_synthetic_relay_owner(false, false, true),
        ExternalInputRelayOwner::BridgeAdapter,
    );
    assert_eq!(
        tui_direct_synthetic_relay_owner(false, false, false),
        ExternalInputRelayOwner::BridgeAdapter,
    );
}

/// #4455: the idle rollout observer records a provisional BridgeAdapter lease
/// before the synthetic-start observer resolves the real owner. Waiting only
/// for a Watcher row misses the healthy SessionBoundRelay outcome and creates a
/// second Discord surface for the same provider output.
#[test]
fn idle_bridge_stands_down_for_every_resolved_non_bridge_synthetic_claim() {
    use super::synthetic_start::tui_direct_synthetic_non_bridge_owner_matches;

    let root = tempfile::tempdir().expect("runtime root");
    let _env = EnvRootGuard::set(root.path());
    let tmux = "AgentDesk-codex-adk-cdx-4455";
    let channel = ChannelId::new(1_479_671_301_387_059_200);
    let lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
    let mut state = build_tui_direct_synthetic_inflight_state(
        ProviderKind::Codex,
        channel,
        MessageId::new(1_525_345_488_264_499_270),
        None,
        "external prompt",
        tmux,
        Some(std::path::Path::new("/tmp/codex-4455.jsonl")),
        0,
        &lease,
        RelayOwnerKind::Watcher,
    );

    assert!(tui_direct_synthetic_non_bridge_owner_matches(
        Some(&state),
        tmux,
        false,
    ));

    state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    assert!(tui_direct_synthetic_non_bridge_owner_matches(
        Some(&state),
        tmux,
        true,
    ));
    assert!(
        !tui_direct_synthetic_non_bridge_owner_matches(Some(&state), tmux, false),
        "a stale SessionBoundRelay stamp without its producer must leave the BridgeAdapter backstop eligible"
    );

    state.set_relay_owner_kind(RelayOwnerKind::None);
    assert!(
        !tui_direct_synthetic_non_bridge_owner_matches(Some(&state), tmux, false),
        "the provisional BridgeAdapter path stays eligible when no non-bridge claim landed"
    );

    state.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
    assert!(!tui_direct_synthetic_non_bridge_owner_matches(
        Some(&state),
        "AgentDesk-codex-other-session",
        true,
    ));
    state.turn_source = TurnSource::Managed;
    assert!(
        !tui_direct_synthetic_non_bridge_owner_matches(Some(&state), tmux, true),
        "a normal Discord turn is not the passive synthetic claim the idle observer waits for"
    );
}

/// #3876 regression pin (terminal delivery, both directions):
/// * producer-present: the `SessionBoundRelay` synthetic owner makes the unchanged
///   sink ownership gate ACCEPT terminal delivery (body committed, not
///   placeholder-only) while exactly one relayer remains (bridge tail stands down).
/// * producer-absent: the owner falls back to `BridgeAdapter`, the sink gate would
///   REJECT the ownerless `None` shape (the data loss), and the watcher-independent
///   bridge tail is the SOLE relayer (`observer_should_spawn_bridge_tail` true) — so
///   the answer is still delivered (no regression).
#[cfg(unix)]
#[test]
fn synthetic_owner_delivery_path_matches_producer_presence() {
    use super::synthetic_start::tui_direct_synthetic_relay_owner;
    use crate::services::discord::session_relay_sink::session_bound_discord_relay_can_own_terminal_delivery;

    let tmux = "AgentDesk-claude-3876-no-owner";
    let dir = tempfile::tempdir().expect("temp dir");
    let output_path = dir.path().join("transcript.jsonl");
    let channel_id = ChannelId::new(940_000_000_003_876);
    let lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));

    let make_row = |owner_kind| {
        build_tui_direct_synthetic_inflight_state(
            ProviderKind::Claude,
            channel_id,
            MessageId::new(940_000_000_203_876),
            Some(MessageId::new(940_000_000_103_876)),
            "## 등록 결과\nfull terminal answer body",
            tmux,
            Some(&output_path),
            0,
            &lease,
            owner_kind,
        )
    };

    // Producer present (the demoed fix case): owner = SessionBoundRelay, the sink
    // gate ACCEPTS the row (commits the body), and the bridge tail stands down →
    // the sink is the SOLE committer.
    let producer_present_owner = tui_direct_synthetic_relay_owner(false, true, true);
    assert_eq!(
        producer_present_owner,
        ExternalInputRelayOwner::SessionBoundRelay
    );
    assert!(
        session_bound_discord_relay_can_own_terminal_delivery(
            Some(&make_row(RelayOwnerKind::SessionBoundRelay)),
            tmux,
        ),
        "#3876: a SessionBoundRelay synthetic row MUST let the sink commit the terminal body"
    );
    assert!(!bridge_adapter_owns_external_turn(producer_present_owner));
    assert!(!observer_should_spawn_bridge_tail(
        false,
        producer_present_owner
    ));

    // The pre-fix ownerless (None) row is REJECTED by the same sink gate — the
    // placeholder-only data-loss shape this fix eliminates.
    assert!(
        !session_bound_discord_relay_can_own_terminal_delivery(
            Some(&make_row(RelayOwnerKind::None)),
            tmux,
        ),
        "#3876 regression: an ownerless (None) synthetic row is rejected by the sink gate"
    );

    // Producer ABSENT (watcher-detached / force-clean): owner = BridgeAdapter, the
    // sink cannot deliver, so the watcher-independent bridge tail MUST be the SOLE
    // relayer — `observer_should_spawn_bridge_tail` true → the answer still ships.
    let producer_absent_owner = tui_direct_synthetic_relay_owner(false, true, false);
    assert_eq!(
        producer_absent_owner,
        ExternalInputRelayOwner::BridgeAdapter
    );
    assert!(bridge_adapter_owns_external_turn(producer_absent_owner));
    assert!(observer_should_spawn_bridge_tail(
        false,
        producer_absent_owner
    ));
}

// ====================================================================
// #4082 defect 1 — compact continuation records are neutral session notes.
// They must render through the existing classifier but must NOT select an
// external turn owner, start a synthetic inflight, or occupy the mailbox.
// Genuine typed TUI input keeps the existing synthetic-inflight path.
// ====================================================================

fn external_turn_test_lease(
    channel_id: ChannelId,
    tmux_session_name: &str,
) -> ExternalInputRelayLease {
    ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some(format!(
            "external:claude:{}:{tmux_session_name}:test",
            channel_id.get()
        )),
        session_key: Some(format!("session:{tmux_session_name}")),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    }
}

#[test]
fn synthetic_lifecycle_anchor_uses_posted_placeholder() {
    let notification_anchor = MessageId::new(940_000_000_004_180);
    let placeholder_anchor = MessageId::new(940_000_000_004_181);

    let anchor = synthetic_start_wiring::synthetic_lifecycle_anchor_from_placeholder_result(
        notification_anchor,
        &Ok(placeholder_anchor),
    );
    assert_eq!(
        anchor.message_id, placeholder_anchor,
        "successful synthetic placeholder delivery must replace the notification anchor"
    );
    assert!(
        anchor.owned_placeholder,
        "a posted placeholder must remain cleanup-owned until the synthetic claim succeeds"
    );
}

#[test]
fn synthetic_lifecycle_anchor_falls_back_after_placeholder_failure() {
    let notification_anchor = MessageId::new(940_000_000_004_280);

    let anchor = synthetic_start_wiring::synthetic_lifecycle_anchor_from_placeholder_result(
        notification_anchor,
        &Err("delivery failed".to_string()),
    );
    assert_eq!(
        anchor.message_id, notification_anchor,
        "failed synthetic placeholder delivery must preserve the notification anchor"
    );
    assert!(
        !anchor.owned_placeholder,
        "the fallback notification anchor must never be selected for placeholder cleanup"
    );
}

#[test]
fn failed_synthetic_claim_cleans_only_owned_placeholder() {
    let anchor = MessageId::new(940_000_000_004_380);

    assert_eq!(
        synthetic_start_wiring::failed_synthetic_placeholder_cleanup_target(anchor, true),
        Some(anchor),
        "a freshly posted synthetic placeholder must be selected for cleanup after claim failure"
    );
    assert_eq!(
        synthetic_start_wiring::failed_synthetic_placeholder_cleanup_target(anchor, false),
        None,
        "a fallback notification/task-card anchor must never be deleted after claim failure"
    );
}

#[test]
fn failed_synthetic_placeholder_delete_is_terminal_cleanup_guarded() {
    let helper_src = include_str!("synthetic_start_wiring.rs");
    let guard_needle = ["terminal_cleanup_", "protects_delete("].concat();
    let delete_needle = [".delete_", "message(&http, anchor_message_id)"].concat();
    let result_needle = ["emit_relay_delete_", "result("].concat();
    let guard = helper_src
        .find(&guard_needle)
        .expect("rejected synthetic cleanup must consult terminal cleanup protection");
    let delete = helper_src
        .find(&delete_needle)
        .expect("rejected synthetic cleanup must retain its owned-placeholder delete");
    let result = helper_src
        .find(&result_needle)
        .expect("rejected synthetic cleanup delete result must be durably observed");

    assert!(
        guard < delete && delete < result,
        "committed and retry-pending terminal placeholders must be skipped before delete"
    );
}

#[tokio::test]
async fn compact_continuation_injection_skips_synthetic_and_leaves_mailbox_free() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(940_000_000_004_082);
    let tmux = "AgentDesk-claude-4082-compact-continuation";
    let anchor_id = MessageId::new(940_000_000_004_182);
    let prompt_text = "This session is being continued from a previous conversation that ran out of context.\n\
         Summary: compacted transcript body";
    let prompt = ObservedTuiPrompt {
        provider: provider.as_str().to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: prompt_text.to_string(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };
    let decision = relay_observed_prompt_injected_prompt_decision(&prompt.prompt);
    assert_eq!(
        decision.injected_class,
        InjectedPromptClass::SystemContinuation
    );
    assert!(
        !decision.starts_external_turn_lifecycle(),
        "compact continuation records must be passive notes, not external turns"
    );

    let mut lease = external_turn_test_lease(channel_id, tmux);
    let deferred = synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
        &shared,
        provider.as_str(),
        channel_id,
        &prompt,
        anchor_id,
        false,
        &decision,
        &mut lease,
    )
    .await;
    assert!(
        !deferred,
        "neutral continuation must not defer or start synthetic ownership"
    );
    assert!(
        super::super::inflight::load_inflight_state(&provider, channel_id.get()).is_none(),
        "neutral continuation must not write a synthetic inflight row"
    );
    let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert_eq!(snapshot.active_user_message_id, None);
    assert!(snapshot.cancel_token.is_none());

    let next_message_id = MessageId::new(940_000_000_004_282);
    assert!(
        super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            Arc::new(CancelToken::new()),
            serenity::UserId::new(42),
            next_message_id,
        )
        .await,
        "subsequent Discord message must start immediately because the mailbox stayed free"
    );
}

// SAFETY (await_holding_lock): `tui_prompt_dedupe::TEST_LOCK` serializes this
// test's runtime-binding registration against tests that reset the shared
// dedupe maps. Test-only.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn genuine_tui_direct_typed_prompt_still_creates_synthetic_inflight() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(940_000_000_004_083);
    let tmux = "AgentDesk-claude-4082-genuine-typed";
    let notification_anchor_id = MessageId::new(940_000_000_004_183);
    let placeholder_anchor_id = MessageId::new(940_000_000_004_283);
    let prompt = ObservedTuiPrompt {
        provider: provider.as_str().to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: "please review PR #1234".to_string(),
        source_event_id: None,
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
    };
    let decision = relay_observed_prompt_injected_prompt_decision(&prompt.prompt);
    assert_eq!(decision.injected_class, InjectedPromptClass::HumanTuiDirect);
    assert!(
        decision.starts_external_turn_lifecycle(),
        "real typed TUI input must keep the synthetic-start lifecycle"
    );
    let transcript_path = temp.path().join("claude-typed.jsonl");
    std::fs::write(&transcript_path, "").expect("seed transcript path");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );

    let synthetic_anchor =
        synthetic_start_wiring::synthetic_lifecycle_anchor_from_placeholder_result(
            notification_anchor_id,
            &Ok(placeholder_anchor_id),
        );
    let anchor_id = synthetic_anchor.message_id;
    let mut lease = external_turn_test_lease(channel_id, tmux);
    let deferred = synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
        &shared,
        provider.as_str(),
        channel_id,
        &prompt,
        anchor_id,
        synthetic_anchor.owned_placeholder,
        &decision,
        &mut lease,
    )
    .await;
    assert!(
        !deferred,
        "no prior turn exists, so the claim should be inline"
    );

    assert_eq!(
        anchor_id, placeholder_anchor_id,
        "the synthetic claim must receive the posted placeholder identity"
    );
    assert_ne!(
        anchor_id, notification_anchor_id,
        "the notification/task-card identity must not remain the streaming anchor after placeholder success"
    );
    let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert_eq!(snapshot.active_user_message_id, Some(placeholder_anchor_id));
    assert!(snapshot.cancel_token.is_some());
    let state = super::super::inflight::load_inflight_state(&provider, channel_id.get())
        .expect("typed TUI prompt must create synthetic inflight");
    assert_eq!(state.turn_source, TurnSource::ExternalInput);
    assert_eq!(state.tmux_session_name.as_deref(), Some(tmux));
    assert_eq!(state.user_msg_id, placeholder_anchor_id.get());
    assert!(
        !state.relay_ownership_only,
        "human typed input must remain a full synthetic external turn"
    );
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
}

#[tokio::test]
async fn tui_direct_pre_save_cleanup_does_not_decrement_global_active() {
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(940_000_000_000_008);
    let user_message_id = MessageId::new(940_000_000_000_108);
    let started = super::super::mailbox_try_start_turn(
        shared.as_ref(),
        channel_id,
        Arc::new(CancelToken::new()),
        serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
        user_message_id,
    )
    .await;
    assert!(started, "test precondition: synthetic mailbox turn starts");

    shared.restart.global_active.store(3, Ordering::Relaxed);
    finish_tui_direct_synthetic_pre_save_failure(&shared, &provider, channel_id).await;

    assert_eq!(
        shared.restart.global_active.load(Ordering::Relaxed),
        3,
        "pre-save cleanup must not decrement a counter it has not incremented"
    );
    let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert!(snapshot.cancel_token.is_none());
    assert_eq!(snapshot.active_user_message_id, None);
}

fn save_ownerless_tui_direct_inflight_for_mailbox_release_test(
    provider: ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
    tmux_session_name: &str,
    output_path: &Path,
    stale_started_at: bool,
) {
    let lease = ExternalInputRelayLease::unassigned(Some(channel_id.get()));
    let mut state = build_tui_direct_synthetic_inflight_state(
        provider,
        channel_id,
        user_message_id,
        None,
        "typed in TUI",
        tmux_session_name,
        Some(output_path),
        0,
        &lease,
        RelayOwnerKind::None,
    );
    state.set_restart_mode(super::super::InflightRestartMode::DrainRestart);
    if stale_started_at {
        let stale_started_at = chrono::Local::now()
            - chrono::Duration::seconds(
                (super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS + 1) as i64,
            );
        state.started_at = stale_started_at.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    state.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    super::super::inflight::save_inflight_state(&state).expect("save inflight state");
}

#[tokio::test]
async fn stale_ownerless_tui_direct_mailbox_release_allows_new_synthetic_claim() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(940_000_000_000_009);
    let stale_message_id = MessageId::new(940_000_000_000_109);
    let anchor_message_id = MessageId::new(940_000_000_000_209);
    let tmux_session_name = "AgentDesk-codex-stale-mailbox-release";
    let output_path = temp.path().join("codex-rollout.jsonl");
    let stale_token = Arc::new(CancelToken::new());

    assert!(
        super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            stale_token.clone(),
            serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
            stale_message_id,
        )
        .await,
        "test precondition: stale mailbox turn starts"
    );
    shared.restart.global_active.store(1, Ordering::Relaxed);
    save_ownerless_tui_direct_inflight_for_mailbox_release_test(
        provider.clone(),
        channel_id,
        stale_message_id,
        tmux_session_name,
        &output_path,
        true,
    );

    assert!(
        release_stale_ownerless_tui_direct_mailbox_if_current(
            &shared,
            &provider,
            channel_id,
            tmux_session_name,
            stale_message_id,
            anchor_message_id,
        )
        .await,
        "stale ownerless ExternalInput mailbox should be released"
    );
    assert!(stale_token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert!(snapshot.cancel_token.is_none());
    assert_eq!(snapshot.active_user_message_id, None);

    assert!(
        super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            Arc::new(CancelToken::new()),
            serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
            anchor_message_id,
        )
        .await,
        "new synthetic claim must be able to occupy the released mailbox"
    );
}

#[tokio::test]
async fn stale_ownerless_tui_direct_mailbox_release_preserves_fresh_owner() {
    let temp = tempfile::tempdir().expect("temp runtime root");
    let _env = EnvRootGuard::set(temp.path());
    let shared = super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(940_000_000_000_010);
    let active_message_id = MessageId::new(940_000_000_000_110);
    let anchor_message_id = MessageId::new(940_000_000_000_210);
    let tmux_session_name = "AgentDesk-codex-fresh-mailbox-preserve";
    let output_path = temp.path().join("codex-rollout.jsonl");
    let active_token = Arc::new(CancelToken::new());

    assert!(
        super::super::mailbox_try_start_turn(
            shared.as_ref(),
            channel_id,
            active_token.clone(),
            serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
            active_message_id,
        )
        .await,
        "test precondition: active mailbox turn starts"
    );
    shared.restart.global_active.store(1, Ordering::Relaxed);
    save_ownerless_tui_direct_inflight_for_mailbox_release_test(
        provider.clone(),
        channel_id,
        active_message_id,
        tmux_session_name,
        &output_path,
        false,
    );

    assert!(
        !release_stale_ownerless_tui_direct_mailbox_if_current(
            &shared,
            &provider,
            channel_id,
            tmux_session_name,
            active_message_id,
            anchor_message_id,
        )
        .await,
        "fresh ownerless ExternalInput mailbox must not be released"
    );
    assert!(!active_token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    let snapshot = super::super::mailbox_snapshot(shared.as_ref(), channel_id).await;
    assert_eq!(snapshot.active_user_message_id, Some(active_message_id));
    assert!(snapshot.cancel_token.is_some());
}

#[cfg(unix)]
#[test]
fn tui_direct_gateway_has_no_live_bot_owner_for_local_queue_dispatch() {
    let gateway = TuiDirectBridgeGateway {
        http: Arc::new(serenity::Http::new("test-token")),
        shared: super::super::make_shared_data_for_tests(),
        provider: ProviderKind::Codex,
    };

    assert_eq!(gateway.bot_owner_provider(), None);
    assert!(
        gateway.can_chain_locally(),
        "bridge adapter still owns Discord delivery for the already-submitted turn"
    );
}

#[cfg(unix)]
#[test]
fn claude_rehydrate_thread_session_resolves_thread_channel_id() {
    let parent_channel_id = 1479671298497183835;
    let thread_id = 1504455726595051591_u64;
    let tmux_session_name =
        ProviderKind::Claude.build_tmux_session_name(&format!("adk-cc-t{thread_id}"));

    assert_eq!(
        rehydrated_claude_channel_id_for_segment(&tmux_session_name, "adk-cc", parent_channel_id),
        Some(thread_id)
    );
}

#[cfg(unix)]
#[test]
fn codex_rehydrate_thread_session_resolves_thread_channel_id() {
    let parent_channel_id = 1479671301387059200;
    let thread_id = 1504455726595051591_u64;
    let tmux_session_name =
        ProviderKind::Codex.build_tmux_session_name(&format!("adk-cdx-t{thread_id}"));

    assert_eq!(
        rehydrated_channel_id_for_segment(
            &ProviderKind::Codex,
            &tmux_session_name,
            "adk-cdx",
            parent_channel_id
        ),
        Some(thread_id)
    );
}

#[cfg(unix)]
#[test]
fn claude_rehydrate_thread_session_rejects_non_numeric_suffix() {
    let tmux_session_name = ProviderKind::Claude.build_tmux_session_name("adk-cc-tthread");

    assert_eq!(
        rehydrated_claude_channel_id_for_segment(&tmux_session_name, "adk-cc", 1479671298497183835),
        None
    );
}

#[cfg(unix)]
#[test]
fn claude_rehydrate_binding_match_requires_current_launch_transcript() {
    let existing = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: "/tmp/old-transcript.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("old-session".to_string()),
        last_offset: 10,
        relay_last_offset: None,
    };
    let fresh = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: "/tmp/current-transcript.jsonl".to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("current-session".to_string()),
        last_offset: 20,
        relay_last_offset: None,
    };

    assert!(!claude_tui_runtime_binding_matches_launch(
        &existing, &fresh
    ));
    assert!(claude_tui_runtime_binding_matches_launch(&fresh, &fresh));
}

// U-11 Missing transcripts still start at zero; existing transcripts
// always start at their current EOF.
#[cfg(unix)]
#[test]
fn claude_rehydrate_start_offset_returns_zero_for_missing_transcript() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("never-written.jsonl");

    assert_eq!(claude_tui_rehydrate_start_offset(&missing), 0);
}

#[cfg(unix)]
#[test]
fn claude_rehydrate_start_offset_uses_current_eof() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("current.jsonl");
    let before = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"direct claude prompt\"}]},\"sessionId\":\"s1\"}\n";
    let after = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
    let body = format!("{before}{prompt}{after}");
    std::fs::write(&transcript, &body).expect("write transcript");

    assert_eq!(
        claude_tui_rehydrate_start_offset(&transcript),
        body.len() as u64
    );
}

// #4549: `/compact` rewrites the same transcript path to a shorter historical
// snapshot. The durable EOF-regression signal must fast-forward to the new EOF
// instead of restarting at zero, which would mirror an old direct prompt again.
#[test]
fn claude_idle_transcript_scan_fast_forwards_when_compaction_shrinks_file() {
    assert_eq!(
        claude_idle_compaction_reanchor(false, 99_999, 250, true),
        Some(ClaudeIdleTranscriptScan::CompactionReanchor { offset: 250 })
    );
}

#[test]
fn claude_idle_transcript_scan_relays_prompt_appended_after_compaction_anchor() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let compact = "{\"type\":\"system\",\"subtype\":\"compact\",\"sessionId\":\"s1\"}\n";
    let historical_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"historical direct prompt\"}]},\"sessionId\":\"s1\"}\n";
    let compacted = format!("{compact}{historical_prompt}");
    std::fs::write(&transcript, &compacted).expect("write compacted transcript");
    let anchored =
        match claude_idle_compaction_reanchor(false, 99_999, compacted.len() as u64, true) {
            Some(ClaudeIdleTranscriptScan::CompactionReanchor { offset }) => offset,
            other => panic!("expected compaction anchor, got {other:?}"),
        };

    let fresh_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"fresh prompt after compact\"}]},\"sessionId\":\"s1\"}\n";
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open compacted transcript")
        .write_all(fresh_prompt.as_bytes())
        .expect("append fresh prompt");

    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, anchored)
            .expect("scan post-compact growth"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "fresh prompt after compact".to_string(),
            prompt_start_offset: anchored,
            line_end_offset: anchored + fresh_prompt.len() as u64,
            entry_id: None,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_preserves_normal_growth() {
    assert_eq!(
        claude_idle_compaction_reanchor(false, 100, 250, false),
        None
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let before = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"normally appended prompt\"}]},\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{before}{prompt}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, before.len() as u64)
            .expect("scan normal growth"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "normally appended prompt".to_string(),
            prompt_start_offset: before.len() as u64,
            line_end_offset: (before.len() + prompt.len()) as u64,
            entry_id: None,
        }
    );
}

#[test]
fn claude_idle_transcript_rotation_lookback_still_observes_new_file_prompt() {
    // A path/session rotation must never be mistaken for in-place compaction,
    // even if the prior binding offset and durable frontier exceed the new EOF.
    assert_eq!(
        claude_idle_compaction_reanchor(true, 99_999, 250, true),
        None
    );

    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("new-session.jsonl");
    let before = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s2\"}\n";
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"fresh rotation prompt\"}]},\"sessionId\":\"s2\"}\n";
    std::fs::write(&transcript, format!("{before}{prompt}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_last_prompt(&transcript, 0)
            .expect("scan replacement transcript lookback"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "fresh rotation prompt".to_string(),
            prompt_start_offset: before.len() as u64,
            line_end_offset: (before.len() + prompt.len()) as u64,
            entry_id: None,
        }
    );
}

#[cfg(unix)]
#[test]
fn claude_idle_response_start_offset_prefers_timestamp_boundary() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let first = r#"{"timestamp":"2026-05-28T00:00:00Z","type":"assistant"}"#;
    let second = r#"{"timestamp":"2026-05-28T00:00:10Z","type":"assistant"}"#;
    std::fs::write(&transcript, format!("{first}\n{second}\n")).expect("write transcript");
    let turn_started_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:00:10Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let offset = claude_idle_response_start_offset_after_timestamp(&transcript, turn_started_at, 0);

    assert_eq!(offset, first.len() as u64 + 1);
}

#[cfg(unix)]
#[test]
fn claude_idle_response_start_offset_resets_stale_fallback_after_shrink() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    std::fs::write(&transcript, "{}\n").expect("write transcript");
    let turn_started_at = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:00:10Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let offset =
        claude_idle_response_start_offset_after_timestamp(&transcript, turn_started_at, 99_999);

    assert_eq!(offset, 0);
}

// #3154 P1 (timestamp-anchor output loss): the worker-spawned BridgeAdapter
// tail must anchor to the claim's post-drain EOF `turn_start_offset`, NOT a
// `Utc::now()` timestamp scan. This proves the divergence on a transcript that
// models the deferred-claim wait window: prior-turn bytes occupy `[0, X)`;
// X is the post-drain EOF (the claim's `turn_start_offset`); THIS synthetic
// turn then writes its response bytes at `[X, EOF)` DURING the wait, all with
// timestamps that predate the worker's `Utc::now()` spawn (the worker spawns
// the tail only AFTER the deferred claim resolves).
//
// RED (old `Utc::now()` timestamp anchoring): the scan looks for the first
// line at/after `Utc::now()`. Every byte written during the wait predates it,
// so the scan returns None and the start offset lands at the fallback (the
// prior cursor) or — when the fallback is the stale binding cursor at X but
// the scan would have to advance PAST the turn's lines — the turn's bytes in
// `[X, EOF)` are skipped: output loss.
//
// GREEN (explicit `turn_start_offset` anchoring): the start offset is exactly
// X. The tail relays `[X, EOF)` — every byte of this turn, no skip — and never
// re-reads `[0, X)` (no prior-turn re-relay). The EOF offset is the boundary.
#[cfg(unix)]
#[test]
fn worker_bridge_tail_anchors_to_turn_start_offset_not_utc_now_timestamp_scan() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");

    // Prior turn's bytes: `[0, X)`. These are NOT part of this synthetic turn.
    let prior_a = r#"{"timestamp":"2026-05-28T00:00:00Z","type":"assistant"}"#;
    let prior_b = r#"{"timestamp":"2026-05-28T00:00:01Z","type":"assistant"}"#;
    let prior = format!("{prior_a}\n{prior_b}\n");
    let turn_start_offset = prior.len() as u64; // post-drain EOF == X (claim's turn_start_offset)

    // THIS synthetic turn's response bytes, written at `[X, EOF)` DURING the
    // deferred-claim wait. Their timestamps predate the worker's spawn instant.
    let turn_a = r#"{"timestamp":"2026-05-28T00:00:05Z","type":"assistant","text":"part-1"}"#;
    let turn_b = r#"{"timestamp":"2026-05-28T00:00:06Z","type":"assistant","text":"part-2"}"#;
    let turn = format!("{turn_a}\n{turn_b}\n");
    std::fs::write(&transcript, format!("{prior}{turn}")).expect("write transcript");
    let eof = (prior.len() + turn.len()) as u64;

    // The worker synthesizes `observed_at = Utc::now()` only AFTER the claim
    // wait — strictly after every byte above was written.
    let worker_spawn_now = chrono::DateTime::parse_from_rfc3339("2026-05-28T00:01:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    // The worker's fallback is the STALE binding cursor — a real pre-reseed
    // value that points PAST this turn (here: EOF). The explicit-anchor path
    // MUST override it; if the explicit offset were ignored and the timestamp
    // scan ran with this fallback, the turn's bytes would be skipped. Using a
    // stale-high fallback (not == X) is what makes the GREEN assertion FAIL if
    // the fix is reverted (explicit anchor ignored) — i.e. a true RED→GREEN.
    let fallback_offset = eof;

    // RED — the old `Utc::now()` timestamp anchoring (what the worker did
    // before this fix): `resolve_idle_tail_start_offset(.., explicit=None, ..)`
    // runs the timestamp scan. Every byte of this turn predates `worker_spawn_now`,
    // so the scan finds no boundary line and returns the fallback. The relay
    // window then starts at the fallback. Demonstrate the skip directly: when
    // the fallback is the stale-high prior cursor (a real pre-reseed value),
    // the timestamp path lands PAST this turn and skips ALL of its bytes.
    let red_offset = resolve_idle_tail_start_offset(
        &transcript,
        None, // old worker behaviour: no explicit anchor → Utc::now() scan
        worker_spawn_now,
        eof, // stale-high fallback (== EOF) the scan falls back to
    );
    assert_eq!(
        red_offset, eof,
        "RED: Utc::now() timestamp anchoring finds no boundary line (all bytes predate \
             the spawn instant) and falls back PAST this turn — the relay window [eof, eof) \
             skips every byte of this synthetic turn"
    );
    assert!(
        eof - red_offset < turn.len() as u64,
        "RED: bytes of this turn are skipped (relayed window is smaller than the turn)"
    );

    // GREEN — explicit anchoring on the claim's post-drain EOF `turn_start_offset`
    // (what the fixed worker passes: `explicit_start_offset = Some(turn_start_offset)`).
    // `observed_at`/`fallback` are IGNORED on this path.
    let green_offset = resolve_idle_tail_start_offset(
        &transcript,
        Some(turn_start_offset),
        worker_spawn_now, // must be ignored
        fallback_offset,  // must be ignored
    );
    assert_eq!(
        green_offset, turn_start_offset,
        "GREEN: explicit turn_start_offset anchoring relays from X — NO byte skip"
    );
    assert!(
        green_offset >= prior.len() as u64,
        "GREEN: the anchor never re-reads prior-turn bytes [0, X) (no re-relay)"
    );
    assert_eq!(
        eof - green_offset,
        turn.len() as u64,
        "GREEN: the relayed window [X, EOF) is EXACTLY this synthetic turn's bytes"
    );
}

#[test]
fn codex_idle_prompt_tails_only_new_ssh_direct_prompt() {
    assert!(codex_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    ));
    assert!(!codex_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
    ));
    assert!(!codex_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedRecentDuplicate
    ));
    assert!(!codex_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::Ignored
    ));
}

#[test]
fn claude_idle_prompt_tails_only_new_ssh_direct_prompt() {
    assert!(claude_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect
    ));
    assert!(!claude_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
    ));
    assert!(!claude_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedRecentDuplicate
    ));
    assert!(!claude_idle_prompt_observation_should_tail_response(
        crate::services::tui_prompt_dedupe::PromptObservation::Ignored
    ));
}

#[test]
fn claude_idle_transcript_scan_finds_user_prompt_and_stops_at_prompt_end() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let before = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"direct claude prompt\"}]},\"sessionId\":\"s1\"}\n";
    let after = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{before}{prompt}{after}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "direct claude prompt".to_string(),
            prompt_start_offset: before.len() as u64,
            line_end_offset: (before.len() + prompt.len()) as u64,
            entry_id: None,
        }
    );
    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, (before.len() + prompt.len()) as u64,)
            .expect("scan after prompt"),
        ClaudeIdleTranscriptScan::NoPrompt {
            offset: (before.len() + prompt.len() + after.len()) as u64,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_ignores_meta_user_prompt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let meta = "{\"type\":\"user\",\"isMeta\":true,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"_\"}]},\"sessionId\":\"s1\"}\n";
    let synthetic = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"No response requested.\"}]},\"sessionId\":\"s1\"}\n";
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"real prompt\"}]},\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{meta}{synthetic}{prompt}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "real prompt".to_string(),
            prompt_start_offset: (meta.len() + synthetic.len()) as u64,
            line_end_offset: (meta.len() + synthetic.len() + prompt.len()) as u64,
            entry_id: None,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_preserves_partial_trailing_jsonl() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let complete = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
    let partial = "{\"type\":\"user\",\"message\":{\"role\":\"user\"";
    std::fs::write(&transcript, format!("{complete}{partial}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("scan partial"),
        ClaudeIdleTranscriptScan::NoPrompt {
            offset: complete.len() as u64,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_for_last_prompt_selects_newest_in_window() {
    // #2843 (codex round-2 P1): a path-change lookback window holding an old
    // finished turn followed by the just-typed prompt must relay only the
    // newest prompt, not the first (which would re-relay the old turn).
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let old_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"old finished turn\"}]},\"sessionId\":\"s1\"}\n";
    let old_answer = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old answer\"}]},\"sessionId\":\"s1\"}\n";
    let new_prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"just typed prompt\"}]},\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{old_prompt}{old_answer}{new_prompt}"))
        .expect("write transcript");

    // First-prompt scan would return the OLD turn (the regression).
    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("first scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "old finished turn".to_string(),
            prompt_start_offset: 0,
            line_end_offset: old_prompt.len() as u64,
            entry_id: None,
        }
    );
    // Last-prompt scan returns the just-typed prompt instead.
    assert_eq!(
        scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("last scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "just typed prompt".to_string(),
            prompt_start_offset: (old_prompt.len() + old_answer.len()) as u64,
            line_end_offset: (old_prompt.len() + old_answer.len() + new_prompt.len()) as u64,
            entry_id: None,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_for_last_prompt_none_when_no_prompt() {
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let init = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n";
    let answer = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]},\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{init}{answer}")).expect("write transcript");

    assert_eq!(
        scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("scan"),
        ClaudeIdleTranscriptScan::NoPrompt {
            offset: (init.len() + answer.len()) as u64,
        }
    );
}

#[test]
fn claude_idle_transcript_scan_for_last_prompt_returns_complete_then_catches_next() {
    // #2843 (codex round-3/round-4): a partial trailing line is NOT consumed
    // and does NOT defer the already-found complete prompt. Deferring would
    // drop the current turn (resolve pins the binding at EOF before the
    // scan, so the next tick starts past the deferred prompt). Returning the
    // last complete prompt never drops the current turn: a prompt written
    // after it (mid-write this tick) is caught on the next tick by the
    // unchanged-path first-prompt scanner from the relayed prompt's line end.
    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let prompt = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"complete prompt\"}]},\"sessionId\":\"s1\"}\n";
    let next_partial = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"next";
    std::fs::write(&transcript, format!("{prompt}{next_partial}")).expect("write transcript");

    // Last-prompt scan returns the complete prompt, ignoring the partial.
    assert_eq!(
        scan_claude_idle_transcript_for_last_prompt(&transcript, 0).expect("scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "complete prompt".to_string(),
            prompt_start_offset: 0,
            line_end_offset: prompt.len() as u64,
            entry_id: None,
        }
    );

    // Once the trailing line completes, the next tick's first-prompt scanner
    // from the relayed prompt's line end catches it — nothing is dropped.
    let next = format!("{next_partial} prompt\"}}]}},\"sessionId\":\"s1\"}}\n");
    std::fs::write(&transcript, format!("{prompt}{next}")).expect("rewrite transcript");
    assert_eq!(
        scan_claude_idle_transcript_for_prompt(&transcript, prompt.len() as u64)
            .expect("next-tick scan"),
        ClaudeIdleTranscriptScan::Prompt {
            prompt: "next prompt".to_string(),
            prompt_start_offset: prompt.len() as u64,
            line_end_offset: (prompt.len() + next.len()) as u64,
            entry_id: None,
        }
    );
}

/// #3540: the watermark-reset / jsonl-head-rotation re-scan must NOT mint a
/// phantom synthetic inflight. End-to-end at the scan→observe seam:
///   1. scan a transcript whose `user` entry carries a stable `uuid`;
///   2. feed (prompt, entry_id) to the relay observer → it relays
///      (`PublishedSshDirect`) and records the uuid;
///   3. simulate a head rotation: rewrite the transcript so the SAME entry
///      survives at a SHIFTED offset (head clipped) — its uuid is unchanged;
///   4. re-scan from offset 0 (watermark reset) and feed the same entry_id to
///      the observer → it is suppressed by IDENTITY
///      (`SuppressedReplayedEntry`), so the scanner's
///      `claude_idle_prompt_observation_should_tail_response` is FALSE and no
///      external-turn lease / synthetic claim is taken.
#[test]
fn watermark_reset_rescan_of_relayed_entry_does_not_resynthesize_turn() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    crate::services::tui_prompt_dedupe::reset_state_for_tests();

    let dir = tempfile::tempdir().expect("temp dir");
    let transcript = dir.path().join("transcript.jsonl");
    let uuid = "1516634295270117460-uuid";
    let head = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"earlier answer\"}]},\"sessionId\":\"s1\"}\n";
    let prompt_line = format!(
        "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"deploy to make=live\"}}]}},\"sessionId\":\"s1\"}}\n"
    );
    std::fs::write(&transcript, format!("{head}{prompt_line}")).expect("write transcript");

    // ---- (1)/(2): first scan + observe → relays, records uuid. ----
    let scan = scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("first scan");
    let (prompt, entry_id) = match scan {
        ClaudeIdleTranscriptScan::Prompt {
            prompt, entry_id, ..
        } => (prompt, entry_id),
        other => panic!("expected Prompt, got {other:?}"),
    };
    assert_eq!(prompt, "deploy to make=live");
    assert_eq!(entry_id.as_deref(), Some(uuid));
    let first = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
        ProviderKind::Claude.as_str(),
        "tmux-3540",
        &prompt,
        entry_id.as_deref(),
        chrono::Utc::now(),
    );
    assert_eq!(
        first,
        crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect,
        "the freshly-typed prompt relays on first sighting"
    );
    assert!(
        claude_idle_prompt_observation_should_tail_response(first),
        "first sighting tails the response (mints the external turn)"
    );

    // ---- (3): head rotation — same entry survives at a SHIFTED offset. ----
    let new_head = "{\"type\":\"system\",\"subtype\":\"init\",\"sessionId\":\"s1\"}\n{\"type\":\"system\",\"subtype\":\"compact\",\"sessionId\":\"s1\"}\n";
    std::fs::write(&transcript, format!("{new_head}{prompt_line}"))
        .expect("rewrite transcript (rotation)");

    // ---- (4): watermark reset → re-scan from 0, same uuid → suppressed. ----
    let rescan = scan_claude_idle_transcript_for_prompt(&transcript, 0).expect("re-scan");
    let (re_prompt, re_entry_id) = match rescan {
        ClaudeIdleTranscriptScan::Prompt {
            prompt,
            entry_id,
            prompt_start_offset,
            ..
        } => {
            assert_eq!(
                prompt_start_offset,
                new_head.len() as u64,
                "the surviving entry is at a SHIFTED offset after head rotation"
            );
            (prompt, entry_id)
        }
        other => panic!("expected Prompt, got {other:?}"),
    };
    assert_eq!(
        re_entry_id.as_deref(),
        Some(uuid),
        "uuid survives the rotation"
    );
    let second = crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
        ProviderKind::Claude.as_str(),
        "tmux-3540",
        &re_prompt,
        re_entry_id.as_deref(),
        chrono::Utc::now(),
    );
    assert_eq!(
        second,
        crate::services::tui_prompt_dedupe::PromptObservation::SuppressedReplayedEntry,
        "#3540: the already-relayed entry re-encountered after a watermark \
             reset / head rotation is suppressed by identity"
    );
    assert!(
        !claude_idle_prompt_observation_should_tail_response(second),
        "#3540 (the fix): a suppressed-replayed entry does NOT tail a \
             response, so no external-turn lease / synthetic claim is taken — the \
             phantom synthetic inflight is never created"
    );
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
}

#[cfg(unix)]
#[test]
fn tui_idle_response_preserves_sideband_notifications_with_done() {
    let output = compose_tui_idle_response(
        Some("final answer".to_string()),
        None,
        "streamed answer".to_string(),
        vec![
            "[started] subagent launched".to_string(),
            "[completed] monitor finished".to_string(),
        ],
    );

    assert_eq!(
        output,
        "[started] subagent launched\n[completed] monitor finished\n\nfinal answer"
    );
}

#[cfg(unix)]
#[test]
fn tui_idle_response_strips_leading_resume_prompt_chrome() {
    let output = compose_tui_idle_response(
        Some("No response requested.fix2_3".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(output, "fix2_3");
}

#[cfg(unix)]
#[test]
fn tui_idle_response_preserves_legitimate_no_response_sentence() {
    let output = compose_tui_idle_response(
        Some("No response requested. But here is the explanation.".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(
        output,
        "No response requested. But here is the explanation."
    );
}

#[cfg(unix)]
#[test]
fn tui_idle_response_preserves_middle_resume_prompt_chrome_text() {
    let output = compose_tui_idle_response(
        Some("Hello\nNo response requested. trailing".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(output, "Hello\nNo response requested. trailing");
}

#[cfg(unix)]
#[test]
fn tui_idle_response_returns_empty_when_body_is_only_resume_prompt_chrome() {
    let output = compose_tui_idle_response(
        Some("No response requested.".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(output, "");
}

#[cfg(unix)]
#[test]
fn tui_idle_response_strips_multiple_leading_resume_prompt_chrome_chunks() {
    let output = compose_tui_idle_response(
        Some("Continue from where you left off.\nNo response requested.\nfinal answer".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(output, "final answer");
}

#[cfg(unix)]
#[test]
fn tui_idle_response_does_not_trim_when_no_resume_prompt_chrome() {
    let output = compose_tui_idle_response(
        Some("  intentional leading spaces".to_string()),
        None,
        String::new(),
        Vec::new(),
    );

    assert_eq!(output, "  intentional leading spaces");
}

#[cfg(unix)]
#[test]
fn idle_response_tail_discord_send_failure_does_not_advance_runtime_binding_offset() {
    assert!(!tui_idle_tail_should_commit_runtime_binding_offset(
        "final answer",
        false
    ));
    assert!(tui_idle_tail_should_commit_runtime_binding_offset(
        "final answer",
        true
    ));
    assert!(tui_idle_tail_should_commit_runtime_binding_offset(
        "", false
    ));
}

// #3041 P1-4 codex: the early-return guard armed right after
// `record_observed_external_turn_lease` must clear EXACTLY its recorded lease on
// drop, so a FAILURE early-return (registry None / notify resolve Err-503 /
// anchor POST failure) does not leave a dangling (BridgeAdapter-owned) lease
// blocking the legitimate watcher/sink delivery for the full TTL.
#[test]
fn observed_lease_early_return_guard_clears_recorded_lease_on_drop() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-early-return-guard-clear";
    let channel_id = ChannelId::new(960_000_000_000_001);
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:960000000000001:early:1".to_string()),
        session_key: Some("host:AgentDesk-early-return-guard-clear".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let recorded = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        lease,
    );
    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease_present(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        ),
        "lease must be present after record (would block delivery)"
    );

    // Simulate a failure early-return: the guard is armed and never disarmed, so
    // dropping it (function returns) clears the recorded lease.
    {
        let _guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id,
            recorded.generation,
        );
    }

    assert!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        )
        .is_none(),
        "an armed early-return guard drop must release the recorded lease so the watcher/sink can deliver"
    );
}

// #3041 P1-4 codex: on the SUCCESS path the caller DISARMs the guard before the
// bridge-tail ownership block, so the lease PERSISTS for the in-flight turn (the
// watcher/sink must not double-deliver). A disarmed guard's drop is a no-op.
#[test]
fn observed_lease_early_return_guard_disarm_preserves_lease() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-early-return-guard-disarm";
    let channel_id = ChannelId::new(960_000_000_000_002);
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:960000000000002:early:1".to_string()),
        session_key: Some("host:AgentDesk-early-return-guard-disarm".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    let recorded = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        lease,
    );

    {
        let mut guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id,
            recorded.generation,
        );
        // SUCCESS path: bridge legitimately takes ownership → disarm.
        guard.disarm();
    }

    assert_eq!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        ),
        Some(recorded),
        "a disarmed early-return guard must leave the lease intact for the in-flight turn"
    );
}

// #3041 P1-4 codex: the early-return guard clears BY GENERATION, so an OLD guard
// armed with turn-1's generation must NOT clobber a NEWER same-key lease recorded
// by turn-2 while turn-1 was awaiting the notify resolve / POST.
#[test]
fn observed_lease_early_return_guard_does_not_clobber_newer_lease() {
    let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap();
    let tmux = "AgentDesk-early-return-guard-noclobber";
    let channel_id = ChannelId::new(960_000_000_000_003);
    let base = ExternalInputRelayLease {
        channel_id: Some(channel_id.get()),
        turn_id: Some("external:claude:960000000000003:early:1".to_string()),
        session_key: Some("host:AgentDesk-early-return-guard-noclobber".to_string()),
        relay_owner: ExternalInputRelayOwner::BridgeAdapter,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    // turn-1 records and the relay arms a guard capturing G1.
    let recorded_turn1 = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        base.clone(),
    );
    let guard = TuiDirectObservedLeaseEarlyReturnGuard::arm(
        ProviderKind::Claude.as_str(),
        tmux,
        channel_id,
        recorded_turn1.generation,
    );
    // turn-2 records a NEWER same-key lease (G2) while turn-1 is in flight.
    let recorded_turn2 = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        ProviderKind::Claude.as_str(),
        tmux,
        ExternalInputRelayLease {
            turn_id: Some("external:claude:960000000000003:early:2".to_string()),
            ..base
        },
    );
    assert_ne!(recorded_turn1.generation, recorded_turn2.generation);

    // turn-1's guard drops (failure early-return) — by G1 it must NOT touch G2.
    drop(guard);

    assert_eq!(
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            ProviderKind::Claude.as_str(),
            tmux,
            channel_id.get(),
        ),
        Some(recorded_turn2),
        "an old early-return guard (G1) must leave turn-2's newer lease (G2) intact"
    );
}

// #3183: the idle-tail start offset must never fall below the watcher's
// committed delivery offset, so the tail cannot re-relay a byte range the
// tmux watcher already delivered (the double-relay regression).
#[cfg(unix)]
#[test]
fn idle_tail_start_offset_clamps_up_to_watcher_committed_offset() {
    // Watcher already committed delivery up to byte 500. A prompt-timestamp
    // derived start offset of 200 sits BELOW the committed end, so the tail
    // would re-relay [200, 500) — exactly the duplicate. The clamp lifts the
    // start to the committed end so nothing the watcher delivered is re-sent.
    assert_eq!(
        clamp_idle_tail_start_offset_to_committed(200, 500),
        500,
        "start offset below the watcher committed offset must clamp up to it"
    );
    // When the watcher covered the whole turn (committed == EOF-ish), the
    // tail starts at the committed end and finds nothing new to relay.
    assert_eq!(
        clamp_idle_tail_start_offset_to_committed(500, 500),
        500,
        "equal start offset is unchanged (no re-relay, no over-skip)"
    );
}

// #3183 outage fallback (#3176): when the watcher stopped / never covered the
// turn, `committed_relay_offset` is 0 (no confirmed delivery this process),
// so the clamp is a no-op and the tail still relays from the timestamp
// offset — no relay-loss regression.
#[cfg(unix)]
#[test]
fn idle_tail_start_offset_clamp_is_noop_when_watcher_not_covering() {
    // committed == 0: watcher delivered nothing → the tail keeps its
    // timestamp-derived start offset and relays the full turn.
    assert_eq!(
        clamp_idle_tail_start_offset_to_committed(200, 0),
        200,
        "no committed delivery must leave the timestamp start offset intact (outage fallback)"
    );
    // A committed offset that lags the timestamp offset (watcher delivered an
    // OLDER region only) also must not pull the start backwards.
    assert_eq!(
        clamp_idle_tail_start_offset_to_committed(800, 300),
        800,
        "a lagging committed offset must not drag the start offset backwards"
    );
}

// #3358: a new synthetic inflight whose `relay_last_offset()` LAGS the
// watcher's committed frontier must be born at/above that frontier so a
// later same-identity re-claim cannot regress `turn_start_offset` /
// `last_offset` below already-delivered bytes (the monotonicity ERROR triple).
// The committed frontier is `Some(..)` here because the caller validated it
// against the CURRENT wrapper generation (see the generation-mismatch test).
#[test]
fn synthetic_start_offset_carries_committed_frontier_forward() {
    // relay_last_offset lags (2821677) the watcher committed end (2838484):
    // born at the committed frontier so no backward re-seed is possible.
    assert_eq!(
        synthetic_start_offset_carry_forward(2_821_677, Some(2_838_484)),
        2_838_484,
        "lagging relay_last_offset must carry the committed frontier forward"
    );
    // Equal frontier → unchanged (born exactly at the committed end).
    assert_eq!(
        synthetic_start_offset_carry_forward(2_838_484, Some(2_838_484)),
        2_838_484
    );
}

// #3358 round 2 — Finding 1 guard: a STALE committed watermark from a
// PREVIOUS wrapper generation must NOT clamp the synthetic forward. The
// caller proves same-generation identity and passes `None` on mismatch, so
// the helper falls back to `relay_last_offset` only. This is the content-skip
// prevention: after a wrapper restart the stream resets to 0 and the new
// synthetic must be born at its own (lagging) relay cursor, NOT lifted over a
// stale frontier that would mark future bytes as already delivered.
#[test]
fn synthetic_start_offset_no_clamp_on_generation_mismatch() {
    // Generation mismatch → caller passes `None`: pre-fix seeding
    // (`relay_last_offset` only), even though a stale watermark (2838484) was
    // numerically higher. The rare monotonicity ERROR here is preferable to a
    // content skip (see helper doc).
    assert_eq!(
        synthetic_start_offset_carry_forward(2_821_677, None),
        2_821_677,
        "a generation-mismatched (stale) watermark must NOT clamp the synthetic forward"
    );
    // Fresh stream reset to 0 after restart, stale watermark unproven → birth
    // stays at 0, so the watcher walks the new generation from the head.
    assert_eq!(
        synthetic_start_offset_carry_forward(0, None),
        0,
        "a fresh post-restart stream must not be lifted over a stale frontier (content skip)"
    );
}

// #3358 genuine-regression guard: the carry-forward is BOUNDED to the
// synthetic-creation handover — it never DRAGS a healthy start offset
// backwards, and a missing/lagging committed frontier (outage / no confirmed
// delivery) leaves the relay_last_offset intact so the invariants still catch
// real backward writes elsewhere.
#[test]
fn synthetic_start_offset_carry_forward_never_regresses() {
    // committed unprovable/absent (`None`) → no-op.
    assert_eq!(
        synthetic_start_offset_carry_forward(2_821_677, None),
        2_821_677
    );
    // committed lags relay_last_offset → must NOT pull the start backwards.
    assert_eq!(
        synthetic_start_offset_carry_forward(900, Some(300)),
        900,
        "a lagging committed frontier must never drag the synthetic start backwards"
    );
}
