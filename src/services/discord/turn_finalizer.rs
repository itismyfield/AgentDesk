//! EPIC #3016 — Single-authority `TurnFinalizer`.
//!
//! Today finalization is a distributed handshake: `mailbox_finish_turn`
//! (mod.rs) is called directly from ~17 sites, the bridge↔watcher handoff
//! uses a per-handle `mailbox_finalize_owed` CAS, and `global_active` is
//! decremented in ≥10 places. The races this caused (bridge-revoke vs
//! watcher-swap, and the silent gate-timeout SKIP that defers finalize to a
//! never-firing 1800s sweeper) are the root cause of the recurring "turn
//! never finalizes / inflight stuck" bug.
//!
//! This module introduces ONE actor that OWNS the side-effects of finalize as
//! an atomic, exactly-once unit:
//!   (1) inflight clear (honouring `PlannedRestartSkipped`),
//!   (2) mailbox cancel_token release via `mailbox_finish_turn`,
//!   (3) `global_active` decrement — gated on `removed_token.is_some()`,
//!   (4) the trailing terminal side-effects that today follow
//!       `mailbox_finish_turn` inline at each call-site (watchdog override
//!       clear, `dispatch_thread_parents` retain, voice barge-in drain,
//!       `dispatch_role_overrides` cleanup).
//!
//! Exactly-once is decided in a single place — the actor task's handling of a
//! `Terminal` message — via a per-turn ledger phase gate
//! (`Pending → Finalizing → Finalized`). Because the gate runs inside one task
//! there is no CAS deciding who finalizes. The legacy `mailbox_finalize_owed`
//! flag is still revoked at the bridge during the incremental window (it is
//! removed in Phase 5) so a stale watcher swap cannot route a terminal onto a
//! later turn, but it no longer arbitrates finalization — the ledger does.
//!
//! Landing is incremental (EPIC §4). Phase 1 ships this dormant: `do_finalize`
//! reproduces today's exact sequence and no call-site submits yet. Phases 2-3
//! rewire the bridge and watcher terminals to `submit(...)`. Because
//! `mailbox_finish_turn` is idempotent (returns `removed_token = None` on the
//! second call) and the counter decrement is gated on `removed_token.is_some()`,
//! a transitional double-finalize during the incremental window is a harmless
//! no-op — never an underflow, never a double Discord notice.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serenity::model::id::ChannelId;
// `tokio::time::Instant` (not `std::time::Instant`) so deadlines respect the
// paused/virtual test clock and the production `interval` clock alike.
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::provider::{CancelToken, ProviderKind};

use super::SharedData;

/// How often the reconciler `Tick` fires to re-check deadline-armed
/// gate-timeout entries and garbage-collect the ledger.
const RECONCILE_INTERVAL: Duration = Duration::from_millis(1000);

/// Bounded backstop for a gate-timeout whose pane is still busy and whose
/// relay owner is still alive. The reconciler finalizes once the pane
/// quiesces, the owner dies, OR this deadline elapses — seconds, NOT the
/// 1800s placeholder-sweeper horizon the old silent SKIP deferred to. The
/// hosted-TUI pre-submit busy guard remains the correctness floor that stops
/// follow-up input from being injected into a still-busy pane, so this only
/// guarantees finalize eventually fires.
const GATE_BACKSTOP: Duration = Duration::from_secs(8);

/// TTL after which a `Finalized` ledger entry is garbage-collected so the
/// ledger stays bounded while still suppressing a late double-submit.
const FINALIZED_TTL: Duration = Duration::from_secs(60);

/// Identity carried by a submission. The ledger key is the FULL identity
/// (`channel_id`, `generation`, `user_msg_id`) so two SEQUENTIAL turns in the
/// same channel are distinct entries — a finalized turn-1 must NOT swallow
/// turn-2's terminal as `AlreadyFinalized` (that would strand turn-2's mailbox
/// token and re-introduce the stuck-channel bug this EPIC fixes).
///
/// A terminal that only knows the channel (`user_msg_id == 0`, recovery/orphan
/// paths) is resolved separately: because the mailbox is channel-scoped with a
/// single active turn, there is at most one non-`Finalized` entry per
/// `(channel, generation)` at a time, and an id-0 terminal collapses onto THAT
/// live entry (see `resolve_ledger_key`) rather than keying on the literal 0.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct TurnKey {
    pub(in crate::services::discord) channel_id: ChannelId,
    /// 0 == "unknown identity" (recovery/orphan paths): resolved to the
    /// channel's single live entry instead of a literal-0 key.
    pub(in crate::services::discord) user_msg_id: u64,
    pub(in crate::services::discord) generation: u64,
}

impl TurnKey {
    pub(in crate::services::discord) fn new(
        channel_id: ChannelId,
        user_msg_id: u64,
        generation: u64,
    ) -> Self {
        Self {
            channel_id,
            user_msg_id,
            generation,
        }
    }

    /// The literal full-identity key for this turn. Shared with the
    /// `StatusPanelController` (#3078), which keys its panel ledger on the same
    /// `LedgerKey` so the panel and finalize ledgers collapse channel-only
    /// (`user_msg_id == 0`) recovery/orphan terminals onto the same live entry.
    pub(in crate::services::discord) fn exact_key(&self) -> LedgerKey {
        LedgerKey {
            channel_id: self.channel_id,
            generation: self.generation,
            user_msg_id: self.user_msg_id,
        }
    }
}

/// The exact ledger match: channel + restart generation + user message id.
/// Full identity so sequential same-channel turns never collide. Shared with
/// the `StatusPanelController` (#3078) so both ledgers key turns identically.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(in crate::services::discord) struct LedgerKey {
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) generation: u64,
    pub(in crate::services::discord) user_msg_id: u64,
}

/// Resolve a submission to the ledger entry it acts on.
///
/// - A real `user_msg_id` uses its exact key (the common case — the watcher and
///   bridge both pass the real id from inflight, so a stale terminal of an
///   already-finalized turn matches that turn's retained `Finalized` entry and
///   correctly returns `AlreadyFinalized`).
/// - A channel-only terminal (`user_msg_id == 0`, recovery/orphan path) collapses
///   onto the channel's single live (non-`Finalized`) entry for this generation
///   ONLY when no `Finalized` entry exists for the same channel/generation. The
///   guard is the cross-turn safety net: if a turn recently finalized, a
///   channel-only terminal is most likely a STALE terminal of THAT turn — not of
///   a queued follow-up that has since registered — so collapsing it onto the
///   new live entry would prematurely finalize the follow-up and release its
///   mailbox token. In that ambiguous case we route to the literal id-0 key
///   (an orphan no-op: idempotent `mailbox_finish_turn` returns `None`, so the
///   live turn is untouched). With no recent finalize, the single live entry is
///   unambiguously the turn this orphan terminal belongs to.
fn resolve_ledger_key(ledger: &HashMap<LedgerKey, LedgerEntry>, key: TurnKey) -> LedgerKey {
    resolve_channel_only(
        key,
        ledger
            .iter()
            .map(|(lk, entry)| (lk, entry.phase == Phase::Finalized)),
    )
}

/// Channel-only collapse over an explicit candidate set. The finalizer keeps
/// its in-line `HashMap` walk above; the `StatusPanelController` (#3078) passes
/// its own `(LedgerKey, is_terminal)` pairs here so the identical
/// finalized-guard / single-live-entry semantics apply to the panel ledger
/// without duplicating the subtle ambiguity rule.
///
/// - A real `user_msg_id` uses its exact key.
/// - A channel-only id collapses onto the single non-terminal entry ONLY when
///   no terminal entry exists for the same channel/generation (ambiguous
///   otherwise → route to the literal orphan key, a no-op for the caller).
pub(in crate::services::discord) fn resolve_channel_only<'a>(
    key: TurnKey,
    candidates: impl Iterator<Item = (&'a LedgerKey, bool)> + Clone,
) -> LedgerKey {
    if key.user_msg_id != 0 {
        return key.exact_key();
    }
    let channel_has_terminal = candidates.clone().any(|(lk, is_terminal)| {
        lk.channel_id == key.channel_id && lk.generation == key.generation && is_terminal
    });
    if channel_has_terminal {
        return key.exact_key();
    }
    candidates
        .into_iter()
        .find(|(lk, is_terminal)| {
            lk.channel_id == key.channel_id && lk.generation == key.generation && !*is_terminal
        })
        .map(|(lk, _)| *lk)
        .unwrap_or_else(|| key.exact_key())
}

/// Every actor submits ONE of these terminal events. The finalizer's ledger
/// gate decides exactly-once. Phase 1 wires the variants the bridge/watcher
/// terminals need (Phases 2-3); the remaining sweeper/recovery variants land
/// in Phase 4 and are listed here so the matrix is explicit.
#[derive(Clone, Debug)]
pub(in crate::services::discord) enum TerminalEvent {
    /// Normal completion — bridge or watcher relayed (or intentionally
    /// suppressed) terminal output and the turn is done.
    Complete,
    /// `/!stop`, reaction, watchdog. `mark_completion_cleanup()` is skipped
    /// (the cancel is a real mid-stream stop) but `cancelled.store(true)`
    /// still fires.
    Cancel,
    /// #2293/#2780 hosted-TUI quiescence gate. `pane_quiescent == Some(false)`
    /// with a still-alive relay owner is the ONLY event that defers: it arms a
    /// bounded deadline and the reconciler finalizes when the precondition
    /// clears or the deadline elapses. Replaces the silent watcher SKIP.
    GateTimeout { pane_quiescent: Option<bool> },
    /// No output owner and an empty response — finalize once and emit the
    /// observability event from inside the finalizer.
    RelayMiss,
}

impl TerminalEvent {
    fn is_cancel(&self) -> bool {
        matches!(self, TerminalEvent::Cancel)
    }
}

/// Per-submission knobs that keep each routed call-site behaviourally
/// identical to its pre-#3016 inline sequence during the incremental window.
/// Each routed site maps exactly to the side-effects its inline code ran, so
/// rewiring it through `do_finalize` is observably a no-op except for which
/// code path issues the (identical) finalize.
#[derive(Clone, Copy, Debug)]
pub(in crate::services::discord) struct FinalizeContext {
    /// Whether `do_finalize` clears inflight as part of the finalize. Bridge
    /// branches and the watcher clear inflight inline in their own flow before
    /// submitting, so they pass `false`; only the deadline-armed reconcile
    /// backstop (no caller to clear it) passes `true`.
    pub(in crate::services::discord) clear_inflight: bool,
    /// Whether to mark the removed token's completion-cleanup. The bridge did
    /// this on a non-cancel terminal (still gated on `!event.is_cancel()`); the
    /// watcher's `finish_restored_watcher_active_turn` did NOT — it only set
    /// `cancelled`. Keeping this per-site avoids changing provider-watchdog
    /// semantics on the watcher path.
    pub(in crate::services::discord) allow_completion_cleanup: bool,
    /// Whether to drain voice barge-in deferred prompts as part of finalize.
    /// The bridge branches drain voice; the watcher path did NOT.
    pub(in crate::services::discord) drain_voice: bool,
    /// Whether to schedule a deferred idle-queue kickoff when the finalize
    /// leaves a pending soft-queue (gated on `mailbox_online && has_pending`).
    /// The watcher's `finish_restored_watcher_active_turn` did this; the bridge
    /// branches deferred kickoff to a later site, so they pass `false`.
    pub(in crate::services::discord) kickoff_queue: bool,
}

impl FinalizeContext {
    /// Bridge non-delegation / missing-handoff branches: bridge owns the
    /// inflight clear elsewhere, marks completion-cleanup on non-cancel, drains
    /// voice, defers queue kickoff.
    pub(in crate::services::discord) fn bridge() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: true,
            drain_voice: true,
            kickoff_queue: false,
        }
    }

    /// Watcher terminal via `finish_restored_watcher_active_turn`: the watcher
    /// clears inflight inline before submitting, does NOT mark completion
    /// cleanup, does NOT drain voice. The queue kickoff stays at the caller
    /// because it is gated on the caller's `dispatch_ok`, which the finalizer
    /// cannot see — so the context leaves kickoff to the submitter.
    pub(in crate::services::discord) fn watcher() -> Self {
        Self {
            clear_inflight: false,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: false,
        }
    }

    /// Deadline-armed gate-timeout backstop, fired from the reconciler with no
    /// caller to have cleared inflight: finalize fully (clear inflight here),
    /// no completion-cleanup or voice drain (watcher semantics), kick off the
    /// queue if backlog remains.
    fn gate_backstop() -> Self {
        Self {
            clear_inflight: true,
            allow_completion_cleanup: false,
            drain_voice: false,
            kickoff_queue: true,
        }
    }
}

/// Outcome of a terminal submission. The submitter uses this to decide whether
/// it owns the post-finalize bookkeeping (it performed the one finalize) or
/// must do nothing (someone else already finalized this turn).
pub(in crate::services::discord) enum FinalizeOutcome {
    /// This submission performed the one finalize.
    Finalized {
        removed_token: Option<Arc<CancelToken>>,
        has_pending: bool,
        mailbox_online: bool,
    },
    /// Another actor already finalized this turn; the submitter must do
    /// nothing (no counter touch, no Discord notice, no re-relay).
    AlreadyFinalized,
    /// Gate-timeout with a still-busy pane and a live relay owner — recorded
    /// with a bounded deadline; the reconciler will finalize when the
    /// precondition clears.
    Deferred,
}

/// Ledger phase for a single turn. Owned solely by the actor task; the
/// check-and-set on this enum is the one place exactly-once is decided.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Pending,
    Finalizing,
    Finalized,
}

struct LedgerEntry {
    phase: Phase,
    relay_owner: RelayOwnerKind,
    provider: ProviderKind,
    /// The originating identity, retained so the deadline-armed reconciler can
    /// reconstruct a `TurnKey` for `do_finalize` (channel-scoped, so any of the
    /// colliding ids is equivalent).
    turn_key: TurnKey,
    /// Backstop deadline for a deferred gate-timeout. `None` unless a
    /// `GateTimeout{Some(false)}` armed it.
    terminal_deadline: Option<Instant>,
    /// When the entry reached `Finalized`, for TTL-based GC.
    finalized_at: Option<Instant>,
}

/// Messages the actor task drains. Each carries `Arc<SharedData>` so the
/// finalize side-effects can run inside the task without the finalizer holding
/// a `Weak<SharedData>` back-reference (which would re-introduce an Arc cycle
/// and ordering ambiguity).
enum FinalizeMsg {
    /// #3018 register: the bridge submits this synchronously at intake/handoff
    /// before the watcher can submit a terminal, so the ledger knows the turn
    /// exists and message arrival order replaces the deleted Release/AcqRel
    /// `mailbox_finalize_owed.store` ordering.
    Start {
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
    },
    Terminal {
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<FinalizeOutcome>,
    },
}

/// A per-runtime actor, held as `Arc<TurnFinalizer>` on `SharedData`. One
/// owning task drains the `mpsc`; all public methods are cheap
/// submit-or-await wrappers.
pub(in crate::services::discord) struct TurnFinalizer {
    tx: mpsc::UnboundedSender<FinalizeMsg>,
}

impl TurnFinalizer {
    /// Spawn the owning actor task and return the handle. Cheap and idle until
    /// the first submission, so it is safe to construct unconditionally at
    /// every `SharedData` construction site (incl. tests).
    ///
    /// The actor task is only spawned when a Tokio runtime handle is available.
    /// Some synchronous unit tests build `SharedData` outside any runtime via
    /// `make_shared_data_for_tests` and never finalize a turn; there `spawn`
    /// must not panic (`tokio::spawn` requires a reactor). When no runtime is
    /// present we skip the task — the unbounded sender simply buffers the
    /// (never-sent, in practice) messages, so the dormant finalizer stays inert
    /// instead of crashing the test process.
    pub(in crate::services::discord) fn spawn() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(actor_loop(rx));
        }
        Arc::new(Self { tx })
    }

    /// #3018 — register a turn so the ledger knows it exists before any
    /// terminal can arrive. Idempotent: a second `Start` for a key already in
    /// the ledger only refreshes the relay owner.
    pub(in crate::services::discord) fn register_start(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
    ) {
        // UnboundedSender::send only fails if the actor task is gone (process
        // teardown); dropping the Start there is harmless because no terminal
        // will be awaited either.
        let _ = self.tx.send(FinalizeMsg::Start {
            key,
            provider,
            relay_owner,
        });
    }

    /// Submit a terminal event and await the exactly-once decision. Returns
    /// `AlreadyFinalized` if another actor already finalized this turn.
    pub(in crate::services::discord) async fn submit_terminal(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        shared: Arc<SharedData>,
    ) -> FinalizeOutcome {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(FinalizeMsg::Terminal {
                key,
                provider,
                event,
                ctx,
                shared,
                ack,
            })
            .is_err()
        {
            // Actor task gone (teardown). Treat as already-finalized so the
            // submitter does no further bookkeeping.
            return FinalizeOutcome::AlreadyFinalized;
        }
        rx.await.unwrap_or(FinalizeOutcome::AlreadyFinalized)
    }
}

/// The single owning task. Owns the ledger and a NON-owning cached handle to
/// `SharedData`, and drives the reconcile timer in the same task via `select!`
/// so the deadline-armed gate-timeout finalize fires deterministically (no
/// cross-task nudge ordering to reason about).
async fn actor_loop(mut rx: mpsc::UnboundedReceiver<FinalizeMsg>) {
    let mut ledger: HashMap<LedgerKey, LedgerEntry> = HashMap::new();
    // A `Weak` (NOT `Arc`) so the actor never keeps `SharedData` alive: the
    // cycle would otherwise be SharedData → Arc<TurnFinalizer> → sender →
    // actor → cached Arc<SharedData>, leaking the whole runtime across a
    // provider restart. The reconcile pass upgrades on demand; if the upgrade
    // fails the runtime is shutting down and there is nothing left to finalize.
    let mut cached_shared: Option<std::sync::Weak<SharedData>> = None;
    let mut reconcile_timer = tokio::time::interval(RECONCILE_INTERVAL);
    reconcile_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            maybe_msg = rx.recv() => {
                let Some(msg) = maybe_msg else { break };
                match msg {
                    FinalizeMsg::Start {
                        key,
                        provider,
                        relay_owner,
                    } => {
                        // A `Start` always carries the real `user_msg_id`, so it
                        // registers under the exact full-identity key.
                        ledger
                            .entry(key.exact_key())
                            .and_modify(|e| {
                                // Only refresh the owner while still live; never
                                // resurrect a finalized turn.
                                if e.phase != Phase::Finalized {
                                    e.relay_owner = relay_owner;
                                    e.provider = provider.clone();
                                    e.turn_key = key;
                                }
                            })
                            .or_insert(LedgerEntry {
                                phase: Phase::Pending,
                                relay_owner,
                                provider,
                                turn_key: key,
                                terminal_deadline: None,
                                finalized_at: None,
                            });
                    }
                    FinalizeMsg::Terminal {
                        key,
                        provider,
                        event,
                        ctx,
                        shared,
                        ack,
                    } => {
                        cached_shared = Some(Arc::downgrade(&shared));
                        let outcome =
                            handle_terminal(&mut ledger, key, provider, event, ctx, &shared)
                                .await;
                        let _ = ack.send(outcome);
                    }
                }
            }
            _ = reconcile_timer.tick() => {
                if let Some(shared) = cached_shared.as_ref().and_then(std::sync::Weak::upgrade) {
                    reconcile(&mut ledger, &shared).await;
                }
            }
        }
    }
}

/// Exactly-once gate + immediate-finalize / deferral decision. Runs inside the
/// actor task, so the check-and-set on `Phase` needs no synchronization.
async fn handle_terminal(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
    provider: ProviderKind,
    event: TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    // Resolve to the entry this terminal acts on. A real id uses its exact
    // key; a channel-only id-0 terminal collapses onto the channel's single
    // live entry (recovery/orphan path). A terminal for an unregistered turn is
    // the orphan path (post-restart inflight on disk, no live `Start`); we
    // still finalize it driven by the inflight identity — idempotent
    // `mailbox_finish_turn` returns `removed_token = None` so the counter is
    // untouched and inflight is cleared exactly once.
    let ledger_key = resolve_ledger_key(ledger, key);

    // Codex P1 — ambiguous channel-only terminal. A `user_msg_id == 0`
    // submission whose resolver fell back to the literal orphan key (because a
    // recently-`Finalized` entry exists) is most likely a STALE terminal of
    // that finalized turn. If a DIFFERENT live entry exists for the channel
    // (a queued follow-up that has since registered), running the
    // channel-scoped `mailbox_finish_turn` here would release the follow-up's
    // token and decrement `global_active` for a turn this terminal does not
    // own. Treat it as a no-op so it cannot corrupt the next active turn. (A
    // genuine orphan with NO live entry still finalizes below — its
    // `mailbox_finish_turn` is harmless: idempotent + counter-gated.)
    if key.user_msg_id == 0 && !ledger.contains_key(&ledger_key) {
        let channel_has_live_turn = ledger.iter().any(|(lk, e)| {
            lk.channel_id == key.channel_id
                && lk.generation == key.generation
                && e.phase != Phase::Finalized
        });
        if channel_has_live_turn {
            return FinalizeOutcome::AlreadyFinalized;
        }
    }

    let entry = ledger.entry(ledger_key).or_insert(LedgerEntry {
        phase: Phase::Pending,
        relay_owner: RelayOwnerKind::None,
        provider,
        turn_key: key,
        terminal_deadline: None,
        finalized_at: None,
    });

    match entry.phase {
        Phase::Finalizing | Phase::Finalized => {
            return FinalizeOutcome::AlreadyFinalized;
        }
        Phase::Pending => {}
    }

    // Gate-timeout with a still-busy pane AND a live relay owner is the only
    // event that defers instead of finalizing now.
    let mut effective_ctx = ctx;
    if let TerminalEvent::GateTimeout {
        pane_quiescent: Some(false),
    } = event
    {
        if entry.relay_owner != RelayOwnerKind::None {
            // Arm the backstop deadline ONCE. The watcher submits a
            // GateTimeout on every pass while the pane stays busy; if each
            // submission pushed the deadline forward by GATE_BACKSTOP the
            // backstop would never fire on a persistently busy pane (exactly
            // the never-finalizing bug). So only set it if not already armed.
            entry
                .terminal_deadline
                .get_or_insert_with(|| Instant::now() + GATE_BACKSTOP);
            return FinalizeOutcome::Deferred;
        }
        // No live relay owner → nothing will drive the pane to quiescence, so
        // finalize now rather than wait out the backstop. This is the
        // recovered/orphan watcher case (post-restart inflight on disk, never
        // registered via the bridge handoff `register_start`) where there is no
        // later watcher block to clear inflight or kick off the queue — the
        // caller submits with `FinalizeContext::watcher()` while the
        // `!lifecycle_stage_paused` cleanup block is SKIPPED and discards this
        // outcome. So this immediate path MUST reproduce that skipped cleanup
        // itself, exactly as the deadline-armed `gate_backstop()` would have:
        //   * clear inflight here (the watcher never cleared it inline), so the
        //     mailbox is not released while the inflight file keeps blocking
        //     the channel; and
        //   * kick off any queued soft-queue backlog, so a follow-up message
        //     queued behind the restored turn actually dispatches instead of
        //     staying stuck (regression of the EPIC's restart/#3011 goal).
        // Without the kickoff the restored channel clears inflight but never
        // drains its pending follow-up, leaving it effectively stuck.
        effective_ctx.clear_inflight = true;
        effective_ctx.kickoff_queue = true;
    }

    // Flip Pending → Finalizing, run the side-effects, flip → Finalized.
    entry.phase = Phase::Finalizing;
    let provider = entry.provider.clone();
    // Codex P1 — finalize on the RESOLVED identity. An id-0 watcher/recovery
    // terminal that collapsed onto a ledger entry registered (via
    // `register_start`) with the real `user_msg_id` must finalize under THAT
    // identity so `do_finalize` takes the guarded `finish_turn_if_matches` /
    // `clear_inflight_state_if_matches` path. Otherwise the id-0 key would force
    // the unguarded channel-scoped finish and could release a newer turn's token
    // when the ledger entry is stale. When the submission already carries a real
    // id, or the entry has no better identity (still id-0), this is the same key.
    let finalize_key = if key.user_msg_id == 0 && entry.turn_key.user_msg_id != 0 {
        entry.turn_key
    } else {
        key
    };
    let outcome = do_finalize(finalize_key, provider, &event, effective_ctx, shared).await;
    if let Some(entry) = ledger.get_mut(&ledger_key) {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.terminal_deadline = None;
    }
    outcome
}

/// The single owner of finalize's side-effects. Reproduces today's exact
/// `clear_inflight (per-site) + mailbox_finish_turn + counter-- + trailing
/// terminal side-effects` sequence so each routed call-site stays
/// behaviourally identical during the incremental landing.
async fn do_finalize(
    key: TurnKey,
    provider: ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) -> FinalizeOutcome {
    let channel_id = key.channel_id;

    // (A) inflight clear. Only the deadline-armed gate-timeout backstop and the
    //     immediate no-owner restored-watcher path set `clear_inflight` (every
    //     live-caller bridge/watcher site clears inflight inline and passes
    //     `false`). Those two paths CONSOLIDATE the pre-#3016 1800s placeholder
    //     sweeper, which was IDENTITY-GUARDED: it re-checked the on-disk
    //     `user_msg_id` still named the same turn and refused to delete a
    //     different (newer) turn's inflight, and never wiped a planned-restart /
    //     rebind-origin marker. So when this finalize carries a real identity
    //     (`user_msg_id != 0`) we reproduce that guard via
    //     `clear_inflight_state_if_matches` — finalize NEVER deletes a newer
    //     turn's inflight and preserves `PlannedRestartSkipped` /
    //     `RebindOriginSkipped`. A true orphan (`user_msg_id == 0`, no identity
    //     to authenticate against) falls back to the unguarded clear, exactly as
    //     the orphan paths always had to.
    if ctx.clear_inflight {
        if key.user_msg_id != 0 {
            let _ = super::inflight::clear_inflight_state_if_matches(
                &provider,
                channel_id.get(),
                key.user_msg_id,
            );
        } else {
            super::inflight::clear_inflight_state(&provider, channel_id.get());
        }
    }

    // (B) mailbox cancel_token release — the routed sites' single
    //     `mailbox_finish_turn`. Idempotent: returns `removed_token = None` on
    //     a second call, which is what makes a transitional double-finalize a
    //     no-op during Phases 2-3.
    //
    //     #3016 root-cause: when the terminal carries a real identity, use the
    //     IDENTITY-GUARDED finish so finalize only releases the token of the
    //     turn it actually owns. This closes the wrong-turn race where a stale
    //     channel-scoped terminal arriving after this turn finalized but before
    //     the next turn registered (or after ledger GC) would otherwise release
    //     the NEWER turn's token and decrement `global_active`. An ambiguous
    //     id-0 (recovery/orphan) terminal keeps the channel-scoped finish — the
    //     ledger gate + id-0 no-op guard already bound those.
    let finish = if key.user_msg_id != 0 {
        super::mailbox_finish_turn_if_matches(
            shared,
            &provider,
            channel_id,
            serenity::model::id::MessageId::new(key.user_msg_id),
        )
        .await
    } else {
        super::mailbox_finish_turn(shared, &provider, channel_id).await
    };

    if let Some(token) = finish.removed_token.as_ref() {
        // A normal completion releases lingering token observers via
        // `mark_completion_cleanup` so provider watchdogs don't treat the
        // post-terminal `cancelled` flip as a live mid-stream cancel. A real
        // cancel must NOT mark completion-cleanup; nor does the watcher path
        // (it historically only set `cancelled`).
        if ctx.allow_completion_cleanup && !event.is_cancel() {
            token.mark_completion_cleanup();
        }
        // Stop any lingering watchdog timer from firing on a newer turn's
        // token.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // (C) #3019 active-counter — decrement ONLY here, ONLY when this submission
    //     actually removed the active turn. Gating on `removed_token.is_some()`
    //     is what guarantees no underflow even under a transitional
    //     double-call.
    if finish.removed_token.is_some() {
        super::saturating_decrement_global_active(shared);
    }

    // The CHANNEL-SCOPED trailing side-effects (D)/(E) below mutate per-channel
    // routing/watchdog state that belongs to whatever turn is CURRENTLY active
    // in the channel. They are safe to run when this finalize actually finished
    // the turn (`removed_token.is_some()`), and harmlessly idempotent on the
    // legacy unguarded id-0 path (which always ran them). But when the
    // IDENTITY-GUARDED finish MISSED — a real `user_msg_id` that did NOT match
    // the live active turn, so `removed_token` is `None` — a DIFFERENT (newer)
    // turn owns the channel. Running these would clear the newer turn's
    // watchdog override, drop its `dispatch_thread_parents` / `dispatch_role_
    // overrides`, and drain its voice deferrals, corrupting a turn this stale
    // terminal does not own (Codex P2). So when the guard was used and missed,
    // skip the channel cleanup entirely — exactly as we already skip the token
    // release and counter decrement. (An id-0 orphan keeps today's behaviour.)
    let guarded_finish_missed = key.user_msg_id != 0 && finish.removed_token.is_none();

    let has_pending_after_voice = if guarded_finish_missed {
        // No-op finalize on a stale terminal: leave the live newer turn's
        // channel state untouched. Report NO backlog (Codex P2): the newer turn
        // is still active and owns its queue. Surfacing `finish.has_pending`
        // here would let the bridge propagate `has_queued_turns` and later drain
        // a queued soft message behind the live turn — concurrently dispatching
        // a follow-up this stale terminal does not own. A guarded miss is a true
        // no-op: no queue kickoff, no backlog reporting.
        false
    } else {
        // (D) trailing terminal side-effects that today follow
        //     `mailbox_finish_turn` inline at the bridge/watcher call-sites.
        //     Moved here so they cannot diverge between the routed paths.
        super::clear_watchdog_deadline_override(channel_id.get()).await;
        shared
            .dispatch_thread_parents
            .retain(|_, thread| *thread != channel_id);

        let voice_deferred_enqueued = if ctx.drain_voice {
            shared
                .voice_barge_in
                .drain_deferred_after_turn(shared, &provider, channel_id)
                .await
        } else {
            false
        };
        let has_pending_after_voice = finish.has_pending || voice_deferred_enqueued;
        if !has_pending_after_voice {
            shared.dispatch_role_overrides.remove(&channel_id);
        }

        // (E) optional deferred queue kickoff (watcher path), gated exactly as
        //     `finish_restored_watcher_active_turn` did.
        if ctx.kickoff_queue && finish.mailbox_online && has_pending_after_voice {
            // #3005: idle has just been confirmed on this finalize, so let the
            // first kickoff attempt run immediately (skipping the 2s pre-sleep)
            // instead of waiting the full deferred-drain INITIAL_DELAY before a
            // queued follow-up can start. Subsequent retries keep the existing
            // 2s cadence (e.g. if the hosted TUI is still transiently Busy).
            super::schedule_deferred_idle_queue_kickoff_immediate(
                shared.clone(),
                provider.clone(),
                channel_id,
                "turn_finalizer terminal completion with queued backlog",
            );
        }
        has_pending_after_voice
    };

    // (F) relay-miss observability — emitted from inside the finalizer so the
    //     signal fires exactly once per finalize regardless of submitter.
    if matches!(event, TerminalEvent::RelayMiss) {
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            None,
            None,
            None,
            "relay_miss_finalized",
            serde_json::json!({
                "removed_token": finish.removed_token.is_some(),
                "has_pending": has_pending_after_voice,
            }),
        );
    }

    FinalizeOutcome::Finalized {
        removed_token: finish.removed_token,
        has_pending: has_pending_after_voice,
        mailbox_online: finish.mailbox_online,
    }
}

/// The one reconciler. Finalizes deadline-armed gate-timeouts whose backstop
/// elapsed (the deadline-armed path; pane-quiescence/owner-death re-checks land
/// with the watcher rewire in Phase 3 and Phase 4 respectively) and
/// garbage-collects `Finalized` entries past their TTL so the ledger stays
/// bounded.
async fn reconcile(ledger: &mut HashMap<LedgerKey, LedgerEntry>, shared: &Arc<SharedData>) {
    let now = Instant::now();

    // Collect deadline-elapsed gate-timeout entries to finalize. We must not
    // hold a `&mut` borrow across the `do_finalize` await, so snapshot first.
    // The stored `turn_key` carries the identity `do_finalize` needs.
    let due: Vec<(LedgerKey, TurnKey, ProviderKind)> = ledger
        .iter()
        .filter_map(|(ledger_key, entry)| {
            if entry.phase == Phase::Pending
                && let Some(deadline) = entry.terminal_deadline
                && now >= deadline
            {
                Some((*ledger_key, entry.turn_key, entry.provider.clone()))
            } else {
                None
            }
        })
        .collect();

    for (ledger_key, turn_key, provider) in due {
        if let Some(entry) = ledger.get_mut(&ledger_key) {
            if entry.phase != Phase::Pending {
                continue;
            }
            entry.phase = Phase::Finalizing;
        }
        // Backstop finalize: the deferred gate-timeout originated from the
        // watcher terminal but no caller is around to clear inflight, so the
        // backstop context clears it here.
        let _ = do_finalize(
            turn_key,
            provider,
            &TerminalEvent::GateTimeout {
                pane_quiescent: Some(true),
            },
            FinalizeContext::gate_backstop(),
            shared,
        )
        .await;
        // Codex P1 — the watcher skipped its cleanup block (lifecycle paused),
        // so its legacy `mailbox_finalize_owed` flag is still `true`. The
        // backstop just released the mailbox/inflight, so revoke that debt now;
        // otherwise the watcher could later `swap(true)` and run stale cleanup
        // against the NEXT active turn. (Flag removed wholesale in phase 5.)
        if let Some(watcher) = shared.tmux_watchers.get(&turn_key.channel_id) {
            watcher
                .mailbox_finalize_owed
                .store(false, std::sync::atomic::Ordering::Release);
        }
        if let Some(entry) = ledger.get_mut(&ledger_key) {
            entry.phase = Phase::Finalized;
            entry.finalized_at = Some(now);
            entry.terminal_deadline = None;
        }
    }

    // GC finalized entries past their TTL.
    ledger.retain(|_, entry| {
        !(entry.phase == Phase::Finalized
            && entry
                .finalized_at
                .is_some_and(|t| now.duration_since(t) >= FINALIZED_TTL))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn key(ch: u64) -> TurnKey {
        TurnKey::new(ChannelId::new(ch), 1, 0)
    }

    /// Isolate a finalizer test from the process-global `AGENTDESK_ROOT_DIR`
    /// runtime root. The finalize path reads that env var (via
    /// `inflight::clear_inflight_state_if_matches` → `runtime_root()`) and can
    /// delete inflight files under whatever root is currently set, so these
    /// tests must serialize against the standby/watcher tests that also mutate
    /// the env. Acquire the SAME shared mutex the existing helpers use
    /// (`standby_relay::with_isolated_runtime_root`,
    /// `gateway::with_isolated_runtime_root`, `runtime_store::lock_test_env`,
    /// all of which resolve to `config::shared_test_env_lock()`) for the FULL
    /// duration of the test and point `AGENTDESK_ROOT_DIR` at a private temp
    /// dir, then clear it on exit so nothing leaks to a concurrent test.
    ///
    /// The guard is a `std::sync::MutexGuard` (not `Send`), held across the
    /// test's `.await` points. That is sound because every finalizer test runs
    /// on a `flavor = "current_thread"` runtime, so its future is never moved
    /// across threads; concurrent tests on other OS threads simply block on
    /// `lock()` until this test releases the guard, which is exactly the
    /// serialization we want.
    // SAFETY (await_holding_lock): the doc comment above explains why the
    // env-dir Mutex is intentionally held across the test awaits (current-thread
    // runtime, serialization is the whole point). Test-only.
    #[allow(clippy::await_holding_lock)]
    async fn with_isolated_runtime_root<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Codex P3 — SAVE the prior value and RESTORE it on exit (instead of
        // unconditionally removing) so running the suite with the variable
        // already set does not leak a cleared env to later tests.
        let prev = std::env::var_os("AGENTDESK_ROOT_DIR");
        let tmp = tempfile::tempdir().expect("create temp runtime dir for turn finalizer test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f().await;
        unsafe {
            match prev {
                Some(value) => std::env::set_var("AGENTDESK_ROOT_DIR", value),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }

    /// A `Complete` on a registered Pending turn finalizes exactly once and the
    /// late loser receives `AlreadyFinalized`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn exactly_once_complete_then_late_complete() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.global_active.store(1, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(101);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));

            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// A turn registered with the real `user_msg_id` and a terminal submitted
    /// with the channel-only `user_msg_id == 0` (recovery/orphan path) MUST
    /// resolve to the SAME ledger entry, so the channel-only terminal finalizes
    /// the registered turn and a later real-id terminal loses the exactly-once
    /// gate. Regression for the codex P2: keying on `user_msg_id` would split
    /// them and duplicate the finalize side-effects.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn unknown_user_msg_id_collapses_onto_registered_turn() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(606);
            let registered = TurnKey::new(ch, 99_999, 0);
            let channel_only = TurnKey::new(ch, 0, 0);
            fin.register_start(registered, ProviderKind::Claude, RelayOwnerKind::Watcher);

            // Channel-only (id 0) terminal finalizes the registered turn.
            let first = fin
                .submit_terminal(
                    channel_only,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(first, FinalizeOutcome::Finalized { .. }));

            // Real-id terminal now hits the same entry → AlreadyFinalized.
            let second = fin
                .submit_terminal(
                    registered,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Two SEQUENTIAL turns in the SAME channel within `FINALIZED_TTL` must be
    /// distinct ledger entries: finalizing turn-1 must NOT make turn-2's
    /// terminal return `AlreadyFinalized` (that would strand turn-2's mailbox
    /// token and re-introduce the stuck-channel bug). Regression for the codex
    /// P1 on the channel-only ledger key.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn sequential_same_channel_turns_each_finalize() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(909);
            let turn1 = TurnKey::new(ch, 1001, 0);
            let turn2 = TurnKey::new(ch, 1002, 0);

            fin.register_start(turn1, ProviderKind::Claude, RelayOwnerKind::Watcher);
            let f1 = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(f1, FinalizeOutcome::Finalized { .. }));

            // turn-2 starts immediately (well within the 60s Finalized TTL) and
            // must finalize on its own, not be swallowed by turn-1's entry.
            fin.register_start(turn2, ProviderKind::Claude, RelayOwnerKind::Watcher);
            let f2 = fin
                .submit_terminal(
                    turn2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(f2, FinalizeOutcome::Finalized { .. }),
                "turn-2 must finalize independently of turn-1"
            );
        })
        .await;
    }

    /// Cross-turn safety: a STALE channel-only (id-0) terminal arriving after
    /// turn-1 finalized and turn-2 registered must NOT finalize turn-2. The
    /// resolver routes it to the orphan key (because a Finalized entry exists),
    /// leaving turn-2 live so turn-2's own terminal still finalizes it.
    /// Regression for the codex P1 on channel-only resolution.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_channel_only_terminal_does_not_finalize_next_turn() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let ch = ChannelId::new(919);
            let turn1 = TurnKey::new(ch, 2001, 0);
            let turn2 = TurnKey::new(ch, 2002, 0);
            let channel_only = TurnKey::new(ch, 0, 0);

            fin.register_start(turn1, ProviderKind::Claude, RelayOwnerKind::Watcher);
            let _ = fin
                .submit_terminal(
                    turn1,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            // turn-2 registers (queued follow-up now live).
            fin.register_start(turn2, ProviderKind::Claude, RelayOwnerKind::Watcher);

            // A STALE id-0 terminal (from turn-1's watcher) arrives. Because a
            // Finalized entry exists AND a different live entry (turn-2) exists,
            // the ambiguous-channel-only guard returns it as a no-op
            // (AlreadyFinalized) WITHOUT running channel-scoped cleanup — so it
            // cannot release turn-2's token.
            let stale = fin
                .submit_terminal(
                    channel_only,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(stale, FinalizeOutcome::AlreadyFinalized),
                "ambiguous stale id-0 terminal must be a no-op"
            );

            // turn-2 is still live: its own terminal finalizes it.
            let f2 = fin
                .submit_terminal(
                    turn2,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(f2, FinalizeOutcome::Finalized { .. }),
                "turn-2 must still be live after the stale id-0 terminal"
            );
        })
        .await;
    }

    /// The counter is decremented at most once even under a double terminal
    /// submission — `removed_token.is_some()` gates the decrement and the
    /// saturating helper can never underflow.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn no_underflow_on_double_terminal() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            // No active mailbox turn → mailbox_finish_turn returns removed_token=None.
            shared.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(202);

            let _ = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            let _ = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            // Never underflows below zero.
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// A gate-timeout with a busy pane and a live relay owner defers; once the
    /// backstop deadline elapses the reconciler finalizes exactly once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_busy_finalizes_after_backstop() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(303);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(deferred, FinalizeOutcome::Deferred));

            // Sleep past GATE_BACKSTOP. Under `start_paused` the runtime
            // auto-advances the clock once all tasks are idle on timers, which lets
            // the actor's own reconcile interval fire and finalize the deferred
            // entry. A couple of extra reconcile intervals guarantees the pass ran.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // A late terminal now sees Finalized → AlreadyFinalized, proving the
            // reconciler finalized the deferred entry.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Repeated gate-timeouts (the watcher submits one per pass while the pane
    /// stays busy) must NOT push the backstop deadline forward — otherwise a
    /// persistently busy pane never finalizes. The deadline is armed once; even
    /// with re-submissions arriving every ~half-backstop, the original deadline
    /// elapses and the reconciler finalizes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn repeated_gate_timeout_does_not_postpone_backstop() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(313);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            // Re-submit gate-timeouts at ~1/3 of the backstop apart, three times,
            // staying UNDER the single backstop window in total. Each is Deferred;
            // critically, the re-submissions must not reset the original deadline.
            let third = GATE_BACKSTOP / 3;
            for _ in 0..3 {
                let d = fin
                    .submit_terminal(
                        k,
                        ProviderKind::Claude,
                        TerminalEvent::GateTimeout {
                            pane_quiescent: Some(false),
                        },
                        FinalizeContext::watcher(),
                        shared.clone(),
                    )
                    .await;
                assert!(matches!(d, FinalizeOutcome::Deferred));
                tokio::time::sleep(third).await;
            }
            // ~GATE_BACKSTOP has now elapsed since the FIRST (and only effective)
            // arming. If re-submissions had postponed it, the deadline would still
            // be ~third away. Sleep a hair more to cross the original deadline and
            // let the reconciler run.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 2).await;
            tokio::task::yield_now().await;

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "the backstop must have finalized despite repeated gate-timeouts"
            );
        })
        .await;
    }

    /// A gate-timeout whose pane is already quiescent finalizes immediately
    /// (no deferral).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(404);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(true),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(outcome, FinalizeOutcome::Finalized { .. }));
        })
        .await;
    }

    /// A cancel submission does not double-apply completion cleanup and yields
    /// the exactly-once gate (a late Complete loses).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_then_late_complete_already_finalized() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(505);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let cancelled = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(cancelled, FinalizeOutcome::Finalized { .. }));

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Phase 3 race: the watcher (`FinalizeContext::watcher`) and the bridge
    /// (`FinalizeContext::bridge`) both submit `Complete` for the same turn.
    /// The single-task ledger gate yields exactly one `Finalized` and one
    /// `AlreadyFinalized` regardless of which arrives first — replacing the old
    /// `mailbox_finalize_owed` CAS handoff. The counter is decremented at most
    /// once (no active mailbox turn here, so it stays at 0 either way).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn bridge_watcher_race_finalizes_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = key(707);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let watcher = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            let bridge = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            // Exactly one Finalized, exactly one AlreadyFinalized.
            let finalized = [&watcher, &bridge]
                .iter()
                .filter(|o| matches!(o, FinalizeOutcome::Finalized { .. }))
                .count();
            let already = [&watcher, &bridge]
                .iter()
                .filter(|o| matches!(o, FinalizeOutcome::AlreadyFinalized))
                .count();
            assert_eq!(finalized, 1, "exactly one submission performs the finalize");
            assert_eq!(already, 1, "the loser receives AlreadyFinalized");
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// Phase 3 gate-timeout then watcher-complete-before-deadline: a
    /// `GateTimeout{Some(false)}` defers, then the watcher submits `Complete`
    /// (pane quiesced) before the backstop elapses → that Complete finalizes
    /// once and the later reconciler tick finds the entry already Finalized, so
    /// no double finalize.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_then_complete_before_deadline_no_double() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let fin = TurnFinalizer::spawn();
            let k = key(808);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(deferred, FinalizeOutcome::Deferred));

            // Pane quiesced: the watcher's next pass submits Complete well before
            // the backstop deadline.
            let complete = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(complete, FinalizeOutcome::Finalized { .. }));

            // Let the backstop elapse; the reconciler must NOT finalize again.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
        })
        .await;
    }

    /// Restored/unregistered-watcher gate-timeout (#3016 P2 regression): a
    /// watcher restored after a restart submits `GateTimeout{Some(false)}`
    /// WITHOUT a prior `register_start` (the bridge handoff never ran), so the
    /// ledger entry is created on-demand with `relay_owner == None`. That path
    /// must finalize IMMEDIATELY (not Deferred — no owner will ever drive the
    /// pane to quiescence and the caller discards the outcome while the
    /// `!lifecycle_stage_paused` cleanup block is skipped), AND it must honor
    /// the pending-queue state so a queued follow-up actually kicks off:
    ///   * outcome is `Finalized` (the channel does NOT stay stuck), and
    ///   * `removed_token` is `Some` (the active turn's mailbox token is
    ///     released — inflight/mailbox not orphaned), and
    ///   * `has_pending` is `true` (the queued follow-up is surfaced so the
    ///     finalizer's `kickoff_queue` schedules its dispatch).
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restored_unregistered_watcher_gate_timeout_finalizes_and_kicks_off_queue() {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.global_active.store(1, Ordering::Relaxed);
            let ch = ChannelId::new(1101);

            // An active turn (live cancel token) so `mailbox_finish_turn` returns
            // `removed_token = Some` — the orphan/restored turn whose token must be
            // released by the immediate finalize.
            let active_token = Arc::new(CancelToken::new());
            shared
                .mailbox(ch)
                .restore_active_turn(active_token.clone(), UserId::new(7), MessageId::new(70))
                .await;
            // A queued soft follow-up behind the active turn so `has_pending` is
            // true; the immediate-no-owner path must surface it (kickoff_queue).
            shared
                .mailbox(ch)
                .replace_queue(
                    vec![Intervention {
                        author_id: UserId::new(1),
                        author_is_bot: false,
                        message_id: MessageId::new(71),
                        source_message_ids: vec![MessageId::new(71)],
                        text: "queued follow-up".to_string(),
                        mode: InterventionMode::Soft,
                        created_at: std::time::Instant::now(),
                        reply_context: None,
                        has_reply_boundary: false,
                        merge_consecutive: false,
                        pending_uploads: Vec::new(),
                        voice_announcement: None,
                    }],
                    super::super::queue_persistence_context(&shared, &ProviderKind::Claude, ch),
                )
                .await;

            let fin = TurnFinalizer::spawn();
            // NOTE: no `register_start` — this is the restored/unregistered watcher.
            // The watcher submits with `FinalizeContext::watcher()` exactly as the
            // tmux_watcher gate-timeout call-site does, and discards the outcome.
            let outcome = fin
                .submit_terminal(
                    TurnKey::new(ch, 0, 0),
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;

            match outcome {
                FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => {
                    assert!(
                        removed_token.is_some(),
                        "restored watcher gate-timeout must release the active mailbox token, \
                     not leave it orphaned"
                    );
                    assert!(
                        has_pending,
                        "the queued follow-up must be honored so the finalizer kicks off the queue"
                    );
                }
                other => panic!(
                    "restored/unregistered-watcher gate-timeout must finalize immediately \
                 (not stay stuck), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Counter decremented exactly once (token was removed).
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// #3016 root-cause regression (the escalated [P1] wrong-turn race): a
    /// terminal carrying turn-1's real `user_msg_id` arriving AFTER turn-1
    /// finalized and turn-2 is the live active mailbox turn must NOT release
    /// turn-2's token or decrement `global_active`. The identity-guarded
    /// `mailbox_finish_turn_if_matches` no-ops because the mailbox's active
    /// `user_message_id` (turn-2) does not match the terminal's (turn-1). This
    /// is the channel-scoped-`mailbox_finish_turn` hazard the guard closes:
    /// before this fix, `do_finalize` would have channel-scoped-finished
    /// turn-2's token.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_real_id_terminal_does_not_release_newer_turn_token() {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(1313);
            let turn1_id = 5001u64;
            let turn2_id = 5002u64;

            // turn-2 is the CURRENT live active turn in the mailbox (a fresh turn
            // that started after turn-1 finalized — its identity is turn2_id).
            let turn2_token = Arc::new(CancelToken::new());
            shared.global_active.store(1, Ordering::Relaxed);
            shared
                .mailbox(ch)
                .restore_active_turn(
                    turn2_token.clone(),
                    UserId::new(7),
                    MessageId::new(turn2_id),
                )
                .await;
            // A queued soft follow-up sits behind turn-2. If the stale terminal
            // surfaced `has_pending`, the bridge could drain THIS message behind the
            // live turn — so the guard-miss path must report no backlog (Codex P2).
            shared
                .mailbox(ch)
                .replace_queue(
                    vec![Intervention {
                        author_id: UserId::new(1),
                        author_is_bot: false,
                        message_id: MessageId::new(5003),
                        source_message_ids: vec![MessageId::new(5003)],
                        text: "queued behind turn-2".to_string(),
                        mode: InterventionMode::Soft,
                        created_at: std::time::Instant::now(),
                        reply_context: None,
                        has_reply_boundary: false,
                        merge_consecutive: false,
                        pending_uploads: Vec::new(),
                        voice_announcement: None,
                    }],
                    super::super::queue_persistence_context(&shared, &ProviderKind::Claude, ch),
                )
                .await;

            // Channel-scoped routing state that belongs to the LIVE turn-2. A stale
            // turn-1 terminal must not clear any of it (Codex P2 — the trailing
            // side-effects must be skipped when the identity guard misses).
            //   * dispatch_thread_parents is cleaned via `retain(|_, v| *v != ch)`,
            //     so seed an entry whose VALUE is `ch` (a thread routing TO it).
            //   * dispatch_role_overrides is keyed BY `ch`.
            let thread_ch = ChannelId::new(1314);
            let override_ch = ChannelId::new(1315);
            shared.dispatch_thread_parents.insert(thread_ch, ch);
            shared.dispatch_role_overrides.insert(ch, override_ch);

            let fin = TurnFinalizer::spawn();
            // A STALE terminal for turn-1 (its real id) arrives. The finalizer
            // ledger has no entry for it (turn-1's entry was GC'd / never here), so
            // it creates a Pending entry and finalizes — but the identity-guarded
            // mailbox finish must refuse to touch turn-2's token.
            let outcome = fin
                .submit_terminal(
                    TurnKey::new(ch, turn1_id, 0),
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            match outcome {
                FinalizeOutcome::Finalized {
                    removed_token,
                    has_pending,
                    ..
                } => {
                    assert!(
                        removed_token.is_none(),
                        "stale turn-1 terminal must NOT release turn-2's mailbox token"
                    );
                    // Codex P2: a guarded miss must report NO backlog so the bridge
                    // does not drain a queued soft message behind the live turn-2.
                    assert!(
                        !has_pending,
                        "stale terminal must report no pending backlog (turn-2 owns its queue)"
                    );
                }
                other => panic!(
                    "stale terminal still flows through the ledger gate as Finalized, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }

            // turn-2's token is untouched and the global counter was NOT
            // decremented for a turn this terminal did not own.
            assert!(
                !turn2_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed),
                "turn-2 must not be cancelled by a stale turn-1 terminal"
            );
            assert!(
                shared.mailbox(ch).has_active_turn().await,
                "turn-2 must remain the live active turn"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                1,
                "global_active must not be decremented for the wrong turn"
            );

            // Codex P2: the live turn-2's channel-scoped routing state survives the
            // stale terminal — the guard-missed finalize must skip the trailing
            // side-effects, not corrupt the newer turn's routing/watchdog metadata.
            assert!(
                shared
                    .dispatch_thread_parents
                    .get(&thread_ch)
                    .is_some_and(|v| *v == ch),
                "stale terminal must NOT drop turn-2's dispatch_thread_parents entry"
            );
            assert!(
                shared
                    .dispatch_role_overrides
                    .get(&ch)
                    .is_some_and(|v| *v == override_ch),
                "stale terminal must NOT drop turn-2's dispatch_role_overrides entry"
            );

            // And turn-2 finalizes correctly on its OWN matching terminal.
            let f2 = fin
                .submit_terminal(
                    TurnKey::new(ch, turn2_id, 0),
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match f2 {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "turn-2's own terminal must release turn-2's token"
                    );
                }
                other => panic!(
                    "turn-2 must finalize on its own id, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "now turn-2 is finalized exactly once"
            );
        })
        .await;
    }

    /// #3016 Task 1 regression: the guarded inflight clear (the deadline-armed
    /// gate-timeout / restored-watcher backstop, which passes
    /// `clear_inflight = true`) must NOT delete a DIFFERENT (next) turn's
    /// inflight file. A backstop firing with turn-1's identity after turn-2 has
    /// written its inflight must preserve turn-2's inflight — exactly as the
    /// pre-#3016 identity-guarded placeholder sweeper did via
    /// `inflight_state_still_same_turn`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn guarded_inflight_clear_preserves_next_turn_inflight() {
        use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};

        with_isolated_runtime_root(|| async move {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(1414);
        let turn1_id = 6001u64;
        let turn2_id = 6002u64;

        // turn-2 has already written its inflight for this channel (it took
        // over after turn-1 finalized).
        let next = InflightTurnState::new(
            ProviderKind::Claude,
            ch.get(),
            None,
            7,
            turn2_id,
            turn2_id,
            "turn-2 text".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        save_inflight_state(&next).expect("persist turn-2 inflight for test");

        let fin = TurnFinalizer::spawn();
        // A backstop-style finalize carrying turn-1's identity with
        // `clear_inflight = true` (gate_backstop semantics). It must NOT delete
        // turn-2's inflight because the on-disk id (turn-2) does not match.
        let _ = fin
            .submit_terminal(
                TurnKey::new(ch, turn1_id, 0),
                ProviderKind::Claude,
                TerminalEvent::GateTimeout {
                    pane_quiescent: Some(true),
                },
                FinalizeContext::gate_backstop(),
                shared.clone(),
            )
            .await;

        // turn-2's inflight survives the wrong-turn backstop clear.
        let surviving = crate::services::discord::inflight::load_inflight_state(
            &ProviderKind::Claude,
            ch.get(),
        );
        assert!(
            surviving.is_some_and(|s| s.user_msg_id == turn2_id),
            "guarded inflight clear must preserve turn-2's inflight when finalize carries turn-1's identity"
        );
        }).await;
    }

    /// #3016 Codex P1 regression: an id-0 watcher/recovery terminal that
    /// collapses onto a ledger entry registered with the REAL `user_msg_id`
    /// must finalize under THAT resolved identity (the guarded
    /// `finish_turn_if_matches` path), not the literal id-0 (unguarded
    /// channel-scoped finish). Here the mailbox's active turn was started with a
    /// DIFFERENT id than the registered turn — simulating a stale ledger entry
    /// while a newer turn is live — so the resolved-identity guarded finish must
    /// refuse to release the newer turn's token.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn id0_terminal_uses_resolved_identity_and_spares_newer_turn() {
        use serenity::model::id::{MessageId, UserId};

        with_isolated_runtime_root(|| async move {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let ch = ChannelId::new(1515);
        let registered_id = 7001u64; // the ledger entry's real identity
        let live_active_id = 7002u64; // the DIFFERENT turn live in the mailbox

        // The mailbox's active turn is a NEWER turn (live_active_id), distinct
        // from the ledger's registered identity.
        let live_token = Arc::new(CancelToken::new());
        shared.global_active.store(1, Ordering::Relaxed);
        shared
            .mailbox(ch)
            .restore_active_turn(
                live_token.clone(),
                UserId::new(7),
                MessageId::new(live_active_id),
            )
            .await;

        let fin = TurnFinalizer::spawn();
        // Register the ledger entry under registered_id (a real id). A later
        // id-0 terminal collapses onto THIS entry.
        fin.register_start(
            TurnKey::new(ch, registered_id, 0),
            ProviderKind::Claude,
            RelayOwnerKind::Watcher,
        );

        // id-0 terminal → resolves to the registered entry → finalize_key uses
        // the registered real id → guarded finish, which does NOT match the
        // live newer turn (live_active_id) → token spared.
        let outcome = fin
            .submit_terminal(
                TurnKey::new(ch, 0, 0),
                ProviderKind::Claude,
                TerminalEvent::Complete,
                FinalizeContext::watcher(),
                shared.clone(),
            )
            .await;

        match outcome {
            FinalizeOutcome::Finalized { removed_token, .. } => {
                assert!(
                    removed_token.is_none(),
                    "id-0 terminal resolved to a stale registered id must NOT release the newer live turn's token"
                );
            }
            other => panic!(
                "expected Finalized via the resolved-identity guarded path, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(
            !live_token
                .cancelled
                .load(std::sync::atomic::Ordering::Relaxed),
            "the newer live turn must not be cancelled"
        );
        assert!(
            shared.mailbox(ch).has_active_turn().await,
            "the newer live turn must remain active"
        );
        assert_eq!(
            shared.global_active.load(Ordering::Relaxed),
            1,
            "counter must not be decremented for the wrong turn"
        );
        }).await;
    }

    // ----------------------------------------------------------------------
    // #3016 step 1 — (actor × terminal-path) exactly-once finalize matrix.
    //
    // The tests below fill the cells the suite above left uncovered so phase 5
    // (legacy `mailbox_finalize_owed` flag removal) lands behind a complete
    // exactly-once guard. Each asserts the three per-cell invariants:
    //   (1) late/double terminal is `AlreadyFinalized` (no double finalize),
    //   (2) `global_active` never underflows,
    //   (3) the mailbox cancel token is released exactly once (no under- or
    //       over-finalize).
    // These SPECIFY current production behaviour (tests only; no prod change).
    // ----------------------------------------------------------------------

    /// Seed a live active mailbox turn so `mailbox_finish_turn` returns
    /// `removed_token = Some` and assert helpers can verify the release.
    /// Returns the token so the caller can check its `cancelled` flag.
    async fn seed_active_turn(
        shared: &Arc<SharedData>,
        ch: ChannelId,
        user_msg_id: u64,
    ) -> Arc<CancelToken> {
        use serenity::model::id::{MessageId, UserId};
        let token = Arc::new(CancelToken::new());
        shared
            .mailbox(ch)
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(user_msg_id))
            .await;
        token
    }

    /// RelayMiss × bridge: a relay-miss terminal on a registered turn finalizes
    /// exactly once, releases the active token, decrements the counter once, and
    /// a late RelayMiss loses the gate. Covers the RelayMiss observability path
    /// (F) of `do_finalize`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_bridge_finalizes_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2001);
            let tid = 8001u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "relay-miss finalize must release the active turn's token"
                    );
                }
                other => panic!(
                    "relay-miss must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the released token must be marked cancelled"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "counter decremented exactly once"
            );

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "late relay-miss must lose the exactly-once gate"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late relay-miss"
            );
        })
        .await;
    }

    /// RelayMiss × watcher then late Complete: the watcher-path relay-miss
    /// finalizes once; a later Complete (different actor) sees `AlreadyFinalized`
    /// and the counter never underflows.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_watcher_then_late_complete_already_finalized() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2002);
            let tid = 8002u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "the watcher relay-miss must release the active turn's token"
                    );
                }
                other => panic!(
                    "watcher relay-miss must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): the released token is cancelled and the active turn
            // is cleared off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the released token must be marked cancelled"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the relay-miss finalize must clear the active turn"
            );

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// RelayMiss × orphan (id-0, no active mailbox turn): the channel-scoped
    /// finish returns `removed_token = None`, so the counter is left untouched
    /// (already 0) and never underflows even on a double submission.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn relay_miss_orphan_no_active_turn_no_underflow() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            shared.global_active.store(0, Ordering::Relaxed);
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ChannelId::new(2003), 0, 0);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_none(),
                        "orphan relay-miss with no active turn removes no token"
                    );
                }
                other => panic!(
                    "orphan relay-miss still finalizes (idempotent no-op), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Double submit on the same id-0 orphan. The first submit recorded a
            // `Finalized` ledger entry, so the second MUST lose the exactly-once
            // gate. Binding (not discarding) this outcome guards invariant (1):
            // if id-0/no-token ever regressed to a second `Finalized` no-op the
            // counter would still read 0 and the discarded-outcome version would
            // pass blind — this assert catches that regression.
            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::RelayMiss,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(second, FinalizeOutcome::AlreadyFinalized),
                "the second orphan relay-miss must lose the exactly-once gate, \
                 not re-finalize as a no-op"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "orphan relay-miss must never underflow the counter"
            );
        })
        .await;
    }

    /// Cancel × watcher: a watcher-path cancel finalizes once, releases the
    /// active token (sets `cancelled`) WITHOUT marking completion-cleanup (the
    /// watcher context passes `allow_completion_cleanup = false` and cancel is
    /// gated out anyway), decrements the counter once, and a late Complete loses.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancel_watcher_finalizes_and_releases_token() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2004);
            let tid = 8004u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let cancelled = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match cancelled {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "watcher cancel must release the active token"
                    );
                }
                other => panic!(
                    "watcher cancel must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "cancel must set the token's cancelled flag"
            );
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "no underflow after the late complete on a cancelled turn"
            );
        })
        .await;
    }

    /// Reconciler-backstop × deferred gate-timeout (the REAL prod backstop cell):
    /// a `GateTimeout{Some(false)}` with a live relay owner defers (arming the
    /// backstop deadline); once the deadline elapses the reconciler drives the
    /// `gate_backstop()` finalize — the ONLY way `gate_backstop()` is reached in
    /// prod (`reconcile()` always submits `GateTimeout{Some(true)}` through it).
    /// That backstop finalize releases the seeded active turn's token exactly
    /// once, clears the active turn, and decrements the counter once; a late
    /// terminal then loses the gate without underflow.
    ///
    /// (Replaces the former `cancel_gate_backstop_finalizes_exactly_once`, which
    /// injected `TerminalEvent::Cancel` through `gate_backstop()` — an impossible
    /// cell that never occurs in prod and overstated matrix coverage.)
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn reconciler_backstop_finalizes_deferred_gate_timeout_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2005);
            let tid = 8005u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            // A live relay owner is what makes `GateTimeout{Some(false)}` defer
            // (arming the backstop) rather than finalize immediately.
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let deferred = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: Some(false),
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(deferred, FinalizeOutcome::Deferred),
                "a busy-pane gate-timeout with a live owner must defer to the backstop"
            );
            // The token is still live while the entry is only deferred.
            assert!(
                !token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the deferred turn's token must not be released before the backstop fires"
            );
            assert!(
                shared.mailbox(ch).has_active_turn().await,
                "the active turn must persist while the gate-timeout is only deferred"
            );

            // Sleep past GATE_BACKSTOP. Under `start_paused` the runtime
            // auto-advances the clock once tasks idle on timers, letting the
            // actor's reconcile interval fire and drive the `gate_backstop()`
            // finalize. A couple extra intervals guarantees the pass ran.
            tokio::time::sleep(GATE_BACKSTOP + RECONCILE_INTERVAL * 3).await;
            tokio::task::yield_now().await;

            // The reconciler-backstop finalize released the seeded active turn's
            // token exactly once.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "the reconciler backstop must release (cancel) the active turn's token"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the reconciler backstop must clear the active turn"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "the reconciler backstop decrements the counter exactly once"
            );

            // A late terminal now sees Finalized → AlreadyFinalized, proving the
            // backstop finalized the deferred entry and the gate holds.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "a late terminal after the backstop must lose the exactly-once gate"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late terminal after the backstop finalize"
            );
        })
        .await;
    }

    /// GateTimeout{None} × watcher: `pane_quiescent == None` is NOT the
    /// `Some(false)` deferral trigger, so it finalizes IMMEDIATELY (like
    /// `Some(true)`) even with a live relay owner. Releases the token once and a
    /// late Complete loses.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_none_watcher_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2006);
            let tid = 8006u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            // Register with a live owner so we PROVE None does not defer the way
            // Some(false) would with an owner present.
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: None,
                    },
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match outcome {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "GateTimeout{{None}} must finalize now and release the token"
                    );
                }
                other => panic!(
                    "GateTimeout{{None}} must finalize immediately (not Deferred), got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);

            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(late, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
        })
        .await;
    }

    /// GateTimeout{None} × bridge: same immediate-finalize semantics through the
    /// bridge context (no deferral), exactly once.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn gate_timeout_pane_quiescent_none_bridge_finalizes_now() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2007);
            let tid = 8007u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let outcome = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::GateTimeout {
                        pane_quiescent: None,
                    },
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match outcome {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "GateTimeout{{None}} via bridge must finalize now and release the token"
                    );
                }
                other => panic!(
                    "GateTimeout{{None}} via bridge must finalize immediately (not Deferred), \
                     got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): the released token is cancelled and the active turn
            // is cleared off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "GateTimeout{{None}} via bridge must mark the released token cancelled"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "GateTimeout{{None}} via bridge must clear the active turn"
            );
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);

            // Late probe: a follow-up terminal on the now-finalized turn must
            // lose the exactly-once gate without underflowing the counter.
            let late = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(
                matches!(late, FinalizeOutcome::AlreadyFinalized),
                "a late terminal after GateTimeout{{None}} must be AlreadyFinalized"
            );
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the late terminal after GateTimeout{{None}}"
            );
        })
        .await;
    }

    /// Genuine orphan recovery (id-0, NO registered ledger entry) with a live
    /// active mailbox turn: with no `Finalized` entry and no live ledger entry
    /// for the channel, the channel-only resolver collapses onto the literal
    /// id-0 key, the entry is created on-demand and finalizes, and the unguarded
    /// channel-scoped finish releases the active turn's token exactly once. A
    /// second submit is `AlreadyFinalized` and the counter never underflows.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn orphan_id0_recovery_finalizes_and_releases_token() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2008);
            let tid = 8008u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            // NO register_start — pure orphan/recovery path keyed only by channel.
            let k = TurnKey::new(ch, 0, 0);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "genuine orphan finalize must release the active turn's token via the \
                         unguarded channel-scoped finish"
                    );
                }
                other => panic!(
                    "genuine orphan terminal must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the orphan finalize must clear the active turn"
            );
            assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);

            // A second id-0 terminal now finds the Finalized entry → no-op.
            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "no underflow on the second orphan terminal"
            );
        })
        .await;
    }

    /// Watcher→bridge handoff double-terminal: the watcher submits Complete
    /// (finalizing the turn) and the bridge then submits its own Complete for the
    /// SAME turn (the post-handoff straggler). Exactly one Finalized, the loser
    /// is AlreadyFinalized, the token releases once, and the counter decrements
    /// exactly once. This is the sequential handoff complement to the concurrent
    /// `bridge_watcher_race_finalizes_exactly_once`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn watcher_then_bridge_handoff_double_terminal_exactly_once() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2009);
            let tid = 8009u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let watcher = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::watcher(),
                    shared.clone(),
                )
                .await;
            let bridge = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Complete,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;

            assert!(
                matches!(watcher, FinalizeOutcome::Finalized { .. }),
                "the first (watcher) submission performs the finalize"
            );
            assert!(
                matches!(bridge, FinalizeOutcome::AlreadyFinalized),
                "the post-handoff bridge straggler loses the gate"
            );
            assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "the counter decremented exactly once across the handoff"
            );
        })
        .await;
    }

    /// Cancel × Cancel double terminal: two cancels for the same turn (e.g. a
    /// reaction and a `/!stop` racing) finalize exactly once and never underflow
    /// the counter. The second cancel is `AlreadyFinalized`.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn double_cancel_finalizes_exactly_once_no_underflow() {
        with_isolated_runtime_root(|| async move {
            let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
            let ch = ChannelId::new(2010);
            let tid = 8010u64;
            shared.global_active.store(1, Ordering::Relaxed);
            let token = seed_active_turn(&shared, ch, tid).await;
            let fin = TurnFinalizer::spawn();
            let k = TurnKey::new(ch, tid, 0);
            fin.register_start(k, ProviderKind::Claude, RelayOwnerKind::Watcher);

            let first = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            match first {
                FinalizeOutcome::Finalized { removed_token, .. } => {
                    assert!(
                        removed_token.is_some(),
                        "the first cancel must release the active turn's token"
                    );
                }
                other => panic!(
                    "the first cancel must finalize, got {:?}",
                    std::mem::discriminant(&other)
                ),
            }
            // Invariant (3): cancel sets the token's cancelled flag and clears
            // the active turn off the channel.
            assert!(
                token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
                "cancel must set the released token's cancelled flag"
            );
            assert!(
                !shared.mailbox(ch).has_active_turn().await,
                "the first cancel must clear the active turn"
            );

            let second = fin
                .submit_terminal(
                    k,
                    ProviderKind::Claude,
                    TerminalEvent::Cancel,
                    FinalizeContext::bridge(),
                    shared.clone(),
                )
                .await;
            assert!(matches!(second, FinalizeOutcome::AlreadyFinalized));
            assert_eq!(
                shared.global_active.load(Ordering::Relaxed),
                0,
                "double cancel must decrement exactly once, never underflow"
            );
        })
        .await;
    }
}
