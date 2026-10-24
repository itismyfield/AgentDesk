//! The claim side of the judge: dispatch claims judged like health attempts, every completed
//! claim owed a claimed row, and the synthetic watcher reads on the server's blocking pool.

use super::*;

/// Conditions an `s3` run must carry for its claim and blocking load to be judged.
pub(in super::super) const CLAIM_RPS_KEY: &str = "claim_rps";
pub(in super::super) const WATCHERS_KEY: &str = "blocking_watchers";
pub(in super::super) const CONFIG_SHA_KEY: &str = "config_sha256";
pub(in super::super) const CONFIG_BYTES_KEY: &str = "config_bytes";

/// What one claim POST produced; `claimed` is the length of the answer's `claimed` list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) enum ClaimOutcome {
    Unsent,
    Timeout,
    Failed(String),
    Answered { status: u16, claimed: Option<usize> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct ClaimAttempt {
    pub(in super::super) seq: usize,
    pub(in super::super) measured: bool,
    pub(in super::super) scheduled: u64,
    pub(in super::super) sent: Option<u64>,
    pub(in super::super) last_byte: Option<u64>,
    pub(in super::super) outcome: ClaimOutcome,
}

/// One synthetic watcher read: submitted to the blocking pool, started there, finished; the
/// last two stay None when the read never ran or never returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in super::super) struct BlockingRead {
    pub(in super::super) watcher: usize,
    pub(in super::super) submitted: u64,
    pub(in super::super) started: Option<u64>,
    pub(in super::super) finished: Option<u64>,
}

/// Measured blocking reads: how many went in, how many came back, and each one's queue wait.
#[derive(Clone, Debug, Default)]
pub(in super::super) struct BlockingSummary {
    pub(in super::super) submitted: usize,
    pub(in super::super) completed: usize,
    pub(in super::super) waits: Vec<Option<u64>>,
}

impl Plan {
    /// Claims at `rps`, jittered like the endpoints but seeded apart from them and the hooks.
    pub(in super::super) fn claim_schedule(&self, rps: u64) -> Vec<u64> {
        self.jittered(7, SEC / rps.max(1))
    }
}

/// A claim answers only with status 200 and a `claimed` list; one that claimed nothing is
/// counted apart, since every seeded row stays claimable once its one-second claim lapses.
pub(super) fn summarize_claims(run: &RunData, plan: Plan, summary: &mut RunSummary) {
    let Some(rps) = run.condition(CLAIM_RPS_KEY) else {
        summary
            .invalid
            .push(format!("conditions_unreadable:{CLAIM_RPS_KEY}"));
        return;
    };
    match run.condition(CONFIG_BYTES_KEY) {
        Some(bytes) if bytes > 0 && run.conditions.contains_key(CONFIG_SHA_KEY) => {}
        _ => summary.invalid.push("config_unrecorded".to_string()),
    }
    let mut attempts: Vec<&ClaimAttempt> = run.claims.iter().collect();
    attempts.sort_by_key(|claim| claim.seq);
    let expected = plan.claim_schedule(rps);
    let reconciled = attempts.len() == expected.len()
        && attempts
            .iter()
            .zip(&expected)
            .enumerate()
            .all(|(seq, (claim, scheduled))| claim.seq == seq && claim.scheduled == *scheduled);
    if !reconciled {
        summary
            .invalid
            .push("claim_reconciliation_mismatch".to_string());
    }
    if attempts
        .iter()
        .any(|claim| claim.measured != (claim.scheduled >= plan.warmup_end()))
    {
        summary.invalid.push("claim_warmup_overlap".to_string());
    }
    let mut out = EndpointSummary::default();
    let (mut claimed, mut empty) = (0, 0);
    for claim in attempts.into_iter().filter(|claim| claim.measured) {
        let class = match &claim.outcome {
            ClaimOutcome::Unsent => Class::Unsent,
            ClaimOutcome::Timeout => Class::Timeout,
            ClaimOutcome::Answered {
                status: 200,
                claimed: Some(rows),
            } if claim.last_byte.is_some() => {
                claimed += rows;
                empty += usize::from(*rows == 0);
                Class::Completed
            }
            _ => Class::Error,
        };
        out.record(class, claim.scheduled, claim.sent, claim.last_byte);
    }
    if out.counts.completed == 0 {
        summary.invalid.push("claim_no_completed".to_string());
    }
    if claimed == 0 {
        summary.invalid.push("claim_unclaimed".to_string());
    }
    if empty > 0 {
        summary.invalid.push(format!("claim_empty:{empty}"));
    }
    summary.claim_rows = claimed;
    summary.claim = Some(out);
    summarize_blocking(run, plan, summary);
}

/// With watchers, measured reads must have gone in and all come back; without, none at all.
fn summarize_blocking(run: &RunData, plan: Plan, summary: &mut RunSummary) {
    let Some(watchers) = run.condition(WATCHERS_KEY) else {
        summary
            .invalid
            .push(format!("conditions_unreadable:{WATCHERS_KEY}"));
        return;
    };
    let mut out = BlockingSummary::default();
    for read in run
        .blocking
        .iter()
        .filter(|read| read.submitted >= plan.warmup_end() && read.submitted < plan.end())
    {
        out.submitted += 1;
        out.completed += usize::from(read.finished.is_some());
        out.waits.push(
            read.started
                .filter(|_| read.finished.is_some())
                .map(|started| started.saturating_sub(read.submitted)),
        );
    }
    if run
        .blocking
        .iter()
        .any(|read| read.watcher as u64 >= watchers)
    {
        summary.invalid.push("blocking_record_mismatch".to_string());
    }
    match (watchers, out.submitted) {
        (0, 0) => {}
        (0, _) => summary.invalid.push("blocking_unexpected".to_string()),
        (_, 0) => summary.invalid.push("blocking_unloaded".to_string()),
        _ if out.completed < out.submitted => summary.invalid.push(format!(
            "blocking_incomplete:{}<{}",
            out.completed, out.submitted
        )),
        _ => {}
    }
    summary.blocking = Some(out);
}

/// Every run's claims, claimed rows and watcher reads, pooled per version.
pub(in super::super) fn pooled_claims(
    runs: &[RunSummary],
) -> (Option<EndpointSummary>, usize, Option<BlockingSummary>) {
    let (mut claim, mut rows, mut blocking) = (None, 0, None);
    for run in runs {
        if let Some(summary) = &run.claim {
            claim
                .get_or_insert_with(EndpointSummary::default)
                .absorb(summary);
        }
        rows += run.claim_rows;
        if let Some(reads) = &run.blocking {
            let pooled: &mut BlockingSummary = blocking.get_or_insert_with(Default::default);
            pooled.submitted += reads.submitted;
            pooled.completed += reads.completed;
            pooled.waits.extend_from_slice(&reads.waits);
        }
    }
    (claim, rows, blocking)
}
