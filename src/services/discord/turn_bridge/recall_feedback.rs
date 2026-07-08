use std::collections::VecDeque;

use serde_json::Value;

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingRecallFeedback {
    pub search_event_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RecallFeedbackTurnAnalysis {
    pub recall_count: usize,
    pub manual_feedback_count: usize,
    pub manual_covered_recall_count: usize,
    pub pending_feedbacks: Vec<PendingRecallFeedback>,
}

impl RecallFeedbackTurnAnalysis {
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
        .map(|(_, recall)| PendingRecallFeedback {
            search_event_id: recall.search_event_id.clone(),
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
    let payload = serde_json::from_str::<Value>(&call.output).unwrap_or(Value::Null);
    RecallObservation {
        completed_at: call.completed_at,
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
mod recall_feedback_reminder_tests {
    use super::*;

    fn tool_event(
        kind: SessionTranscriptEventKind,
        tool_name: &str,
        content: &str,
    ) -> SessionTranscriptEvent {
        SessionTranscriptEvent {
            kind,
            tool_name: Some(tool_name.to_string()),
            summary: None,
            content: content.to_string(),
            status: None,
            is_error: false,
        }
    }

    fn recall_pair(search_event_id: &str) -> Vec<SessionTranscriptEvent> {
        vec![
            tool_event(SessionTranscriptEventKind::ToolUse, "recall", "{}"),
            tool_event(
                SessionTranscriptEventKind::ToolResult,
                "recall",
                &format!("{{\"_searchEventId\":\"{search_event_id}\"}}"),
            ),
        ]
    }

    fn tool_feedback_pair(search_event_id: &str) -> Vec<SessionTranscriptEvent> {
        vec![
            tool_event(
                SessionTranscriptEventKind::ToolUse,
                "tool_feedback",
                &format!("{{\"tool_name\":\"recall\",\"search_event_id\":\"{search_event_id}\"}}"),
            ),
            tool_event(
                SessionTranscriptEventKind::ToolResult,
                "tool_feedback",
                "{}",
            ),
        ]
    }

    // Truth table core: whenever there is at least one recall AND at least one
    // uncovered recall (pending_feedbacks non-empty), a voluntary reminder is
    // required — and therefore always injected. This is exactly why the removed
    // auto-submit fallback (`!reminder_injected && !pending.is_empty()`) was
    // unreachable: its second conjunct implies the first is false. #4307 PR-A.
    #[test]
    fn pending_feedback_presence_drives_reminder_truth_table() {
        let with_pending = RecallFeedbackTurnAnalysis {
            recall_count: 2,
            manual_feedback_count: 1,
            manual_covered_recall_count: 1,
            pending_feedbacks: vec![PendingRecallFeedback {
                search_event_id: Some("evt-1".to_string()),
            }],
        };
        assert!(with_pending.needs_voluntary_feedback_reminder());
        assert!(build_voluntary_feedback_reminder(&with_pending).is_some());

        let all_covered = RecallFeedbackTurnAnalysis {
            recall_count: 2,
            manual_feedback_count: 2,
            manual_covered_recall_count: 2,
            pending_feedbacks: Vec::new(),
        };
        assert!(!all_covered.needs_voluntary_feedback_reminder());
        assert!(build_voluntary_feedback_reminder(&all_covered).is_none());

        let no_recall = RecallFeedbackTurnAnalysis::default();
        assert!(!no_recall.needs_voluntary_feedback_reminder());
        assert!(build_voluntary_feedback_reminder(&no_recall).is_none());
    }

    #[test]
    fn uncovered_recall_yields_pending_and_requires_reminder() {
        let events = recall_pair("evt-1");
        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 1);
        assert_eq!(analysis.manual_feedback_count, 0);
        assert_eq!(analysis.manual_covered_recall_count, 0);
        assert_eq!(analysis.pending_feedbacks.len(), 1);
        assert_eq!(
            analysis.pending_feedbacks[0].search_event_id.as_deref(),
            Some("evt-1")
        );
        assert!(analysis.needs_voluntary_feedback_reminder());

        let reminder = build_voluntary_feedback_reminder(&analysis).expect("reminder");
        assert!(reminder.contains("evt-1"));
    }

    #[test]
    fn covered_recall_leaves_no_pending_and_no_reminder() {
        let mut events = recall_pair("evt-1");
        events.extend(tool_feedback_pair("evt-1"));
        let analysis = analyze_recall_feedback_turn(&events);

        assert_eq!(analysis.recall_count, 1);
        assert_eq!(analysis.manual_feedback_count, 1);
        assert_eq!(analysis.manual_covered_recall_count, 1);
        assert!(analysis.pending_feedbacks.is_empty());
        assert!(!analysis.needs_voluntary_feedback_reminder());
        assert!(build_voluntary_feedback_reminder(&analysis).is_none());
    }
}
