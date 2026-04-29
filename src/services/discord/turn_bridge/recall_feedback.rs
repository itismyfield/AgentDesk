use std::collections::VecDeque;
use std::time::Duration;

use serde_json::Value;

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::services::discord::settings::ResolvedMemorySettings;
use crate::services::memory::{MementoBackend, MementoToolFeedbackRequest, TokenUsage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingRecallFeedback {
    pub session_id: Option<String>,
    pub search_event_id: Option<String>,
    pub fragment_ids: Vec<String>,
    pub relevant: bool,
    pub sufficient: bool,
}

impl PendingRecallFeedback {
    fn into_request(self, session_id_fallback: Option<&str>) -> MementoToolFeedbackRequest {
        MementoToolFeedbackRequest {
            tool_name: "recall".to_string(),
            relevant: self.relevant,
            sufficient: self.sufficient,
            session_id: self
                .session_id
                .or_else(|| normalized_opt(session_id_fallback)),
            search_event_id: self.search_event_id,
            fragment_ids: self.fragment_ids,
            suggestion: None,
            context: None,
            trigger_type: Some("automatic".to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RecallFeedbackTurnAnalysis {
    pub recall_count: usize,
    pub manual_feedback_count: usize,
    pub manual_covered_recall_count: usize,
    pub pending_feedbacks: Vec<PendingRecallFeedback>,
}

impl RecallFeedbackTurnAnalysis {
    pub(super) fn covered_recall_count_after(&self, auto_feedback_count: usize) -> usize {
        self.manual_covered_recall_count
            .saturating_add(auto_feedback_count)
            .min(self.recall_count)
    }

    pub(super) fn needs_voluntary_feedback_reminder(&self) -> bool {
        self.recall_count > 0 && !self.pending_feedbacks.is_empty()
    }
}

pub(super) fn build_voluntary_feedback_reminder(
    analysis: &RecallFeedbackTurnAnalysis,
) -> Option<String> {
    if !analysis.needs_voluntary_feedback_reminder() {
        return None;
    }

    let search_event_ids = analysis
        .pending_feedbacks
        .iter()
        .filter_map(|feedback| feedback.search_event_id.as_deref())
        .collect::<Vec<_>>();
    let id_list = if search_event_ids.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", search_event_ids.join(", "))
    };

    Some(format!(
        "이번 턴 recall {}건 중 tool_feedback {}/{}. 다음 search_event_ids에 대해 tool_feedback(search_event_id, relevant, sufficient)을 평가 후 턴 종료: {}",
        analysis.recall_count, analysis.manual_covered_recall_count, analysis.recall_count, id_list
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RecallFeedbackAutoSubmitResult {
    pub submitted_count: usize,
    pub token_usage: TokenUsage,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    name: String,
    input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MementoToolKind {
    Recall,
    ToolFeedback,
}

#[derive(Debug, Clone)]
struct CompletedMementoToolCall {
    kind: MementoToolKind,
    input: String,
    output: String,
    completed_at: usize,
}

#[derive(Debug, Clone)]
struct RecallObservation {
    completed_at: usize,
    session_id: Option<String>,
    search_event_id: Option<String>,
    fragment_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct ToolFeedbackObservation {
    completed_at: usize,
    search_event_id: Option<String>,
    fragment_ids: Vec<String>,
}

pub(super) fn analyze_recall_feedback_turn(
    events: &[SessionTranscriptEvent],
) -> RecallFeedbackTurnAnalysis {
    let completed_calls = completed_memento_tool_calls(events);
    let recalls = completed_calls
        .iter()
        .filter_map(|call| match call.kind {
            MementoToolKind::Recall => Some(parse_recall_observation(call)),
            MementoToolKind::ToolFeedback => None,
        })
        .collect::<Vec<_>>();
    if recalls.is_empty() {
        return RecallFeedbackTurnAnalysis::default();
    }

    let feedbacks = completed_calls
        .iter()
        .filter_map(|call| match call.kind {
            MementoToolKind::ToolFeedback => parse_tool_feedback_observation(call),
            MementoToolKind::Recall => None,
        })
        .collect::<Vec<_>>();

    let mut matched_recalls = vec![false; recalls.len()];
    for feedback in &feedbacks {
        if let Some(index) = find_matching_recall(&recalls, &matched_recalls, feedback) {
            matched_recalls[index] = true;
        }
    }

    let pending_feedbacks = recalls
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_recalls[*index])
        .map(|(index, recall)| PendingRecallFeedback {
            session_id: recall.session_id.clone(),
            search_event_id: recall.search_event_id.clone(),
            fragment_ids: recall.fragment_ids.clone(),
            relevant: recall_is_relevant(events, recall),
            sufficient: index + 1 == recalls.len(),
        })
        .collect::<Vec<_>>();

    RecallFeedbackTurnAnalysis {
        recall_count: recalls.len(),
        manual_feedback_count: feedbacks.len(),
        manual_covered_recall_count: matched_recalls
            .into_iter()
            .filter(|matched| *matched)
            .count(),
        pending_feedbacks,
    }
}

pub(super) async fn submit_pending_feedbacks(
    settings: &ResolvedMemorySettings,
    session_id: Option<&str>,
    pending_feedbacks: Vec<PendingRecallFeedback>,
) -> RecallFeedbackAutoSubmitResult {
    if pending_feedbacks.is_empty() {
        return RecallFeedbackAutoSubmitResult::default();
    }

    let backend = MementoBackend::new(settings.clone());
    let mut result = RecallFeedbackAutoSubmitResult::default();

    for pending_feedback in pending_feedbacks {
        match tokio::time::timeout(
            Duration::from_millis(settings.capture_timeout_ms),
            backend.tool_feedback(pending_feedback.into_request(session_id)),
        )
        .await
        {
            Ok(Ok(token_usage)) => {
                result.submitted_count += 1;
                result.token_usage.saturating_add_assign(token_usage);
            }
            Ok(Err(error)) => result.errors.push(error),
            Err(_) => result.errors.push(format!(
                "memento tool_feedback timed out after {}ms",
                settings.capture_timeout_ms
            )),
        }
    }

    result
}

pub(super) fn transcript_contains_explicit_memento_tool_call(
    events: &[SessionTranscriptEvent],
) -> bool {
    events.iter().any(|event| {
        matches!(
            event.kind,
            SessionTranscriptEventKind::ToolUse
                | SessionTranscriptEventKind::ToolResult
                | SessionTranscriptEventKind::Error
        ) && event
            .tool_name
            .as_deref()
            .and_then(explicit_memento_tool_kind)
            .is_some()
    })
}

fn completed_memento_tool_calls(
    events: &[SessionTranscriptEvent],
) -> Vec<CompletedMementoToolCall> {
    let mut pending = VecDeque::<PendingToolCall>::new();
    let mut completed = Vec::new();

    for (index, event) in events.iter().enumerate() {
        match event.kind {
            SessionTranscriptEventKind::ToolUse => {
                pending.push_back(PendingToolCall {
                    name: event.tool_name.clone().unwrap_or_default(),
                    input: event.content.clone(),
                });
            }
            SessionTranscriptEventKind::ToolResult | SessionTranscriptEventKind::Error => {
                let Some(pending_call) = pending.pop_front() else {
                    continue;
                };
                if event.is_error || event.kind == SessionTranscriptEventKind::Error {
                    continue;
                }
                let tool_name = event
                    .tool_name
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&pending_call.name);
                let Some(kind) = canonical_memento_tool_kind(tool_name) else {
                    continue;
                };
                completed.push(CompletedMementoToolCall {
                    kind,
                    input: pending_call.input,
                    output: event.content.clone(),
                    completed_at: index,
                });
            }
            _ => {}
        }
    }

    completed
}

fn parse_recall_observation(call: &CompletedMementoToolCall) -> RecallObservation {
    let input = serde_json::from_str::<Value>(&call.input).unwrap_or(Value::Null);
    let payload = serde_json::from_str::<Value>(&call.output).unwrap_or(Value::Null);
    RecallObservation {
        completed_at: call.completed_at,
        session_id: string_field(&input, &["session_id", "sessionId"]),
        search_event_id: string_field(
            &payload,
            &["_searchEventId", "searchEventId", "search_event_id"],
        ),
        fragment_ids: recall_fragment_ids(&payload),
    }
}

fn parse_tool_feedback_observation(
    call: &CompletedMementoToolCall,
) -> Option<ToolFeedbackObservation> {
    let payload = serde_json::from_str::<Value>(&call.input).ok()?;
    let tool_name = string_field(&payload, &["tool_name", "toolName"])?;
    if canonical_memento_tool_kind(&tool_name) != Some(MementoToolKind::Recall) {
        return None;
    }

    Some(ToolFeedbackObservation {
        completed_at: call.completed_at,
        search_event_id: string_field(&payload, &["search_event_id", "searchEventId"]),
        fragment_ids: string_array_field(&payload, &["fragment_ids", "fragmentIds"]),
    })
}

fn find_matching_recall(
    recalls: &[RecallObservation],
    matched_recalls: &[bool],
    feedback: &ToolFeedbackObservation,
) -> Option<usize> {
    find_recall_by_search_event_id(recalls, matched_recalls, feedback)
        .or_else(|| find_recall_by_fragment_ids(recalls, matched_recalls, feedback))
        .or_else(|| find_latest_prior_unmatched_recall(recalls, matched_recalls, feedback))
}

fn find_recall_by_search_event_id(
    recalls: &[RecallObservation],
    matched_recalls: &[bool],
    feedback: &ToolFeedbackObservation,
) -> Option<usize> {
    let search_event_id = feedback.search_event_id.as_deref()?;
    recalls
        .iter()
        .enumerate()
        .rev()
        .find(|(index, recall)| {
            !matched_recalls[*index]
                && recall.completed_at < feedback.completed_at
                && recall.search_event_id.as_deref() == Some(search_event_id)
        })
        .map(|(index, _)| index)
}

fn find_recall_by_fragment_ids(
    recalls: &[RecallObservation],
    matched_recalls: &[bool],
    feedback: &ToolFeedbackObservation,
) -> Option<usize> {
    if feedback.fragment_ids.is_empty() {
        return None;
    }

    recalls
        .iter()
        .enumerate()
        .rev()
        .find(|(index, recall)| {
            !matched_recalls[*index]
                && recall.completed_at < feedback.completed_at
                && recall
                    .fragment_ids
                    .iter()
                    .any(|fragment_id| feedback.fragment_ids.iter().any(|id| id == fragment_id))
        })
        .map(|(index, _)| index)
}

fn find_latest_prior_unmatched_recall(
    recalls: &[RecallObservation],
    matched_recalls: &[bool],
    feedback: &ToolFeedbackObservation,
) -> Option<usize> {
    recalls
        .iter()
        .enumerate()
        .rev()
        .find(|(index, recall)| {
            !matched_recalls[*index] && recall.completed_at < feedback.completed_at
        })
        .map(|(index, _)| index)
}

fn recall_is_relevant(events: &[SessionTranscriptEvent], recall: &RecallObservation) -> bool {
    if recall.fragment_ids.is_empty() {
        return false;
    }

    events.iter().skip(recall.completed_at + 1).any(|event| {
        matches!(
            event.kind,
            SessionTranscriptEventKind::Assistant | SessionTranscriptEventKind::Result
        ) && recall
            .fragment_ids
            .iter()
            .any(|fragment_id| event.content.contains(fragment_id))
    })
}

fn canonical_memento_tool_kind(name: &str) -> Option<MementoToolKind> {
    let normalized = name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        // Memento MCP exposes recall via the `context` tool, while older
        // transcript fixtures still use `recall`.
        "context" | "recall" => return Some(MementoToolKind::Recall),
        "tool_feedback" => return Some(MementoToolKind::ToolFeedback),
        _ => {}
    }

    explicit_memento_tool_kind(&normalized)
}

fn explicit_memento_tool_kind(name: &str) -> Option<MementoToolKind> {
    let normalized = name.trim().to_ascii_lowercase();
    let segments = normalized.split("__").collect::<Vec<_>>();
    if segments.len() < 2 || !segments.iter().any(|segment| *segment == "memento") {
        return None;
    }
    match segments.last().copied() {
        Some("context") | Some("recall") => Some(MementoToolKind::Recall),
        Some("tool_feedback") => Some(MementoToolKind::ToolFeedback),
        _ => None,
    }
}

fn recall_fragment_ids(payload: &Value) -> Vec<String> {
    if let Some(fragments) = payload.get("fragments").and_then(Value::as_array) {
        let ids = fragments
            .iter()
            .filter_map(|fragment| {
                fragment
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
            })
            .collect::<Vec<_>>();
        if !ids.is_empty() {
            return ids;
        }
    }

    string_array_field(payload, &["fragment_ids", "fragmentIds"])
}

fn string_field(payload: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn string_array_field(payload: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_array))
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn normalized_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

pub(super) fn reminder_transcript_event(content: String) -> SessionTranscriptEvent {
    SessionTranscriptEvent {
        kind: SessionTranscriptEventKind::System,
        tool_name: None,
        summary: Some("memento voluntary tool_feedback reminder".to_string()),
        content,
        status: Some("info".to_string()),
        is_error: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_use(name: &str, content: Value) -> SessionTranscriptEvent {
        SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::ToolUse,
            tool_name: Some(name.to_string()),
            summary: None,
            content: serde_json::to_string(&content).unwrap(),
            status: Some("running".to_string()),
            is_error: false,
        }
    }

    fn tool_result(name: &str, content: Value) -> SessionTranscriptEvent {
        SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::ToolResult,
            tool_name: Some(name.to_string()),
            summary: None,
            content: serde_json::to_string(&content).unwrap(),
            status: Some("success".to_string()),
            is_error: false,
        }
    }

    fn assistant(content: &str) -> SessionTranscriptEvent {
        SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::Assistant,
            tool_name: None,
            summary: None,
            content: content.to_string(),
            status: Some("success".to_string()),
            is_error: false,
        }
    }

    fn result(content: &str) -> SessionTranscriptEvent {
        SessionTranscriptEvent {
            kind: SessionTranscriptEventKind::Result,
            tool_name: None,
            summary: None,
            content: content.to_string(),
            status: Some("success".to_string()),
            is_error: false,
        }
    }

    #[test]
    fn analyze_turn_marks_relevance_and_missing_feedback() {
        let events = vec![
            tool_use("mcp__memento__recall", json!({"query":"foo"})),
            tool_result(
                "mcp__memento__recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-1",
                    "fragments": [{"id": "frag-1"}]
                }),
            ),
            assistant("frag-1 정보를 바탕으로 정리합니다."),
            tool_use("mcp__memento__recall", json!({"query":"bar"})),
            tool_result(
                "mcp__memento__recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-2",
                    "fragments": [{"id": "frag-2"}]
                }),
            ),
            result("최종 답변에는 fragment id를 노출하지 않습니다."),
        ];

        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 2);
        assert_eq!(analysis.manual_feedback_count, 0);
        assert_eq!(analysis.manual_covered_recall_count, 0);
        assert_eq!(
            analysis.pending_feedbacks,
            vec![
                PendingRecallFeedback {
                    session_id: None,
                    search_event_id: Some("search-1".to_string()),
                    fragment_ids: vec!["frag-1".to_string()],
                    relevant: true,
                    sufficient: false,
                },
                PendingRecallFeedback {
                    session_id: None,
                    search_event_id: Some("search-2".to_string()),
                    fragment_ids: vec!["frag-2".to_string()],
                    relevant: false,
                    sufficient: true,
                },
            ]
        );
        assert_eq!(analysis.covered_recall_count_after(1), 1);
        assert_eq!(analysis.covered_recall_count_after(2), 2);
    }

    #[test]
    fn analyze_turn_treats_memento_context_as_recall() {
        let events = vec![
            tool_use("mcp__memento__context", json!({"query":"foo"})),
            tool_result(
                "mcp__memento__context",
                json!({
                    "success": true,
                    "_searchEventId": "search-1",
                    "fragments": [{"id": "frag-1"}]
                }),
            ),
            assistant("frag-1 정보를 바탕으로 정리합니다."),
        ];

        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 1);
        assert_eq!(analysis.manual_feedback_count, 0);
        assert_eq!(analysis.manual_covered_recall_count, 0);
        assert_eq!(
            analysis.pending_feedbacks,
            vec![PendingRecallFeedback {
                session_id: None,
                search_event_id: Some("search-1".to_string()),
                fragment_ids: vec!["frag-1".to_string()],
                relevant: true,
                sufficient: true,
            }]
        );
    }

    #[test]
    fn analyze_turn_matches_manual_feedback_by_search_event_id() {
        let events = vec![
            tool_use("recall", json!({"query":"foo"})),
            tool_result(
                "recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-1",
                    "fragments": [{"id": "frag-1"}]
                }),
            ),
            assistant("frag-1을 사용한 답변"),
            tool_use(
                "tool_feedback",
                json!({
                    "tool_name": "recall",
                    "search_event_id": "search-1",
                    "fragment_ids": ["frag-1"],
                    "relevant": true,
                    "sufficient": true
                }),
            ),
            tool_result("tool_feedback", json!({"success": true})),
        ];

        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 1);
        assert_eq!(analysis.manual_feedback_count, 1);
        assert_eq!(analysis.manual_covered_recall_count, 1);
        assert!(analysis.pending_feedbacks.is_empty());
    }

    #[test]
    fn analyze_turn_falls_back_to_latest_prior_recall_when_feedback_lacks_ids() {
        let events = vec![
            tool_use("recall", json!({"query":"foo"})),
            tool_result(
                "recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-1",
                    "fragments": [{"id": "frag-1"}]
                }),
            ),
            tool_use("recall", json!({"query":"bar"})),
            tool_result(
                "recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-2",
                    "fragments": [{"id": "frag-2"}]
                }),
            ),
            tool_use(
                "tool_feedback",
                json!({
                    "tool_name": "recall",
                    "relevant": false,
                    "sufficient": true
                }),
            ),
            tool_result("tool_feedback", json!({"success": true})),
        ];

        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 2);
        assert_eq!(analysis.manual_feedback_count, 1);
        assert_eq!(analysis.manual_covered_recall_count, 1);
        assert_eq!(analysis.pending_feedbacks.len(), 1);
        assert_eq!(
            analysis.pending_feedbacks[0].search_event_id.as_deref(),
            Some("search-1")
        );
    }

    #[test]
    fn analyze_turn_preserves_recall_session_id_for_auto_feedback() {
        let events = vec![
            tool_use("recall", json!({"query":"foo", "sessionId":"session-keep"})),
            tool_result(
                "recall",
                json!({
                    "success": true,
                    "_searchEventId": "search-1",
                    "fragments": [{"id": "frag-1"}]
                }),
            ),
            assistant("frag-1을 사용한 답변"),
        ];

        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.pending_feedbacks.len(), 1);
        assert_eq!(
            analysis.pending_feedbacks[0].session_id.as_deref(),
            Some("session-keep")
        );
        assert_eq!(
            analysis.pending_feedbacks[0]
                .clone()
                .into_request(Some("session-fallback"))
                .session_id
                .as_deref(),
            Some("session-keep")
        );
    }

    #[test]
    fn reminder_names_pending_search_event_ids_before_fallback() {
        let analysis = RecallFeedbackTurnAnalysis {
            recall_count: 2,
            manual_feedback_count: 0,
            manual_covered_recall_count: 0,
            pending_feedbacks: vec![
                PendingRecallFeedback {
                    session_id: None,
                    search_event_id: Some("search-1".to_string()),
                    fragment_ids: vec!["frag-1".to_string()],
                    relevant: true,
                    sufficient: false,
                },
                PendingRecallFeedback {
                    session_id: None,
                    search_event_id: Some("search-2".to_string()),
                    fragment_ids: vec!["frag-2".to_string()],
                    relevant: false,
                    sufficient: true,
                },
            ],
        };

        let reminder = build_voluntary_feedback_reminder(&analysis).unwrap();

        assert!(reminder.contains("recall 2건"));
        assert!(reminder.contains("tool_feedback 0/2"));
        assert!(reminder.contains("search-1"));
        assert!(reminder.contains("search-2"));
        assert!(reminder.contains("tool_feedback(search_event_id, relevant, sufficient)"));
    }

    #[test]
    fn transcript_contains_explicit_memento_tool_call_ignores_bare_context_names() {
        let bare_context = vec![
            tool_use("context", json!({"query":"foo"})),
            tool_result("context", json!({"success": true})),
        ];
        assert!(!transcript_contains_explicit_memento_tool_call(
            &bare_context
        ));

        let explicit_memento = vec![
            tool_use("mcp__memento__context", json!({"query":"foo"})),
            tool_result("mcp__memento__context", json!({"success": true})),
        ];
        assert!(transcript_contains_explicit_memento_tool_call(
            &explicit_memento
        ));
    }
}
