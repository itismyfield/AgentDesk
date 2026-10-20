//! The per-channel half of the health snapshot: each mailbox the provider probe observed,
//! enriched with its session, relay proof and relay verdict.

use std::collections::BTreeSet;

use poise::serenity_prelude::ChannelId;

use super::super::mailbox::{
    MailboxHealthSnapshot, QueueParkHealthObservations as ParkObservations,
};
#[cfg(unix)]
use super::super::reachability::composite::{
    RelayVerdictProbe, RelayVerdictReport, apply_relay_verdict_polarity, observe_relay_verdict,
};
use super::super::redaction;
use super::super::session_enrichment::{
    self, ExecutorWitness, SessionEnrichment, TmuxObservationBudget,
};
use super::super::stall_verdict;
use super::super::unpaired_active_token::{
    self, RelayHealthBuildInput, build_relay_health_snapshot,
};
use super::super::{HealthRegistry, ProviderEntry};
#[cfg(unix)]
use super::relay_probe::relay_verdict_probe_operands;
use super::{
    HealthStatus, RelayThreadProofSnapshot, last_outbound_activity_ms,
    relay_active_turn_from_inflight, trace_relay_health_classification,
};
use crate::services::discord::relay_health::{FrontierProvenanceReport, RelayStallClassifier};
use crate::services::discord::{self, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;

/// What one provider's channel pass reads besides the mailbox snapshots.
pub(super) struct ChannelPass<'a> {
    pub(super) registry: &'a HealthRegistry,
    pub(super) entry: &'a ProviderEntry,
    pub(super) provider_kind: &'a Option<ProviderKind>,
    pub(super) park: &'a ParkObservations<'a>,
    pub(super) include_mailbox_details: bool,
    #[cfg(unix)]
    pub(super) composite_governs_polarity: bool,
    #[cfg(unix)]
    pub(super) o_owned_channels: &'a BTreeSet<u64>,
}

/// The snapshot-wide values the channel pass adds to.
pub(super) struct Sinks<'a> {
    pub(super) degraded_reasons: &'a mut Vec<String>,
    #[cfg(unix)]
    pub(super) expired_relay_ledgers: &'a mut Vec<String>,
    pub(super) status: &'a mut HealthStatus,
    pub(super) mailbox_entries: &'a mut Vec<MailboxHealthSnapshot>,
}

pub(super) async fn channel_entries<'s>(
    pass: ChannelPass<'_>,
    observed_mailboxes: impl Iterator<Item = (&'s ChannelId, &'s ChannelMailboxSnapshot)>,
    tmux: &mut TmuxObservationBudget,
    sinks: Sinks<'_>,
) {
    let ChannelPass {
        registry,
        entry,
        provider_kind,
        park,
        include_mailbox_details,
        #[cfg(unix)]
        composite_governs_polarity,
        #[cfg(unix)]
        o_owned_channels,
    } = pass;
    let Sinks {
        degraded_reasons,
        #[cfg(unix)]
        expired_relay_ledgers,
        status,
        mailbox_entries,
    } = sinks;
    for (channel_id, snapshot) in observed_mailboxes {
        let channel = *channel_id;
        let session = SessionEnrichment::load(&entry.shared, provider_kind.as_ref(), channel).await;
        // #5942: one probe, charged once against the shared budget, read
        // three-valued. `tmux_present` keeps its exact previous meaning
        // (only `Present` is a live session), so every existing consumer of
        // it is unchanged; what is new is that the ledger TTL below can
        // tell "no session" from "no answer".
        let executor_witness = session.tmux_session_witness_within(tmux).await;
        let tmux_present = matches!(executor_witness, ExecutorWitness::Present);
        let desynced = session.desynced(tmux_present, session.watcher_attached);
        let mailbox_has_cancel_token = snapshot.cancel_token.is_some();
        let queue_depth = snapshot.intervention_queue.len();
        let mailbox_active_user_msg_id =
            redaction::visible_serenity_message_id(snapshot.active_user_message_id);
        let relay_thread_proof = relay_thread_proof_for_channel(
            &entry.shared,
            provider_kind.as_ref(),
            channel,
            mailbox_has_cancel_token || session.inflight_state_present || session.watcher_attached,
        )
        .await;
        // Resolved before `relay_thread_proof` is folded into the relay
        // health snapshot below; the coordinate LOOKUP it feeds is
        // detail-only and stays behind the payload gate (#5736).
        let counterpart_channel_id = relay_thread_proof.counterpart_channel_id(channel.get());
        let active_turn =
            relay_active_turn_from_inflight(mailbox_has_cancel_token, session.inflight.as_ref());
        let unpaired_active_token_reconfirmed = unpaired_active_token::reconfirm(
            &entry.shared,
            provider_kind.as_ref(),
            channel,
            snapshot,
            session.inflight_state_present,
        )
        .await;
        let relay_health = build_relay_health_snapshot(RelayHealthBuildInput {
            provider: entry.name.clone(),
            channel_id: channel.get(),
            mailbox_has_cancel_token,
            mailbox_active_user_msg_id,
            mailbox_turn_started_at_ms: snapshot
                .turn_started_at
                .map(|started_at| started_at.timestamp_millis()),
            unpaired_active_token_reconfirmed,
            queue_depth,
            watcher_attached: session.watcher_attached,
            watcher_attached_stale: session.watcher_attached_stale,
            watcher_owner_channel_id: session.watcher_owner_channel_id,
            tmux_session: session.tmux_session.clone(),
            tmux_alive: session.tmux_session.as_ref().map(|_| tmux_present),
            bridge_inflight_present: session.inflight_state_present,
            bridge_current_msg_id: session.inflight_current_msg_id(),
            watcher_owns_live_relay: session.watcher_owns_live_relay(),
            last_relay_ts_ms: session.last_relay_ts_ms,
            last_relay_offset: session.last_relay_offset,
            last_capture_offset: session.last_capture_offset,
            unread_bytes: session.unread_bytes,
            desynced,
            thread_proof: relay_thread_proof,
            active_turn,
            last_outbound_activity_ms: last_outbound_activity_ms(
                session.last_relay_ts_ms,
                session.inflight.as_ref(),
            ),
        });
        // #5071 T4-B6 (4987 §4.3/§4.4): compose the relay verdict from
        // the durable T4-B2c ledger, the T4-B3 receipt projection and
        // the T4-B5 sidecar. Whether it may also change polarity is the
        // `RelayVerdictSource` switch, read once per poll below. The
        // row's `output_path` is passed to the divergence comparison
        // only; nothing here resolves or tails through it.
        #[cfg(unix)]
        let relay_verdict = {
            // The health poll's `tmux_present` witness is intentionally
            // weaker than the recovery snapshot's has-session probe, and
            // weaker again once the shared observation budget is spent
            // (`session_enrichment::witness_tmux_session_within`). The
            // explicit helper operand
            // keeps that difference visible instead of silently forking the
            // remaining inputs.
            let operands = relay_verdict_probe_operands(
                executor_witness,
                &relay_health,
                registry.started_at_unix(),
            );
            observe_relay_verdict(RelayVerdictProbe {
                provider: provider_kind.as_ref(),
                channel_id: channel.get(),
                row_output_path: session
                    .inflight
                    .as_ref()
                    .and_then(|state| state.output_path.as_deref()),
                registry_output_path: session.watcher_output_path.as_deref(),
                pane_idle_confirmed: operands.pane_idle_confirmed,
                rowless_turn: operands.rowless_turn,
                placeholder_present: operands.placeholder_present,
                executor: operands.executor,
                now_epoch_ms: operands.now_epoch_ms,
                process_started_at_epoch_ms: operands.process_started_at_epoch_ms,
            })
        };
        #[cfg(unix)]
        let verdict_governs =
            composite_governs_polarity && !o_owned_channels.contains(&channel.get());
        #[cfg(unix)]
        apply_relay_verdict_polarity(
            verdict_governs,
            &relay_verdict,
            &entry.name,
            channel.get(),
            degraded_reasons,
            expired_relay_ledgers,
            status,
        );
        // #5736: this gate is the PAYLOAD boundary, not the judgement one.
        // The polarity pass above now runs on both builds; while it sat
        // inside this block the public `/api/health` reported `healthy` from
        // the same registry `/api/health/detail` called `degraded`.
        //
        // Everything the polarity pass does NOT read stays behind the gate,
        // so raising the judgement did not raise the evidence: the r2 review
        // found the counterpart-coordinate lookup, the frontier provenance
        // report and the shadow stall verdict computed on the unauthenticated
        // path and then discarded.
        if include_mailbox_details {
            // #5071 relay-tail S1 (I-4): the counterpart's coordinate is
            // read here, where the axis is already resolved, and published
            // beside this channel's own pair as raw evidence. It feeds NO
            // hypothesis — H3 is deferred past S1 (design §1.4, §9's S1
            // row) and the r1 review measured the cost of letting it decide
            // early. One more lock-free `.get()` on the same in-memory map;
            // `None` when this channel is not part of a parent/thread pair.
            let frontier_provenance = FrontierProvenanceReport::of(
                session.frontier_provenance,
                counterpart_channel_id.map(|counterpart| {
                    session_enrichment::observe_coord_frontier(
                        &entry.shared,
                        ChannelId::new(counterpart),
                    )
                }),
            );
            let stall_shadow_verdict = stall_verdict::classify_health_snapshot_lossy(
                provider_kind.as_ref(),
                channel,
                &session,
                &relay_health,
                registry.started_at_unix(),
            );
            #[cfg(unix)]
            let reachability = RelayVerdictReport::of(&relay_verdict, verdict_governs);
            let relay_stall_state = RelayStallClassifier::classify(&relay_health);
            trace_relay_health_classification(&relay_health, relay_stall_state);
            mailbox_entries.push(MailboxHealthSnapshot {
                provider: entry.name.clone(),
                channel_id: channel.get(),
                has_cancel_token: mailbox_has_cancel_token,
                queue_depth,
                queue_park: park.project(channel),
                recovery_started: snapshot.recovery_started_at.is_some(),
                active_request_owner: snapshot.active_request_owner.map(|id| id.get()),
                active_user_message_id: mailbox_active_user_msg_id,
                agent_turn_status: super::super::mailbox::mailbox_agent_turn_status(
                    mailbox_has_cancel_token,
                    super::super::mailbox::residual_occupancy(&entry.shared, channel, &snapshot),
                ),
                watcher_attached: session.watcher_attached,
                inflight_state_present: session.inflight_state_present,
                tmux_present,
                process_present: false, // set after every presence probe
                active_dispatch_present: session.active_dispatch_present(),
                stall_shadow_verdict,
                #[cfg(unix)]
                reachability,
                frontier_provenance,
                relay_stall_state,
                tail_observation: session.tail_observation,
                relay_health,
            });
        }
    }
}

pub(super) async fn relay_thread_proof_for_channel(
    shared: &SharedData,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    current_channel_has_live_evidence: bool,
) -> RelayThreadProofSnapshot {
    let thread_channel_id = shared
        .dispatch
        .thread_parents
        .get(&channel_id)
        .map(|entry| entry.value().get());
    let parent_channel_id = shared
        .dispatch
        .thread_parents
        .iter()
        .find_map(|entry| (*entry.value() == channel_id).then_some(entry.key().get()));

    let child_has_live_evidence = match thread_channel_id {
        Some(thread_id) => {
            let thread_channel = ChannelId::new(thread_id);
            let thread_mailbox = discord::mailbox_snapshot(shared, thread_channel).await;
            // #5736: read-only — see `SessionEnrichment::load`. This probe exists
            // to answer "does the paired thread show live evidence", never to
            // rewrite the row it reads.
            let thread_inflight = provider.and_then(|provider| {
                discord::inflight::load_inflight_state_read_only(provider, thread_id)
            });
            thread_mailbox.cancel_token.is_some()
                || thread_inflight.is_some()
                || shared.tmux_watchers.contains_key(&thread_channel)
        }
        None => false,
    };

    RelayThreadProofSnapshot {
        parent_channel_id,
        thread_channel_id,
        stale_thread_proof: thread_channel_id.is_some_and(|_| !child_has_live_evidence)
            || parent_channel_id.is_some_and(|_| !current_channel_has_live_evidence),
    }
}
