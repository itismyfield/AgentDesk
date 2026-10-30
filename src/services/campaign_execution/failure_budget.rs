use super::model::{NodeDisposition, NodeRuntime};
use super::profile::ProfileLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureDisposition {
    Repair,
    Exhausted,
    AlreadyCounted,
}

// One quality failure per (series, repair cycle) of the failed attempt; a blocked result after the
// last allowed repair fails the node. Waits, rejected evidence and interruptions never reach here.
pub fn record_quality_failure(
    node: &mut NodeRuntime,
    failed_cycle: u32,
    limits: &ProfileLimits,
) -> FailureDisposition {
    let key = (node.series, failed_cycle);
    if failed_cycle < node.repair_cycles || node.counted_failure == Some(key) {
        return FailureDisposition::AlreadyCounted;
    }
    node.counted_failure = Some(key);
    node.quality_failures += 1;
    if failed_cycle >= limits.quality_repairs {
        node.disposition = NodeDisposition::Failed;
        return FailureDisposition::Exhausted;
    }
    node.repair_cycles = failed_cycle + 1;
    node.disposition = NodeDisposition::Pending;
    FailureDisposition::Repair
}

#[cfg(test)]
#[path = "failure_budget_tests.rs"]
mod tests;
