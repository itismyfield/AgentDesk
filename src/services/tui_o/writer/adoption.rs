//! Adoption of a selected Claude channel that already holds output: O starts at Legacy's cursor, or
//! at the source's end once Legacy stalled behind it. Records Legacy left before it are only reported.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::WriterAlarm;
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
    /// What restarts Legacy's own redrive cycle for the channel; unchanged when nothing can say.
    fn epoch(&self, _channel: u64) -> LegacyEpoch {
        LegacyEpoch::default()
    }
}

/// Legacy's redrive episode as this process can read it: its frontier reset, watcher reattaches,
/// and the inflight row's identity (user message, start, tmux, turn start) and turn nonce.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LegacyEpoch {
    pub reset_incarnation: u64,
    pub reconnects: u64,
    pub turn: Option<(u64, String, Option<String>, Option<u64>, Option<String>)>,
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

/// Why a pin or its recheck did not take the channel, with the line it is logged as.
#[derive(Debug)]
pub struct Refused {
    pub hold: Hold,
    detail: String,
}

/// What a refusal waits on. A first attempt defers only an open turn; any other refusal is final.
#[derive(Debug, PartialEq, Eq)]
pub enum Hold {
    /// The last turn before Legacy's cursor is open in the file as it was read.
    OpenTurn(ReadVersion),
    /// Legacy's cursor is not yet bound at the end of the current source.
    Cursor { tmux: String, path: PathBuf },
    /// A bind is pending or the log moved; any later log entry may clear it.
    Binding,
    /// Legacy's delivered frontier has not reached the cursor on a record end.
    Delivery {
        read: ReadVersion,
        tmux: String,
        frontier: u64,
    },
    /// A read that may pass later, with nothing cheaper to wait on.
    Retry,
    /// Refused for the rest of this process.
    Final,
}

impl Refused {
    pub(super) fn new(hold: Hold, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self { hold, detail }
    }

    pub(super) fn retry(detail: impl Into<String>) -> Self {
        Self::new(Hold::Retry, detail)
    }

    /// Whether what refused may have changed since; false only while it surely still holds.
    pub fn may_pass(&self, legacy: &dyn LegacyView, channel: u64) -> bool {
        match &self.hold {
            Hold::OpenTurn(read) => read.moved(),
            Hold::Cursor { tmux, path } => match legacy.cursor(tmux) {
                LegacyCursor::Bound { path: at, offset } => {
                    at == *path && len_of(path).is_ok_and(|len| len == offset)
                }
                LegacyCursor::Unbound => false,
                LegacyCursor::NoPane => true,
            },
            Hold::Binding => false,
            Hold::Delivery {
                read,
                tmux,
                frontier,
            } => read.moved() || legacy.frontier(channel, tmux, read.len) != Some(*frontier),
            Hold::Retry | Hold::Final => true,
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl From<Refused> for String {
    fn from(refused: Refused) -> Self {
        refused.detail
    }
}

/// A source file as one read opened it: identity, length and modification time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadVersion {
    path: PathBuf,
    identity: (u64, u64),
    len: u64,
    modified: Option<SystemTime>,
}

impl ReadVersion {
    /// The file at `path` as it stands now.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            identity: file_identity(&meta),
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn moved(&self) -> bool {
        Self::of(&self.path).as_ref() != Some(self)
    }
}

/// Whether `later` supersedes `pending` as the writer drops it, never to resolve it: a hook on the
/// pane adopting another session, a later Pending, or a prompt of its own session it outlived.
pub(super) fn supersedes(pending: &BindingEvent, later: &BindingEvent) -> bool {
    #[cfg(test)]
    use crate::services::claude_tui::source_verify::n2b_mutant;
    let same_pane = later.tmux_session == pending.tmux_session;
    #[cfg(test)]
    let same_pane = same_pane || n2b_mutant("r5-overwrite-any-pane");
    let claude = later.provider == ShadowProvider::Claude;
    let BindingRecord::Bound {
        old, new, evidence, ..
    } = &later.record
    else {
        return false;
    };
    if later.seq <= pending.seq || !same_pane || !claude {
        return false;
    }
    let source = match new {
        #[cfg(test)]
        BindingTarget::Pending { .. } if n2b_mutant("r5-overwrite-off") => return false,
        BindingTarget::Pending { .. } => return true,
        BindingTarget::Source(source) => source,
    };
    #[cfg(test)]
    if n2b_mutant("o-supersede-off") {
        return false;
    }
    let moved = old
        .as_ref()
        .is_none_or(|old| old.session_id != source.session_id);
    // The writer's own fold judged the prompt reclaim; O follows that one judgment.
    let reclaims = evidence.reclaims;
    #[cfg(test)]
    let reclaims = reclaims && !n2b_mutant("r5-reclaim-o-off");
    !evidence.hook_event.is_empty() && (moved || reclaims)
}

/// The sources a binding log names: every bound one in seq order, and those only named as an old
/// or parent source. A bind still pending refuses the log; a superseded one does not.
pub(super) fn logged(events: &[BindingEvent]) -> Result<(Vec<&SourceId>, Vec<&SourceId>), Refused> {
    let (mut bound, mut named, mut pending) = (Vec::new(), Vec::new(), Vec::<&BindingEvent>::new());
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
                    BindingTarget::Source(source) => {
                        pending.retain(|waiting| !supersedes(waiting, event));
                        bound.push(source);
                    }
                    BindingTarget::Pending { .. } => {
                        pending.retain(|waiting| !supersedes(waiting, event));
                        pending.push(event);
                    }
                }
            }
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } => {
                pending.retain(|waiting| waiting.seq != *resolves_seq);
                bound.push(source);
            }
            BindingRecord::Rejected { .. } => {}
        }
    }
    if let Some(seq) = pending.iter().map(|waiting| waiting.seq).min() {
        return Err(Refused::new(
            Hold::Binding,
            format!("bind {seq} is still pending"),
        ));
    }
    if bound.is_empty() {
        return Err(Refused::new(Hold::Final, "no source is bound"));
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

    fn version(&self) -> ReadVersion {
        ReadVersion {
            path: self.source.path.clone(),
            identity: (self.source.dev, self.source.ino),
            len: self.len,
            modified: self.modified,
        }
    }

    fn init(&self) -> InitSource {
        InitSource {
            source_id: self.source.clone(),
            delivery_start: self.len,
            prefix_hash: self.hash.clone(),
        }
    }
}

/// The records before the cursor as they bear on Legacy's delivered frontier: whether output
/// follows the last turn end, and the first record past the frontier that is not quiet.
struct Turns {
    frontier: u64,
    closed_at: u64,
    open: bool,
    /// Whether the frontier is 0 or ends a record.
    frontier_on_line: bool,
    undelivered: Option<u64>,
}

/// What neither Legacy nor O posts: turn ends and the TUI's own bookkeeping. Anything else
/// may post or start a turn.
fn quiet(record: &Value) -> bool {
    let field = |value: &Value, key| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let attachment = record.get("attachment").unwrap_or(&Value::Null);
    match field(record, "type").as_deref() {
        Some("last-prompt" | "ai-title" | "mode" | "permission-mode") => true,
        Some("atis-latch" | "cost-state" | "file-history-snapshot") => true,
        Some("system") => matches!(
            field(record, "subtype").as_deref(),
            Some("stop_hook_summary" | "turn_duration" | "informational")
        ),
        Some("attachment") => field(attachment, "type").as_deref() == Some("hook_success"),
        _ => false,
    }
}

impl Turns {
    fn new(frontier: u64) -> Self {
        Self {
            frontier,
            closed_at: 0,
            open: false,
            frontier_on_line: frontier == 0,
            undelivered: None,
        }
    }

    fn record(&mut self, line: &[u8], start: u64, end: u64) {
        self.frontier_on_line |= end == self.frontier;
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let record: Option<Value> = serde_json::from_slice(line).ok();
        let facts = record.as_ref().map(|r| classify(ShadowProvider::Claude, r));
        let facts = facts.unwrap_or_default();
        if facts.iter().any(|fact| matches!(fact, RecordFact::Idle(_))) {
            (self.closed_at, self.open) = (end, false);
        } else if !facts.is_empty() || record.is_none() {
            // An unreadable record after the last turn end may hold output.
            self.open = true;
        }
        if start < self.frontier {
            return;
        }
        let opens =
            |fact: &RecordFact| matches!(fact, RecordFact::Prompt(..) | RecordFact::TurnStart(_));
        let posts = facts
            .iter()
            .any(|fact| !opens(fact) && !matches!(fact, RecordFact::Idle(_)));
        let user = record.as_ref().and_then(|r| r.get("type")) == Some(&Value::from("user"));
        let prompts = user || facts.iter().any(opens);
        if !posts && !prompts && record.as_ref().is_some_and(quiet) {
            return;
        }
        self.undelivered.get_or_insert(start);
    }
}

/// Why a read of `0..len` failed: the file did not end at `len` as a record end, or anything else.
enum Unread {
    Short(String),
    Other(String),
}

/// Reads `0..len` of `source` once: its hash, and for the current source its turns.
fn read(source: &SourceId, len: u64, turns: Option<&mut Turns>) -> Result<Pinned, Unread> {
    let path = source.path.display();
    let io = |error: std::io::Error| Unread::Other(format!("source {path}: {error}"));
    let file = File::open(&source.path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if file_identity(&meta) != (source.dev, source.ino) {
        return Err(Unread::Other(format!("source {path} was replaced")));
    }
    if meta.len() != len {
        let held = meta.len();
        return Err(Unread::Short(format!(
            "source {path} holds {held} bytes, not {len}"
        )));
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
        let start = at;
        at += read as u64;
        if line.last() != Some(&b'\n') {
            return Err(Unread::Short(format!(
                "source {path} ends inside a line at {at}"
            )));
        }
        if let Some(turns) = turns.as_deref_mut() {
            turns.record(&line, start, at);
        }
    }
    if at != len {
        return Err(Unread::Other(format!(
            "source {path} ended at {at} while read to {len}"
        )));
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
    /// Legacy's cursor on the current source when the pin read it.
    cursor: Option<u64>,
    /// The current source first, then the ones bound before it.
    pinned: Vec<Pinned>,
    named: Vec<SourceId>,
    /// The first record past Legacy's delivered frontier that is not quiet.
    undelivered: Option<u64>,
    frontier: u64,
}

/// Where a pin starts the current source.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum At {
    /// At Legacy's cursor, which must be the source's end.
    Cursor,
    /// At the source's end, with Legacy's cursor bound on it at or before that end.
    End,
}

/// Pins every source the channel's `events` bind; the current one starts at Legacy's cursor, after
/// a closed turn and with Legacy's delivered frontier on a record at or before it.
pub fn pin(
    legacy: &dyn LegacyView,
    events: &[BindingEvent],
    channel: u64,
) -> Result<Snapshot, Refused> {
    pin_at(legacy, events, channel, At::Cursor)
}

/// As `pin`, starting the current source where `at` says. A pin at the end checks the frontier
/// before the turn, so an open turn it reports already has a frontier on a record within the end.
pub fn pin_at(
    legacy: &dyn LegacyView,
    events: &[BindingEvent],
    channel: u64,
    at: At,
) -> Result<Snapshot, Refused> {
    #[cfg(test)]
    super::activation::test_hook::run(channel, super::activation::test_hook::Step::Snapshot)
        .map_err(Refused::retry)?;
    let (bound, named) = logged(events)?;
    let seq = events.last().map_or(0, |event| event.seq);
    let current = bound
        .last()
        .copied()
        .ok_or(Refused::new(Hold::Final, "no source is bound"))?;
    let tmux = events
        .iter()
        .rev()
        .find(|event| event_binds(event, current))
        .map(|event| event.tmux_session.clone())
        .ok_or(Refused::retry("no event binds the current source"))?;
    let waiting = |detail: String| {
        let (tmux, path) = (tmux.clone(), current.path.clone());
        Refused::new(Hold::Cursor { tmux, path }, detail)
    };
    let cursor = match legacy.cursor(&tmux) {
        LegacyCursor::Bound { path, offset } if path == current.path => Some(offset),
        LegacyCursor::Bound { path, .. } => {
            return Err(waiting(format!("Legacy reads {} instead", path.display())));
        }
        LegacyCursor::Unbound => return Err(waiting("legacy cursor not established".into())),
        LegacyCursor::NoPane => None,
    };
    let start = match (at, cursor) {
        (At::Cursor, Some(offset)) => offset,
        (At::Cursor, None) => len_of(&current.path).map_err(Refused::retry)?,
        (At::End, cursor) => {
            let end = len_of(&current.path).map_err(Refused::retry)?;
            if cursor.is_some_and(|offset| offset > end) {
                return Err(waiting(format!("Legacy's cursor is past {end}")));
            }
            end
        }
    };
    let frontier = legacy.frontier(channel, &tmux, start);
    let not_authority = || Refused::new(Hold::Final, "the delivery record is not authoritative");
    let frontier = frontier.ok_or_else(not_authority)?;
    let mut turns = Turns::new(frontier);
    let head = match read(current, start, Some(&mut turns)) {
        Ok(head) => head,
        Err(Unread::Short(detail)) => return Err(waiting(detail)),
        Err(Unread::Other(detail)) => return Err(Refused::retry(detail)),
    };
    let open = || {
        let detail = format!("a turn after {} is still open", turns.closed_at);
        Refused::new(Hold::OpenTurn(head.version()), detail)
    };
    if turns.open && at == At::Cursor {
        return Err(open());
    }
    if !turns.frontier_on_line || frontier > start {
        let (read, tmux) = (head.version(), tmux.clone());
        let detail = format!("frontier {frontier} ends no record within ..={start}");
        let hold = Hold::Delivery {
            read,
            tmux,
            frontier,
        };
        return Err(Refused::new(hold, detail));
    }
    if turns.open {
        return Err(open());
    }
    let mut pinned = vec![head];
    let past = past_of(&bound);
    let lens = past.iter().map(|source| len_of(&source.path));
    let lens: Vec<u64> = lens.collect::<Result<_, _>>().map_err(Refused::retry)?;
    if past.len() > PAST_BUDGET_SOURCES || lens.iter().sum::<u64>() > PAST_BUDGET_BYTES {
        return Err(Refused::new(Hold::Final, "past sources exceed budget"));
    }
    for (source, len) in past.into_iter().zip(lens) {
        match read(source, len, None) {
            Ok(read) => pinned.push(read),
            Err(Unread::Short(detail) | Unread::Other(detail)) => {
                return Err(Refused::retry(detail));
            }
        }
    }
    for source in &named {
        let still = super::activation::still_empty(source);
        still.map_err(|detail| Refused::new(Hold::Final, detail))?;
    }
    Ok(Snapshot {
        seq,
        tmux,
        cursor,
        pinned,
        named: named.into_iter().cloned().collect(),
        undelivered: turns.undelivered,
        frontier,
    })
}

/// The sources bound before the current one, newest first and each once.
fn past_of<'a>(bound: &[&'a SourceId]) -> Vec<&'a SourceId> {
    let Some(&current) = bound.last() else {
        return Vec::new();
    };
    let mut past: Vec<&SourceId> = Vec::new();
    for &source in bound.iter().rev() {
        if source != current && !past.contains(&source) {
            past.push(source);
        }
    }
    past
}

/// A source other than `current` that the log bound after `seq`; Legacy may still owe output
/// from it, read while it was current.
pub fn rotated<'a>(
    events: &'a [BindingEvent],
    seq: u64,
    current: &SourceId,
) -> Option<&'a SourceId> {
    let bound = |event: &'a BindingEvent| match &event.record {
        BindingRecord::Bound {
            new: BindingTarget::Source(source),
            ..
        }
        | BindingRecord::Resolved { source, .. } => Some(source),
        _ => None,
    };
    let later = events.iter().filter(|event| event.seq > seq);
    later.filter_map(bound).find(|source| *source != current)
}

/// The source a binding log binds now and its tmux session; none while the log refuses.
pub fn current(events: &[BindingEvent]) -> Option<(SourceId, String)> {
    let current = *logged(events).ok()?.0.last()?;
    let event = events
        .iter()
        .rev()
        .find(|event| event_binds(event, current))?;
    Some((current.clone(), event.tmux_session.clone()))
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
    ) -> Result<Vec<InitSource>, Refused> {
        let events = bindings.binding_events_since(channel, 0);
        let events = events.map_err(|error| Refused::retry(format!("binding log: {error}")))?;
        if events.last().map_or(0, |event| event.seq) != self.seq {
            let detail = format!("the binding log moved past seq {}", self.seq);
            return Err(Refused::new(Hold::Binding, detail));
        }
        let (current, past) = self
            .pinned
            .split_first()
            .ok_or(Refused::retry("nothing is pinned"))?;
        current.unchanged().map_err(Refused::retry)?;
        if legacy.tail_running(&self.tmux) {
            return Err(Refused::retry("a Legacy response tail is running"));
        }
        for pinned in past {
            pinned.unchanged().map_err(Refused::retry)?;
        }
        for source in &self.named {
            let still = super::activation::still_empty(source);
            still.map_err(|detail| Refused::new(Hold::Final, detail))?;
        }
        Ok(self.pinned.iter().map(Pinned::init).collect())
    }

    /// Where O starts on the current source.
    pub fn start(&self) -> u64 {
        self.pinned.first().map_or(0, |pinned| pinned.len)
    }

    /// Why a runtime adoption must wait: a record past Legacy's frontier that Legacy may still send.
    pub fn owed(&self) -> Option<Refused> {
        let current = self.pinned.first()?;
        let from = self.undelivered?;
        let (read, tmux, frontier) = (current.version(), self.tmux.clone(), self.frontier);
        let detail = format!("a record at {from} is past frontier {frontier}");
        let hold = Hold::Delivery {
            read,
            tmux,
            frontier,
        };
        Some(Refused::new(hold, detail))
    }

    /// The records Legacy left undelivered before O's start; neither writer posts them.
    pub fn abandoned(&self) -> Option<WriterAlarm> {
        let current = self.pinned.first()?;
        Some(WriterAlarm::Abandoned {
            source: current.source.clone(),
            from: self.undelivered?,
            to: current.len,
        })
    }

    /// Whether Legacy is behind O's start: its cursor short of it or a record past its frontier.
    pub fn behind(&self) -> bool {
        self.undelivered.is_some() || self.cursor != Some(self.start())
    }

    /// Whether the current source and Legacy's cursor and frontier still read as this pin saw them.
    pub fn unchanged(&self, legacy: &dyn LegacyView, channel: u64) -> bool {
        let Some(current) = self.pinned.first() else {
            return false;
        };
        let cursor = match legacy.cursor(&self.tmux) {
            LegacyCursor::Bound { path, offset } if path == current.source.path => Some(offset),
            LegacyCursor::NoPane => None,
            _ => return false,
        };
        let frontier = legacy.frontier(channel, &self.tmux, current.len);
        ReadVersion::of(&current.source.path).as_ref() == Some(&current.version())
            && cursor == self.cursor
            && frontier == Some(self.frontier)
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
