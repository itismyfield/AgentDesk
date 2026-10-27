//! The mailbox half of the health snapshot. Every actor read ends by the request's deadline,
//! and a read that did not answer is reported, never taken as an idle mailbox.

#[cfg(unix)]
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::time::Duration;

use futures::future::join_all;
use poise::serenity_prelude::ChannelId;
use serde::Serialize;
use tokio::time::{Instant, timeout_at};

use super::super::mailbox::MailboxHealthSnapshot;
use super::super::redaction;
use super::super::session_enrichment::{
    self, ExecutorWitness, SessionEnrichment, TmuxObservationBudget,
};
use super::super::stall_verdict;
use super::super::transcript_binding_stall as binding_stall;
use super::super::unpaired_active_token::{
    self, RelayHealthBuildInput, build_relay_health_snapshot,
};
use super::super::{HealthRegistry, ProviderEntry};
use super::{
    DiscordHealthSnapshot, HealthStatus, RelayThreadProofSnapshot, last_outbound_activity_ms,
    relay_active_turn_from_inflight, trace_relay_health_classification,
};
use crate::services::discord::relay_health::FrontierProvenanceReport;
use crate::services::discord::{self, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, MailboxObservationFailure};

/// One provider's first mailbox reads, keyed by channel.
pub(super) type Observations =
    HashMap<ChannelId, Result<ChannelMailboxSnapshot, MailboxObservationFailure>>;

/// The mailbox-read deadline of a snapshot built outside `health_snapshot_by`.
const UNBOUNDED_CALLER_BUDGET: Duration = Duration::from_millis(1_500);

/// The degraded reason for a snapshot in which some actor read did not answer.
const MAILBOX_SNAPSHOT_UNAVAILABLE: &str = "mailbox_snapshot_unavailable";

tokio::task_local! {
    /// The deadline of the `/api/health` request whose snapshot this task is building.
    static REQUEST_DEADLINE: Instant;
}

/// The request's deadline inside `health_snapshot_by`; any other build gets its own budget.
pub(super) fn snapshot_deadline() -> Instant {
    REQUEST_DEADLINE
        .try_with(|deadline| *deadline)
        .unwrap_or_else(|_| Instant::now() + UNBOUNDED_CALLER_BUDGET)
}

/// A mailbox read that did not answer; its channel gets no idle snapshot or proof from it.
#[derive(Debug, Serialize)]
pub(super) struct MailboxReadFailure {
    provider: String,
    channel_id: u64,
    read: &'static str,
    observation_failure: &'static str,
}

impl MailboxReadFailure {
    fn new(provider: &str, channel: ChannelId, read: &'static str) -> Self {
        Self {
            provider: provider.to_string(),
            channel_id: channel.get(),
            read,
            observation_failure: MailboxObservationFailure::Unreachable.as_str(),
        }
    }
}

impl HealthRegistry {
    /// The health snapshot whose mailbox reads all end by `deadline`, the request's own.
    pub(crate) async fn health_snapshot_by(
        &self,
        include_details: bool,
        deadline: Instant,
    ) -> DiscordHealthSnapshot {
        let build = async {
            if include_details {
                super::build_health_snapshot(self).await
            } else {
                super::build_public_health_snapshot(self).await
            }
        };
        REQUEST_DEADLINE.scope(deadline, build).await
    }
}

/// Every provider's first mailbox reads, all started together and ending by `deadline`: a slow
/// actor may use all of it, and the later reads then end unanswered rather than past it.
pub(super) async fn first_reads(
    providers: &[ProviderEntry],
    deadline: Instant,
) -> Vec<Observations> {
    join_all(providers.iter().map(|entry| {
        let shared = entry.shared.as_ref();
        shared
            .mailboxes
            .try_snapshot_all_for_health(Vec::new(), deadline)
    }))
    .await
}

/// Names each channel whose first read did not answer.
pub(super) fn record_first_read_failures(
    provider: &str,
    observations: &Observations,
    failures: &mut Vec<MailboxReadFailure>,
) {
    for (channel, observation) in observations {
        if let Err(MailboxObservationFailure::Unreachable) = observation {
            failures.push(MailboxReadFailure::new(provider, *channel, "snapshot"));
        }
    }
}

/// Any unanswered read degrades the snapshot once; the failures list names the channels.
pub(super) fn degrade_on_read_failures(
    failures: &mut [MailboxReadFailure],
    status: &mut HealthStatus,
    degraded_reasons: &mut Vec<String>,
) {
    failures.sort_by(|a, b| {
        (&a.provider, a.channel_id, a.read).cmp(&(&b.provider, b.channel_id, b.read))
    });
    if !failures.is_empty() {
        *status = status.worsen(HealthStatus::Degraded);
        degraded_reasons.push(MAILBOX_SNAPSHOT_UNAVAILABLE.to_string());
    }
}

/// What one provider's channel pass reads besides the mailbox snapshots.
pub(super) struct ChannelPass<'a> {
    pub(super) registry: &'a HealthRegistry,
    pub(super) entry: &'a ProviderEntry,
    pub(super) provider_kind: &'a Option<ProviderKind>,
    pub(super) include_mailbox_details: bool,
    #[cfg(unix)]
    pub(super) composite_governs_polarity: bool,
    #[cfg(unix)]
    pub(super) o_owned_channels: &'a BTreeSet<u64>,
    pub(super) deadline: Instant,
}

/// The snapshot-wide values the channel pass adds to.
pub(super) struct Sinks<'a> {
    #[cfg(unix)]
    pub(super) degraded_reasons: &'a mut Vec<String>,
    #[cfg(unix)]
    pub(super) expired_relay_ledgers: &'a mut Vec<String>,
    #[cfg(unix)]
    pub(super) status: &'a mut HealthStatus,
    pub(super) mailbox_entries: &'a mut Vec<MailboxHealthSnapshot>,
    pub(super) read_failures: &'a mut Vec<MailboxReadFailure>,
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
        include_mailbox_details,
        #[cfg(unix)]
        composite_governs_polarity,
        #[cfg(unix)]
        o_owned_channels,
        deadline,
    } = pass;
    let Sinks {
        #[cfg(unix)]
        degraded_reasons,
        #[cfg(unix)]
        expired_relay_ledgers,
        #[cfg(unix)]
        status,
        mailbox_entries,
        read_failures,
    } = sinks;
    // Session and tmux reads keep channel order: they share one tmux budget.
    let mut loaded = Vec::new();
    for (channel_id, snapshot) in observed_mailboxes {
        let channel = *channel_id;
        let session = SessionEnrichment::load(&entry.shared, provider_kind.as_ref(), channel).await;
        // #5942: one probe, charged once against the shared budget, read
        // three-valued. `tmux_present` keeps its exact previous meaning
        // (only `Present` is a live session), so every existing consumer of
        // it is unchanged; what is new is that the ledger TTL below can
        // tell "no session" from "no answer".
        let executor_witness = session.tmux_session_witness_within(tmux).await;
        loaded.push((channel, snapshot, session, executor_witness));
    }
    // The thread-proof and reconfirm reads of every channel run together, each cut at the deadline.
    let second = join_all(loaded.iter().map(|(channel, snapshot, session, _)| {
        second_reads(
            entry,
            provider_kind.as_ref(),
            *channel,
            snapshot,
            session,
            deadline,
        )
    }))
    .await;
    for ((channel, snapshot, session, executor_witness), second) in loaded.into_iter().zip(second) {
        let SecondReads {
            relay_thread_proof,
            unpaired_active_token_reconfirmed,
            unanswered,
        } = second;
        for read in unanswered {
            read_failures.push(MailboxReadFailure::new(&entry.name, channel, read));
        }
        let tmux_present = matches!(executor_witness, ExecutorWitness::Present);
        let desynced = session.desynced(tmux_present, session.watcher_attached);
        let mailbox_has_cancel_token = snapshot.cancel_token.is_some();
        let queue_depth = snapshot.intervention_queue.len();
        let mailbox_active_user_msg_id =
            redaction::visible_serenity_message_id(snapshot.active_user_message_id);
        // Resolved before `relay_thread_proof` is folded into the relay
        // health snapshot below; the coordinate LOOKUP it feeds is
        // detail-only and stays behind the payload gate (#5736).
        let counterpart_channel_id = relay_thread_proof.counterpart_channel_id(channel.get());
        let active_turn =
            relay_active_turn_from_inflight(mailbox_has_cancel_token, session.inflight.as_ref());
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
        // The verdict is composed and applied to the polarity in the parent, a judgment consumer.
        #[cfg(unix)]
        let relay_verdict = super::ChannelRelayVerdict::compose(
            super::ChannelVerdictInput {
                registry,
                provider: &entry.name,
                provider_kind: provider_kind.as_ref(),
                channel,
                session: &session,
                relay_health: &relay_health,
                executor_witness,
                governs: composite_governs_polarity && !o_owned_channels.contains(&channel.get()),
            },
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
            let reachability = relay_verdict.report();
            let relay_stall_state = binding_stall::classify_relay_stall(&relay_health);
            trace_relay_health_classification(&relay_health, relay_stall_state);
            mailbox_entries.push(MailboxHealthSnapshot {
                provider: entry.name.clone(),
                channel_id: channel.get(),
                has_cancel_token: mailbox_has_cancel_token,
                queue_depth,
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

/// What the fresh reads after a channel's first read found, and which of them did not answer.
struct SecondReads {
    relay_thread_proof: RelayThreadProofSnapshot,
    unpaired_active_token_reconfirmed: bool,
    unanswered: Vec<&'static str>,
}

/// The paired thread's read and the unpaired-token reconfirm, both fresh and both ending by
/// `deadline`. A reconfirm that did not answer confirms nothing.
async fn second_reads(
    entry: &ProviderEntry,
    provider: Option<&ProviderKind>,
    channel: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
    session: &SessionEnrichment,
    deadline: Instant,
) -> SecondReads {
    let live = snapshot.cancel_token.is_some()
        || session.inflight_state_present
        || session.watcher_attached;
    let reconfirm = unpaired_active_token::reconfirm(
        &entry.shared,
        provider,
        channel,
        snapshot,
        session.inflight_state_present,
    );
    let ((relay_thread_proof, thread_failure), reconfirmed) = tokio::join!(
        relay_thread_proof_for_channel(&entry.shared, provider, channel, live, Some(deadline)),
        timeout_at(deadline, reconfirm),
    );
    let mut unanswered = Vec::new();
    if thread_failure.is_some() {
        unanswered.push("thread_proof");
    }
    if reconfirmed.is_err() {
        unanswered.push("reconfirm");
    }
    SecondReads {
        relay_thread_proof,
        unpaired_active_token_reconfirmed: reconfirmed.unwrap_or(false),
        unanswered,
    }
}

/// A fresh read of `channel`'s actor ending by `deadline`; a channel with no actor reads idle.
async fn read_by(
    shared: &SharedData,
    channel: ChannelId,
    deadline: Instant,
) -> Result<ChannelMailboxSnapshot, MailboxObservationFailure> {
    let Some(handle) = shared.mailbox_peek(channel) else {
        return Ok(ChannelMailboxSnapshot::no_actor(channel));
    };
    timeout_at(deadline, handle.try_snapshot())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(MailboxObservationFailure::Unreachable)
}

/// The channel's parent/thread proof. With a `deadline` the thread read ends by it, and an
/// unanswered read is returned instead of marking the proof stale.
pub(super) async fn relay_thread_proof_for_channel(
    shared: &SharedData,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    current_channel_has_live_evidence: bool,
    deadline: Option<Instant>,
) -> (RelayThreadProofSnapshot, Option<MailboxObservationFailure>) {
    let (thread_channel_id, parent_channel_id) = super::thread_pair(shared, channel_id);

    let mut unanswered = None;
    let child_has_live_evidence = match thread_channel_id {
        Some(thread_id) => {
            let thread_channel = ChannelId::new(thread_id);
            let thread_mailbox = match deadline {
                Some(deadline) => read_by(shared, thread_channel, deadline).await,
                None => Ok(discord::mailbox_snapshot(shared, thread_channel).await),
            };
            // #5736: read-only — see `SessionEnrichment::load`. This probe exists
            // to answer "does the paired thread show live evidence", never to
            // rewrite the row it reads.
            let thread_inflight = provider.and_then(|provider| {
                discord::inflight::load_inflight_state_read_only(provider, thread_id)
            });
            match thread_mailbox {
                Ok(thread_mailbox) => {
                    thread_mailbox.cancel_token.is_some()
                        || thread_inflight.is_some()
                        || shared.tmux_watchers.contains_key(&thread_channel)
                }
                // An unanswered read proves nothing either way, so it cannot make the proof stale.
                Err(failure) => {
                    unanswered = Some(failure);
                    true
                }
            }
        }
        None => false,
    };

    let proof = RelayThreadProofSnapshot {
        parent_channel_id,
        thread_channel_id,
        stale_thread_proof: thread_channel_id.is_some_and(|_| !child_has_live_evidence)
            || parent_channel_id.is_some_and(|_| !current_channel_has_live_evidence),
    };
    (proof, unanswered)
}

#[cfg(all(test, unix))]
#[path = "health_observation_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "health_observation_pg_tests.rs"]
pub(in crate::services::discord::health) mod pg_tests;
