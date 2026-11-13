//! Current-source Idle readiness; external Busy injection keeps its own entry path.
mod clear;
pub(crate) mod compact;
#[cfg(unix)]
pub(crate) use source::adopt_awaiting_clear;
mod dialog;
mod evidence;
pub(crate) mod idle;
mod source;
#[cfg(test)]
#[path = "readiness_tests.rs"]
pub(crate) mod tests;
mod wait;

use super::hook_server::HookEventKind;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use chrono::{DateTime, Utc};
use evidence::{End, Stamp};
use serde_json::Value;
use source::Source;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};

#[derive(Default)]
struct Entry {
    revision: u64,
    start: bool,
    clear_start: bool,
    pending: Option<(u64, u64)>,
    submitted: bool,
    compact_ticket: Option<u64>,
    compact_done: Option<compact::completion::Done>,
    recovery: Option<(u64, DateTime<Utc>)>,
    file: Option<(u64, u64)>,
    high_water: u64,
}
type FileKey = (PathBuf, (u64, u64), Option<String>);
#[derive(Default)]
struct Ledger {
    entries: HashMap<Source, Entry>,
    continuations: HashMap<FileKey, (u64, DateTime<Utc>)>,
    final_stops: HashMap<FileKey, u64>,
    revision: u64,
    starts: HashMap<(String, String), (DateTime<Utc>, Source)>,
    unread_continuations: HashSet<(PathBuf, Option<String>)>,
    published: HashMap<FileKey, DateTime<Utc>>,
}
static STARTED: LazyLock<DateTime<Utc>> = LazyLock::new(Utc::now);
pub(crate) fn initialize() {
    LazyLock::force(&STARTED);
}
static LEDGER: LazyLock<Mutex<Ledger>> = LazyLock::new(|| Mutex::new(Ledger::default()));
static CHANGED: Condvar = Condvar::new();
fn ledger() -> MutexGuard<'static, Ledger> {
    LEDGER.lock().unwrap_or_else(|e| e.into_inner())
}
fn changed(state: &mut Ledger) {
    state.revision += 1;
    CHANGED.notify_all();
}

pub(crate) struct Proof {
    source: Source,
    stamp: Option<Stamp>,
    revision: u64,
    clear: Option<clear::Evidence>,
}
pub(crate) fn observe(session: &str, path: Option<&Path>) -> Option<Proof> {
    let source = Source::current(session, path)?;
    let clear = clear::current(&source)?;
    let mut tail = evidence::read(&source.path);
    if tail.as_ref().is_some_and(|tail| {
        tail.native
            .as_deref()
            .is_some_and(|native| native != source.native)
    }) {
        return None;
    }
    let native_idle = tail.is_none()
        || crate::services::claude_tui::transcript_tail::observe_transcript_turn_state(
            &source.path,
        ) == crate::services::tui_turn_state::TuiTurnState::Idle;
    let mut state = ledger();
    // Keep ordinary Running observation side-effect-free; only our completed control can differ.
    if !native_idle
        && state
            .entries
            .get(&source)
            .is_none_or(|entry| entry.compact_done.is_none())
    {
        return None;
    }
    if state
        .unread_continuations
        .contains(&(source.path.clone(), source.nonce.clone()))
        || state
            .unread_continuations
            .contains(&(source.path.clone(), None))
    {
        return None;
    }
    let file = tail
        .as_ref()
        .map(|tail| (source.path.clone(), tail.stamp.file, source.nonce.clone()));
    let blocked = file.as_ref().and_then(|key| {
        state
            .continuations
            .get(key)
            .into_iter()
            .chain(state.continuations.get(&(key.0.clone(), key.1, None)))
            .max()
            .copied()
    });
    let final_stop = file
        .as_ref()
        .and_then(|key| state.final_stops.get(key))
        .copied();
    let entry = state
        .entries
        .entry(source.clone())
        .or_insert_with(|| Entry {
            recovery: Some((tail.as_ref().map_or(0, |tail| tail.stamp.len), *STARTED)),
            ..Entry::default()
        });
    if let Some(tail) = &tail {
        if entry.file.is_some_and(|file| file != tail.stamp.file)
            || tail.stamp.len < entry.high_water
        {
            entry.recovery = Some((tail.stamp.len, Utc::now()));
            entry.start = false;
            entry.compact_done = None;
            entry.revision += 1;
        }
        entry.file = Some(tail.stamp.file);
        entry.high_water = tail.stamp.len;
    }
    if let Some(tail) = &mut tail {
        let floor = entry
            .pending
            .map(|(floor, _)| floor)
            .into_iter()
            .chain(entry.recovery.map(|(floor, _)| floor))
            .chain(blocked.map(|(floor, _)| floor))
            .max();
        if let Some(floor) = floor
            && tail.activity <= floor
            && tail.terminal > floor
            && let Some(activity) = evidence::activity_after(
                &source.path,
                floor,
                tail.terminal,
                &source.native,
                blocked.map(|(_, at)| at),
            )
        {
            (tail.activity_start, tail.activity, tail.activity_at) = activity;
        }
        if Stamp::read(&source.path).as_ref() != Some(&tail.stamp) {
            return None;
        }
    }
    // Local compact completion only replaces that control's fence, never a continuation.
    let compact_ready = tail.is_some()
        && entry.compact_ticket.is_none()
        && entry.pending.is_none()
        && blocked.is_none()
        && entry.compact_done.as_ref().is_some_and(|done| {
            entry
                .recovery
                .is_none_or(|recovery| Some(recovery) == done.recovery)
                && compact::completion::read(&source.path, &done.floor, &source.native, true)
                    .is_some()
        });
    let ready = compact_ready
        || (native_idle
            && entry.compact_ticket.is_none()
            && match &tail {
                Some(tail) => {
                    let recovered = entry.recovery.is_none_or(|(floor, _)| {
                        tail.activity_start >= floor
                            && tail.activity > floor
                            && tail.terminal >= tail.activity
                    });
                    let after_pending = entry.pending.is_none_or(|(floor, _)| {
                        tail.activity_start >= floor
                            && tail.activity > floor
                            && tail.terminal >= tail.activity
                    });
                    // Finishing an old partial row is not activity from the requested continuation.
                    let after_continuation = blocked.is_none_or(|(floor, at)| {
                        tail.activity_start >= floor
                            && tail.activity > floor
                            && tail.terminal >= tail.activity
                            && tail.activity_at.is_some_and(|seen| seen > at)
                    });
                    let terminal = tail.end == End::Hard
                        || (tail.end == End::Summary
                            && final_stop.is_some_and(|floor| tail.activity <= floor));
                    (terminal && recovered && after_pending && after_continuation)
                        || (entry.start
                            && !entry.submitted
                            && entry.pending.is_none()
                            && tail.empty
                            && blocked.is_none())
                }
                None => {
                    entry.start
                        && !entry.submitted
                        && entry.pending.is_none()
                        && entry.file.is_none()
                        && !source.path.exists()
                }
            });
    if !ready {
        return None;
    }
    if entry.pending.take().is_some() {
        entry.revision += 1;
    }
    let proof = Proof {
        source,
        stamp: tail.map(|t| t.stamp),
        revision: entry.revision,
        clear,
    };
    drop(state);
    (Source::current(session, path).as_ref() == Some(&proof.source)
        && clear::current(&proof.source) == Some(proof.clear.clone()))
    .then_some(proof)
}
pub(crate) fn reserve(proof: Proof) -> Option<Reservation> {
    let current = observe(&proof.source.session, Some(&proof.source.path))?;
    if current.source != proof.source
        || current.stamp != proof.stamp
        || current.revision != proof.revision
        || current.clear != proof.clear
    {
        return None;
    }
    let mut state = ledger();
    let entry = state.entries.get_mut(&proof.source)?;
    if entry.revision != proof.revision || entry.pending.is_some() {
        return None;
    }
    entry.revision += 1;
    entry.pending = Some((proof.stamp.as_ref().map_or(0, |s| s.len), entry.revision));
    let revision = entry.revision;
    let recovery = entry.recovery;
    changed(&mut state);
    let reservation = Reservation {
        source: proof.source,
        stamp: proof.stamp,
        revision,
        recovery,
        clear: proof.clear,
    };
    drop(state);
    #[cfg(all(test, unix))]
    tests::clear_tests::after_reserve();
    Some(reservation)
}
pub(crate) struct Reservation {
    source: Source,
    stamp: Option<Stamp>,
    revision: u64,
    recovery: Option<(u64, DateTime<Utc>)>,
    clear: Option<clear::Evidence>,
}
impl Reservation {
    pub(crate) fn current(&self) -> bool {
        Source::current(&self.source.session, Some(&self.source.path)).as_ref()
            == Some(&self.source)
            && clear::current(&self.source) == Some(self.clear.clone())
            && Stamp::read(&self.source.path) == self.stamp
            && ledger()
                .entries
                .get(&self.source)
                .is_some_and(|entry| entry.revision == self.revision)
    }

    /// Only a known pre-key refusal releases a local reservation; uncertainty stays fenced.
    pub(crate) fn not_sent(self) {
        let mut state = ledger();
        if let Some(entry) = state.entries.get_mut(&self.source)
            && entry
                .pending
                .is_some_and(|(_, token)| token == self.revision)
        {
            entry.pending = None;
            entry.revision += 1;
            changed(&mut state);
        }
    }
    pub(crate) fn sent(self) {
        let mut state = ledger();
        if let Some(entry) = state.entries.get_mut(&self.source) {
            entry.submitted = true;
        }
    }
}
pub(crate) use wait::prepare;

/// Both root output blocking and feedback continuation use this same fence before replying.
pub(crate) fn note_hook(
    provider: &str,
    kind: &HookEventKind,
    routed: &str,
    payload: &Value,
    context: Option<&BindingContext>,
    published: Option<DateTime<Utc>>,
    continuing: bool,
) {
    if provider != "claude"
        || !matches!(
            kind,
            HookEventKind::Stop | HookEventKind::SessionStart | HookEventKind::UserPromptSubmit
        )
    {
        return;
    }
    let _compact_wake = compact::HookWake;
    let Some(path) = payload["transcript_path"].as_str().map(PathBuf::from) else {
        return;
    };
    let native = payload["session_id"].as_str().unwrap_or(routed);
    let supplied_context = context.is_some();
    let context = context.filter(|ctx| {
        ctx.provider == "claude"
            && if *kind == HookEventKind::SessionStart || continuing {
                crate::services::tui_prompt_dedupe::binding_context::execution_context(
                    "claude",
                    &ctx.execution_nonce,
                )
                .is_ok_and(|canonical| canonical == **ctx)
            } else {
                source::nonce(&ctx.tmux_session) == Some(Some(ctx.execution_nonce.clone()))
            }
    });
    if supplied_context && context.is_none() {
        return;
    }
    if *kind != HookEventKind::SessionStart {
        if let Some(ctx) = context
            && let Some(current) = Source::current(&ctx.tmux_session, None)
            && (current.native != native || current.path != path)
        {
            return;
        }
        if let Some(tail) = evidence::read(&path) {
            if published.is_some_and(|at| tail.activity_at.is_some_and(|seen| seen > at)) {
                return;
            }
        }
    }
    #[cfg(unix)]
    if *kind == HookEventKind::SessionStart && payload["source"] == "clear" {
        source::note_clear(context, published, native, &path);
    }
    let mut state = ledger();
    if *kind == HookEventKind::SessionStart {
        if let (Some(ctx), Some(at)) = (context, published)
            && at >= *STARTED
            && at >= ctx.created_at
            && matches!(
                payload["source"].as_str(),
                Some("startup" | "resume" | "clear")
            )
            && (matches!(payload["source"].as_str(), Some("clear" | "resume"))
                || ctx
                    .expected_native_session_id
                    .as_deref()
                    .is_none_or(|expected| expected == native))
            && ctx.provider_root.as_deref().is_some_and(|root| {
                super::source_verify::is_top_level_transcript(root, &path, native)
            })
        {
            let start_key = (ctx.tmux_session.clone(), ctx.execution_nonce.clone());
            if state
                .starts
                .get(&start_key)
                .is_some_and(|(seen, _)| *seen >= at)
            {
                return;
            }
            let source = Source {
                session: ctx.tmux_session.clone(),
                nonce: Some(ctx.execution_nonce.clone()),
                native: native.into(),
                path,
            };
            state.starts.insert(start_key, (at, source.clone()));
            state.entries.retain(|old, _| {
                old.session != source.session || old.nonce != source.nonce || old == &source
            });
            let entry = state.entries.entry(source).or_default();
            entry.start = true;
            entry.clear_start = payload["source"] == "clear";
            entry.compact_done = None;
            entry.recovery = None;
            entry.revision += 1;
            changed(&mut state);
        }
        return;
    }
    if let Some(ctx) = context {
        state
            .entries
            .entry(Source {
                session: ctx.tmux_session.clone(),
                nonce: Some(ctx.execution_nonce.clone()),
                native: native.into(),
                path: path.clone(),
            })
            .or_insert_with(|| Entry {
                recovery: Some((Stamp::read(&path).map_or(0, |stamp| stamp.len), *STARTED)),
                ..Entry::default()
            });
    }
    let nonce = context.map(|ctx| ctx.execution_nonce.clone()).or_else(|| {
        let mut sources = state.entries.keys().filter(|source| {
            source.path == path && source::nonce(&source.session) == Some(source.nonce.clone())
        });
        let first = sources.next()?.nonce.clone();
        sources.next().is_none().then_some(first).flatten()
    });
    let Some(stamp) = Stamp::read(&path) else {
        if continuing {
            state.unread_continuations.insert((path.clone(), nonce));
            for (source, entry) in &mut state.entries {
                if source.path == path
                    && context.is_none_or(|ctx| {
                        source.nonce.as_deref() == Some(ctx.execution_nonce.as_str())
                    })
                {
                    entry.revision += 1;
                }
            }
            changed(&mut state);
        }
        return;
    };
    let key = (path.clone(), stamp.file, nonce.clone());
    if let Some(at) = published {
        if state.published.get(&key).is_some_and(|seen| *seen >= at) {
            return;
        }
        state.published.insert(key.clone(), at);
    }
    if *kind == HookEventKind::UserPromptSubmit && context.is_some() {
        for (source, entry) in &mut state.entries {
            if source.path == path
                && context
                    .is_none_or(|ctx| source.nonce.as_deref() == Some(ctx.execution_nonce.as_str()))
            {
                // A real new submit supersedes local-control ownership, retaining its own fence.
                entry.compact_ticket = None;
                entry.compact_done = None;
                entry.pending = Some((stamp.len, entry.revision + 1));
            }
        }
    }
    if continuing {
        for (source, entry) in &mut state.entries {
            if source.path == path
                && context
                    .is_none_or(|ctx| source.nonce.as_deref() == Some(ctx.execution_nonce.as_str()))
            {
                entry.start = false;
            }
        }
        state
            .continuations
            .insert(key.clone(), (stamp.len, published.unwrap_or_else(Utc::now)));
        state.final_stops.remove(&key);
    } else if let Some(tail) = evidence::read(&path)
        && *kind == HookEventKind::Stop
        && !super::memento_feedback::stop_hook_active(payload)
        && context.is_some()
        && published.is_some_and(|at| tail.activity_at.is_some_and(|seen| seen <= at))
        && payload["last_assistant_message"]
            .as_str()
            .is_some_and(|text| tail.assistant.as_deref() == Some(text))
    {
        if state.continuations.get(&key).is_none_or(|(floor, at)| {
            tail.activity_start >= *floor
                && tail.activity > *floor
                && tail.activity_at.is_some_and(|seen| seen > *at)
        }) {
            state.unread_continuations.remove(&(path.clone(), nonce));
            state.continuations.remove(&key);
            state.final_stops.insert(key, tail.stamp.len);
            for (source, entry) in &mut state.entries {
                if source.path == path
                    && entry
                        .recovery
                        .is_some_and(|(_, restored)| published.is_some_and(|at| at >= restored))
                {
                    entry.recovery = None;
                }
            }
        }
    }
    // Invalidate every outstanding proof of this exact file before returning a continuation.
    for (source, entry) in &mut state.entries {
        if source.path == path
            && context
                .is_none_or(|ctx| source.nonce.as_deref() == Some(ctx.execution_nonce.as_str()))
        {
            entry.revision += 1;
        }
    }
    changed(&mut state);
}

/// A complete empty input box is draft evidence even when the status footer is clipped.
pub(crate) fn empty_draft(capture: &str) -> bool {
    matches!(
        super::busy_inject::screen::input_box(capture),
        super::busy_inject::screen::Composer::Empty
    )
}
pub(crate) fn watch_draft(session: &str, path: Option<&Path>) {
    watch(session, path, true);
}
// Current source evidence survives a UI modal; retry only after its focus is released.
pub(crate) fn watch_modal(session: &str, path: Option<&Path>) {
    #[cfg(unix)]
    {
        let session = session.to_owned();
        let path = path.map(Path::to_path_buf);
        super::owner_input::record_preparation_watch(Box::new(move || {
            if observe(&session, path.as_deref()).is_some()
                && super::host_input::observe_draft(&session)
                    .as_deref()
                    .is_some_and(|capture| !idle::focus_modal(capture))
            {
                super::busy_inject::Screen::Ready
            } else {
                super::busy_inject::Screen::Unknown
            }
        }));
    }
    #[cfg(not(unix))]
    let _ = (session, path);
}
pub(crate) fn watch_current(session: &str, path: Option<&Path>) {
    watch(session, path, false);
}
fn watch(session: &str, path: Option<&Path>, draft_required: bool) {
    #[cfg(unix)]
    {
        let session = session.to_owned();
        let path = path.map(Path::to_path_buf);
        super::owner_input::record_preparation_watch(Box::new(move || {
            if observe(&session,path.as_deref()).is_some()
                && (!draft_required || super::host_input::observe_draft(&session).as_deref()
                    .is_some_and(|capture| empty_draft(capture)
                        && !crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(capture))) {
                super::busy_inject::Screen::Ready
            } else { super::busy_inject::Screen::Unknown }
        }));
    }
    #[cfg(not(unix))]
    let _ = (session, path, draft_required);
}

#[cfg(all(test, unix))]
pub(crate) use source::CLEAR_AFTER_PUBLICATION;

#[cfg(unix)]
pub(crate) use source::clear_wakeable;
