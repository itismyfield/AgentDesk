use std::collections::HashSet;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};

use super::auto_remember_store::{
    AutoRememberAuditEntry, AutoRememberMemoryStatus, AutoRememberStore,
};
use super::{
    MementoBackend, MementoRememberRequest, TokenUsage, backend_is_active,
    resolve_memento_workspace,
};

const AUTO_REMEMBER_SOURCE: &str = "agentdesk:auto-remember";
const AUTO_REMEMBER_AGENT_ID: &str = "default";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AutoRememberSignalKind {
    ConfirmedErrorRootCause,
    TechnicalDecision,
    ConfigChange,
}

impl AutoRememberSignalKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConfirmedErrorRootCause => "confirmed_error_root_cause",
            Self::TechnicalDecision => "technical_decision",
            Self::ConfigChange => "config_change",
        }
    }

    fn topic(self) -> &'static str {
        match self {
            Self::ConfirmedErrorRootCause => "error-root-cause",
            Self::TechnicalDecision => "technical-decision",
            Self::ConfigChange => "config-change",
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Self::ConfirmedErrorRootCause => "error",
            Self::TechnicalDecision => "decision",
            Self::ConfigChange => "fact",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AutoRememberCandidate {
    signal_kind: AutoRememberSignalKind,
    content: String,
    keywords: Vec<String>,
    workspace: String,
    candidate_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberTurnRequest {
    pub(crate) turn_id: String,
    pub(crate) role_id: String,
    pub(crate) channel_id: u64,
    pub(crate) user_text: String,
    pub(crate) assistant_text: String,
    pub(crate) transcript_events: Vec<SessionTranscriptEvent>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AutoRememberExecutionResult {
    pub(crate) token_usage: TokenUsage,
    pub(crate) remembered_count: usize,
    pub(crate) duplicate_count: usize,
    pub(crate) warnings: Vec<String>,
}

pub(crate) async fn run_auto_remember(
    settings: &ResolvedMemorySettings,
    request: AutoRememberTurnRequest,
) -> AutoRememberExecutionResult {
    if !settings.auto_remember_enabled || settings.backend != MemoryBackendKind::Memento {
        return AutoRememberExecutionResult::default();
    }
    if !backend_is_active(MemoryBackendKind::Memento) {
        return AutoRememberExecutionResult {
            warnings: vec!["memento backend inactive; skipping auto-remember".to_string()],
            ..AutoRememberExecutionResult::default()
        };
    }

    let candidates = extract_candidates(&request);
    if candidates.is_empty() {
        return AutoRememberExecutionResult::default();
    }

    let store = match AutoRememberStore::open() {
        Ok(store) => store,
        Err(error) => {
            return AutoRememberExecutionResult {
                warnings: vec![error],
                ..AutoRememberExecutionResult::default()
            };
        }
    };
    let backend = MementoBackend::new(settings.clone());

    let mut seen_hashes = HashSet::new();
    let mut result = AutoRememberExecutionResult::default();

    for candidate in candidates {
        if !seen_hashes.insert(candidate.candidate_hash.clone()) {
            result.duplicate_count += 1;
            let _ = store.upsert_audit(AutoRememberAuditEntry {
                turn_id: &request.turn_id,
                candidate_hash: &candidate.candidate_hash,
                signal_kind: candidate.signal_kind.as_str(),
                workspace: &candidate.workspace,
                status: AutoRememberMemoryStatus::DuplicateSkip,
                error: Some("duplicate candidate in same turn"),
            });
            continue;
        }

        match store.lookup_status(&candidate.workspace, &candidate.candidate_hash) {
            Ok(Some(status)) if status.suppresses_repeat() => {
                result.duplicate_count += 1;
                let _ = store.upsert_audit(AutoRememberAuditEntry {
                    turn_id: &request.turn_id,
                    candidate_hash: &candidate.candidate_hash,
                    signal_kind: candidate.signal_kind.as_str(),
                    workspace: &candidate.workspace,
                    status: AutoRememberMemoryStatus::DuplicateSkip,
                    error: Some("candidate already remembered"),
                });
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                result.warnings.push(error);
                continue;
            }
        }

        match backend
            .remember(memento_request_from_candidate(&candidate))
            .await
        {
            Ok(token_usage) => {
                result.token_usage.saturating_add_assign(token_usage);
                result.remembered_count += 1;
                if let Err(error) = store.upsert_audit(AutoRememberAuditEntry {
                    turn_id: &request.turn_id,
                    candidate_hash: &candidate.candidate_hash,
                    signal_kind: candidate.signal_kind.as_str(),
                    workspace: &candidate.workspace,
                    status: AutoRememberMemoryStatus::Remembered,
                    error: None,
                }) {
                    result.warnings.push(error);
                }
            }
            Err(error) => {
                result.warnings.push(error.clone());
                if let Err(store_error) = store.upsert_audit(AutoRememberAuditEntry {
                    turn_id: &request.turn_id,
                    candidate_hash: &candidate.candidate_hash,
                    signal_kind: candidate.signal_kind.as_str(),
                    workspace: &candidate.workspace,
                    status: AutoRememberMemoryStatus::RememberFailed,
                    error: Some(&error),
                }) {
                    result.warnings.push(store_error);
                }
            }
        }
    }

    result
}

fn extract_candidates(request: &AutoRememberTurnRequest) -> Vec<AutoRememberCandidate> {
    let workspace_override = std::env::var("MEMENTO_WORKSPACE").ok();
    let workspace = resolve_memento_workspace(
        &request.role_id,
        request.channel_id,
        workspace_override.as_deref(),
    );

    collect_candidate_units(request)
        .into_iter()
        .filter_map(|unit| candidate_from_unit(&workspace, &unit))
        .collect()
}

fn collect_candidate_units(request: &AutoRememberTurnRequest) -> Vec<String> {
    let mut units = Vec::new();
    let mut seen = HashSet::new();

    push_text_units(&mut units, &mut seen, &request.assistant_text);
    for event in &request.transcript_events {
        match event.kind {
            SessionTranscriptEventKind::Assistant
            | SessionTranscriptEventKind::ToolResult
            | SessionTranscriptEventKind::Result
            | SessionTranscriptEventKind::Error => {
                if let Some(summary) = event.summary.as_deref() {
                    push_text_units(&mut units, &mut seen, summary);
                }
                push_text_units(&mut units, &mut seen, &event.content);
            }
            _ => {}
        }
    }

    units
}

fn push_text_units(units: &mut Vec<String>, seen: &mut HashSet<String>, text: &str) {
    for raw in text.lines() {
        let normalized = normalize_whitespace(raw);
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            units.push(normalized);
        }
    }
}

fn candidate_from_unit(workspace: &str, unit: &str) -> Option<AutoRememberCandidate> {
    if contains_uncertainty(unit) {
        return None;
    }

    let mut matches = Vec::new();
    if is_confirmed_error_root_cause(unit) {
        matches.push(AutoRememberSignalKind::ConfirmedErrorRootCause);
    }
    if is_technical_decision(unit) {
        matches.push(AutoRememberSignalKind::TechnicalDecision);
    }
    if is_config_change(unit) {
        matches.push(AutoRememberSignalKind::ConfigChange);
    }

    if matches.len() != 1 {
        return None;
    }

    let signal_kind = matches[0];
    let content = normalize_whitespace(unit);
    let keywords = extract_keywords(signal_kind, &content);
    let candidate_hash = hash_candidate(workspace, signal_kind, &content);

    Some(AutoRememberCandidate {
        signal_kind,
        content,
        keywords,
        workspace: workspace.to_string(),
        candidate_hash,
    })
}

fn contains_uncertainty(unit: &str) -> bool {
    let lower = unit.to_ascii_lowercase();
    [
        "추정",
        "추측",
        "아마도",
        "가능성이",
        "가능성",
        "일 수",
        "일수",
        "might",
        "maybe",
        "probably",
        "likely",
        "guess",
        "hypothesis",
        "브레인스토밍",
        "가정",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_confirmed_error_root_cause(unit: &str) -> bool {
    let lower = unit.to_ascii_lowercase();
    [
        "원인은",
        "실패 원인은",
        "because",
        "due to",
        "caused by",
        "root cause",
        "때문에 발생",
        "로 인해 발생",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn is_technical_decision(unit: &str) -> bool {
    let lower = unit.to_ascii_lowercase();
    let has_decision_verb = [
        "결정",
        "채택",
        "사용하기로",
        "고정",
        "표준화",
        "하기로 한다",
        "decided to",
        "we use",
        "we will use",
        "adopt",
        "standardize",
        "policy",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let has_technical_target = [
        "backend",
        "sqlite",
        "memento",
        "mem0",
        "agentdesk",
        "구조",
        "아키텍처",
        "패턴",
        "전략",
        "contract",
        "schema",
        "coordinator",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    has_decision_verb && has_technical_target
}

fn is_config_change(unit: &str) -> bool {
    let lower = unit.to_ascii_lowercase();
    let has_change_verb = [
        "변경",
        "바꿨",
        "수정",
        "set ",
        "set to",
        "updated",
        "changed",
        "configured",
        "enabled",
        "disabled",
        "switched",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !has_change_verb {
        return false;
    }

    let key_value_re = Regex::new(r"[A-Za-z0-9_.:-]+\s*=\s*\S+").unwrap();
    let from_to_re = Regex::new(r"(?i)\bfrom\b.+\bto\b").unwrap();
    let korean_transition_re = Regex::new(r".+에서\s+.+로").unwrap();

    unit.contains("->")
        || unit.contains("=>")
        || key_value_re.is_match(unit)
        || from_to_re.is_match(unit)
        || korean_transition_re.is_match(unit)
}

fn extract_keywords(signal_kind: AutoRememberSignalKind, unit: &str) -> Vec<String> {
    let token_re = Regex::new(r"[A-Za-z0-9_.:/-]{4,}").unwrap();
    let mut keywords = vec![signal_kind.as_str().to_string()];
    let mut seen = HashSet::from([signal_kind.as_str().to_string()]);

    for matched in token_re.find_iter(unit) {
        let token = matched.as_str().to_ascii_lowercase();
        if seen.insert(token.clone()) {
            keywords.push(token);
        }
        if keywords.len() >= 4 {
            break;
        }
    }

    keywords
}

fn hash_candidate(workspace: &str, signal_kind: AutoRememberSignalKind, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(workspace.as_bytes());
    hasher.update(b"\n");
    hasher.update(signal_kind.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

fn memento_request_from_candidate(candidate: &AutoRememberCandidate) -> MementoRememberRequest {
    MementoRememberRequest {
        content: candidate.content.clone(),
        topic: candidate.signal_kind.topic().to_string(),
        kind: candidate.signal_kind.kind().to_string(),
        keywords: candidate.keywords.clone(),
        source: Some(AUTO_REMEMBER_SOURCE.to_string()),
        workspace: Some(candidate.workspace.clone()),
        agent_id: Some(AUTO_REMEMBER_AGENT_ID.to_string()),
        ..MementoRememberRequest::default()
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::services::discord::runtime_store::lock_test_env;
    use crate::services::discord::settings::ResolvedMemorySettings;

    fn remember_request_from_text(
        signal_kind: AutoRememberSignalKind,
        content: &str,
    ) -> MementoRememberRequest {
        let candidate = AutoRememberCandidate {
            signal_kind,
            content: content.to_string(),
            keywords: extract_keywords(signal_kind, content),
            workspace: "agentdesk-default".to_string(),
            candidate_hash: "hash".to_string(),
        };
        memento_request_from_candidate(&candidate)
    }

    #[test]
    fn extracts_confirmed_error_root_cause_from_assistant_line() {
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: "왜 실패했어?".to_string(),
            assistant_text: "실패 원인은 MCP 세션 ID가 누락되었기 때문이다.".to_string(),
            transcript_events: Vec::new(),
        };

        let candidates = extract_candidates(&request);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].signal_kind,
            AutoRememberSignalKind::ConfirmedErrorRootCause
        );
    }

    #[test]
    fn skips_uncertain_lines() {
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "원인은 아마도 MCP 세션 ID 누락일 수 있다.".to_string(),
            transcript_events: Vec::new(),
        };

        assert!(extract_candidates(&request).is_empty());
    }

    #[test]
    fn config_change_requires_explicit_transition_or_value() {
        let ambiguous = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "메모리 설정을 변경했다.".to_string(),
            transcript_events: Vec::new(),
        };
        let explicit = AutoRememberTurnRequest {
            turn_id: "turn-2".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "memory.backend를 file에서 memento로 변경했다.".to_string(),
            transcript_events: Vec::new(),
        };

        assert!(extract_candidates(&ambiguous).is_empty());
        let candidates = extract_candidates(&explicit);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].signal_kind,
            AutoRememberSignalKind::ConfigChange
        );
    }

    #[test]
    fn candidate_mapping_uses_exact_contract() {
        let root_cause = remember_request_from_text(
            AutoRememberSignalKind::ConfirmedErrorRootCause,
            "실패 원인은 MCP 세션 ID 누락이다.",
        );
        assert_eq!(root_cause.topic, "error-root-cause");
        assert_eq!(root_cause.kind, "error");

        let decision = remember_request_from_text(
            AutoRememberSignalKind::TechnicalDecision,
            "SQLite sidecar를 표준으로 사용하기로 결정했다.",
        );
        assert_eq!(decision.topic, "technical-decision");
        assert_eq!(decision.kind, "decision");

        let change = remember_request_from_text(
            AutoRememberSignalKind::ConfigChange,
            "memory.backend를 file에서 memento로 변경했다.",
        );
        assert_eq!(change.topic, "config-change");
        assert_eq!(change.kind, "fact");
        assert_eq!(change.source.as_deref(), Some(AUTO_REMEMBER_SOURCE));
        assert_eq!(change.agent_id.as_deref(), Some(AUTO_REMEMBER_AGENT_ID));
        assert_eq!(
            change.keywords.first().map(String::as_str),
            Some("config_change")
        );
    }

    #[tokio::test]
    async fn disabled_auto_remember_does_not_create_sidecar_file() {
        let _guard = lock_test_env();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let settings = ResolvedMemorySettings::default();
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "실패 원인은 MCP 세션 ID 누락이다.".to_string(),
            transcript_events: Vec::new(),
        };

        let result = run_auto_remember(&settings, request).await;
        assert_eq!(result, AutoRememberExecutionResult::default());
        assert!(
            !temp
                .path()
                .join("data")
                .join("memory-auto-remember.sqlite")
                .exists()
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[test]
    fn confirmed_error_root_cause_does_not_bleed_into_other_signal_classes() {
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "실패 원인은 MCP 세션 ID 누락이다.".to_string(),
            transcript_events: Vec::new(),
        };

        let candidates = extract_candidates(&request);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].signal_kind,
            AutoRememberSignalKind::ConfirmedErrorRootCause,
            "should not bleed into TechnicalDecision or ConfigChange"
        );
    }

    #[test]
    fn brainstorming_and_generic_explanations_produce_no_candidates() {
        let cases = [
            "예를 들어, SQLite를 backend로 사용할 수 있습니다.",
            "이 접근 방식의 장점을 살펴보겠습니다.",
            "브레인스토밍: coordinator 구조를 도입하면 어떨까?",
            "가능한 방법으로는 A, B, C가 있다.",
        ];

        for text in &cases {
            let request = AutoRememberTurnRequest {
                turn_id: "turn-1".to_string(),
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                user_text: String::new(),
                assistant_text: (*text).to_string(),
                transcript_events: Vec::new(),
            };
            let candidates = extract_candidates(&request);
            assert!(
                candidates.is_empty(),
                "expected no candidates for: {text}, got: {candidates:?}"
            );
        }
    }

    #[tokio::test]
    async fn cross_turn_duplicate_hash_is_recorded_as_duplicate_skip() {
        use crate::services::memory::auto_remember_store::{
            AutoRememberAuditEntry, AutoRememberMemoryStatus, AutoRememberStore,
        };

        let _guard = lock_test_env();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let store = AutoRememberStore::open().unwrap();
        let workspace = "agentdesk-default";
        let hash = "hash-cross-turn-001";

        store
            .upsert_audit(AutoRememberAuditEntry {
                turn_id: "turn-prev",
                candidate_hash: hash,
                signal_kind: "technical_decision",
                workspace,
                status: AutoRememberMemoryStatus::Remembered,
                error: None,
            })
            .unwrap();

        store
            .upsert_audit(AutoRememberAuditEntry {
                turn_id: "turn-next",
                candidate_hash: hash,
                signal_kind: "technical_decision",
                workspace,
                status: AutoRememberMemoryStatus::DuplicateSkip,
                error: Some("candidate already remembered"),
            })
            .unwrap();

        let status = store.lookup_status(workspace, hash).unwrap();
        assert_eq!(status, Some(AutoRememberMemoryStatus::DuplicateSkip));
        assert!(status.unwrap().suppresses_repeat());

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }

    #[tokio::test]
    async fn non_memento_backend_does_not_create_sidecar_file() {
        let _guard = lock_test_env();
        let temp = TempDir::new().unwrap();
        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let settings = ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            auto_remember_enabled: true,
            ..ResolvedMemorySettings::default()
        };
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: "실패 원인은 MCP 세션 ID 누락이다.".to_string(),
            transcript_events: Vec::new(),
        };

        let result = run_auto_remember(&settings, request).await;
        assert_eq!(result, AutoRememberExecutionResult::default());
        assert!(
            !temp
                .path()
                .join("data")
                .join("memory-auto-remember.sqlite")
                .exists()
        );

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}
