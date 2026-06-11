use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

pub(crate) const PENDING_SEARCH_TTL_SECS: i64 = 6 * 60 * 60;

#[derive(Clone, Debug, Default)]
pub(crate) struct PendingMementoFeedbackTracker {
    inner: Arc<RwLock<PendingMementoFeedbackState>>,
}

#[derive(Debug, Default)]
struct PendingMementoFeedbackState {
    sessions: BTreeMap<String, PendingSessionSearches>,
}

#[derive(Debug, Default)]
struct PendingSessionSearches {
    ids: BTreeMap<String, DateTime<Utc>>,
    unknown_added_at: Vec<DateTime<Utc>>,
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
        state.prune_expired(now);
        match kind {
            MementoHookToolKind::Search => {
                let search_event_id = extract_search_event_id(payload);
                state.track_search(session_id, search_event_id.as_deref(), now);
                MementoPostToolUseObservation::SearchTracked { search_event_id }
            }
            MementoHookToolKind::ToolFeedback => {
                let search_event_id = extract_tool_feedback_search_event_id(payload);
                state.clear_feedback(session_id, search_event_id.as_deref());
                MementoPostToolUseObservation::FeedbackCleared { search_event_id }
            }
        }
    }

    pub(crate) fn take_stop_flush(
        &self,
        session_id: &str,
        payload: &Value,
    ) -> Option<PendingMementoFeedbackFlush> {
        self.take_stop_flush_at(session_id, payload, Utc::now())
    }

    pub(crate) fn take_stop_flush_at(
        &self,
        session_id: &str,
        payload: &Value,
        now: DateTime<Utc>,
    ) -> Option<PendingMementoFeedbackFlush> {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return None;
        }

        let mut state = self
            .inner
            .write()
            .unwrap_or_else(|error| error.into_inner());
        state.prune_expired(now);
        let Some(pending) = state.sessions.remove(session_id) else {
            return None;
        };
        if stop_hook_active(payload) {
            return None;
        }
        pending.into_flush()
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
        now: DateTime<Utc>,
    ) {
        let session = self.sessions.entry(session_id.to_string()).or_default();
        match search_event_id.and_then(non_empty_string) {
            Some(id) => {
                session.ids.insert(id.to_string(), now);
            }
            None => session.unknown_added_at.push(now),
        }
    }

    fn clear_feedback(&mut self, session_id: &str, search_event_id: Option<&str>) {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return;
        };
        match search_event_id.and_then(non_empty_string) {
            Some(id) => {
                if session.ids.remove(id).is_none() && !session.unknown_added_at.is_empty() {
                    session.unknown_added_at.pop();
                }
                if session.is_empty() {
                    self.sessions.remove(session_id);
                }
            }
            None => {
                self.sessions.remove(session_id);
            }
        }
    }

    fn prune_expired(&mut self, now: DateTime<Utc>) {
        let cutoff = now - Duration::seconds(PENDING_SEARCH_TTL_SECS);
        self.sessions.retain(|_, pending| {
            pending.ids.retain(|_, added_at| *added_at >= cutoff);
            pending
                .unknown_added_at
                .retain(|added_at| *added_at >= cutoff);
            !pending.is_empty()
        });
    }
}

impl PendingSessionSearches {
    fn is_empty(&self) -> bool {
        self.ids.is_empty() && self.unknown_added_at.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len() + self.unknown_added_at.len()
    }

    fn into_flush(self) -> Option<PendingMementoFeedbackFlush> {
        if self.is_empty() {
            return None;
        }
        let search_event_ids = self.ids.into_keys().collect::<Vec<_>>();
        let includes_unknown_searches = !self.unknown_added_at.is_empty();
        Some(PendingMementoFeedbackFlush {
            additional_context: stop_feedback_flush_instruction(
                &search_event_ids,
                includes_unknown_searches,
            ),
            search_event_ids,
            includes_unknown_searches,
        })
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

pub(crate) fn extract_search_event_id(payload: &Value) -> Option<String> {
    for hay in [payload.get("tool_response"), Some(payload)]
        .into_iter()
        .flatten()
    {
        if let Some(id) = search_event_id_from_value(hay) {
            return Some(id);
        }
        if let Some(id) = scan_search_event_id(&hay.to_string()) {
            return Some(id);
        }
    }
    None
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
    let target = match search_event_id {
        Some(id) => format!("search_event_id={id}"),
        None => "the search_event_id shown under `_meta.searchEventId` in that result".to_string(),
    };
    format!(
        "Action required: you just received a memento search result. Submit one \
`mcp__memento__tool_feedback` call immediately for THIS result with \
{target}, `relevant` = whether any returned fragment was on-topic, and `sufficient` = whether the \
results were enough to proceed. If `mcp__memento__tool_feedback` is not in your active tools \
(memento tools are deferred), first load it with ToolSearch query \
`select:mcp__memento__tool_feedback`, then make the call. Do this now, then continue."
    )
}

fn stop_feedback_flush_instruction(
    search_event_ids: &[String],
    includes_unknown_searches: bool,
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
    format!(
        "Action required before ending this turn: there are memento search results without \
submitted feedback. Submit `mcp__memento__tool_feedback` for {target}, with `relevant` = whether \
any returned fragment was on-topic and `sufficient` = whether the results were enough to proceed. \
If `mcp__memento__tool_feedback` is not in your active tools (memento tools are deferred), first \
load it with ToolSearch query `select:mcp__memento__tool_feedback`, then make the feedback call.\
{unknown_clause} Do this now, then stop."
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
    fn stop_flush_is_one_shot_and_lists_ids() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__context",
                "tool_response": {"_meta":{"searchEventId":"42"}}
            }),
            Utc::now(),
        );

        let flush = tracker
            .take_stop_flush_at("sess", &json!({}), Utc::now())
            .unwrap();
        assert_eq!(flush.search_event_ids, vec!["42"]);
        assert!(flush.additional_context.contains("[42]"));
        assert!(
            flush
                .additional_context
                .contains("mcp__memento__tool_feedback")
        );
        assert!(
            tracker
                .take_stop_flush_at("sess", &json!({}), Utc::now())
                .is_none()
        );
    }

    #[test]
    fn stop_hook_active_suppresses_and_clears_flush() {
        let tracker = PendingMementoFeedbackTracker::default();
        tracker.observe_post_tool_use_at(
            "sess",
            &json!({
                "tool_name": "mcp__memento__recall",
                "tool_response": {"_meta":{"searchEventId":"7"}}
            }),
            Utc::now(),
        );

        assert!(
            tracker
                .take_stop_flush_at("sess", &json!({"stop_hook_active": true}), Utc::now())
                .is_none()
        );
        assert_eq!(tracker.pending_count("sess"), 0);
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

        assert!(
            tracker
                .take_stop_flush_at("sess", &json!({}), Utc::now())
                .is_none()
        );
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
