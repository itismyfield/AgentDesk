use crate::engine::PolicyEngine;

#[derive(Debug, Clone)]
pub struct CronJobDescriptor {
    pub job_id: String,
    pub name: String,
    pub every_ms: i64,
    pub kv_label: String,
}

const TIER_DEFINITIONS: &[(&str, &str, i64, &str)] = &[
    (
        "tick:30s",
        "onTick30s — [J] retry, [I-0] notification recovery, [I] deadlock, [K] orphan",
        30_000,
        "30s",
    ),
    (
        "tick:1min",
        "onTick1min — [A][C][D][E][L] non-critical timeouts",
        60_000,
        "1min",
    ),
    (
        "tick:5min",
        "onTick5min — [R][B][F][G][H][M][O] non-critical reconciliation + idle session cleanup",
        300_000,
        "5min",
    ),
];

pub fn tier_descriptors() -> Vec<CronJobDescriptor> {
    TIER_DEFINITIONS
        .iter()
        .map(|(job_id, name, every_ms, kv_label)| CronJobDescriptor {
            job_id: (*job_id).to_string(),
            name: (*name).to_string(),
            every_ms: *every_ms,
            kv_label: (*kv_label).to_string(),
        })
        .collect()
}

pub fn tier_descriptor_by_label(label: &str) -> Option<CronJobDescriptor> {
    TIER_DEFINITIONS
        .iter()
        .find_map(|(job_id, name, every_ms, kv_label)| {
            (*kv_label == label).then(|| CronJobDescriptor {
                job_id: (*job_id).to_string(),
                name: (*name).to_string(),
                every_ms: *every_ms,
                kv_label: (*kv_label).to_string(),
            })
        })
}

pub fn legacy_policy_descriptors(engine: &PolicyEngine) -> Vec<CronJobDescriptor> {
    engine
        .list_policies()
        .iter()
        .filter(|policy| {
            policy.hooks.iter().any(|hook| hook == "onTick") && policy.name != "timeouts"
        })
        .map(|policy| CronJobDescriptor {
            job_id: format!("policy:{}", policy.name),
            name: format!("policy/{} → onTick (5min legacy)", policy.name),
            every_ms: 300_000,
            kv_label: "legacy".to_string(),
        })
        .collect()
}

pub fn github_issue_sync_descriptor(interval_minutes: u64) -> Option<CronJobDescriptor> {
    (interval_minutes > 0).then(|| CronJobDescriptor {
        job_id: "github_issue_card_sync".to_string(),
        name: "github issue card sync — reconcile GitHub issues and mainline commits into kanban cards".to_string(),
        every_ms: (interval_minutes as i64) * 60_000,
        kv_label: "github_sync".to_string(),
    })
}

/// #1072 Turn-lifecycle SLO aggregation (Epic #905 Phase 1).
/// Piggybacks on the 5-minute tier — exposed separately so the cron-jobs API
/// can surface the aggregator as a distinct job for observability.
pub fn slo_aggregation_descriptor() -> CronJobDescriptor {
    CronJobDescriptor {
        job_id: "slo_aggregation".to_string(),
        name: "SLO aggregation — 3 turn-lifecycle metrics (success rate, duplicate relays, avg latency) + threshold alerter".to_string(),
        every_ms: 300_000,
        kv_label: "5min".to_string(),
    }
}
