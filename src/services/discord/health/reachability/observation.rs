//! The reachability observation task — 4987 S1 second half (#5071 T4-B2).
//!
//! # This is where S1 observation starts, and it is ALL it starts
//!
//! T4-B1 landed the vocabulary and the two file primitives wired to nothing.
//! This slice gives them a caller: an independent 30 s task that resolves each
//! live watcher's transcript, reads what it grew by, frames the growth into
//! canonical obligations, and writes them to a durable per-channel ledger.
//!
//! **It has no judgment authority, and that is an invariant, not a phase.**
//! Concretely, and each of these is checked by a named test:
//!
//! * no health field, snapshot, API response or recovery input reads anything
//!   produced here — the ledger's only readers are this task and its tests;
//! * no destructive action is reachable from here. There is no turn cancel, no
//!   tmux or process kill, no registry removal, no mailbox or row force-clean,
//!   and no redelivery. `ReachabilityVerdict::authorizes_destructive_action` is
//!   false on every variant (4987 §7.1 / I15), `authorizes_redelivery` likewise
//!   (S7 stays NO-GO);
//! * `TransportUnknown` is not produced here at all, and no verdict this task
//!   records — including that one — is translated into health or redelivery.
//!   4987 §4.1's polarity is composed in T4-B6, behind `G-T4`.
//!
//! The task's whole output is a sidecar file and a counter. That is the point:
//! 4987 §-1.7 made S1 observation-only because the detection argument narrowed
//! to a single mechanism, and §3.4 requires 30 days of histogram before any
//! bound is chosen.
//!
//! # What this task CANNOT yet conclude
//!
//! Two of the three verdict shapes are deliberately unreachable here.
//!
//! * **`Degraded` / `Unreachable`** need `warn_bound` / `fail_bound`, and 4987
//!   §10 lists hardcoding a threshold at S1 as NO-GO — the bounds are the
//!   OUTPUT of the observation this task is starting. They are also missing
//!   their subtrahend: the receipt index is 4987 S2 / T4-B3, so a live
//!   obligation here means "not yet subtracted", never "undelivered".
//! * **`Reachable`** is spellable, but only under 4987 §-1.4's positive
//!   incarnation-alive requirement: the transcript resolved, the ledger holds
//!   zero obligations, AND the file grew this tick. "Nothing observed" is never
//!   GREEN.
//!
//! Everything else is recorded as a typed [`VerdictWithheldReason`] rather than
//! as a verdict. Withholding is the fail-closed choice: a reader of the sidecar
//! sees the named reason no conclusion was drawn, instead of an `Unknown`
//! borrowed from B1's fixed five reasons where none of them fits.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use super::discovery::{TranscriptCandidates, TranscriptResolution, resolve_transcript};
use super::ledger::{
    LedgerIncarnation, ObligationExtinction, ReachabilityLedger, ledger_file_exists, ledger_path,
    read_ledger_at, write_ledger_at,
};
use super::obligation::scan_canonical;
use super::tail::{TAIL_READ_CAP_BYTES, TailCursor, TailOutcome, read_incremental};
use super::verdict::{ReachabilityUnknownReason, ReachabilityVerdict};
use crate::services::discord::SharedData;
// The incarnation sidecar readers, taken from the modules that already own
// them: `.generation` mtime is 4987 §-1.2's row-independent authority, and
// `.spawn_nonce` is read through the #5071 T3-A0 read model rather than a
// second reader, so a `None` keeps T3-R2's meaning — absent evidence, never a
// wildcard match.
use crate::services::discord::tmux::execution_identity::capture_spawn_nonce;
use crate::services::discord::tmux::read_generation_file_mtime_ns;
use crate::services::provider::ProviderKind;

/// Tick cadence. Deliberately the SAME constant the stall watchdog already
/// runs on (4987 §3.1's measured baseline) rather than a new number, so the
/// per-tick budget `G-T4`'s `tick_budget_ok` measures is comparable to a cost
/// the host already pays.
use crate::services::discord::health::recovery::STALL_WATCHDOG_INTERVAL_SECS;

/// Let startup recovery and the first watcher attachments settle before the
/// first observation, so a bootstrap does not race a restore in progress. Only
/// affects when the first sidecar appears.
const OBSERVATION_INITIAL_DELAY_SECS: u64 = 45;

/// The coordinates one channel's observation needs, snapshotted out of the
/// registry so no map guard is held while the filesystem work runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ObservationInput {
    pub channel_id: u64,
    pub tmux_session_name: String,
    /// 4987 §-1.3 rank 1: the LIVE watcher entry's transcript. Sourced
    /// independently of the row, which is the whole point of I14.
    pub registry_output_path: Option<String>,
}

/// Why a tick drew no verdict. Named reasons, never a silent absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum VerdictWithheldReason {
    /// Obligations are live but nothing can subtract them yet: the receipt
    /// index is T4-B3 and the bounds are the output of this observation.
    CoverageAndBoundsUnavailable,
    /// Resolved, zero obligations, but no positive incarnation-alive evidence
    /// this tick, so 4987 §-1.4 forbids spelling `Reachable`.
    NoIncarnationAliveEvidence,
    /// The ledger was (re)bootstrapped this tick. The tail before the bootstrap
    /// offset was never read, so its absence proves nothing about it.
    IncarnationBootstrapped,
    /// No runtime root, so there is nowhere durable to record the observation.
    LedgerUnavailable,
}

/// One tick's outcome for one channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ObservationOutcome {
    /// A verdict this slice is entitled to spell. Recorded and consumed by
    /// nothing: the health composition is T4-B6, behind `G-T4`.
    Verdict(ReachabilityVerdict),
    Withheld(VerdictWithheldReason),
}

/// What one channel's tick did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ObservationReport {
    pub channel_id: u64,
    pub outcome: ObservationOutcome,
    pub new_obligations: usize,
    pub extinctions: Vec<ObligationExtinction>,
}

/// Process-local memory of when each channel's current `Unknown` reason began.
///
/// `since_secs` is therefore relative to THIS process: a dcserver restart
/// starts it at zero again. That is honest for an observation record and
/// nothing depends on it — a durable "since" would need the ledger, and the
/// `Unknown` cases that matter most are exactly the ones where no ledger could
/// be bound.
#[derive(Debug, Default)]
pub(in crate::services::discord) struct ObservationState {
    unknown_since_ms: HashMap<(u64, &'static str), u64>,
}

impl ObservationState {
    /// Seconds this channel has been reporting `reason`, starting the clock on
    /// first sight and restarting it when the reason changes.
    fn since_secs(
        &mut self,
        channel_id: u64,
        reason: ReachabilityUnknownReason,
        now_ms: u64,
    ) -> u64 {
        let key = (channel_id, unknown_reason_key(reason));
        self.unknown_since_ms
            .retain(|(channel, other), _| *channel != channel_id || *other == key.1);
        let first = *self.unknown_since_ms.entry(key).or_insert(now_ms);
        now_ms.saturating_sub(first) / 1_000
    }

    fn clear_unknown(&mut self, channel_id: u64) {
        self.unknown_since_ms
            .retain(|(channel, _), _| *channel != channel_id);
    }
}

/// A stable key per `Unknown` reason. Spelled out with no catch-all so a sixth
/// reason cannot silently share another's clock.
fn unknown_reason_key(reason: ReachabilityUnknownReason) -> &'static str {
    match reason {
        ReachabilityUnknownReason::TranscriptUnresolved => "transcript_unresolved",
        ReachabilityUnknownReason::TranscriptCoordinateDivergence => "coordinate_divergence",
        ReachabilityUnknownReason::RowlessActiveTurn => "rowless_active_turn",
        ReachabilityUnknownReason::ReadTruncated => "read_truncated",
        ReachabilityUnknownReason::ReceiptStoreUnreadable => "receipt_store_unreadable",
    }
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Observe one channel: resolve, read, frame, record.
///
/// Every filesystem effect this slice has is in this function, and all of them
/// are reads except the ledger write. It opens no socket, spawns no process,
/// and touches nothing the relay owns.
pub(in crate::services::discord) fn observe_channel(
    provider: &ProviderKind,
    input: &ObservationInput,
    state: &mut ObservationState,
    now_ms: u64,
) -> ObservationReport {
    let Some(path) = ledger_path(provider, input.channel_id) else {
        return report(
            input,
            ObservationOutcome::Withheld(VerdictWithheldReason::LedgerUnavailable),
        );
    };

    let registry_path = input.registry_output_path.as_ref().map(Path::new);
    let resolution = resolve_transcript(TranscriptCandidates {
        registry_output_path: registry_path,
        // 4987 §-1.1 R1: `TuiRuntimeBinding` can fall back to the wrapper, so
        // it is not an independent coordinate. This slice offers rank 2 to
        // NOBODY rather than offering a coordinate it would then have to
        // discount; wiring it as a comparison operand is T4-B4's divergence
        // work, which is what consumes `BindingComparison`.
        runtime_binding_path: None,
        // Rank 3 filesystem discovery is left empty on purpose: 4987 §-1.3
        // prices it "비용·위험 큼", and choosing roots is a decision with its
        // own failure mode. Rank 1 unresolved therefore fails closed to
        // `Unknown`, which is non-GREEN — the §-1.4 property that makes a
        // resolution failure a detection rather than a blind spot.
        discovery_roots: &[],
    });
    let TranscriptResolution::Resolved(resolved) = resolution else {
        let TranscriptResolution::Unresolved(reason) = resolution else {
            unreachable!("resolution is one of two variants");
        };
        return report(input, unknown(state, input.channel_id, reason, now_ms));
    };

    let incarnation = LedgerIncarnation::new(
        input.tmux_session_name.clone(),
        read_generation_file_mtime_ns(&input.tmux_session_name),
        capture_spawn_nonce(&input.tmux_session_name),
        resolved.stat.file_id,
    );

    let stored = read_ledger_at(&path);
    if stored.is_none() && ledger_file_exists(&path) {
        // A file that exists and will not parse. 4987 §-1.4 counterexample 7:
        // a malformed store is `Unknown`, never a conclusion about delivery.
        return report(
            input,
            unknown(
                state,
                input.channel_id,
                ReachabilityUnknownReason::ReceiptStoreUnreadable,
                now_ms,
            ),
        );
    }

    let mut ledger = match stored {
        Some(ledger) if ledger.binds_to(&incarnation) => ledger,
        Some(ledger) => {
            let rebootstrapped = ledger.retire_and_rebootstrap(incarnation, resolved.stat.len);
            return persist_and_report(
                &path,
                rebootstrapped,
                input,
                ObservationOutcome::Withheld(VerdictWithheldReason::IncarnationBootstrapped),
                0,
                Vec::new(),
            );
        }
        None => {
            // First sight. Bootstrap at the CURRENT end of file, not at zero:
            // this task claims only what it watched, and `bootstrap_offset`
            // records where that claim begins so an empty obligation list is
            // never read as "the earlier tail was delivered".
            let fresh =
                ReachabilityLedger::bootstrap(incarnation, resolved.stat.len, Default::default());
            return persist_and_report(
                &path,
                fresh,
                input,
                ObservationOutcome::Withheld(VerdictWithheldReason::IncarnationBootstrapped),
                0,
                Vec::new(),
            );
        }
    };

    let cursor = TailCursor::new(ledger.incarnation.identity(), ledger.cursor_offset);
    let outcome = read_incremental(&resolved.path, cursor);
    let TailOutcome::Read {
        bytes,
        start,
        end: _,
        cap_truncated,
    } = outcome
    else {
        let reason = outcome
            .unknown_reason()
            .unwrap_or(ReachabilityUnknownReason::TranscriptUnresolved);
        if matches!(
            reason,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence
        ) {
            // The coordinate under the cursor broke. Retire this incarnation's
            // obligations by their typed reason and re-bootstrap, so the next
            // tick reads a coordinate that means something. The verdict for
            // THIS tick is still the divergence.
            let rebootstrapped =
                ledger.retire_and_rebootstrap(ledger.incarnation.clone(), resolved.stat.len);
            let _ = write_ledger_at(&path, &rebootstrapped);
        }
        return report(input, unknown(state, input.channel_id, reason, now_ms));
    };

    let scan = scan_canonical(
        &bytes,
        start,
        ledger.incarnation.generation_mtime_ns,
        ledger.incarnation.identity(),
        TAIL_READ_CAP_BYTES,
    );
    let obligations: Vec<_> = scan.obligations().copied().collect();
    let extinctions = ledger.append_obligations(&obligations, now_ms);
    let incomplete = cap_truncated || scan.observation_is_incomplete();
    let advanced = resolved.stat.len > ledger.last_observed_len;

    ledger.cursor_offset = scan.next_offset;
    ledger.last_observed_len = resolved.stat.len;
    ledger.counters.ticks = ledger.counters.ticks.saturating_add(1);
    if incomplete {
        ledger.counters.incomplete_observations =
            ledger.counters.incomplete_observations.saturating_add(1);
    }

    let outcome = if incomplete {
        unknown(
            state,
            input.channel_id,
            ReachabilityUnknownReason::ReadTruncated,
            now_ms,
        )
    } else if !ledger.live_obligations().is_empty() {
        state.clear_unknown(input.channel_id);
        ObservationOutcome::Withheld(VerdictWithheldReason::CoverageAndBoundsUnavailable)
    } else if advanced {
        state.clear_unknown(input.channel_id);
        ObservationOutcome::Verdict(ReachabilityVerdict::Reachable)
    } else {
        state.clear_unknown(input.channel_id);
        ObservationOutcome::Withheld(VerdictWithheldReason::NoIncarnationAliveEvidence)
    };

    persist_and_report(
        &path,
        ledger,
        input,
        outcome,
        obligations.len(),
        extinctions,
    )
}

fn unknown(
    state: &mut ObservationState,
    channel_id: u64,
    reason: ReachabilityUnknownReason,
    now_ms: u64,
) -> ObservationOutcome {
    let since_secs = state.since_secs(channel_id, reason, now_ms);
    ObservationOutcome::Verdict(ReachabilityVerdict::unknown(reason, since_secs))
}

fn report(input: &ObservationInput, outcome: ObservationOutcome) -> ObservationReport {
    ObservationReport {
        channel_id: input.channel_id,
        outcome,
        new_obligations: 0,
        extinctions: Vec::new(),
    }
}

/// Write the ledger, then report. A write failure is logged and swallowed: the
/// ledger has no consumer, and an unpersisted cursor makes the NEXT tick
/// re-read the same bytes, which over-counts obligations rather than dropping
/// them. Over-counting is the safe direction — an obligation this task never
/// records is one no later slice can subtract.
fn persist_and_report(
    path: &Path,
    ledger: ReachabilityLedger,
    input: &ObservationInput,
    outcome: ObservationOutcome,
    new_obligations: usize,
    extinctions: Vec<ObligationExtinction>,
) -> ObservationReport {
    if let Err(error) = write_ledger_at(path, &ledger) {
        tracing::warn!(
            channel_id = input.channel_id,
            tmux_session = %input.tmux_session_name,
            error = %error,
            "#5071 T4-B2 reachability ledger write failed (observation only; no verdict depends on it)"
        );
    }
    ObservationReport {
        channel_id: input.channel_id,
        outcome,
        new_obligations,
        extinctions,
    }
}

/// Snapshot every live watcher's coordinates. Takes no lock across an await and
/// holds no map reference once it returns.
fn snapshot_inputs(shared: &SharedData) -> Vec<ObservationInput> {
    let sessions: Vec<(String, String)> = shared
        .tmux_watchers
        .iter()
        .map(|entry| (entry.key().clone(), entry.output_path.clone()))
        .collect();
    sessions
        .into_iter()
        .filter_map(|(tmux_session_name, output_path)| {
            let channel_id = shared
                .tmux_watchers
                .owner_channel_for_tmux_session(&tmux_session_name)?;
            Some(ObservationInput {
                channel_id: channel_id.get(),
                tmux_session_name,
                registry_output_path: Some(output_path),
            })
        })
        .collect()
}

/// One tick over every live watcher.
fn observe_all(
    provider: &ProviderKind,
    inputs: &[ObservationInput],
    state: &mut ObservationState,
) -> Vec<ObservationReport> {
    let now_ms = now_epoch_ms();
    inputs
        .iter()
        .map(|input| observe_channel(provider, input, state, now_ms))
        .collect()
}

/// The observation loop. 4987 §9.4 puts it here as an independent task rather
/// than inside `health/recovery.rs`, which is registry-tracked as a giant.
pub(in crate::services::discord) async fn run_observation_loop(
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    tokio::time::sleep(std::time::Duration::from_secs(
        OBSERVATION_INITIAL_DELAY_SECS,
    ))
    .await;
    let mut state = ObservationState::default();
    loop {
        let inputs = snapshot_inputs(&shared);
        let provider_for_tick = provider.clone();
        let carried = std::mem::take(&mut state);
        let started = std::time::Instant::now();
        let joined = tokio::task::spawn_blocking(move || {
            let mut state = carried;
            let reports = observe_all(&provider_for_tick, &inputs, &mut state);
            (state, reports)
        })
        .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        match joined {
            Ok((carried, reports)) => {
                state = carried;
                let obligations: usize = reports.iter().map(|report| report.new_obligations).sum();
                tracing::debug!(
                    provider = provider.as_str(),
                    channels = reports.len(),
                    obligations,
                    elapsed_ms,
                    "#5071 T4-B2 reachability observation tick"
                );
            }
            Err(error) => {
                tracing::warn!(
                    provider = provider.as_str(),
                    error = %error,
                    "#5071 T4-B2 reachability observation tick did not complete"
                );
            }
        }

        // `G-T4`'s `tick_budget_ok` needs `tick_overrun_count == 0` against the
        // 30 s budget. Logging the overrun is the whole response: this task
        // changes no behaviour when it is slow, and 4987 §A8 says an overrun
        // means the tick needs sharding, which is a design decision and not
        // something a running loop should take on itself.
        if elapsed_ms >= STALL_WATCHDOG_INTERVAL_SECS * 1_000 {
            tracing::warn!(
                provider = provider.as_str(),
                elapsed_ms,
                budget_ms = STALL_WATCHDOG_INTERVAL_SECS * 1_000,
                "#5071 T4-B2 reachability observation tick exceeded its budget (G-T4 tick_overrun)"
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS)).await;
    }
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;
