//! The judge for `health_amplification_profile`: one class per scheduled attempt or policy hook,
//! percentiles that rank unfinished ones at +inf, and the BASE-versus-candidate verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;

use super::resource_regression_tests::THREAD_SLACK;

mod cases_tests;
mod claim_verdict_tests;
mod policy_verdict_tests;
mod s2_cases_tests;
mod s3_cases_tests;

use claim_verdict_tests::summarize_claims;
pub(super) use claim_verdict_tests::{
    BlockingRead, BlockingSummary, CLAIM_RPS_KEY, CONFIG_BYTES_KEY, CONFIG_SHA_KEY, ClaimAttempt,
    ClaimOutcome, WATCHERS_KEY, pooled_claims,
};
pub(super) use policy_verdict_tests::{
    Acquire, Lane, PolicyAttempt, PolicyOutcome, RELOADED, pooled_policy,
};
use policy_verdict_tests::{candidate_invalid, summarize_policy};

pub(super) const SEC: u64 = 1_000_000;
/// Per version and endpoint, summed over its runs.
pub(super) const MIN_ATTEMPTS: usize = 10_000;
/// Measured tick slots per version.
pub(super) const MIN_TICKS: usize = 1_000;
/// The one condition expected to differ between versions, and to match within one.
pub(super) const BINARY_KEY: &str = "binary_sha256";
const OPERATOR_PG_PORT: u16 = 15432;
/// The one age reason whose number grows while the run lasts.
const AGING_REASON: &str = "dispatch_outbox_oldest_pending_age";

/// `s1` loads health only; `s2` adds the policy read hooks, the tick engine and their probes;
/// `s3` adds dispatch claims and optional watcher reads on the server's blocking pool.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Profile {
    #[default]
    S1,
    S2,
    S3,
}

impl Profile {
    pub(super) fn name(self) -> &'static str {
        match self {
            Profile::S1 => "s1",
            Profile::S2 => "s2",
            Profile::S3 => "s3",
        }
    }

    pub(super) fn parse(name: &str) -> Option<Self> {
        [Profile::S1, Profile::S2, Profile::S3]
            .into_iter()
            .find(|profile| profile.name() == name)
    }

    /// Whether the run carries the policy read load and its probes.
    pub(super) fn has_policy(self) -> bool {
        self != Profile::S1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Endpoint {
    Public,
    Detail,
}

impl Endpoint {
    pub(super) const ALL: [Endpoint; 2] = [Endpoint::Public, Endpoint::Detail];

    pub(super) fn name(self) -> &'static str {
        match self {
            Endpoint::Public => "public",
            Endpoint::Detail => "detail",
        }
    }

    pub(super) fn path(self) -> &'static str {
        match self {
            Endpoint::Public => "/health",
            Endpoint::Detail => "/health/detail",
        }
    }

    pub(super) fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|endpoint| endpoint.name() == name)
    }
}

/// What one HTTP exchange produced, before its integrity is judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Unsent,
    Timeout,
    Failed(String),
    Answered(Answer),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Answer {
    pub(super) status: u16,
    pub(super) parsed: bool,
    pub(super) observation_status: String,
    pub(super) node_hostname: bool,
    /// `degraded_reasons`, sorted.
    pub(super) reasons: Vec<String>,
}

/// One scheduled request; instants are microseconds from the run origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Attempt {
    pub(super) endpoint: Endpoint,
    pub(super) seq: usize,
    /// The generator's own phase tag, cross-checked against the schedule.
    pub(super) measured: bool,
    pub(super) scheduled: u64,
    pub(super) sent: Option<u64>,
    pub(super) last_byte: Option<u64>,
    pub(super) outcome: Outcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    Completed,
    Error,
    Timeout,
    Unsent,
}

/// The status and reason set every measured answer must repeat.
pub(super) type Reference = (u16, Vec<String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    pub(super) seed: u64,
    /// Requests per second, per endpoint.
    pub(super) rps: u64,
    pub(super) warmup_secs: u64,
    pub(super) measure_secs: u64,
}

impl Plan {
    pub(super) fn warmup_end(&self) -> u64 {
        self.warmup_secs * SEC
    }

    pub(super) fn end(&self) -> u64 {
        (self.warmup_secs + self.measure_secs) * SEC
    }

    /// Fixed intervals with seeded jitter inside the first half of each, so every run of either
    /// version replays the same send times.
    pub(super) fn schedule(&self, endpoint: Endpoint) -> Vec<u64> {
        self.jittered(endpoint as u64 + 1, SEC / self.rps)
    }

    /// A lane's hooks every `interval_ms`, jittered like the endpoints but seeded apart.
    pub(super) fn policy_schedule(&self, lane: Lane, interval_ms: u64) -> Vec<u64> {
        self.jittered(lane as u64 + 3, interval_ms.max(1) * 1_000)
    }

    fn jittered(&self, salt: u64, interval: u64) -> Vec<u64> {
        let mut state = self.seed ^ salt.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (0..self.end() / interval)
            .map(|slot| slot * interval + splitmix64(&mut state) % (interval / 2))
            .collect()
    }

    /// Tick slot `k` is due at `k` seconds; the measured ones start at the warm-up's end.
    pub(super) fn tick_slots(&self) -> u64 {
        self.warmup_secs + self.measure_secs
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Each count is None when it was not observed or not readable back; never recorded as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Sample {
    pub(super) threads: Option<usize>,
    pub(super) pool_size: Option<u32>,
    pub(super) pool_idle: Option<usize>,
    pub(super) proxy_accepted: Option<usize>,
    pub(super) proxy_open: Option<usize>,
}

impl Sample {
    pub(super) const FIELDS: [&str; 5] = [
        "threads",
        "pool_size",
        "pool_idle",
        "proxy_accepted",
        "proxy_open",
    ];

    pub(super) fn field(&self, name: &str) -> Option<usize> {
        match name {
            "threads" => self.threads,
            "pool_size" => self.pool_size.map(|size| size as usize),
            "pool_idle" => self.pool_idle,
            "proxy_accepted" => self.proxy_accepted,
            "proxy_open" => self.proxy_open,
            _ => None,
        }
    }
}

/// Highs sampled through the measured window; `beyond_pool` is open proxy connections minus
/// server pool connections at one instant, the estimate of reader connections `C`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Peak {
    pub(super) proxy_open: Option<usize>,
    pub(super) pool_size: Option<u32>,
    pub(super) beyond_pool: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Resources {
    pub(super) start: Sample,
    pub(super) end: Sample,
    pub(super) pool_max: u32,
    /// Statements the proxy saw executed inside the measured window.
    pub(super) pg_statements: usize,
    pub(super) peak: Peak,
    /// Window executions of the two policy read SELECTs.
    pub(super) policy_statements: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct RunData {
    pub(super) name: String,
    pub(super) conditions: BTreeMap<String, String>,
    pub(super) attempts: Vec<Attempt>,
    /// `(slot, woke)` for each slot the probe ran; a skipped slot is absent.
    pub(super) ticks: Vec<(u64, u64)>,
    pub(super) resources: Resources,
    pub(super) policy: Vec<PolicyAttempt>,
    pub(super) acquires: Vec<Acquire>,
    pub(super) claims: Vec<ClaimAttempt>,
    pub(super) blocking: Vec<BlockingRead>,
    /// Problems already named while loading the run (runner evidence).
    pub(super) invalid: Vec<String>,
}

impl RunData {
    /// A run recorded before profiles existed is the health-only one.
    pub(super) fn profile(&self) -> Option<Profile> {
        self.conditions
            .get("profile")
            .map_or(Some(Profile::S1), |name| Profile::parse(name))
    }

    pub(super) fn condition(&self, key: &str) -> Option<u64> {
        self.conditions.get(key)?.parse().ok()
    }

    pub(super) fn plan(&self) -> Option<Plan> {
        let get = |key: &str| self.conditions.get(key)?.parse::<u64>().ok();
        Some(Plan {
            seed: get("seed")?,
            rps: get("rps_per_endpoint")?.max(1),
            warmup_secs: get("warmup_secs")?,
            measure_secs: get("measure_secs")?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stat {
    /// Microseconds.
    Value(u64),
    Unresolved {
        incomplete: usize,
        total: usize,
    },
}

impl std::fmt::Display for Stat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stat::Value(us) => write!(f, "{:.1}ms", *us as f64 / 1000.0),
            Stat::Unresolved { incomplete, total } => {
                write!(f, "UNRESOLVED/INVALID (incomplete {incomplete} of {total})")
            }
        }
    }
}

/// Nearest rank over every attempt, `None` ranking as +inf; `permille` 1000 is the maximum.
pub(super) fn percentile(latencies: &[Option<u64>], permille: usize) -> Stat {
    let total = latencies.len();
    let mut done: Vec<u64> = latencies.iter().flatten().copied().collect();
    let incomplete = total - done.len();
    done.sort_unstable();
    let rank = (permille * total).div_ceil(1000).max(1);
    match done.get(rank - 1) {
        Some(value) if total > 0 => Stat::Value(*value),
        _ => Stat::Unresolved { incomplete, total },
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) attempts: usize,
    pub(super) completed: usize,
    pub(super) errors: usize,
    pub(super) timeouts: usize,
    pub(super) unsent: usize,
}

impl Counts {
    pub(super) fn incomplete(&self) -> usize {
        self.attempts - self.completed
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct EndpointSummary {
    pub(super) counts: Counts,
    /// Last byte minus scheduled send: the judged latency.
    pub(super) judged: Vec<Option<u64>>,
    /// Last byte minus actual send, reported only.
    pub(super) sent_basis: Vec<Option<u64>>,
    /// Actual minus scheduled send.
    pub(super) lag: Vec<Option<u64>>,
}

impl EndpointSummary {
    /// Counts one measured attempt; only a completed one gets a finite latency.
    fn record(&mut self, class: Class, scheduled: u64, sent: Option<u64>, last: Option<u64>) {
        let counts = &mut self.counts;
        counts.attempts += 1;
        match class {
            Class::Completed => counts.completed += 1,
            Class::Error => counts.errors += 1,
            Class::Timeout => counts.timeouts += 1,
            Class::Unsent => counts.unsent += 1,
        }
        let done = (class == Class::Completed).then_some(last).flatten();
        self.judged
            .push(done.map(|last| last.saturating_sub(scheduled)));
        self.sent_basis
            .push(done.zip(sent).map(|(last, sent)| last.saturating_sub(sent)));
        self.lag
            .push(sent.map(|sent| sent.saturating_sub(scheduled)));
    }

    fn absorb(&mut self, other: &EndpointSummary) {
        let (mine, theirs) = (&mut self.counts, &other.counts);
        mine.attempts += theirs.attempts;
        mine.completed += theirs.completed;
        mine.errors += theirs.errors;
        mine.timeouts += theirs.timeouts;
        mine.unsent += theirs.unsent;
        self.judged.extend_from_slice(&other.judged);
        self.sent_basis.extend_from_slice(&other.sent_basis);
        self.lag.extend_from_slice(&other.lag);
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct RunSummary {
    pub(super) name: String,
    pub(super) conditions: BTreeMap<String, String>,
    pub(super) endpoints: BTreeMap<Endpoint, EndpointSummary>,
    /// One delay per measured tick slot; a missed slot is `None`.
    pub(super) ticks: Vec<Option<u64>>,
    pub(super) resources: Resources,
    pub(super) reference: BTreeMap<Endpoint, Reference>,
    pub(super) profile: Profile,
    pub(super) policy: BTreeMap<Lane, EndpointSummary>,
    /// Server pool acquire waits, None for a failed one.
    pub(super) acquires: Vec<Option<u64>>,
    pub(super) claim: Option<EndpointSummary>,
    /// Rows the measured claims claimed.
    pub(super) claim_rows: usize,
    pub(super) blocking: Option<BlockingSummary>,
    pub(super) invalid: Vec<String>,
}

/// An answer counts only when it is whole JSON, observed the release manifest, exposes the
/// hostname exactly on detail, and repeats the warm-up's status and reasons.
pub(super) fn classify(attempt: &Attempt, reference: Option<&Reference>) -> Class {
    match &attempt.outcome {
        Outcome::Unsent => Class::Unsent,
        Outcome::Timeout => Class::Timeout,
        Outcome::Failed(_) => Class::Error,
        Outcome::Answered(answer) => {
            let intact = answer_intact(attempt.endpoint, answer)
                && reference.is_some_and(|(status, reasons)| {
                    *status == answer.status && *reasons == reason_set(&answer.reasons)
                });
            if intact && attempt.last_byte.is_some() {
                Class::Completed
            } else {
                Class::Error
            }
        }
    }
}

fn answer_intact(endpoint: Endpoint, answer: &Answer) -> bool {
    answer.parsed
        && answer.observation_status == "observed"
        && answer.node_hostname == (endpoint == Endpoint::Detail)
}

/// Reasons with only the outbox age's growing number dropped; every other value stays.
fn reason_set(reasons: &[String]) -> Vec<String> {
    let mut set: Vec<String> = reasons
        .iter()
        .map(|reason| match reason.rsplit_once(':') {
            Some((AGING_REASON, value))
                if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                AGING_REASON
            }
            _ => reason.as_str(),
        })
        .map(str::to_string)
        .collect();
    set.sort();
    set.dedup();
    set
}

/// The warm-up's most frequent intact status and reason set; ties go to the smaller value.
fn reference(attempts: &[&Attempt]) -> Option<Reference> {
    let mut seen: BTreeMap<Reference, usize> = BTreeMap::new();
    for attempt in attempts {
        if let Outcome::Answered(answer) = &attempt.outcome
            && answer_intact(attempt.endpoint, answer)
        {
            *seen
                .entry((answer.status, reason_set(&answer.reasons)))
                .or_default() += 1;
        }
    }
    let top = *seen.values().max()?;
    seen.into_iter()
        .find(|(_, count)| *count == top)
        .map(|(reference, _)| reference)
}

pub(super) fn summarize(run: &RunData) -> RunSummary {
    let mut summary = RunSummary {
        name: run.name.clone(),
        conditions: run.conditions.clone(),
        resources: run.resources,
        invalid: run.invalid.clone(),
        ..RunSummary::default()
    };
    let (Some(plan), Some(profile)) = (run.plan(), run.profile()) else {
        summary.invalid.push("conditions_unreadable".to_string());
        return summary;
    };
    summary.profile = profile;
    for endpoint in Endpoint::ALL {
        let mut attempts: Vec<&Attempt> = run
            .attempts
            .iter()
            .filter(|attempt| attempt.endpoint == endpoint)
            .collect();
        attempts.sort_by_key(|attempt| attempt.seq);
        let expected = plan.schedule(endpoint);
        let reconciled = attempts.len() == expected.len()
            && attempts
                .iter()
                .zip(&expected)
                .enumerate()
                .all(|(seq, (attempt, scheduled))| {
                    attempt.seq == seq && attempt.scheduled == *scheduled
                });
        if !reconciled {
            summary
                .invalid
                .push(format!("reconciliation_mismatch:{}", endpoint.name()));
        }
        if attempts
            .iter()
            .any(|attempt| attempt.measured != (attempt.scheduled >= plan.warmup_end()))
        {
            summary
                .invalid
                .push(format!("warmup_overlap:{}", endpoint.name()));
        }
        let (measured, warmup): (Vec<&Attempt>, Vec<&Attempt>) =
            attempts.into_iter().partition(|attempt| attempt.measured);
        let reference = reference(&warmup);
        let mut out = EndpointSummary::default();
        for attempt in measured {
            let class = classify(attempt, reference.as_ref());
            out.record(class, attempt.scheduled, attempt.sent, attempt.last_byte);
        }
        if out.counts.completed == 0 {
            summary
                .invalid
                .push(format!("no_completed:{}", endpoint.name()));
        }
        if let Some(reference) = reference {
            summary.reference.insert(endpoint, reference);
        }
        summary.endpoints.insert(endpoint, out);
    }
    let mut woke: BTreeMap<u64, u64> = BTreeMap::new();
    for (slot, at) in &run.ticks {
        if *slot >= plan.tick_slots() || woke.insert(*slot, *at).is_some() {
            summary.invalid.push("tick_record_mismatch".to_string());
        }
    }
    summary.ticks = (plan.warmup_secs..plan.tick_slots())
        .map(|slot| woke.get(&slot).map(|at| at.saturating_sub(slot * SEC)))
        .collect();
    summary
        .invalid
        .extend(resource_invalid(&run.resources, profile));
    if profile.has_policy() {
        summarize_policy(run, plan, &mut summary);
    }
    if profile == Profile::S3 {
        summarize_claims(run, plan, &mut summary);
    }
    summary.invalid.dedup();
    summary
}

/// Every count observed, threads steady, the server pool within its size and no larger at
/// the end; health-only runs open no connection at all once warm.
fn resource_invalid(resources: &Resources, profile: Profile) -> Vec<String> {
    let mut invalid = Vec::new();
    let (start, end) = (&resources.start, &resources.end);
    for (point, sample) in [("start", start), ("end", end)] {
        for field in Sample::FIELDS {
            if sample.field(field).is_none() {
                invalid.push(format!("resource_unobserved:{point}.{field}"));
            }
        }
    }
    let grew = |before: Option<usize>, after: Option<usize>| {
        before.zip(after).filter(|(before, after)| after > before)
    };
    if let (Some(before), Some(after)) = (start.threads, end.threads)
        && before.abs_diff(after) > THREAD_SLACK
    {
        invalid.push("thread_drift".to_string());
    }
    if start.pool_size.max(end.pool_size) > Some(resources.pool_max) {
        invalid.push("pool_over_cap".to_string());
    }
    if grew(start.field("pool_size"), end.field("pool_size")).is_some() {
        invalid.push("pool_growth".to_string());
    }
    if resources.pg_statements == 0 {
        invalid.push("pg_unqueried".to_string());
    }
    match profile {
        Profile::S1 => {
            if let Some((before, after)) = grew(start.proxy_accepted, end.proxy_accepted) {
                invalid.push(format!("connection_churn:{}", after - before));
            }
            if grew(start.proxy_open, end.proxy_open).is_some() {
                invalid.push("connection_growth".to_string());
            }
        }
        Profile::S2 | Profile::S3 => {
            let peak = &resources.peak;
            for (field, value) in [
                ("proxy_open", peak.proxy_open),
                ("pool_size", peak.pool_size.map(|size| size as usize)),
                ("beyond_pool", peak.beyond_pool),
            ] {
                if value.is_none() {
                    invalid.push(format!("resource_unobserved:peak.{field}"));
                }
            }
            if resources.policy_statements.is_none() {
                invalid.push("resource_unobserved:policy_statements".to_string());
            }
        }
    }
    invalid
}

/// Every run's attempts and measured ticks pooled into one version-wide summary.
pub(super) fn pooled(
    runs: &[RunSummary],
) -> (BTreeMap<Endpoint, EndpointSummary>, Vec<Option<u64>>) {
    let mut endpoints: BTreeMap<Endpoint, EndpointSummary> = BTreeMap::new();
    let mut ticks = Vec::new();
    for run in runs {
        for (endpoint, summary) in &run.endpoints {
            endpoints.entry(*endpoint).or_default().absorb(summary);
        }
        ticks.extend_from_slice(&run.ticks);
    }
    (endpoints, ticks)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    Improved,
    Worse,
    Unconfirmed,
    UnconfirmedControlMoved,
    Unresolved,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Improved => "개선",
            Verdict::Worse => "악화",
            Verdict::Unconfirmed => "이득 미확인",
            Verdict::UnconfirmedControlMoved => "이득 미확인(대조군 이동)",
            Verdict::Unresolved => "UNRESOLVED/INVALID",
        })
    }
}

/// One metric's p99 per run, pooled per version, and the verdict drawn from them.
#[derive(Clone, Debug)]
pub(super) struct MetricComparison {
    pub(super) verdict: Verdict,
    pub(super) base_runs: Vec<Stat>,
    pub(super) candidate_runs: Vec<Stat>,
    pub(super) base_pooled: Stat,
    pub(super) candidate_pooled: Stat,
    pub(super) base_incomplete: usize,
    pub(super) candidate_incomplete: usize,
}

impl MetricComparison {
    /// Candidate minus BASE pooled p99, in microseconds, when both resolved.
    pub(super) fn difference(&self) -> Option<i64> {
        match (self.base_pooled, self.candidate_pooled) {
            (Stat::Value(base), Stat::Value(candidate)) => Some(candidate as i64 - base as i64),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Comparison {
    pub(super) invalid: Vec<String>,
    pub(super) detail: MetricComparison,
    pub(super) public: MetricComparison,
    pub(super) tick: MetricComparison,
    /// Main-engine policy hooks, judged only when every run carries the policy load.
    pub(super) policy: Option<MetricComparison>,
    /// Dispatch claims, judged only when every run carries the claim load.
    pub(super) claim: Option<MetricComparison>,
}

fn values(stats: &[Stat]) -> Option<Vec<u64>> {
    stats
        .iter()
        .map(|stat| match stat {
            Stat::Value(value) => Some(*value),
            Stat::Unresolved { .. } => None,
        })
        .collect()
}

/// Improved only when every candidate run's p99 beats every BASE run's with no more unfinished
/// attempts; reversed ranges are worse, overlap is unconfirmed, any unresolved p99 unresolved.
fn metric_verdict(
    base: &[Stat],
    candidate: &[Stat],
    base_incomplete: usize,
    candidate_incomplete: usize,
) -> Verdict {
    let (Some(base), Some(candidate)) = (values(base), values(candidate)) else {
        return Verdict::Unresolved;
    };
    let (Some(base_min), Some(base_max)) = (base.iter().min(), base.iter().max()) else {
        return Verdict::Unresolved;
    };
    let (Some(candidate_min), Some(candidate_max)) =
        (candidate.iter().min(), candidate.iter().max())
    else {
        return Verdict::Unresolved;
    };
    if candidate_max < base_min && candidate_incomplete <= base_incomplete {
        Verdict::Improved
    } else if candidate_min > base_max {
        Verdict::Worse
    } else {
        Verdict::Unconfirmed
    }
}

fn metric(
    base: &[RunSummary],
    candidate: &[RunSummary],
    latencies: impl Fn(&RunSummary) -> Vec<Option<u64>>,
    valid: bool,
) -> MetricComparison {
    let p99 = |run: &RunSummary| percentile(&latencies(run), 990);
    let incomplete = |runs: &[RunSummary]| {
        runs.iter()
            .map(|run| {
                latencies(run)
                    .iter()
                    .filter(|value| value.is_none())
                    .count()
            })
            .sum::<usize>()
    };
    let pooled_p99 = |runs: &[RunSummary]| {
        let all: Vec<Option<u64>> = runs.iter().flat_map(&latencies).collect();
        percentile(&all, 990)
    };
    let base_runs: Vec<Stat> = base.iter().map(p99).collect();
    let candidate_runs: Vec<Stat> = candidate.iter().map(p99).collect();
    let (base_incomplete, candidate_incomplete) = (incomplete(base), incomplete(candidate));
    let verdict = if valid {
        metric_verdict(
            &base_runs,
            &candidate_runs,
            base_incomplete,
            candidate_incomplete,
        )
    } else {
        Verdict::Unresolved
    };
    MetricComparison {
        verdict,
        base_pooled: pooled_p99(base),
        candidate_pooled: pooled_p99(candidate),
        base_runs,
        candidate_runs,
        base_incomplete,
        candidate_incomplete,
    }
}

fn comparison_invalid(base: &[RunSummary], candidate: &[RunSummary]) -> Vec<String> {
    let mut invalid = BTreeSet::new();
    if base.is_empty() || candidate.is_empty() {
        invalid.insert("no_runs".to_string());
    }
    let all: Vec<&RunSummary> = base.iter().chain(candidate).collect();
    for run in &all {
        for reason in &run.invalid {
            invalid.insert(format!("run_invalid:{}:{reason}", run.name));
        }
    }
    if let Some(first) = all.first() {
        for run in &all[1..] {
            let keys: BTreeSet<&String> = first
                .conditions
                .keys()
                .chain(run.conditions.keys())
                .collect();
            for key in keys {
                if key != BINARY_KEY && first.conditions.get(key) != run.conditions.get(key) {
                    invalid.insert(format!("condition_mismatch:{key}"));
                }
            }
        }
    }
    let binaries = |runs: &[RunSummary]| -> BTreeSet<Option<String>> {
        runs.iter()
            .map(|run| run.conditions.get(BINARY_KEY).cloned())
            .collect()
    };
    for (version, runs) in [("base", base), ("candidate", candidate)] {
        let set = binaries(runs);
        if set.contains(&None) {
            invalid.insert(format!("binary_unrecorded:{version}"));
        } else if set.len() > 1 {
            invalid.insert(format!("binary_mismatch:{version}"));
        }
        let (endpoints, ticks) = pooled(runs);
        for endpoint in Endpoint::ALL {
            let attempts = endpoints.get(&endpoint).map_or(0, |e| e.counts.attempts);
            if attempts < MIN_ATTEMPTS {
                invalid.insert(format!(
                    "insufficient_samples:{version}:{}:{attempts}<{MIN_ATTEMPTS}",
                    endpoint.name()
                ));
            }
        }
        if ticks.len() < MIN_TICKS {
            invalid.insert(format!(
                "insufficient_ticks:{version}:{}<{MIN_TICKS}",
                ticks.len()
            ));
        }
    }
    if !base.is_empty() && !candidate.is_empty() && binaries(base) == binaries(candidate) {
        invalid.insert("same_binary_both_versions".to_string());
    }
    // A candidate that answers with another status or reason set is not the same health.
    for endpoint in Endpoint::ALL {
        let references: BTreeSet<Option<&Reference>> =
            all.iter().map(|run| run.reference.get(&endpoint)).collect();
        if references.len() > 1 {
            invalid.insert(format!("reference_mismatch:{}", endpoint.name()));
        }
    }
    invalid.extend(candidate_invalid(candidate));
    invalid.into_iter().collect()
}

pub(super) fn compare(base: &[RunSummary], candidate: &[RunSummary]) -> Comparison {
    let invalid = comparison_invalid(base, candidate);
    let valid = invalid.is_empty();
    let judged = |endpoint: Endpoint| {
        move |run: &RunSummary| {
            run.endpoints
                .get(&endpoint)
                .map(|summary| summary.judged.clone())
                .unwrap_or_default()
        }
    };
    let mut detail = metric(base, candidate, judged(Endpoint::Detail), valid);
    let public = metric(base, candidate, judged(Endpoint::Public), valid);
    let tick = metric(base, candidate, |run| run.ticks.clone(), valid);
    let all_s2 = base
        .iter()
        .chain(candidate)
        .all(|run| run.profile.has_policy());
    // Health-only, public never reads the hostname, so its movement means something else moved.
    if !all_s2
        && detail.verdict == Verdict::Improved
        && matches!(public.verdict, Verdict::Improved | Verdict::Worse)
    {
        detail.verdict = Verdict::UnconfirmedControlMoved;
    }
    let policy = (all_s2 && !base.is_empty() && !candidate.is_empty()).then(|| {
        let main = |run: &RunSummary| {
            run.policy
                .get(&Lane::Main)
                .map(|summary| summary.judged.clone())
                .unwrap_or_default()
        };
        metric(base, candidate, main, valid)
    });
    let all_s3 = base
        .iter()
        .chain(candidate)
        .all(|run| run.profile == Profile::S3);
    let claim = (all_s3 && !base.is_empty() && !candidate.is_empty()).then(|| {
        let claims = |run: &RunSummary| {
            run.claim
                .as_ref()
                .map(|summary| summary.judged.clone())
                .unwrap_or_default()
        };
        metric(base, candidate, claims, valid)
    });
    Comparison {
        invalid,
        detail,
        public,
        tick,
        policy,
        claim,
    }
}

/// Only a loopback cluster on an explicit port other than the operator's 15432 may be loaded.
pub(super) fn isolated_pg_base(base: Option<&str>) -> Result<(), String> {
    let base = base.ok_or("POSTGRES_TEST_DATABASE_URL_BASE is unset")?;
    let rest = base
        .strip_prefix("postgres://")
        .or_else(|| base.strip_prefix("postgresql://"))
        .ok_or_else(|| format!("not a postgres URL: {base}"))?;
    let authority = rest.split(['/', '?']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let (host, port) = host_port
        .rsplit_once(':')
        .ok_or_else(|| format!("no explicit port: {base}"))?;
    let host: IpAddr = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| format!("host is not an IP literal: {base}"))?;
    if !host.is_loopback() {
        return Err(format!("host is not loopback: {base}"));
    }
    let port: u16 = port.parse().map_err(|_| format!("bad port: {base}"))?;
    if port == OPERATOR_PG_PORT {
        return Err(format!(
            "port {OPERATOR_PG_PORT} is the operator cluster: {base}"
        ));
    }
    Ok(())
}
