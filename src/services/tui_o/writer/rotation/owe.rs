//! Which spooled records are owed. Live capture and spool resume share this one decision; only
//! the collector that receives the derived work differs.

use std::collections::VecDeque;

use super::fork_lineage::{Class, RowIds};
use super::{
    BindingEvents, BindingRecord, BindingTarget, Boundary, Derived, Sources, UnitDeriver,
    claude_boundary,
};
use crate::services::tui_o::shadow::{CapturedRecord, SourceId};
use crate::services::tui_o::store::rotation::SourceLink;
use crate::services::tui_o::store::spool::source_key;

/// Receives every owed record with its stored source, even when derivation yields nothing.
pub trait Owe {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>);
    fn finish_replay(
        &mut self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
    ) -> super::super::ReplaySummary;
}

impl Owe for VecDeque<Derived> {
    fn owe(&mut self, _: &SourceId, _: &CapturedRecord, derived: Vec<Derived>) {
        self.extend(derived);
    }

    fn finish_replay(
        &mut self,
        ledger: &crate::services::tui_o::store::ledger::LedgerState,
    ) -> super::super::ReplaySummary {
        super::super::replay::filter(ledger, |keep| self.retain(keep))
    }
}

impl<B: BindingEvents> Sources<B> {
    /// Derives a record unless the source's boundary withholds it or its parent already has it.
    pub(super) fn owe(
        &mut self,
        deriver: &mut UnitDeriver,
        owed: &mut impl Owe,
        source: &SourceId,
        record: &CapturedRecord,
        identity_version: u32,
    ) {
        let key = source_key(source);
        let Some(link) = self.rotation.links.get(&key).cloned() else {
            owed.owe(
                source,
                record,
                self.derive_record(deriver, source, record, identity_version),
            );
            return;
        };
        match link.boundary {
            Boundary::Pending { .. } => {}
            Boundary::Owed { from } => {
                if record.start >= from {
                    match self.masked(&link, record) {
                        Ok(false) => owed.owe(
                            source,
                            record,
                            self.derive_record(deriver, source, record, identity_version),
                        ),
                        Ok(true) => {}
                        Err(reason) => owed.owe(
                            source,
                            record,
                            vec![Derived::Blocked {
                                reason: reason.into(),
                            }],
                        ),
                    }
                }
            }
            Boundary::Undecided => {
                let row = RowIds::of(self.provider, &record.line);
                if row.as_ref().is_some_and(RowIds::is_empty) {
                    return;
                }
                let parent = link.parent.as_ref();
                let lineage = parent.and_then(|parent| self.lineage.get(&source_key(parent)));
                let class = row.zip(lineage).map(|(row, lineage)| lineage.class(&row));
                match class {
                    Some(Class::Inherited) => {
                        self.prefix_seen.insert(key);
                    }
                    Some(Class::New) if self.prefix_seen.contains(&key) => {
                        owed.owe(
                            source,
                            record,
                            self.derive_record(deriver, source, record, identity_version),
                        );
                        self.decide(&key, Boundary::Owed { from: record.start });
                    }
                    _ => self.pend(&key, record.start),
                }
            }
        }
    }

    fn derive_record(
        &self,
        deriver: &mut UnitDeriver,
        source: &SourceId,
        record: &CapturedRecord,
        version: u32,
    ) -> Vec<Derived> {
        if !deriver.input_notices_enabled() {
            return deriver.derive_with_version(record, version);
        }
        let origin = self.notice_origin(source, record);
        deriver.derive_scoped(record, version, origin)
    }

    fn notice_origin(
        &self,
        source: &SourceId,
        record: &CapturedRecord,
    ) -> Result<super::super::input_notices::NoticeOrigin, String> {
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        let matches_source = |event: &super::BindingEvent| match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(bound),
                ..
            } => bound == source,
            BindingRecord::Bound {
                new: BindingTarget::Pending { .. },
                ..
            } => events.iter().any(|resolved| {
                matches!(&resolved.record, BindingRecord::Resolved { resolves_seq, source: bound }
                    if *resolves_seq == event.seq && bound == source)
            }),
            _ => false,
        };
        // Root sources predate rotation links; their first exact binding is the attachment proof.
        let event = match self.rotation.link(source) {
            Some(link) => events.iter().find(|event| event.seq == link.seq),
            None => events.iter().find(|event| matches_source(event)),
        }
        .ok_or("input notice original binding is missing")?;
        if !matches_source(event)
            || event.channel_id != self.channel
            || event.provider != self.provider
        {
            return Err("input notice binding belongs to another owner".into());
        }
        Ok(super::super::input_notices::NoticeOrigin {
            record: crate::services::tui_o::shadow::SourceRange {
                source: source.clone(),
                start: record.start,
                end: record.end,
            },
            logical_session: event.tmux_session.clone(),
            binding_seq: event.seq,
            execution_nonce: event.execution_nonce.clone(),
        })
    }

    /// Masks only identities consumed within the parent's publishing responsibility.
    /// Root/adopted parents retain their existing from-zero policy.
    fn masked(&self, link: &SourceLink, record: &CapturedRecord) -> Result<bool, &'static str> {
        if link.hot_candidate.is_some() {
            let compare = || {
                let parent = link.parent.as_ref()?;
                let from = claude_boundary::parent_from(&self.rotation, parent)?;
                let lineage = self.lineage.get(&source_key(parent))?;
                let row = RowIds::of(self.provider, &record.line)?;
                if row.is_empty() {
                    return Some(false);
                }
                match lineage.native_class(self.provider, &record.line, from) {
                    Class::Inherited => Some(true),
                    Class::New => Some(false),
                    Class::Unidentified => None,
                }
            };
            return compare().ok_or("automatic parent content comparison failed");
        }
        let Some(parent) = link.parent.as_ref() else {
            return Ok(false);
        };
        let parent_link = self.rotation.link(parent).map(|l| &l.boundary);
        let from = match parent_link {
            None => 0,
            Some(Boundary::Owed { from }) => *from,
            _ => return Ok(false),
        };
        let Some(lineage) = self.lineage.get(&source_key(parent)) else {
            return Ok(false);
        };
        Ok(RowIds::of(self.provider, &record.line).is_some_and(|row| {
            !row.is_empty() && lineage.class_from(&row, from) == Class::Inherited
        }))
    }
}
