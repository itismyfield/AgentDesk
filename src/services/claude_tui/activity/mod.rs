//! Source-scoped observation only. Input, queue, stop and close owners retain every effect.
mod hooks;
mod poll;
mod source;
mod tools;
mod transcript;

use chrono::{DateTime, Utc};
pub(crate) use hooks::note_hook;
use source::{Source, Stamp};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex, MutexGuard},
};
use transcript::{Frame, Turn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Foreground {
    Unknown,
    Submitted,
    Generating,
    Tool,
    Idle,
}
#[derive(Default, Clone, PartialEq, Eq)]
struct Entry {
    revision: u64,
    turn: Option<Turn>,
    stamp: Option<Stamp>,
    tools: tools::Tools,
    continuation: u64,
    high_water: u64,
    tainted: bool,
    frame: Option<Frame>,
}
#[derive(Default)]
struct Ledger {
    entries: HashMap<Source, Entry>,
}
static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(Default::default);
fn ledger() -> MutexGuard<'static, Ledger> {
    LEDGER.lock().unwrap_or_else(|e| e.into_inner())
}
const SOURCE_LIMIT: usize = 256;
fn entry<'a>(state: &'a mut Ledger, source: &Source) -> Option<&'a mut Entry> {
    state
        .entries
        .retain(|old, _| old.session != source.session || old == source);
    if state.entries.len() >= SOURCE_LIMIT && !state.entries.contains_key(source) {
        // A fully replayed terminal can be rebuilt from the pinned file before accepting any Pre.
        state.entries.retain(|_, entry| {
            entry
                .frame
                .as_ref()
                .is_none_or(|frame| !frame.caught_up() || frame.terminal == 0)
        });
        if state.entries.len() >= SOURCE_LIMIT {
            return None;
        }
    }
    Some(state.entries.entry(source.clone()).or_default())
}
fn fold(entry: &mut Entry, frame: &Frame) {
    if frame.stamp.len < entry.high_water
        || entry
            .stamp
            .as_ref()
            .is_some_and(|old| old.len == frame.stamp.len && old.modified != frame.stamp.modified)
    {
        entry.tainted = true;
    }
    entry.high_water = entry.high_water.max(frame.stamp.len);
    entry.turn = frame.turn.clone();
    entry.stamp = Some(frame.stamp.clone());
    entry.tools.reconcile(frame);
    entry.frame = Some(frame.clone());
}
fn read_source(source: &Source) -> Option<Frame> {
    let previous = ledger()
        .entries
        .get(source)
        .and_then(|entry| entry.frame.clone());
    transcript::read(source, previous)
}
fn installable(entry: &Entry, source: &Source, frame: &Frame) -> bool {
    source.stamp().as_ref() == Some(&frame.stamp)
        && entry
            .frame
            .as_ref()
            .is_none_or(|old| old.through <= frame.through)
}
fn foreground(entry: &Entry, frame: &Frame, now: DateTime<Utc>) -> Foreground {
    if entry.tainted || !frame.caught_up() || entry.tools.uncertain() {
        return Foreground::Unknown;
    }
    let Some(turn) = &frame.turn else {
        return Foreground::Unknown;
    };
    if frame.stamp.len <= entry.continuation {
        return Foreground::Unknown;
    }
    if frame.terminal > turn.floor && frame.terminal > entry.continuation {
        return Foreground::Idle;
    }
    if entry.tools.busy(frame, now) {
        return Foreground::Tool;
    }
    if let Some((offset, at)) = frame.progress
        && offset > entry.continuation
        && now >= at
        && now - at < chrono::Duration::seconds(2)
    {
        return Foreground::Generating;
    }
    if frame.uses.is_empty()
        && frame.progress.is_none()
        && now >= turn.at
        && now - turn.at < tools::TOOL_LEASE
    {
        return Foreground::Submitted;
    }
    Foreground::Unknown
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub foreground: Foreground,
    source: Option<Source>,
    turn: Option<Turn>,
    revision: u64,
    stamp: Option<Stamp>,
    expires: Option<DateTime<Utc>>,
}
impl Snapshot {
    fn unknown() -> Self {
        Self {
            foreground: Foreground::Unknown,
            source: None,
            turn: None,
            revision: 0,
            stamp: None,
            expires: None,
        }
    }
    /// Presence checks the same source and collector revision after the bounded read.
    pub(crate) fn current(&self) -> bool {
        if self.expires.is_some_and(|until| Utc::now() >= until) {
            return false;
        }
        let Some(source) = &self.source else {
            return false;
        };
        if Source::current(&source.session).as_ref() != Some(source) || source.stamp() != self.stamp
        {
            return false;
        }
        ledger()
            .entries
            .get(source)
            .is_some_and(|entry| entry.revision == self.revision && entry.turn == self.turn)
    }
}
pub(crate) fn observe(session: &str) -> Snapshot {
    poll::start();
    observe_at(session, Utc::now())
}
fn observe_at(session: &str, now: DateTime<Utc>) -> Snapshot {
    let Some(source) = Source::current(session) else {
        return Snapshot::unknown();
    };
    let Some(frame) = read_source(&source) else {
        return Snapshot::unknown();
    };
    let mut state = ledger();
    if Source::current(session).as_ref() != Some(&source) {
        return Snapshot::unknown();
    }
    let Some(entry) = entry(&mut state, &source) else {
        return Snapshot::unknown();
    };
    if !installable(entry, &source, &frame) {
        return Snapshot::unknown();
    }
    let before = entry.clone();
    fold(entry, &frame);
    if *entry != before {
        entry.revision += 1;
    }
    let foreground = foreground(entry, &frame, now);
    let expires = match foreground {
        Foreground::Tool => entry.tools.lease_end(&frame, now),
        Foreground::Generating => frame
            .progress
            .map(|(_, at)| at + chrono::Duration::seconds(2)),
        Foreground::Submitted => frame.turn.as_ref().map(|turn| turn.at + tools::TOOL_LEASE),
        _ => None,
    };
    let snapshot = Snapshot {
        foreground,
        expires,
        source: Some(source),
        turn: frame.turn,
        revision: entry.revision,
        stamp: Some(frame.stamp),
    };
    drop(state);
    if snapshot.current() {
        snapshot
    } else {
        Snapshot::unknown()
    }
}

#[cfg(test)]
pub(crate) fn poll_for_tests() {
    poll::tick();
}

#[cfg(test)]
pub(crate) fn revision_for_tests(session: &str) -> Option<u64> {
    let source = Source::current(session)?;
    ledger().entries.get(&source).map(|entry| entry.revision)
}
