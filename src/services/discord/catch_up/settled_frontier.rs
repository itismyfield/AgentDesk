//! Phase-1 settlement frontier. A later settled id must not carry the durable
//! checkpoint or a retry cursor past an earlier message the scan left open.

use super::super::recovery_known_ids::RecoveryKnownIdArm;
use super::classification::CatchUpClassification;
use super::frontier_evidence::FrontierEvidence;
use super::phase2::Phase2EnqueueCommit;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct SettledFrontier {
    newest_settled: Option<u64>,
    first_blocked_id: Option<u64>,
}

impl SettledFrontier {
    /// The scan is oldest-first, so once sealed nothing later is contiguous.
    pub(super) fn settle(&mut self, message_id: u64) {
        if self.first_blocked_id.is_none() {
            self.newest_settled = Some(
                self.newest_settled
                    .map_or(message_id, |n| n.max(message_id)),
            );
        }
    }

    pub(super) fn seal(&mut self, message_id: u64) {
        self.first_blocked_id = Some(
            self.first_blocked_id
                .map_or(message_id, |b| b.min(message_id)),
        );
    }

    pub(super) fn record_skipped(
        &mut self,
        message_id: u64,
        outcome: CatchUpClassification,
        arm: Option<RecoveryKnownIdArm>,
    ) {
        self.record(message_id, FrontierEvidence::of_known(outcome, arm));
    }

    pub(super) fn record_duplicate_commit(&mut self, message_id: u64, commit: Phase2EnqueueCommit) {
        self.record(message_id, FrontierEvidence::of_commit(commit));
    }

    /// Only `Dispatched` evidence may join the contiguous settled run.
    fn record(&mut self, message_id: u64, evidence: FrontierEvidence) {
        match evidence {
            FrontierEvidence::Dispatched => self.settle(message_id),
            FrontierEvidence::Open => self.seal(message_id),
        }
    }

    pub(super) fn newest(&self) -> Option<u64> {
        safe_durable_candidate(self.newest_settled, self.first_blocked_id)
    }

    pub(super) fn retained_barrier(
        &self,
        scan_checkpoint: Option<u64>,
        exhausted: bool,
    ) -> Option<RetainedBarrier> {
        let barrier = self.first_blocked_id?;
        Some(RetainedBarrier {
            barrier,
            retry_after: self.retry_after(scan_checkpoint, barrier),
            exhausted,
        })
    }

    pub(super) fn retry_after(&self, scan_checkpoint: Option<u64>, current_mid: u64) -> u64 {
        let candidate = self
            .newest_settled
            .or(scan_checkpoint)
            .unwrap_or_else(|| current_mid.saturating_sub(1));
        clamp_retry_cursor(candidate, self.first_blocked_id)
    }
}

/// An open barrier: phase 1 hands its own to phase 2; both reach the retry arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RetainedBarrier {
    pub(super) barrier: u64,
    pub(super) retry_after: u64,
    /// A retry budget ran out this sweep, so no fresh retry may be armed.
    pub(super) exhausted: bool,
}

/// A retry cursor may sit just before the barrier so the next scan rereads it.
pub(super) fn clamp_retry_cursor(candidate: u64, barrier: Option<u64>) -> u64 {
    barrier.map_or(candidate, |b| candidate.min(b.saturating_sub(1)))
}

/// A durable checkpoint must be an observed settled id, so a candidate at or
/// past the barrier is dropped rather than lowered to `barrier - 1`.
pub(super) fn safe_durable_candidate(candidate: Option<u64>, barrier: Option<u64>) -> Option<u64> {
    candidate.filter(|&n| barrier.is_none_or(|b| n < b))
}
