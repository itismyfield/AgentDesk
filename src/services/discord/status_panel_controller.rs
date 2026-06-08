//! EPIC #3078 — single-authority `StatusPanelController` (PR-1, DORMANT).
//!
//! Today the user-visible status panel message id
//! (`InflightTurnState.status_message_id`) has NO single writer: five actors
//! mutate it (turn_bridge, tmux_watcher, recovery_engine, placeholder_sweeper,
//! status_panel_orphan_store). That scattered ownership is the shared root of
//! the recurring panel bugs — stale ⏳ (#3099), wrong-classification duplicate
//! panels (#3100), drift/zombie mirrors (#3105), and `MissingTarget` /
//! inflight-missing suppression (#3107). The panel's create → stream-edit →
//! finalize → reclaim lifecycle needs ONE authority, exactly as the landed
//! `TurnFinalizer` (turn_finalizer.rs) gave turn-termination one authority.
//!
//! This module introduces that authority as a PEER actor of the finalizer:
//! an `Arc<StatusPanelController>` on `SharedData`, an mpsc-driven owning task,
//! and a per-turn ledger keyed by the SAME `turn_finalizer::TurnKey` /
//! `LedgerKey` (reusing its channel-only `user_msg_id == 0` recovery/orphan
//! collapse via `turn_finalizer::resolve_channel_only`). The finalizer owns
//! turn-termination side-effects; the controller owns the panel message. The
//! in-memory ledger surviving an inflight clear is exactly what fixes the
//! #3107-class panel-owner loss.
//!
//! State / authority: the in-memory ledger is authoritative for
//! (`msg_id`, `owner`, `phase`, coalesced last-rendered text); the durable
//! mirror is `InflightTurnState.status_message_id`, written ONLY through the
//! #3077 typed ops (`bind_status_panel` / `clear_status_panel_if_current`) so
//! the controller is the single writer. `PanelPhase` gives exactly-once
//! finalize/reclaim (mutually idempotent), mirroring the finalizer's `Phase`.
//!
//! ## DORMANT (PR-1)
//!
//! This PR ships the substrate ONLY: the full API surface, the ledger, and the
//! actor loop, spawned next to the finalizer. NO call site routes through it
//! yet (turn_bridge / tmux_watcher / recovery / sweeper are untouched), so
//! there is ZERO behaviour change. The spawn is gated on
//! `status_panel_v2_enabled`: when v2 is off the actor task is not spawned and
//! the controller stays inert, mirroring the existing v2 short-circuits.
//! Later PRs (#3078 staged plan) route each actor through `ensure_created` /
//! `stream_update` / `finalize` / `reclaim` / `clear_if_current`.

use std::collections::HashMap;
use std::sync::Arc;

use serenity::model::id::{ChannelId, MessageId};
use tokio::sync::{mpsc, oneshot};

use crate::services::provider::ProviderKind;

use super::SharedData;
use super::turn_finalizer::{self, LedgerKey, TurnKey};

/// Which actor owns a turn's status panel. Distinct from
/// `inflight::RelayOwnerKind` because the panel owner is a finer-grained,
/// panel-lifecycle concept (e.g. the recovery engine and the placeholder
/// sweeper are panel owners but not relay owners). The controller records the
/// owner so a later stream/finalize from a DIFFERENT owner can be reconciled
/// against the authority rather than blindly overwriting it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum PanelOwnerKind {
    /// The turn bridge (live relay through the bot's own dispatch).
    Bridge,
    /// The tmux watcher (TUI-direct / external-input publish path).
    Watcher,
    /// The recovery engine (post-restart adoption of an orphaned panel).
    Recovery,
    /// The standby relay (channel with no live owner yet).
    Standby,
    /// A session-bound relay sink.
    SessionBound,
}

/// Lifecycle of a single turn's panel. Owned solely by the actor task; the
/// check-and-set on this enum is the one place exactly-once finalize/reclaim is
/// decided, mirroring the finalizer's `Phase`.
///
/// `NotCreated → Live → Completed | Reclaimed`. `Completed` and `Reclaimed` are
/// both terminal and mutually exclusive: a finalize after a finalize is a
/// no-op, a reclaim after a finalize is a no-op, and vice-versa.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum PanelPhase {
    /// Registered but no panel message exists yet.
    NotCreated,
    /// A panel message exists (id in the ledger) and is being stream-edited.
    Live,
    /// Finalized to its terminal rendering — the turn completed normally.
    Completed,
    /// Reclaimed (deleted / abandoned) — a stop, cleanup, or supersede.
    Reclaimed,
}

impl PanelPhase {
    /// `true` once the panel has reached a terminal phase (finalize/reclaim
    /// already decided). The exactly-once gate.
    fn is_terminal(self) -> bool {
        matches!(self, PanelPhase::Completed | PanelPhase::Reclaimed)
    }
}

/// Outcome of a `finalize` / `reclaim` commit. The caller uses this to decide
/// whether it performed the one terminal transition (so it owns any follow-up
/// bookkeeping) or whether the panel was already terminal / never created.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum PanelCommitOutcome {
    /// This call performed the one terminal transition; `msg_id` is the panel
    /// it acted on (the durable mirror was reconciled accordingly).
    Committed { msg_id: Option<MessageId> },
    /// The panel was already terminal (a prior finalize/reclaim won). No-op.
    AlreadyTerminal,
    /// No panel was ever created for this turn (NotCreated). No-op.
    NoPanel,
}

/// EPIC #3078 — the controller's independent decision for the watcher's
/// status-panel CREATE/ADOPT site (`tmux_watcher.rs` ~7213): given a TUI-direct /
/// external-input turn that has reached the panel-publish gate, does the panel
/// already exist (adopt it), should a fresh panel be created, or is there nothing
/// to do?
///
/// This is the create/adopt analogue of the RECLAIM parity (`sweeper_reclaim_*`):
/// instead of echoing the already-resolved legacy id back (the tautology that
/// scoped PR-4 down — see the deferred-site note below), the controller
/// independently re-derives the decision from the SAME RAW inputs the legacy
/// `watcher_should_create_external_input_status_panel` branch reads, so the
/// shadow parity check is meaningful. The create id is not known until the send
/// returns, so the faithful comparison is on the DECISION (adopt-which-id /
/// create / none), not on a post-send message id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::services::discord) enum WatcherCreateDecision {
    /// A panel already persisted on this turn's inflight row (restart-safe
    /// adoption): adopt the existing id, do NOT publish a duplicate.
    AdoptPersisted(MessageId),
    /// No panel exists yet for an eligible external-input turn: the watcher must
    /// publish (create) the panel itself.
    Create,
    /// Nothing to do at this site: v2 off, a (non-persisted) panel already
    /// exists, or the turn is not an external-input/panel-eligible turn.
    None,
}

/// What a panel should render and how to publish it. The actual Discord
/// create/edit/delete IO is abstracted behind [`PanelSink`] so the ledger +
/// durable-mirror logic is the single authority and is unit-testable without a
/// gateway. Routing PRs (#3078) supply a concrete sink; PR-1 ships the request
/// shape and the in-memory authority only.
#[derive(Clone, Debug)]
pub(in crate::services::discord) struct PanelCreateRequest {
    /// The rendered panel text to publish on create.
    pub(in crate::services::discord) panel_text: String,
}

/// The side that performs the actual Discord message IO (create / edit /
/// delete). DORMANT in PR-1: there is no live implementation wired into a call
/// site yet. Routing PRs provide a gateway-backed sink. Kept as a trait so the
/// controller's authority logic never embeds turn_bridge's heavy send paths
/// (the LoC-freeze constraint on `turn_bridge/mod.rs`).
#[allow(dead_code)]
pub(in crate::services::discord) trait PanelSink: Send + Sync {
    /// Create the panel message, returning its id (or `None` on a headless /
    /// failed send).
    fn create(&self, channel_id: ChannelId, text: &str) -> Option<MessageId>;
    /// Edit an existing panel message in place.
    fn edit(&self, channel_id: ChannelId, msg_id: MessageId, text: &str);
    /// Finalize (edit to terminal rendering) an existing panel message.
    fn finalize(&self, channel_id: ChannelId, msg_id: MessageId, text: &str);
    /// Delete / abandon a panel message on reclaim.
    fn reclaim(&self, channel_id: ChannelId, msg_id: MessageId);
}

/// One ledger entry per turn. The authority for the panel's id, owner, phase,
/// and last-rendered text.
struct PanelEntry {
    msg_id: Option<MessageId>,
    owner: PanelOwnerKind,
    provider: ProviderKind,
    phase: PanelPhase,
    /// Coalesced last-rendered panel text. A `stream_update` that does not
    /// change this is dropped (the coalescing the scattered writers lacked).
    last_text: Option<String>,
}

/// Messages the owning task drains. Each side-effecting variant carries an
/// `Arc<SharedData>` so the durable mirror runs inside the task (single
/// writer) and an ack `oneshot` so the public method can await the authority's
/// decision, exactly like the finalizer.
enum PanelMsg {
    /// Idempotent registration: the ledger learns the turn exists (and its
    /// owner) before any create/stream/finalize. A second register for a live
    /// key only refreshes the owner; it never resurrects a terminal entry.
    Register {
        key: TurnKey,
        provider: ProviderKind,
        owner: PanelOwnerKind,
    },
    EnsureCreated {
        key: TurnKey,
        req: PanelCreateRequest,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<Option<MessageId>>,
    },
    StreamUpdate {
        key: TurnKey,
        panel_text: String,
        shared: Arc<SharedData>,
    },
    Finalize {
        key: TurnKey,
        terminal_text: Option<String>,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<PanelCommitOutcome>,
    },
    Reclaim {
        key: TurnKey,
        reason: &'static str,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<PanelCommitOutcome>,
    },
    /// Compare-and-clear: clear the ledger id ONLY if it currently equals
    /// `msg_id`. The stale-cleanup TOCTOU guard.
    ClearIfCurrent {
        key: TurnKey,
        msg_id: MessageId,
        reason: &'static str,
    },
    /// Read the current panel id for a turn (debug parity / observability).
    CurrentPanel {
        key: TurnKey,
        ack: oneshot::Sender<Option<MessageId>>,
    },
    /// EPIC #3078 PR-3 — READ-ONLY shadow parity query: "which panel id does the
    /// live turn own, per the inflight row?" The legacy sweeper/orphan gates read
    /// the id from the inflight row directly (`inflight.status_message_id`), so
    /// the faithful parity value is that `inflight_panel_id` — NOT whatever
    /// (possibly stale) id the in-memory ledger happens to hold. This query
    /// therefore returns `inflight_panel_id` unchanged, EXCEPT it collapses to
    /// `None` when the resolved ledger entry is already terminal
    /// (Completed/Reclaimed): a panel the controller has finalized/reclaimed is
    /// no longer owned by a live turn. It NEVER mutates the ledger (no seed, no
    /// resurrection of a terminal entry) and NEVER writes the durable mirror, so
    /// it is byte-for-byte inert next to the still-executing legacy path. The key
    /// is resolved with the same `resolve_channel_only` collapse the live ledger
    /// uses, so the #3003 channel-only (`user_msg_id == 0`) defer is honored.
    OrphanParityTarget {
        key: TurnKey,
        inflight_panel_id: Option<MessageId>,
        ack: oneshot::Sender<Option<MessageId>>,
    },
    /// Adopt an ALREADY-EXISTING panel message into the ledger as the live panel
    /// for this turn (the post-restart recovery/orphan case: the panel was sent
    /// by a previous process, so there is nothing to create — the controller
    /// only needs to learn the id so it can own the finalize). Idempotent and
    /// non-resurrecting: a terminal entry is left untouched.
    AdoptRecovered {
        key: TurnKey,
        provider: ProviderKind,
        owner: PanelOwnerKind,
        msg_id: Option<MessageId>,
    },
}

/// The per-runtime actor, held as `Arc<StatusPanelController>` on `SharedData`.
/// One owning task drains the `mpsc`; all public methods are cheap
/// submit-or-await wrappers, exactly like `TurnFinalizer`.
pub(in crate::services::discord) struct StatusPanelController {
    tx: mpsc::UnboundedSender<PanelMsg>,
}

impl StatusPanelController {
    /// Spawn the owning actor task and return the handle.
    ///
    /// `v2_enabled` gates the actor task: when status-panel-v2 is OFF the task
    /// is NOT spawned and the controller stays inert (the unbounded sender just
    /// buffers the never-sent messages), mirroring the existing
    /// `status_panel_v2_enabled` short-circuits across the discord module. The
    /// task is also only spawned when a Tokio runtime handle is available, so
    /// synchronous unit tests that build `SharedData` outside a runtime via
    /// `make_shared_data_for_tests*` never panic (`tokio::spawn` needs a
    /// reactor) — same posture as `TurnFinalizer::spawn`.
    pub(in crate::services::discord) fn spawn(v2_enabled: bool) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        if v2_enabled && let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(actor_loop(rx));
        }
        Arc::new(Self { tx })
    }

    /// Idempotent registration. A second `register` for a live key only
    /// refreshes the owner; it never resurrects a terminal entry. Mirrors
    /// `TurnFinalizer::register_start`.
    pub(in crate::services::discord) fn register(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        owner: PanelOwnerKind,
    ) {
        let _ = self.tx.send(PanelMsg::Register {
            key,
            provider,
            owner,
        });
    }

    /// Ensure a panel message exists for this turn, creating one (and binding
    /// the durable mirror via #3077 `bind_status_panel`) if not. Returns the
    /// current panel id. Idempotent: a second call on a live entry returns the
    /// already-bound id without a duplicate send.
    pub(in crate::services::discord) async fn ensure_created(
        &self,
        key: TurnKey,
        req: PanelCreateRequest,
        shared: Arc<SharedData>,
    ) -> Option<MessageId> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(PanelMsg::EnsureCreated {
                key,
                req,
                shared,
                ack,
            })
            .is_err()
        {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// Stream a new rendering into the live panel. Coalesced: a text identical
    /// to the last-rendered text is dropped. A no-op on a terminal entry.
    pub(in crate::services::discord) fn stream_update(
        &self,
        key: TurnKey,
        panel_text: String,
        shared: Arc<SharedData>,
    ) {
        let _ = self.tx.send(PanelMsg::StreamUpdate {
            key,
            panel_text,
            shared,
        });
    }

    /// Finalize the panel to its terminal rendering, exactly once. A finalize
    /// after a finalize/reclaim returns `AlreadyTerminal`.
    pub(in crate::services::discord) async fn finalize(
        &self,
        key: TurnKey,
        terminal_text: Option<String>,
        shared: Arc<SharedData>,
    ) -> PanelCommitOutcome {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(PanelMsg::Finalize {
                key,
                terminal_text,
                shared,
                ack,
            })
            .is_err()
        {
            return PanelCommitOutcome::AlreadyTerminal;
        }
        rx.await.unwrap_or(PanelCommitOutcome::AlreadyTerminal)
    }

    /// Reclaim (delete / abandon) the panel, exactly once and mutually
    /// idempotent with `finalize`. A reclaim after a finalize/reclaim returns
    /// `AlreadyTerminal`. Clears the durable mirror via #3077
    /// `clear_status_panel_if_current`.
    pub(in crate::services::discord) async fn reclaim(
        &self,
        key: TurnKey,
        reason: &'static str,
        shared: Arc<SharedData>,
    ) -> PanelCommitOutcome {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(PanelMsg::Reclaim {
                key,
                reason,
                shared,
                ack,
            })
            .is_err()
        {
            return PanelCommitOutcome::AlreadyTerminal;
        }
        rx.await.unwrap_or(PanelCommitOutcome::AlreadyTerminal)
    }

    /// Compare-and-clear the ledger panel id, ONLY when it currently equals
    /// `msg_id`. The stale-cleanup TOCTOU guard: an actor that loaded an older
    /// snapshot cannot wipe a panel a newer turn already rebound.
    pub(in crate::services::discord) fn clear_if_current(
        &self,
        key: TurnKey,
        msg_id: MessageId,
        reason: &'static str,
    ) {
        let _ = self.tx.send(PanelMsg::ClearIfCurrent {
            key,
            msg_id,
            reason,
        });
    }

    /// Read the current panel id for a turn (debug parity / observability).
    pub(in crate::services::discord) async fn current_panel(
        &self,
        key: TurnKey,
    ) -> Option<MessageId> {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(PanelMsg::CurrentPanel { key, ack }).is_err() {
            return None;
        }
        rx.await.unwrap_or(None)
    }

    /// Adopt an already-existing panel message into the ledger (post-restart
    /// recovery: the panel was created by a previous process). The controller
    /// learns the id so it can own the finalize. Idempotent and
    /// non-resurrecting: a terminal entry is never reopened. Fire-and-forget;
    /// observe the resulting id with [`Self::current_panel`].
    pub(in crate::services::discord) fn adopt_recovered(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        owner: PanelOwnerKind,
        msg_id: Option<MessageId>,
    ) {
        let _ = self.tx.send(PanelMsg::AdoptRecovered {
            key,
            provider,
            owner,
            msg_id,
        });
    }

    /// EPIC #3078 PR-2 — the controller's chosen status-panel completion id for
    /// a recovery turn, computed through the ledger collapse so it can be
    /// asserted equal to the legacy `recovery_status_panel_message_id_for_completion`
    /// result behind a parity check. Adopts the recovered panel id, then reads
    /// back the resolved id. Shadow-only: this does NOT finalize or touch the
    /// durable mirror, so the legacy completion path keeps executing unchanged.
    pub(in crate::services::discord) async fn recovery_completion_parity_id(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        recovered_msg_id: Option<MessageId>,
    ) -> Option<MessageId> {
        self.adopt_recovered(key, provider, PanelOwnerKind::Recovery, recovered_msg_id);
        self.current_panel(key).await
    }

    /// EPIC #3078 PR-3 — the panel id the controller WOULD reclaim for an orphaned
    /// status-panel row the placeholder sweeper is converging, asserted equal to the
    /// legacy `panel_reclaim_target` / `clear_status_panel_if_current` target. The
    /// legacy reclaim target IS the persisted panel id on the inflight row, so the
    /// faithful parity value is that `panel_msg_id` argument itself — read back
    /// through a READ-ONLY query that ONLY collapses to `None` when the controller
    /// has already finalized/reclaimed this turn's panel (a terminal ledger entry).
    /// Crucially this does NOT seed the ledger from the row's id (the prior
    /// `adopt_recovered` seed read back a STALE in-memory id when one was already
    /// present, diverging from legacy); it reflects the row's CURRENT id. Shadow-
    /// only: NO finalize/reclaim, NO ledger mutation, NO durable-mirror write, NO
    /// Discord IO, so the legacy sweeper keeps executing the actual delete/clear
    /// unchanged.
    pub(in crate::services::discord) async fn sweeper_reclaim_parity_id(
        &self,
        key: TurnKey,
        _provider: ProviderKind,
        panel_msg_id: Option<MessageId>,
    ) -> Option<MessageId> {
        self.orphan_parity_target(key, panel_msg_id).await
    }

    /// EPIC #3078 — the controller's INDEPENDENT decision for the watcher's
    /// status-panel CREATE/ADOPT site, re-derived from RAW inputs (not the
    /// already-resolved legacy id, which `orphan_parity_target` would echo back
    /// tautologically — the codex P2 finding that scoped PR-4 down). Faithful to
    /// the legacy branch order at `tmux_watcher.rs` ~7213:
    ///
    /// 1. a persisted (restart-safe) panel id on this turn's inflight row →
    ///    [`WatcherCreateDecision::AdoptPersisted`] (adopt, never duplicate);
    /// 2. else if `watcher_should_create_external_input_status_panel`
    ///    (v2 on, no panel yet, external-input/panel-eligible turn) →
    ///    [`WatcherCreateDecision::Create`];
    /// 3. else [`WatcherCreateDecision::None`].
    ///
    /// Pure (no actor round-trip, no ledger read, no IO): the create/adopt
    /// decision is a function of the raw inputs ALONE, so this stays faithfully
    /// shadow-only. The legacy `!watcher_external_input_turn_abandoned` guard is a
    /// separate suppression LAYERED AFTER this decision (it reads turn-tombstone
    /// files); a faithful create/adopt parity compares the publish DECISION before
    /// that suppression, so the abandoned guard is intentionally NOT modeled here.
    /// COMPLETION parity remains deferred to the controller execute-cutover PR:
    /// it needs the SendFallback-aware terminal id from the legacy completion
    /// path, which is only known after the bridge/watcher send resolves.
    pub(in crate::services::discord) fn watcher_create_parity_decision(
        &self,
        status_panel_v2_enabled: bool,
        panel_present: bool,
        inflight_represents_external_input: bool,
        persisted_panel_id: Option<MessageId>,
    ) -> WatcherCreateDecision {
        if let Some(persisted) = persisted_panel_id {
            return WatcherCreateDecision::AdoptPersisted(persisted);
        }
        if status_panel_v2_enabled && !panel_present && inflight_represents_external_input {
            return WatcherCreateDecision::Create;
        }
        WatcherCreateDecision::None
    }

    /// EPIC #3078 PR-3 — the controller's view of "does the live turn still own
    /// THIS EXACT panel?", the gate the orphan-store drain uses to defer a delete
    /// while the live turn's completion path may be finalizing the panel. Must
    /// agree with the legacy inflight-row gate `inflight.status_message_id ==
    /// Some(candidate)`, INCLUDING the channel-only (`user_msg_id == 0`) collapse
    /// for the #3003 turn-aware defer. Computed from the inflight row's CURRENT
    /// panel id (`inflight_panel_id`, the value the legacy gate reads) via a
    /// READ-ONLY query — NOT from whatever (possibly stale) id the in-memory ledger
    /// holds, the divergence the prior `adopt_recovered` seed introduced. Shadow-
    /// only: NO ledger mutation, NO durable write, NO Discord IO — the legacy drain
    /// executes the real defer/delete unchanged.
    pub(in crate::services::discord) async fn orphan_gate_owns_panel(
        &self,
        key: TurnKey,
        _provider: ProviderKind,
        inflight_panel_id: Option<MessageId>,
        candidate: MessageId,
    ) -> bool {
        // Faithful to `inflight_panel_id == Some(candidate)`: read the row's
        // CURRENT panel id back through the read-only query. A row that owns no
        // panel (state file gone → `None`) resolves to `None`, agreeing the live
        // turn does NOT own the orphan — exactly the legacy `None == Some(candidate)`
        // → false outcome — and a terminal controller entry likewise collapses to
        // `None` (the panel was already released, never resurrected here).
        self.orphan_parity_target(key, inflight_panel_id).await == Some(candidate)
    }

    /// READ-ONLY shared helper for the two PR-3 parity gates: the panel id the
    /// live turn owns per the inflight row (`inflight_panel_id`), collapsed to
    /// `None` only when the controller has already finalized/reclaimed this turn's
    /// panel. Never seeds/mutates the ledger and never writes the durable mirror —
    /// the value reflects the inflight row's CURRENT id, faithfully matching the
    /// legacy gates, instead of a possibly-stale in-memory ledger id.
    async fn orphan_parity_target(
        &self,
        key: TurnKey,
        inflight_panel_id: Option<MessageId>,
    ) -> Option<MessageId> {
        let (ack, rx) = oneshot::channel();
        if self
            .tx
            .send(PanelMsg::OrphanParityTarget {
                key,
                inflight_panel_id,
                ack,
            })
            .is_err()
        {
            // Actor not spawned (v2 off) / shut down: fall back to the raw row id,
            // the legacy value, so a v2-off path still computes faithful parity.
            return inflight_panel_id;
        }
        rx.await.unwrap_or(inflight_panel_id)
    }
}

/// Resolve a submission to the ledger entry it acts on, reusing the finalizer's
/// channel-only collapse semantics (`turn_finalizer::resolve_channel_only`).
/// A real `user_msg_id` uses its exact key; a channel-only id-0 terminal
/// (recovery/orphan path) collapses onto the channel's single non-terminal
/// entry, falling back to the literal orphan key when ambiguous (a terminal
/// entry already exists) — identical to the finalizer ledger.
fn resolve_key(ledger: &HashMap<LedgerKey, PanelEntry>, key: TurnKey) -> LedgerKey {
    turn_finalizer::resolve_channel_only(
        key,
        ledger.iter().map(|(lk, e)| (lk, e.phase.is_terminal())),
    )
}

/// The single owning task. Owns the ledger; every public method routes through
/// it so the ledger phase check-and-set needs no synchronization.
async fn actor_loop(mut rx: mpsc::UnboundedReceiver<PanelMsg>) {
    let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();

    while let Some(msg) = rx.recv().await {
        match msg {
            PanelMsg::Register {
                key,
                provider,
                owner,
            } => {
                ledger
                    .entry(key.exact_key())
                    .and_modify(|e| {
                        // Only refresh the owner while still live; never
                        // resurrect a terminal panel.
                        if !e.phase.is_terminal() {
                            e.owner = owner;
                            e.provider = provider.clone();
                        }
                    })
                    .or_insert_with(|| PanelEntry {
                        msg_id: None,
                        owner,
                        provider,
                        phase: PanelPhase::NotCreated,
                        last_text: None,
                    });
            }
            PanelMsg::EnsureCreated {
                key,
                req,
                shared,
                ack,
            } => {
                let id = ensure_created(&mut ledger, key, req, &shared);
                let _ = ack.send(id);
            }
            PanelMsg::StreamUpdate {
                key,
                panel_text,
                shared,
            } => {
                stream_update(&mut ledger, key, panel_text, &shared);
            }
            PanelMsg::Finalize {
                key,
                terminal_text,
                shared,
                ack,
            } => {
                let outcome = commit_terminal(
                    &mut ledger,
                    key,
                    PanelPhase::Completed,
                    terminal_text,
                    &shared,
                );
                let _ = ack.send(outcome);
            }
            PanelMsg::Reclaim {
                key,
                reason,
                shared,
                ack,
            } => {
                let _ = reason;
                let outcome =
                    commit_terminal(&mut ledger, key, PanelPhase::Reclaimed, None, &shared);
                let _ = ack.send(outcome);
            }
            PanelMsg::ClearIfCurrent { key, msg_id, .. } => {
                clear_if_current(&mut ledger, key, msg_id);
            }
            PanelMsg::CurrentPanel { key, ack } => {
                let lk = resolve_key(&ledger, key);
                let id = ledger.get(&lk).and_then(|e| e.msg_id);
                let _ = ack.send(id);
            }
            PanelMsg::OrphanParityTarget {
                key,
                inflight_panel_id,
                ack,
            } => {
                // READ-ONLY: faithful to the legacy gate, which reads the panel id
                // straight from the inflight row. Return that `inflight_panel_id`
                // verbatim so the parity reflects the row's CURRENT id, never a
                // stale ledger seed. The ONLY override is terminal-safety: if the
                // resolved entry is already Completed/Reclaimed, the controller has
                // released this panel, so the live turn no longer owns it → `None`.
                // No ledger mutation, no durable write, no resurrection.
                let lk = resolve_key(&ledger, key);
                let target = match ledger.get(&lk) {
                    Some(e) if e.phase.is_terminal() => None,
                    _ => inflight_panel_id,
                };
                let _ = ack.send(target);
            }
            PanelMsg::AdoptRecovered {
                key,
                provider,
                owner,
                msg_id,
            } => {
                adopt_recovered(&mut ledger, key, provider, owner, msg_id);
            }
        }
    }
}

/// Adopt an already-existing (recovery) panel id into the ledger as the live
/// panel for this turn. Mirrors `Register` but seeds the known `msg_id` and the
/// `Live` phase, since the panel was created by a previous process and only the
/// id needs to be learned. Never resurrects a terminal entry; if a live entry
/// already owns a panel its id is left intact (idempotent re-adopt). No durable
/// write — adoption only learns the in-memory authority; the durable mirror was
/// already bound by the process that created the panel.
fn adopt_recovered(
    ledger: &mut HashMap<LedgerKey, PanelEntry>,
    key: TurnKey,
    provider: ProviderKind,
    owner: PanelOwnerKind,
    msg_id: Option<MessageId>,
) {
    let lk = resolve_key(ledger, key);
    let entry = ledger.entry(lk).or_insert_with(|| PanelEntry {
        msg_id,
        owner,
        provider: provider.clone(),
        phase: PanelPhase::Live,
        last_text: None,
    });
    if entry.phase.is_terminal() {
        // Never reopen a finalized/reclaimed panel.
        return;
    }
    entry.owner = owner;
    entry.provider = provider;
    if entry.msg_id.is_none() {
        entry.msg_id = msg_id;
    }
    if entry.phase == PanelPhase::NotCreated {
        entry.phase = PanelPhase::Live;
    }
}

/// Create a panel if the turn has none, binding the durable mirror via the
/// #3077 typed `bind_status_panel`. Idempotent on a live entry that already
/// owns a panel. PR-1 has no live `PanelSink`, so the actual Discord send is a
/// later-PR routing concern: here we only synthesize/record the authority and
/// mirror it durably. With no sink and no created id the entry stays
/// `NotCreated` (nothing to mirror), which is the dormant behaviour.
fn ensure_created(
    ledger: &mut HashMap<LedgerKey, PanelEntry>,
    key: TurnKey,
    req: PanelCreateRequest,
    shared: &Arc<SharedData>,
) -> Option<MessageId> {
    let lk = resolve_key(ledger, key);
    let entry = ledger.entry(lk).or_insert_with(|| PanelEntry {
        msg_id: None,
        owner: PanelOwnerKind::Bridge,
        provider: ProviderKind::Claude,
        phase: PanelPhase::NotCreated,
        last_text: None,
    });

    // Terminal entries never re-create. A live entry that already owns a panel
    // returns it (idempotent). Only a NotCreated entry would create — but PR-1
    // ships dormant without a `PanelSink`, so there is nothing to send; the
    // request text is recorded as the coalescing baseline so a later routing
    // PR's first stream is correctly deduped.
    if entry.phase.is_terminal() {
        return entry.msg_id;
    }
    if let Some(existing) = entry.msg_id {
        return Some(existing);
    }
    entry.last_text = Some(req.panel_text);
    // DORMANT: no sink wired in PR-1, so no id is created and no durable bind
    // fires. Routing PRs replace this with a real `create` + `bind_status_panel`
    // (see `bind_durable_mirror`). Keep `shared` referenced so the dormant
    // signature matches the routed signature.
    let _ = shared;
    None
}

/// Stream a new rendering into the live panel, coalescing on the last-rendered
/// text. No-op on a terminal or not-yet-created entry. DORMANT: no sink, so the
/// coalesced text is recorded in the authority only.
fn stream_update(
    ledger: &mut HashMap<LedgerKey, PanelEntry>,
    key: TurnKey,
    panel_text: String,
    shared: &Arc<SharedData>,
) {
    let lk = resolve_key(ledger, key);
    let Some(entry) = ledger.get_mut(&lk) else {
        return;
    };
    if entry.phase != PanelPhase::Live {
        return;
    }
    if entry.last_text.as_deref() == Some(panel_text.as_str()) {
        // Coalesced: identical to the last rendering, drop it.
        return;
    }
    entry.last_text = Some(panel_text);
    let _ = shared;
}

/// The exactly-once terminal gate shared by `finalize` (→ `Completed`) and
/// `reclaim` (→ `Reclaimed`). A terminal entry returns `AlreadyTerminal`; a
/// never-created entry returns `NoPanel`. On the one transition it reconciles
/// the durable mirror: `finalize` leaves the bind in place (the panel persists
/// as the turn's record), `reclaim` clears it via #3077
/// `clear_status_panel_if_current`.
fn commit_terminal(
    ledger: &mut HashMap<LedgerKey, PanelEntry>,
    key: TurnKey,
    to: PanelPhase,
    terminal_text: Option<String>,
    shared: &Arc<SharedData>,
) -> PanelCommitOutcome {
    let lk = resolve_key(ledger, key);
    let Some(entry) = ledger.get_mut(&lk) else {
        return PanelCommitOutcome::NoPanel;
    };
    if entry.phase.is_terminal() {
        return PanelCommitOutcome::AlreadyTerminal;
    }
    if entry.phase == PanelPhase::NotCreated && entry.msg_id.is_none() {
        // No panel was ever created. Still gate the phase so a later stray
        // stream cannot resurrect it, but report NoPanel so the caller does no
        // panel bookkeeping.
        entry.phase = to;
        return PanelCommitOutcome::NoPanel;
    }

    let msg_id = entry.msg_id;
    let provider = entry.provider.clone();
    if let Some(text) = terminal_text {
        entry.last_text = Some(text);
    }
    entry.phase = to;

    // Reconcile the durable mirror. Reclaim clears it (compare-and-clear so a
    // newer turn's rebind is never wiped); finalize leaves the bind so the
    // panel remains the turn's durable record. DORMANT: still routed through
    // the #3077 typed op so it is the controller's ONLY persistence call.
    let _ = shared;
    if to == PanelPhase::Reclaimed
        && let Some(id) = msg_id
    {
        clear_durable_mirror(&provider, lk.channel_id, id);
    }

    PanelCommitOutcome::Committed { msg_id }
}

/// Compare-and-clear the ledger id, ONLY when it currently equals `msg_id`.
/// Also clears the durable mirror via #3077 `clear_status_panel_if_current`.
fn clear_if_current(ledger: &mut HashMap<LedgerKey, PanelEntry>, key: TurnKey, msg_id: MessageId) {
    let lk = resolve_key(ledger, key);
    let Some(entry) = ledger.get_mut(&lk) else {
        return;
    };
    if entry.msg_id != Some(msg_id) {
        // A newer turn already rebound the panel — do NOT clear it.
        return;
    }
    let provider = entry.provider.clone();
    entry.msg_id = None;
    clear_durable_mirror(&provider, lk.channel_id, msg_id);
}

/// Internal: durable bind via the #3077 typed op. The controller's ONLY bind
/// write. Reserved for routing PRs once a `PanelSink` produces a created id;
/// kept here so the persistence surface lives entirely in this file.
#[allow(dead_code)]
fn bind_durable_mirror(provider: &ProviderKind, channel_id: ChannelId, msg_id: MessageId) {
    let guard = super::inflight::StatusPanelBindGuard::default();
    let _ = super::inflight::bind_status_panel(provider, channel_id.get(), msg_id.get(), &guard);
}

/// Internal: durable clear via the #3077 typed op. The controller's ONLY clear
/// write — compare-and-clear so a newer turn's rebound panel is never wiped.
fn clear_durable_mirror(provider: &ProviderKind, channel_id: ChannelId, msg_id: MessageId) {
    let guard = super::inflight::StatusPanelClearGuard::default();
    let _ = super::inflight::clear_status_panel_if_current(
        provider,
        channel_id.get(),
        msg_id.get(),
        &guard,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lk(ch: u64, user: u64) -> LedgerKey {
        TurnKey::new(ChannelId::new(ch), user, 0).exact_key()
    }

    fn live_entry(ch: u64, user: u64, id: u64) -> (LedgerKey, PanelEntry) {
        (
            lk(ch, user),
            PanelEntry {
                msg_id: Some(MessageId::new(id)),
                owner: PanelOwnerKind::Bridge,
                provider: ProviderKind::Claude,
                phase: PanelPhase::Live,
                last_text: None,
            },
        )
    }

    /// finalize after finalize is idempotent: the first commits, the second is
    /// AlreadyTerminal (the exactly-once gate), and the phase stays Completed.
    #[test]
    fn finalize_after_finalize_is_idempotent() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(1, 10, 100);
        ledger.insert(k, e);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let key = TurnKey::new(ChannelId::new(1), 10, 0);

        let first = commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        assert_eq!(
            first,
            PanelCommitOutcome::Committed {
                msg_id: Some(MessageId::new(100))
            }
        );
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Completed);

        let second = commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        assert_eq!(second, PanelCommitOutcome::AlreadyTerminal);
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Completed);
    }

    /// reclaim after finalize is a no-op (AlreadyTerminal) and does NOT flip
    /// the phase to Reclaimed — finalize won.
    #[test]
    fn reclaim_after_finalize_is_noop() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(2, 20, 200);
        ledger.insert(k, e);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let key = TurnKey::new(ChannelId::new(2), 20, 0);

        let fin = commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        assert!(matches!(fin, PanelCommitOutcome::Committed { .. }));

        let rec = commit_terminal(&mut ledger, key, PanelPhase::Reclaimed, None, &shared);
        assert_eq!(rec, PanelCommitOutcome::AlreadyTerminal);
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Completed);
    }

    /// finalize after reclaim is a no-op (AlreadyTerminal) — reclaim won (the
    /// vice-versa case).
    #[test]
    fn finalize_after_reclaim_is_noop() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(3, 30, 300);
        ledger.insert(k, e);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let key = TurnKey::new(ChannelId::new(3), 30, 0);

        let rec = commit_terminal(&mut ledger, key, PanelPhase::Reclaimed, None, &shared);
        assert!(matches!(rec, PanelCommitOutcome::Committed { .. }));

        let fin = commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        assert_eq!(fin, PanelCommitOutcome::AlreadyTerminal);
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Reclaimed);
    }

    /// `clear_if_current` clears ONLY when the ledger id matches (TOCTOU
    /// compare-and-clear); a stale id leaves a newer turn's panel intact.
    #[test]
    fn clear_if_current_compare_and_clear() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(4, 40, 400);
        ledger.insert(k, e);
        let key = TurnKey::new(ChannelId::new(4), 40, 0);

        // Stale id (a newer turn rebound the row to 400): clearing 999 is a
        // no-op, the panel stays 400.
        clear_if_current(&mut ledger, key, MessageId::new(999));
        assert_eq!(ledger.get(&k).unwrap().msg_id, Some(MessageId::new(400)));

        // The matching id clears.
        clear_if_current(&mut ledger, key, MessageId::new(400));
        assert_eq!(ledger.get(&k).unwrap().msg_id, None);
    }

    /// `stream_update` coalesces: a text identical to the last-rendered text is
    /// dropped, and a stream on a non-Live entry is a no-op.
    #[test]
    fn stream_update_coalesces_and_gates_on_live() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(5, 50, 500);
        ledger.insert(k, e);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let key = TurnKey::new(ChannelId::new(5), 50, 0);

        stream_update(&mut ledger, key, "alpha".to_string(), &shared);
        assert_eq!(ledger.get(&k).unwrap().last_text.as_deref(), Some("alpha"));

        // Identical text is coalesced (still "alpha"); distinct text updates.
        stream_update(&mut ledger, key, "alpha".to_string(), &shared);
        assert_eq!(ledger.get(&k).unwrap().last_text.as_deref(), Some("alpha"));
        stream_update(&mut ledger, key, "beta".to_string(), &shared);
        assert_eq!(ledger.get(&k).unwrap().last_text.as_deref(), Some("beta"));

        // After finalize the entry is terminal → stream is a no-op.
        commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        stream_update(&mut ledger, key, "gamma".to_string(), &shared);
        assert_eq!(ledger.get(&k).unwrap().last_text.as_deref(), Some("beta"));
    }

    /// `register` is idempotent: a second register on a live entry only
    /// refreshes the owner and never resurrects a terminal panel.
    #[tokio::test(flavor = "current_thread")]
    async fn register_is_idempotent_and_never_resurrects() {
        // Drive register through the actor loop so the idempotency path
        // (and_modify vs or_insert) is exercised as written.
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(6), 60, 0);

        ctl.register(key, ProviderKind::Claude, PanelOwnerKind::Bridge);
        ctl.register(key, ProviderKind::Claude, PanelOwnerKind::Watcher);
        // No panel was created, so current_panel is None; the point is that the
        // double register did not panic and the actor processed both.
        assert_eq!(ctl.current_panel(key).await, None);

        // Finalize a never-created entry → NoPanel, phase gated terminal.
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let outcome = ctl.finalize(key, None, shared.clone()).await;
        assert_eq!(outcome, PanelCommitOutcome::NoPanel);
        // A register after the terminal must not resurrect it: a follow-up
        // finalize still reports NoPanel/AlreadyTerminal, never Committed.
        ctl.register(key, ProviderKind::Claude, PanelOwnerKind::Recovery);
        let again = ctl.finalize(key, None, shared).await;
        assert!(matches!(
            again,
            PanelCommitOutcome::AlreadyTerminal | PanelCommitOutcome::NoPanel
        ));
    }

    /// EPIC #3078 PR-2: `adopt_recovered` seeds an existing (recovery) panel id
    /// into the ledger as Live, is idempotent (a second adopt keeps the first
    /// id), and never resurrects a terminal entry.
    #[test]
    fn adopt_recovered_seeds_idempotent_and_never_resurrects() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let key = TurnKey::new(ChannelId::new(8), 80, 0);
        let k = key.exact_key();

        adopt_recovered(
            &mut ledger,
            key,
            ProviderKind::Claude,
            PanelOwnerKind::Recovery,
            Some(MessageId::new(800)),
        );
        let entry = ledger.get(&k).unwrap();
        assert_eq!(entry.msg_id, Some(MessageId::new(800)));
        assert_eq!(entry.phase, PanelPhase::Live);
        assert_eq!(entry.owner, PanelOwnerKind::Recovery);

        // Idempotent: a second adopt keeps the already-bound id (does not clobber
        // with a stale id) while still refreshing the owner.
        adopt_recovered(
            &mut ledger,
            key,
            ProviderKind::Claude,
            PanelOwnerKind::Watcher,
            Some(MessageId::new(999)),
        );
        let entry = ledger.get(&k).unwrap();
        assert_eq!(entry.msg_id, Some(MessageId::new(800)));
        assert_eq!(entry.owner, PanelOwnerKind::Watcher);

        // Non-resurrecting: after a terminal commit, adopt is a no-op on phase.
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        commit_terminal(&mut ledger, key, PanelPhase::Completed, None, &shared);
        adopt_recovered(
            &mut ledger,
            key,
            ProviderKind::Claude,
            PanelOwnerKind::Recovery,
            Some(MessageId::new(801)),
        );
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Completed);
    }

    /// EPIC #3078 PR-2: the shadow parity helper adopts the recovered id and
    /// reports it back as the controller's chosen completion id, exercised
    /// through the live actor loop.
    #[tokio::test(flavor = "current_thread")]
    async fn recovery_completion_parity_id_reports_adopted_id() {
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(9), 90, 0);
        let chosen = ctl
            .recovery_completion_parity_id(key, ProviderKind::Claude, Some(MessageId::new(900)))
            .await;
        assert_eq!(chosen, Some(MessageId::new(900)));

        // A None recovered id (no panel) reports None.
        let key2 = TurnKey::new(ChannelId::new(9), 91, 0);
        let none = ctl
            .recovery_completion_parity_id(key2, ProviderKind::Claude, None)
            .await;
        assert_eq!(none, None);
    }

    /// EPIC #3078 PR-3: the sweeper-reclaim shadow helper adopts the persisted
    /// orphan panel id and reports it back as the controller's chosen reclaim
    /// target (parity with the legacy `panel_reclaim_target`), and reports `None`
    /// for a row that carries no panel id.
    #[tokio::test(flavor = "current_thread")]
    async fn sweeper_reclaim_parity_id_reports_adopted_id() {
        let ctl = StatusPanelController::spawn(true);

        // A row carrying a real persisted panel id → that id is the reclaim target.
        let key = TurnKey::new(ChannelId::new(11), 110, 0);
        let chosen = ctl
            .sweeper_reclaim_parity_id(key, ProviderKind::Claude, Some(MessageId::new(1100)))
            .await;
        assert_eq!(chosen, Some(MessageId::new(1100)));

        // A row with no persisted panel id → no reclaim target.
        let key2 = TurnKey::new(ChannelId::new(11), 111, 0);
        let none = ctl
            .sweeper_reclaim_parity_id(key2, ProviderKind::Claude, None)
            .await;
        assert_eq!(none, None);
    }

    /// EPIC #3078 PR-3: the orphan-store drain gate helper agrees with the legacy
    /// inflight-row gate — it returns `true` only when the live turn still owns the
    /// EXACT orphan panel, including the channel-only (`user_msg_id == 0`) collapse
    /// for the #3003 turn-aware defer, and `false` when the row owns a different
    /// panel or no panel at all (state file gone).
    #[tokio::test(flavor = "current_thread")]
    async fn orphan_gate_owns_panel_agrees_with_inflight_row() {
        // Live turn owns THIS exact panel → defer (true), matching
        // `inflight.status_message_id == Some(candidate)`.
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(12), 120, 0);
        let owns = ctl
            .orphan_gate_owns_panel(
                key,
                ProviderKind::Claude,
                Some(MessageId::new(1200)),
                MessageId::new(1200),
            )
            .await;
        assert!(owns);

        // Live turn owns a DIFFERENT panel → do not defer (false), matching
        // `Some(other) == Some(candidate)` → false.
        let ctl2 = StatusPanelController::spawn(true);
        let key2 = TurnKey::new(ChannelId::new(12), 121, 0);
        let owns2 = ctl2
            .orphan_gate_owns_panel(
                key2,
                ProviderKind::Claude,
                Some(MessageId::new(9999)),
                MessageId::new(1200),
            )
            .await;
        assert!(!owns2);

        // No live row / state file gone (`None`) → do not defer (false), matching
        // `None == Some(candidate)` → false. The orphan store is the only reclaim
        // path here, so the drain must NOT defer forever.
        let ctl3 = StatusPanelController::spawn(true);
        let key3 = TurnKey::new(ChannelId::new(12), 122, 0);
        let owns3 = ctl3
            .orphan_gate_owns_panel(key3, ProviderKind::Claude, None, MessageId::new(1200))
            .await;
        assert!(!owns3);

        // Channel-only (`user_msg_id == 0`) recovery/orphan key collapses onto the
        // adopted live entry → still recognizes ownership of the exact panel.
        let ctl4 = StatusPanelController::spawn(true);
        let key4 = TurnKey::new(ChannelId::new(13), 0, 0);
        let owns4 = ctl4
            .orphan_gate_owns_panel(
                key4,
                ProviderKind::Claude,
                Some(MessageId::new(1300)),
                MessageId::new(1300),
            )
            .await;
        assert!(owns4);
    }

    /// EPIC #3078 PR-3 regression: a STALE different id already in the controller
    /// ledger for the key must NOT leak into the parity decision. The legacy gate
    /// compares the inflight row's CURRENT id; the prior `adopt_recovered` seed read
    /// the stale ledger id back instead and diverged. Both PR-3 parity helpers must
    /// now compute from the passed-in inflight-row id, agreeing with legacy.
    #[tokio::test(flavor = "current_thread")]
    async fn parity_uses_inflight_row_id_not_stale_ledger() {
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(20), 200, 0);

        // Plant a STALE id in the ledger (a prior turn's panel the controller still
        // holds in memory). `adopt_recovered` is the only public seed path.
        ctl.adopt_recovered(
            key,
            ProviderKind::Claude,
            PanelOwnerKind::Standby,
            Some(MessageId::new(7777)),
        );
        assert_eq!(ctl.current_panel(key).await, Some(MessageId::new(7777)));

        // The inflight row now owns a DIFFERENT (current) panel id 8888. Legacy:
        // `Some(8888) == Some(8888)` → true; `Some(8888) == Some(7777)` → false.
        // The fix must reflect the row id 8888, NOT the stale 7777.
        let owns_current = ctl
            .orphan_gate_owns_panel(
                key,
                ProviderKind::Claude,
                Some(MessageId::new(8888)),
                MessageId::new(8888),
            )
            .await;
        assert!(owns_current, "gate must own the inflight row's CURRENT id");

        let owns_stale = ctl
            .orphan_gate_owns_panel(
                key,
                ProviderKind::Claude,
                Some(MessageId::new(8888)),
                MessageId::new(7777),
            )
            .await;
        assert!(
            !owns_stale,
            "gate must NOT own the stale ledger id once the row moved on"
        );

        // Sweeper reclaim target is the row's CURRENT id, not the stale ledger id.
        let reclaim = ctl
            .sweeper_reclaim_parity_id(key, ProviderKind::Claude, Some(MessageId::new(8888)))
            .await;
        assert_eq!(reclaim, Some(MessageId::new(8888)));

        // The query is READ-ONLY: the stale ledger id is untouched (no seed/clobber).
        assert_eq!(ctl.current_panel(key).await, Some(MessageId::new(7777)));
    }

    /// EPIC #3078 PR-3: a Completed/Reclaimed terminal entry is NOT resurrected by
    /// the parity query; instead the live turn is reported as no longer owning the
    /// panel (`None`), and the terminal phase is left intact.
    #[tokio::test(flavor = "current_thread")]
    async fn parity_query_never_resurrects_terminal_entry() {
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(21), 210, 0);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);

        // Adopt a live panel, then finalize it (terminal Completed).
        ctl.adopt_recovered(
            key,
            ProviderKind::Claude,
            PanelOwnerKind::Standby,
            Some(MessageId::new(9100)),
        );
        let outcome = ctl.finalize(key, None, shared).await;
        assert!(matches!(outcome, PanelCommitOutcome::Committed { .. }));

        // The inflight row may still carry 9100, but the controller already
        // finalized it → the parity query collapses to None (released), and the
        // gate does not defer.
        let target = ctl
            .sweeper_reclaim_parity_id(key, ProviderKind::Claude, Some(MessageId::new(9100)))
            .await;
        assert_eq!(target, None, "terminal entry → no live reclaim target");

        let owns = ctl
            .orphan_gate_owns_panel(
                key,
                ProviderKind::Claude,
                Some(MessageId::new(9100)),
                MessageId::new(9100),
            )
            .await;
        assert!(!owns, "terminal entry must not be reported as live-owned");

        // A follow-up finalize is still AlreadyTerminal — the query never reopened
        // the entry.
        let shared2 = super::super::make_shared_data_for_tests_with_storage(None, None);
        let again = ctl.finalize(key, None, shared2).await;
        assert_eq!(again, PanelCommitOutcome::AlreadyTerminal);
    }

    /// EPIC #3078: the watcher CREATE/ADOPT decision is re-derived from RAW
    /// inputs and is faithful to the legacy branch order — persisted id wins
    /// (adopt), else an eligible external-input turn with no panel creates, else
    /// nothing. Mirrors `watcher_should_create_external_input_status_panel`'s
    /// truth table plus the persisted-adopt branch above it.
    #[test]
    fn watcher_create_decision_is_faithful_to_legacy_branch_order() {
        let ctl = StatusPanelController::spawn(false);
        let persisted = MessageId::new(1234);

        // 1. A persisted (restart-safe) id wins regardless of the other inputs:
        //    adopt it, never create a duplicate.
        assert_eq!(
            ctl.watcher_create_parity_decision(true, false, true, Some(persisted)),
            WatcherCreateDecision::AdoptPersisted(persisted)
        );
        // Persisted still wins even when v2 is off / not external-input (the
        // legacy branch order checks `persisted_panel_msg_id` first).
        assert_eq!(
            ctl.watcher_create_parity_decision(false, true, false, Some(persisted)),
            WatcherCreateDecision::AdoptPersisted(persisted)
        );

        // 2. No persisted id, v2 on, no live panel, external-input turn → Create.
        assert_eq!(
            ctl.watcher_create_parity_decision(true, false, true, None),
            WatcherCreateDecision::Create
        );

        // 3a. v2 off → None (the `status_panel_v2_enabled` gate).
        assert_eq!(
            ctl.watcher_create_parity_decision(false, false, true, None),
            WatcherCreateDecision::None
        );
        // 3b. A panel already exists → None (no duplicate).
        assert_eq!(
            ctl.watcher_create_parity_decision(true, true, true, None),
            WatcherCreateDecision::None
        );
        // 3c. Not an external-input/panel-eligible turn → None (the Discord
        //     intake path owns the panel for those turns).
        assert_eq!(
            ctl.watcher_create_parity_decision(true, false, false, None),
            WatcherCreateDecision::None
        );
    }

    /// A channel-only (`user_msg_id == 0`) finalize collapses onto the single
    /// live entry registered with a real id — reusing the finalizer's
    /// `resolve_channel_only` semantics — so the recovery/orphan path finalizes
    /// the registered turn.
    #[test]
    fn channel_only_collapses_onto_live_entry() {
        let mut ledger: HashMap<LedgerKey, PanelEntry> = HashMap::new();
        let (k, e) = live_entry(7, 70, 700);
        ledger.insert(k, e);
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let channel_only = TurnKey::new(ChannelId::new(7), 0, 0);

        let outcome = commit_terminal(
            &mut ledger,
            channel_only,
            PanelPhase::Completed,
            None,
            &shared,
        );
        assert_eq!(
            outcome,
            PanelCommitOutcome::Committed {
                msg_id: Some(MessageId::new(700))
            }
        );
        assert_eq!(ledger.get(&k).unwrap().phase, PanelPhase::Completed);
    }
}
