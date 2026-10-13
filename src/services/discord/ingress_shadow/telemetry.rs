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
        Pass { paired: usize },
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
        let wanted: BTreeSet<&str> = expected.nodes.iter().map(|n| n.incarnation.as_str()).collect();
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
                [Record::Start {
                    instance_id,
                    build,
                    bot,
                    provider,
                    allowlist,
                    ..
                }] => {
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
}
