//! The shadow's structured records and counters. A start record pins who observed; each close
//! record states how many sequences were submitted, completed, lost and unknown by then.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::EventKind;

/// Shared by the gateway hook and the worker; nothing here names a channel, source or node.
#[derive(Debug, Default)]
pub(super) struct Counts {
    /// Sequences handed out; every allowlisted ingress event takes one, dropped ones included.
    pub(super) submitted: AtomicU64,
    pub(super) dropped_full: AtomicU64,
    pub(super) dropped_closed: AtomicU64,
    /// Events that carry no channel ingress, such as edits, deletes, pings and presence.
    pub(super) excluded: AtomicU64,
    pub(super) completed: AtomicU64,
    /// Completed while the authority, a clock, the local identity or the local gate was unknown.
    pub(super) unknown: AtomicU64,
}

impl Counts {
    pub(super) fn close(&self, incarnation: &str, terminal: bool) -> Record {
        let read = |count: &AtomicU64| count.load(Ordering::Acquire);
        Record::Close {
            incarnation: incarnation.to_owned(),
            last_sequence: read(&self.submitted),
            completed: read(&self.completed),
            lost: read(&self.dropped_full) + read(&self.dropped_closed),
            unknown: read(&self.unknown),
            excluded: read(&self.excluded),
            terminal,
            at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub(super) enum Record {
    Start {
        incarnation: String,
        instance_id: String,
        build: String,
        bot: String,
        provider: String,
        generation: u64,
        allowlist: Vec<u64>,
        at: DateTime<Utc>,
    },
    Observed(Box<Observed>),
    Close {
        incarnation: String,
        last_sequence: u64,
        completed: u64,
        lost: u64,
        unknown: u64,
        excluded: u64,
        /// Written once the queue closed; a periodic close is `false`.
        terminal: bool,
        at: DateTime<Utc>,
    },
}

/// One judged event: who judged it, its ids, how long each step waited, what was read and the
/// verdict. Never content, attachments, tokens or database errors.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Observed {
    pub(super) incarnation: String,
    pub(super) instance_id: String,
    pub(super) generation: u64,
    pub(super) bot: String,
    pub(super) provider: String,
    pub(super) sequence: u64,
    pub(super) kind: EventKind,
    pub(super) channel: u64,
    pub(super) source_id: u64,
    pub(super) application_id: Option<u64>,
    pub(super) queued_ms: u64,
    pub(super) lookup_ms: u64,
    pub(super) sample_ms: u64,
    pub(super) lookup: String,
    pub(super) sampler: String,
    pub(super) row_epoch: Option<i64>,
    pub(super) local_epoch: Option<i64>,
    pub(super) verdict: String,
    pub(super) reason: Option<String>,
    pub(super) holder: Option<String>,
}

/// Where records go: the structured log, or a test's own list.
#[derive(Clone)]
pub(super) enum Sink {
    Log,
    #[cfg(test)]
    Capture(std::sync::Arc<std::sync::Mutex<Vec<Record>>>),
}

impl Sink {
    pub(super) fn emit(&self, record: Record) {
        match self {
            Self::Log => match serde_json::to_string(&record) {
                Ok(json) => {
                    tracing::info!(target: "agentdesk::ingress_shadow", record = %json, "ingress shadow");
                }
                Err(error) => {
                    tracing::warn!(target: "agentdesk::ingress_shadow", %error, "ingress shadow record not encoded");
                }
            },
            #[cfg(test)]
            Self::Capture(records) => records.lock().unwrap().push(record),
        }
    }
}

/// Judges an observation window from its records; no runtime reader exists, so tests only.
#[cfg(test)]
pub(super) mod window {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{Observed, Record};

    /// One observer expected in the window, as its start record must name it.
    #[derive(Clone, Debug)]
    pub(in crate::services::discord) struct ExpectedNode {
        pub(in crate::services::discord) instance_id: String,
        pub(in crate::services::discord) incarnation: String,
        pub(in crate::services::discord) build: String,
    }

    #[derive(Clone, Debug)]
    pub(in crate::services::discord) struct Expected {
        pub(in crate::services::discord) nodes: Vec<ExpectedNode>,
        pub(in crate::services::discord) bot: String,
        pub(in crate::services::discord) provider: String,
        pub(in crate::services::discord) allowlist: BTreeSet<u64>,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::services::discord) enum Gap {
        UnexpectedIncarnation(String),
        MissingStart(String),
        StartMismatch(String),
        MissingClose(String),
        /// The last close shows a sequence lost, unknown or not yet completed.
        Unfinished(String),
        MissingSequence(String, u64),
        DuplicateSequence(String, u64),
        /// Judged after the last close, so no close vouches for it.
        AfterClose(String, u64),
        Unpaired(u64, u64),
        EpochSplit(u64, u64),
        NoPairs,
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(in crate::services::discord) enum Window {
        Pass {
            paired: usize,
        },
        Incomplete(Vec<Gap>),
        /// Sources more than one observer process judged its own candidate.
        Conflict(Vec<(u64, u64)>),
    }

    #[derive(Default)]
    struct Seen<'a> {
        starts: Vec<&'a Record>,
        close: Option<(u64, bool)>,
        observed: Vec<&'a Observed>,
    }

    fn incarnation(record: &Record) -> &str {
        match record {
            Record::Start { incarnation, .. } | Record::Close { incarnation, .. } => incarnation,
            Record::Observed(observed) => &observed.incarnation,
        }
    }

    /// PASS only when every expected start matches, each last close is finished with its sequences
    /// present once and none after it, and every observer judged every source at one row epoch.
    pub(in crate::services::discord) fn evaluate_window(
        expected: &Expected,
        records: &[Record],
    ) -> Window {
        let mut gaps = Vec::new();
        let mut seen: BTreeMap<&str, Seen<'_>> = BTreeMap::new();
        for record in records {
            let entry = seen.entry(incarnation(record)).or_default();
            match record {
                Record::Start { .. } => entry.starts.push(record),
                Record::Close {
                    last_sequence,
                    completed,
                    lost,
                    unknown,
                    ..
                } => {
                    let finished = *lost == 0 && *unknown == 0 && completed == last_sequence;
                    if entry.close.is_none_or(|(last, _)| *last_sequence >= last) {
                        entry.close = Some((*last_sequence, finished));
                    }
                }
                Record::Observed(observed) => entry.observed.push(observed),
            }
        }
        let wanted: BTreeSet<&str> = expected
            .nodes
            .iter()
            .map(|n| n.incarnation.as_str())
            .collect();
        for incarnation in seen.keys().filter(|seen| !wanted.contains(*seen)) {
            gaps.push(Gap::UnexpectedIncarnation((*incarnation).to_owned()));
        }
        for node in &expected.nodes {
            let name = node.incarnation.clone();
            let Some(entry) = seen.get(node.incarnation.as_str()) else {
                gaps.extend([Gap::MissingStart(name.clone()), Gap::MissingClose(name)]);
                continue;
            };
            match entry.starts.as_slice() {
                [
                    Record::Start {
                        instance_id,
                        build,
                        bot,
                        provider,
                        allowlist,
                        ..
                    },
                ] => {
                    let listed: BTreeSet<u64> = allowlist.iter().copied().collect();
                    if *instance_id != node.instance_id
                        || *build != node.build
                        || *bot != expected.bot
                        || *provider != expected.provider
                        || listed != expected.allowlist
                    {
                        gaps.push(Gap::StartMismatch(name.clone()));
                    }
                }
                _ => gaps.push(Gap::MissingStart(name.clone())),
            }
            let last = match entry.close {
                Some((last, finished)) => {
                    if !finished {
                        gaps.push(Gap::Unfinished(name.clone()));
                    }
                    last
                }
                None => {
                    gaps.push(Gap::MissingClose(name.clone()));
                    0
                }
            };
            let mut sequences = BTreeSet::new();
            for observed in &entry.observed {
                if observed.sequence > last {
                    gaps.push(Gap::AfterClose(name.clone(), observed.sequence));
                } else if !sequences.insert(observed.sequence) {
                    gaps.push(Gap::DuplicateSequence(name.clone(), observed.sequence));
                }
            }
            let missing = (1..=last).filter(|sequence| !sequences.contains(sequence));
            gaps.extend(missing.map(|sequence| Gap::MissingSequence(name.clone(), sequence)));
        }
        // Every judged record joins its source, from expected observers or not.
        let mut sources: BTreeMap<(u64, u64), Vec<&Observed>> = BTreeMap::new();
        for observed in seen.values().flat_map(|entry| &entry.observed) {
            let source = (observed.channel, observed.source_id);
            sources.entry(source).or_default().push(observed);
        }
        let (mut conflicts, mut paired) = (Vec::new(), 0);
        for (&(channel, source), observed) in &sources {
            let candidates: BTreeSet<&str> = observed
                .iter()
                .filter(|observed| observed.verdict == "local_candidate")
                .map(|observed| observed.incarnation.as_str())
                .collect();
            if candidates.len() > 1 {
                conflicts.push((channel, source));
            }
            let judged: BTreeSet<&str> = observed.iter().map(|o| o.incarnation.as_str()).collect();
            let epochs: BTreeSet<Option<i64>> = observed.iter().map(|o| o.row_epoch).collect();
            if judged != wanted {
                gaps.push(Gap::Unpaired(channel, source));
            } else if epochs.len() > 1 {
                gaps.push(Gap::EpochSplit(channel, source));
            } else {
                paired += 1;
            }
        }
        if !conflicts.is_empty() {
            return Window::Conflict(conflicts);
        }
        if paired == 0 {
            gaps.push(Gap::NoPairs);
        }
        if gaps.is_empty() {
            Window::Pass { paired }
        } else {
            Window::Incomplete(gaps)
        }
    }

    use chrono::{DateTime, TimeZone, Utc};

    use super::super::EventKind;

    const CH: u64 = 5_713_000_000_000_201;

    fn renewed() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap()
    }

    fn start(incarnation: &str, instance_id: &str, allowlist: &[u64]) -> Record {
        Record::Start {
            incarnation: incarnation.into(),
            instance_id: instance_id.into(),
            build: "build-1".into(),
            bot: "bot-1".into(),
            provider: "claude".into(),
            generation: 1,
            allowlist: allowlist.to_vec(),
            at: renewed(),
        }
    }

    fn judged(incarnation: &str, sequence: u64, source: u64, verdict: &str, epoch: i64) -> Record {
        Record::Observed(Box::new(Observed {
            incarnation: incarnation.into(),
            instance_id: incarnation.trim_end_matches(char::is_numeric).into(),
            generation: 1,
            bot: "bot-1".into(),
            provider: "claude".into(),
            sequence,
            kind: EventKind::Message,
            channel: CH,
            source_id: source,
            application_id: None,
            queued_ms: 0,
            lookup_ms: 0,
            sample_ms: 0,
            lookup: "row".into(),
            sampler: "sampled".into(),
            row_epoch: Some(epoch),
            local_epoch: None,
            verdict: verdict.into(),
            reason: None,
            holder: None,
        }))
    }

    fn close(incarnation: &str, last: u64, completed: u64, lost: u64, unknown: u64) -> Record {
        Record::Close {
            incarnation: incarnation.into(),
            last_sequence: last,
            completed,
            lost,
            unknown,
            excluded: 0,
            terminal: true,
            at: renewed(),
        }
    }

    fn expected(nodes: &[(&str, &str)]) -> Expected {
        let node = |(instance_id, incarnation): &(&str, &str)| ExpectedNode {
            instance_id: (*instance_id).into(),
            incarnation: (*incarnation).into(),
            build: "build-1".into(),
        };
        Expected {
            nodes: nodes.iter().map(node).collect(),
            bot: "bot-1".into(),
            provider: "claude".into(),
            allowlist: BTreeSet::from([CH]),
        }
    }

    /// Node a holds the channel; node b sees the same three sources as foreign.
    fn paired_window() -> Vec<Record> {
        let mut records = vec![start("a1", "a", &[CH]), start("b1", "b", &[CH])];
        for sequence in 1..=3 {
            records.push(judged("a1", sequence, 80 + sequence, "local_candidate", 4));
            records.push(judged("b1", sequence, 80 + sequence, "ignore_foreign", 4));
        }
        records.extend([close("a1", 3, 3, 0, 0), close("b1", 3, 3, 0, 0)]);
        records
    }

    fn without(records: &[Record], drop: impl Fn(&Record) -> bool) -> Vec<Record> {
        records
            .iter()
            .filter(|record| !drop(record))
            .cloned()
            .collect()
    }

    fn is_judged(record: &Record, incarnation: &str, sequence: u64) -> bool {
        matches!(record, Record::Observed(o) if o.incarnation == incarnation && o.sequence == sequence)
    }

    #[test]
    fn a_window_passes_only_when_every_observer_and_source_joined_to_a_finished_close() {
        let nodes = expected(&[("a", "a1"), ("b", "b1")]);
        let window = paired_window();
        assert_eq!(evaluate_window(&nodes, &window), Window::Pass { paired: 3 });
        let incomplete = |records: Vec<Record>, gaps: Vec<Gap>| {
            assert_eq!(evaluate_window(&nodes, &records), Window::Incomplete(gaps));
        };
        let a1 = || "a1".to_owned();
        incomplete(
            without(&window, |r| is_judged(r, "a1", 2)),
            vec![Gap::MissingSequence(a1(), 2), Gap::Unpaired(CH, 82)],
        );
        incomplete(
            without(&window, |r| is_judged(r, "a1", 3) || is_judged(r, "b1", 3)),
            vec![
                Gap::MissingSequence(a1(), 3),
                Gap::MissingSequence("b1".into(), 3),
            ],
        );
        incomplete(
            without(
                &window,
                |r| matches!(r, Record::Close { incarnation, .. } if incarnation == "a1"),
            ),
            vec![
                Gap::MissingClose(a1()),
                Gap::AfterClose(a1(), 1),
                Gap::AfterClose(a1(), 2),
                Gap::AfterClose(a1(), 3),
            ],
        );
        let only_a = without(&window, |r| match r {
            Record::Start { incarnation, .. } | Record::Close { incarnation, .. } => {
                incarnation == "b1"
            }
            Record::Observed(o) => o.incarnation == "b1",
        });
        incomplete(
            only_a,
            vec![
                Gap::MissingStart("b1".into()),
                Gap::MissingClose("b1".into()),
                Gap::Unpaired(CH, 81),
                Gap::Unpaired(CH, 82),
                Gap::Unpaired(CH, 83),
                Gap::NoPairs,
            ],
        );
        let mut wider = window.clone();
        wider[1] = start("b1", "b", &[CH, CH + 1]);
        incomplete(wider, vec![Gap::StartMismatch("b1".into())]);
        let mut tail = window.clone();
        tail.push(judged("a1", 4, 84, "local_candidate", 4));
        incomplete(tail, vec![Gap::AfterClose(a1(), 4), Gap::Unpaired(CH, 84)]);
        let mut stray = window.clone();
        stray.push(judged("a2", 1, 81, "ignore_foreign", 4));
        incomplete(
            stray,
            vec![
                Gap::UnexpectedIncarnation("a2".into()),
                Gap::Unpaired(CH, 81),
            ],
        );
    }

    #[test]
    fn a_lost_unknown_or_unfinished_sequence_or_no_pair_never_certifies() {
        let nodes = expected(&[("a", "a1"), ("b", "b1")]);
        let window = paired_window();
        let with_close = |last, completed, lost, unknown| {
            let mut records = without(
                &window,
                |r| matches!(r, Record::Close { incarnation, .. } if incarnation == "a1"),
            );
            records.push(close("a1", last, completed, lost, unknown));
            evaluate_window(&nodes, &records)
        };
        let a1 = || "a1".to_owned();
        assert_eq!(
            with_close(4, 3, 1, 0),
            Window::Incomplete(vec![Gap::Unfinished(a1()), Gap::MissingSequence(a1(), 4)])
        );
        assert_eq!(
            with_close(3, 3, 0, 1),
            Window::Incomplete(vec![Gap::Unfinished(a1())])
        );
        assert_eq!(
            with_close(3, 2, 0, 0),
            Window::Incomplete(vec![Gap::Unfinished(a1())])
        );
        let idle = vec![
            start("a1", "a", &[CH]),
            start("b1", "b", &[CH]),
            close("a1", 0, 0, 0, 0),
            close("b1", 0, 0, 0, 0),
        ];
        assert_eq!(
            evaluate_window(&nodes, &idle),
            Window::Incomplete(vec![Gap::NoPairs])
        );
    }

    #[test]
    fn two_processes_judging_one_source_their_own_is_a_conflict_across_ids_and_epochs() {
        // Two processes run as instance `a`: same id, different incarnations.
        let twins = expected(&[("a", "a1"), ("a", "a2")]);
        let mut records = vec![start("a1", "a", &[CH]), start("a2", "a", &[CH])];
        records.push(judged("a1", 1, 91, "local_candidate", 4));
        records.push(judged("a2", 1, 91, "local_candidate", 4));
        records.extend([close("a1", 1, 1, 0, 0), close("a2", 1, 1, 0, 0)]);
        assert_eq!(
            evaluate_window(&twins, &records),
            Window::Conflict(vec![(CH, 91)])
        );

        let nodes = expected(&[("a", "a1"), ("b", "b1")]);
        let mut moved = paired_window();
        moved.retain(|r| !is_judged(r, "b1", 2));
        moved.push(judged("b1", 2, 82, "local_candidate", 5));
        assert_eq!(
            evaluate_window(&nodes, &moved),
            Window::Conflict(vec![(CH, 82)])
        );
        let mut handed = paired_window();
        handed.retain(|r| !is_judged(r, "b1", 2));
        handed.push(judged("b1", 2, 82, "ignore_foreign", 5));
        assert_eq!(
            evaluate_window(&nodes, &handed),
            Window::Incomplete(vec![Gap::EpochSplit(CH, 82)])
        );
    }
}
