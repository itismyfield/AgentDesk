use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const PENDING_SEARCH_TTL_SECS: i64 = 6 * 60 * 60;

#[derive(Clone, Debug, Default)]
pub(crate) struct PendingMementoFeedbackTracker {
    inner: Arc<RwLock<PendingMementoFeedbackState>>,
}

#[derive(Debug, Default)]
struct PendingMementoFeedbackState {
    sessions: BTreeMap<String, PendingSessionSearches>,
    completed: BTreeMap<String, CompletedSessionSearches>,
}

#[derive(Debug, Default)]
struct PendingSessionSearches {
    ids: BTreeMap<String, PendingSearch>,
    unknown: Vec<PendingUnknownSearch>,
}

#[derive(Debug)]
struct PendingUnknownSearch {
    identity: UnknownSearchIdentity,
    pending: PendingSearch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UnknownSearchIdentity {
    ToolUseId(String),
    PayloadDigest([u8; 32]),
}

#[derive(Debug, Default)]
struct CompletedSessionSearches {
    ids: BTreeMap<String, DateTime<Utc>>,
    unknown: Vec<CompletedUnknownSearch>,
}

#[derive(Debug)]
struct CompletedUnknownSearch {
    identity: UnknownSearchIdentity,
    completed_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct CompletedSearchBatch {
    ids: Vec<String>,
    unknown: Vec<UnknownSearchIdentity>,
}

#[derive(Debug)]
struct PendingSearch {
    added_at: DateTime<Utc>,
    reminder_stage: ReminderStage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReminderStage {
    Fresh,
    InitialReminded,
    RetryReminded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReminderBoundary {
    Stop,
    UserPromptSubmit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PendingMementoFeedbackTransition {
    pub flush: Option<PendingMementoFeedbackFlush>,
    pub unsubmitted_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingMementoFeedbackFlush {
    pub search_event_ids: Vec<String>,
    pub includes_unknown_searches: bool,
    pub additional_context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MementoPostToolUseObservation {
    Ignored,
    SearchTracked { search_event_id: Option<String> },
    FeedbackCleared { search_event_id: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MementoHookToolKind {
    Search,
    ToolFeedback,
}

impl PendingMementoFeedbackTracker {
    pub(crate) fn observe_post_tool_use(
        &self,
        session_id: &str,
        payload: &Value,
    ) -> MementoPostToolUseObservation {
        self.observe_post_tool_use_at(session_id, payload, Utc::now())
    }

    pub(crate) fn observe_post_tool_use_at(
        &self,
        session_id: &str,
        payload: &Value,
        now: DateTime<Utc>,
    ) -> MementoPostToolUseObservation {
        let Some(kind) = classify_memento_hook_tool(payload) else {
            return MementoPostToolUseObservation::Ignored;
        };
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return MementoPostToolUseObservation::Ignored;
        }

        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        match kind {
            MementoHookToolKind::Search => {
                let search_event_id = extract_search_event_id(payload);
                // An expired completed tombstone must not suppress this new
                // identity lifetime. Pending replay is different: refresh its
                // TTL before pruning so reconnect delivery keeps the consumed
                // retry stage instead of recreating a Fresh obligation.
                state.prune_completed(now);
                state.track_search(session_id, search_event_id.as_deref(), payload, now);
                state.prune_pending(now);
                MementoPostToolUseObservation::SearchTracked { search_event_id }
            }
            MementoHookToolKind::ToolFeedback => {
                state.prune_expired(now);
                let search_event_id = extract_tool_feedback_search_event_id(payload);
                state.clear_feedback(session_id, search_event_id.as_deref(), now);
                MementoPostToolUseObservation::FeedbackCleared { search_event_id }
            }
        }
    }

    pub(crate) fn advance_stop_flush(
        &self,
        session_id: &str,
        payload: &Value,
    ) -> PendingMementoFeedbackTransition {
        self.advance_stop_flush_at(session_id, payload, Utc::now())
    }

    pub(crate) fn advance_stop_flush_at(
        &self,
        session_id: &str,
        payload: &Value,
        now: DateTime<Utc>,
    ) -> PendingMementoFeedbackTransition {
        // A re-entrant Stop is generated while Claude is already processing a
        // Stop hook. It must not spend the one retry, drop pending work, or
        // inject another reminder and recurse indefinitely.
        if stop_hook_active(payload) {
            return PendingMementoFeedbackTransition::default();
        }
        self.advance_at(session_id, ReminderBoundary::Stop, now)
    }

    pub(crate) fn advance_user_prompt_submit(
        &self,
        session_id: &str,
    ) -> PendingMementoFeedbackTransition {
        self.advance_at(session_id, ReminderBoundary::UserPromptSubmit, Utc::now())
    }

    fn advance_at(
        &self,
        session_id: &str,
        boundary: ReminderBoundary,
        now: DateTime<Utc>,
    ) -> PendingMementoFeedbackTransition {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return PendingMementoFeedbackTransition::default();
        }

        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        state.prune_expired(now);
        state.advance(session_id, boundary, now)
    }

    pub(crate) fn clear_session(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        state.sessions.remove(session_id);
        state.completed.remove(session_id);
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self, session_id: &str) -> usize {
        let state = self.inner.read().unwrap_or_else(|error| error.into_inner());
        state
            .sessions
            .get(session_id)
            .map(PendingSessionSearches::len)
            .unwrap_or(0)
    }
}

impl PendingMementoFeedbackState {
    fn track_search(
        &mut self,
        session_id: &str,
        search_event_id: Option<&str>,
        payload: &Value,
        now: DateTime<Utc>,
    ) {
        match search_event_id.and_then(non_empty_string) {
            Some(id) => {
                if self
                    .completed
                    .get(session_id)
                    .is_some_and(|completed| completed.ids.contains_key(id))
                {
                    return;
                }
                let session = self.sessions.entry(session_id.to_string()).or_default();
                session
                    .ids
                    .entry(id.to_string())
                    .and_modify(|pending| pending.added_at = now)
                    .or_insert(PendingSearch {
                        added_at: now,
                        reminder_stage: ReminderStage::Fresh,
                    });
            }
            None => {
                let identity = unknown_search_identity(payload);
                if self.completed.get(session_id).is_some_and(|completed| {
                    completed
                        .unknown
                        .iter()
                        .any(|search| search.identity == identity)
                }) {
                    return;
                }
                let session = self.sessions.entry(session_id.to_string()).or_default();
                if let Some(existing) = session
                    .unknown
                    .iter_mut()
                    .find(|unknown| unknown.identity == identity)
                {
                    // A reconnect can replay the same PostToolUse hook. Match
                    // known-id behavior: refresh TTL without resetting the
                    // already-consumed reminder stage.
                    existing.pending.added_at = now;
                } else {
                    session.unknown.push(PendingUnknownSearch {
                        identity,
                        pending: PendingSearch {
                            added_at: now,
                            reminder_stage: ReminderStage::Fresh,
                        },
                    });
                }
            }
        }
    }

    fn clear_feedback(
        &mut self,
        session_id: &str,
        search_event_id: Option<&str>,
        now: DateTime<Utc>,
    ) {
        let Some(mut session) = self.sessions.remove(session_id) else {
            return;
        };
        let mut completed = CompletedSearchBatch::default();
        match search_event_id.and_then(non_empty_string) {
            Some(id) => {
                if session.ids.remove(id).is_some() {
                    completed.ids.push(id.to_string());
                } else if let Some(unknown) = session.unknown.pop() {
                    completed.unknown.push(unknown.identity);
                }
            }
            None => {
                completed.ids.extend(session.ids.keys().cloned());
                completed
                    .unknown
                    .extend(session.unknown.iter().map(|search| search.identity.clone()));
                session.ids.clear();
                session.unknown.clear();
            }
        }
        if !session.is_empty() {
            self.sessions.insert(session_id.to_string(), session);
        }
        self.record_completed(session_id, completed, now);
    }

    fn prune_expired(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::seconds(PENDING_SEARCH_TTL_SECS);
        self.prune_pending_at_cutoff(cutoff);
        self.prune_completed_at_cutoff(cutoff);
    }

    fn prune_pending(&mut self, now: DateTime<Utc>) {
        self.prune_pending_at_cutoff(now - Duration::seconds(PENDING_SEARCH_TTL_SECS));
    }

    fn prune_pending_at_cutoff(&mut self, cutoff: DateTime<Utc>) {
        self.sessions.retain(|_, pending| {
            pending
                .ids
                .retain(|_, pending_search| pending_search.added_at >= cutoff);
            pending
                .unknown
                .retain(|unknown| unknown.pending.added_at >= cutoff);
            !pending.is_empty()
        });
    }

    fn prune_completed(&mut self, now: DateTime<Utc>) {
        self.prune_completed_at_cutoff(now - Duration::seconds(PENDING_SEARCH_TTL_SECS));
    }

    fn prune_completed_at_cutoff(&mut self, cutoff: DateTime<Utc>) {
        self.completed.retain(|_, completed| {
            completed
                .ids
                .retain(|_, completed_at| *completed_at >= cutoff);
            completed
                .unknown
                .retain(|search| search.completed_at >= cutoff);
            !completed.ids.is_empty() || !completed.unknown.is_empty()
        });
    }

    fn advance(
        &mut self,
        session_id: &str,
        boundary: ReminderBoundary,
        now: DateTime<Utc>,
    ) -> PendingMementoFeedbackTransition {
        let Some(mut pending) = self.sessions.remove(session_id) else {
            return PendingMementoFeedbackTransition::default();
        };
        let (transition, completed) = pending.advance(boundary);
        if !pending.is_empty() {
            self.sessions.insert(session_id.to_string(), pending);
        }
        self.record_completed(session_id, completed, now);
        transition
    }

    fn record_completed(
        &mut self,
        session_id: &str,
        batch: CompletedSearchBatch,
        now: DateTime<Utc>,
    ) {
        if batch.ids.is_empty() && batch.unknown.is_empty() {
            return;
        }
        let completed = self.completed.entry(session_id.to_string()).or_default();
        for id in batch.ids {
            completed.ids.insert(id, now);
        }
        for identity in batch.unknown {
            if let Some(existing) = completed
                .unknown
                .iter_mut()
                .find(|search| search.identity == identity)
            {
                existing.completed_at = now;
            } else {
                completed.unknown.push(CompletedUnknownSearch {
                    identity,
                    completed_at: now,
                });
            }
        }
    }
}

impl PendingSessionSearches {
    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.unknown.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len() + self.unknown.len()
    }

    fn advance(
        &mut self,
        boundary: ReminderBoundary,
    ) -> (PendingMementoFeedbackTransition, CompletedSearchBatch) {
        let mut search_event_ids = Vec::new();
        let mut includes_unknown_searches = false;
        let mut unsubmitted_count = 0usize;
        let mut completed = CompletedSearchBatch::default();

        self.ids
            .retain(|search_event_id, pending| match pending.advance(boundary) {
                PendingSearchAdvance::Preserve => true,
                PendingSearchAdvance::Remind => {
                    search_event_ids.push(search_event_id.clone());
                    true
                }
                PendingSearchAdvance::Drop => {
                    unsubmitted_count = unsubmitted_count.saturating_add(1);
                    completed.ids.push(search_event_id.clone());
                    false
                }
            });
        self.unknown
            .retain_mut(|unknown| match unknown.pending.advance(boundary) {
                PendingSearchAdvance::Preserve => true,
                PendingSearchAdvance::Remind => {
                    includes_unknown_searches = true;
                    true
                }
                PendingSearchAdvance::Drop => {
                    unsubmitted_count = unsubmitted_count.saturating_add(1);
                    completed.unknown.push(unknown.identity.clone());
                    false
                }
            });

        let flush = (!search_event_ids.is_empty() || includes_unknown_searches).then(|| {
            PendingMementoFeedbackFlush {
                additional_context: feedback_flush_instruction(
                    &search_event_ids,
                    includes_unknown_searches,
                    boundary,
                ),
                search_event_ids,
                includes_unknown_searches,
            }
        });
        (
            PendingMementoFeedbackTransition {
                flush,
                unsubmitted_count,
            },
            completed,
        )
    }
}

/// Stable identity for a search whose Memento response omitted
/// `searchEventId`.
///
/// Claude's top-level `tool_use_id` is the authoritative invocation identity:
/// replayed delivery keeps it, while two genuine invocations receive distinct
/// ids even when their query and response are identical. Defensive aliases are
/// accepted for captured/normalized hook payloads. If the provider omits that
/// identity, the complete `serde_json::Value` is deterministically serialized
/// and hashed. Two fully equivalent id-less payloads are inherently
/// indistinguishable, so that ambiguous case is conservatively treated as a
/// replay; any different input/response remains an independent obligation.
fn unknown_search_identity(payload: &Value) -> UnknownSearchIdentity {
    for key in ["tool_use_id", "toolUseId", "tool-use-id"] {
        if let Some(tool_use_id) = payload
            .get(key)
            .and_then(Value::as_str)
            .and_then(non_empty_string)
        {
            return UnknownSearchIdentity::ToolUseId(tool_use_id.to_string());
        }
    }
    UnknownSearchIdentity::PayloadDigest(Sha256::digest(payload.to_string().as_bytes()).into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingSearchAdvance {
    Preserve,
    Remind,
    Drop,
}

impl PendingSearch {
    fn advance(&mut self, boundary: ReminderBoundary) -> PendingSearchAdvance {
        match (boundary, self.reminder_stage) {
            (ReminderBoundary::Stop, ReminderStage::Fresh) => {
                self.reminder_stage = ReminderStage::InitialReminded;
                PendingSearchAdvance::Remind
            }
            (ReminderBoundary::Stop, ReminderStage::InitialReminded)
            | (ReminderBoundary::UserPromptSubmit, ReminderStage::InitialReminded) => {
                self.reminder_stage = ReminderStage::RetryReminded;
                PendingSearchAdvance::Remind
            }
            (_, ReminderStage::RetryReminded) => PendingSearchAdvance::Drop,
            (ReminderBoundary::UserPromptSubmit, ReminderStage::Fresh) => {
                PendingSearchAdvance::Preserve
            }
        }
    }
}

impl PendingMementoFeedbackFlush {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "additional_context": self.additional_context,
            "search_event_ids": self.search_event_ids,
            "includes_unknown_searches": self.includes_unknown_searches,
        })
    }
}

pub(crate) fn classify_memento_hook_tool(payload: &Value) -> Option<MementoHookToolKind> {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| payload.get("toolName").and_then(Value::as_str))?
        .trim();
    let leaf = memento_tool_leaf(tool_name)?;
    match leaf {
        "recall" | "context" => Some(MementoHookToolKind::Search),
        "tool_feedback" => Some(MementoHookToolKind::ToolFeedback),
        _ => None,
    }
}

pub(crate) fn memento_search_tool_name(payload: &Value) -> Option<String> {
    matches!(
        classify_memento_hook_tool(payload),
        Some(MementoHookToolKind::Search)
    )
    .then(|| {
        payload
            .get("tool_name")
            .and_then(Value::as_str)
            .or_else(|| payload.get("toolName").and_then(Value::as_str))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    })
}

fn memento_tool_leaf(tool_name: &str) -> Option<&str> {
    let lowered = tool_name.trim().to_ascii_lowercase();
    let stripped = lowered
        .strip_prefix("mcp__memento__")
        .or_else(|| lowered.strip_prefix("memento."))
        .or_else(|| lowered.strip_prefix("memento/"));
    if let Some(stripped) = stripped {
        return match stripped {
            "recall" => Some("recall"),
            "context" => Some("context"),
            "tool_feedback" => Some("tool_feedback"),
            _ => None,
        };
    }
    let mut saw_memento_segment = false;
    let mut last_segment = None;
    for segment in lowered.split("__") {
        saw_memento_segment |= segment == "memento";
        last_segment = Some(segment);
    }
    if saw_memento_segment {
        return match last_segment {
            Some("recall") => Some("recall"),
            Some("context") => Some("context"),
            Some("tool_feedback") => Some("tool_feedback"),
            _ => None,
        };
    }
    None
}

/// Maximum accepted `searchEventId` length. Real ids are small integers (DB
/// row ids — every fixture and the digit-only `scan_search_event_id` capture
/// agree); 64 leaves generous headroom while bounding what can be inlined
/// into the injected instruction.
const MAX_SEARCH_EVENT_ID_LEN: usize = 64;

/// How many envelope layers (`content` wrapper / stringified JSON) are
/// unwrapped before giving up. Real payloads need at most two.
const MAX_ENVELOPE_DEPTH: u8 = 4;

/// Extracts `searchEventId` from the PostToolUse payload — trusted path only.
///
/// #4330 rework: the id must come from the response envelope's **top-level**
/// `_meta.searchEventId` (mirroring the first-party client in
/// `services::memory::memento::search_event_feedback_hint`). The hook payload
/// carries the MCP result as `tool_response` in one of these server-authored
/// shapes, all of which are unwrapped:
///
/// - the envelope object itself: `{"_meta":{"searchEventId":...}, ...}`
/// - an MCP `CallToolResult` wrapper: `{"content":[<text blocks>], ...}`
/// - an array of MCP text blocks whose `text` is the stringified envelope
/// - a stringified envelope
///
/// Fragment/content **values** are never searched: recalled memory text is
/// attacker-influencable, so a `searchEventId` marker echoed inside a fragment
/// body must not be able to steer the injected instruction (contract violation
/// + prompt-injection surface). Extracted candidates are additionally
/// sanitized to short digit strings (`is_valid_search_event_id`) before use;
/// anything else yields `None` and the instruction omits the
/// `search_event_id` ask.
pub(crate) fn extract_search_event_id(payload: &Value) -> Option<String> {
    for key in ["tool_response", "toolResponse"] {
        if let Some(id) = payload
            .get(key)
            .and_then(|response| envelope_search_event_id(response, 0))
        {
            return Some(id);
        }
    }
    None
}

fn envelope_search_event_id(response: &Value, depth: u8) -> Option<String> {
    if depth > MAX_ENVELOPE_DEPTH {
        return None;
    }
    match response {
        Value::Object(map) => {
            if let Some(id) = meta_search_event_id(map) {
                return Some(id);
            }
            // MCP CallToolResult wrapper: the envelope JSON lives inside the
            // `content` text blocks. This is the only key descended into —
            // notably NOT `fragments` or other result data.
            map.get("content")
                .and_then(|content| envelope_search_event_id(content, depth + 1))
        }
        Value::Array(blocks) => blocks.iter().find_map(|block| {
            block
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|parsed| envelope_search_event_id(&parsed, depth + 1))
        }),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| envelope_search_event_id(&parsed, depth + 1)),
        _ => None,
    }
}

/// Reads `searchEventId` from an envelope object's **top-level** `_meta`
/// only, applying `is_valid_search_event_id` to the candidate value.
fn meta_search_event_id(map: &serde_json::Map<String, Value>) -> Option<String> {
    let meta = map.get("_meta").and_then(Value::as_object)?;
    for key in [
        "searchEventId",
        "search_event_id",
        "_searchEventId",
        "searchEventID",
    ] {
        if let Some(id) = meta
            .get(key)
            .and_then(string_or_integer)
            .map(|candidate| candidate.trim().to_string())
            .filter(|candidate| is_valid_search_event_id(candidate))
        {
            return Some(id);
        }
    }
    None
}

/// A trustworthy memento `searchEventId` is a short ASCII-digit string (see
/// `MAX_SEARCH_EVENT_ID_LEN`). Anything else is rejected so it can never be
/// inlined into a model-visible instruction.
fn is_valid_search_event_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= MAX_SEARCH_EVENT_ID_LEN
        && candidate.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn extract_tool_feedback_search_event_id(payload: &Value) -> Option<String> {
    for key in ["tool_input", "toolInput", "input", "arguments", "args"] {
        if let Some(container) = payload.get(key)
            && let Some(id) = search_event_id_from_value(container)
        {
            return Some(id);
        }
    }
    search_event_id_from_value(payload).or_else(|| scan_search_event_id(&payload.to_string()))
}

fn search_event_id_from_value(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in [
                "search_event_id",
                "searchEventId",
                "searchEventID",
                "_searchEventId",
                "_search_event_id",
            ] {
                if let Some(id) = map.get(key).and_then(string_or_integer) {
                    return Some(id);
                }
            }
            for nested in map.values() {
                if let Some(id) = search_event_id_from_value(nested) {
                    return Some(id);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(search_event_id_from_value),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| search_event_id_from_value(&parsed))
            .or_else(|| scan_search_event_id(text)),
        _ => None,
    }
}

fn string_or_integer(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => non_empty_string(text).map(ToString::to_string),
        Value::Number(number) => Some(number.to_string()).filter(|value| !value.is_empty()),
        _ => None,
    }
}

pub(crate) fn scan_search_event_id(serialized: &str) -> Option<String> {
    for marker in ["searchEventId", "search_event_id", "_searchEventId"] {
        if let Some(id) = scan_search_event_id_marker(serialized, marker) {
            return Some(id);
        }
    }
    None
}

fn scan_search_event_id_marker(serialized: &str, marker: &str) -> Option<String> {
    let mut haystack = serialized;
    loop {
        let rel = haystack.find(marker)?;
        let after = &haystack[rel + marker.len()..];
        let key_tail = after
            .strip_prefix("\\\"")
            .or_else(|| after.strip_prefix('"'))
            .unwrap_or(after)
            .trim_start();
        if let Some(value_part) = key_tail.strip_prefix(':') {
            let value = value_part.trim_start();
            let value = value
                .strip_prefix("\\\"")
                .or_else(|| value.strip_prefix('"'))
                .unwrap_or(value);
            let digits = value
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
        haystack = after;
    }
}

pub(crate) fn immediate_feedback_instruction(search_event_id: Option<String>) -> String {
    // #4330: the `search_event_id` ask is conditional on the tool response
    // actually carrying `_meta.searchEventId`. `recall`/`context` normally
    // return it, but the hook payload may not surface it (nested/stringified MCP
    // text), and the fixed defect injected a "submit the searchEventId shown
    // under `_meta.searchEventId`" line even when the result had none. When we
    // do have the id we inline it; otherwise the reminder only asks for the
    // required `tool_name`/`relevant`/`sufficient`, matching the prompt_builder
    // feedback contract reconciled in #4328 (required: `tool_name`, `relevant`,
    // `sufficient`; pass `search_event_id` only when `_meta.searchEventId` is
    // present — recommended). Defense in depth: the id is re-validated here so
    // only a short digit string can ever be inlined into the model-visible
    // instruction, regardless of the caller.
    let search_event_clause = match search_event_id
        .as_deref()
        .map(str::trim)
        .filter(|id| is_valid_search_event_id(id))
    {
        Some(id) => format!(
            " This result carried `_meta.searchEventId`, so also pass \
`search_event_id={id}` (recommended)."
        ),
        None => String::new(),
    };
    format!(
        "Action required: you just received a memento search result. Submit one \
`mcp__memento__tool_feedback` call immediately for THIS result with the required `tool_name` (the \
memento search tool you just called), `relevant` = whether any returned fragment was on-topic, and \
`sufficient` = whether the results were enough to proceed.{search_event_clause} If \
`mcp__memento__tool_feedback` is not in your active tools (memento tools are deferred), first load \
it with ToolSearch query `select:mcp__memento__tool_feedback`, then make the call. Do this now, \
then continue."
    )
}

fn feedback_flush_instruction(
    search_event_ids: &[String],
    includes_unknown_searches: bool,
    boundary: ReminderBoundary,
) -> String {
    let target = if search_event_ids.is_empty() {
        "the pending search_event_id values shown under `_meta.searchEventId` in the memento search results".to_string()
    } else {
        format!(
            "each pending search_event_id in [{}]",
            search_event_ids.join(", ")
        )
    };
    let unknown_clause = if includes_unknown_searches && !search_event_ids.is_empty() {
        " For any pending memento search not listed, use the `_meta.searchEventId` shown in that result."
    } else {
        ""
    };
    let (timing, continuation) = match boundary {
        ReminderBoundary::Stop => ("before ending this turn", "Do this now, then stop."),
        ReminderBoundary::UserPromptSubmit => (
            "at the start of this turn",
            "Do this now, then continue with the submitted prompt.",
        ),
    };
    format!(
        "Action required {timing}: there are memento search results without \
submitted feedback. Submit `mcp__memento__tool_feedback` for {target}, with `relevant` = whether \
any returned fragment was on-topic and `sufficient` = whether the results were enough to proceed. \
If `mcp__memento__tool_feedback` is not in your active tools (memento tools are deferred), first \
load it with ToolSearch query `select:mcp__memento__tool_feedback`, then make the feedback call.\
{unknown_clause} {continuation}"
    )
}

pub(crate) fn stop_hook_active(payload: &Value) -> bool {
    payload
        .get("stop_hook_active")
        .or_else(|| payload.get("stopHookActive"))
        .is_some_and(|value| match value {
            Value::Bool(active) => *active,
            Value::String(text) => text.trim().eq_ignore_ascii_case("true"),
            _ => false,
        })
}

fn non_empty_string(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_search_and_clears_matching_tool_feedback_id() {
        let tracker = PendingMementoFeedbackTracker::default();
        let search = json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": [{"type":"text","text":"{\"_meta\":{\"searchEventId\":\"22752\"}}"}]
        });
        let feedback = json!({
            "tool_name": "mcp__memento__tool_feedback",
            "tool_input": {"search_event_id": 22752, "relevant": true, "sufficient": true}
        });

        tracker.observe_post_tool_use_at("sess", &search, Utc::now());
        assert_eq!(tracker.pending_count("sess"), 1);
        tracker.observe_post_tool_use_at("sess", &feedback, Utc::now());

        assert_eq!(tracker.pending_count("sess"), 0);
    }

    #[test]
    fn tool_feedback_without_id_clears_session() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"1"}}
            }),
            now,
        );
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__tool_feedback",
                "tool_input": {"relevant": true, "sufficient": true}
            }),
            now,
        );

        assert_eq!(tracker.pending_count("sess"), 0);
    }

    #[test]
    fn stop_flush_retries_once_then_drops_and_lists_ids() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__context",
                "tool_response": {"_meta":{"searchEventId":"42"}}
            }),
            Utc::now(),
        );

        let first = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        let flush = first.flush.unwrap();
        assert_eq!(flush.search_event_ids, vec!["42"]);
        assert!(flush.additional_context.contains("[42]"));
        assert!(
            flush
                .additional_context
                .contains("mcp__memento__tool_feedback")
        );
        assert!(flush.additional_context.contains("then stop"));
        assert_eq!(first.unsubmitted_count, 0);
        assert_eq!(tracker.pending_count("sess"), 1);

        let retry = tracker.advance_user_prompt_submit("sess");
        let retry_flush = retry.flush.unwrap();
        assert_eq!(retry_flush.search_event_ids, vec!["42"]);
        assert!(
            retry_flush
                .additional_context
                .contains("continue with the submitted prompt")
        );
        assert!(!retry_flush.additional_context.contains("then stop"));
        assert_eq!(retry.unsubmitted_count, 0);
        assert_eq!(tracker.pending_count("sess"), 1);

        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert!(dropped.flush.is_none());
        assert_eq!(dropped.unsubmitted_count, 1);
        assert_eq!(tracker.pending_count("sess"), 0);

        let after_drop = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(after_drop, PendingMementoFeedbackTransition::default());
    }

    #[test]
    fn stop_hook_active_suppresses_without_spending_retry() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"7"}}
            }),
            Utc::now(),
        );

        let reentrant =
            tracker.advance_stop_flush_at("sess", &json!({"stop_hook_active": true}), Utc::now());
        assert_eq!(reentrant, PendingMementoFeedbackTransition::default());
        assert_eq!(tracker.pending_count("sess"), 1);

        let first = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(first.flush.unwrap().search_event_ids, vec!["7"]);
        assert_eq!(tracker.pending_count("sess"), 1);

        let retry = tracker.advance_user_prompt_submit("sess");
        assert_eq!(retry.flush.unwrap().search_event_ids, vec!["7"]);
        let reentrant_after_retry =
            tracker.advance_stop_flush_at("sess", &json!({"stop_hook_active": true}), Utc::now());
        assert_eq!(
            reentrant_after_retry,
            PendingMementoFeedbackTransition::default()
        );
        assert_eq!(tracker.pending_count("sess"), 1);

        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(dropped.unsubmitted_count, 1);
        assert!(dropped.flush.is_none());
    }

    #[test]
    fn prompt_before_initial_stop_preserves_the_fresh_reminder() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"9"}}
            }),
            Utc::now(),
        );

        assert_eq!(
            tracker.advance_user_prompt_submit("sess"),
            PendingMementoFeedbackTransition::default()
        );
        assert_eq!(tracker.pending_count("sess"), 1);
        let first = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(first.flush.unwrap().search_event_ids, vec!["9"]);
        assert_eq!(first.unsubmitted_count, 0);
    }

    #[test]
    fn next_stop_is_retry_fallback_when_no_user_prompt_arrives() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"8"}}
            }),
            Utc::now(),
        );

        assert!(
            tracker
                .advance_stop_flush_at("sess", &json!({}), Utc::now())
                .flush
                .is_some()
        );
        let retry = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(retry.flush.unwrap().search_event_ids, vec!["8"]);
        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(dropped.unsubmitted_count, 1);
        assert!(dropped.flush.is_none());
    }

    #[test]
    fn retry_and_drop_are_isolated_by_session() {
        let tracker = PendingMementoFeedbackTracker::default();
        for (session_id, search_event_id) in [("a", "10"), ("b", "20")] {
            tracker.observe_post_tool_use_at(
                session_id,
                &json!({
                    "tool_name": "mcp__memento__recall",
                    "tool_response": {"_meta":{"searchEventId":search_event_id}}
                }),
                Utc::now(),
            );
        }

        tracker.advance_stop_flush_at("a", &json!({}), Utc::now());
        tracker.advance_user_prompt_submit("a");
        let dropped_a = tracker.advance_stop_flush_at("a", &json!({}), Utc::now());
        assert_eq!(dropped_a.unsubmitted_count, 1);
        assert_eq!(tracker.pending_count("a"), 0);
        assert_eq!(tracker.pending_count("b"), 1);

        let first_b = tracker.advance_stop_flush_at("b", &json!({}), Utc::now());
        assert_eq!(first_b.flush.unwrap().search_event_ids, vec!["20"]);
        assert_eq!(first_b.unsubmitted_count, 0);
    }

    #[test]
    fn older_retry_drop_does_not_spend_a_newer_search_reminder() {
        let tracker = PendingMementoFeedbackTracker::default();
        let observe = |search_event_id: &str| {
            tracker.observe_post_tool_use_at(
                "sess",
                &json!({
                    "tool_name": "mcp__memento__recall",
                    "tool_response": {"_meta":{"searchEventId":search_event_id}}
                }),
                Utc::now(),
            );
        };

        observe("30");
        tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        observe("31");

        let retry = tracker.advance_user_prompt_submit("sess");
        assert_eq!(retry.flush.unwrap().search_event_ids, vec!["30"]);
        assert_eq!(retry.unsubmitted_count, 0);

        let mixed = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(mixed.unsubmitted_count, 1);
        assert_eq!(mixed.flush.unwrap().search_event_ids, vec!["31"]);
        assert_eq!(tracker.pending_count("sess"), 1);
    }

    #[test]
    fn duplicate_search_delivery_refreshes_ttl_without_resetting_retry_stage() {
        let tracker = PendingMementoFeedbackTracker::default();
        let observe = |now| {
            tracker.observe_post_tool_use_at(
                "sess",
                &json!({
                    "tool_name": "mcp__memento__recall",
                    "tool_response": {"_meta":{"searchEventId":"32"}}
                }),
                now,
            );
        };
        let now = Utc::now();
        let old = now - Duration::seconds(PENDING_SEARCH_TTL_SECS + 5);

        observe(old);
        tracker.advance_stop_flush_at("sess", &json!({}), old);
        observe(now);
        let retry = tracker.advance_user_prompt_submit("sess");
        assert_eq!(retry.flush.unwrap().search_event_ids, vec!["32"]);

        observe(now);
        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), now);
        assert_eq!(dropped.unsubmitted_count, 1);
        assert!(dropped.flush.is_none());
        assert_eq!(tracker.pending_count("sess"), 0);
    }

    #[test]
    fn duplicate_unknown_search_delivery_cannot_restart_the_retry_budget() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        let old = now - Duration::seconds(PENDING_SEARCH_TTL_SECS + 5);
        let search = json!({
            "tool_use_id": "toolu-unknown-replay",
            "tool_name": "mcp__memento__recall",
            "tool_input": {"query": "bounded retry"},
            "tool_response": {"fragments": []}
        });

        tracker.observe_post_tool_use_at("sess", &search, old);
        let initial = tracker.advance_stop_flush_at("sess", &json!({}), old);
        assert!(initial.flush.unwrap().includes_unknown_searches);

        // The same PostToolUse observation may be replayed after reconnect.
        // It refreshes TTL but must not append a new Fresh obligation behind
        // the already-reminded one.
        tracker.observe_post_tool_use_at("sess", &search, now);
        assert_eq!(tracker.pending_count("sess"), 1);
        let retry = tracker.advance_user_prompt_submit("sess");
        assert!(retry.flush.unwrap().includes_unknown_searches);

        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), now);
        assert!(dropped.flush.is_none());
        assert_eq!(dropped.unsubmitted_count, 1);
        assert_eq!(tracker.pending_count("sess"), 0);
        assert_eq!(
            tracker.advance_stop_flush_at("sess", &json!({}), now),
            PendingMementoFeedbackTransition::default()
        );
    }

    #[test]
    fn replay_after_terminal_drop_does_not_recreate_known_or_unknown_obligations() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        let known = json!({
            "tool_use_id": "toolu-known-drop",
            "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta":{"searchEventId":"4104"}}
        });
        let unknown = json!({
            "tool_use_id": "toolu-unknown-drop",
            "tool_name": "mcp__memento__context",
            "tool_response": {"fragments": []}
        });
        tracker.observe_post_tool_use_at("sess", &known, now);
        tracker.observe_post_tool_use_at("sess", &unknown, now);

        tracker.advance_stop_flush_at("sess", &json!({}), now);
        tracker.advance_stop_flush_at("sess", &json!({}), now);
        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), now);
        assert_eq!(dropped.unsubmitted_count, 2);
        assert_eq!(tracker.pending_count("sess"), 0);

        let replayed_at = now + Duration::seconds(1);
        tracker.observe_post_tool_use_at("sess", &known, replayed_at);
        tracker.observe_post_tool_use_at("sess", &unknown, replayed_at);
        assert_eq!(tracker.pending_count("sess"), 0);
        assert_eq!(
            tracker.advance_stop_flush_at("sess", &json!({}), replayed_at),
            PendingMementoFeedbackTransition::default(),
            "replay must not recreate a reminder or another unsubmitted count"
        );
    }

    #[test]
    fn replay_after_successful_feedback_does_not_recreate_known_or_unknown_obligations() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        let known = json!({
            "tool_use_id": "toolu-known-success",
            "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta":{"searchEventId":"4308"}}
        });
        let unknown = json!({
            "tool_use_id": "toolu-unknown-success",
            "tool_name": "mcp__memento__context",
            "tool_response": {"fragments": []}
        });
        tracker.observe_post_tool_use_at("known-session", &known, now);
        tracker.observe_post_tool_use_at("unknown-session", &unknown, now);
        tracker.observe_post_tool_use_at(
            "known-session",
            &json!({
                "tool_name": "mcp__memento__tool_feedback",
                "tool_input": {"search_event_id": 4308, "relevant": true, "sufficient": true}
            }),
            now,
        );
        tracker.observe_post_tool_use_at(
            "unknown-session",
            &json!({
                "tool_name": "mcp__memento__tool_feedback",
                "tool_input": {"relevant": true, "sufficient": true}
            }),
            now,
        );

        let replayed_at = now + Duration::seconds(1);
        tracker.observe_post_tool_use_at("known-session", &known, replayed_at);
        tracker.observe_post_tool_use_at("unknown-session", &unknown, replayed_at);
        for session_id in ["known-session", "unknown-session"] {
            assert_eq!(tracker.pending_count(session_id), 0);
            assert_eq!(
                tracker.advance_stop_flush_at(session_id, &json!({}), replayed_at),
                PendingMementoFeedbackTransition::default()
            );
        }
    }

    #[test]
    fn completed_tombstones_allow_unrelated_searches_and_expire_for_new_identity_lifetimes() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        let completed_known = json!({
            "tool_use_id": "toolu-known-completed",
            "tool_name": "mcp__memento__recall",
            "tool_response": {"_meta":{"searchEventId":"51"}}
        });
        let completed_unknown = json!({
            "tool_use_id": "toolu-unknown-completed",
            "tool_name": "mcp__memento__context",
            "tool_response": {"fragments": []}
        });
        tracker.observe_post_tool_use_at("sess", &completed_known, now);
        tracker.observe_post_tool_use_at("sess", &completed_unknown, now);
        tracker.advance_stop_flush_at("sess", &json!({}), now);
        tracker.advance_stop_flush_at("sess", &json!({}), now);
        assert_eq!(
            tracker
                .advance_stop_flush_at("sess", &json!({}), now)
                .unsubmitted_count,
            2
        );

        let unrelated_at = now + Duration::seconds(1);
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_use_id": "toolu-known-unrelated",
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"52"}}
            }),
            unrelated_at,
        );
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_use_id": "toolu-unknown-unrelated",
                "tool_name": "mcp__memento__context",
                "tool_response": {"fragments": ["new"]}
            }),
            unrelated_at,
        );
        let unrelated = tracker.advance_stop_flush_at("sess", &json!({}), unrelated_at);
        let unrelated_flush = unrelated.flush.expect("unrelated searches stay live");
        assert_eq!(unrelated_flush.search_event_ids, vec!["52"]);
        assert!(unrelated_flush.includes_unknown_searches);

        let expired_at = now + Duration::seconds(PENDING_SEARCH_TTL_SECS + 1);
        tracker.observe_post_tool_use_at("sess", &completed_known, expired_at);
        tracker.observe_post_tool_use_at("sess", &completed_unknown, expired_at);
        assert_eq!(tracker.pending_count("sess"), 4);
        let after_expiry = tracker.advance_stop_flush_at("sess", &json!({}), expired_at);
        let after_expiry_flush = after_expiry
            .flush
            .expect("expired identities may be new again");
        assert!(
            after_expiry_flush
                .search_event_ids
                .contains(&"51".to_string())
        );
        assert!(after_expiry_flush.includes_unknown_searches);
    }

    #[test]
    fn distinct_unknown_search_tool_uses_keep_independent_retry_budgets() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        for tool_use_id in ["toolu-unknown-a", "toolu-unknown-b"] {
            tracker.observe_post_tool_use_at(
                "sess",
                &json!({
                    "tool_use_id": tool_use_id,
                    "tool_name": "mcp__memento__recall",
                    "tool_input": {"query": "same query"},
                    "tool_response": {"fragments": []}
                }),
                now,
            );
        }

        assert_eq!(tracker.pending_count("sess"), 2);
        assert!(
            tracker
                .advance_stop_flush_at("sess", &json!({}), now)
                .flush
                .unwrap()
                .includes_unknown_searches
        );
        assert!(
            tracker
                .advance_user_prompt_submit("sess")
                .flush
                .unwrap()
                .includes_unknown_searches
        );
        let dropped = tracker.advance_stop_flush_at("sess", &json!({}), now);
        assert_eq!(dropped.unsubmitted_count, 2);
        assert_eq!(tracker.pending_count("sess"), 0);
    }

    #[test]
    fn equivalent_unknown_search_payload_without_tool_use_id_is_conservatively_deduped() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        let search = json!({
            "tool_name": "mcp__memento__context",
            "tool_input": {"query": "ambiguous hook payload"},
            "tool_response": {"fragments": []}
        });

        tracker.observe_post_tool_use_at("sess", &search, now);
        tracker.observe_post_tool_use_at("sess", &search, now);

        assert_eq!(tracker.pending_count("sess"), 1);
    }

    #[test]
    fn different_unknown_search_payloads_without_tool_use_id_stay_distinct() {
        let tracker = PendingMementoFeedbackTracker::default();
        let now = Utc::now();
        for query in ["first ambiguous search", "second ambiguous search"] {
            tracker.observe_post_tool_use_at(
                "sess",
                &json!({
                    "tool_name": "mcp__memento__context",
                    "tool_input": {"query": query},
                    "tool_response": {"fragments": []}
                }),
                now,
            );
        }

        assert_eq!(tracker.pending_count("sess"), 2);
    }

    #[test]
    fn ttl_prunes_old_searches() {
        let tracker = PendingMementoFeedbackTracker::default();
        let old = Utc::now() - Duration::seconds(PENDING_SEARCH_TTL_SECS + 1);
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"7"}}
            }),
            old,
        );

        let expired = tracker.advance_stop_flush_at("sess", &json!({}), Utc::now());
        assert_eq!(expired, PendingMementoFeedbackTransition::default());
    }

    #[test]
    fn immediate_feedback_instruction_inlines_id_when_present() {
        // #4330: with an extractable searchEventId, the reminder keeps the full
        // contract (tool_name/relevant/sufficient) and recommends the id inline.
        let ctx = immediate_feedback_instruction(Some("22752".to_string()));
        assert!(ctx.contains("search_event_id=22752"));
        assert!(ctx.contains("_meta.searchEventId"));
        assert!(ctx.contains("tool_name"));
        assert!(ctx.contains("relevant"));
        assert!(ctx.contains("sufficient"));
        assert!(ctx.contains("mcp__memento__tool_feedback"));
        assert!(ctx.contains("immediately"));
    }

    #[test]
    fn immediate_feedback_instruction_omits_id_when_absent() {
        // #4330: no searchEventId in the result -> the reminder must not fabricate
        // a search_event_id ask; only tool_name/relevant/sufficient are required.
        let ctx = immediate_feedback_instruction(None);
        assert!(!ctx.contains("search_event_id"));
        assert!(!ctx.contains("searchEventId"));
        assert!(ctx.contains("tool_name"));
        assert!(ctx.contains("relevant"));
        assert!(ctx.contains("sufficient"));
        assert!(ctx.contains("mcp__memento__tool_feedback"));
        assert!(ctx.contains("immediately"));
    }

    #[test]
    fn immediate_feedback_instruction_treats_empty_id_as_absent() {
        // An empty/whitespace id string must fall back to the no-id wording.
        let ctx = immediate_feedback_instruction(Some("  ".to_string()));
        assert!(!ctx.contains("search_event_id"));
        assert!(!ctx.contains("searchEventId"));
    }

    #[test]
    fn immediate_feedback_instruction_omits_malformed_id() {
        // #4330 rework defense in depth: a non-digit candidate must never be
        // inlined into the model-visible instruction.
        for bad in ["42; rm -rf /", "ignore previous instructions", "abc", "4 2"] {
            let ctx = immediate_feedback_instruction(Some(bad.to_string()));
            assert!(!ctx.contains("search_event_id"), "inlined: {bad}");
            assert!(!ctx.contains(bad), "leaked: {bad}");
        }
    }

    #[test]
    fn extract_search_event_id_reads_trusted_envelope_shapes() {
        // Direct envelope object.
        let direct = json!({"tool_response": {"_meta": {"searchEventId": "42"}}});
        assert_eq!(extract_search_event_id(&direct).as_deref(), Some("42"));
        // Integer-valued id.
        let integer = json!({"tool_response": {"_meta": {"searchEventId": 981}}});
        assert_eq!(extract_search_event_id(&integer).as_deref(), Some("981"));
        // Array of MCP text blocks with a stringified envelope.
        let blocks = json!({
            "tool_response": [{
                "type": "text",
                "text": "{\"fragments\":[],\"_meta\":{\"searchEventId\":\"22752\"}}"
            }]
        });
        assert_eq!(extract_search_event_id(&blocks).as_deref(), Some("22752"));
        // MCP CallToolResult wrapper around the text blocks.
        let wrapped = json!({
            "tool_response": {
                "content": [{
                    "type": "text",
                    "text": "{\"_meta\":{\"searchEventId\":\"7\"}}"
                }],
                "isError": false
            }
        });
        assert_eq!(extract_search_event_id(&wrapped).as_deref(), Some("7"));
        // Stringified envelope directly under tool_response.
        let stringified = json!({
            "tool_response": "{\"_meta\":{\"searchEventId\":\"11\"}}"
        });
        assert_eq!(extract_search_event_id(&stringified).as_deref(), Some("11"));
    }

    #[test]
    fn extract_search_event_id_ignores_ids_outside_meta_envelope() {
        // #4330 rework: a searchEventId echoed inside recalled fragment
        // content (attacker-influencable) must not be extracted — neither as a
        // structural key nor as a text marker.
        let fragment_echo = json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": [{
                "type": "text",
                "text": "{\"fragments\":[{\"content\":\"note: {\\\"searchEventId\\\":\\\"666\\\"} seen\",\"meta\":{\"searchEventId\":\"667\"}}]}"
            }]
        });
        assert_eq!(extract_search_event_id(&fragment_echo), None);
        // Structural id nested in result data without a top-level `_meta`.
        let nested = json!({
            "tool_response": {"data": {"searchEventId": "668"}}
        });
        assert_eq!(extract_search_event_id(&nested), None);
        // Ids at the payload top level (outside `tool_response`) are ignored.
        let top_level = json!({
            "tool_name": "mcp__memento__recall",
            "searchEventId": "669"
        });
        assert_eq!(extract_search_event_id(&top_level), None);
        // `_meta` nested below the envelope top level is not trusted either.
        let deep_meta = json!({
            "tool_response": {"fragments": [{"_meta": {"searchEventId": "670"}}]}
        });
        assert_eq!(extract_search_event_id(&deep_meta), None);
    }

    #[test]
    fn extract_search_event_id_rejects_malformed_ids() {
        for bad in [
            json!("abc"),
            json!("42abc"),
            json!("42; rm -rf /"),
            json!(""),
            json!("1".repeat(MAX_SEARCH_EVENT_ID_LEN + 1)),
        ] {
            let payload = json!({"tool_response": {"_meta": {"searchEventId": bad}}});
            assert_eq!(extract_search_event_id(&payload), None, "accepted: {bad}");
        }
        // Boundary: a max-length digit id is still accepted.
        let max_len = "9".repeat(MAX_SEARCH_EVENT_ID_LEN);
        let payload = json!({"tool_response": {"_meta": {"searchEventId": max_len}}});
        assert_eq!(
            extract_search_event_id(&payload).as_deref(),
            Some(max_len.as_str())
        );
    }

    #[test]
    fn fragment_echoed_id_is_not_tracked_as_known_search() {
        // End-to-end tracker check: a fragment-echoed id must not become a
        // tracked "known" search id (it lands as an unknown search instead).
        let tracker = PendingMementoFeedbackTracker::default();
        let observation = tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": [{
                    "type": "text",
                    "text": "{\"fragments\":[{\"content\":\"searchEventId: 666\"}]}"
                }]
            }),
            Utc::now(),
        );
        assert_eq!(
            observation,
            MementoPostToolUseObservation::SearchTracked {
                search_event_id: None
            }
        );
        let flush = tracker
            .advance_stop_flush_at("sess", &json!({}), Utc::now())
            .flush
            .unwrap();
        assert!(flush.search_event_ids.is_empty());
        assert!(flush.includes_unknown_searches);
    }

    #[test]
    fn extracts_tool_feedback_search_event_id_from_stringified_input() {
        let payload = json!({
            "tool_name": "mcp__memento__tool_feedback",
            "tool_input": "{\"search_event_id\":981,\"relevant\":true}"
        });

        assert_eq!(
            extract_tool_feedback_search_event_id(&payload).as_deref(),
            Some("981")
        );
    }
}
