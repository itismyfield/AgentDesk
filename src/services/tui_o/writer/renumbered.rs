//! A binding event names its source with the dev its stat read, and a reboot may renumber that
//! dev. O keeps the identity it stored first and maps every later variant of the file onto it.

use std::collections::HashMap;

use super::WriterAlarm;
use super::binding::{BindingEvent, BindingRecord, BindingTarget};
use super::rotation::{binding_baseline, bound_source};
use crate::services::tui_o::shadow::capture::{SourceCapture, same_file};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::ChannelStore;
use crate::services::tui_o::store::spool::{Cursor, source_key};

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

/// Binding events as logged and as named by the stored sources, for the baseline and history.
pub(super) type Logged = (Vec<BindingEvent>, Vec<BindingEvent>);

/// How a running writer names logged sources. A Claude hook may later fill the session a stored
/// source was bound with empty, so that empty name also stands for one filled session.
pub(super) struct Names {
    provider: ShadowProvider,
    /// The one filled session each stored source with an empty session was taken for, by key.
    confirmed: HashMap<String, String>,
    /// Unset until the whole log was read, so the confirmed sessions are not yet known.
    ready: bool,
}

impl Names {
    pub(super) fn new(provider: ShadowProvider) -> Self {
        let (confirmed, ready) = (HashMap::new(), false);
        Self {
            provider,
            confirmed,
            ready,
        }
    }

    /// Where a read of the log past `after` starts: at 0 until the whole log was read.
    pub(super) fn from(&self, after: u64) -> u64 {
        if self.ready { after } else { 0 }
    }

    /// The stored cursor of `source`'s file: the one exactly naming it, else for Claude the only
    /// one an empty session leaves compatible, flagged as such. Two exact ones are refused.
    fn stored<'a>(
        &self,
        store: &'a ChannelStore,
        source: &SourceId,
    ) -> Result<Option<(&'a Cursor, bool)>, String> {
        let exact = canonical(store.cursors(), source)?;
        if let Some(cursor) = store.cursors().find(|cursor| cursor.source == exact) {
            return Ok(Some((cursor, false)));
        }
        if self.provider != ShadowProvider::Claude {
            return Ok(None);
        }
        let mut loose = store
            .cursors()
            .filter(|cursor| self.compatible(&cursor.source, source));
        match (loose.next(), loose.next()) {
            (Some(one), None) => Ok(Some((one, true))),
            _ => Ok(None),
        }
    }

    /// Same path and inode, sessions equal or one empty, and not a source taken for another one.
    fn compatible(&self, stored: &SourceId, source: &SourceId) -> bool {
        let (path, ino) = (stored.path == source.path, stored.ino == source.ino);
        let (one, other) = (&stored.session_id, &source.session_id);
        let sessions = one == other || one.is_empty() || other.is_empty();
        let taken = self.confirmed.get(&source_key(stored));
        path && ino && sessions && taken.is_none_or(|taken| taken == other)
    }

    fn confirm(&mut self, stored: &SourceId, source: &SourceId) {
        if stored.session_id.is_empty() && !source.session_id.is_empty() {
            let session = source.session_id.clone();
            self.confirmed.entry(source_key(stored)).or_insert(session);
        }
    }

    /// The native session of a stored source: the one it was taken for when stored empty.
    pub(super) fn session<'a>(&'a self, source: &'a SourceId) -> &'a str {
        let taken = self.confirmed.get(&source_key(source));
        taken.map_or(source.session_id.as_str(), String::as_str)
    }

    /// The stored name a bind uses. A compatible stored source is taken only once its bytes before
    /// the cursor still read as stored; otherwise the bind halts rather than attach the file anew.
    pub(super) fn bind_name(
        &mut self,
        store: &ChannelStore,
        source: &SourceId,
    ) -> Result<SourceId, WriterAlarm> {
        match self.stored(store, source).map_err(halted)? {
            Some((cursor, true)) => {
                reopen(cursor).map_err(Reopen::halt)?;
                self.confirm(&cursor.source, source);
                Ok(cursor.source.clone())
            }
            Some((cursor, false)) => Ok(cursor.source.clone()),
            None => Ok(source.clone()),
        }
    }

    /// `events` past `after`, raw and by stored name; a whole log first rebuilds the sessions taken
    /// by events through `applied`, or by every event without a checkpoint.
    pub(super) fn read(
        &mut self,
        store: &ChannelStore,
        events: Vec<BindingEvent>,
        after: u64,
        applied: Option<u64>,
    ) -> Result<Option<Logged>, WriterAlarm> {
        if self.from(after) == 0 {
            self.rebuild(store, &events, applied).map_err(halted)?;
        }
        let mut named = events.clone();
        let mut failed = Ok(());
        for event in &mut named {
            map_sources(event, |source| match self.stored(store, source) {
                Ok(stored) => stored.map_or_else(|| source.clone(), |(c, _)| c.source.clone()),
                Err(error) => {
                    failed = Err(error);
                    source.clone()
                }
            });
        }
        failed.map_err(halted)?;
        let past = |event: &BindingEvent| event.seq > after;
        let (events, named) = (
            events.into_iter().filter(past),
            named.into_iter().filter(past),
        );
        Ok(Some((events.collect(), named.collect())))
    }

    /// Takes again, in seq order, the sessions the binds through `applied` took, as `follow` binds:
    /// a Pending binds the source its Resolved names, and a superseded one binds nothing.
    fn rebuild(
        &mut self,
        store: &ChannelStore,
        events: &[BindingEvent],
        applied: Option<u64>,
    ) -> Result<(), String> {
        (self.confirmed, self.ready) = (HashMap::new(), true);
        let resolved: HashMap<u64, &SourceId> = (events.iter())
            .filter_map(|event| match &event.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } => Some((*resolves_seq, source)),
                _ => None,
            })
            .collect();
        for event in events
            .iter()
            .filter(|e| applied.is_none_or(|seq| e.seq <= seq))
        {
            let BindingRecord::Bound {
                old,
                new,
                parent_hint,
                ..
            } = &event.record
            else {
                continue;
            };
            let new = match new {
                BindingTarget::Source(source) => Some(source),
                BindingTarget::Pending { .. } => resolved.get(&event.seq).copied(),
            };
            let Some(new) = new else {
                continue;
            };
            for source in old.iter().chain(parent_hint).chain([new]) {
                if let Some((cursor, true)) = self.stored(store, source)? {
                    self.confirm(&cursor.source, source);
                }
            }
        }
        Ok(())
    }

    /// Where a resume or seed starts: the checkpoint, else the last bind of an attached source,
    /// whose name an empty session leaves compatible must still read as stored to be taken.
    pub(super) fn start(
        &self,
        store: &ChannelStore,
        (raw, named): &Logged,
        checkpoint: Option<u64>,
    ) -> Result<Option<u64>, WriterAlarm> {
        if checkpoint.is_some() {
            return Ok(checkpoint);
        }
        let Some(seq) = binding_baseline(named, |source| store.cursor(source).is_some()) else {
            return Ok(None);
        };
        let source = raw
            .iter()
            .find(|event| event.seq == seq)
            .and_then(bound_source);
        let stored = source.map(|source| self.stored(store, source)).transpose();
        if let Some((cursor, true)) = stored.map_err(halted)?.flatten() {
            reopen(cursor).map_err(Reopen::halt)?;
        }
        Ok(Some(seq))
    }

    /// The whole log by stored name, or `None` when it or a name cannot be read, without alarms.
    pub(super) fn history(
        &mut self,
        store: &ChannelStore,
        read: Result<Vec<BindingEvent>, String>,
        applied: Option<u64>,
    ) -> Option<Vec<BindingEvent>> {
        let (_, named) = self.read(store, read.ok()?, 0, applied).ok()??;
        Some(named)
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
