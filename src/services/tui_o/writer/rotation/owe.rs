//! Which spooled records are owed. Live capture and spool resume share this one decision; only
//! the collector that receives the derived work differs.

use std::collections::VecDeque;

use super::fork_lineage::{Class, RowIds};
use super::{BindingEvents, Boundary, Derived, Sources, UnitDeriver};
use crate::services::tui_o::shadow::{CapturedRecord, SourceId};
use crate::services::tui_o::store::rotation::SourceLink;
use crate::services::tui_o::store::spool::source_key;

/// Receives every owed record with its stored source, even when derivation yields nothing.
pub trait Owe {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>);
}

impl Owe for VecDeque<Derived> {
    fn owe(&mut self, _: &SourceId, _: &CapturedRecord, derived: Vec<Derived>) {
        self.extend(derived);
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
    ) {
        let key = source_key(source);
        let Some(link) = self.rotation.links.get(&key).cloned() else {
            owed.owe(source, record, deriver.derive(record));
            return;
        };
        match link.boundary {
            Boundary::Pending { .. } => {}
            Boundary::Owed { from } => {
                if record.start >= from && !self.masked(&link, record) {
                    owed.owe(source, record, deriver.derive(record));
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
                        owed.owe(source, record, deriver.derive(record));
                        self.decide(&key, Boundary::Owed { from: record.start });
                    }
                    _ => self.pend(&key, record.start),
                }
            }
        }
    }

    /// Masks only identities consumed within the parent's publishing responsibility.
    /// Root/adopted parents retain their existing from-zero policy.
    fn masked(&self, link: &SourceLink, record: &CapturedRecord) -> bool {
        let Some(parent) = link.parent.as_ref() else {
            return false;
        };
        let parent_link = self.rotation.link(parent).map(|l| &l.boundary);
        let from = match parent_link {
            None => 0,
            Some(Boundary::Owed { from }) => *from,
            _ => return false,
        };
        let Some(lineage) = self.lineage.get(&source_key(parent)) else {
            return false;
        };
        RowIds::of(self.provider, &record.line).is_some_and(|row| {
            !row.is_empty() && lineage.class_from(&row, from) == Class::Inherited
        })
    }
}
