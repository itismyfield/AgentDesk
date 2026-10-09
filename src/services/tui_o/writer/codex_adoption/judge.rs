//! The one evaluator the probe and the verifier share. The boundary rule takes Legacy's consumption
//! boundary, retired sources and no remaining obligation; the strict rule also needs receipts.

use std::collections::BTreeSet;
use std::fmt::Debug;

use super::{
    Anchor, CandidateState, Closed, Coverage, Evidence, Load, Obligation, Parse, Proof, Provider,
    Retirement, Role, RuntimeKind, SourceEvidence, SourceRole, Step, StoreState, Suffix,
    WrapperEvidence,
};

/// Read budget over every source and wrapper of a channel; past it the channel is refused.
pub const BUDGET_SOURCES: usize = 64;
pub const BUDGET_BYTES: u64 = 128 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Eligible,
    Refused,
    /// No refusal, but some evidence could not be confirmed; adoption treats it as a refusal.
    Unknown,
    /// Not a Fresh, Pending Codex TUI channel with history.
    OutOfScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judgment {
    pub boundary: Verdict,
    pub strict: Verdict,
    /// Codes that refused or left a rule unknown; `strict.` codes bear on the strict rule only.
    pub refused: BTreeSet<String>,
    pub unknown: BTreeSet<String>,
}

#[derive(Clone, Default)]
struct Found {
    refused: BTreeSet<String>,
    unknown: BTreeSet<String>,
}

impl Found {
    fn refuse(&mut self, code: impl Into<String>) {
        self.refused.insert(code.into());
    }

    fn unknown(&mut self, code: impl Into<String>) {
        self.unknown.insert(code.into());
    }

    fn verdict(&self) -> Verdict {
        match (self.refused.is_empty(), self.unknown.is_empty()) {
            (false, _) => Verdict::Refused,
            (true, false) => Verdict::Unknown,
            (true, true) => Verdict::Eligible,
        }
    }
}

/// `CodexTui` as `codex_tui`: the snake-case name a code or a probe field carries.
pub(super) fn snake(value: &impl Debug) -> String {
    let mut name = String::new();
    for (index, ch) in format!("{value:?}").chars().enumerate() {
        if ch.is_ascii_uppercase() && index > 0 {
            name.push('_');
        }
        name.push(ch.to_ascii_lowercase());
    }
    name
}

pub fn judge(evidence: &Evidence) -> Judgment {
    if let Some(code) = out_of_scope(evidence) {
        return Judgment {
            boundary: Verdict::OutOfScope,
            strict: Verdict::OutOfScope,
            refused: BTreeSet::from([code.to_owned()]),
            unknown: BTreeSet::new(),
        };
    }
    let mut found = Found::default();
    channel(evidence, &mut found);
    sources(&evidence.sources, &mut found);
    let boundary = found.verdict();
    for source in evidence.sources.iter().filter(|s| bound(s)) {
        match source.receipts {
            Coverage::Complete => {}
            Coverage::Gap => found.refuse("strict.receipts_gap"),
            Coverage::Unknown => found.unknown("strict.receipts"),
        }
    }
    Judgment {
        boundary,
        strict: found.verdict(),
        refused: found.refused,
        unknown: found.unknown,
    }
}

fn bound(source: &SourceEvidence) -> bool {
    matches!(source.role, SourceRole::Current | SourceRole::Retired)
}

/// Only a definite state leaves the denominator; an unstated one stays in it as Unknown.
fn out_of_scope(evidence: &Evidence) -> Option<&'static str> {
    if evidence.provider == Provider::Claude || evidence.runtime_kind == RuntimeKind::ClaudeTui {
        return Some("scope.claude");
    }
    if matches!(evidence.store, StoreState::Store | StoreState::Held) {
        return Some("scope.store_exists");
    }
    if !matches!(
        evidence.candidate,
        CandidateState::Pending | CandidateState::Unknown
    ) {
        return Some("scope.candidate_decided");
    }
    let sources = &evidence.sources;
    let empty = |source: &SourceEvidence| source.bytes == Some(0);
    (sources.iter().any(bound) && sources.iter().all(empty)).then_some("scope.empty_history")
}

fn channel(evidence: &Evidence, found: &mut Found) {
    let unstated = [
        (evidence.provider == Provider::Unknown, "scope.provider"),
        (
            evidence.runtime_kind == RuntimeKind::Unknown,
            "scope.runtime_kind",
        ),
        (evidence.store == StoreState::Unknown, "scope.store"),
        (
            evidence.candidate == CandidateState::Unknown,
            "scope.candidate",
        ),
    ];
    for (_, code) in unstated.iter().filter(|(unstated, _)| *unstated) {
        found.unknown(*code);
    }
    match evidence.role {
        Role::Gateway | Role::RestWorker => {}
        Role::Standby | Role::Utility => found.refuse("role_unsupported"),
        Role::Unknown => found.unknown("role"),
    }
    for (step, name) in [
        (evidence.discovery, "discovery"),
        (evidence.recovery, "recovery"),
    ] {
        match step {
            Step::Complete => {}
            Step::Unknown => found.unknown(name),
            other => found.refuse(format!("{name}_{}", snake(&other))),
        }
    }
    for kind in Obligation::ALL.iter().copied() {
        match evidence.obligations.get(&kind) {
            Some(Load::Clear) => {}
            Some(Load::Busy) => found.refuse(format!("busy.{}", snake(&kind))),
            Some(Load::Unknown) | None => found.unknown(format!("obligation.{}", snake(&kind))),
        }
    }
    if evidence.obligations.contains_key(&Obligation::Unknown) {
        found.unknown("obligation.unrecognized");
    }
    match evidence.anchor {
        Anchor::Latest { .. } | Anchor::Empty => {}
        Anchor::Skipped => found.unknown("anchor_skipped"),
        Anchor::Unknown => found.unknown("anchor"),
    }
}

fn sources(sources: &[SourceEvidence], found: &mut Found) {
    if sources.len() > BUDGET_SOURCES {
        found.refuse("budget_sources");
    }
    let wrapper = |s: &SourceEvidence| s.wrapper.as_ref().and_then(|w| w.eof).unwrap_or(0);
    let bytes: u64 = (sources.iter())
        .map(|s| s.bytes.unwrap_or(0).saturating_add(wrapper(s)))
        .fold(0, u64::saturating_add);
    if bytes > BUDGET_BYTES {
        found.refuse("budget_bytes");
    }
    if sources.is_empty() {
        found.unknown("sources");
    } else if !sources.iter().any(|s| s.role == SourceRole::Current) {
        found.refuse("no_current_source");
    }
    for source in sources {
        match source.role {
            SourceRole::Current => {
                bound_source("current", source, found);
                current(source, found);
            }
            SourceRole::Retired => {
                bound_source("retired", source, found);
                match source.retirement {
                    Retirement::Replaced | Retirement::Exited => {}
                    Retirement::Live => found.refuse("retired.live"),
                    Retirement::Missing => found.refuse("retired.unproven"),
                    Retirement::Unknown => found.unknown("retired.retirement"),
                }
            }
            SourceRole::Named => match source.bytes {
                Some(0) => {}
                Some(_) => found.refuse("named.nonempty"),
                None => found.unknown("named.unreadable"),
            },
            SourceRole::Unknown => found.unknown("source_role"),
        }
    }
}

fn bound_source(role: &str, source: &SourceEvidence, found: &mut Found) {
    if source.bytes.is_none() {
        found.unknown(format!("{role}.unreadable"));
    }
    match source.proof {
        Proof::Linked => {}
        Proof::Unknown => found.unknown(format!("{role}.proof")),
        other => found.refuse(format!("{role}.proof_{}", snake(&other))),
    }
    match source.prefix {
        Parse::Strict => {}
        Parse::Malformed => found.refuse(format!("{role}.prefix_malformed")),
        Parse::Unknown => found.unknown(format!("{role}.prefix")),
    }
    if let Some(wrapper) = &source.wrapper {
        wrapped(wrapper, found);
    }
}

fn current(source: &SourceEvidence, found: &mut Found) {
    match (source.native_cursor, source.bytes) {
        (None, _) => found.unknown("current.cursor"),
        (Some(cursor), Some(eof)) if cursor > eof => found.refuse("current.cursor_past_eof"),
        _ => {}
    }
    suffix("current", source.suffix, found);
    match source.closed {
        Closed::Own => {}
        Closed::Open => found.refuse("current.turn_open"),
        Closed::Unknown => found.unknown("current.closed"),
    }
}

/// A wrapper this evaluator cannot tie to its source is unsupported, never merely unknown.
fn wrapped(wrapper: &WrapperEvidence, found: &mut Found) {
    if wrapper.proof != Proof::Linked {
        found.refuse("wrapper.unsupported");
    }
    match (wrapper.floor, wrapper.eof) {
        (Some(floor), Some(eof)) if floor > eof => found.refuse("wrapper.floor_past_eof"),
        (Some(_), Some(_)) => {}
        _ => found.unknown("wrapper.floor"),
    }
    if let (Some(cursor), Some(eof)) = (wrapper.cursor, wrapper.eof)
        && cursor > eof
    {
        found.refuse("wrapper.cursor_past_eof");
    }
    suffix("wrapper", wrapper.suffix, found);
    match wrapper.backlog {
        Load::Clear => {}
        Load::Busy => found.refuse("wrapper.backlog"),
        Load::Unknown => found.unknown("wrapper.backlog"),
    }
}

fn suffix(role: &str, suffix: Suffix, found: &mut Found) {
    match suffix {
        Suffix::Quiet => {}
        Suffix::Unknown => found.unknown(format!("{role}.suffix")),
        other => found.refuse(format!("{role}.suffix_{}", snake(&other))),
    }
}
