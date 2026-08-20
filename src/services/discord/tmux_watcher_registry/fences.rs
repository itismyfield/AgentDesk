//! #5457: the #5071 relay-tail S4 destructive-fence conjuncts, lifted verbatim
//! out of the registry root so `tmux_watcher_registry.rs` sits back under the
//! 1000-line production ceiling. Behaviour-preserving: this module holds the two
//! conjunct types plus their rollout-mode and test-hook plumbing, while the
//! registry root keeps the maps, the CAS cores that consult these, and the
//! `IdentityFencedRegistry` view that binds them.
//!
//! Being a CHILD of the registry module is load-bearing. The CAS cores reach
//! `WatcherIdentityFence::permits_destruction` and
//! `commit_under_delivery_fence`, which are `pub(super)` — a descendant module
//! can see its ancestors' private items, so the seam needs no widening into the
//! wider `discord` tree. For the same reason the destructive call-site ratchet's
//! owner-file exclusion is untouched: the fenced view, both `*_if_current`
//! spellings and `under_identity_fence` all stay in the root it names.

use super::*;

use crate::config::ExecutionIdentityMode;

/// The `.spawn_nonce` read model lives under `super::super::tmux`, which is
/// `cfg(unix)`; this module is not.
#[cfg(unix)]
use super::super::tmux::execution_identity::{
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
/// A VALUE match, and only that. Under `Enforce` the two CAS cores that consume
/// this tuple refuse whenever any captured value stops equalling the live one,
/// which covers every shape the conjuncts were added for: a row whose owner
/// channel moved, a row whose output path moved, a replacement watcher carrying
/// a fresh cancel `Arc` (`Arc::ptr_eq` fails), a respawn that minted a new
/// `.spawn_nonce`, and a marker that is unreadable on either side. `Legacy`
/// keeps each call site's pre-A1 comparison verbatim and `Observe` only counts.
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
    /// pin it would have discarded — the answer the CAS core reaches through
    /// `permits_destruction` is unchanged.
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

    pub(super) fn permits_destruction(
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
    /// so `commit_if_permitted` reads the CURRENT state through it at CAS time,
    /// not a copy taken here.
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
pub(super) fn commit_under_delivery_fence<T>(
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
