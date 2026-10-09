//! A binding event names its source with the dev its stat read, and a reboot may renumber that
//! dev. O keeps the identity it stored first and maps every later variant of the file onto it.

use std::collections::HashMap;

use super::binding::{BindingEvent, BindingRecord, BindingTarget};
use super::deliver::ChannelWriter;
use super::{AlarmSink, DeliveryLease, DiscordPort, WriterAlarm};
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::{SourceCapture, same_file};
use crate::services::tui_o::store::spool::Cursor;

/// The stored source naming `source`'s file, or `source` when none does. Two stored variants of
/// one file are refused, not merged, even when one of them is `source` itself.
pub(super) fn canonical<'a>(
    cursors: impl Iterator<Item = &'a Cursor>,
    source: &SourceId,
) -> Result<SourceId, String> {
    let mut stored =
        (cursors.map(|cursor| &cursor.source)).filter(|stored| same_file(stored, source));
    match (stored.next(), stored.next()) {
        (None, _) => Ok(source.clone()),
        (Some(one), None) => Ok(one.clone()),
        (Some(_), Some(_)) => Err(ambiguous(source)),
    }
}

/// Refuses a store holding two cursors for one file.
pub(super) fn unique<'a>(cursors: impl Iterator<Item = &'a Cursor>) -> Result<(), String> {
    let mut seen: Vec<&SourceId> = Vec::new();
    for cursor in cursors {
        if seen.iter().any(|source| same_file(source, &cursor.source)) {
            return Err(ambiguous(&cursor.source));
        }
        seen.push(&cursor.source);
    }
    Ok(())
}

fn ambiguous(source: &SourceId) -> String {
    let path = source.path.display();
    format!("source {path} names more than one stored source")
}

/// Replaces every source `event` names with `map` of it.
pub(super) fn map_sources(event: &mut BindingEvent, mut map: impl FnMut(&SourceId) -> SourceId) {
    match &mut event.record {
        BindingRecord::Bound {
            old,
            new,
            parent_hint,
            ..
        } => {
            for source in old.iter_mut().chain(parent_hint.iter_mut()) {
                *source = map(source);
            }
            if let BindingTarget::Source(source) = new {
                *source = map(source);
            }
        }
        BindingRecord::Resolved { source, .. } => *source = map(source),
        BindingRecord::Rejected { .. } => {}
    }
}

/// An O-owned copy of `events` naming each file by the first source the log gave it, so a
/// renumbered rebind reads as the same source while the events keep their order.
pub(super) fn first_named(mut events: Vec<BindingEvent>) -> Vec<BindingEvent> {
    let mut first: HashMap<(String, std::path::PathBuf, u64), SourceId> = HashMap::new();
    for event in &mut events {
        map_sources(event, |source| {
            let key = (source.session_id.clone(), source.path.clone(), source.ino);
            first.entry(key).or_insert_with(|| source.clone()).clone()
        });
    }
    events
}

/// Why a stored source did not reopen: changed bytes halt even a retired source.
pub(super) enum Reopen {
    Changed,
    Unread(std::io::Error),
}

impl Reopen {
    pub(super) fn halt(self) -> WriterAlarm {
        match self {
            Self::Changed => halted("source bytes before the cursor changed"),
            Self::Unread(error) => halted(format!("source reopen: {error}")),
        }
    }
}

pub(super) fn reopen(cursor: &Cursor) -> Result<SourceCapture, Reopen> {
    let (source, through) = (cursor.source.clone(), cursor.captured_through);
    SourceCapture::reopen(source, through, &cursor.prefix_hash).map_err(|error| {
        match error.kind() {
            std::io::ErrorKind::InvalidData => Reopen::Changed,
            _ => Reopen::Unread(error),
        }
    })
}

/// Names each logged source by the cursor stored for its file, for the baseline and history.
pub(super) fn stored_names<P: DiscordPort, L: DeliveryLease, A: AlarmSink>(
    writer: &mut ChannelWriter<P, L, A>,
    events: &mut [BindingEvent],
) {
    let store = writer.store();
    for event in events {
        map_sources(event, |source| {
            canonical(store.cursors(), source).unwrap_or_else(|_| source.clone())
        });
    }
}

fn halted(detail: impl Into<String>) -> WriterAlarm {
    let detail = detail.into();
    WriterAlarm::Halted { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(dev: u64) -> Cursor {
        let source = SourceId {
            session_id: "s".into(),
            path: "/t.jsonl".into(),
            dev,
            ino: 9,
        };
        let (captured_through, prefix_hash, retired) = (0, String::new(), false);
        Cursor {
            source,
            captured_through,
            prefix_hash,
            retired,
        }
    }

    #[test]
    fn two_stored_variants_of_one_file_refuse_every_dev_including_their_own() {
        let stored = [cursor(1), cursor(2)];
        for dev in [1, 2, 3] {
            let asked = cursor(dev).source;
            assert!(canonical(stored.iter(), &asked).is_err(), "dev {dev}");
        }
        assert!(unique(stored.iter()).is_err());
        let one = [cursor(1)];
        assert_eq!(
            canonical(one.iter(), &cursor(3).source),
            Ok(cursor(1).source)
        );
        assert!(unique(one.iter()).is_ok());
    }
}
