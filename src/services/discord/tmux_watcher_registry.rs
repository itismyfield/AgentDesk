use super::*;

use crate::config::ExecutionIdentityMode;

/// Shared state for the Discord bot (multi-channel: each channel has its own session)
/// Handle for a background tmux output watcher
pub(in crate::services::discord) struct TmuxWatcherHandle {
    /// Tmux session this watcher owns. Used to enforce the single-watcher
    /// policy when the same session is reattached through another path.
    pub(in crate::services::discord) tmux_session_name: String,
    /// JSONL/transcript path this watcher tails for the session. A single tmux
    /// session can change relay files when it graduates from the prelaunch
    /// wrapper to a provider-native TUI handoff.
    pub(in crate::services::discord) output_path: String,
    /// Signal to pause monitoring (while Discord handler reads its own turn)
    pub(in crate::services::discord) paused: Arc<std::sync::atomic::AtomicBool>,
    /// After Discord handler finishes its turn, set this offset so watcher resumes from here
    pub(in crate::services::discord) resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    /// Signal to cancel the watcher (quiet exit, no "session ended" message)
    pub(in crate::services::discord) cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Epoch counter: incremented each time paused is set to true.
    /// Watcher snapshots this before reading; if it changed, the read is stale.
    pub(in crate::services::discord) pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Set by turn_bridge when it delivers the response directly (non-handoff path).
    /// Watcher checks this before relay to avoid duplicate messages.
    pub(in crate::services::discord) turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    /// Updated by the watcher task loop. If this stops moving while the registry
    /// still has a slot, the slot is stale and must not suppress a new watcher.
    pub(in crate::services::discord) last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
}

// #3016 phase-5b2: the per-handle `mailbox_finalize_owed: Arc<AtomicBool>` field
// (#1452 turn-scoped bridge→watcher finalization debt) has been removed. Phase-5b1
// replaced every finalize-decision consumer of the flag — the watcher's
// normal-completion finalize now fires on the confirmed-completion / structural
// signal (`normal_completion = true`), and the bridge-handoff invariant uses the
// ledger's `register_start(RelayOwnerKind::Watcher)` authority — so the flag was
// write-only and is now deleted entirely with identical behaviour.

pub(in crate::services::discord) const TMUX_WATCHER_STALE_HEARTBEAT_MS: i64 = 60_000;

pub(in crate::services::discord) fn tmux_watcher_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl TmuxWatcherHandle {
    pub(in crate::services::discord) fn heartbeat_stale(&self) -> bool {
        let last = self
            .last_heartbeat_ts_ms
            .load(std::sync::atomic::Ordering::Acquire);
        last <= 0 || tmux_watcher_now_ms().saturating_sub(last) > TMUX_WATCHER_STALE_HEARTBEAT_MS
    }
}

pub(in crate::services::discord) type TmuxWatcherRegistryGuard = std::sync::MutexGuard<'static, ()>;

static TMUX_WATCHER_REGISTRY_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub(in crate::services::discord) fn lock_tmux_watcher_registry() -> TmuxWatcherRegistryGuard {
    TMUX_WATCHER_REGISTRY_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The `.spawn_nonce` read model lives under `super::tmux`, which is
/// `cfg(unix)`; this module is not.
#[cfg(unix)]
use super::tmux::execution_identity::{
    capture_spawn_nonce as capture_session_spawn_nonce, destruction_permitted_under_identity,
};

/// A platform that cannot host a tmux session has no marker to capture, so the
/// comparison is permanently `Unknown` there — the same "absence of evidence is
/// not proof" answer a missing marker gets on unix, and only `Enforce` turns it
/// into a deny. There is no counter to move because there is no marker store.
#[cfg(not(unix))]
fn capture_session_spawn_nonce(_tmux_session_name: &str) -> Option<String> {
    None
}

#[cfg(not(unix))]
fn destruction_permitted_under_identity(
    mode: ExecutionIdentityMode,
    _site: &'static str,
    _tmux_session_name: &str,
    _captured_spawn_nonce: Option<&str>,
) -> bool {
    !mode.denies_on_incarnation_mismatch()
}

#[cfg(test)]
static EXECUTION_IDENTITY_MODE_OVERRIDE: std::sync::Mutex<Option<ExecutionIdentityMode>> =
    std::sync::Mutex::new(None);

/// The live rollout stage for the #5071 T3 identity fence.
///
/// Read per decision through `config_live_reload::current()` so an
/// `agentdesk.yaml` edit applies without a restart, and `Legacy` before the boot
/// config is installed. `Legacy` is also the compiled-in default, so an
/// untouched deployment keeps each converted call site's pre-existing registry
/// CAS verbatim — the channel binding, session name, output path and cancel
/// pointer for `cancel_and_remove_channel_if_current`; the session key and the
/// cancel pointer alone for `remove_tmux_session_if_current`.
///
/// `Legacy` is NOT "this PR's behaviour reverted": #5071 T3-A1 deleted the two
/// #5067 in-flight emission fences outright, in every mode, and no value of this
/// switch brings them back. The switch only chooses whether the identity
/// conjuncts below are ignored, counted, or enforced. Restoring the fenced
/// baseline means reverting the T3-A1 PR.
pub(in crate::services::discord) fn execution_identity_mode() -> ExecutionIdentityMode {
    #[cfg(test)]
    if let Some(mode) = *EXECUTION_IDENTITY_MODE_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
    {
        return mode;
    }
    crate::config_live_reload::current()
        .map(|config| config.runtime.execution_identity_mode)
        .unwrap_or_default()
}

#[cfg(test)]
pub(in crate::services::discord) struct ExecutionIdentityModeGuard {
    previous: Option<ExecutionIdentityMode>,
}

#[cfg(test)]
impl Drop for ExecutionIdentityModeGuard {
    fn drop(&mut self) {
        *EXECUTION_IDENTITY_MODE_OVERRIDE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = self.previous.take();
    }
}

/// Override [`execution_identity_mode`] for the lifetime of the returned guard.
/// Process-global, so callers must already hold whichever test lock serializes
/// their suite.
#[cfg(test)]
pub(in crate::services::discord) fn set_execution_identity_mode_for_tests(
    mode: ExecutionIdentityMode,
) -> ExecutionIdentityModeGuard {
    let previous = EXECUTION_IDENTITY_MODE_OVERRIDE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .replace(mode);
    ExecutionIdentityModeGuard { previous }
}

/// The owner-channel and output-path VALUES a caller read off the live registry
/// row before its CAS, for the T3-R2 conjuncts its removal helper does not
/// compare on its own.
///
/// `remove_tmux_session_if_current` compares the session key and the cancel
/// pointer and nothing else, so T3-R2's `(owner_channel, tmux_session_name,
/// output_path, Arc::ptr_eq(cancel))` tuple is complete at that call site only
/// when the caller pins the owner channel and the output path here. The
/// canceling sibling `cancel_and_remove_channel_if_current` already compares
/// both itself, so it pins nothing and this stays `None` there.
struct PinnedWatcherBinding {
    owner_channel_id: ChannelId,
    output_path: String,
}

/// #5071 T3-A1: the execution-identity conjunct the two H6 registry CAS sites
/// add on top of the existing `(channel, session, output_path, cancel pointer)`
/// comparison, plus the [`PinnedWatcherBinding`] half of that comparison for the
/// call site whose helper does not make it.
///
/// For a mode that consumes the comparison, the nonce is captured once at
/// decision time and re-read inside the registry lock immediately before the
/// removal, so the re-read cannot interleave with another registry mutation.
/// The spawn path does not take that lock, so a marker rename racing the
/// re-read is still possible; the design records that as a non-guarantee rather
/// than papering over it. This is an identity re-check, not an emission lease.
/// `Legacy` consumes neither read and so makes neither (#5399).
///
/// # What the completed tuple establishes
///
/// A VALUE match, and only that. Under `Enforce` the two CAS cores below refuse
/// whenever any captured value stops equalling the live one, which covers every
/// shape the conjuncts were added for: a row whose owner channel moved, a row
/// whose output path moved, a replacement watcher carrying a fresh cancel `Arc`
/// (`Arc::ptr_eq` fails), a respawn that minted a new `.spawn_nonce`, and a
/// marker that is unreadable on either side. `Legacy` keeps each call site's
/// pre-A1 comparison verbatim and `Observe` only counts.
///
/// # Declared limit: A -> B -> A readmission is indistinguishable
///
/// Every conjunct compares captured values against live values, and neither the
/// registry nor this fence keeps a row generation, epoch, or insertion counter
/// for the comparison to read. So a row that is replaced by a different one and
/// then re-admitted with ALL of its pinned values restored — the same owner
/// channel, the same output path, the very same cancel `Arc`, and an untouched
/// `.spawn_nonce` — compares equal to a row that never moved, and `Enforce`
/// permits the removal. That readmission is a DECLARED limit of a value CAS, not
/// a defect these conjuncts were meant to catch; #5071 T3 deliberately adds no
/// generation counter to close it. The documentation test
/// `value_cas_declared_non_guarantee_readmitted_identical_row_passes_enforce`
/// (in `tmux_watcher_registry_restore_tests`) fixes the current behaviour, so a
/// future change to it forces this paragraph to be rewritten.
///
/// No production writer can build that sequence at this commit. `insert_locked`
/// is the registry's only insertion path, and its only production callers are
/// `watchers::lifecycle::claims::{try_claim_watcher_with_thread_parent,
/// claim_watcher}`. Both take the handle by value from the caller, and every
/// production caller that reaches them constructs a NEW handle with a fresh
/// `Arc<AtomicBool>` for `cancel` immediately beforehand:
/// `turn_bridge::runtime_handoff_loop` and its `watcher_handoff` sibling;
/// `watchers::lifecycle::restore::restore_tmux_watchers`;
/// `recovery_engine::restore_inflight::restore_inflight_turns` (two
/// constructions); `attach_paused_turn_watcher_inner` in
/// `router::message_handler::watchdog`; and
/// `recovery_engine::manual_rebind::rebind_inflight_for_channel_inner`. So no
/// production re-admission restores the pinned pointer. The handoff pair is the
/// closest shape to a "move" and is not one: it mints a fresh pointer and lets
/// `claim_watcher` cancel and remove the incumbent, which is a replacement the
/// CAS correctly refuses for a pin taken before it.
///
/// That is an enumeration of today's writers, not an invariant the types hold. A
/// future writer that re-inserts a handle carrying an existing `cancel` `Arc`
/// would make the sequence reachable and would falsify this paragraph.
pub(in crate::services::discord) struct WatcherIdentityFence {
    mode: ExecutionIdentityMode,
    /// Static label the observation counters and logs attribute this to.
    site: &'static str,
    tmux_session_name: String,
    captured_spawn_nonce: Option<String>,
    pinned_binding: Option<PinnedWatcherBinding>,
}

impl WatcherIdentityFence {
    /// #5399: the marker is read only for a mode that will use the comparison.
    /// `Legacy` captures `None` without touching the disk, which is the same
    /// pin it would have discarded — the answer at the CAS below is unchanged.
    pub(in crate::services::discord) fn capture(
        mode: ExecutionIdentityMode,
        site: &'static str,
        tmux_session_name: &str,
    ) -> Self {
        let captured_spawn_nonce = mode
            .consults_spawn_nonce()
            .then(|| capture_session_spawn_nonce(tmux_session_name))
            .flatten();
        Self {
            mode,
            site,
            tmux_session_name: tmux_session_name.to_string(),
            captured_spawn_nonce,
            pinned_binding: None,
        }
    }

    /// Carry the owner channel and output path the caller read off the same
    /// live registry row it pinned its cancel pointer from, so the CAS can
    /// re-compare them.
    pub(in crate::services::discord) fn with_pinned_binding(
        mut self,
        owner_channel_id: ChannelId,
        output_path: &str,
    ) -> Self {
        self.pinned_binding = Some(PinnedWatcherBinding {
            owner_channel_id,
            output_path: output_path.to_string(),
        });
        self
    }

    fn permits_destruction(
        &self,
        live_owner_channel_id: Option<ChannelId>,
        live_output_path: &str,
    ) -> bool {
        // Evaluate both conjuncts before either can veto, so `Observe` records
        // the same pair of outcomes `Enforce` decides on.
        let binding_permits = self.permits_pinned_binding(live_owner_channel_id, live_output_path);
        let identity_permits = destruction_permitted_under_identity(
            self.mode,
            self.site,
            &self.tmux_session_name,
            self.captured_spawn_nonce.as_deref(),
        );
        binding_permits && identity_permits
    }

    /// The owner-channel/output-path half of T3-R2. A caller that pinned no
    /// binding always passes here, because its own helper made this comparison.
    ///
    /// The mode ladder matches the nonce conjunct's: `Legacy` neither records
    /// nor refuses, so the pre-A1 answer at the pinning call site — the session
    /// key plus `Arc::ptr_eq(cancel)`, and nothing more — is what `Legacy` still
    /// gives; `Observe` records; only `Enforce` refuses.
    fn permits_pinned_binding(
        &self,
        live_owner_channel_id: Option<ChannelId>,
        live_output_path: &str,
    ) -> bool {
        let Some(pinned) = self.pinned_binding.as_ref() else {
            return true;
        };
        if live_owner_channel_id == Some(pinned.owner_channel_id)
            && live_output_path == pinned.output_path
        {
            return true;
        }
        if self.mode.records_identity_observations() {
            tracing::info!(
                counter = "execution_identity_binding_mismatch",
                site = self.site,
                session_key = self.tmux_session_name.as_str(),
                pinned_owner_channel_id = pinned.owner_channel_id.get(),
                live_owner_channel_id = live_owner_channel_id.map(ChannelId::get).unwrap_or(0),
                output_path_changed = live_output_path != pinned.output_path,
                "pinned watcher binding no longer matches the live registry row"
            );
        }
        !self.mode.denies_on_incarnation_mismatch()
    }
}

/// #5071 relay-tail S4 (I-1): the delivery-lease conjunct the two fenced
/// registry CAS sites add alongside [`WatcherIdentityFence`].
///
/// The identity fence answers "is this still the SAME watcher incarnation?".
/// This one answers a different question the identity conjuncts explicitly do
/// not — "is a terminal delivery for the very turn being destroyed still in
/// flight?" — because a same-incarnation emission race is a declared
/// non-guarantee of the value CAS (see [`WatcherIdentityFence`]).
///
/// # What it compares, and why the key comparison is the point
///
/// A channel-wide "is any lease held?" test is NOT usable here: the delivery
/// lease is a per-channel cell reused across sequential turns, so a lease taken
/// by a LATER sink/bridge owner would veto the cleanup of an OLDER stale watcher
/// forever. The conjunct therefore refuses only on
/// `LeaseSnapshot::identity_matched` against the key the destructive probe was
/// built from, i.e. the same `(channel, generation, user_msg_id | started_at +
/// turn_start_offset)` tuple the turn itself leases under, AND only while that
/// lease is still `Leased` with an unelapsed deadline.
///
/// # What it does NOT establish
///
/// * `Committed` is not a veto. A committed lease has no deadline and awaits
///   only a release; its holder is done sending.
/// * The deadline is holder LIVENESS, not a delivery duration cap — a live
///   holder heartbeat-renews it (`DeliveryLeaseHeartbeat`), so this refuses for
///   as long as the holder keeps renewing and permits within roughly one
///   `DELIVERY_LEASE_DEADLINE_MS` after a holder dies. That bound is the whole
///   reason this veto is safe to add: unlike the #5067 emission fence #5071
///   T3-A1 deleted, it cannot latch.
/// * A degenerate legacy id-0 key collapses sibling turns (see
///   [`LeaseSnapshot::identity_matched`]), so on that residual class the veto is
///   channel-and-generation wide rather than turn-precise. It fails CLOSED —
///   toward keeping the watcher — which is the direction this gate wants.
/// * A turn whose relay owner leased under the FALLBACK-OFFSET key can never be
///   matched here, so it fails OPEN. `tmux_watcher::turn_identity::
///   pinned_delivery_lease_key` falls back to
///   `DeliveryLeaseKey::new_for_site_with_fallback_offset(.., 0, None, None,
///   Some(relay_range_start))` whenever no inflight row matched the session, and
///   that key is `(user_msg_id 0, started_at None, turn_start_offset Some)`.
///   `expected_key` here always comes from
///   `DestructiveCancelProbeSnapshot::delivery_lease_key`, i.e. from
///   `DeliveryLeaseKey::from_inflight_state_for_site`, which passes NO fallback
///   offset and therefore only ever produces `(id, None, None)`, `(0,
///   Some(started_at), Some(offset))` or the degenerate `(0, None, None)`. None
///   of those three shapes can equal the fallback shape, so whenever the relay
///   owner took that branch this conjunct permits regardless of how live the
///   delivery is. This is a fail-OPEN residual, unlike the degenerate id-0 class
///   above.
/// * Nothing about leases on OTHER channels, and nothing at all about the
///   destructive call sites listed as out of scope in the S4 commit body; those
///   still reach the unfenced helpers.
///
/// # Lock order (#5071 relay-tail S4 r2, P1-1)
///
/// [`TerminalDeliveryFence::commit_if_permitted`] holds the lease cell's payload
/// mutex across BOTH the judgment and the registry mutation it authorizes, which
/// is the only way the two can be atomic against a concurrent `try_acquire` (see
/// [`DeliveryLeaseCell::with_state_locked`]). That establishes the nesting
///
/// ```text
/// TMUX_WATCHER_REGISTRY_LOCK  ->  DeliveryLeaseCell::payload  ->  registry DashMap shards
/// ```
///
/// and both reverse directions were enumerated to be absent at this commit:
///
/// * `payload -> TMUX_WATCHER_REGISTRY_LOCK` cannot exist. The payload mutex is
///   private to `DeliveryLeaseCell` and is only ever taken inside its own
///   methods; exactly one of them (`with_state_locked`) runs caller code under
///   it, and its only callers are `read()` (which runs nothing) and this fence,
///   which is already under the registry lock when it gets there.
/// * `DashMap shard -> payload` was checked against every site that holds a
///   registry shard guard — a `Ref` from `tmux_watchers.get`/`.iter()` or from
///   the `by_tmux_session` field. None of them touches a delivery lease while
///   the guard is live: `turn_finalizer/watcher_backstop.rs` explicitly drops
///   its `Ref` before the lease read further down, and every other site either
///   clones values out of the `Ref` immediately or only performs atomic loads
///   and stores on the handle. The transitive callees that DO run under a live
///   `Ref` — `destructive_cancel_gate::fresh_watcher_heartbeat_should_block`,
///   `health/relay_auto_heal::nudge_watcher_handle_for_backlog` and its
///   `redrive_should_yield_to_live_relay` — reach no lease cell.
///
/// That is an enumeration of today's callers, not an invariant the types hold. A
/// future reader that keeps a registry `Ref` alive across a `DeliveryLeaseCell`
/// call would close the cycle and falsify this paragraph.
pub(in crate::services::discord) struct TerminalDeliveryFence {
    lease: Arc<DeliveryLeaseCell>,
    expected_key: DeliveryLeaseKey,
    /// Static label the veto log attributes this to; mirrors
    /// [`WatcherIdentityFence`]'s `site`.
    site: &'static str,
}

impl TerminalDeliveryFence {
    /// Pin the channel's lease cell and the turn identity to re-read it against.
    /// Both are cheap value captures — the `Arc` is the live per-channel cell,
    /// so the CAS below reads the CURRENT state through it, not a copy taken
    /// here.
    pub(in crate::services::discord) fn capture(
        lease: Arc<DeliveryLeaseCell>,
        expected_key: DeliveryLeaseKey,
        site: &'static str,
    ) -> Self {
        Self {
            lease,
            expected_key,
            site,
        }
    }

    /// Judge the lease and, if it permits, run `commit` — BOTH under one hold of
    /// the lease cell's payload mutex. Returns `None` when the conjunct refuses,
    /// in which case `commit` never ran.
    ///
    /// #5071 relay-tail S4 r2 (P1-1) is exactly this atomicity. The r1 shape read
    /// a `LeaseSnapshot`, dropped the payload mutex on the way out of `read()`,
    /// returned a `bool`, and only then let the caller remove the row. The
    /// registry lock the caller holds is a DIFFERENT lock from the lease cell's,
    /// so it fences nothing here: a `Sink`/`Bridge` `try_acquire` under the very
    /// key just judged absent could win inside that gap, and the removal would
    /// proceed on a judgment that was already false. Deciding and acting under
    /// the one mutex every lease mutation also runs under closes it — a racing
    /// acquirer now either wins BEFORE the judgment (and is seen, and vetoes) or
    /// blocks until the removal is already committed.
    fn commit_if_permitted<T>(&self, commit: impl FnOnce() -> T) -> Option<T> {
        self.lease.with_state_locked(|snapshot| {
            // `lease_now_ms` is the clock the deadline was written against
            // (process-monotonic, anchored to a process-start `Instant`).
            // Comparing it to a wall clock would make an NTP step decide this
            // conjunct.
            let now_ms = lease_now_ms();
            if let Some(deadline_ms) = snapshot
                .identity_matched(&self.expected_key)
                .and_then(|matched| matched.deadline_ms)
                && deadline_ms > now_ms
            {
                tracing::info!(
                    counter = "terminal_delivery_fence_veto",
                    site = self.site,
                    channel_id = self.expected_key.channel_id().get(),
                    deadline_ms,
                    now_ms,
                    "identity-matched delivery lease is still live; refusing the destructive watcher removal"
                );
                return None;
            }
            #[cfg(test)]
            run_delivery_fence_permitted_hook_for_tests(self.site);
            Some(commit())
        })
    }
}

/// The S4 (I-1) conjunct, spelled ONCE so both CAS cores gate their commit
/// through the same predicate and one mutation can neutralize it. `None` means
/// the conjunct refused and `commit` did not run; a caller with no fence bound
/// commits unconditionally.
fn commit_under_delivery_fence<T>(
    delivery: Option<&TerminalDeliveryFence>,
    commit: impl FnOnce() -> T,
) -> Option<T> {
    match delivery {
        None => Some(commit()),
        Some(fence) => fence.commit_if_permitted(commit),
    }
}

/// #5071 relay-tail S4 r2 (P1-1): fires inside `commit_if_permitted`, AFTER the
/// conjunct permitted and BEFORE the registry mutation runs — i.e. in the exact
/// window the r1 shape left open, and while the payload mutex is held. Only the
/// atomicity test installs it, and it is filtered by fence `site` so a fence
/// running concurrently in another test is unaffected.
#[cfg(test)]
type DeliveryFencePermittedHook = Arc<dyn Fn(&'static str) + Send + Sync + 'static>;

#[cfg(test)]
static DELIVERY_FENCE_PERMITTED_HOOK: std::sync::LazyLock<
    std::sync::Mutex<Option<DeliveryFencePermittedHook>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

#[cfg(test)]
fn run_delivery_fence_permitted_hook_for_tests(site: &'static str) {
    let hook = DELIVERY_FENCE_PERMITTED_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    if let Some(hook) = hook {
        hook(site);
    }
}

#[cfg(test)]
pub(in crate::services::discord) struct DeliveryFencePermittedHookGuard;

#[cfg(test)]
impl Drop for DeliveryFencePermittedHookGuard {
    fn drop(&mut self) {
        *DELIVERY_FENCE_PERMITTED_HOOK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn set_delivery_fence_permitted_hook_for_tests(
    hook: DeliveryFencePermittedHook,
) -> DeliveryFencePermittedHookGuard {
    *DELIVERY_FENCE_PERMITTED_HOOK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hook);
    DeliveryFencePermittedHookGuard
}

/// #5071 relay-tail S4 r2 (P1-2 ②): the HALF-BUILT view
/// [`TmuxWatcherRegistry::under_identity_fence`] returns. It carries the
/// [`WatcherIdentityFence`] and deliberately exposes NO destructive method — the
/// only thing that can be done with it is
/// [`IdentityFencePendingDelivery::with_terminal_delivery_fence`], which
/// consumes it and yields the [`IdentityFencedRegistry`] that can actually
/// remove.
///
/// This is the type-level half of keeping the S4 conjunct attached. In r1 both
/// fences rode on one view with `delivery: Option<_>`, so deleting the single
/// `.with_terminal_delivery_fence(..)` line at a call site still compiled and
/// still removed — silently unfenced. Now that deletion does not typecheck. The
/// [`identity_fence_bind`/`delivery_fence_bind` pairing check][ratchet] in
/// `scripts/check_destructive_call_site_ratchet.py` is the lexical half, and
/// catches the same omission for anything the type system stops covering (a new
/// binder added in a file the compiler is happy with but reviewers should see).
///
/// [ratchet]: ../../../scripts/check_destructive_call_site_ratchet.py
pub(in crate::services::discord) struct IdentityFencePendingDelivery<'a> {
    registry: &'a TmuxWatcherRegistry,
    fence: WatcherIdentityFence,
}

impl<'a> IdentityFencePendingDelivery<'a> {
    /// Add the S4 delivery-lease conjunct and complete the view. Separate from
    /// [`TmuxWatcherRegistry::under_identity_fence`] so the two production
    /// callers stay the only `under_identity_fence` sites the destructive
    /// ratchet counts, and REQUIRED because a caller that has no lease
    /// coordinate to pin must be a compile error rather than a silently `None`
    /// argument.
    pub(in crate::services::discord) fn with_terminal_delivery_fence(
        self,
        delivery: TerminalDeliveryFence,
    ) -> IdentityFencedRegistry<'a> {
        IdentityFencedRegistry {
            registry: self.registry,
            fence: self.fence,
            delivery,
        }
    }
}

/// A registry view whose two CAS removals also require the
/// [`WatcherIdentityFence`] conjunct AND the [`TerminalDeliveryFence`] one. It
/// is unconstructible without both: the only way to reach it is
/// [`IdentityFencePendingDelivery::with_terminal_delivery_fence`].
///
/// The fenced methods keep the SAME names as their unfenced originals on
/// purpose: the #5071 T3-A4 destructive-call-site ratchet counts those names
/// lexically, so routing a destructive removal through this view must not make
/// the call site disappear from the inventory.
pub(in crate::services::discord) struct IdentityFencedRegistry<'a> {
    registry: &'a TmuxWatcherRegistry,
    fence: WatcherIdentityFence,
    delivery: TerminalDeliveryFence,
}

impl IdentityFencedRegistry<'_> {
    /// Only REMOVES — like the unfenced original it never writes `cancel`, so
    /// the caller stores it after a `Some` return.
    pub(in crate::services::discord) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.registry.remove_tmux_session_if_current_locked(
            &guard,
            tmux_session_name,
            expected_cancel,
            Some(&self.fence),
            Some(&self.delivery),
        )
    }

    /// Cancels internally, so a `true` return already means the removed
    /// handle's `cancel` was stored — and a `false` return means nothing was.
    pub(in crate::services::discord) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.registry.cancel_and_remove_channel_if_current_locked(
            &guard,
            channel_id,
            expected_tmux_session_name,
            expected_output_path,
            expected_cancel,
            Some(&self.fence),
            Some(&self.delivery),
        )
    }
}

/// Registry for active tmux output watchers.
///
/// Ownership is keyed by tmux session name so duplicate attaches for the same
/// live session converge before a second relay can spawn. A channel index is
/// retained for existing routing and diagnostics callers that ask "does this
/// Discord channel currently have watcher coverage?".
pub(in crate::services) struct TmuxWatcherRegistry {
    pub(in crate::services::discord) by_tmux_session: dashmap::DashMap<String, TmuxWatcherHandle>,
    tmux_session_by_channel: dashmap::DashMap<ChannelId, String>,
    owner_channel_by_tmux_session: dashmap::DashMap<String, ChannelId>,
    /// #3105: authoritative owner-channel bindings re-registered for LIVE tmux
    /// sessions that currently have no live watcher handle — e.g. a Claude TUI
    /// session the user is typing into directly whose watcher slot was evicted
    /// by a compact/restart/rebind and never re-claimed (no foreground turn).
    ///
    /// This is part of the authoritative registry, NOT the `tui_prompt_dedupe`
    /// mirror: it is sourced only from the configured channel→provider bindings
    /// (`settings::list_registered_channel_bindings`), which deterministically
    /// resolve a session's owner channel from its (base or thread-suffixed)
    /// tmux name. Kept in a separate map so the strict 1:1 watcher-handle
    /// invariant across the three maps above is untouched; the live watcher map
    /// always wins on lookup, and a real watcher claim for the session clears
    /// the restored entry so it can never shadow live truth.
    restored_owner_by_tmux_session: dashmap::DashMap<String, ChannelId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct TmuxWatcherBinding {
    pub(in crate::services::discord) owner_channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: String,
    /// #5071 T4-B0 (#4987 S0): the transcript the LIVE watcher handle for this
    /// session is tailing, read off the same registry lookup that resolved the
    /// binding.
    ///
    /// [`TmuxWatcherHandle::output_path`] has always carried this, and two
    /// existing reads already reach it: [`TmuxWatcherRegistry::watcher_output_path`]
    /// for a caller who already knows the tmux session name, and
    /// [`TmuxWatcherRegistry::get`] for one keyed by `ChannelId` —
    /// `health::relay_auto_heal` gates its redrive nudge on exactly that
    /// `get(&owner_channel_id)` → `.output_path` hop. So the claim here is
    /// narrower than "no route existed": what `channel_binding` did not carry
    /// was the coordinate. It is the lock-free VALUE snapshot of a watcher slot
    /// — owner channel and session name, cloned out, holding nothing borrowed
    /// once it returns — whereas `get` hands back a `dashmap::mapref::one::Ref`
    /// that borrows the entry and holds a read guard on its shard for as long
    /// as the caller keeps it. This field puts the transcript on the snapshot
    /// side of that split; it is populated by, and stays equal to,
    /// `watcher_output_path` for the same session.
    ///
    /// It is worth its own coordinate because it is sourced independently of
    /// the in-flight row's `output_path`. `watchers::lifecycle::claims` keeps a
    /// watcher that has been promoted to a provider-native TUI transcript from
    /// being demoted back to the prelaunch wrapper file; the row carries no
    /// such guard, so the two can name different files for one channel.
    ///
    /// `None` means no live handle was keyed under `tmux_session_name` at the
    /// instant of the read, NOT that the watcher has no output path — a live
    /// handle always has one. The mutating helpers keep the channel index and
    /// the handle map in step under the registry mutex, but `channel_binding`
    /// takes no lock, so its two reads can straddle a concurrent mutation.
    pub(in crate::services::discord) output_path: Option<String>,
}

#[rustfmt::skip]
impl TmuxWatcherRegistry {
    pub(in crate::services::discord) fn new() -> Self {
        Self {
            by_tmux_session: dashmap::DashMap::new(),
            tmux_session_by_channel: dashmap::DashMap::new(),
            owner_channel_by_tmux_session: dashmap::DashMap::new(),
            restored_owner_by_tmux_session: dashmap::DashMap::new(),
        }
    }

    pub(in crate::services::discord) fn len(&self) -> usize {
        self.by_tmux_session.len()
    }

    pub(in crate::services::discord) fn contains_key(&self, channel_id: &ChannelId) -> bool {
        self.channel_binding(channel_id)
            .and_then(|binding| self.by_tmux_session.get(&binding.tmux_session_name))
            .is_some()
    }

    pub(in crate::services::discord) fn get(
        &self,
        channel_id: &ChannelId,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, TmuxWatcherHandle>> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        self.by_tmux_session.get(&tmux_session_name)
    }

    // #3034: test-only convenience wrapper (prod code calls `insert_locked`
    // with an explicit registry guard). Used only by `#[cfg(test)]` setup.
    #[allow(dead_code)]
    pub(in crate::services::discord) fn insert(
        &self,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        let guard = lock_tmux_watcher_registry();
        self.insert_locked(&guard, channel_id, handle)
    }

    pub(in crate::services::discord) fn insert_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: ChannelId,
        handle: TmuxWatcherHandle,
    ) -> Option<TmuxWatcherHandle> {
        if let Some((_, old_tmux_session_name)) = self.tmux_session_by_channel.remove(&channel_id) {
            self.owner_channel_by_tmux_session
                .remove(&old_tmux_session_name);
            self.by_tmux_session.remove(&old_tmux_session_name);
        }

        let tmux_session_name = handle.tmux_session_name.clone();
        if let Some((_, old_owner_channel_id)) = self
            .owner_channel_by_tmux_session
            .remove(&tmux_session_name)
        {
            self.tmux_session_by_channel.remove(&old_owner_channel_id);
        }

        // #3105: a live watcher handle is now the authoritative owner for this
        // session — drop any restored owner-only binding so it can never shadow
        // or contradict live truth.
        self.restored_owner_by_tmux_session
            .remove(&tmux_session_name);

        self.tmux_session_by_channel
            .insert(channel_id, tmux_session_name.clone());
        self.owner_channel_by_tmux_session
            .insert(tmux_session_name.clone(), channel_id);
        self.by_tmux_session.insert(tmux_session_name, handle)
    }

    pub(in crate::services::discord) fn remove(&self, channel_id: &ChannelId) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_locked(&guard, channel_id)
    }

    pub(in crate::services::discord) fn remove_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        channel_id: &ChannelId,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, tmux_session_name) = self.tmux_session_by_channel.remove(channel_id)?;
        self.owner_channel_by_tmux_session
            .remove(&tmux_session_name);
        self.by_tmux_session
            .remove(&tmux_session_name)
            .map(|(_, handle)| (*channel_id, handle))
    }

    pub(in crate::services::discord) fn remove_tmux_session_locked(
        &self,
        _guard: &TmuxWatcherRegistryGuard,
        tmux_session_name: &str,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let (_, owner_channel_id) = self
            .owner_channel_by_tmux_session
            .remove(tmux_session_name)?;
        self.tmux_session_by_channel.remove(&owner_channel_id);
        self.by_tmux_session
            .remove(tmux_session_name)
            .map(|(_, handle)| (owner_channel_id, handle))
    }

    pub(in crate::services::discord) fn remove_tmux_session_if_current(
        &self,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let guard = lock_tmux_watcher_registry();
        self.remove_tmux_session_if_current_locked(
            &guard,
            tmux_session_name,
            expected_cancel,
            None,
            None,
        )
    }

    /// Shared core for the fenced and unfenced spellings. `identity` is the
    /// #5071 T3-A1 conjunct and `delivery` the #5071 relay-tail S4 (I-1) one:
    /// both are evaluated INSIDE the registry lock and only after the
    /// cancel-pointer CAS already matched, so a fence miss and a pointer miss
    /// produce the same answer — nothing removed, nothing stored.
    ///
    /// The unfenced spelling compares the session key and the cancel pointer
    /// only. The live owner channel and output path read here complete T3-R2's
    /// tuple for a fence that pinned them; without a fence they are read and
    /// discarded, leaving the unfenced answer unchanged. Every identity conjunct
    /// is a value comparison — see [`WatcherIdentityFence`] for what that does
    /// and does not establish, and [`TerminalDeliveryFence`] for the separate
    /// in-flight-delivery question it answers.
    fn remove_tmux_session_if_current_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        tmux_session_name: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
        identity: Option<&WatcherIdentityFence>,
        delivery: Option<&TerminalDeliveryFence>,
    ) -> Option<(ChannelId, TmuxWatcherHandle)> {
        let live_output_path = self
            .by_tmux_session
            .get(tmux_session_name)
            .filter(|entry| Arc::ptr_eq(&entry.cancel, expected_cancel))
            .map(|entry| entry.output_path.clone())?;
        if let Some(identity) = identity
            && !identity.permits_destruction(
                self.owner_channel_for_tmux_session(tmux_session_name),
                &live_output_path,
            )
        {
            return None;
        }
        // #5071 relay-tail S4 r2 (P1-1): the removal runs INSIDE the delivery
        // conjunct's critical section, not after a bool it returned.
        commit_under_delivery_fence(delivery, || {
            self.remove_tmux_session_locked(guard, tmux_session_name)
        })
        .flatten()
    }

    pub(in crate::services::discord) fn cancel_and_remove_channel_if_current(
        &self,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let guard = lock_tmux_watcher_registry();
        self.cancel_and_remove_channel_if_current_locked(
            &guard,
            channel_id,
            expected_tmux_session_name,
            expected_output_path,
            expected_cancel,
            None,
            None,
        )
    }

    /// Shared core for the fenced and unfenced spellings. The `cancel` store
    /// happens only after every conjunct — channel binding, session name,
    /// output path, cancel pointer, the optional #5071 T3-A1 identity re-read,
    /// and the optional #5071 relay-tail S4 delivery-lease re-read — has
    /// compared equal and the entry has been removed, so a `false` return leaves
    /// the watcher both registered and uncancelled. Equality of those values is
    /// the whole identity guarantee; see [`WatcherIdentityFence`] and
    /// [`TerminalDeliveryFence`].
    fn cancel_and_remove_channel_if_current_locked(
        &self,
        guard: &TmuxWatcherRegistryGuard,
        channel_id: &ChannelId,
        expected_tmux_session_name: &str,
        expected_output_path: &str,
        expected_cancel: &Arc<std::sync::atomic::AtomicBool>,
        identity: Option<&WatcherIdentityFence>,
        delivery: Option<&TerminalDeliveryFence>,
    ) -> bool {
        let Some(tmux_session_name) = self
            .tmux_session_by_channel
            .get(channel_id)
            .map(|entry| entry.clone())
        else {
            return false;
        };
        if tmux_session_name != expected_tmux_session_name {
            return false;
        }
        let matches_current = self
            .by_tmux_session
            .get(&tmux_session_name)
            .is_some_and(|entry| {
                entry.output_path == expected_output_path
                    && Arc::ptr_eq(&entry.cancel, expected_cancel)
            });
        if !matches_current {
            return false;
        }
        if let Some(identity) = identity
            && !identity.permits_destruction(
                self.owner_channel_for_tmux_session(&tmux_session_name),
                expected_output_path,
            )
        {
            return false;
        }
        // #5071 relay-tail S4 r2 (P1-1): the removal AND the `cancel` store run
        // INSIDE the delivery conjunct's critical section, not after a bool it
        // returned — the whole destruction is what has to be atomic against a
        // racing acquire, not just the map mutation.
        commit_under_delivery_fence(delivery, || {
            let Some((_, handle)) = self.remove_locked(guard, channel_id) else {
                return false;
            };
            handle
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
    }

    /// Bind an identity fence to the next CAS removal. The returned view is
    /// HALF-BUILT and has no destructive method: the caller must chain
    /// [`IdentityFencePendingDelivery::with_terminal_delivery_fence`] to add the
    /// S4 delivery-lease conjunct before it can remove anything. The completed
    /// view spells the two removal helpers with their original names so the
    /// destructive call-site ratchet keeps counting the caller.
    pub(in crate::services::discord) fn under_identity_fence(
        &self,
        fence: WatcherIdentityFence,
    ) -> IdentityFencePendingDelivery<'_> {
        IdentityFencePendingDelivery { registry: self, fence }
    }

    pub(in crate::services::discord) fn iter(&self) -> dashmap::iter::Iter<'_, String, TmuxWatcherHandle> {
        self.by_tmux_session.iter()
    }

    pub(in crate::services::discord) fn channel_binding(&self, channel_id: &ChannelId) -> Option<TmuxWatcherBinding> {
        let tmux_session_name = self.tmux_session_by_channel.get(channel_id)?.clone();
        let owner_channel_id = self
            .owner_channel_by_tmux_session
            .get(&tmux_session_name)
            .map(|entry| *entry.value())
            .unwrap_or(*channel_id);
        let output_path = self.watcher_output_path(&tmux_session_name);
        Some(TmuxWatcherBinding {
            owner_channel_id,
            tmux_session_name,
            output_path,
        })
    }

    pub(in crate::services::discord) fn owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ChannelId> {
        // The live watcher-handle binding is the primary authority. When no live
        // watcher owns the session (e.g. a TUI-direct session whose slot was
        // evicted by compact/restart/rebind), fall back to the #3105 restored
        // owner map — still authoritative (settings-derived), unlike the dedupe
        // mirror, which is never consulted here.
        self.owner_channel_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            .or_else(|| {
                self.restored_owner_by_tmux_session
                    .get(tmux_session_name)
                    .map(|entry| *entry.value())
            })
    }

    /// #3105: re-register the authoritative owner channel for a LIVE tmux
    /// session that currently has no live watcher handle.
    ///
    /// This is the self-heal path for the permanent relay drop described in
    /// #3105: the idle transcript relay resolves a session's owner channel
    /// deterministically from the configured channel→provider bindings (which
    /// handle both base and thread-suffixed tmux names) and promotes that
    /// evidence into the authoritative registry here, instead of routing from
    /// the `tui_prompt_dedupe` mirror (which #3018 forbids as a reverse
    /// authority).
    ///
    /// No-ops when a live watcher already owns the session (live truth wins) or
    /// when the binding is unchanged. Returns `true` only on the first/changed
    /// registration so callers can emit a single bounded incident instead of a
    /// per-poll log.
    pub(in crate::services::discord) fn restore_owner_channel_for_tmux_session(
        &self,
        tmux_session_name: &str,
        channel_id: ChannelId,
    ) -> bool {
        let _guard = lock_tmux_watcher_registry();
        if self
            .owner_channel_by_tmux_session
            .contains_key(tmux_session_name)
        {
            // A live watcher handle already owns this session authoritatively.
            self.restored_owner_by_tmux_session
                .remove(tmux_session_name);
            return false;
        }
        let changed = self
            .restored_owner_by_tmux_session
            .get(tmux_session_name)
            .map(|entry| *entry.value())
            != Some(channel_id);
        self.restored_owner_by_tmux_session
            .insert(tmux_session_name.to_string(), channel_id);
        changed
    }

    /// #3105: drop a restored owner-only binding (e.g. once the session is no
    /// longer live). Idempotent.
    pub(in crate::services::discord) fn clear_restored_owner_for_tmux_session(&self, tmux_session_name: &str) {
        self.restored_owner_by_tmux_session
            .remove(tmux_session_name);
    }

    /// Preserve authoritative routing for a live tmux pane after the channel's
    /// provider session is rebound. Existing watcher ownership already wins; an
    /// owner-only entry covers watcher teardown until pane death is confirmed.
    pub(in crate::services::discord) fn retain_owner_during_session_rebind(
        &self,
        tmux_session_name: &str,
        channel_id: ChannelId,
    ) -> bool {
        let _guard = lock_tmux_watcher_registry();
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return false;
        }
        if let Some(current_owner) = self.owner_channel_by_tmux_session.get(tmux_session_name)
            && *current_owner.value() != channel_id
        {
            return false;
        }
        self.restored_owner_by_tmux_session
            .insert(tmux_session_name.to_string(), channel_id);
        true
    }

    /// #3105 (codex P1 sub-case B): true when a LIVE watcher handle currently
    /// owns this tmux session. Used to distinguish a genuinely dead/orphaned
    /// session (no live watcher) from a live session whose authoritative owner
    /// map entry was transiently evicted (which must self-heal, not be tombstoned).
    pub(in crate::services::discord) fn has_live_watcher_handle(&self, tmux_session_name: &str) -> bool {
        self.by_tmux_session.contains_key(tmux_session_name)
    }

    pub(in crate::services::discord) fn tmux_session_is_stale(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.heartbeat_stale())
    }

    pub(in crate::services::discord) fn tmux_session_live_for_relay(&self, tmux_session_name: &str) -> Option<bool> {
        self.by_tmux_session.get(tmux_session_name).map(|entry| {
            !entry.cancel.load(std::sync::atomic::Ordering::Relaxed) && !entry.heartbeat_stale()
        })
    }

    /// #2843: the output path the live watcher (if any) is tailing for this tmux
    /// session. The Claude idle relay uses this to decide whether a non-stale
    /// watcher genuinely covers the freshest transcript (and thus already relays
    /// it). Comparing against the runtime *binding* is wrong: re-registering the
    /// binding does not retarget the already-running watcher, so the binding and
    /// the watcher can point at different files.
    pub(in crate::services::discord) fn watcher_output_path(&self, tmux_session_name: &str) -> Option<String> {
        self.by_tmux_session
            .get(tmux_session_name)
            .map(|entry| entry.output_path.clone())
    }
}
