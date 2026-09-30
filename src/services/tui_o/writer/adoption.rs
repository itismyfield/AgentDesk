//! Adoption of a selected Claude channel that already holds output: O starts at Legacy's own
//! cursor, only past a closed turn Legacy's delivery record covers, so no byte changes readers.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use super::binding::{BindingEvent, BindingEvents, BindingRecord, BindingTarget};
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_o::shadow::identity::{RecordFact, classify};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::InitSource;

/// As long as unmapped hooks are refused after the receiver starts, so a live Legacy pass is seen.
const LEGACY_START_WAIT: Duration = Duration::from_secs(60);
const LEGACY_START_POLL: Duration = Duration::from_millis(100);
/// Past sources are reopened and rehashed on every O boot; beyond these that boot is estimated
/// to take over a second per channel.
const PAST_BUDGET_BYTES: u64 = 128 << 20;
const PAST_BUDGET_SOURCES: usize = 64;

/// Legacy's cursor for one tmux session as this process's relay holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LegacyCursor {
    Bound {
        path: PathBuf,
        offset: u64,
    },
    /// The pane is live but Legacy holds no Claude cursor for it.
    Unbound,
    /// No live pane, so nothing writes the transcript.
    NoPane,
}

/// What Legacy's relay holds for a channel, read without deciding anything.
pub trait LegacyView: Send + Sync + 'static {
    /// Whether a rehydrate pass has listed tmux in this process, so every live pane has a cursor.
    fn started(&self) -> bool;
    fn cursor(&self, tmux: &str) -> LegacyCursor;
    /// Legacy's delivered frontier within `eof`; `None` while its delivery record is not authority.
    fn frontier(&self, channel: u64, tmux: &str, eof: u64) -> Option<u64>;
    fn tail_running(&self, tmux: &str) -> bool;
}

/// Waits for Legacy's first rehydrate pass; false once the wait is over without one.
pub async fn legacy_started(legacy: &dyn LegacyView) -> bool {
    let deadline = tokio::time::Instant::now() + LEGACY_START_WAIT;
    while !legacy.started() {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(LEGACY_START_POLL).await;
    }
    true
}

/// The sources a binding log names: every bound one in seq order, and those only named as an old
/// or parent source. A bind still pending refuses the log.
pub(super) fn logged(events: &[BindingEvent]) -> Result<(Vec<&SourceId>, Vec<&SourceId>), String> {
    let (mut bound, mut named, mut pending) = (Vec::new(), Vec::new(), Vec::new());
    for event in events {
        match &event.record {
            BindingRecord::Bound {
                old,
                new,
                parent_hint,
                ..
            } => {
                named.extend(old.iter().chain(parent_hint));
                match new {
                    BindingTarget::Source(source) => bound.push(source),
                    BindingTarget::Pending { .. } => pending.push(event.seq),
                }
            }
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } => {
                pending.retain(|seq| seq != resolves_seq);
                bound.push(source);
            }
            BindingRecord::Rejected { .. } => {}
        }
    }
    if let Some(seq) = pending.iter().min() {
        return Err(format!("bind {seq} is still pending"));
    }
    if bound.is_empty() {
        return Err("no source is bound".into());
    }
    named.retain(|source| !bound.contains(source));
    Ok((bound, named))
}

/// Whether a source the channel's log binds already holds bytes; an unreadable log says no, and
/// the empty-channel checks then refuse it.
pub fn holds_output<B: BindingEvents>(bindings: &B, channel: u64) -> bool {
    let Ok(events) = bindings.binding_events_since(channel, 0) else {
        return false;
    };
    let Ok((bound, _)) = logged(&events) else {
        return false;
    };
    let len = |source: &&SourceId| std::fs::metadata(&source.path).map_or(0, |meta| meta.len());
    bound.iter().any(|source| len(source) > 0)
}

/// A source as first read: its init starts at `len`, over bytes hashing to `hash`.
#[derive(Clone, Debug)]
struct Pinned {
    source: SourceId,
    len: u64,
    modified: Option<SystemTime>,
    hash: String,
}

impl Pinned {
    /// A stat only: the same file, the same length and the same modification time.
    fn unchanged(&self) -> Result<(), String> {
        let path = self.source.path.display();
        let meta = std::fs::metadata(&self.source.path);
        let meta = meta.map_err(|error| format!("source {path}: {error}"))?;
        if file_identity(&meta) != (self.source.dev, self.source.ino) {
            return Err(format!("source {path} was replaced"));
        }
        if meta.len() != self.len {
            return Err(format!("source {path} length moved off {}", self.len));
        }
        if meta.modified().ok() != self.modified {
            return Err(format!("source {path} mtime moved"));
        }
        Ok(())
    }

    fn init(&self) -> InitSource {
        InitSource {
            source_id: self.source.clone(),
            delivery_start: self.len,
            prefix_hash: self.hash.clone(),
        }
    }
}

/// Where the last turn ending before the cursor ends, and whether output follows it.
#[derive(Default)]
struct Turns {
    closed_at: u64,
    open: bool,
}

impl Turns {
    fn record(&mut self, line: &[u8], end: u64) {
        let Ok(record) = serde_json::from_slice(line) else {
            // An unreadable record after the last turn end may hold output.
            self.open |= !line.iter().all(u8::is_ascii_whitespace);
            return;
        };
        let facts = classify(ShadowProvider::Claude, &record);
        if facts.iter().any(|fact| matches!(fact, RecordFact::Idle(_))) {
            (self.closed_at, self.open) = (end, false);
        } else if !facts.is_empty() {
            self.open = true;
        }
    }
}

/// Reads `0..len` of `source` once: its hash, and for the current source its turns.
fn read(source: &SourceId, len: u64, turns: Option<&mut Turns>) -> Result<Pinned, String> {
    let path = source.path.display();
    let io = |error: std::io::Error| format!("source {path}: {error}");
    let file = File::open(&source.path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if file_identity(&meta) != (source.dev, source.ino) {
        return Err(format!("source {path} was replaced"));
    }
    if meta.len() != len {
        return Err(format!(
            "source {path} holds {} bytes, not {len}",
            meta.len()
        ));
    }
    let (mut hasher, mut reader) = (Sha256::new(), BufReader::new(file.take(len)));
    let (mut at, mut line, mut turns) = (0, Vec::new(), turns);
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line).map_err(io)?;
        if read == 0 {
            break;
        }
        hasher.update(&line);
        at += read as u64;
        if line.last() != Some(&b'\n') {
            return Err(format!("source {path} ends inside a line at {at}"));
        }
        if let Some(turns) = turns.as_deref_mut() {
            turns.record(&line, at);
        }
    }
    if at != len {
        return Err(format!("source {path} ended at {at} while read to {len}"));
    }
    Ok(Pinned {
        source: source.clone(),
        len,
        modified: meta.modified().ok(),
        hash: hex::encode(hasher.finalize()),
    })
}

/// Every source a channel's init would name, pinned outside the adoption lock.
#[derive(Debug)]
pub struct Snapshot {
    seq: u64,
    tmux: String,
    /// The current source first, then the ones bound before it.
    pinned: Vec<Pinned>,
    named: Vec<SourceId>,
}

/// Reads Legacy's cursor and pins every source the channel's `events` bind. The current one starts
/// at that cursor, which must end a line after a closed turn Legacy's frontier already covers.
pub fn pin(
    legacy: &dyn LegacyView,
    events: &[BindingEvent],
    channel: u64,
) -> Result<Snapshot, String> {
    #[cfg(test)]
    super::activation::test_hook::run(channel, super::activation::test_hook::Step::Snapshot)?;
    let (bound, named) = logged(events)?;
    let seq = events.last().map_or(0, |event| event.seq);
    let current = bound.last().copied().ok_or("no source is bound")?;
    let tmux = events
        .iter()
        .rev()
        .find(|event| event_binds(event, current))
        .map(|event| event.tmux_session.clone())
        .ok_or("no event binds the current source")?;
    let start = match legacy.cursor(&tmux) {
        LegacyCursor::Bound { path, offset } if path == current.path => offset,
        LegacyCursor::Bound { path, .. } => {
            return Err(format!("Legacy reads {} instead", path.display()));
        }
        LegacyCursor::Unbound => return Err("legacy cursor not established".into()),
        LegacyCursor::NoPane => len_of(&current.path)?,
    };
    let mut turns = Turns::default();
    let head = read(current, start, Some(&mut turns))?;
    if turns.open {
        return Err(format!("a turn after {} is still open", turns.closed_at));
    }
    let frontier = legacy.frontier(channel, &tmux, start);
    let frontier = frontier.ok_or("the delivery record is not authoritative")?;
    if !(turns.closed_at..=start).contains(&frontier) {
        let closed = turns.closed_at;
        return Err(format!("frontier {frontier} is outside {closed}..={start}"));
    }
    let mut pinned = vec![head];
    let mut past: Vec<&SourceId> = Vec::new();
    for &source in bound.iter().rev() {
        if source != current && !past.contains(&source) {
            past.push(source);
        }
    }
    let lens = past.iter().map(|source| len_of(&source.path));
    let lens: Vec<u64> = lens.collect::<Result<_, _>>()?;
    if past.len() > PAST_BUDGET_SOURCES || lens.iter().sum::<u64>() > PAST_BUDGET_BYTES {
        return Err("past sources exceed budget".into());
    }
    for (source, len) in past.into_iter().zip(lens) {
        pinned.push(read(source, len, None)?);
    }
    for source in &named {
        super::activation::still_empty(source)?;
    }
    Ok(Snapshot {
        seq,
        tmux,
        pinned,
        named: named.into_iter().cloned().collect(),
    })
}

fn event_binds(event: &BindingEvent, source: &SourceId) -> bool {
    match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(bound),
            ..
        }
        | BindingRecord::Resolved { source: bound, .. } => bound == source,
        _ => false,
    }
}

fn len_of(path: &Path) -> Result<u64, String> {
    let meta =
        std::fs::metadata(path).map_err(|error| format!("source {}: {error}", path.display()));
    Ok(meta?.len())
}

impl Snapshot {
    /// Rechecked under the adoption lock with stats only: the log, every pinned source and the
    /// running Legacy tails are as pinned. Returns the init's sources.
    pub fn recheck<B: BindingEvents>(
        &self,
        legacy: &dyn LegacyView,
        bindings: &B,
        channel: u64,
    ) -> Result<Vec<InitSource>, String> {
        let events = bindings.binding_events_since(channel, 0);
        let events = events.map_err(|error| format!("binding log: {error}"))?;
        if events.last().map_or(0, |event| event.seq) != self.seq {
            return Err(format!("the binding log moved past seq {}", self.seq));
        }
        let (current, past) = self.pinned.split_first().ok_or("nothing is pinned")?;
        current.unchanged()?;
        if legacy.tail_running(&self.tmux) {
            return Err("a Legacy response tail is running".into());
        }
        for pinned in past {
            pinned.unchanged()?;
        }
        for source in &self.named {
            super::activation::still_empty(source)?;
        }
        Ok(self.pinned.iter().map(Pinned::init).collect())
    }

    /// Where O starts on the current source.
    pub fn start(&self) -> u64 {
        self.pinned.first().map_or(0, |pinned| pinned.len)
    }
}

/// Fails closed at once: Legacy has started but holds no cursor.
#[cfg(test)]
pub(crate) struct NoLegacy;

#[cfg(test)]
impl LegacyView for NoLegacy {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Unbound
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        None
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "adoption_tests.rs"]
mod tests;
