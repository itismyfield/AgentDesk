use sqlx::PgPool;

use super::markers::{ApiFrictionReport, truncate_chars};
use super::storage::SourceContext;
use crate::services::discord::settings::{
    MemoryBackendKind, ResolvedMemorySettings, resolve_memory_settings,
};
use crate::services::memory::{MementoBackend, MementoRememberRequest, TokenUsage};

const MAX_MEMORY_CONTENT_CHARS: usize = 900;

#[derive(Clone, Debug)]
pub(super) struct EventMemoryDraft {
    pub(super) event_id: String,
    pub(super) request: MementoRememberRequest,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ApiFrictionMemorySyncResult {
    pub(super) memory_stored_count: usize,
    pub(super) memory_errors: Vec<String>,
    pub(super) token_usage: TokenUsage,
}

pub(super) async fn sync_event_memory_pg(
    pg_pool: &PgPool,
    memory_settings: &ResolvedMemorySettings,
    drafts: Vec<EventMemoryDraft>,
) -> ApiFrictionMemorySyncResult {
    let memory_backend = match resolve_memory_backend_for_friction(memory_settings) {
        Some(settings) => Some(MementoBackend::new(settings)),
        None => None,
    };

    let mut result = ApiFrictionMemorySyncResult::default();

    for draft in drafts {
        let Some(backend) = memory_backend.as_ref() else {
            mark_event_memory_status_pg(
                pg_pool,
                &draft.event_id,
                "skipped_backend",
                Some("memento backend is not active for API friction".to_string()),
            )
            .await;
            continue;
        };

        match backend.remember(draft.request).await {
            Ok(token_usage) => {
                result.memory_stored_count += 1;
                result.token_usage.saturating_add_assign(token_usage);
                mark_event_memory_status_pg(pg_pool, &draft.event_id, "stored", None).await;
            }
            Err(error) => {
                result.memory_errors.push(error.clone());
                mark_event_memory_status_pg(pg_pool, &draft.event_id, "failed", Some(error)).await;
            }
        }
    }

    result
}

pub(super) fn build_memento_request(
    source_context: &SourceContext,
    report: &ApiFrictionReport,
    fingerprint: &str,
    dispatch_id: Option<&str>,
) -> MementoRememberRequest {
    let source = [
        dispatch_id.map(|value| format!("dispatch:{value}")),
        source_context
            .card_id
            .as_deref()
            .map(|value| format!("card:{value}")),
        source_context
            .issue_number
            .map(|value| format!("issue:{value}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("/");

    let repo_workspace = source_context
        .repo_id
        .as_deref()
        .and_then(|value| value.split('/').next_back())
        .map(crate::services::memory::sanitize_memento_workspace_segment)
        .unwrap_or_else(|| "agentdesk".to_string());

    let content = truncate_chars(
        &format!(
            "API friction on {} ({})\nSummary: {}\nWorkaround: {}\nSuggested fix: {}\nTask: {}",
            report.endpoint,
            report.friction_type,
            report.summary,
            report.workaround.as_deref().unwrap_or("not provided"),
            report.suggested_fix.as_deref().unwrap_or("not provided"),
            source_context
                .task_summary
                .as_deref()
                .unwrap_or("not provided"),
        ),
        MAX_MEMORY_CONTENT_CHARS,
    );

    MementoRememberRequest {
        content,
        topic: "api-friction".to_string(),
        kind: "error".to_string(),
        importance: None,
        keywords: report.keywords.clone(),
        source: (!source.is_empty()).then_some(source),
        workspace: Some(repo_workspace),
        agent_id: Some("default".to_string()),
        case_id: Some(fingerprint.to_string()),
        goal: Some(format!("Reduce API friction for {}", report.endpoint)),
        outcome: Some("observed".to_string()),
        phase: Some("runtime".to_string()),
        resolution_status: Some("open".to_string()),
        assertion_status: Some("reported".to_string()),
        context_summary: Some(report.summary.clone()),
    }
}

fn resolve_memory_backend_for_friction(
    memory_settings: &ResolvedMemorySettings,
) -> Option<ResolvedMemorySettings> {
    if memory_settings.backend == MemoryBackendKind::Memento {
        return Some(memory_settings.clone());
    }
    let resolved = resolve_memory_settings(None, None);
    (resolved.backend == MemoryBackendKind::Memento).then_some(resolved)
}

async fn mark_event_memory_status_pg(
    pg_pool: &PgPool,
    event_id: &str,
    status: &str,
    error: Option<String>,
) {
    let _ = sqlx::query(
        "UPDATE api_friction_events
             SET memory_status = $1, memory_error = $2
             WHERE id = $3",
    )
    .bind(status)
    .bind(error.as_deref())
    .bind(event_id)
    .execute(pg_pool)
    .await;
}
