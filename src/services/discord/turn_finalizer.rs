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
use tokio::time::Instant;
use tokio::sync::{mpsc, oneshot};

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

    /// The literal full-identity key for this turn.
    fn exact_key(&self) -> LedgerKey {
        LedgerKey {
            channel_id: self.channel_id,
            generation: self.generation,
            user_msg_id: self.user_msg_id,
        }
    }
}

/// The exact ledger match: channel + restart generation + user message id.
/// Full identity so sequential same-channel turns never collide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct LedgerKey {
    channel_id: ChannelId,
    generation: u64,
    user_msg_id: u64,
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
fn resolve_ledger_key(
    ledger: &HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
) -> LedgerKey {
    if key.user_msg_id != 0 {
        return key.exact_key();
    }
    let channel_has_finalized = ledger.iter().any(|(lk, entry)| {
        lk.channel_id == key.channel_id
            && lk.generation == key.generation
            && entry.phase == Phase::Finalized
    });
    if channel_has_finalized {
        // Ambiguous (a turn just finalized) → do NOT grab a possibly-new live
        // entry. Route to the literal id-0 orphan key.
        return key.exact_key();
    }
    ledger
        .iter()
        .find(|(lk, entry)| {
            lk.channel_id == key.channel_id
                && lk.generation == key.generation
                && entry.phase != Phase::Finalized
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
    let outcome = do_finalize(key, provider, &event, effective_ctx, shared).await;
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

    // (A) inflight clear — per-site behaviour preserved. Watcher cleared
    //     inflight inline before finalizing; bridge clears it elsewhere.
    if ctx.clear_inflight {
        super::inflight::clear_inflight_state(&provider, channel_id.get());
    }

    // (B) mailbox cancel_token release — the routed sites' single
    //     `mailbox_finish_turn`. Idempotent: returns `removed_token = None` on
    //     a second call, which is what makes a transitional double-finalize a
    //     no-op during Phases 2-3.
    let finish = super::mailbox_finish_turn(shared, &provider, channel_id).await;

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

    // (D) trailing terminal side-effects that today follow `mailbox_finish_turn`
    //     inline at the bridge/watcher call-sites. Moved here so they cannot
    //     diverge between the routed paths.
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
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "turn_finalizer terminal completion with queued backlog",
        );
    }

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
    async fn with_isolated_runtime_root<F, Fut>(f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("create temp runtime dir for turn finalizer test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f().await;
        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
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
        }).await;
    }
}
