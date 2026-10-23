//! The policy side of the judge: hooks per engine lane, their reads, the server pool acquire
//! probe, and the candidate's bound of one reader connection per Context.

use super::*;

/// The engine a policy hook was fired on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in super::super) enum Lane {
    Main,
    Tick,
}

impl Lane {
    pub(in super::super) const ALL: [Lane; 2] = [Lane::Main, Lane::Tick];

    pub(in super::super) fn name(self) -> &'static str {
        match self {
            Lane::Main => "main",
            Lane::Tick => "tick",
        }
    }

    pub(in super::super) fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lane| lane.name() == name)
    }

    /// The conditions key holding this lane's hook interval.
    pub(in super::super) fn interval_key(self) -> &'static str {
        match self {
            Lane::Main => "policy_main_interval_ms",
            Lane::Tick => "policy_tick_interval_ms",
        }
    }
}

/// What one policy hook produced: its reads all matched, some did not, or it never ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) enum PolicyOutcome {
    Unsent,
    Timeout,
    Failed(String),
    Mismatch(String),
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct PolicyAttempt {
    pub(in super::super) lane: Lane,
    pub(in super::super) seq: usize,
    pub(in super::super) measured: bool,
    pub(in super::super) scheduled: u64,
    pub(in super::super) sent: Option<u64>,
    pub(in super::super) end: Option<u64>,
    pub(in super::super) outcome: PolicyOutcome,
}

/// One server pool acquire of the probe; `waited` is None when it failed or timed out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in super::super) struct Acquire {
    pub(in super::super) measured: bool,
    pub(in super::super) waited: Option<u64>,
}

/// Policy hooks judged like health attempts; a run whose hooks did not read, or read wrong
/// values, is not a sample of the policy load.
pub(super) fn summarize_policy(run: &RunData, plan: Plan, summary: &mut RunSummary) {
    let mut whole_hooks = 0;
    for lane in Lane::ALL {
        let Some(interval) = run.condition(lane.interval_key()) else {
            summary
                .invalid
                .push(format!("conditions_unreadable:{}", lane.interval_key()));
            continue;
        };
        let mut attempts: Vec<&PolicyAttempt> =
            run.policy.iter().filter(|hook| hook.lane == lane).collect();
        attempts.sort_by_key(|hook| hook.seq);
        let expected = plan.policy_schedule(lane, interval);
        let reconciled = attempts.len() == expected.len()
            && attempts
                .iter()
                .zip(&expected)
                .enumerate()
                .all(|(seq, (hook, scheduled))| hook.seq == seq && hook.scheduled == *scheduled);
        if !reconciled {
            summary
                .invalid
                .push(format!("policy_reconciliation_mismatch:{}", lane.name()));
        }
        if attempts
            .iter()
            .any(|hook| hook.measured != (hook.scheduled >= plan.warmup_end()))
        {
            summary
                .invalid
                .push(format!("policy_warmup_overlap:{}", lane.name()));
        }
        let mut out = EndpointSummary::default();
        let mut mismatches = 0;
        for hook in attempts.into_iter().filter(|hook| hook.measured) {
            let class = match &hook.outcome {
                PolicyOutcome::Read if hook.end.is_some() => Class::Completed,
                PolicyOutcome::Unsent => Class::Unsent,
                PolicyOutcome::Timeout => Class::Timeout,
                PolicyOutcome::Mismatch(_) => {
                    mismatches += 1;
                    Class::Error
                }
                _ => Class::Error,
            };
            // Only a hook wholly inside the counted window owes all its SELECTs to it.
            if class == Class::Completed
                && hook
                    .sent
                    .is_some_and(|sent| sent >= plan.warmup_end() + SEC)
                && hook.end.is_some_and(|end| end + SEC <= plan.end())
            {
                whole_hooks += 1;
            }
            out.record(class, hook.scheduled, hook.sent, hook.end);
        }
        if out.counts.completed == 0 {
            summary
                .invalid
                .push(format!("policy_no_completed:{}", lane.name()));
        }
        if mismatches > 0 {
            summary
                .invalid
                .push(format!("policy_read_mismatch:{}:{mismatches}", lane.name()));
        }
        summary.policy.insert(lane, out);
    }
    match (
        run.resources.policy_statements,
        run.condition("selects_per_hook"),
    ) {
        (Some(0), _) => summary.invalid.push("policy_unqueried".to_string()),
        (Some(seen), Some(per_hook)) if (seen as u64) < whole_hooks * per_hook => {
            summary.invalid.push(format!(
                "policy_reads_short:{seen}<{}",
                whole_hooks * per_hook
            ))
        }
        (Some(_), None) => summary
            .invalid
            .push("conditions_unreadable:selects_per_hook".to_string()),
        _ => {}
    }
    summary.acquires = run
        .acquires
        .iter()
        .filter(|acquire| acquire.measured)
        .map(|acquire| acquire.waited)
        .collect();
    if summary.acquires.is_empty() {
        summary.invalid.push("acquire_unobserved".to_string());
    }
}

/// Every run's policy hooks per lane and server pool acquires, pooled per version.
pub(in super::super) fn pooled_policy(
    runs: &[RunSummary],
) -> (BTreeMap<Lane, EndpointSummary>, Vec<Option<u64>>) {
    let mut lanes: BTreeMap<Lane, EndpointSummary> = BTreeMap::new();
    let mut acquires = Vec::new();
    for run in runs {
        for (lane, summary) in &run.policy {
            lanes.entry(*lane).or_default().absorb(summary);
        }
        acquires.extend_from_slice(&run.acquires);
    }
    (lanes, acquires)
}

/// The candidate owns one reader per Context: never more connections than Contexts.
pub(super) fn candidate_invalid(candidate: &[RunSummary]) -> Vec<String> {
    let mut invalid = Vec::new();
    for run in candidate.iter().filter(|run| run.profile == Profile::S2) {
        let contexts = run
            .conditions
            .get("contexts_with_pg")
            .and_then(|value| value.parse::<usize>().ok());
        let r = &run.resources;
        let churn = r
            .end
            .proxy_accepted
            .zip(r.start.proxy_accepted)
            .map(|(end, start)| end.saturating_sub(start));
        for (what, seen) in [
            ("connection_churn", churn),
            ("connections_beyond_pool", r.peak.beyond_pool),
        ] {
            match (seen, contexts) {
                (Some(seen), Some(contexts)) if seen > contexts => {
                    invalid.push(format!("candidate_{what}:{}:{seen}>{contexts}", run.name));
                }
                (Some(_), Some(_)) => {}
                _ => {
                    invalid.push(format!("candidate_{what}_unobserved:{}", run.name));
                }
            }
        }
    }
    invalid
}
