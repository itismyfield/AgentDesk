use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use serde_json::{Value, json};

use crate::services::provider_cli::io::{
    load_migration_state, load_registry, load_smoke_result, save_migration_state,
};
use crate::services::provider_cli::orchestration::{
    promote_registry_candidate, rollback_registry_previous, session_guard_evidence,
};
use crate::services::provider_cli::registry::MigrationState;
use crate::services::provider_cli::upgrade::transition;
use crate::services::provider_cli::{
    MigrationDiagnostics, ProviderCliActionRequest, ProviderCliStatusResponse, ProviderDiagnostics,
};

use super::AppState;

const ALL_PROVIDERS: &[&str] = &["codex", "claude", "gemini", "qwen"];

/// GET /api/provider-cli — current registry channels + migration states.
pub async fn get_provider_cli_status(State(_state): State<AppState>) -> (StatusCode, Json<Value>) {
    let Some(root) = crate::config::runtime_root() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "runtime root not configured"})),
        );
    };

    let registry = match load_registry(&root) {
        Ok(r) => r.unwrap_or_default(),
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load registry: {e}")})),
            );
        }
    };

    let providers: Vec<ProviderDiagnostics> = ALL_PROVIDERS
        .iter()
        .map(|provider| {
            let channels = registry.providers.get(*provider);
            ProviderDiagnostics {
                provider: provider.to_string(),
                current: channels.and_then(|c| c.current.clone()),
                candidate: channels.and_then(|c| c.candidate.clone()),
                previous: channels.and_then(|c| c.previous.clone()),
                smoke_current: load_smoke_result(&root, provider, "current").ok().flatten(),
                smoke_candidate: load_smoke_result(&root, provider, "candidate")
                    .ok()
                    .flatten(),
                evidence: Default::default(),
            }
        })
        .collect();

    let mut migrations = Vec::new();
    for provider in ALL_PROVIDERS {
        if let Ok(Some(ms)) = load_migration_state(&root, provider) {
            migrations.push(MigrationDiagnostics {
                provider: provider.to_string(),
                state: format!("{:?}", ms.state),
                canary_agent_id: ms.selected_agent_id.clone(),
                started_at: Some(ms.started_at),
                updated_at: Some(ms.updated_at),
                history_len: ms.history.len(),
            });
        }
    }

    let response = ProviderCliStatusResponse {
        providers,
        migrations,
        generated_at: Utc::now(),
    };

    match serde_json::to_value(&response) {
        Ok(v) => (StatusCode::OK, Json(v)),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("serialize: {e}")})),
        ),
    }
}

/// PATCH /api/provider-cli/{provider} — apply action to migration state.
///
/// Accepted actions: "confirm_promote", "rollback", "rollback_to_previous".
pub async fn patch_provider_cli(
    State(_state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<ProviderCliActionRequest>,
) -> (StatusCode, Json<Value>) {
    let confirm_promote = match body.action.as_str() {
        "confirm_promote" => true,
        "rollback" | "rollback_to_previous" => false,
        action => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown action: {action}")})),
            );
        }
    };

    let Some(root) = crate::config::runtime_root() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "runtime root not configured"})),
        );
    };

    let mut migration = match load_migration_state(&root, &provider) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("no migration state for provider: {provider}")})),
            );
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("load migration state: {e}")})),
            );
        }
    };

    let transition_result = if confirm_promote {
        let target_agent_ids: Vec<String> = migration.selected_agent_id.iter().cloned().collect();
        let guard = crate::services::provider_cli::session_guard::evaluate_session_migration_guards(
            &root,
            &provider,
            &target_agent_ids,
            "candidate",
            body.force_recreate_active,
        );

        if guard.is_clear() {
            advance_to(
                &mut migration,
                MigrationState::ProviderSessionsSafeEnding,
                Some(session_guard_evidence(body.evidence.as_deref(), &guard)),
            )
            .and_then(|_| {
                advance_to(
                    &mut migration,
                    MigrationState::ProviderSessionsRecreated,
                    Some(guard.evidence_json()),
                )
            })
            .and_then(|_| advance_to(&mut migration, MigrationState::ProviderAgentsMigrated, None))
            .and_then(|_| promote_registry_candidate(&root, &provider))
        } else {
            let _ = transition(
                &mut migration,
                MigrationState::Failed,
                Some(guard.evidence_json()),
            );
            let _ = save_migration_state(&root, &migration);
            Err(format!(
                "safe session guard blocked promotion: {}",
                guard.blockers.join("; ")
            ))
        }
    } else {
        transition(
            &mut migration,
            MigrationState::RolledBack,
            body.evidence.clone(),
        )
        .map_err(|e| e.to_string())
        .and_then(|_| rollback_registry_previous(&root, &provider))
    };

    if let Err(e) = transition_result {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": format!("invalid transition: {e}")})),
        );
    }

    if let Err(e) = save_migration_state(&root, &migration) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("save migration state: {e}")})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "provider": provider,
            "action": body.action,
            "state": format!("{:?}", migration.state),
            "updated_at": migration.updated_at,
        })),
    )
}

fn advance_to(
    state: &mut crate::services::provider_cli::ProviderCliMigrationState,
    next: MigrationState,
    evidence: Option<String>,
) -> Result<(), String> {
    if state.state == next {
        return Ok(());
    }
    transition(state, next, evidence).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::PolicyEngine;
    use crate::server::routes::AppState;

    struct RuntimeRootOverrideGuard {
        previous: Option<std::path::PathBuf>,
    }

    impl RuntimeRootOverrideGuard {
        fn set(path: &std::path::Path) -> Self {
            let previous = crate::config::current_test_runtime_root_override();
            crate::config::set_test_runtime_root_override(Some(path.to_path_buf()));
            Self { previous }
        }
    }

    impl Drop for RuntimeRootOverrideGuard {
        fn drop(&mut self) {
            crate::config::set_test_runtime_root_override(self.previous.take());
        }
    }

    fn make_state() -> AppState {
        let db = crate::db::init(&crate::config::Config::default()).unwrap();
        AppState::test_state(
            db,
            PolicyEngine::new(&crate::config::Config::default()).unwrap(),
        )
    }

    #[tokio::test]
    async fn get_status_returns_ok_without_runtime_root() {
        // When AGENTDESK_ROOT_DIR is not set and no home dir is mocked,
        // the handler degrades gracefully — either 200 (empty) or 503.
        // We just verify it doesn't panic.
        let state = make_state();
        let (status, _body) = get_provider_cli_status(State(state)).await;
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "unexpected status: {status}"
        );
    }

    #[tokio::test]
    async fn patch_unknown_action_returns_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        // Create a migration state file first.
        use crate::services::provider_cli::registry::{MigrationState, ProviderCliMigrationState};
        use chrono::Utc;
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryPassed,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        crate::services::provider_cli::io::save_migration_state(dir.path(), &ms).unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "invalid_action".to_string(),
            evidence: None,
            force_recreate_active: false,
        };
        let (status, _) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_confirm_promote_transitions_state() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliChannel, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::Utc;
        let current = ProviderCliChannel {
            path: "/tmp/current-codex".to_string(),
            canonical_path: "/tmp/current-codex".to_string(),
            version: "current".to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: Utc::now(),
            evidence: Default::default(),
        };
        let candidate = ProviderCliChannel {
            path: "/tmp/candidate-codex".to_string(),
            canonical_path: "/tmp/candidate-codex".to_string(),
            version: "candidate".to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: Utc::now(),
            evidence: Default::default(),
        };
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: None,
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        crate::services::provider_cli::io::save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels {
            current: Some(current),
            candidate: Some(candidate.clone()),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        crate::services::provider_cli::io::save_registry(dir.path(), &registry).unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "confirm_promote".to_string(),
            evidence: Some("operator approved".to_string()),
            force_recreate_active: false,
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["state"].as_str().unwrap(), "ProviderAgentsMigrated");
        let registry = crate::services::provider_cli::io::load_registry(dir.path())
            .unwrap()
            .unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&candidate));
        assert!(channels.agent_overrides.is_empty());
    }
}
