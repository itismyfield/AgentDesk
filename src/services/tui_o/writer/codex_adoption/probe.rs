//! Reads an explicitly given, anonymized snapshot and reports each channel's strict and boundary verdicts
//! with their reasons, and the N/K/U counts. It finds no input on its own and prints no body text.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Debug, Write};

use serde::Deserialize;

use super::judge::{Judgment, Verdict, judge, snake};
use super::{Anchor, Evidence, Load, Obligation, SourceEvidence, SourceRole, WrapperEvidence};

pub const SCHEMA: u32 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema: u32,
    sample_id: String,
    boot_epoch: String,
    channels: Vec<Evidence>,
}

/// In-scope channels `n`; `k` passed every boundary check and its anchor; `u` holds no refusal but
/// something unconfirmed. The adoptable range is `k..=k + u`; `u` is never counted as adoptable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub n: usize,
    pub k: usize,
    pub u: usize,
    pub refused: usize,
    pub strict_k: usize,
    pub strict_u: usize,
    pub excluded: usize,
}

#[derive(Debug)]
pub struct Report {
    pub lines: Vec<String>,
    pub counts: Counts,
    /// How many in-scope channels each code refused or left unknown.
    pub reasons: BTreeMap<String, usize>,
}

/// Judges every channel of `snapshot`; a malformed snapshot, a foreign schema or a channel listed
/// twice fails the whole probe instead of shrinking its denominator.
pub fn probe(snapshot: &str) -> Result<Report, String> {
    let snapshot: Snapshot =
        serde_json::from_str(snapshot).map_err(|error| format!("snapshot: {error}"))?;
    if snapshot.schema != SCHEMA {
        return Err(format!(
            "snapshot schema {} is not {SCHEMA}",
            snapshot.schema
        ));
    }
    let mut seen = BTreeSet::new();
    if let Some(twice) =
        (snapshot.channels.iter()).find(|e| e.channel == 0 || !seen.insert(e.channel))
    {
        return Err(format!(
            "channel {} is missing or listed twice",
            twice.channel
        ));
    }
    let head = format!(
        "schema={SCHEMA} boot_epoch={:?} sample_id={:?}",
        snapshot.boot_epoch, snapshot.sample_id
    );
    let (mut counts, mut reasons, mut lines) = (Counts::default(), BTreeMap::new(), Vec::new());
    for evidence in &snapshot.channels {
        let judgment = judge(evidence);
        if judgment.boundary == Verdict::OutOfScope {
            counts.excluded += 1;
        } else {
            counts.n += 1;
            let k = (&mut counts.k, &mut counts.u);
            let strict = (&mut counts.strict_k, &mut counts.strict_u);
            for (verdict, (k, u)) in [(judgment.boundary, k), (judgment.strict, strict)] {
                match verdict {
                    Verdict::Eligible => *k += 1,
                    Verdict::Unknown => *u += 1,
                    _ => {}
                }
            }
            counts.refused += usize::from(judgment.boundary == Verdict::Refused);
            for code in judgment.refused.iter().chain(&judgment.unknown) {
                *reasons.entry(code.clone()).or_insert(0) += 1;
            }
        }
        lines.push(line(&head, evidence, &judgment));
    }
    let Counts { n, k, u, .. } = counts;
    let codes: Vec<String> = reasons.iter().map(|(c, n)| format!("{c}:{n}")).collect();
    lines.push(format!(
        "event=codex_adoption_probe_summary {head} n={n} k={k} u={u} refused={} strict_k={} \
         strict_u={} excluded={} range={k}..{} reasons=[{}]",
        counts.refused,
        counts.strict_k,
        counts.strict_u,
        counts.excluded,
        k + u,
        codes.join(",")
    ));
    Ok(Report {
        lines,
        counts,
        reasons,
    })
}

impl Report {
    pub fn render(&self) -> String {
        self.lines.iter().map(|line| format!("{line}\n")).collect()
    }
}

fn line(head: &str, evidence: &Evidence, judgment: &Judgment) -> String {
    let of = |role| (evidence.sources.iter()).filter(move |s: &&SourceEvidence| s.role == role);
    let current: Vec<&SourceEvidence> = of(SourceRole::Current).collect();
    let each = |field: fn(&SourceEvidence) -> String| -> String {
        let values: Vec<String> = current.iter().map(|s| field(s)).collect();
        if values.is_empty() {
            "none".into()
        } else {
            values.join(",")
        }
    };
    let obligation = |kind| snake(evidence.obligations.get(&kind).unwrap_or(&Load::Unknown));
    let total: u64 = (evidence.sources.iter())
        .map(|s| s.bytes.unwrap_or(0) + s.wrapper.as_ref().and_then(|w| w.eof).unwrap_or(0))
        .sum();
    let retired: Vec<String> = of(SourceRole::Retired)
        .map(|s| snake(&s.retirement))
        .collect();
    let anchor = match evidence.anchor {
        Anchor::Latest { .. } => "latest",
        Anchor::Empty => "empty",
        Anchor::Skipped => "skipped",
        Anchor::Unknown => "unknown",
    };
    let reasons: Vec<&str> = (judgment.refused.iter())
        .chain(&judgment.unknown)
        .map(String::as_str)
        .collect();
    let mut out = format!(
        "event=codex_adoption_probe {head} channel={}",
        evidence.channel
    );
    let fields: [(&str, String); 29] = [
        ("provider", snake(&evidence.provider)),
        ("runtime_kind", snake(&evidence.runtime_kind)),
        ("role", snake(&evidence.role)),
        ("store_state", snake(&evidence.store)),
        ("candidate_state", snake(&evidence.candidate)),
        ("current_sources", current.len().to_string()),
        ("retired_sources", retired.len().to_string()),
        ("named_sources", of(SourceRole::Named).count().to_string()),
        ("total_bytes", total.to_string()),
        ("source_proof", each(|s| snake(&s.proof))),
        ("native_cursor", each(|s| number(s.native_cursor))),
        ("native_eof", each(|s| number(s.bytes))),
        (
            "relay_namespace",
            each(|s| ["native", "wrapper"][usize::from(s.wrapper.is_some())].into()),
        ),
        ("relay_cursor", each(|s| relay(s, |w| w.cursor))),
        ("relay_eof", each(|s| relay(s, |w| w.eof))),
        ("committed_floor", each(|s| relay(s, |w| w.floor))),
        ("strict_parse", each(|s| snake(&s.prefix))),
        ("closed", each(|s| snake(&s.closed))),
        ("retirement_proof", list(&retired)),
        ("discovery_status", snake(&evidence.discovery)),
        ("recovery_status", snake(&evidence.recovery)),
        ("custody", obligation(Obligation::Custody)),
        ("mailbox_status", obligation(Obligation::Mailbox)),
        ("active_readers", obligation(Obligation::NativeTail)),
        ("pending_receivers", obligation(Obligation::Receiver)),
        ("emission_epoch", number(evidence.emission_epoch)),
        ("anchor_status", anchor.into()),
        ("strict_eligible", snake(&judgment.strict)),
        ("r2_eligible", snake(&judgment.boundary)),
    ];
    for (key, value) in fields {
        let _ = write!(out, " {key}={value}");
    }
    let _ = write!(out, " reasons=[{}]", reasons.join(","));
    out
}

fn relay(source: &SourceEvidence, field: fn(&WrapperEvidence) -> Option<u64>) -> String {
    (source.wrapper.as_ref()).map_or("none".into(), |wrapper| number(field(wrapper)))
}

fn number(value: Option<u64>) -> String {
    value.map_or("unknown".into(), |value| value.to_string())
}

fn list(values: &[impl Debug + AsRef<str>]) -> String {
    if values.is_empty() {
        "none".into()
    } else {
        (values.iter().map(AsRef::as_ref).collect::<Vec<_>>()).join(",")
    }
}
