//! Session-level Claude turn-interrupt ownership.

use super::cancel_token_cleanup::authority::{self, KillAuthorization, SessionKillGuard};
use super::{CancelToken, ProviderKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Observations of this token's Herdr input, not a source or settlement authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HerdrSubmission {
    Unsubmitted,
    Submitted,
    Unknown,
}

pub(crate) struct HerdrInterruptState {
    pub(crate) owner: crate::db::dispatched_sessions::hosted_execution::HostedOwner,
    pub(crate) submission: Mutex<super::herdr_before_start::HerdrInputState>,
    pub(crate) user_stop: AtomicBool,
    /// Where this token's own input began; terminal admission reads its turn from here only.
    pub(crate) turn_start: std::sync::OnceLock<HerdrTurnStart>,
    /// Whether this turn's reader has seen its own start, and a stop that met the turn unbound.
    pub(crate) own_start: Mutex<OwnStart>,
    /// The backstop's read of this turn from its own start, resumed poll to poll.
    pub(crate) own_turn: Mutex<OwnTurnCursor>,
}

/// A stop's one late delivery attempt; it holds the token only weakly.
pub(crate) type LateStop = Box<dyn Fn() + Send>;

pub(crate) enum OwnStart {
    Unseen(Option<LateStop>),
    /// `progress` is the reader's complete-record end; a kept retry runs once the reader passes `after`.
    /// `turn_id` is the native ID of the start the reader first saw; later reads never replace it.
    Seen {
        progress: u64,
        retry: Option<(LateStop, u64)>,
        turn_id: String,
    },
}

/// The source and offset an executor observed at this token's input, never taken from a frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HerdrTurnStart {
    pub(crate) execution_nonce: String,
    pub(crate) source: std::path::PathBuf,
    /// The source's (dev, ino) at the input; a cold start's transcript does not exist yet.
    pub(crate) file: Option<(u64, u64)>,
    pub(crate) offset: u64,
    /// Claude's input instant: its turn begins at the first record stamped at or after it.
    pub(crate) submitted_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl HerdrInterruptState {
    /// Records the turn start once; a different second start is refused, never overwritten.
    pub(crate) fn record_turn_start(&self, start: HerdrTurnStart) -> bool {
        match self.turn_start.set(start) {
            Ok(()) => true,
            Err(start) => self.turn_start.get() == Some(&start),
        }
    }

    /// Keeps `stop` until the reader first sees this turn's own start; `false`, dropping it, when
    /// that was already seen. Only the first armed stop is kept.
    pub(crate) fn arm_late_stop(&self, stop: LateStop) -> bool {
        match &mut *self.own_start.lock().unwrap_or_else(|e| e.into_inner()) {
            OwnStart::Unseen(armed) => {
                armed.get_or_insert(stop);
                true
            }
            OwnStart::Seen { .. } => false,
        }
    }

    /// The reader saw this turn's own start `turn_id`, records complete up to `progress`: an armed
    /// stop runs once, outside the slot, and a kept retry once the reader passed its record.
    pub(crate) fn own_start_observed(&self, progress: u64, turn_id: &str) {
        let mut slot = self.own_start.lock().unwrap_or_else(|e| e.into_inner());
        let run = match &mut *slot {
            OwnStart::Unseen(armed) => {
                let armed = armed.take();
                *slot = OwnStart::Seen {
                    progress,
                    retry: None,
                    turn_id: turn_id.to_owned(),
                };
                armed
            }
            OwnStart::Seen {
                progress: seen,
                retry,
                ..
            } => {
                *seen = (*seen).max(progress);
                match retry {
                    Some((_, after)) if progress > *after => retry.take().map(|(stop, _)| stop),
                    _ => None,
                }
            }
        };
        drop(slot);
        if let Some(stop) = run {
            stop();
        }
    }

    /// The reader's complete-record end once it has seen this turn's own start.
    pub(crate) fn seen_progress(&self) -> Option<u64> {
        match &*self.own_start.lock().unwrap_or_else(|e| e.into_inner()) {
            OwnStart::Seen { progress, .. } => Some(*progress),
            OwnStart::Unseen(_) => None,
        }
    }

    /// The native ID of this turn's start as its reader first saw it.
    pub(crate) fn seen_turn_id(&self) -> Option<String> {
        match &*self.own_start.lock().unwrap_or_else(|e| e.into_inner()) {
            OwnStart::Seen { turn_id, .. } => Some(turn_id.clone()),
            OwnStart::Unseen(_) => None,
        }
    }

    /// Keeps one more attempt of a stop that sent nothing for the reader's next record past `after`.
    pub(crate) fn retry_late_stop(&self, stop: LateStop, after: u64) {
        if let OwnStart::Seen { retry, .. } =
            &mut *self.own_start.lock().unwrap_or_else(|e| e.into_inner())
        {
            retry.get_or_insert((stop, after));
        }
    }
}

impl HerdrTurnStart {
    /// The start of input to `source` at its current end, read before the input is written.
    pub(crate) fn at_end_of(
        execution_nonce: &str,
        source: &std::path::Path,
        submitted_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Self {
        let meta = std::fs::metadata(source).ok();
        Self {
            execution_nonce: execution_nonce.to_owned(),
            source: source.to_owned(),
            file: meta.as_ref().and_then(file_identity),
            offset: meta.map_or(0, |meta| meta.len()),
            submitted_at,
        }
    }

    /// The Codex turn that began at this input: the first record there of any turn must start a
    /// named turn.
    pub(crate) fn codex_own_turn(&self) -> Result<CodexOwnTurn, OwnTurnRead> {
        use std::io::{BufRead, Seek};
        let mut file = std::fs::File::open(&self.source).map_err(|_| OwnTurnRead::Unreadable)?;
        let opened = file.metadata().ok();
        if self.file.is_some() && opened.as_ref().and_then(file_identity) != self.file {
            return Err(OwnTurnRead::Foreign);
        }
        file.seek(std::io::SeekFrom::Start(self.offset))
            .map_err(|_| OwnTurnRead::Unreadable)?;
        let mut reader = std::io::BufReader::new(file);
        let (mut line, mut offset, mut own) = (Vec::new(), self.offset, None);
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .map_err(|_| OwnTurnRead::Unreadable)?;
            if read == 0 {
                return Err(OwnTurnRead::NotYet);
            }
            if !line.ends_with(b"\n") {
                return Err(OwnTurnRead::Unreadable);
            }
            own_turn_step(&mut own, &line, offset)?;
            if let Some(turn) = own.take() {
                return Ok(turn);
            }
            offset += read as u64;
        }
    }
}

/// Steps one complete rollout record at `position` of the turn read from its input: `Ok(true)`
/// once that turn ended or another began; any turn's record before its start is `Foreign`.
fn own_turn_step(
    own: &mut Option<CodexOwnTurn>,
    line: &[u8],
    position: u64,
) -> Result<bool, OwnTurnRead> {
    use crate::services::agent_protocol::{codex_payload_turn_id, same_codex_turn};
    let record: serde_json::Value =
        serde_json::from_slice(line).map_err(|_| OwnTurnRead::Unreadable)?;
    let payload = &record["payload"];
    let kind = payload["type"].as_str().unwrap_or("");
    let named = codex_payload_turn_id(payload);
    let Some(turn) = own.as_mut() else {
        match record["type"].as_str() {
            Some("event_msg") if kind == "task_started" => {
                let turn_id = named.ok_or(OwnTurnRead::Foreign)?.to_owned();
                *own = Some(CodexOwnTurn {
                    started_at: position,
                    turn_id,
                    aborted: false,
                });
            }
            Some("event_msg") if matches!(kind, "task_complete" | "turn_aborted") => {
                return Err(OwnTurnRead::Foreign);
            }
            Some("response_item") if codex_turn_content(payload) => {
                return Err(OwnTurnRead::Foreign);
            }
            _ => {}
        }
        return Ok(false);
    };
    let mine = same_codex_turn(Some(&turn.turn_id), named);
    Ok(match kind {
        "turn_aborted" if mine => {
            turn.aborted = true;
            true
        }
        "task_complete" if mine => true,
        "task_started" => !mine,
        _ => false,
    })
}

/// Bytes one backstop poll reads of a held turn; a longer turn is read on at later polls.
pub(crate) const OWN_TURN_POLL_BUDGET: u64 = 256 * 1024;

/// The backstop's read of a held turn from its own input start.
#[derive(Default)]
pub(crate) struct OwnTurnCursor {
    /// The identity of the file read and the end of its records consumed; `None` before a read.
    at: Option<(Option<(u64, u64)>, u64)>,
    /// An incomplete last line, kept until its newline is written.
    partial: Vec<u8>,
    own: Option<CodexOwnTurn>,
    /// The read is over: the turn ended or another began, or what followed the input was not it.
    over: bool,
    /// Bytes the last poll read.
    #[cfg(test)]
    pub(crate) polled: u64,
}

impl OwnTurnCursor {
    /// Reads at most the poll budget past `consumed` and steps each complete record read.
    fn read_on(&mut self, file: std::fs::File, consumed: u64) -> std::io::Result<()> {
        use std::io::{Read, Seek};
        let budget = OWN_TURN_POLL_BUDGET;
        #[cfg(test)]
        let budget = match herdr_interrupt_mutant("backstop_budget_ignored") {
            true => u64::MAX,
            false => budget,
        };
        let mut file = file;
        let kept = self.partial.len();
        file.seek(std::io::SeekFrom::Start(consumed + kept as u64))?;
        file.take(budget).read_to_end(&mut self.partial)?;
        #[cfg(test)]
        {
            self.polled = (self.partial.len() - kept) as u64;
        }
        let mut used = 0;
        while !self.over
            && let Some(end) = self.partial[used..].iter().position(|byte| *byte == b'\n')
        {
            let line = &self.partial[used..=used + end];
            match own_turn_step(&mut self.own, line, consumed + used as u64) {
                Ok(over) => self.over = over,
                Err(_) => (self.own, self.over) = (None, true),
            }
            used += end + 1;
        }
        self.partial.drain(..used);
        self.at = self.at.map(|(file, _)| (file, consumed + used as u64));
        Ok(())
    }
}

impl HerdrInterruptState {
    /// This turn read from its start in `source`, `OWN_TURN_POLL_BUDGET` bytes a call, once its end
    /// is read; a partial line, a changed file or a turn the reader did not see proves nothing.
    pub(crate) fn read_own_codex_turn(
        &self,
        source: &std::path::Path,
        expected: Option<&str>,
    ) -> Option<CodexOwnTurn> {
        let start = self.turn_start.get()?;
        #[cfg(test)]
        let start = &identity_mutant(start, source);
        if source != start.source {
            return None;
        }
        let file = std::fs::File::open(source).ok()?;
        let meta = file.metadata().ok()?;
        let identity = file_identity(&meta);
        if start.file.is_some() && identity != start.file {
            return None;
        }
        let mut cursor = self.own_turn.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        if herdr_interrupt_mutant("backstop_cursor_restarts") {
            *cursor = OwnTurnCursor::default();
        }
        let (read, consumed) = *cursor.at.get_or_insert((identity, start.offset));
        if read != identity || meta.len() < consumed + cursor.partial.len() as u64 {
            // Another file, or this one cut short: the next poll reads again from the start.
            *cursor = OwnTurnCursor::default();
            return None;
        }
        if !cursor.over {
            cursor.read_on(file, consumed).ok()?;
        }
        let other = cursor.own.as_ref().zip(expected);
        if other.is_some_and(|(own, expected)| own.turn_id != expected) {
            // Not the turn the reader saw here: never cached, the next poll reads the start again.
            #[cfg(test)]
            if herdr_interrupt_mutant("backstop_mismatch_cursor_retained") {
                return None;
            }
            *cursor = OwnTurnCursor::default();
            return None;
        }
        cursor.over.then(|| cursor.own.clone()).flatten()
    }
}

/// Test-only: any turn in the read transcript, from its first record.
#[cfg(test)]
fn identity_mutant(start: &HerdrTurnStart, source: &std::path::Path) -> HerdrTurnStart {
    match herdr_interrupt_mutant("backstop_identity_skipped") {
        true => HerdrTurnStart {
            source: source.to_owned(),
            file: None,
            offset: 0,
            ..start.clone()
        },
        false => start.clone(),
    }
}

/// The Codex turn a Herdr input began, as its rollout shows it from that input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexOwnTurn {
    pub(crate) started_at: u64,
    pub(crate) turn_id: String,
    /// Read through the turn: it ended on its own abort before any other turn began.
    pub(crate) aborted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnTurnRead {
    Unreadable,
    /// No record of any turn follows the input yet.
    NotYet,
    /// Another file, or another turn's record before this turn's start.
    Foreign,
}

/// Assistant text, a tool record or reasoning: content of whichever turn is running.
fn codex_turn_content(payload: &serde_json::Value) -> bool {
    match payload["type"].as_str().unwrap_or("") {
        "message" => payload["role"].as_str() == Some("assistant"),
        "function_call" | "custom_tool_call" | "tool_search_call" | "reasoning" => true,
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => true,
        _ => false,
    }
}

/// Live Herdr turns by logical session and turn nonce, held weakly, so a synchronous reader finds
/// a held turn's own state; a nonce two live turns share names neither.
static HERDR_TURNS: std::sync::LazyLock<Mutex<HerdrTurns>> =
    std::sync::LazyLock::new(Default::default);

type HerdrTurns =
    std::collections::HashMap<(String, String), Vec<std::sync::Weak<HerdrInterruptState>>>;

/// The interrupt state of Herdr turn `turn_nonce` on `logical`, while that one turn's token lives.
pub(crate) fn herdr_turn(logical: &str, turn_nonce: &str) -> Option<Arc<HerdrInterruptState>> {
    let turns = HERDR_TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let key = (logical.to_owned(), turn_nonce.to_owned());
    let live: Vec<_> = turns
        .get(&key)?
        .iter()
        .filter_map(|turn| turn.upgrade())
        .collect();
    drop(turns);
    let [state] = <[_; 1]>::try_from(live).ok()?;
    Some(state)
}

/// Whether a live turn holds this exact index; with `herdr_turn` empty, that means ambiguous.
pub(crate) fn herdr_turn_indexed(logical: &str, nonce: &str) -> bool {
    HERDR_TURNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(logical.to_owned(), nonce.to_owned()))
        .is_some_and(|turns| turns.iter().any(|turn| turn.strong_count() > 0))
}

/// The (dev, ino) of the descriptor a reader opened; `(0, 0)` names none.
pub(crate) fn opened_file_identity(
    source: Option<&crate::services::cluster::stream_relay::SourceFileIdentity>,
) -> (u64, u64) {
    #[cfg(unix)]
    if let Some(crate::services::cluster::stream_relay::SourceFileIdentity::Unix { dev, ino }) =
        source
    {
        return (*dev, *ino);
    }
    let _ = source;
    (0, 0)
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

impl CancelToken {
    /// [`Self::try_prepare_herdr_interrupt`] for a test turn that must take its state.
    #[cfg(test)]
    pub(crate) fn prepare_herdr_interrupt(
        &self,
        provider: ProviderKind,
        owner: &crate::db::dispatched_sessions::hosted_execution::HostedOwner,
    ) -> Arc<HerdrInterruptState> {
        let state = self.try_prepare_herdr_interrupt(provider, owner);
        state.expect("a test turn takes its Herdr stop state")
    }

    /// Install observation before input; only Herdr executors use this slot. `None`, installing
    /// nothing, once a cancel landed first or when the slot holds another owner's turn.
    pub(crate) fn try_prepare_herdr_interrupt(
        &self,
        provider: ProviderKind,
        owner: &crate::db::dispatched_sessions::hosted_execution::HostedOwner,
    ) -> Option<Arc<HerdrInterruptState>> {
        let mut slot = self
            .herdr_interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(state) = slot.as_ref() {
            #[cfg(test)]
            if herdr_interrupt_mutant("prepare_reset") {
                self.claude_interrupt_claim.store(0, Ordering::Release);
            }
            return (state.owner == *owner).then(|| state.clone());
        }
        // A stop decided under this slot before it held a state cancelled a turn that wrote nothing.
        let cancelled = self.cancelled.load(Ordering::Acquire);
        #[cfg(test)]
        let cancelled = cancelled && !herdr_interrupt_mutant("prepare_ignores_cancel");
        if cancelled {
            return None;
        }
        let state = Arc::new(HerdrInterruptState {
            owner: owner.clone(),
            submission: Mutex::new(super::herdr_before_start::HerdrInputState::default()),
            user_stop: AtomicBool::new(false),
            turn_start: std::sync::OnceLock::new(),
            own_start: Mutex::new(OwnStart::Unseen(None)),
            own_turn: Mutex::new(OwnTurnCursor::default()),
        });
        self.bind_interrupt_session(provider, &owner.logical_key);
        if let Some(nonce) = self.turn_nonce() {
            let mut turns = HERDR_TURNS.lock().unwrap_or_else(|e| e.into_inner());
            turns.retain(|_, live| {
                live.retain(|turn| turn.strong_count() > 0);
                !live.is_empty()
            });
            #[cfg(test)]
            if herdr_interrupt_mutant("index_single_slot") {
                turns.retain(|(logical, _), _| *logical != owner.logical_key);
            }
            let key = (owner.logical_key.clone(), nonce.to_owned());
            turns.entry(key).or_default().push(Arc::downgrade(&state));
        }
        *slot = Some(state.clone());
        Some(state)
    }

    pub(crate) fn herdr_interrupt_state(&self) -> Option<Arc<HerdrInterruptState>> {
        self.herdr_interrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// A guarded channel stop's cancel, decided once under the token's Herdr slot and handed back as
/// is: a later change to the token never turns one outcome into another.
#[derive(Clone)]
pub(crate) enum StopCancel {
    /// The token was not the channel's turn: nothing was published.
    NotCurrent,
    /// The token was already cancelling: nothing was published.
    AlreadyStopping(Arc<CancelToken>),
    /// This stop published the cancel.
    Published(Arc<CancelToken>),
    /// The turn's Herdr state was installed first: nothing was published, the stop takes the
    /// turn's intent path.
    Herdr(Arc<CancelToken>),
}

impl StopCancel {
    /// The mailbox's cancel of its current `token`, under the slot lock a Herdr prepare takes;
    /// without settlement it is the existing guarded cancel and takes no slot.
    pub(crate) fn decide(token: Option<Arc<CancelToken>>, reason: String) -> Self {
        let Some(token) = token else {
            return Self::NotCurrent;
        };
        let settled = herdr_stop_settlement_available();
        #[cfg(test)]
        let settled = settled || herdr_interrupt_mutant("p2b_settlement_unchecked");
        let slot = settled.then(|| {
            token
                .herdr_interrupt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
        });
        if token.cancelled.load(Ordering::Relaxed) {
            return Self::AlreadyStopping(token.clone());
        }
        if slot.as_ref().is_some_and(|slot| slot.is_some()) {
            return Self::Herdr(token.clone());
        }
        #[cfg(test)]
        let slot = if herdr_interrupt_mutant("p2b_unlock_before_publish") {
            drop(slot);
            None
        } else {
            slot
        };
        #[cfg(test)]
        run_decide_hook();
        #[cfg(test)]
        let slot = if herdr_interrupt_mutant("p2b_unlock_after_hook") {
            drop(slot);
            None
        } else {
            slot
        };
        token.publish_cancel(reason);
        #[cfg(test)]
        run_publish_hook();
        drop(slot);
        Self::Published(token)
    }
}

/// Test seam between a stop's slot judgement and its publish: the decide thread's hook observes
/// the window a concurrent prepare must not enter.
#[cfg(test)]
fn run_decide_hook() {
    DECIDE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

/// Test seam between a stop's publish and its slot release: the hook observes the published
/// cancel while the slot is still held.
#[cfg(test)]
fn run_publish_hook() {
    PUBLISH_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

/// Provider-terminal settlement is not yet wired; tests exercise only delivery machinery.
pub(crate) fn herdr_stop_settlement_available() -> bool {
    #[cfg(all(test, unix))]
    {
        HERDR_SETTLEMENT_OVERRIDE.with(|value| value.get())
    }
    #[cfg(not(all(test, unix)))]
    {
        false
    }
}

#[cfg(test)]
pub(crate) fn herdr_interrupt_mutant(name: &str) -> bool {
    std::env::var("ADK_P10_3_MUTANT").ok().as_deref() == Some(name)
}

/// A requested switch cannot enable Escape before provider-terminal settlement exists.
pub(crate) fn herdr_cancel_enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = HERDR_CANCEL_OVERRIDE.with(|value| value.get()) {
        return enabled && herdr_stop_settlement_available();
    }
    let enabled = cfg!(unix)
        && crate::config_live_reload::current()
            .and_then(|config| config.runtime.herdr_cancel_enabled)
            .unwrap_or(false);
    if enabled && !herdr_stop_settlement_available() {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                event = "herdr_stop_settlement_unavailable",
                "herdr stop settlement not available; Escape disabled"
            );
        }
        return false;
    }
    enabled
}

#[cfg(test)]
thread_local! {
    pub(crate) static HERDR_CANCEL_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    #[cfg(unix)]
    pub(crate) static HERDR_SETTLEMENT_OVERRIDE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    static DECIDE_HOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
    static PUBLISH_HOOK: std::cell::RefCell<Option<Box<dyn Fn()>>> = const { std::cell::RefCell::new(None) };
}

pub(crate) struct ClaudeInterruptDeliveryGuard<'a> {
    token: &'a CancelToken,
    _session: SessionKillGuard,
}

impl ClaudeInterruptDeliveryGuard<'_> {
    pub(crate) fn commit_success<R, E>(self, outcome: Result<R, E>) -> Result<R, E> {
        if outcome.is_ok() {
            // The claim owner is the only caller that can hold this generation
            // guard. Commit with a plain store while the session lock is still
            // held, so no rollback/reclaim can interleave after provider I/O.
            self.token
                .claude_interrupt_claim
                .store(2, Ordering::Release);
            self.token.clear_claude_interrupt_submit_pending();
        }
        outcome
    }
}

pub(crate) fn submit_claude_wrapper_followup<Write>(
    token: Option<&CancelToken>,
    tmux_session_name: &str,
    write: Write,
) -> Result<(), String>
where
    Write: FnOnce() -> Result<(), String>,
{
    if let Some(token) = token {
        token.bind_claude_tmux_session(tmux_session_name);
    }
    write()?;
    if let Some(token) = token {
        token.mark_claude_interrupt_submit_pending();
    }
    Ok(())
}

pub(crate) fn observe_claude_wrapper_followup<R, Read>(
    token: Option<&CancelToken>,
    read: Read,
) -> Result<R, String>
where
    Read: FnOnce() -> Result<R, String>,
{
    let result = read();
    if let Some(token) = token {
        token.clear_claude_interrupt_submit_pending();
    }
    result
}

impl CancelToken {
    /// Publish this turn before its Claude pane can become reachable.
    ///
    /// The registry is monotonic per session: delayed recovery/rebind callers may
    /// refresh their token-local tmux name, but cannot replace a newer turn.
    pub(crate) fn bind_claude_tmux_session(&self, tmux_session_name: &str) {
        self.bind_interrupt_session(ProviderKind::Claude, tmux_session_name);
    }

    /// Publish the provider's generation before its pane receives input.
    pub(crate) fn bind_interrupt_session(&self, provider: ProviderKind, tmux_session_name: &str) {
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return;
        }
        let Some(binding) = authority::publish(
            provider,
            tmux_session_name,
            self.claude_interrupt_generation,
        ) else {
            return;
        };
        *self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(binding);
    }

    /// Acquire the session-level generation fence for provider delivery.
    ///
    /// The returned guard holds the registry lock through the caller's provider
    /// write and synchronous claim commit. A newer turn cannot publish its
    /// generation between the check and the write.
    pub(crate) fn lock_current_claude_interrupt_session(
        &self,
        tmux_session_name: &str,
    ) -> Option<ClaudeInterruptDeliveryGuard<'_>> {
        self.lock_current_interrupt_session(ProviderKind::Claude, tmux_session_name)
    }

    /// Keep the provider-specific current generation held through write and claim commit.
    pub(crate) fn lock_current_interrupt_session(
        &self,
        provider: ProviderKind,
        tmux_session_name: &str,
    ) -> Option<ClaudeInterruptDeliveryGuard<'_>> {
        let binding = self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let matches_requested_name = match binding.as_ref() {
            Some(authority::TmuxBinding::Published { key, .. }) => {
                *key == authority::SessionKey::new(provider, tmux_session_name.trim())
            }
            _ => false,
        };
        if !matches_requested_name {
            return None;
        }
        match authority::authorize(binding.as_ref()) {
            KillAuthorization::Current(session) => Some(ClaudeInterruptDeliveryGuard {
                token: self,
                _session: session,
            }),
            KillAuthorization::Unregistered | KillAuthorization::Stale { .. } => None,
        }
    }

    /// Store a tmux name without publishing a managed generation slot.
    pub(crate) fn bind_unmanaged_session_name(&self, tmux_session_name: &str) {
        let tmux_session_name = tmux_session_name.trim();
        if tmux_session_name.is_empty() {
            return;
        }
        *self
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(
            super::cancel_token_cleanup::authority::TmuxBinding::NameOnly {
                name: tmux_session_name.to_string(),
            },
        );
    }

    pub(crate) fn tmux_session_name(&self) -> Option<String> {
        self.tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|binding| binding.name().to_string())
    }

    /// Record that a wrapper accepted this turn before JSONL confirms it.
    pub(crate) fn mark_claude_interrupt_submit_pending(&self) {
        self.claude_interrupt_submit_pending
            .store(true, Ordering::Release);
    }

    pub(crate) fn claude_interrupt_submit_pending(&self) -> bool {
        self.claude_interrupt_submit_pending.load(Ordering::Acquire)
    }

    /// Clear the handoff window once delivery commits or turn observation ends.
    pub(crate) fn clear_claude_interrupt_submit_pending(&self) {
        self.claude_interrupt_submit_pending
            .store(false, Ordering::Release);
    }

    /// Reserve the Claude interrupt-delivery right for this turn.
    pub(crate) fn claim_claude_interrupt(&self) -> bool {
        self.claude_interrupt_claim
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Release an undelivered reservation so a later stop can retry this turn.
    pub(crate) fn release_claude_interrupt_claim(&self) -> bool {
        self.claude_interrupt_claim
            .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn claude_interrupt_generation(&self) -> u64 {
        self.claude_interrupt_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn wrapper_followup_publishes_then_writes_then_marks_pending() {
        let session = "AgentDesk-claude-wrapper-followup-submit-order";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        let write_observed = AtomicUsize::new(0);

        submit_claude_wrapper_followup(Some(&current), session, || {
            assert!(
                stale
                    .lock_current_claude_interrupt_session(session)
                    .is_none(),
                "current generation must publish before FIFO write"
            );
            assert!(
                !current.claude_interrupt_submit_pending(),
                "submit-pending must remain false until FIFO flush succeeds"
            );
            write_observed.store(1, Ordering::Release);
            Ok(())
        })
        .unwrap();

        assert_eq!(write_observed.load(Ordering::Acquire), 1);
        assert!(current.claude_interrupt_submit_pending());
    }

    #[test]
    fn wrapper_followup_write_failure_does_not_mark_submit_pending() {
        let token = CancelToken::new();
        assert!(
            submit_claude_wrapper_followup(
                Some(&token),
                "AgentDesk-claude-wrapper-write-failure",
                || { Err("write failed".to_string()) }
            )
            .is_err()
        );
        assert!(!token.claude_interrupt_submit_pending());
    }

    #[test]
    fn wrapper_followup_read_end_clears_submit_pending() {
        for expected_ok in [true, false] {
            let token = CancelToken::new();
            token.mark_claude_interrupt_submit_pending();

            let observed = observe_claude_wrapper_followup(Some(&token), || {
                if expected_ok {
                    Ok("completed")
                } else {
                    Err("read failed".to_string())
                }
            });

            assert_eq!(observed.is_ok(), expected_ok);
            assert!(
                !token.claude_interrupt_submit_pending(),
                "terminal and error read exits must close the submitted window"
            );
        }
    }

    #[test]
    fn session_generation_advance_blocks_stale_stop_operation() {
        let session = "AgentDesk-claude-session-generation-advance";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        assert!(stale.claim_claude_interrupt());

        current.bind_claude_tmux_session(session);
        let writes = AtomicUsize::new(0);
        let guard = stale.lock_current_claude_interrupt_session(session);
        if let Some(guard) = guard {
            let outcome = guard.commit_success((|| {
                writes.fetch_add(1, Ordering::Relaxed);
                Ok::<(), ()>(())
            })());
            assert_eq!(outcome, Ok(()));
        }

        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none()
        );
        assert_eq!(writes.load(Ordering::Relaxed), 0);
        assert!(stale.release_claude_interrupt_claim());
    }

    #[test]
    fn stale_rebind_cannot_replace_a_newer_session_generation() {
        let session = "AgentDesk-claude-session-stale-rebind";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        current.bind_claude_tmux_session(session);

        stale.bind_claude_tmux_session(session);

        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none()
        );
        assert!(
            current
                .lock_current_claude_interrupt_session(session)
                .is_some()
        );
    }

    #[test]
    fn stale_pending_stop_is_rejected_after_next_generation_publishes() {
        let session = "AgentDesk-claude-session-pending-generation-advance";
        let stale = CancelToken::new();
        let current = CancelToken::new();
        stale.bind_claude_tmux_session(session);
        stale.mark_claude_interrupt_submit_pending();

        current.bind_claude_tmux_session(session);

        assert!(stale.claude_interrupt_submit_pending());
        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none(),
            "pending state must never bypass a newer generation publication"
        );
        assert!(
            current
                .lock_current_claude_interrupt_session(session)
                .is_some()
        );
    }

    #[test]
    fn unmanaged_binding_stores_name_without_registry_authority() {
        let token = CancelToken::new();
        token.bind_unmanaged_session_name("AgentDesk-codex-name-only-binding");

        assert_eq!(
            token.tmux_session_name().as_deref(),
            Some("AgentDesk-codex-name-only-binding")
        );
        let binding = token
            .tmux_binding
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        assert!(matches!(
            authority::authorize(binding.as_ref()),
            KillAuthorization::Unregistered
        ));
    }

    #[test]
    fn submitted_window_clears_when_delivery_commits() {
        let session = "AgentDesk-claude-session-pending-commit";
        let token = CancelToken::new();
        token.bind_claude_tmux_session(session);
        token.mark_claude_interrupt_submit_pending();
        assert!(token.claim_claude_interrupt());

        token
            .lock_current_claude_interrupt_session(session)
            .unwrap()
            .commit_success(Ok::<(), ()>(()))
            .unwrap();

        assert!(!token.claude_interrupt_submit_pending());
    }

    #[test]
    fn successful_operation_commits_before_returning() {
        let session = "AgentDesk-claude-session-atomic-commit";
        let token = CancelToken::new();
        token.bind_claude_tmux_session(session);
        assert!(token.claim_claude_interrupt());

        token
            .lock_current_claude_interrupt_session(session)
            .expect("current generation must acquire delivery guard")
            .commit_success(Ok::<(), ()>(()))
            .expect("current generation must deliver");

        assert!(!token.claim_claude_interrupt());
        assert!(!token.release_claude_interrupt_claim());
    }

    fn herdr_owner(
        channel: &str,
        logical: &str,
    ) -> crate::db::dispatched_sessions::hosted_execution::HostedOwner {
        crate::db::dispatched_sessions::hosted_execution::HostedOwner {
            provider: "codex".into(),
            discord_token_hash: "hash".into(),
            channel_id: channel.into(),
            logical_key: logical.into(),
            owner_node: "node".into(),
            runtime_root: "/tmp".into(),
        }
    }

    /// A live Herdr turn stays found by its own nonce whichever other turns, of this or another
    /// channel, its session registers or drops.
    #[test]
    fn a_live_herdr_turn_is_found_by_its_own_nonce_beside_other_turns() {
        let logical = format!("AgentDesk-codex-turn-index-{}", std::process::id());
        let older = CancelToken::new();
        let a = older.prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
        let newer = CancelToken::new();
        let b = newer.prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("2", &logical));
        let (a_nonce, b_nonce) = (older.turn_nonce().unwrap(), newer.turn_nonce().unwrap());
        let found = |nonce: &str, state: &Arc<HerdrInterruptState>| {
            herdr_turn(&logical, nonce).is_some_and(|found| Arc::ptr_eq(&found, state))
        };
        assert!(
            found(a_nonce, &a),
            "a newer turn never evicts a live older one"
        );
        assert!(found(b_nonce, &b));
        assert!(herdr_turn("AgentDesk-codex-turn-index-other", a_nonce).is_none());
        let b_nonce = b_nonce.to_owned();
        drop((b, newer));
        assert!(
            herdr_turn(&logical, &b_nonce).is_none(),
            "a dropped turn is gone"
        );
        assert!(
            found(a_nonce, &a),
            "dropping the newer turn keeps the older one"
        );
        let a_nonce = a_nonce.to_owned();
        drop((a, older));
        assert!(herdr_turn(&logical, &a_nonce).is_none());
    }

    /// A nonce two live turns share names neither; the survivor is found once the other drops.
    #[test]
    fn a_nonce_two_live_herdr_turns_share_names_neither() {
        let logical = format!("AgentDesk-codex-turn-collision-{}", std::process::id());
        let nonce = format!("collision-{}", std::process::id());
        let owner = herdr_owner("1", &logical);
        let first = CancelToken::from_persisted_turn_nonce(Some(nonce.clone()));
        let a = first.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
        let second = CancelToken::from_persisted_turn_nonce(Some(nonce.clone()));
        let b = second.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
        assert!(
            herdr_turn(&logical, &nonce).is_none(),
            "a shared nonce is unknown"
        );
        drop((b, second));
        let survivor = herdr_turn(&logical, &nonce);
        assert!(survivor.is_some_and(|found| Arc::ptr_eq(&found, &a)));
    }

    /// A token's Herdr state is taken once, by its own owner: another owner's prepare gets none
    /// and changes nothing.
    #[test]
    fn a_herdr_stop_state_is_prepared_again_only_by_its_owner() {
        let logical = format!("AgentDesk-codex-prepare-owner-{}", std::process::id());
        let token = CancelToken::new();
        let state =
            token.try_prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
        let state = state.expect("a fresh token takes its state");
        let again =
            token.try_prepare_herdr_interrupt(ProviderKind::Codex, &herdr_owner("1", &logical));
        assert!(again.is_some_and(|again| Arc::ptr_eq(&again, &state)));
        let other = herdr_owner("2", "AgentDesk-codex-prepare-owner-other");
        assert!(
            token
                .try_prepare_herdr_interrupt(ProviderKind::Codex, &other)
                .is_none()
        );
        assert_eq!(token.tmux_session_name().as_deref(), Some(logical.as_str()));
    }

    /// Under the Herdr slot a stop before a prepare publishes and the prepare takes nothing; after
    /// one it leaves the token to its intent path; without settlement it is the existing cancel.
    #[cfg(unix)]
    #[test]
    fn a_stop_and_a_herdr_prepare_are_ordered_by_the_slot() {
        let owner = herdr_owner("1", "AgentDesk-codex-stop-order");
        let reason = || "mailbox_cancel_active_turn".to_string();
        let first = Arc::new(CancelToken::new());
        let decided = StopCancel::decide(Some(first.clone()), reason());
        assert!(matches!(decided, StopCancel::Published(_)));
        assert!(first.cancelled.load(Ordering::SeqCst));
        assert!(
            first
                .try_prepare_herdr_interrupt(ProviderKind::Codex, &owner)
                .is_none()
        );
        assert!(first.herdr_interrupt_state().is_none());
        assert!(
            first.tmux_session_name().is_none(),
            "a refused prepare binds nothing"
        );

        let prepared = Arc::new(CancelToken::new());
        prepared.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
        let decided = StopCancel::decide(Some(prepared.clone()), reason());
        assert!(matches!(decided, StopCancel::Herdr(_)));
        assert!(!prepared.cancelled.load(Ordering::SeqCst));
        assert!(prepared.cancel_source().is_none());

        prepared.publish_cancel("earlier");
        let decided = StopCancel::decide(Some(prepared.clone()), reason());
        assert!(matches!(decided, StopCancel::AlreadyStopping(_)));
        assert_eq!(prepared.cancel_source().as_deref(), Some("earlier"));
        assert!(matches!(
            StopCancel::decide(None, reason()),
            StopCancel::NotCurrent
        ));

        let unsettled = Arc::new(CancelToken::new());
        unsettled.prepare_herdr_interrupt(ProviderKind::Codex, &owner);
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        let decided = StopCancel::decide(Some(unsettled.clone()), reason());
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        assert!(
            matches!(decided, StopCancel::Published(_)),
            "without settlement it cancels"
        );
        assert!(unsettled.cancelled.load(Ordering::SeqCst));
    }

    /// With settlement a prepare racing into a stop's judgement-to-publish window waits on the
    /// slot, then sees the cancel and installs nothing; without settlement the stop takes no slot.
    #[cfg(unix)]
    #[test]
    fn p2b_decide_holds_slot_lock_until_publish() {
        use std::sync::mpsc;
        use std::time::Duration;

        struct HookReset;
        impl Drop for HookReset {
            fn drop(&mut self) {
                DECIDE_HOOK.with(|hook| hook.borrow_mut().take());
                HERDR_SETTLEMENT_OVERRIDE.set(true);
            }
        }
        let _reset = HookReset;
        let owner = herdr_owner("1", "AgentDesk-codex-stop-lock-window");
        let reason = || "mailbox_cancel_active_turn".to_string();

        let token = Arc::new(CancelToken::new());
        let (done_tx, done_rx) = mpsc::channel::<bool>();
        let done_rx = std::rc::Rc::new(done_rx);
        let hook_runs = Arc::new(AtomicUsize::new(0));
        {
            let token = token.clone();
            let owner = owner.clone();
            let hook_runs = hook_runs.clone();
            let done_rx = done_rx.clone();
            DECIDE_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    hook_runs.fetch_add(1, Ordering::SeqCst);
                    let (started_tx, started_rx) = mpsc::channel::<()>();
                    let racer = token.clone();
                    let owner = owner.clone();
                    let done_tx = done_tx.clone();
                    std::thread::spawn(move || {
                        started_tx.send(()).unwrap();
                        let prepared =
                            racer.try_prepare_herdr_interrupt(ProviderKind::Codex, &owner);
                        done_tx.send(prepared.is_some()).unwrap();
                    });
                    started_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("the racing prepare starts");
                    let raced = done_rx.recv_timeout(Duration::from_millis(500));
                    assert!(
                        matches!(raced, Err(mpsc::RecvTimeoutError::Timeout)),
                        "the racing prepare is not past the slot while the stop holds it: {raced:?}"
                    );
                    assert!(
                        token.herdr_interrupt.try_lock().is_err(),
                        "the stop holds the slot between its judgement and its publish"
                    );
                    assert!(!token.cancelled.load(Ordering::SeqCst));
                }));
            });
        }
        let decided = StopCancel::decide(Some(token.clone()), reason());
        assert_eq!(
            hook_runs.load(Ordering::SeqCst),
            1,
            "the window was observed"
        );
        assert!(matches!(decided, StopCancel::Published(_)));
        let installed = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the racing prepare finishes once the slot is released");
        assert!(
            !installed,
            "the prepare sees the published cancel and takes nothing"
        );
        assert!(token.cancelled.load(Ordering::SeqCst));
        assert!(token.herdr_interrupt_state().is_none());
        assert!(
            token.tmux_session_name().is_none(),
            "a refused prepare binds nothing"
        );

        HERDR_SETTLEMENT_OVERRIDE.set(false);
        let unsettled = Arc::new(CancelToken::new());
        let unsettled_runs = Arc::new(AtomicUsize::new(0));
        {
            let unsettled = unsettled.clone();
            let unsettled_runs = unsettled_runs.clone();
            DECIDE_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    unsettled_runs.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        unsettled.herdr_interrupt.try_lock().is_ok(),
                        "without settlement the stop takes no slot"
                    );
                }));
            });
        }
        let decided = StopCancel::decide(Some(unsettled.clone()), reason());
        assert_eq!(
            unsettled_runs.load(Ordering::SeqCst),
            1,
            "the window was observed"
        );
        assert!(matches!(decided, StopCancel::Published(_)));
        assert!(unsettled.cancelled.load(Ordering::SeqCst));
    }

    /// With settlement the stop still holds the slot after its publish: the published cancel and
    /// the held slot are seen together, with no racing thread.
    #[cfg(unix)]
    #[test]
    fn p2b_decide_publishes_before_releasing_the_slot() {
        struct HookReset;
        impl Drop for HookReset {
            fn drop(&mut self) {
                PUBLISH_HOOK.with(|hook| hook.borrow_mut().take());
                HERDR_SETTLEMENT_OVERRIDE.set(true);
            }
        }
        let _reset = HookReset;
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        let token = Arc::new(CancelToken::new());
        let hook_runs = Arc::new(AtomicUsize::new(0));
        {
            let token = token.clone();
            let hook_runs = hook_runs.clone();
            PUBLISH_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    hook_runs.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        token.cancelled.load(Ordering::SeqCst),
                        "the cancel is published"
                    );
                    assert!(
                        token.herdr_interrupt.try_lock().is_err(),
                        "the stop holds the slot through its publish"
                    );
                }));
            });
        }
        let decided = StopCancel::decide(
            Some(token.clone()),
            "mailbox_cancel_active_turn".to_string(),
        );
        assert_eq!(
            hook_runs.load(Ordering::SeqCst),
            1,
            "the publish was observed"
        );
        assert!(matches!(decided, StopCancel::Published(_)));
        assert!(
            token.herdr_interrupt.try_lock().is_ok(),
            "the slot is released"
        );
    }
}
