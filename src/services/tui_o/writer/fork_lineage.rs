//! Native row identities a forked source shares with the bytes its parent's cursor has consumed.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::{LINEAGE_SCAN_CAP_BYTES, Seen, record_keys};
use crate::services::tui_o::shadow::capture::{FileMatch, file_identity, file_match};
use crate::services::tui_o::shadow::identity::row_key;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, UnitKind};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Id {
    Row(String),
    Unit(String, UnitKind, Seen),
}

/// A row's uuid and the unit keys it seals or announces; `units` is `None` for a blocked row.
pub(super) struct RowIds {
    row: Option<String>,
    units: Option<Vec<(String, UnitKind, Seen)>>,
}

impl RowIds {
    /// `None` when the line is not a JSON record.
    pub(super) fn of(provider: ShadowProvider, line: &[u8]) -> Option<Self> {
        let units = record_keys(provider, line);
        if units.as_ref().is_some_and(Vec::is_empty) && line.iter().all(u8::is_ascii_whitespace) {
            return Some(Self { row: None, units });
        }
        let value: serde_json::Value = serde_json::from_slice(line).ok()?;
        let row = row_key(&value);
        Some(Self { row, units })
    }

    /// Carries no identity at all, so it neither extends nor ends an inherited prefix.
    pub(super) fn is_empty(&self) -> bool {
        self.row.is_none() && self.units.as_ref().is_some_and(Vec::is_empty)
    }

    fn all(&self) -> impl Iterator<Item = Id> + '_ {
        let row = self.row.iter().map(|uuid| Id::Row(uuid.clone()));
        let units = self.units.iter().flatten();
        row.chain(units.map(|(key, kind, seen)| Id::Unit(key.clone(), *kind, *seen)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// Every identity is in the parent's consumed set.
    Inherited,
    /// None is.
    New,
    /// Some are, or a blocked row whose uuid the parent never consumed.
    Unidentified,
}

/// Identities of the complete lines in the parent's bytes `0..through`.
pub(super) struct Lineage {
    through: u64,
    ids: HashMap<Id, u64>,
    native: HashMap<Id, serde_json::Value>,
    /// Bytes after the last newline before `through`, joined to the next read.
    tail: Vec<u8>,
}

impl Lineage {
    /// Automatic boundaries require all identities and native content, including excluded bodies.
    pub(super) fn native_class(&self, provider: ShadowProvider, line: &[u8], from: u64) -> Class {
        let Some(row) = RowIds::of(provider, line).filter(|r| !r.is_empty() && r.units.is_some())
        else {
            return Class::Unidentified;
        };
        match self.class_from(&row, from) {
            Class::Inherited => {
                let native = native_content(line);
                if row.all().all(|id| self.native.get(&id) == native.as_ref()) {
                    Class::Inherited
                } else {
                    Class::Unidentified
                }
            }
            Class::New if self.class(&row) == Class::New => Class::New,
            _ => Class::Unidentified,
        }
    }

    pub(super) fn class(&self, row: &RowIds) -> Class {
        self.class_from(row, 0)
    }

    pub(super) fn class_from(&self, row: &RowIds, from: u64) -> Class {
        let in_range = |id: &Id| self.ids.get(id).is_some_and(|start| *start >= from);
        let held = |id: &Id| match id {
            Id::Unit(key, kind, Seen::Announced) => {
                in_range(id) || in_range(&Id::Unit(key.clone(), *kind, Seen::Sealed))
            }
            id => in_range(id),
        };
        if row.units.is_none() {
            let inherited = row
                .row
                .as_ref()
                .is_some_and(|uuid| held(&Id::Row(uuid.clone())));
            return if inherited {
                Class::Inherited
            } else {
                Class::Unidentified
            };
        }
        let (count, kept) = row
            .all()
            .fold((0, 0), |(n, k), id| (n + 1, k + usize::from(held(&id))));
        match kept {
            0 => Class::New,
            kept if kept == count => Class::Inherited,
            _ => Class::Unidentified,
        }
    }

    fn add(&mut self, provider: ShadowProvider, bytes: &[u8]) {
        let mut start = self.through - self.tail.len() as u64;
        self.tail.extend_from_slice(bytes);
        let Some(last) = self.tail.iter().rposition(|byte| *byte == b'\n') else {
            return;
        };
        let rest = self.tail.split_off(last + 1);
        for line in self.tail.split_inclusive(|byte| *byte == b'\n') {
            if let Some(row) = RowIds::of(provider, line) {
                for id in row.all() {
                    if let Some(native) = native_content(line) {
                        self.native.insert(id.clone(), native);
                    }
                    self.ids
                        .entry(id)
                        .and_modify(|offset| *offset = (*offset).max(start))
                        .or_insert(start);
                }
            }
            start += line.len() as u64;
        }
        self.tail = rest;
    }
}

/// Brings `cached` to the parent's consumed bytes `0..through`, checking the parent path's
/// identity and length on every call, so a cached set is never used for a file that is gone.
pub(super) fn sync(
    cached: Option<Lineage>,
    provider: ShadowProvider,
    parent: &SourceId,
    through: u64,
) -> Option<Lineage> {
    let readable = |file: &File| {
        let meta = file.metadata().ok()?;
        // A renumbered parent was proven by its own reopen.
        let ok = file_match(parent, file_identity(&meta)) != FileMatch::Other
            && meta.len() >= through
            && through <= LINEAGE_SCAN_CAP_BYTES;
        ok.then_some(())
    };
    let mut file = File::open(&parent.path).ok()?;
    readable(&file)?;
    let mut lineage = match cached {
        Some(lineage) if lineage.through <= through => lineage,
        _ => Lineage {
            through: 0,
            ids: HashMap::new(),
            native: HashMap::new(),
            tail: Vec::new(),
        },
    };
    let want = through - lineage.through;
    if want > 0 {
        file.seek(SeekFrom::Start(lineage.through)).ok()?;
        let mut bytes = Vec::new();
        file.take(want).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 != want {
            return None;
        }
        lineage.add(provider, &bytes);
        lineage.through = through;
    }
    Some(lineage)
}

/// Compare interpretation and raw native content; transport/session metadata is not body identity.
fn native_content(line: &[u8]) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(line).ok()?;
    let message = &value["message"];
    let native = serde_json::json!([
        value["type"],
        value["subtype"],
        value["isMeta"],
        value["isApiErrorMessage"],
        value["apiBlockIndex"],
        message["role"],
        message["content"],
        message["model"].as_str() == Some("<synthetic>")
    ]);
    Some(native)
}

#[cfg(all(test, unix))]
#[path = "fork_lineage_tests.rs"]
mod tests;
