//! Idempotency helpers for recovery's no-anchor terminal-text delivery.

use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

use super::super::{formatting, recovery_paths};
use super::RecoveryRelayOutcome;
// #5071 T1 S5: the family's ONE door to the `#[cfg(unix)]` delivery journal.
// Imported under its own name, never aliased, so every call site below reads the
// platform bound in the identifier itself.
use super::unix_journal;
use crate::services::discord::inflight::{opt_channel_id, opt_message_id};
use crate::services::discord::outbound::{delivery_frontier_probe, delivery_record};
use crate::services::discord::{
    DELIVERY_LEASE_DEADLINE_MS, DeliveryLeaseCell, DeliveryLeaseHeartbeat, DeliveryLeaseKey,
    LeaseHolder, LeaseOutcome, SharedData, inflight, lease_now_ms,
};
use crate::services::provider::ProviderKind;

#[derive(Clone)]
pub(in crate::services::discord) struct RecoveryDeliveryContext {
    provider: ProviderKind,
    channel_id: ChannelId,
    record_channel_id: ChannelId,
    tmux_session_name: Option<String>,
    lease_key: DeliveryLeaseKey,
    identity: inflight::InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_current_msg_id: u64,
    durable_range: Option<(u64, u64)>,
    /// #4188: current transcript (output_path) byte length, snapshotted from the
    /// inflight state at construction. Bounds the durable frontier so a stale
    /// prior-generation/compaction frontier whose end exceeds the current
    /// transcript EOF is distrusted. `None` when the state has no output_path or
    /// it cannot be stat'd → fail-safe distrust.
    current_output_eof: Option<u64>,
    attempts: u32,
    reuse_recorded_anchor: bool,
    expected_gone_anchor: Option<(u64, u64)>,
    /// #4564: the inbound `user_msg_id` this turn answers, copied from the
    /// inflight state. Handed to the `shadow_mirror_delivered_frontier` funnel
    /// as its `ledger_user_msg_id`, which appends it to the completed-turn
    /// ledger. `0` (synthetic/no-inbound turn) is a no-op sentinel, filtered by
    /// the funnel's `pinned_ledger_user_msg_id` exactly as the pre-#5071-T1-S7
    /// local append filtered it.
    ///
    /// #5071 T1 S7 REPLACED THE SENTENCE THAT USED TO BE HERE: "this recovery
    /// path bypasses the `shadow_mirror_delivered_frontier` funnel, so the
    /// append is wired here too". It no longer bypasses it, and the append is no
    /// longer wired here — it is the funnel's, in the funnel's order.
    user_msg_id: u64,
    /// #5071 T1 S7 (D2): the relay frontier RESET INCARNATION observed for
    /// `record_channel_id` when this recovery decision was taken — i.e. at the
    /// same moment `durable_range`, `identity` and `expected_turn_start_offset`
    /// were snapshotted out of the inflight state, and before any transport.
    ///
    /// This is the recovery analogue of the watcher's lease-time
    /// `lease_reset_incarnation`. `record_durable_frontier` re-presents it to
    /// `acquire_relay_frontier_mutation_for_incarnation`; a `None` there means a
    /// watermark reset landed between the snapshot and the durable write, so the
    /// bytes this delivery describes are no longer from the source that was
    /// captured. The capture point is what gives the check any power at all —
    /// reading the incarnation immediately before the write could never fail.
    frontier_reset_incarnation: u64,
}

/// #5071 T1 S7: which of the two anchor decisions built this context. It exists
/// so `from_state_for_channel` stays inside the argument-count lint without an
/// `#[allow]` (the structural clippy ratchet, #4519, counts those per file), and
/// it makes the pairing explicit: the two call sites always passed
/// `(true, None)` and `(false, Some(..))`, never any other combination.
///
/// The struct keeps BOTH fields separately all the same, because
/// `resolve_durable_identity` still refuses a `ReplaceProvenGone` context whose
/// anchor is absent — a combination this enum cannot express but the fields can,
/// and that refusal is not this slice's to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryAnchorMode {
    ReuseRecorded,
    ReplaceProvenGone((u64, u64)),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RecoveryAnchorReuse {
    DurableAlreadyDelivered(MessageId),
    InflightAnchor(MessageId),
}

impl RecoveryDeliveryContext {
    pub(in crate::services::discord) fn from_state(
        shared: &SharedData,
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
    ) -> Option<Self> {
        let channel_id = opt_channel_id(state.channel_id)?;
        Some(Self::from_state_for_channel(
            shared,
            provider,
            state,
            channel_id,
            durable_range,
            delivery_generation,
            RecoveryAnchorMode::ReuseRecorded,
        ))
    }

    pub(in crate::services::discord) fn send_new_after_gone_anchor(
        shared: &SharedData,
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        channel_id: ChannelId,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
        expected_gone_anchor: (u64, u64),
    ) -> Self {
        Self::from_state_for_channel(
            shared,
            provider,
            state,
            channel_id,
            durable_range,
            delivery_generation,
            RecoveryAnchorMode::ReplaceProvenGone(expected_gone_anchor),
        )
    }

    pub(in crate::services::discord) fn with_record_channel_id(
        mut self,
        record_channel_id: ChannelId,
    ) -> Self {
        self.record_channel_id = record_channel_id;
        self
    }

    fn from_state_for_channel(
        shared: &SharedData,
        provider: &ProviderKind,
        state: &inflight::InflightTurnState,
        channel_id: ChannelId,
        durable_range: Option<(u64, u64)>,
        delivery_generation: u64,
        anchor_mode: RecoveryAnchorMode,
    ) -> Self {
        let (reuse_recorded_anchor, expected_gone_anchor) = match anchor_mode {
            RecoveryAnchorMode::ReuseRecorded => (true, None),
            RecoveryAnchorMode::ReplaceProvenGone(anchor) => (false, Some(anchor)),
        };
        let record_channel_id =
            opt_channel_id(state.delivery_record_owner_channel_id()).unwrap_or(channel_id);
        Self {
            provider: provider.clone(),
            channel_id,
            record_channel_id,
            tmux_session_name: state.tmux_session_name.clone(),
            lease_key: DeliveryLeaseKey::from_inflight_state_for_site(
                channel_id,
                delivery_generation,
                state,
                "recovery.no_anchor",
            ),
            identity: inflight::InflightTurnIdentity::from_state(state),
            expected_turn_start_offset: state.turn_start_offset,
            expected_current_msg_id: state.current_msg_id,
            durable_range,
            current_output_eof: state
                .output_path
                .as_deref()
                .and_then(|path| std::fs::metadata(path).ok().map(|meta| meta.len())),
            attempts: state.recovery_relay_attempts,
            reuse_recorded_anchor,
            expected_gone_anchor,
            user_msg_id: state.user_msg_id,
            // Keyed by `record_channel_id`, the OFFSET-AUTHORITY channel: that is
            // the coord whose watermark a reset would rewind and the channel the
            // durable record is keyed by. `with_record_channel_id` can move that
            // key afterwards, which is why the re-presentation in
            // `record_durable_frontier` reads `self.record_channel_id` and not a
            // second copy captured here.
            frontier_reset_incarnation: shared
                .relay_frontier_token(record_channel_id)
                .reset_incarnation,
        }
    }

    pub(in crate::services::discord) fn anchor_reuse_decision(
        &self,
    ) -> Option<RecoveryAnchorReuse> {
        if !self.reuse_recorded_anchor {
            return None;
        }
        if let Some(anchor) = self.durable_recorded_anchor().and_then(opt_message_id) {
            return Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor));
        }
        self.inflight_recorded_anchor()
            .and_then(opt_message_id)
            .map(RecoveryAnchorReuse::InflightAnchor)
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn recorded_anchor(&self) -> Option<MessageId> {
        match self.anchor_reuse_decision() {
            Some(RecoveryAnchorReuse::DurableAlreadyDelivered(anchor))
            | Some(RecoveryAnchorReuse::InflightAnchor(anchor)) => Some(anchor),
            None => None,
        }
    }

    fn durable_recorded_anchor(&self) -> Option<u64> {
        let range = self.durable_range?;
        let tmux_session_name = self.tmux_session_name.as_deref()?;
        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &self.provider,
            self.record_channel_id,
            tmux_session_name,
            self.current_output_eof,
        )?;
        (anchor.panel_channel_id == self.channel_id.get() && anchor.range == range)
            .then_some(anchor.panel_msg_id)
    }

    fn inflight_recorded_anchor(&self) -> Option<u64> {
        inflight::recovery_anchor_msg_id_if_matches_identity(
            &self.provider,
            self.channel_id.get(),
            &self.identity,
            self.expected_turn_start_offset,
        )
    }

    pub(in crate::services::discord) fn try_acquire_fresh_send_lease(
        &self,
        shared: &Arc<SharedData>,
        text: &str,
    ) -> Option<RecoveryFreshSendLease> {
        let cell = shared.delivery_lease(self.channel_id);
        cell.reclaim_if_expired(lease_now_ms());
        let (start, end) = self.lease_range(text);
        let holder = LeaseHolder::Sink;
        let deadline = lease_now_ms().saturating_add(DELIVERY_LEASE_DEADLINE_MS);
        if !cell.try_acquire(self.lease_key.clone(), holder, start, end, deadline) {
            return None;
        }
        Some(RecoveryFreshSendLease {
            cell: cell.clone(),
            holder,
            key: self.lease_key.clone(),
            start,
            end,
            heartbeat: Some(DeliveryLeaseHeartbeat::spawn(
                cell,
                holder,
                self.lease_key.clone(),
            )),
            released: false,
        })
    }

    fn lease_range(&self, text: &str) -> (u64, u64) {
        if let Some((start, end)) = self.durable_range {
            if end > start {
                return (start, end);
            }
        }
        let start = self.expected_turn_start_offset.unwrap_or(0);
        let width = u64::try_from(text.len().max(1)).unwrap_or(u64::MAX);
        (start, start.saturating_add(width))
    }

    /// The leased no-anchor fresh send confirmed its POST
    /// (`relay_no_anchor_terminal_text`).
    pub(in crate::services::discord) fn record_successful_fresh_send_after_no_anchor_post(
        &self,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
    ) {
        self.record_successful_fresh_send(
            shared,
            anchor,
            text,
            unix_journal::Disposition::NoAnchorFreshSend,
        );
    }

    /// The anchored replace fell back to a POST after its edit failed
    /// (`replace_anchored_terminal_text`).
    pub(in crate::services::discord) fn record_successful_fresh_send_after_anchored_edit_fallback(
        &self,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
    ) {
        self.record_successful_fresh_send(
            shared,
            anchor,
            text,
            unix_journal::Disposition::AnchoredEditFallback,
        );
    }

    /// The same edit-failure fallback, driven through the turn-output controller
    /// (`recovery_paths/controller_cutover.rs`).
    ///
    /// The three entry points exist so the journal's disposition never appears in
    /// a caller's signature: `controller_cutover.rs` sits in a subtree that is not
    /// `#[cfg(unix)]`, and naming a journal type there would reopen exactly the
    /// `E0433` windows break #5071 T1 S4 landed and then had to fix.
    pub(in crate::services::discord) fn record_successful_fresh_send_after_controller_edit_fallback(
        &self,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
    ) {
        self.record_successful_fresh_send(
            shared,
            anchor,
            text,
            unix_journal::Disposition::ControllerEditFallback,
        );
    }

    /// The family's single confirmed-delivery funnel. Every caller has already
    /// seen Discord accept the message; what is still unknown is whether the
    /// durable frontier advances, which is the whole of what #5071 T1 S5 observes.
    ///
    /// The observation opens here and not before the transport: two of the three
    /// entry points only learn that they advance the frontier at all from the edit
    /// transport's own answer. `session_relay_sink/journal/recovery.rs` records
    /// what that costs — this family can never see a delivery lost mid-POST.
    fn record_successful_fresh_send(
        &self,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
        disposition: unix_journal::Disposition,
    ) {
        let mut observation = unix_journal::begin_recovery_terminal(
            shared,
            &self.provider,
            disposition,
            (self.record_channel_id, self.channel_id),
            self.tmux_session_name.as_deref(),
            self.expected_current_msg_id,
            self.durable_range,
        );
        let bind = inflight::bind_recovery_anchor_if_matches_identity(
            &self.provider,
            self.channel_id.get(),
            &self.identity,
            self.expected_turn_start_offset,
            self.expected_current_msg_id,
            None,
            anchor.get(),
            text.len(),
            None,
            None,
        );
        if matches!(
            bind,
            inflight::GuardedSaveOutcome::Saved | inflight::GuardedSaveOutcome::Missing
        ) {
            let settlement = self.record_durable_frontier(shared, anchor, text);
            unix_journal::settle_recovery_terminal(&mut observation, Some(anchor), settlement);
        } else {
            unix_journal::settle_recovery_terminal(
                &mut observation,
                Some(anchor),
                unix_journal::Settlement::AnchorBindNotPersisted,
            );
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                anchor_msg_id = anchor.get(),
                outcome = ?bind,
                "recovery no-anchor delivery: inflight anchor bind did not persist; skipping durable anchor write"
            );
        }
        // Backstop, single-use: both branches above already closed the
        // obligation, so this appends nothing today. It is here so a future early
        // return added to this fn leaves a `U` rather than a dangling `O`+`A`.
        unix_journal::settle_recovery_terminal(
            &mut observation,
            Some(anchor),
            unix_journal::Settlement::DeliveryNotRecorded,
        );
    }

    /// Returns the funnel's own verdict about the durable frontier so the caller
    /// can settle the journal obligation with it.
    ///
    /// #5071 T1 S7 JOINED THIS TO THE SHADOW-MIRROR FUNNEL. It used to call
    /// `delivery_record::write_delivered_frontier` /
    /// `write_proven_gone_equal_range_frontier` directly and append the
    /// completed-turn ledger itself, ahead of both. Those three calls are gone,
    /// replaced by one `delivery_record::record_recovery_terminal_delivery`.
    /// What changed for these three writes, and nothing is claimed beyond them:
    ///
    ///   D2  a relay frontier mutation admission is now taken for the
    ///       incarnation captured when the recovery decision was made;
    ///   D3  the exact-receipt decision is now the funnel's
    ///       `exact_receipt_from_inflight`. That predicate wants the fresh
    ///       inflight row's `turn_start_offset` to equal `range.0`, while this
    ///       path builds its range as `(state.last_offset, confirmed_end)`, so
    ///       the receipt-less arm remains the expected outcome. The DECISION
    ///       moved; no receipt was created;
    ///   D4  the delivered-content fingerprint (#4081) is recorded. Before S7
    ///       this path wrote none at all;
    ///   D5  the completed-turn ledger append now happens AFTER a successful
    ///       frontier persist, and on the unknown-generation path instead of it
    ///       — the funnel's own separation of settlement from persistence,
    ///       which is what #4564 wanted from the pre-S7 leading append.
    ///
    /// D1 (`WatcherFrontierLockAuthority`) is NOT joined and this is deliberate:
    /// that authority compares the caller's generation to the IN-MEMORY
    /// `confirmed_end_generation_mtime_ns`, which the recovery path never
    /// advances, so presenting one would refuse startup-recovery writes outright.
    /// D6 (advancing the in-memory watermark) is untouched for the same reason —
    /// both belong to #5071 T1 S7b.
    ///
    /// ONE BEHAVIOUR NARROWED, STATED RATHER THAN BURIED. Pre-S7 the ledger
    /// append ran before the durable write and therefore also ran when that write
    /// returned `Err`. Under the funnel a failed persist returns before the
    /// append, exactly as it does for every other funnel caller.
    fn record_durable_frontier(
        &self,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
    ) -> unix_journal::Settlement {
        // D2. Held across the funnel call below so a watermark reset cannot land
        // between admission and the durable write. `None` means one already
        // landed since this recovery decision snapshotted its range and identity.
        let admission = shared.acquire_relay_frontier_mutation_for_incarnation(
            self.record_channel_id,
            self.frontier_reset_incarnation,
        );
        let recordable = if admission.is_some() {
            self.resolve_durable_identity()
        } else {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                record_channel = self.record_channel_id.get(),
                captured_reset_incarnation = self.frontier_reset_incarnation,
                "recovery no-anchor delivery: relay frontier reset after the recovery decision; refusing the durable frontier"
            );
            Err(unix_journal::Settlement::FrontierResetDuringDelivery)
        };
        // EVERY arm reaches the funnel, refusals included, and a refusal reaches
        // it carrying the unknown-generation sentinel `0`. That is what keeps the
        // #4564 settlement: the funnel appends the completed-turn ledger on its
        // unknown-generation branch, which is where the pre-S7 unconditional
        // leading append has moved to.
        let (range, generation_mtime_ns) = recordable.unwrap_or(((0, 0), 0));
        let persisted = delivery_record::record_recovery_terminal_delivery(
            shared,
            &self.provider,
            self.record_channel_id,
            self.tmux_session_name.as_deref(),
            delivery_record::RecoveryDeliveryRecordAuthority {
                generation_mtime_ns,
                attempts: self.attempts,
                expected_gone_anchor: if self.reuse_recorded_anchor {
                    None
                } else {
                    self.expected_gone_anchor
                },
                range,
                terminal_anchor: (self.channel_id.get(), anchor.get()),
                ledger_user_msg_id: (self.user_msg_id != 0).then_some(self.user_msg_id),
            },
            text,
        );
        drop(admission);
        match recordable {
            Err(settlement) => settlement,
            Ok(_) if persisted => unix_journal::Settlement::FrontierPersisted,
            Ok(_) => {
                tracing::warn!(
                    provider = %self.provider.as_str(),
                    channel_id = self.channel_id.get(),
                    record_channel = self.record_channel_id.get(),
                    range = ?range,
                    "recovery no-anchor delivery: durable anchor write failed"
                );
                unix_journal::Settlement::DurableWriteFailed
            }
        }
    }

    /// The durable identity this delivery may be recorded under, or the exit that
    /// names why it has none. Split out so the refusals stay one readable list
    /// while `record_durable_frontier` holds the admission and the funnel call.
    ///
    /// The order of the four refusals is the pre-S7 order, unchanged, because the
    /// journal settlement is which one is reached FIRST.
    fn resolve_durable_identity(&self) -> Result<((u64, u64), i64), unix_journal::Settlement> {
        let Some(range) = self.durable_range else {
            // Unreachable with an open obligation: `begin_recovery_terminal`
            // refuses the same two range cases this fn does, so nothing was
            // opened. The variant keeps the exit named all the same.
            return Err(unix_journal::Settlement::DeliveryNotRecorded);
        };
        if range.1 <= range.0 {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                range = ?range,
                "recovery no-anchor delivery: refusing to record empty durable range"
            );
            return Err(unix_journal::Settlement::DeliveryNotRecorded);
        }
        let Some(tmux_session_name) = self.tmux_session_name.as_deref() else {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                "recovery no-anchor delivery: no tmux session name; durable anchor unavailable"
            );
            return Err(unix_journal::Settlement::NoTmuxSessionName);
        };
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux_session_name);
        if generation_mtime_ns == 0 {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                tmux_session_name,
                "recovery no-anchor delivery: no current generation marker; durable anchor unavailable"
            );
            return Err(unix_journal::Settlement::NoGenerationMarker);
        }
        if !self.reuse_recorded_anchor && self.expected_gone_anchor.is_none() {
            // Pre-S7 this was the writer-selection `else` arm, producing
            // `Err("recovery gone-anchor delivery lacks expected durable anchor
            // identity")`. It has to stay a refusal HERE: the funnel picks
            // `ReplaceProvenGone` from this same `Option`, so an absent anchor
            // would otherwise fall through to the ordinary monotonic merge —
            // turning what used to be a refusal into a write.
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = self.channel_id.get(),
                record_channel = self.record_channel_id.get(),
                "recovery gone-anchor delivery lacks expected durable anchor identity"
            );
            return Err(unix_journal::Settlement::DurableWriteFailed);
        }
        Ok((range, generation_mtime_ns))
    }
}

pub(in crate::services::discord) async fn replace_anchored_terminal_text(
    http: &serenity::Http,
    channel_id: ChannelId,
    placeholder: MessageId,
    text: &str,
    shared: &Arc<SharedData>,
    recovery_context: Option<&RecoveryDeliveryContext>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let outcome = formatting::replace_long_message_raw_with_outcome(
        http,
        channel_id,
        placeholder,
        text,
        shared,
        &mut None,
    )
    .await?;
    record_anchored_fallback_replacement(recovery_context, shared, channel_id, &outcome, text);
    formatting::replace_long_message_outcome_to_result(outcome)
}

fn record_anchored_fallback_replacement(
    recovery_context: Option<&RecoveryDeliveryContext>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    outcome: &formatting::ReplaceLongMessageOutcome,
    text: &str,
) {
    let formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
        replacement_anchor: Some(anchor),
        ..
    } = outcome
    else {
        return;
    };
    if let Some(context) = recovery_context {
        context.record_successful_fresh_send_after_anchored_edit_fallback(shared, *anchor, text);
    } else {
        tracing::warn!(
            channel_id = channel_id.get(),
            anchor_msg_id = anchor.get(),
            "recovery anchored delivery fell back to fresh send without D1 context; replacement anchor not recorded"
        );
    }
}

pub(in crate::services::discord) async fn relay_no_anchor_terminal_text(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    text: &str,
    recovery_context: Option<&RecoveryDeliveryContext>,
) -> RecoveryRelayOutcome {
    let Some(context) = recovery_context else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "recovery no-anchor delivery has no D1 idempotency context; falling back to legacy fresh send"
        );
        return match formatting::send_long_message_raw(http, channel_id, text, shared).await {
            Ok(()) => RecoveryRelayOutcome::Delivered,
            Err(error) => {
                let classified =
                    recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
                recovery_paths::shared::escalate_transient_relay_outcome_with_probe(
                    classified,
                    || recovery_paths::restart::probe_channel_liveness(http, channel_id),
                )
                .await
            }
        };
    };
    let Some(mut lease) = context.try_acquire_fresh_send_lease(shared, text) else {
        tracing::warn!(
            channel_id = channel_id.get(),
            "recovery no-anchor delivery lease busy; skipping fresh send for retry"
        );
        return RecoveryRelayOutcome::TransientFailure;
    };
    let result = formatting::send_long_message_raw_with_reference_returning_message_ids(
        http, channel_id, text, shared, None,
    )
    .await;
    match result {
        Ok(message_ids) => {
            let committed = lease.commit(LeaseOutcome::Delivered);
            // Record chunk 0's message id. If only the inflight row proves reuse
            // later, the anchored replace arm must edit the first message and
            // regenerate continuations, not edit a tail continuation.
            if let Some(anchor) = message_ids.first().copied() {
                if committed {
                    context.record_successful_fresh_send_after_no_anchor_post(shared, anchor, text);
                } else {
                    tracing::warn!(
                        channel_id = channel_id.get(),
                        anchor_msg_id = anchor.get(),
                        "recovery no-anchor delivery posted but lease commit failed; durable anchor not recorded"
                    );
                }
            } else {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    "recovery no-anchor delivery posted without a message id; anchor not recorded"
                );
            }
            lease.release();
            RecoveryRelayOutcome::Delivered
        }
        Err(error) => {
            let _ = lease.commit(LeaseOutcome::Unknown);
            lease.release();
            let classified = recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
            recovery_paths::shared::escalate_transient_relay_outcome_with_probe(classified, || {
                recovery_paths::restart::probe_channel_liveness(http, channel_id)
            })
            .await
        }
    }
}

pub(in crate::services::discord) struct RecoveryFreshSendLease {
    cell: Arc<DeliveryLeaseCell>,
    holder: LeaseHolder,
    key: DeliveryLeaseKey,
    start: u64,
    end: u64,
    heartbeat: Option<DeliveryLeaseHeartbeat>,
    released: bool,
}

impl RecoveryFreshSendLease {
    pub(in crate::services::discord) fn commit(&mut self, outcome: LeaseOutcome) -> bool {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        self.cell
            .commit(self.holder, self.key.clone(), self.start, self.end, outcome)
    }

    pub(in crate::services::discord) fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        self.cell
            .release(self.holder, self.key.clone(), self.start, self.end);
    }
}

impl Drop for RecoveryFreshSendLease {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.stop();
        }
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // #5071 T1 S7: production stopped importing `completed_turn_ledger` when the
    // ledger append moved into the funnel. The S7 tests below read it back, so
    // the import is test-scoped now -- which is itself part of the contract.
    use crate::services::discord::make_shared_data_for_tests;
    use crate::services::discord::outbound::completed_turn_ledger;

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn state(provider: ProviderKind, channel_id: u64) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            provider,
            channel_id,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            0,
            0,
            "recover this".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-adk-test".to_string()),
            Some("/tmp/recovery-idempotent.jsonl".to_string()),
            None,
            128,
        );
        state.turn_start_offset = Some(128);
        state.save_generation = 9;
        state.full_response = "answer".to_string();
        state
    }

    fn set_runtime_root() -> (tempfile::TempDir, EnvReset) {
        let reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::TempDir::new().expect("runtime root");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        (temp, reset)
    }

    fn write_generation_marker(tmux_session_name: &str) {
        let path = crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(parent).expect("generation parent");
        }
        std::fs::write(path, "1").expect("generation marker");
    }

    /// #5071 T1 S5: the confirmed-delivery funnel now takes the `SharedData` the
    /// journal's shadow-admission gate reads, plus the entry point that confirmed
    /// the delivery. A test process has no PG pool, so `begin_recovery_terminal`
    /// returns `None` and every assertion below observes exactly the durable
    /// writes it observed before the slice.
    fn record_fresh_send_for_test(ctx: &RecoveryDeliveryContext, anchor: MessageId, text: &str) {
        ctx.record_successful_fresh_send(
            &make_shared_data_for_tests(),
            anchor,
            text,
            unix_journal::Disposition::NoAnchorFreshSend,
        );
    }

    fn inflight_state_path_for_test(
        agentdesk_root: &std::path::Path,
        provider: &ProviderKind,
        channel_id: u64,
    ) -> std::path::PathBuf {
        agentdesk_root
            .join("runtime")
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{channel_id}.json"))
    }

    #[test]
    fn zero_channel_or_anchor_ids_skip_recovery_context_without_panicking() {
        let provider = ProviderKind::Codex;
        let zero_channel_state = state(provider.clone(), 0);
        assert!(
            RecoveryDeliveryContext::from_state(
                &make_shared_data_for_tests(),
                &provider,
                &zero_channel_state,
                None,
                42
            )
            .is_none()
        );

        let context = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state(provider.clone(), 44_099),
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");
        assert_eq!(context.inflight_recorded_anchor(), None);
        assert_eq!(context.anchor_reuse_decision(), None);
    }

    #[tokio::test]
    async fn same_turn_retry_after_anchor_persist_keeps_same_lease_key() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_000);
        inflight::save_inflight_state(&state).expect("save inflight");

        let delivery_generation = 42;
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            None,
            delivery_generation,
        )
        .expect("non-zero test channel id");
        record_fresh_send_for_test(&ctx, MessageId::new(77_000), "answer");

        let persisted =
            inflight::load_inflight_state(&provider, state.channel_id).expect("persisted row");
        assert!(
            persisted.save_generation > state.save_generation,
            "anchor bind should bump the per-file save generation"
        );
        let retry_ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &persisted,
            None,
            delivery_generation,
        )
        .expect("non-zero test channel id");

        assert_eq!(
            ctx.lease_key, retry_ctx.lease_key,
            "same-turn retry must keep the same delivery lease key after anchor persistence"
        );
        assert_eq!(retry_ctx.lease_key.generation, delivery_generation);
    }

    #[tokio::test]
    async fn same_turn_second_recovery_attempt_uses_recorded_anchor_not_fresh_post() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_001);
        inflight::save_inflight_state(&state).expect("save inflight");
        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::from_state(
            &shared,
            &provider,
            &state,
            None,
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");

        let mut fresh_posts = 0;
        assert!(ctx.recorded_anchor().is_none());
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "answer")
            .expect("first attempt acquires");
        fresh_posts += 1;
        assert!(lease.commit(LeaseOutcome::Delivered));
        record_fresh_send_for_test(&ctx, MessageId::new(77_001), "answer");
        lease.release();

        assert_eq!(ctx.recorded_anchor(), Some(MessageId::new(77_001)));
        if ctx.recorded_anchor().is_none() {
            fresh_posts += 1;
        }
        assert_eq!(fresh_posts, 1, "second attempt must edit/skip, not POST");
    }

    #[tokio::test]
    async fn bind_rejected_skips_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_002);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");

        let mut newer = state.clone();
        newer.user_msg_id = newer.user_msg_id.saturating_add(1);
        newer.turn_start_offset = Some(512);
        inflight::save_inflight_state(&newer).expect("save newer inflight");

        record_fresh_send_for_test(&ctx, MessageId::new(77_002), "answer");

        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "identity-mismatched bind must not write a durable frontier"
        );
    }

    #[tokio::test]
    async fn bind_read_io_error_skips_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_006);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let path = inflight_state_path_for_test(temp.path(), &provider, state.channel_id);
        std::fs::create_dir_all(path.parent().expect("inflight parent")).expect("inflight parent");
        std::fs::create_dir(&path).expect("directory at inflight path forces read_to_string error");
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");

        record_fresh_send_for_test(&ctx, MessageId::new(77_006), "answer");

        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "non-NotFound read failure must block the durable frontier write"
        );
    }

    #[tokio::test]
    async fn bind_missing_row_allows_durable_frontier_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_007);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");

        record_fresh_send_for_test(&ctx, MessageId::new(77_007), "answer");

        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            ChannelId::new(state.delivery_record_owner_channel_id()),
            tmux,
            Some(u64::MAX),
        )
        .expect("genuine absence is safe to record as durable delivered");
        assert_eq!(anchor.panel_msg_id, 77_007);
        assert_eq!(anchor.panel_channel_id, state.channel_id);
        assert_eq!(anchor.range, (128, 256));
    }

    #[tokio::test]
    async fn durable_matched_reuse_returns_delivered_without_discord_post() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_003);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        // #4188: a genuinely durable-delivered anchor implies the transcript
        // (output_path) exists and is at least as long as the recorded range end
        // (256). Seed it so the EOF-bound guard trusts the current-generation
        // frontier instead of fail-safe distrusting an absent transcript.
        std::fs::write(
            state.output_path.as_deref().expect("output_path"),
            vec![b'x'; 512],
        )
        .expect("seed transcript at/above the durable frontier end");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::from_state(
            &shared,
            &provider,
            &state,
            Some((128, 256)),
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "answer")
            .expect("first attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        record_fresh_send_for_test(&ctx, MessageId::new(77_003), "answer");
        lease.release();

        let fresh_shared_after_restart = make_shared_data_for_tests();
        let retry_ctx = RecoveryDeliveryContext::from_state(
            &fresh_shared_after_restart,
            &provider,
            &state,
            Some((128, 256)),
            fresh_shared_after_restart.restart.current_generation,
        )
        .expect("non-zero test channel id");
        assert_eq!(
            retry_ctx.anchor_reuse_decision(),
            Some(RecoveryAnchorReuse::DurableAlreadyDelivered(
                MessageId::new(77_003)
            )),
            "durable terminal anchor has range proof and must be treated as already delivered"
        );

        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let outcome = super::super::relay_recovered_terminal_text_to_placeholder(
            &http,
            &fresh_shared_after_restart,
            ChannelId::new(state.channel_id),
            None,
            "answer",
            Some(&retry_ctx),
        )
        .await;
        assert!(
            outcome.delivered(),
            "durable reuse should return Delivered before any Discord POST can be attempted"
        );
    }

    #[tokio::test]
    async fn gone_anchor_repost_context_records_replacement_to_matched_record_channel() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_008);
        state.current_msg_id = 77_008;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        let matched_record_channel = ChannelId::new(55_008);
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux);
        delivery_record::write_delivered_frontier(
            &provider,
            matched_record_channel.get(),
            tmux,
            delivery_record::DeliveredCommit {
                range: (128, 256),
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(77_008),
                panel_channel_id: Some(state.channel_id),
            },
        )
        .expect("old matched-owner durable anchor");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::send_new_after_gone_anchor(
            &shared,
            &provider,
            &state,
            ChannelId::new(state.channel_id),
            Some((128, 256)),
            shared.restart.current_generation,
            (state.channel_id, 77_008),
        )
        .with_record_channel_id(matched_record_channel);
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "replacement")
            .expect("repost attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        record_fresh_send_for_test(&ctx, MessageId::new(88_008), "replacement");
        lease.release();

        let matched_anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            matched_record_channel,
            tmux,
            Some(u64::MAX),
        )
        .expect("replacement durable anchor should overwrite the matched owner record");
        assert_eq!(matched_anchor.panel_msg_id, 88_008);
        assert_eq!(matched_anchor.panel_channel_id, state.channel_id);
        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "replacement must not be written to the stale state-derived owner record"
        );
    }

    #[tokio::test]
    async fn gone_anchor_repost_context_does_not_reuse_old_anchor_but_records_replacement() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_004);
        state.current_msg_id = 77_004;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");
        let generation_mtime_ns = delivery_record::current_generation_mtime_ns(tmux);
        delivery_record::write_delivered_frontier(
            &provider,
            state.delivery_record_owner_channel_id(),
            tmux,
            delivery_record::DeliveredCommit {
                range: (128, 256),
                generation_mtime_ns,
                attempts: 1,
                panel_msg_id: Some(77_004),
                panel_channel_id: Some(state.channel_id),
            },
        )
        .expect("old durable anchor");

        let shared = make_shared_data_for_tests();
        let ctx = RecoveryDeliveryContext::send_new_after_gone_anchor(
            &shared,
            &provider,
            &state,
            ChannelId::new(state.channel_id),
            Some((128, 256)),
            shared.restart.current_generation,
            (state.channel_id, 77_004),
        );
        assert_eq!(
            ctx.recorded_anchor(),
            None,
            "gone-anchor repost must not edit the old anchor it just proved missing"
        );
        let mut lease = ctx
            .try_acquire_fresh_send_lease(&shared, "replacement")
            .expect("repost attempt acquires");
        assert!(lease.commit(LeaseOutcome::Delivered));
        record_fresh_send_for_test(&ctx, MessageId::new(88_004), "replacement");
        lease.release();

        let retry_ctx = RecoveryDeliveryContext::from_state(
            &shared,
            &provider,
            &state,
            Some((128, 256)),
            shared.restart.current_generation,
        )
        .expect("non-zero test channel id");
        assert_eq!(
            retry_ctx.recorded_anchor(),
            Some(MessageId::new(88_004)),
            "replacement anchor should be reused by later ordinary recovery retries"
        );
    }

    #[test]
    fn anchored_fallback_fresh_send_records_replacement_anchor() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let mut state = state(provider.clone(), 44_005);
        state.current_msg_id = 77_005;
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        inflight::save_inflight_state(&state).expect("save inflight");

        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");
        let outcome =
            crate::services::discord::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "404 stale anchor".to_string(),
                replacement_anchor: Some(MessageId::new(88_005)),
            };

        record_anchored_fallback_replacement(
            Some(&ctx),
            &make_shared_data_for_tests(),
            ChannelId::new(state.channel_id),
            &outcome,
            "replacement",
        );

        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            ChannelId::new(state.delivery_record_owner_channel_id()),
            tmux,
            Some(u64::MAX),
        )
        .expect("replacement durable anchor");
        assert_eq!(anchor.panel_msg_id, 88_005);
        assert_eq!(
            inflight::load_inflight_state(&provider, state.channel_id)
                .expect("inflight row")
                .current_msg_id,
            88_005,
            "fallback replacement should become the next anchored-edit target"
        );
    }

    // ---------------------------------------------------------------------
    // #5071 T1 S7 — the join, asserted by what breaks when it is undone.
    //
    // Every assertion below FAILS on the pre-S7 code, which called
    // `delivery_record::write_delivered_frontier` /
    // `write_proven_gone_equal_range_frontier` directly and appended the
    // completed-turn ledger itself, ahead of both. They are the runtime half of
    // the contract; `scripts/check_durable_frontier_writer_call_sites.py` holds
    // the lexical half.
    // ---------------------------------------------------------------------

    fn ledger_state(
        provider: ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
    ) -> inflight::InflightTurnState {
        let mut state = state(provider, channel_id);
        state.user_msg_id = user_msg_id;
        state
    }

    fn record_fresh_send_with_shared(
        ctx: &RecoveryDeliveryContext,
        shared: &SharedData,
        anchor: MessageId,
        text: &str,
    ) {
        ctx.record_successful_fresh_send(
            shared,
            anchor,
            text,
            unix_journal::Disposition::NoAnchorFreshSend,
        );
    }

    /// D4. The funnel records the #4081 delivered-content fingerprint after a
    /// successful persist; the pre-S7 raw write recorded none at all, so this
    /// assertion is `false` on that code.
    ///
    /// It says the fingerprint EXISTS for this body under this generation. It
    /// says nothing about whether any reader consults it on a recovery path.
    #[tokio::test]
    async fn joined_funnel_records_the_delivered_content_fingerprint() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = state(provider.clone(), 44_501);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let record_channel = ChannelId::new(state.delivery_record_owner_channel_id());
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");

        assert!(
            !delivery_record::recent_delivered_content_matches(
                &provider,
                record_channel,
                tmux,
                "answer"
            ),
            "nothing recorded before the delivery"
        );

        record_fresh_send_for_test(&ctx, MessageId::new(77_501), "answer");

        assert!(
            delivery_record::recent_delivered_content_matches(
                &provider,
                record_channel,
                tmux,
                "answer"
            ),
            "joining the funnel is what records the fingerprint; the pre-S7 raw \
             write recorded none"
        );
    }

    /// D5. Ordering, in the only case where the two orders differ observably:
    /// the durable write FAILS. Pre-S7 the ledger append ran first and therefore
    /// survived the failure; under the funnel a failed persist returns before it.
    ///
    /// The failure is forced by putting a FILE where the provider's
    /// delivery-record directory has to be, so the writer's `create_dir_all`
    /// cannot succeed. The generation marker is present and the range is valid,
    /// so nothing else refuses first.
    #[tokio::test]
    async fn joined_funnel_does_not_settle_the_ledger_when_the_frontier_write_fails() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = ledger_state(provider.clone(), 44_502, 99_502);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let record_path = delivery_record::delivery_record_path(&provider, state.channel_id)
            .expect("record path");
        let provider_dir = record_path.parent().expect("provider dir");
        std::fs::create_dir_all(provider_dir.parent().expect("records root"))
            .expect("records root");
        std::fs::write(provider_dir, b"not a directory").expect("block the record directory");

        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");
        record_fresh_send_for_test(&ctx, MessageId::new(77_502), "answer");

        assert!(
            !completed_turn_ledger::settled_user_msg_ids(&provider, state.channel_id)
                .contains(&99_502),
            "a failed durable write must not leave a settled-turn claim behind; \
             pre-S7 the leading append did exactly that"
        );
    }

    /// D5, the other half — the guarantee #4564 wanted from that leading append
    /// is KEPT. With no readable generation marker there is no frontier to
    /// write, and the funnel's unknown-generation branch still settles the turn.
    #[tokio::test]
    async fn joined_funnel_still_settles_the_ledger_without_a_generation_marker() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = ledger_state(provider.clone(), 44_503, 99_503);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        // deliberately NO write_generation_marker(tmux)
        let ctx = RecoveryDeliveryContext::from_state(
            &make_shared_data_for_tests(),
            &provider,
            &state,
            Some((128, 256)),
            42,
        )
        .expect("non-zero test channel id");

        record_fresh_send_with_shared(
            &ctx,
            &make_shared_data_for_tests(),
            MessageId::new(77_503),
            "answer",
        );

        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, state.channel_id)
                .contains(&99_503),
            "no generation marker must still suppress the false TooOld notice (#4564)"
        );
        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                ChannelId::new(state.delivery_record_owner_channel_id()),
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "and it must still write no durable frontier"
        );
    }

    /// D2. The admission is real, not decorative: a frontier reset landing
    /// between the recovery decision and the durable write refuses the write.
    ///
    /// This is the assertion that fails if the guard is acquired and dropped
    /// without being consulted, or acquired against an incarnation read at write
    /// time instead of at construction time. The ledger settlement survives —
    /// admission governs the FRONTIER, not whether the turn was answered.
    #[tokio::test]
    async fn frontier_reset_after_the_recovery_decision_refuses_the_durable_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = ledger_state(provider.clone(), 44_504, 99_504);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let shared = make_shared_data_for_tests();
        let record_channel = ChannelId::new(state.delivery_record_owner_channel_id());

        let ctx =
            RecoveryDeliveryContext::from_state(&shared, &provider, &state, Some((128, 256)), 42)
                .expect("non-zero test channel id");

        // The reset the recovery decision did not see.
        let coord = shared.tmux_relay_coord(record_channel);
        coord
            .confirmed_end_offset
            .store(300, std::sync::atomic::Ordering::Release);
        assert!(coord.reset_confirmed_frontier(300, 17));

        record_fresh_send_with_shared(&ctx, &shared, MessageId::new(77_504), "answer");

        assert!(
            delivery_frontier_probe::current_generation_delivered_anchor(
                &provider,
                record_channel,
                tmux,
                Some(u64::MAX),
            )
            .is_none(),
            "a reset between the decision and the write must refuse the frontier"
        );
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, state.channel_id)
                .contains(&99_504),
            "the turn was still answered; only the frontier is refused"
        );
    }

    /// The control for the test above: with NO reset, the same shapes write the
    /// frontier. Without this, a guard that always refused would pass.
    #[tokio::test]
    async fn unreset_frontier_still_records_the_durable_write() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (_temp, _reset) = set_runtime_root();
        let provider = ProviderKind::Codex;
        let state = ledger_state(provider.clone(), 44_505, 99_505);
        let tmux = state.tmux_session_name.as_deref().unwrap();
        write_generation_marker(tmux);
        let shared = make_shared_data_for_tests();
        let record_channel = ChannelId::new(state.delivery_record_owner_channel_id());

        let ctx =
            RecoveryDeliveryContext::from_state(&shared, &provider, &state, Some((128, 256)), 42)
                .expect("non-zero test channel id");
        record_fresh_send_with_shared(&ctx, &shared, MessageId::new(77_505), "answer");

        let anchor = delivery_frontier_probe::current_generation_delivered_anchor(
            &provider,
            record_channel,
            tmux,
            Some(u64::MAX),
        )
        .expect("durable anchor");
        assert_eq!(anchor.panel_msg_id, 77_505);
        assert_eq!(anchor.range, (128, 256));
        assert!(
            completed_turn_ledger::settled_user_msg_ids(&provider, state.channel_id)
                .contains(&99_505)
        );
    }
}
