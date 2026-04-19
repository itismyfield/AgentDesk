use std::collections::HashSet;

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};

use super::auto_remember_quality::{
    AutoRememberQualityInput, improve_candidate_with_config, rewrite_supported_with_config,
};
use super::auto_remember_store::{
    AutoRememberAuditEntry, AutoRememberMemoryStatus, AutoRememberRetryRecord, AutoRememberStage,
    AutoRememberStore,
};
use super::{
    MementoBackend, MementoFragmentSummary, MementoRememberRequest, TokenUsage, backend_is_active,
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

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "confirmed_error_root_cause" => Some(Self::ConfirmedErrorRootCause),
            "technical_decision" => Some(Self::TechnicalDecision),
            "config_change" => Some(Self::ConfigChange),
            _ => None,
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

    fn importance(self) -> f64 {
        match self {
            Self::ConfirmedErrorRootCause => 0.75,
            Self::TechnicalDecision => 0.70,
            Self::ConfigChange => 0.65,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NormalizedToolEvidence {
    summary: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CandidateUnit {
    text: String,
    supporting_evidence: Vec<String>,
    normalized_tool_evidence: Vec<NormalizedToolEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AutoRememberRawCandidate {
    signal_kind: AutoRememberSignalKind,
    raw_content: String,
    supporting_evidence: Vec<String>,
    entity_key: Option<String>,
    workspace: String,
    candidate_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AutoRememberValidatedCandidate {
    signal_kind: AutoRememberSignalKind,
    content: String,
    keywords: Vec<String>,
    entity_key: Option<String>,
    workspace: String,
    candidate_hash: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoRememberDecision {
    StoreDirectly,
    RewriteNeeded,
    Skip(AutoRememberSkipReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutoRememberSkipReason {
    Uncertain,
    MissingEntityKey,
    MissingWorkspace,
    ValidatorRejected,
    RewriteUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AutoRememberCandidateSource {
    Fresh,
    RetryQueue { retry_count: u32 },
}

impl AutoRememberCandidateSource {
    fn retry_count(&self) -> u32 {
        match self {
            Self::Fresh => 0,
            Self::RetryQueue { retry_count } => *retry_count,
        }
    }

    fn is_retry_queue(&self) -> bool {
        matches!(self, Self::RetryQueue { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AutoRememberQueuedCandidate {
    raw: AutoRememberRawCandidate,
    source: AutoRememberCandidateSource,
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

    let mut result = AutoRememberExecutionResult::default();
    let existing_store = match AutoRememberStore::open_existing() {
        Ok(store) => store,
        Err(error) => {
            result.warnings.push(error);
            None
        }
    };

    let retry_candidates = load_retry_candidates(existing_store.as_ref(), &mut result);
    let fresh_candidates = extract_candidates(&request)
        .into_iter()
        .map(|raw| AutoRememberQueuedCandidate {
            raw,
            source: AutoRememberCandidateSource::Fresh,
        })
        .collect::<Vec<_>>();

    if retry_candidates.is_empty() && fresh_candidates.is_empty() {
        return result;
    }

    let store = match existing_store {
        Some(store) => store,
        None => match AutoRememberStore::open() {
            Ok(store) => store,
            Err(error) => {
                result.warnings.push(error);
                return result;
            }
        },
    };
    let backend = MementoBackend::new(settings.clone());

    let mut seen_hashes = HashSet::new();
    let mut candidates = retry_candidates;
    candidates.extend(fresh_candidates);

    for queued in candidates {
        process_candidate(
            &request,
            settings,
            &store,
            &backend,
            queued,
            &mut seen_hashes,
            &mut result,
        )
        .await;
    }

    result
}

pub(crate) async fn resubmit_auto_remember_candidate(
    workspace: &str,
    candidate_hash: &str,
) -> Result<AutoRememberExecutionResult, String> {
    let workspace = normalize_whitespace(workspace);
    let candidate_hash = normalize_whitespace(candidate_hash);
    if workspace.is_empty() {
        return Err("auto-remember resubmit requires non-empty workspace".to_string());
    }
    if candidate_hash.is_empty() {
        return Err("auto-remember resubmit requires non-empty candidate hash".to_string());
    }

    if !backend_is_active(MemoryBackendKind::Memento) {
        return Err(
            "memento backend inactive; cannot resubmit auto-remember candidate".to_string(),
        );
    }

    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    let record = store
        .load_resubmittable_candidate(&workspace, &candidate_hash)?
        .ok_or_else(|| {
            format!(
                "no resubmittable auto-remember candidate found for workspace='{workspace}' hash='{candidate_hash}'"
            )
        })?;
    let raw_candidate = candidate_from_retry_record(&record).ok_or_else(|| {
        format!(
            "auto-remember candidate '{}' has unsupported signal_kind '{}'",
            record.candidate_hash, record.signal_kind
        )
    })?;

    store.reset_retry_state(&workspace, &candidate_hash)?;

    let mut resubmit_settings = resolve_memory_settings(None, None);
    resubmit_settings.backend = MemoryBackendKind::Memento;
    resubmit_settings.auto_remember_enabled = true;
    resubmit_settings.auto_remember.enabled = true;
    let backend = MementoBackend::new(resubmit_settings.clone());
    let request = AutoRememberTurnRequest {
        turn_id: format!(
            "manual-resubmit-{}",
            chrono::Local::now().format("%Y%m%d%H%M%S")
        ),
        role_id: String::new(),
        channel_id: 0,
        user_text: String::new(),
        assistant_text: String::new(),
        transcript_events: Vec::new(),
    };

    let mut seen_hashes = HashSet::new();
    let mut result = AutoRememberExecutionResult::default();
    process_candidate(
        &request,
        &resubmit_settings,
        &store,
        &backend,
        AutoRememberQueuedCandidate {
            raw: raw_candidate,
            source: AutoRememberCandidateSource::Fresh,
        },
        &mut seen_hashes,
        &mut result,
    )
    .await;
    Ok(result)
}

pub(crate) fn verify_auto_remember_candidate(
    workspace: &str,
    candidate_hash: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let workspace = normalize_whitespace(workspace);
    let candidate_hash = normalize_whitespace(candidate_hash);
    if workspace.is_empty() {
        return Err("auto-remember verify requires non-empty workspace".to_string());
    }
    if candidate_hash.is_empty() {
        return Err("auto-remember verify requires non-empty candidate hash".to_string());
    }

    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    store.set_operator_status(
        &workspace,
        &candidate_hash,
        AutoRememberMemoryStatus::OperatorVerified,
        note,
    )
}

pub(crate) fn reject_auto_remember_candidate(
    workspace: &str,
    candidate_hash: &str,
    note: Option<&str>,
) -> Result<(), String> {
    let workspace = normalize_whitespace(workspace);
    let candidate_hash = normalize_whitespace(candidate_hash);
    if workspace.is_empty() {
        return Err("auto-remember reject requires non-empty workspace".to_string());
    }
    if candidate_hash.is_empty() {
        return Err("auto-remember reject requires non-empty candidate hash".to_string());
    }

    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    store.set_operator_status(
        &workspace,
        &candidate_hash,
        AutoRememberMemoryStatus::OperatorRejected,
        note,
    )
}

pub(crate) fn requeue_auto_remember_candidate(
    workspace: &str,
    candidate_hash: &str,
) -> Result<(), String> {
    let workspace = normalize_whitespace(workspace);
    let candidate_hash = normalize_whitespace(candidate_hash);
    if workspace.is_empty() {
        return Err("auto-remember requeue requires non-empty workspace".to_string());
    }
    if candidate_hash.is_empty() {
        return Err("auto-remember requeue requires non-empty candidate hash".to_string());
    }

    let store = AutoRememberStore::open_existing()?
        .ok_or_else(|| "auto-remember sidecar does not exist for this runtime root".to_string())?;
    store.requeue_candidate(&workspace, &candidate_hash)
}

fn load_retry_candidates(
    store: Option<&AutoRememberStore>,
    result: &mut AutoRememberExecutionResult,
) -> Vec<AutoRememberQueuedCandidate> {
    let Some(store) = store else {
        return Vec::new();
    };
    let records = match store.load_retry_batch() {
        Ok(records) => records,
        Err(error) => {
            result.warnings.push(error);
            return Vec::new();
        }
    };

    let mut queued = Vec::new();
    for record in records {
        match candidate_from_retry_record(&record) {
            Some(raw) => queued.push(AutoRememberQueuedCandidate {
                raw,
                source: AutoRememberCandidateSource::RetryQueue {
                    retry_count: record.retry_count,
                },
            }),
            None => {
                result.warnings.push(format!(
                    "auto-remember retry queue contains invalid signal_kind '{}'",
                    record.signal_kind
                ));
                if let Err(error) = store.delete_retry(&record.workspace, &record.candidate_hash) {
                    result.warnings.push(error);
                }
            }
        }
    }

    queued
}

async fn process_candidate(
    request: &AutoRememberTurnRequest,
    settings: &ResolvedMemorySettings,
    store: &AutoRememberStore,
    backend: &MementoBackend,
    queued: AutoRememberQueuedCandidate,
    seen_hashes: &mut HashSet<String>,
    result: &mut AutoRememberExecutionResult,
) {
    let raw_candidate = queued.raw;
    let retry_count = queued.source.retry_count();

    if !seen_hashes.insert(raw_candidate.candidate_hash.clone()) {
        result.duplicate_count += 1;
        let _ = store.upsert_audit(AutoRememberAuditEntry {
            turn_id: &request.turn_id,
            candidate_hash: &raw_candidate.candidate_hash,
            signal_kind: raw_candidate.signal_kind.as_str(),
            workspace: &raw_candidate.workspace,
            stage: AutoRememberStage::Dedupe,
            status: AutoRememberMemoryStatus::DuplicateSkip,
            retry_count,
            error: Some("duplicate candidate in retry-drain + current turn"),
            raw_content: Some(&raw_candidate.raw_content),
            entity_key: raw_candidate.entity_key.as_deref(),
            supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
        });
        return;
    }

    match store.lookup_record(&raw_candidate.workspace, &raw_candidate.candidate_hash) {
        Ok(Some(record)) if record.status.suppresses_repeat() => {
            result.duplicate_count += 1;
            let _ = store.upsert_audit(AutoRememberAuditEntry {
                turn_id: &request.turn_id,
                candidate_hash: &raw_candidate.candidate_hash,
                signal_kind: raw_candidate.signal_kind.as_str(),
                workspace: &raw_candidate.workspace,
                stage: AutoRememberStage::Dedupe,
                status: AutoRememberMemoryStatus::DuplicateSkip,
                retry_count: record.retry_count,
                error: Some("candidate already processed"),
                raw_content: Some(&raw_candidate.raw_content),
                entity_key: raw_candidate.entity_key.as_deref(),
                supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
            });
            if queued.source.is_retry_queue() {
                if let Err(error) =
                    store.delete_retry(&raw_candidate.workspace, &raw_candidate.candidate_hash)
                {
                    result.warnings.push(error);
                }
            }
            return;
        }
        Ok(_) => {}
        Err(error) => {
            result.warnings.push(error);
            return;
        }
    }

    let decision = pre_validate_candidate(&raw_candidate, settings);
    let candidate = match materialize_candidate(&raw_candidate, decision, settings).await {
        Ok(Some(candidate)) => candidate,
        Ok(None) => {
            let _ = store.upsert_audit(AutoRememberAuditEntry {
                turn_id: &request.turn_id,
                candidate_hash: &raw_candidate.candidate_hash,
                signal_kind: raw_candidate.signal_kind.as_str(),
                workspace: &raw_candidate.workspace,
                stage: AutoRememberStage::Validate,
                status: AutoRememberMemoryStatus::ValidationSkipped,
                retry_count,
                error: Some(skip_reason_text(decision)),
                raw_content: Some(&raw_candidate.raw_content),
                entity_key: raw_candidate.entity_key.as_deref(),
                supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
            });
            if queued.source.is_retry_queue() {
                if let Err(error) =
                    store.delete_retry(&raw_candidate.workspace, &raw_candidate.candidate_hash)
                {
                    result.warnings.push(error);
                }
            }
            return;
        }
        Err(error) => {
            queue_retry_failure(request, store, &raw_candidate, retry_count, error, result);
            return;
        }
    };

    let existing_fragments = {
        let lookup_keywords = candidate_lookup_keywords(&candidate);
        if lookup_keywords.is_empty() {
            Vec::new()
        } else {
            match backend
                .lookup_fragments(
                    &candidate.workspace,
                    candidate.signal_kind.topic(),
                    &lookup_keywords,
                )
                .await
            {
                Ok((fragments, usage)) => {
                    result.token_usage.saturating_add_assign(usage);
                    fragments
                }
                Err(error) => {
                    result.warnings.push(error);
                    Vec::new()
                }
            }
        }
    };
    let active_fragments = existing_fragments
        .into_iter()
        .filter(|fragment| !fragment_is_rejected(fragment))
        .collect::<Vec<_>>();

    if let Some(equivalent_fragment) = active_fragments
        .iter()
        .find(|fragment| fragment_matches_candidate(fragment, &candidate))
    {
        let assertion_status = fragment_assertion_status(equivalent_fragment);
        if assertion_status == Some("inferred") {
            match backend
                .amend_assertion_status(&equivalent_fragment.id, "verified")
                .await
            {
                Ok(token_usage) => {
                    result.token_usage.saturating_add_assign(token_usage);
                    let _ = store.upsert_audit(AutoRememberAuditEntry {
                        turn_id: &request.turn_id,
                        candidate_hash: &candidate.candidate_hash,
                        signal_kind: candidate.signal_kind.as_str(),
                        workspace: &candidate.workspace,
                        stage: AutoRememberStage::Verify,
                        status: AutoRememberMemoryStatus::VerifiedPromoted,
                        retry_count,
                        error: Some("promoted equivalent inferred fragment to verified"),
                        raw_content: Some(&raw_candidate.raw_content),
                        entity_key: raw_candidate.entity_key.as_deref(),
                        supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
                    });
                    if let Err(error) =
                        store.delete_retry(&candidate.workspace, &candidate.candidate_hash)
                    {
                        result.warnings.push(error);
                    }
                    return;
                }
                Err(error) => result.warnings.push(error),
            }
        } else {
            result.duplicate_count += 1;
            let _ = store.upsert_audit(AutoRememberAuditEntry {
                turn_id: &request.turn_id,
                candidate_hash: &candidate.candidate_hash,
                signal_kind: candidate.signal_kind.as_str(),
                workspace: &candidate.workspace,
                stage: AutoRememberStage::Dedupe,
                status: AutoRememberMemoryStatus::DuplicateSkip,
                retry_count,
                error: Some("equivalent fragment already exists"),
                raw_content: Some(&raw_candidate.raw_content),
                entity_key: raw_candidate.entity_key.as_deref(),
                supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
            });
            if let Err(error) = store.delete_retry(&candidate.workspace, &candidate.candidate_hash)
            {
                result.warnings.push(error);
            }
            return;
        }
    }

    let supersedes = active_fragments
        .iter()
        .map(|fragment| fragment.id.clone())
        .collect::<Vec<_>>();

    match backend
        .remember(memento_request_from_candidate(&candidate, supersedes))
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
                stage: AutoRememberStage::Remember,
                status: AutoRememberMemoryStatus::Remembered,
                retry_count,
                error: None,
                raw_content: Some(&raw_candidate.raw_content),
                entity_key: raw_candidate.entity_key.as_deref(),
                supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
            }) {
                result.warnings.push(error);
            }
            if let Err(error) = store.delete_retry(&candidate.workspace, &candidate.candidate_hash)
            {
                result.warnings.push(error);
            }
        }
        Err(error) => {
            queue_retry_failure(request, store, &raw_candidate, retry_count, error, result)
        }
    }
}

fn extract_candidates(request: &AutoRememberTurnRequest) -> Vec<AutoRememberRawCandidate> {
    let workspace_override = std::env::var("MEMENTO_WORKSPACE").ok();
    let workspace = resolve_memento_workspace(
        &request.role_id,
        request.channel_id,
        workspace_override.as_deref(),
    );
    if workspace.trim().is_empty() {
        return Vec::new();
    }

    collect_candidate_units(request)
        .into_iter()
        .filter_map(|unit| candidate_from_unit(&workspace, unit))
        .collect()
}

fn collect_candidate_units(request: &AutoRememberTurnRequest) -> Vec<CandidateUnit> {
    let mut units = Vec::new();
    let mut seen = HashSet::new();

    push_text_units(
        &mut units,
        &mut seen,
        &request.assistant_text,
        SessionTranscriptEventKind::Assistant,
    );
    for event in &request.transcript_events {
        match event.kind {
            SessionTranscriptEventKind::Assistant
            | SessionTranscriptEventKind::ToolResult
            | SessionTranscriptEventKind::Result
            | SessionTranscriptEventKind::Error => {
                if let Some(summary) = event.summary.as_deref() {
                    push_text_units(&mut units, &mut seen, summary, event.kind);
                }
                push_text_units(&mut units, &mut seen, &event.content, event.kind);
            }
            _ => {}
        }
    }

    units
}

fn push_text_units(
    units: &mut Vec<CandidateUnit>,
    seen: &mut HashSet<String>,
    text: &str,
    event_kind: SessionTranscriptEventKind,
) {
    for raw in text.lines() {
        let normalized = normalize_whitespace(raw);
        if normalized.is_empty() {
            continue;
        }
        if !seen.insert(normalized.clone()) {
            continue;
        }
        let mut normalized_tool_evidence = Vec::new();
        if matches!(
            event_kind,
            SessionTranscriptEventKind::ToolResult
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
        ) && looks_like_tool_evidence(&normalized)
        {
            normalized_tool_evidence.push(NormalizedToolEvidence {
                summary: normalized.clone(),
            });
        }
        units.push(CandidateUnit {
            text: normalized.clone(),
            supporting_evidence: vec![normalized],
            normalized_tool_evidence,
        });
    }
}

fn candidate_from_unit(workspace: &str, unit: CandidateUnit) -> Option<AutoRememberRawCandidate> {
    if contains_uncertainty(&unit.text) {
        return None;
    }

    let mut matches = Vec::new();
    if is_confirmed_error_root_cause(&unit.text) {
        matches.push(AutoRememberSignalKind::ConfirmedErrorRootCause);
    }
    if is_technical_decision(&unit.text) {
        matches.push(AutoRememberSignalKind::TechnicalDecision);
    }
    if is_config_change(&unit.text) {
        matches.push(AutoRememberSignalKind::ConfigChange);
    }
    if matches.len() != 1 {
        return None;
    }

    let signal_kind = matches[0];
    if !passes_precision_gate(signal_kind, &unit.text) {
        return None;
    }

    let mut supporting_evidence = unit.supporting_evidence;
    supporting_evidence.extend(
        unit.normalized_tool_evidence
            .into_iter()
            .map(|evidence| evidence.summary),
    );

    let entity_key = if signal_kind == AutoRememberSignalKind::ConfigChange {
        normalized_config_key(&unit.text)
    } else {
        None
    };
    let candidate_hash = hash_candidate(workspace, signal_kind, &unit.text);

    Some(AutoRememberRawCandidate {
        signal_kind,
        raw_content: unit.text,
        supporting_evidence,
        entity_key,
        workspace: workspace.to_string(),
        candidate_hash,
    })
}

fn pre_validate_candidate(
    candidate: &AutoRememberRawCandidate,
    settings: &ResolvedMemorySettings,
) -> AutoRememberDecision {
    if candidate.workspace.trim().is_empty() {
        return AutoRememberDecision::Skip(AutoRememberSkipReason::MissingWorkspace);
    }
    if contains_uncertainty(&candidate.raw_content) {
        return AutoRememberDecision::Skip(AutoRememberSkipReason::Uncertain);
    }
    let rewrite_supported = rewrite_supported_with_config(Some(&settings.auto_remember.improver));

    match candidate.signal_kind {
        AutoRememberSignalKind::ConfigChange => {
            if candidate.entity_key.is_none() {
                return AutoRememberDecision::Skip(AutoRememberSkipReason::MissingEntityKey);
            }
            if is_self_contained_config_change(
                &candidate.raw_content,
                candidate.entity_key.as_deref(),
            ) {
                AutoRememberDecision::StoreDirectly
            } else if rewrite_supported && can_rewrite_config_change(candidate) {
                AutoRememberDecision::RewriteNeeded
            } else {
                AutoRememberDecision::Skip(AutoRememberSkipReason::RewriteUnavailable)
            }
        }
        AutoRememberSignalKind::ConfirmedErrorRootCause => {
            if is_self_contained(&candidate.raw_content)
                && is_atomic_sentence(&candidate.raw_content)
            {
                AutoRememberDecision::StoreDirectly
            } else if rewrite_supported && can_rewrite_root_cause(candidate) {
                AutoRememberDecision::RewriteNeeded
            } else {
                AutoRememberDecision::Skip(if rewrite_supported {
                    AutoRememberSkipReason::ValidatorRejected
                } else {
                    AutoRememberSkipReason::RewriteUnavailable
                })
            }
        }
        AutoRememberSignalKind::TechnicalDecision => {
            if is_self_contained(&candidate.raw_content)
                && is_atomic_sentence(&candidate.raw_content)
            {
                AutoRememberDecision::StoreDirectly
            } else if rewrite_supported && can_rewrite_decision(candidate) {
                AutoRememberDecision::RewriteNeeded
            } else {
                AutoRememberDecision::Skip(if rewrite_supported {
                    AutoRememberSkipReason::ValidatorRejected
                } else {
                    AutoRememberSkipReason::RewriteUnavailable
                })
            }
        }
    }
}

fn build_validated_candidate(
    raw: &AutoRememberRawCandidate,
    content: String,
    keyword_suggestions: &[String],
) -> AutoRememberValidatedCandidate {
    AutoRememberValidatedCandidate {
        signal_kind: raw.signal_kind,
        keywords: build_keywords(
            raw.signal_kind,
            raw.entity_key.as_deref(),
            &content,
            keyword_suggestions,
        ),
        entity_key: raw.entity_key.clone(),
        workspace: raw.workspace.clone(),
        candidate_hash: raw.candidate_hash.clone(),
        content,
    }
}

async fn materialize_candidate(
    raw_candidate: &AutoRememberRawCandidate,
    decision: AutoRememberDecision,
    settings: &ResolvedMemorySettings,
) -> Result<Option<AutoRememberValidatedCandidate>, String> {
    match decision {
        AutoRememberDecision::StoreDirectly => Ok(validate_candidate(
            raw_candidate.signal_kind,
            &raw_candidate.raw_content,
            raw_candidate.entity_key.as_deref(),
        )
        .map(|content| build_validated_candidate(raw_candidate, content, &[]))),
        AutoRememberDecision::RewriteNeeded => {
            let rewritten = improve_candidate_with_config(
                &AutoRememberQualityInput {
                    signal_kind: raw_candidate.signal_kind.as_str().to_string(),
                    raw_content: raw_candidate.raw_content.clone(),
                    supporting_evidence: raw_candidate.supporting_evidence.clone(),
                    entity_key: raw_candidate.entity_key.clone(),
                },
                Some(&settings.auto_remember.improver),
            )
            .await?;

            let content = validate_candidate(
                raw_candidate.signal_kind,
                &rewritten.content,
                raw_candidate.entity_key.as_deref(),
            )
            .ok_or_else(|| "auto-remember rewritten candidate failed validator".to_string())?;

            Ok(Some(build_validated_candidate(
                raw_candidate,
                content,
                &rewritten.keyword_suggestions,
            )))
        }
        AutoRememberDecision::Skip(_) => Ok(None),
    }
}

fn candidate_from_retry_record(
    record: &AutoRememberRetryRecord,
) -> Option<AutoRememberRawCandidate> {
    Some(AutoRememberRawCandidate {
        signal_kind: AutoRememberSignalKind::from_str(&record.signal_kind)?,
        raw_content: record.raw_content.clone(),
        supporting_evidence: record.supporting_evidence.clone(),
        entity_key: record.entity_key.clone(),
        workspace: record.workspace.clone(),
        candidate_hash: record.candidate_hash.clone(),
    })
}

fn queue_retry_failure(
    request: &AutoRememberTurnRequest,
    store: &AutoRememberStore,
    raw_candidate: &AutoRememberRawCandidate,
    current_retry_count: u32,
    error: String,
    result: &mut AutoRememberExecutionResult,
) {
    result.warnings.push(error.clone());
    match store.next_failure_status(&raw_candidate.workspace, &raw_candidate.candidate_hash) {
        Ok((status, retry_count)) => {
            if let Err(store_error) = store.upsert_audit(AutoRememberAuditEntry {
                turn_id: &request.turn_id,
                candidate_hash: &raw_candidate.candidate_hash,
                signal_kind: raw_candidate.signal_kind.as_str(),
                workspace: &raw_candidate.workspace,
                stage: AutoRememberStage::Remember,
                status,
                retry_count,
                error: Some(&error),
                raw_content: Some(&raw_candidate.raw_content),
                entity_key: raw_candidate.entity_key.as_deref(),
                supporting_evidence: Some(raw_candidate.supporting_evidence.as_slice()),
            }) {
                result.warnings.push(store_error);
            }

            if status.suppresses_repeat() {
                if let Err(store_error) =
                    store.delete_retry(&raw_candidate.workspace, &raw_candidate.candidate_hash)
                {
                    result.warnings.push(store_error);
                }
                return;
            }

            if let Err(store_error) = store.upsert_retry(&AutoRememberRetryRecord {
                turn_id: request.turn_id.clone(),
                workspace: raw_candidate.workspace.clone(),
                candidate_hash: raw_candidate.candidate_hash.clone(),
                signal_kind: raw_candidate.signal_kind.as_str().to_string(),
                raw_content: raw_candidate.raw_content.clone(),
                entity_key: raw_candidate.entity_key.clone(),
                supporting_evidence: raw_candidate.supporting_evidence.clone(),
                retry_count: retry_count.max(current_retry_count.saturating_add(1)),
                error: Some(error),
                available_at_ms: store.next_retry_available_at_ms(retry_count),
            }) {
                result.warnings.push(store_error);
            }
        }
        Err(store_error) => result.warnings.push(store_error),
    }
}

fn normalize_keyword_suggestion(value: &str) -> String {
    let token_re = Regex::new(r"[A-Za-z0-9_.:/-]{3,}").unwrap();
    token_re
        .find(&normalize_whitespace(value))
        .map(|matched| matched.as_str().to_ascii_lowercase())
        .unwrap_or_default()
}

fn validate_candidate(
    signal_kind: AutoRememberSignalKind,
    content: &str,
    entity_key: Option<&str>,
) -> Option<String> {
    let content = normalize_whitespace(content);
    if content.is_empty()
        || contains_uncertainty(&content)
        || contains_deixis(&content)
        || !is_atomic_sentence(&content)
        || !passes_precision_gate(signal_kind, &content)
    {
        return None;
    }
    if signal_kind == AutoRememberSignalKind::ConfigChange {
        let key = entity_key?;
        if !content
            .to_ascii_lowercase()
            .contains(&key.to_ascii_lowercase())
        {
            return None;
        }
    }
    Some(ensure_period(&content))
}

fn build_keywords(
    signal_kind: AutoRememberSignalKind,
    entity_key: Option<&str>,
    content: &str,
    keyword_suggestions: &[String],
) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    push_keyword(&mut keywords, &mut seen, signal_kind.as_str().to_string());
    if let Some(entity_key) = entity_key {
        push_keyword(&mut keywords, &mut seen, format!("config-key:{entity_key}"));
        push_keyword(&mut keywords, &mut seen, entity_key.to_string());
    }
    if let Some(canonical_key) = canonical_identity_key(signal_kind, entity_key, content) {
        push_keyword(
            &mut keywords,
            &mut seen,
            format!("canonical-key:{canonical_key}"),
        );
    }

    let token_re = Regex::new(r"[A-Za-z0-9_.:/-]{4,}").unwrap();
    for matched in token_re.find_iter(content) {
        push_keyword(
            &mut keywords,
            &mut seen,
            matched.as_str().to_ascii_lowercase(),
        );
        if keywords.len() >= 6 {
            break;
        }
    }

    for suggestion in keyword_suggestions {
        push_keyword(
            &mut keywords,
            &mut seen,
            normalize_keyword_suggestion(suggestion),
        );
        if keywords.len() >= 8 {
            break;
        }
    }

    keywords
}

fn push_keyword(keywords: &mut Vec<String>, seen: &mut HashSet<String>, value: String) {
    let value = normalize_whitespace(&value);
    if value.is_empty() {
        return;
    }
    if seen.insert(value.clone()) {
        keywords.push(value);
    }
}

fn candidate_lookup_keywords(candidate: &AutoRememberValidatedCandidate) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut seen = HashSet::new();

    match candidate.signal_kind {
        AutoRememberSignalKind::ConfigChange => {
            if let Some(entity_key) = candidate.entity_key.as_deref() {
                push_keyword(&mut keywords, &mut seen, format!("config-key:{entity_key}"));
            }
        }
        AutoRememberSignalKind::ConfirmedErrorRootCause
        | AutoRememberSignalKind::TechnicalDecision => {
            if let Some(canonical_key) = canonical_identity_key(
                candidate.signal_kind,
                candidate.entity_key.as_deref(),
                &candidate.content,
            ) {
                push_keyword(
                    &mut keywords,
                    &mut seen,
                    format!("canonical-key:{canonical_key}"),
                );
            }
        }
    }

    keywords
}

fn canonical_identity_key(
    signal_kind: AutoRememberSignalKind,
    entity_key: Option<&str>,
    content: &str,
) -> Option<String> {
    match signal_kind {
        AutoRememberSignalKind::ConfigChange => entity_key.map(|value| value.to_ascii_lowercase()),
        AutoRememberSignalKind::ConfirmedErrorRootCause => {
            canonical_identity_key_from_tokens("root-cause", content)
        }
        AutoRememberSignalKind::TechnicalDecision => {
            canonical_identity_key_from_tokens("decision", content)
        }
    }
}

fn canonical_identity_key_from_tokens(prefix: &str, content: &str) -> Option<String> {
    let token_re = Regex::new(r"[A-Za-z0-9_.:/-]{3,}|[가-힣]{2,}").unwrap();
    let mut tokens = token_re
        .find_iter(&content.to_ascii_lowercase())
        .map(|matched| normalize_whitespace(matched.as_str()))
        .filter(|token| !token.is_empty())
        .filter(|token| !is_generic_identity_token(token))
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.dedup();
    if tokens.len() < 2 {
        return None;
    }
    tokens.truncate(5);
    Some(format!("{prefix}:{}", tokens.join("|")))
}

fn is_generic_identity_token(token: &str) -> bool {
    matches!(
        token,
        "because"
            | "caused"
            | "cause"
            | "config"
            | "decision"
            | "decided"
            | "due"
            | "error"
            | "failure"
            | "from"
            | "root"
            | "standardize"
            | "technical"
            | "the"
            | "this"
            | "that"
            | "updated"
            | "using"
            | "with"
            | "결정"
            | "구조"
            | "기술"
            | "변경"
            | "설정"
            | "원인"
            | "오류"
            | "이유"
            | "표준"
            | "표준화"
            | "하다"
            | "했다"
    )
}

fn fragment_is_rejected(fragment: &MementoFragmentSummary) -> bool {
    fragment_assertion_status(fragment) == Some("rejected")
}

fn fragment_assertion_status(fragment: &MementoFragmentSummary) -> Option<&str> {
    fragment
        .assertion_status
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn fragment_matches_candidate(
    fragment: &MementoFragmentSummary,
    candidate: &AutoRememberValidatedCandidate,
) -> bool {
    fragment
        .content
        .as_deref()
        .map(|content| equivalent_memory_content(content, &candidate.content))
        .unwrap_or(false)
}

fn equivalent_memory_content(lhs: &str, rhs: &str) -> bool {
    normalize_whitespace(lhs)
        .trim_end_matches('.')
        .eq_ignore_ascii_case(normalize_whitespace(rhs).trim_end_matches('.'))
}

fn memento_request_from_candidate(
    candidate: &AutoRememberValidatedCandidate,
    supersedes: Vec<String>,
) -> MementoRememberRequest {
    MementoRememberRequest {
        content: candidate.content.clone(),
        topic: candidate.signal_kind.topic().to_string(),
        kind: candidate.signal_kind.kind().to_string(),
        keywords: candidate.keywords.clone(),
        importance: Some(candidate.signal_kind.importance()),
        source: Some(AUTO_REMEMBER_SOURCE.to_string()),
        workspace: Some(candidate.workspace.clone()),
        agent_id: Some(AUTO_REMEMBER_AGENT_ID.to_string()),
        assertion_status: Some("inferred".to_string()),
        supersedes,
        ..MementoRememberRequest::default()
    }
}

fn skip_reason_text(decision: AutoRememberDecision) -> &'static str {
    match decision {
        AutoRememberDecision::StoreDirectly => "store_directly",
        AutoRememberDecision::RewriteNeeded => "rewrite_needed_but_invalid",
        AutoRememberDecision::Skip(reason) => match reason {
            AutoRememberSkipReason::Uncertain => "uncertain_candidate",
            AutoRememberSkipReason::MissingEntityKey => "missing_entity_key",
            AutoRememberSkipReason::MissingWorkspace => "missing_workspace",
            AutoRememberSkipReason::ValidatorRejected => "validator_rejected",
            AutoRememberSkipReason::RewriteUnavailable => "rewrite_unavailable",
        },
    }
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

fn contains_deixis(unit: &str) -> bool {
    let lower = unit.to_ascii_lowercase();
    let english = Regex::new(r"(?i)\b(this|that|these|those|it|they|them|he|she)\b").unwrap();
    english.is_match(unit)
        || [
            "그것",
            "이것",
            "저것",
            "이 에러",
            "그 에러",
            "이 구조",
            "그 구조",
            "저 구조",
            "이 설정",
            "그 설정",
            "이 변경",
            "그 변경",
            "이 값",
            "그 값",
            "이 문제",
            "그 문제",
            "이번",
            "위에서",
            "앞서",
            "해당",
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
    let has_transition =
        parse_config_change_from_to(unit).is_some() || unit.contains("->") || unit.contains("=>");
    let has_assignment_change = parse_config_assignment_value(unit).is_some() && has_change_verb;

    normalized_config_key(unit).is_some() && (has_transition || has_assignment_change)
}

fn passes_precision_gate(signal_kind: AutoRememberSignalKind, unit: &str) -> bool {
    match signal_kind {
        AutoRememberSignalKind::ConfirmedErrorRootCause => {
            is_confirmed_error_root_cause(unit) && !contains_deixis(unit)
        }
        AutoRememberSignalKind::TechnicalDecision => is_technical_decision(unit),
        AutoRememberSignalKind::ConfigChange => {
            normalized_config_key(unit).is_some()
                && (parse_config_change_from_to(unit).is_some()
                    || unit.contains("->")
                    || unit.contains("=>")
                    || (parse_config_assignment_value(unit).is_some()
                        && [
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
                        .any(|needle| unit.to_ascii_lowercase().contains(needle))))
        }
    }
}

fn is_self_contained(unit: &str) -> bool {
    !contains_deixis(unit) && !contains_uncertainty(unit)
}

fn is_atomic_sentence(unit: &str) -> bool {
    !(unit.contains('\n')
        || unit.contains(';')
        || unit.contains(" 그리고 ")
        || unit.contains(" and ")
        || unit.contains(" / "))
}

fn is_self_contained_config_change(unit: &str, entity_key: Option<&str>) -> bool {
    let Some(entity_key) = entity_key else {
        return false;
    };
    is_self_contained(unit)
        && is_atomic_sentence(unit)
        && unit
            .to_ascii_lowercase()
            .contains(&entity_key.to_ascii_lowercase())
}

fn can_rewrite_config_change(candidate: &AutoRememberRawCandidate) -> bool {
    candidate.entity_key.is_some()
        && (parse_config_change_from_to(&candidate.raw_content).is_some()
            || parse_config_assignment_value(&candidate.raw_content).is_some())
}

fn can_rewrite_root_cause(candidate: &AutoRememberRawCandidate) -> bool {
    passes_precision_gate(candidate.signal_kind, &candidate.raw_content)
        && !contains_deixis(&candidate.raw_content)
}

fn can_rewrite_decision(candidate: &AutoRememberRawCandidate) -> bool {
    passes_precision_gate(candidate.signal_kind, &candidate.raw_content)
        && !contains_deixis(&candidate.raw_content)
}

fn normalized_config_key(unit: &str) -> Option<String> {
    let explicit_key_re =
        Regex::new(r"(?i)([A-Za-z][A-Za-z0-9_.:-]*[._:-][A-Za-z0-9_.:-]+)").unwrap();
    explicit_key_re
        .captures(unit)
        .and_then(|captures| captures.get(1).map(|m| m.as_str().to_ascii_lowercase()))
}

fn parse_config_change_from_to(unit: &str) -> Option<(String, String)> {
    let english_re = Regex::new(
        r"(?i)([A-Za-z][A-Za-z0-9_.:-]*[._:-][A-Za-z0-9_.:-]+).*?\bfrom\b\s+([^\s,.;]+)\s+\bto\b\s+([^\s,.;]+)",
    )
    .unwrap();
    if let Some(captures) = english_re.captures(unit) {
        return Some((
            captures.get(2)?.as_str().to_string(),
            captures.get(3)?.as_str().to_string(),
        ));
    }

    let korean_re = Regex::new(
        r"(?i)([A-Za-z][A-Za-z0-9_.:-]*[._:-][A-Za-z0-9_.:-]+)(?:를|을|은|는|이|가)?\s+([^\s,.;]+)에서\s+([^\s,.;]+)로",
    )
    .unwrap();
    korean_re.captures(unit).and_then(|captures| {
        Some((
            captures.get(2)?.as_str().to_string(),
            captures.get(3)?.as_str().to_string(),
        ))
    })
}

fn parse_config_assignment_value(unit: &str) -> Option<String> {
    let assignment_re =
        Regex::new(r"(?i)([A-Za-z][A-Za-z0-9_.:-]*[._:-][A-Za-z0-9_.:-]+)\s*=\s*([^\s,.;]+)")
            .unwrap();
    assignment_re
        .captures(unit)
        .and_then(|captures| captures.get(2).map(|m| m.as_str().to_string()))
}

fn looks_like_tool_evidence(unit: &str) -> bool {
    unit.len() <= 200
        && (unit.contains("->")
            || unit.contains("=>")
            || unit.contains(".rs:")
            || unit.contains(".md:")
            || unit.contains('=')
            || unit.contains(" changed ")
            || unit.contains(" updated "))
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

fn ensure_period(value: &str) -> String {
    let value = value.trim();
    if value.ends_with('.') {
        value.to_string()
    } else {
        format!("{value}.")
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::services::discord::runtime_store::lock_test_env;
    use crate::services::discord::settings::ResolvedMemorySettings;
    use crate::services::memory::{
        auto_remember_quality, refresh_backend_health, reset_backend_health_for_tests,
    };

    fn remember_request_from_candidate(
        signal_kind: AutoRememberSignalKind,
        content: &str,
        entity_key: Option<&str>,
    ) -> MementoRememberRequest {
        let candidate = AutoRememberValidatedCandidate {
            signal_kind,
            content: content.to_string(),
            keywords: build_keywords(signal_kind, entity_key, content, &[]),
            entity_key: entity_key.map(ToOwned::to_owned),
            workspace: "agentdesk-default".to_string(),
            candidate_hash: "hash".to_string(),
        };
        memento_request_from_candidate(&candidate, vec!["frag-1".to_string()])
    }

    async fn spawn_local_rewrite_server(
        response_body: &str,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<Vec<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = response_body.to_string();
        let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                let _ = requests_tx.send(Vec::new());
                return;
            };
            let mut buf = [0u8; 32768];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let raw_response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(raw_response.as_bytes()).await;
            let _ = stream.shutdown().await;
            let _ = requests_tx.send(vec![request]);
        });
        (
            format!("http://{addr}/v1/chat/completions"),
            requests_rx,
            handle,
        )
    }

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    fn install_memento_runtime(
        base_url: &str,
        workspace: Option<&str>,
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
        let previous_workspace = std::env::var_os("MEMENTO_WORKSPACE");
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("agentdesk.yaml"),
            format!(
                "server:\n  port: 8791\nmemory:\n  backend: memento\n  auto_remember:\n    enabled: true\n  mcp:\n    endpoint: {base_url}\n    access_key_env: MEMENTO_TEST_KEY\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
            std::env::set_var("MEMENTO_TEST_KEY", "memento-key");
        }
        match workspace {
            Some(workspace) => unsafe { std::env::set_var("MEMENTO_WORKSPACE", workspace) },
            None => unsafe { std::env::remove_var("MEMENTO_WORKSPACE") },
        }
        (guard, temp, previous_root, previous_key, previous_workspace)
    }

    struct MockHttpResponse {
        status_line: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    async fn spawn_response_sequence_server(
        responses: Vec<MockHttpResponse>,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<Vec<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 32768];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());

                let mut raw_response = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
                    response.status_line,
                    response.body.len()
                );
                for (header, value) in response.headers {
                    raw_response.push_str(&format!("{header}: {value}\r\n"));
                }
                raw_response.push_str("\r\n");
                raw_response.push_str(&response.body);

                let _ = stream.write_all(raw_response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
            let _ = requests_tx.send(requests);
        });
        (format!("http://{}", addr), requests_rx, handle)
    }

    #[test]
    fn extracts_confirmed_error_root_cause_from_assistant_line() {
        let request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: "왜 실패했어?".to_string(),
            assistant_text: "실패 원인은 MCP 세션 ID 누락 때문이다.".to_string(),
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
    fn config_change_requires_explicit_key_and_transition() {
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
        assert_eq!(candidates[0].entity_key.as_deref(), Some("memory.backend"));
        assert_eq!(
            candidates[0].signal_kind,
            AutoRememberSignalKind::ConfigChange
        );
    }

    #[tokio::test]
    async fn quality_improver_local_llm_uses_json_contract() {
        let _guard = lock_test_env();
        let response = json!({
            "choices": [
                {
                    "message": {
                        "content": json!({
                            "schema_version": "1",
                            "content": "Config key memory.backend changed from file to memento.",
                            "keywords": ["memory.backend", "memento"]
                        }).to_string()
                    }
                }
            ]
        })
        .to_string();
        let (base_url, requests_rx, handle) = spawn_local_rewrite_server(&response).await;
        let previous_base_url = std::env::var_os("AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL");
        let previous_model = std::env::var_os("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL");
        let previous_backend = std::env::var_os("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND");
        unsafe {
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL", &base_url);
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL", "qwen3-local");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", "local_llm");
        }

        let rewritten = auto_remember_quality::improve_candidate(
            &auto_remember_quality::AutoRememberQualityInput {
                signal_kind: "config_change".to_string(),
                raw_content: "memory.backend를 file에서 memento로 변경했다".to_string(),
                supporting_evidence: vec![
                    "memory.backend를 file에서 memento로 변경했다".to_string(),
                ],
                entity_key: Some("memory.backend".to_string()),
            },
        )
        .await
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten.content,
            "Config key memory.backend changed from file to memento."
        );
        assert!(
            rewritten
                .keyword_suggestions
                .iter()
                .any(|keyword| keyword == "memory.backend")
        );

        let requests = tokio::time::timeout(Duration::from_secs(1), requests_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].contains("\"model\":\"qwen3-local\""));
        assert!(requests[0].contains("\"response_format\":{\"type\":\"json_object\"}"));
        assert!(requests[0].contains("memory.backend"));

        handle.await.unwrap();
        restore_env("AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL", previous_base_url);
        restore_env("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL", previous_model);
        restore_env("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", previous_backend);
    }

    #[test]
    fn quality_improver_agent_backend_accepts_provider_override() {
        let _guard = lock_test_env();
        let previous_backend = std::env::var_os("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND");
        let previous_provider = std::env::var_os("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER");
        let previous_model = std::env::var_os("AGENTDESK_AUTO_REMEMBER_AGENT_MODEL");
        let previous_label = std::env::var_os("AGENTDESK_AUTO_REMEMBER_AGENT_LABEL");
        unsafe {
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", "agent");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER", "gemini");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_AGENT_MODEL", "gemini-2.5-flash");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_AGENT_LABEL", "memory-critic");
        }

        let config = auto_remember_quality::agent_rewrite_runtime_config_for_tests().unwrap();
        assert_eq!(config.0, crate::services::provider::ProviderKind::Gemini);
        assert_eq!(config.1.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(config.2, "memory-critic");
        assert_eq!(config.3, Duration::from_secs(45));

        auto_remember_quality::set_test_agent_improver(Some(Box::new(
            |provider, model, prompt, _timeout, label| {
                assert_eq!(provider, crate::services::provider::ProviderKind::Gemini);
                assert_eq!(model.as_deref(), Some("gemini-2.5-flash"));
                assert!(prompt.contains("memory.backend"));
                assert!(label.contains("memory-critic"));
                Ok(json!({
                    "schema_version": "1",
                    "content": "Config key memory.backend changed from file to memento.",
                    "keywords": ["memory.backend", "memento"]
                })
                .to_string())
            },
        )));

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let rewritten = runtime
            .block_on(auto_remember_quality::improve_candidate(
                &auto_remember_quality::AutoRememberQualityInput {
                    signal_kind: "config_change".to_string(),
                    raw_content: "memory.backend를 file에서 memento로 변경했다".to_string(),
                    supporting_evidence: vec![
                        "memory.backend를 file에서 memento로 변경했다".to_string(),
                    ],
                    entity_key: Some("memory.backend".to_string()),
                },
            ))
            .unwrap();
        assert_eq!(
            rewritten.content,
            "Config key memory.backend changed from file to memento."
        );

        auto_remember_quality::set_test_agent_improver(None);
        restore_env("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", previous_backend);
        restore_env("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER", previous_provider);
        restore_env("AGENTDESK_AUTO_REMEMBER_AGENT_MODEL", previous_model);
        restore_env("AGENTDESK_AUTO_REMEMBER_AGENT_LABEL", previous_label);
    }

    #[test]
    fn quality_improver_none_mode_disables_rewrite() {
        let _guard = lock_test_env();
        let previous_backend = std::env::var_os("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND");
        unsafe {
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", "none");
        }

        assert_eq!(
            auto_remember_quality::configured_backend_for_tests(),
            auto_remember_quality::AutoRememberImproverBackend::None
        );
        assert!(!auto_remember_quality::rewrite_supported());

        restore_env("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", previous_backend);
    }

    #[tokio::test]
    async fn quality_improver_both_mode_falls_back_to_agent() {
        let _guard = lock_test_env();
        let previous_base_url = std::env::var_os("AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL");
        let previous_local_model = std::env::var_os("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL");
        let previous_backend = std::env::var_os("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND");
        let previous_provider = std::env::var_os("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER");
        unsafe {
            std::env::set_var(
                "AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL",
                "http://192.168.0.10:1234/v1/chat/completions",
            );
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL", "qwen3-local");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", "both");
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER", "codex");
        }

        auto_remember_quality::set_test_agent_improver(Some(Box::new(
            |provider, model, prompt, _timeout, label| {
                assert_eq!(provider, crate::services::provider::ProviderKind::Codex);
                assert!(model.is_none());
                assert!(prompt.contains("memory.backend"));
                assert!(label.contains("codex"));
                Ok(json!({
                    "schema_version": "1",
                    "content": "Config key memory.backend changed from file to memento.",
                    "keywords": ["memory.backend", "memento"]
                })
                .to_string())
            },
        )));

        let rewritten = auto_remember_quality::improve_candidate(
            &auto_remember_quality::AutoRememberQualityInput {
                signal_kind: "config_change".to_string(),
                raw_content: "memory.backend를 file에서 memento로 변경했다".to_string(),
                supporting_evidence: vec![
                    "memory.backend를 file에서 memento로 변경했다".to_string(),
                ],
                entity_key: Some("memory.backend".to_string()),
            },
        )
        .await
        .expect("both-mode fallback should succeed");

        assert_eq!(
            rewritten.content,
            "Config key memory.backend changed from file to memento."
        );

        auto_remember_quality::set_test_agent_improver(None);
        restore_env("AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL", previous_base_url);
        restore_env("AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL", previous_local_model);
        restore_env("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", previous_backend);
        restore_env("AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER", previous_provider);
    }

    #[tokio::test]
    async fn retry_queue_retries_without_candidate_reappearing() {
        let response_health = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: "{}".to_string(),
        };
        let response_initialize_1 = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-1")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall_1 = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "{\"fragments\":[]}"
                    }],
                    "usage": { "input_tokens": 3, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_remember_1 = MockHttpResponse {
            status_line: "500 Internal Server Error",
            headers: vec![],
            body: "{\"error\":\"temporary failure\"}".to_string(),
        };
        let response_initialize_2 = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-2")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall_2 = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "{\"fragments\":[]}"
                    }],
                    "usage": { "input_tokens": 3, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_remember_2 = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{ "type": "text", "text": "{\"success\":true}" }],
                    "usage": { "input_tokens": 10, "output_tokens": 5 }
                }
            })
            .to_string(),
        };
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            response_health,
            response_initialize_1,
            response_recall_1,
            response_remember_1,
            response_initialize_2,
            response_recall_2,
            response_remember_2,
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, Some("agentdesk-default"));
        let previous_backend = std::env::var_os("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND");
        reset_backend_health_for_tests();
        let snapshot = refresh_backend_health("auto-remember-retry-test").await;
        assert!(snapshot.memento.active);
        unsafe {
            std::env::set_var("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", "local_llm");
        }

        let settings = ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            auto_remember_enabled: true,
            ..ResolvedMemorySettings::default()
        };
        let first_request = AutoRememberTurnRequest {
            turn_id: "turn-1".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text:
                "AgentDesk decided to standardize on SQLite sidecar as the audit store.".to_string(),
            transcript_events: Vec::new(),
        };

        let first_result = run_auto_remember(&settings, first_request).await;
        assert_eq!(first_result.remembered_count, 0);
        assert!(!first_result.warnings.is_empty());

        let store = AutoRememberStore::open_existing()
            .unwrap()
            .expect("retry sidecar should exist after failed remember");
        let mut retry_record = store
            .load_resubmittable_candidate(
                "agentdesk-default",
                &hash_candidate(
                    "agentdesk-default",
                    AutoRememberSignalKind::TechnicalDecision,
                    "AgentDesk decided to standardize on SQLite sidecar as the audit store.",
                ),
            )
            .unwrap()
            .expect("retry candidate should be recoverable from audit");
        retry_record.available_at_ms = chrono::Utc::now().timestamp_millis();
        store.upsert_retry(&retry_record).unwrap();

        let second_request = AutoRememberTurnRequest {
            turn_id: "turn-2".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text: String::new(),
            transcript_events: Vec::new(),
        };
        let second_result = run_auto_remember(&settings, second_request).await;
        assert_eq!(second_result.remembered_count, 1);
        assert_eq!(store.load_retry_batch().unwrap().len(), 0);

        let requests = tokio::time::timeout(Duration::from_secs(1), request_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 7);

        handle.await.unwrap();
        restore_env("AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND", previous_backend);
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_backend_health_for_tests();
    }

    #[tokio::test]
    async fn technical_decision_recall_result_is_superseded_on_new_write() {
        let response_health = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: "{}".to_string(),
        };
        let response_initialize = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-supersede")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&json!({
                            "fragments": [{
                                "id": "frag-old",
                                "content": "AgentDesk decided to standardize on JSONL sidecar as the audit store.",
                                "assertionStatus": "verified"
                            }]
                        })).unwrap()
                    }],
                    "usage": { "input_tokens": 4, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_remember = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{ "type": "text", "text": "{\"success\":true}" }],
                    "usage": { "input_tokens": 8, "output_tokens": 3 }
                }
            })
            .to_string(),
        };
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            response_health,
            response_initialize,
            response_recall,
            response_remember,
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, Some("agentdesk-default"));
        reset_backend_health_for_tests();
        let snapshot = refresh_backend_health("auto-remember-supersede-test").await;
        assert!(snapshot.memento.active);

        let settings = ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            auto_remember_enabled: true,
            ..ResolvedMemorySettings::default()
        };
        let request = AutoRememberTurnRequest {
            turn_id: "turn-supersede".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text:
                "AgentDesk decided to standardize on SQLite sidecar as the audit store.".to_string(),
            transcript_events: Vec::new(),
        };

        let result = run_auto_remember(&settings, request).await;
        assert_eq!(result.remembered_count, 1);
        assert!(result.warnings.is_empty());

        let requests = tokio::time::timeout(Duration::from_secs(1), request_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("\"name\":\"recall\""));
        assert!(requests[3].contains("\"name\":\"remember\""));
        assert!(requests[3].contains("\"supersedes\":[\"frag-old\"]"));

        handle.await.unwrap();
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_backend_health_for_tests();
    }

    #[tokio::test]
    async fn inferred_fragment_is_promoted_to_verified_without_duplicate_write() {
        let response_health = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: "{}".to_string(),
        };
        let response_initialize = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-verify")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string(&json!({
                            "fragments": [{
                                "id": "frag-inferred",
                                "content": "AgentDesk decided to standardize on SQLite sidecar as the audit store.",
                                "assertionStatus": "inferred"
                            }]
                        })).unwrap()
                    }],
                    "usage": { "input_tokens": 4, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_amend = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{ "type": "text", "text": "{\"success\":true}" }],
                    "usage": { "input_tokens": 6, "output_tokens": 2 }
                }
            })
            .to_string(),
        };
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            response_health,
            response_initialize,
            response_recall,
            response_amend,
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, Some("agentdesk-default"));
        reset_backend_health_for_tests();
        let snapshot = refresh_backend_health("auto-remember-verify-test").await;
        assert!(snapshot.memento.active);

        let settings = ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            auto_remember_enabled: true,
            ..ResolvedMemorySettings::default()
        };
        let request = AutoRememberTurnRequest {
            turn_id: "turn-verify".to_string(),
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            user_text: String::new(),
            assistant_text:
                "AgentDesk decided to standardize on SQLite sidecar as the audit store.".to_string(),
            transcript_events: Vec::new(),
        };

        let result = run_auto_remember(&settings, request).await;
        assert_eq!(result.remembered_count, 0);
        assert_eq!(result.duplicate_count, 0);
        assert!(result.warnings.is_empty());

        let store = AutoRememberStore::open_existing()
            .unwrap()
            .expect("verify promotion should create sidecar");
        let counts = store.count_by_status(Some("agentdesk-default")).unwrap();
        assert!(
            counts
                .iter()
                .any(|(status, count)| status == "verified_promoted" && *count == 1)
        );

        let requests = tokio::time::timeout(Duration::from_secs(1), request_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("\"name\":\"recall\""));
        assert!(requests[3].contains("\"name\":\"amend\""));
        assert!(requests[3].contains("\"id\":\"frag-inferred\""));
        assert!(requests[3].contains("\"assertionStatus\":\"verified\""));

        handle.await.unwrap();
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_backend_health_for_tests();
    }

    #[tokio::test]
    async fn manual_resubmit_cli_replays_abandoned_candidate() {
        let response_health = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: "{}".to_string(),
        };
        let response_initialize = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-resubmit")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "{\"fragments\":[]}"
                    }],
                    "usage": { "input_tokens": 3, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_remember = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{ "type": "text", "text": "{\"success\":true}" }],
                    "usage": { "input_tokens": 7, "output_tokens": 2 }
                }
            })
            .to_string(),
        };
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            response_health,
            response_initialize,
            response_recall,
            response_remember,
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, Some("agentdesk-default"));
        reset_backend_health_for_tests();
        let snapshot = refresh_backend_health("auto-remember-resubmit-test").await;
        assert!(snapshot.memento.active);

        let workspace = "agentdesk-default";
        let raw_content = "AgentDesk decided to standardize on SQLite sidecar as the audit store.";
        let candidate_hash = hash_candidate(
            workspace,
            AutoRememberSignalKind::TechnicalDecision,
            raw_content,
        );
        let store = AutoRememberStore::open().unwrap();
        let evidence = vec![raw_content.to_string()];
        store
            .upsert_audit(AutoRememberAuditEntry {
                turn_id: "turn-abandoned",
                candidate_hash: &candidate_hash,
                signal_kind: AutoRememberSignalKind::TechnicalDecision.as_str(),
                workspace,
                stage: AutoRememberStage::Remember,
                status: AutoRememberMemoryStatus::AbandonedAfterRetries,
                retry_count: 3,
                error: Some("temporary failure"),
                raw_content: Some(raw_content),
                entity_key: None,
                supporting_evidence: Some(evidence.as_slice()),
            })
            .unwrap();

        crate::cli::auto_remember::cmd_auto_remember_resubmit(workspace, &candidate_hash)
            .await
            .unwrap();
        assert!(
            store
                .load_resubmittable_candidate(workspace, &candidate_hash)
                .unwrap()
                .is_none()
        );

        let requests = tokio::time::timeout(Duration::from_secs(1), request_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("\"name\":\"recall\""));
        assert!(requests[3].contains("\"name\":\"remember\""));

        handle.await.unwrap();
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_backend_health_for_tests();
    }

    #[tokio::test]
    async fn manual_resubmit_cli_uses_runtime_improver_configuration() {
        let response_health = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: "{}".to_string(),
        };
        let response_initialize = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![("MCP-Session-Id", "session-resubmit-runtime")],
            body: json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "protocolVersion": "2025-11-25", "capabilities": {} }
            })
            .to_string(),
        };
        let response_recall = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": "{\"fragments\":[]}"
                    }],
                    "usage": { "input_tokens": 3, "output_tokens": 1 }
                }
            })
            .to_string(),
        };
        let response_remember = MockHttpResponse {
            status_line: "200 OK",
            headers: vec![],
            body: json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{ "type": "text", "text": "{\"success\":true}" }],
                    "usage": { "input_tokens": 7, "output_tokens": 2 }
                }
            })
            .to_string(),
        };
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            response_health,
            response_initialize,
            response_recall,
            response_remember,
        ])
        .await;
        let (_guard, temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, Some("agentdesk-default"));
        std::fs::write(
            temp.path().join("config").join("agentdesk.yaml"),
            format!(
                "server:\n  port: 8791\nmemory:\n  backend: memento\n  auto_remember:\n    enabled: true\n    improver:\n      mode: agent\n      agent:\n        provider: gemini\n        model: gemini-2.5-pro\n        label: runtime-memory-critic\n  mcp:\n    endpoint: {base_url}\n    access_key_env: MEMENTO_TEST_KEY\n"
            ),
        )
        .unwrap();
        reset_backend_health_for_tests();
        let snapshot = refresh_backend_health("auto-remember-resubmit-runtime-test").await;
        assert!(snapshot.memento.active);

        auto_remember_quality::set_test_agent_improver(Some(Box::new(
            |provider, model, prompt, _timeout, label| {
                assert_eq!(provider, crate::services::provider::ProviderKind::Gemini);
                assert_eq!(model.as_deref(), Some("gemini-2.5-pro"));
                assert_eq!(label, "runtime-memory-critic");
                assert!(prompt.contains("SQLite sidecar"));
                Ok(json!({
                    "schema_version": "1",
                    "content": "AgentDesk decided to standardize on SQLite sidecar as the audit store.",
                    "keywords": ["sqlite", "sidecar", "audit-store"]
                })
                .to_string())
            },
        )));

        let workspace = "agentdesk-default";
        let raw_content = "AgentDesk decided to standardize on SQLite sidecar as the audit store.";
        let candidate_hash = hash_candidate(
            workspace,
            AutoRememberSignalKind::TechnicalDecision,
            raw_content,
        );
        let store = AutoRememberStore::open().unwrap();
        let evidence = vec![raw_content.to_string()];
        store
            .upsert_audit(AutoRememberAuditEntry {
                turn_id: "turn-abandoned-runtime",
                candidate_hash: &candidate_hash,
                signal_kind: AutoRememberSignalKind::TechnicalDecision.as_str(),
                workspace,
                stage: AutoRememberStage::Remember,
                status: AutoRememberMemoryStatus::AbandonedAfterRetries,
                retry_count: 3,
                error: Some("temporary failure"),
                raw_content: Some(raw_content),
                entity_key: None,
                supporting_evidence: Some(evidence.as_slice()),
            })
            .unwrap();

        crate::cli::auto_remember::cmd_auto_remember_resubmit(workspace, &candidate_hash)
            .await
            .unwrap();
        assert!(
            store
                .load_resubmittable_candidate(workspace, &candidate_hash)
                .unwrap()
                .is_none()
        );

        let requests = tokio::time::timeout(Duration::from_secs(1), request_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[2].contains("\"name\":\"recall\""));
        assert!(requests[3].contains("\"name\":\"remember\""));

        auto_remember_quality::set_test_agent_improver(None);
        handle.await.unwrap();
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
        reset_backend_health_for_tests();
    }

    #[test]
    fn candidate_mapping_uses_exact_contract() {
        let root_cause = remember_request_from_candidate(
            AutoRememberSignalKind::ConfirmedErrorRootCause,
            "MCP session ID missing caused the failure.",
            None,
        );
        assert_eq!(root_cause.topic, "error-root-cause");
        assert_eq!(root_cause.kind, "error");
        assert_eq!(root_cause.importance, Some(0.75));
        assert_eq!(root_cause.assertion_status.as_deref(), Some("inferred"));

        let decision = remember_request_from_candidate(
            AutoRememberSignalKind::TechnicalDecision,
            "SQLite sidecar is the standard audit store.",
            None,
        );
        assert_eq!(decision.topic, "technical-decision");
        assert_eq!(decision.kind, "decision");
        assert_eq!(decision.importance, Some(0.70));

        let change = remember_request_from_candidate(
            AutoRememberSignalKind::ConfigChange,
            "Config key memory.backend changed from file to memento.",
            Some("memory.backend"),
        );
        assert_eq!(change.topic, "config-change");
        assert_eq!(change.kind, "fact");
        assert_eq!(change.importance, Some(0.65));
        assert_eq!(change.source.as_deref(), Some(AUTO_REMEMBER_SOURCE));
        assert_eq!(change.agent_id.as_deref(), Some(AUTO_REMEMBER_AGENT_ID));
        assert!(
            change
                .keywords
                .iter()
                .any(|keyword| keyword == "config-key:memory.backend")
        );
        assert_eq!(change.supersedes, vec!["frag-1".to_string()]);
    }

    #[test]
    fn validation_rejects_deictic_sentence() {
        assert!(
            validate_candidate(
                AutoRememberSignalKind::TechnicalDecision,
                "이 구조로 가기로 결정했다.",
                None
            )
            .is_none()
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
            assistant_text: "실패 원인은 MCP 세션 ID 누락 때문이다.".to_string(),
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
            assistant_text: "실패 원인은 MCP 세션 ID 누락 때문이다.".to_string(),
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
    fn retry_limit_constant_matches_spec() {
        assert_eq!(
            crate::services::memory::auto_remember_store::AUTO_REMEMBER_MAX_RETRIES,
            3
        );
    }
}
