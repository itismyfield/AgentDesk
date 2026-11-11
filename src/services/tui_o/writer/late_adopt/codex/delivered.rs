//! Legacy's Codex delivery records as a floor per rollout. A record names no file, so it counts
//! only of the current generation, dated in that rollout's bind span or undated with it alone.

use chrono::{DateTime, Utc};

use super::prefix::Frontier;
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::writer::adoption::CodexDelivered;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord, BindingTarget};

/// Each bind of a source, from its bind (a resolved Pending from the Pending's own bind, since
/// Legacy may read the file from then) until the next bind.
struct Span {
    source: SourceId,
    from: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
}

/// The records a takeover attempt reads once, with the spans they are placed by.
pub(in crate::services::tui_o::writer::late_adopt) struct Records {
    delivered: CodexDelivered,
    spans: Vec<Span>,
}

impl Records {
    pub(in crate::services::tui_o::writer::late_adopt) fn new(
        delivered: CodexDelivered,
        events: &[BindingEvent],
    ) -> Self {
        let mut spans: Vec<Span> = Vec::new();
        let close = |spans: &mut Vec<Span>, at: DateTime<Utc>| {
            if let Some(open) = spans.last_mut().filter(|span| span.until.is_none()) {
                open.until = Some(at);
            }
        };
        for event in events {
            let (source, from) = match &event.record {
                BindingRecord::Bound { new, .. } => {
                    close(&mut spans, event.committed_at);
                    let BindingTarget::Source(source) = new else {
                        continue;
                    };
                    (source, event.committed_at)
                }
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } => {
                    close(&mut spans, event.committed_at);
                    let pending = events.iter().find(|e| e.seq == *resolves_seq);
                    (
                        source,
                        pending.map_or(event.committed_at, |e| e.committed_at),
                    )
                }
                BindingRecord::Rejected { .. } => continue,
            };
            let (source, until) = (source.clone(), None);
            spans.push(Span {
                source,
                from,
                until,
            });
        }
        Self { delivered, spans }
    }

    /// Every source bound in the current generation, oldest first; every source of the log when
    /// the generation cannot be read.
    pub(in crate::services::tui_o::writer::late_adopt) fn generation(&self) -> Vec<SourceId> {
        let since = self.delivered.generation_since;
        let alive = |span: &&Span| {
            span.until
                .is_none_or(|until| since.is_none_or(|s| until > s))
        };
        let mut sources: Vec<SourceId> = Vec::new();
        for span in self.spans.iter().filter(alive) {
            if !sources.contains(&span.source) {
                sources.push(span.source.clone());
            }
        }
        sources
    }

    fn owner(&self, at: Option<DateTime<Utc>>) -> Option<SourceId> {
        match at {
            Some(at) => (self.spans.iter().rev())
                .find(|span| span.from <= at && span.until.is_none_or(|until| at < until))
                .map(|span| span.source.clone()),
            None => match self.generation().as_slice() {
                [only] => Some(only.clone()),
                _ => None,
            },
        }
    }

    /// The end of the last turn Legacy's records show it delivered from `source`; 0 when none.
    pub(in crate::services::tui_o::writer::late_adopt) fn floor(
        &self,
        source: &SourceId,
        frontier: &Frontier,
    ) -> u64 {
        if self.delivered.generation_since.is_none() {
            return 0;
        }
        let records = self
            .delivered
            .frontier
            .iter()
            .chain(&self.delivered.receipts);
        records
            .filter(|record| self.owner(record.at).as_ref() == Some(source))
            .filter_map(|record| frontier.closes(record.range, record.at))
            .max()
            .unwrap_or(0)
    }
}
