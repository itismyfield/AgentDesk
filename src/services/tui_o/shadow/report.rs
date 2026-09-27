//! `o-shadow report`: judges one window against the E1 sample, population and classification bar.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::metrics::MetricsSnapshot;
use super::{
    DeriveOutput, DiffCause, DiffClass, DiffRecord, IDENTITY_VERSION, MATCH_WINDOW,
    PopulationSnapshot, PopulationSource, REPORT_VERSION, SCHEMA_VERSION, ShadowProvider,
    ShadowRecord, ShadowTurn, ShadowUnit, SourceId, SyntheticEntry, UnitKey, UnitKind,
};
use crate::services::agent_protocol::RuntimeHandoffKind;

pub const MIN_TOTAL_TURNS: usize = 30;
pub const MIN_PROFILE_TURNS: usize = 10;
pub const MIN_TOOL_TURNS: usize = 3;
pub const MIN_SPLIT_TURNS: usize = 1;
/// The fixed measurement window; a longer one would admit turns the design does not count.
pub const WINDOW_MINUTES: i64 = 120;
/// Criteria the records cannot show; the coordinator records them next to the verdict.
pub const EXTERNAL_CHECKS: [&str; 2] = ["write_zero_audit", "resource_limits"];

/// Provider behind a TUI runtime kind; exhaustive so a new kind fails to compile here.
fn tui_provider(kind: RuntimeHandoffKind) -> Option<&'static str> {
    match kind {
        RuntimeHandoffKind::ClaudeTui => Some("claude"),
        RuntimeHandoffKind::CodexTui => Some("codex"),
        RuntimeHandoffKind::LegacyTmuxWrapper
        | RuntimeHandoffKind::ProcessBackend
        | RuntimeHandoffKind::ClaudeEAdapter => None,
    }
}

/// TUI profile of a provider id, or `unknown:<id>` when no TUI runtime kind serves it.
pub fn profile_of(provider: &str) -> String {
    use RuntimeHandoffKind::*;
    [
        LegacyTmuxWrapper,
        ClaudeTui,
        CodexTui,
        ProcessBackend,
        ClaudeEAdapter,
    ]
    .into_iter()
    .find(|kind| tui_provider(*kind) == Some(provider))
    .map_or_else(
        || format!("unknown:{provider}"),
        |kind| kind.as_str().to_string(),
    )
}

fn provider_id(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude",
        ShadowProvider::Codex => "codex",
    }
}

/// S2: TUI kinds the shadow bound in the run live at `from` and any later run up to `to`.
pub fn bound_kinds(
    records: &[ShadowRecord],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    read_at: DateTime<Utc>,
) -> PopulationSource {
    let mut kinds = BTreeSet::new();
    for record in records {
        match record {
            ShadowRecord::Header { started_at, .. } if *started_at <= from => kinds.clear(),
            ShadowRecord::Binding { change } if change.at <= to => {
                kinds.extend(
                    change
                        .new
                        .iter()
                        .map(|b| profile_of(provider_id(b.provider))),
                );
            }
            _ => {}
        }
    }
    let observed_kinds = kinds.into_iter().collect();
    PopulationSource {
        name: "s2_bindings".into(),
        read_at,
        ok: true,
        observed_kinds,
    }
}

/// Channels bound when `to` was reached, with the provider serving each; a new run rebinds from empty.
pub fn bound_channels(records: &[ShadowRecord], to: DateTime<Utc>) -> BTreeMap<u64, String> {
    let mut bound = BTreeMap::new();
    for record in records {
        match record {
            ShadowRecord::Header { started_at, .. } if *started_at <= to => bound.clear(),
            ShadowRecord::Binding { change } if change.at <= to => {
                let provider = change.new.as_ref().map(|b| provider_id(b.provider));
                match provider {
                    Some(provider) => bound.insert(change.channel_id, provider.to_string()),
                    None => bound.remove(&change.channel_id),
                };
            }
            _ => {}
        }
    }
    bound
}

/// Sources the latest observer run still has attached, with their attach time: cleared by a new
/// header, dropped when rebound away or broken by an anomaly.
pub fn attached_sources(records: &[ShadowRecord]) -> Vec<(SourceId, DateTime<Utc>)> {
    let mut attached: Vec<(SourceId, DateTime<Utc>)> = Vec::new();
    for record in records {
        match record {
            ShadowRecord::Header { .. } => attached.clear(),
            ShadowRecord::Attach {
                source,
                attached_at,
                ..
            } => attached.push((source.clone(), *attached_at)),
            ShadowRecord::Binding { change } => {
                attached.retain(|(s, _)| change.old.as_ref().is_none_or(|old| old.source != *s))
            }
            ShadowRecord::Anomaly { anomaly } => attached.retain(|(s, _)| *s != anomaly.source),
            _ => {}
        }
    }
    attached
}

/// One operator verdict from `report --classify`; only Expected, Legacy_defect and O_defect apply.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Classification {
    pub diff_key: String,
    pub cause: DiffCause,
    pub note: String,
}

/// The `--classify` file; an unreadable one fails the report instead of being skipped.
pub enum ClassifyInput {
    Absent,
    Entries(Vec<Classification>),
    Unreadable(String),
}

/// A non-Match diff in the window: the cause it was recorded with and the one judged.
#[derive(Debug, Serialize)]
pub struct DiffEntry {
    pub diff_key: String,
    pub class: DiffClass,
    pub recorded: DiffCause,
    pub cause: DiffCause,
}

pub struct ReportInput<'a> {
    pub records: &'a [ShadowRecord],
    pub manifest: &'a [SyntheticEntry],
    pub population: &'a PopulationSnapshot,
    pub allowlist: &'a [u64],
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub reported_at: DateTime<Utc>,
    pub classify: &'a ClassifyInput,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct ProfileCounts {
    pub turns: usize,
    pub tool_turns: usize,
    pub split_turns: usize,
    pub synthetic_turns: usize,
}

#[derive(Debug, Serialize)]
pub struct ReportOutcome {
    pub pass: bool,
    pub failures: Vec<String>,
    pub warnings: Vec<String>,
    pub external_checks: [&'static str; 2],
    /// `[schema, identity, report]` versions this verdict was computed under.
    pub versions: [u32; 3],
    pub t0: DateTime<Utc>,
    pub t1: DateTime<Utc>,
    pub total_turns: usize,
    pub profiles: BTreeMap<String, ProfileCounts>,
    pub uncounted_turns: BTreeMap<String, usize>,
    pub synthetic: BTreeMap<String, &'static str>,
    pub metrics: MetricsSnapshot,
    pub diffs: Vec<DiffEntry>,
    /// Cause totals over every in-window diff, as recorded and after operator classification.
    pub causes_before: BTreeMap<String, usize>,
    pub causes_after: BTreeMap<String, usize>,
    /// Operator changes keyed `recorded->classified`.
    pub reclassified: BTreeMap<String, usize>,
    /// Operator entries that replaced an automatic `OOnlyTool` cause.
    pub auto_overridden: usize,
    pub o_only_tool: usize,
}

fn record_time(
    record: &ShadowRecord,
    units: &HashMap<&UnitKey, &ShadowUnit>,
    legacy: &HashMap<u64, DateTime<Utc>>,
) -> Option<DateTime<Utc>> {
    match record {
        ShadowRecord::Header { started_at: at, .. }
        | ShadowRecord::Attach {
            attached_at: at, ..
        }
        | ShadowRecord::WindowStart { t0: at, .. }
        | ShadowRecord::Binding {
            change: super::BindingChange { at, .. },
        } => Some(*at),
        ShadowRecord::Population { snapshot } => Some(snapshot.taken_at),
        ShadowRecord::Legacy { msg } => Some(msg.created_at),
        ShadowRecord::Derived {
            output: DeriveOutput::Sealed(unit),
        } => Some(unit.sealed_at),
        ShadowRecord::Derived {
            output: DeriveOutput::TurnClosed(turn),
        } => Some(turn.closed_at),
        // A diff belongs to the sample it judges: its unit's sealing, else its Legacy post.
        ShadowRecord::Diff { diff } => match &diff.unit_key {
            Some(key) => units.get(key).map(|u| u.sealed_at),
            None => diff
                .legacy_msg_ids
                .iter()
                .filter_map(|id| legacy.get(id))
                .min()
                .copied(),
        },
        _ => None,
    }
}

pub fn evaluate(input: &ReportInput) -> ReportOutcome {
    let (mut failures, mut warnings) = (Vec::new(), Vec::new());
    let starts: Vec<_> = (input.records.iter().enumerate())
        .filter_map(|(at, r)| match r {
            ShadowRecord::WindowStart { t0, sources } if (input.from..=input.to).contains(t0) => {
                Some((at, *t0, sources))
            }
            _ => None,
        })
        .collect();
    if starts.len() != 1 {
        let found = starts.len();
        failures.push(format!(
            "window needs exactly one window_start record, found {found}"
        ));
    }
    let t0 = starts.first().map_or(input.from, |(_, t0, _)| *t0);
    // The window is fixed at two hours from the recorded t0; `--to` must name that instant.
    let t1 = t0 + Duration::minutes(WINDOW_MINUTES);
    if (input.to - t1).abs() >= Duration::seconds(1) {
        failures.push(format!(
            "window end {} is not t0 + {WINDOW_MINUTES} minutes ({t1})",
            input.to
        ));
    }
    // Closers at or below this extent were on disk before t0 (or before a mid-window attach).
    let mut boundary: HashMap<&SourceId, u64> = HashMap::new();
    if let Some((at, t0, sources)) = starts.first() {
        boundary.extend(sources.iter().map(|s| (&s.source, s.window_start_extent)));
        // Attach rows from before t0 may land after the WindowStart line; the run's later rows count too.
        let later = input.records[at + 1..].iter();
        let later = later.take_while(|r| !matches!(r, ShadowRecord::Header { .. }));
        let later = later.filter_map(|r| match r {
            ShadowRecord::Attach {
                source,
                attached_at,
                ..
            } => Some((source.clone(), *attached_at)),
            _ => None,
        });
        let missed = attached_sources(&input.records[..*at])
            .into_iter()
            .chain(later)
            .filter(|(source, attached_at)| attached_at < t0 && !boundary.contains_key(source))
            .map(|(source, _)| source)
            .collect::<HashSet<_>>()
            .len();
        if missed > 0 {
            failures.push(format!(
                "window_start missed {missed} source(s) attached before t0"
            ));
        }
    }
    let mut units: HashMap<&UnitKey, &ShadowUnit> = HashMap::new();
    let (mut excluded, mut legacy) = (HashSet::new(), HashMap::new());
    let mut turns: Vec<&ShadowTurn> = Vec::new();
    for record in input.records {
        match record {
            ShadowRecord::Attach {
                source,
                attach_extent,
                attached_at,
                ..
            } if (t0..=t1).contains(attached_at) => {
                boundary.entry(source).or_insert(*attach_extent);
            }
            ShadowRecord::Derived {
                output: DeriveOutput::Sealed(unit),
            } => {
                units.insert(&unit.unit_key, unit);
            }
            ShadowRecord::Derived {
                output: DeriveOutput::TurnClosed(turn),
            } => turns.push(turn),
            ShadowRecord::Derived {
                output: DeriveOutput::Excluded { unit_key, .. },
            } => {
                excluded.insert(unit_key);
            }
            ShadowRecord::Legacy { msg } => {
                legacy.insert(msg.msg_id, msg.created_at);
            }
            _ => {}
        }
    }

    // Version mix and in-window totals; timeless rows take the latest earlier timestamp.
    let late = t1 + Duration::seconds(2 * MATCH_WINDOW.as_secs() as i64);
    // Units and Legacy rows near t1 are judged only after two match windows.
    if input.reported_at < late {
        failures.push(format!(
            "reported before {late}, when the last diffs are judged"
        ));
    }
    let (mut header, mut clock, mut stale) = (None, None, 0);
    let mut metrics = MetricsSnapshot::default();
    let mut classes: HashMap<&UnitKey, DiffClass> = HashMap::new();
    let mut window_diffs: Vec<&DiffRecord> = Vec::new();
    for record in input.records {
        if let ShadowRecord::Header {
            schema_version,
            identity_version,
            ..
        } = record
        {
            header = Some((*schema_version, *identity_version));
        }
        let own = record_time(record, &units, &legacy);
        clock = clock.max(own);
        if let ShadowRecord::Diff { diff } = record {
            if let Some(key) = &diff.unit_key {
                classes.insert(key, diff.class);
            }
        }
        if own.or(clock).is_some_and(|at| t0 <= at && at <= t1) {
            stale += usize::from(header != Some((SCHEMA_VERSION, IDENTITY_VERSION)));
            metrics.record(record);
            if let ShadowRecord::Diff { diff } = record {
                window_diffs.push(diff);
            }
        }
    }
    if stale > 0 {
        failures.push(format!(
            "stale samples: {stale} in-window records under another version"
        ));
    }

    let (mut seen, mut counted, mut uncounted) = (HashSet::new(), Vec::new(), BTreeMap::new());
    for turn in turns
        .into_iter()
        .filter(|t| (t0..=t1).contains(&t.closed_at))
    {
        let key = (turn.channel_id, turn.provider, turn.native_turn_id.as_str());
        let reason = if !turn.live {
            turn.excluded_reason
                .clone()
                .unwrap_or_else(|| "not_live".into())
        } else if boundary
            .get(&turn.source_range.source)
            .is_none_or(|b| turn.source_range.end <= *b)
        {
            "before_window_boundary".into()
        } else if !seen.insert(key) {
            "duplicate_turn_key".into()
        } else {
            counted.push(turn);
            continue;
        };
        *uncounted.entry(reason).or_insert(0) += 1;
    }

    // A turn open across t0 can carry warm-up units; only units sealed past the boundary are samples.
    let sampled = |key: &UnitKey| {
        units.get(key).is_some_and(|u| {
            let bound = boundary.get(&u.source_range.source);
            (t0..=t1).contains(&u.sealed_at) && bound.is_some_and(|b| u.source_range.start >= *b)
        })
    };
    let is_split = |key: &UnitKey| {
        key.kind == UnitKind::Body
            && sampled(key)
            && units.get(key).is_some_and(|u| u.pieces.len() >= 2)
            && !matches!(
                classes.get(key),
                Some(DiffClass::OSchemaBlocked | DiffClass::OUnsealed)
            )
    };
    let population = input.population;
    let tokens: HashSet<&str> = input.manifest.iter().map(|e| e.token.as_str()).collect();
    let mut profiles: BTreeMap<String, ProfileCounts> = population
        .profiles
        .iter()
        .map(|p| (p.clone(), ProfileCounts::default()))
        .collect();
    for turn in &counted {
        let counts = profiles
            .entry(profile_of(provider_id(turn.provider)))
            .or_default();
        counts.turns += 1;
        let tool = |k: &UnitKey| k.kind == UnitKind::Tool && sampled(k);
        counts.tool_turns += usize::from(turn.unit_keys.iter().any(tool));
        counts.split_turns += usize::from(turn.unit_keys.iter().any(is_split));
        counts.synthetic_turns += usize::from(
            turn.synthetic_tokens
                .iter()
                .any(|t| tokens.contains(t.as_str())),
        );
    }

    let mut synthetic = BTreeMap::new();
    for entry in input.manifest.iter().filter(|e| e.created_at <= t1) {
        let channel = population
            .channels
            .iter()
            .find(|c| c.channel_id == entry.channel_id);
        if channel.map(|c| profile_of(&c.provider)).as_deref()
            != Some(entry.expected_runtime_kind.as_str())
        {
            failures.push(format!(
                "synthetic {}: channel is not effective {}",
                entry.entry_id, entry.expected_runtime_kind
            ));
        }
        let hit = counted
            .iter()
            .find(|t| t.synthetic_tokens.contains(&entry.token));
        if hit.is_some_and(|t| profile_of(provider_id(t.provider)) != entry.expected_runtime_kind) {
            failures.push(format!(
                "synthetic {}: ran under another profile",
                entry.entry_id
            ));
        }
        let status = match hit {
            None => "not_executed",
            Some(turn) if turn.synthetic_tokens.len() >= 2 => "merged",
            Some(_) => "live",
        };
        synthetic.insert(entry.entry_id.clone(), status);
    }

    let in_population: BTreeSet<&String> = population.profiles.iter().collect();
    if in_population.is_empty() {
        failures.push("population is empty".into());
    }
    for profile in &population.profiles {
        let allowlisted = population.channels.iter().any(|c| {
            profile_of(&c.provider) == *profile && input.allowlist.contains(&c.channel_id)
        });
        if profile.starts_with("unknown:") {
            failures.push(format!(
                "{profile}: effective TUI without a TUI runtime kind"
            ));
        } else if !allowlisted {
            failures.push(format!("{profile}: no allowlisted effective-TUI channel"));
        }
    }
    for aux in &population.aux {
        if !aux.ok {
            warnings.push(format!("coverage_unverified: {}", aux.name));
        }
        for kind in aux
            .observed_kinds
            .iter()
            .filter(|k| !in_population.contains(k))
        {
            failures.push(format!(
                "{}: observed {kind} outside the population",
                aux.name
            ));
        }
    }
    for (profile, counts) in &profiles {
        let bar = (MIN_PROFILE_TURNS, MIN_TOOL_TURNS, MIN_SPLIT_TURNS);
        if !in_population.contains(profile) {
            failures.push(format!(
                "{profile}: {} turns outside the population",
                counts.turns
            ));
        } else if counts.turns < bar.0 || counts.tool_turns < bar.1 || counts.split_turns < bar.2 {
            failures.push(format!("{profile}: below sample bar {counts:?}"));
        }
    }
    if counted.len() < MIN_TOTAL_TURNS {
        failures.push(format!(
            "total live turns {} < {MIN_TOTAL_TURNS}",
            counted.len()
        ));
    }
    // Completion is proven per unit by a terminal diff, never inferred from elapsed time.
    let decided: HashSet<&UnitKey> = (input.records.iter())
        .filter_map(|r| match r {
            ShadowRecord::Diff { diff } => diff.unit_key.as_ref(),
            _ => None,
        })
        .collect();
    let undecided = (units.values())
        .filter(|u| (t0..=t1).contains(&u.sealed_at) && !decided.contains(&u.unit_key))
        .count();
    let unsealed = (counted.iter().flat_map(|t| &t.unit_keys))
        .filter(|k| !units.contains_key(k) && !excluded.contains(k))
        .collect::<HashSet<_>>()
        .len();
    let windowless = uncounted.get("no_window").copied().unwrap_or(0) as u64;
    for (count, what) in [
        (undecided as u64, "window units without a terminal diff"),
        (unsealed as u64, "units of counted turns never sealed"),
        (
            windowless,
            "turns closed before the observer applied the window",
        ),
    ] {
        if count > 0 {
            failures.push(format!("{count} {what}"));
        }
    }
    let judged = classify(&window_diffs, input.classify, &mut failures);
    let after = |cause| judged.causes_after.get(&label(cause)).copied().unwrap_or(0) as u64;
    for (count, what) in [
        (after(DiffCause::Unknown), "diffs still Unknown"),
        (after(DiffCause::ODefect), "diffs classified O_defect"),
        (
            metrics.split_over_limit_total,
            "split pieces over the Discord limit",
        ),
        (metrics.tap_dropped_total, "tap events dropped"),
    ] {
        if count > 0 {
            failures.push(format!("{count} {what}"));
        }
    }
    ReportOutcome {
        pass: failures.is_empty(),
        failures,
        warnings,
        external_checks: EXTERNAL_CHECKS,
        versions: [SCHEMA_VERSION, IDENTITY_VERSION, REPORT_VERSION],
        t0,
        t1,
        total_turns: counted.len(),
        profiles,
        uncounted_turns: uncounted,
        synthetic,
        metrics,
        diffs: judged.diffs,
        causes_before: judged.causes_before,
        causes_after: judged.causes_after,
        reclassified: judged.reclassified,
        auto_overridden: judged.auto_overridden,
        o_only_tool: judged.o_only_tool,
    }
}

fn label(value: impl Serialize) -> String {
    let value = serde_json::to_value(value).ok();
    let text = value.as_ref().and_then(|v| v.as_str());
    text.unwrap_or_default().to_string()
}

/// Operator-facing identity of a diff: its unit key, else its Legacy ids, else its class.
fn diff_key(diff: &DiffRecord) -> String {
    let (channel, class) = (diff.channel_id, label(diff.class));
    match (&diff.unit_key, diff.legacy_msg_ids.as_slice()) {
        (Some(k), _) => {
            let (provider, kind) = (label(k.provider), label(k.kind));
            format!("{}/{provider}/{kind}/{}", k.channel_id, k.native_key)
        }
        (None, []) => format!("{channel}/{class}"),
        (None, ids) => {
            let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
            format!("{channel}/{class}/{}", ids.join("+"))
        }
    }
}

#[derive(Default)]
struct Judged {
    diffs: Vec<DiffEntry>,
    causes_before: BTreeMap<String, usize>,
    causes_after: BTreeMap<String, usize>,
    reclassified: BTreeMap<String, usize>,
    auto_overridden: usize,
    o_only_tool: usize,
}

/// Applies operator causes to the window's non-Match diffs; every input defect is a failure.
fn classify(diffs: &[&DiffRecord], input: &ClassifyInput, failures: &mut Vec<String>) -> Judged {
    let mut judged = Judged::default();
    let mut seen_keys: HashMap<String, usize> = HashMap::new();
    for diff in diffs {
        *judged.causes_before.entry(label(diff.cause)).or_default() += 1;
        judged.o_only_tool += usize::from(diff.cause == DiffCause::OOnlyTool);
        if diff.class == DiffClass::Match {
            *judged.causes_after.entry(label(diff.cause)).or_default() += 1;
            continue;
        }
        let base = diff_key(diff);
        let n = seen_keys.entry(base.clone()).or_default();
        *n += 1;
        let diff_key = if *n == 1 { base } else { format!("{base}#{n}") };
        let (class, recorded, cause) = (diff.class, diff.cause, diff.cause);
        judged.diffs.push(DiffEntry {
            diff_key,
            class,
            recorded,
            cause,
        });
    }
    let entries = match input {
        ClassifyInput::Absent => &[][..],
        ClassifyInput::Entries(entries) => entries.as_slice(),
        ClassifyInput::Unreadable(error) => {
            failures.push(format!("classify: unreadable input: {error}"));
            &[][..]
        }
    };
    let mut used = HashSet::new();
    for entry in entries {
        let key = &entry.diff_key;
        let operator_cause = matches!(
            entry.cause,
            DiffCause::Expected | DiffCause::LegacyDefect | DiffCause::ODefect
        );
        let target = judged.diffs.iter_mut().find(|d| &d.diff_key == key);
        let problem = if entry.note.trim().is_empty() {
            "has an empty note"
        } else if !used.insert(key.as_str()) {
            "is listed twice"
        } else if !operator_cause {
            "names a cause operators cannot assign"
        } else if target.is_none() {
            "is not a diff of this report"
        } else {
            ""
        };
        match target {
            Some(diff) if problem.is_empty() => {
                if diff.recorded == DiffCause::OOnlyTool {
                    judged.auto_overridden += 1;
                }
                if diff.recorded != entry.cause {
                    let pair = format!("{}->{}", label(diff.recorded), label(entry.cause));
                    *judged.reclassified.entry(pair).or_default() += 1;
                }
                diff.cause = entry.cause;
            }
            _ => failures.push(format!("classify: {key} {problem}")),
        }
    }
    for diff in &judged.diffs {
        *judged.causes_after.entry(label(diff.cause)).or_default() += 1;
    }
    judged
}
