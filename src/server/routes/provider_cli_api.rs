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
    canary_promotion_evidence, clear_provider_channel_overrides, evaluate_provider_session_guard,
    promote_registry_candidate, rollback_registry_previous, session_guard_evidence,
};
use crate::services::provider_cli::registry::MigrationState;
use crate::services::provider_cli::upgrade::{migration_state_rank, transition};
use crate::services::provider_cli::{
    MigrationDiagnostics, ProviderCliActionRequest, ProviderCliStatusResponse, ProviderDiagnostics,
    migration_state_wire_value,
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
                state: migration_state_wire_value(&ms.state),
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
    let action = match body.action.as_str() {
        "confirm_promote" => ProviderCliApiAction::ConfirmPromote,
        "rollback" => ProviderCliApiAction::Rollback,
        "rollback_to_previous" => ProviderCliApiAction::RollbackToPrevious,
        action => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown action: {action}")})),
            );
        }
    };

    if !is_supported_provider(&provider) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("unsupported provider: {provider}")})),
        );
    }

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

    if matches!(action, ProviderCliApiAction::ConfirmPromote)
        && migration.state == MigrationState::ProviderAgentsMigrated
    {
        return (
            StatusCode::OK,
            Json(json!({
                "provider": provider,
                "action": body.action,
                "state": migration_state_wire_value(&migration.state),
                "updated_at": migration.updated_at,
            })),
        );
    }

    let transition_result = if matches!(action, ProviderCliApiAction::ConfirmPromote) {
        let canary_ready_result = if migration.state == MigrationState::CanaryActive {
            canary_promotion_evidence(&root, &migration, body.evidence.as_deref())
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))
                .and_then(|canary_evidence| {
                    advance_to(
                        &mut migration,
                        MigrationState::CanaryPassed,
                        Some(canary_evidence),
                    )
                    .map_err(|e| {
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            format!("invalid transition: {e}"),
                        )
                    })
                })
                .and_then(|_| {
                    advance_to(
                        &mut migration,
                        MigrationState::AwaitingOperatorPromote,
                        body.evidence.clone(),
                    )
                    .map_err(|e| {
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            format!("invalid transition: {e}"),
                        )
                    })
                })
                .and_then(|_| {
                    save_migration_state(&root, &migration).map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("save migration state: {e}"),
                        )
                    })
                })
        } else if migration.state == MigrationState::CanaryPassed {
            advance_to(
                &mut migration,
                MigrationState::AwaitingOperatorPromote,
                body.evidence.clone(),
            )
            .map_err(|e| {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("invalid transition: {e}"),
                )
            })
            .and_then(|_| {
                save_migration_state(&root, &migration).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("save migration state: {e}"),
                    )
                })
            })
        } else if migration.state == MigrationState::AwaitingOperatorPromote {
            Ok(())
        } else {
            Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "promotion requires canary_active, canary_passed, or awaiting_operator_promote state; current state is {:?}",
                    migration.state
                ),
            ))
        };

        let guard = evaluate_provider_session_guard(
            &root,
            &provider,
            migration.selected_agent_id.as_deref(),
            "candidate",
        );

        if let Err(error) = canary_ready_result {
            Err(error)
        } else if guard.is_clear() {
            advance_to(
                &mut migration,
                MigrationState::ProviderSessionsSafeEnding,
                Some(session_guard_evidence(body.evidence.as_deref(), &guard)),
            )
            .map_err(|e| {
                (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("invalid transition: {e}"),
                )
            })
            .and_then(|_| {
                advance_to(
                    &mut migration,
                    MigrationState::ProviderSessionsRecreated,
                    Some(guard.evidence_json()),
                )
                .map_err(|e| {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        format!("invalid transition: {e}"),
                    )
                })
            })
            .and_then(|_| {
                promote_registry_candidate(&root, &provider).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("update provider registry: {e}"),
                    )
                })
            })
            .and_then(|_| {
                advance_to(&mut migration, MigrationState::ProviderAgentsMigrated, None).map_err(
                    |e| {
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            format!("invalid transition: {e}"),
                        )
                    },
                )
            })
            .and_then(|_| {
                save_migration_state(&root, &migration).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("save migration state: {e}"),
                    )
                })
            })
        } else {
            let _ = transition(
                &mut migration,
                MigrationState::Failed,
                Some(guard.evidence_json()),
            );
            let clear_result = clear_provider_channel_overrides(&root, &provider);
            let _ = save_migration_state(&root, &migration);
            if let Err(error) = clear_result {
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("update provider registry: {error}"),
                ))
            } else {
                Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!(
                        "safe session guard blocked promotion: {}",
                        guard.blockers.join("; ")
                    ),
                ))
            }
        }
    } else {
        transition(
            &mut migration,
            MigrationState::RolledBack,
            body.evidence.clone(),
        )
        .map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid transition: {e}"),
            )
        })
        .and_then(|_| {
            if matches!(action, ProviderCliApiAction::RollbackToPrevious) {
                rollback_registry_previous(&root, &provider)
            } else {
                clear_provider_channel_overrides(&root, &provider)
            }
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("update provider registry: {e}"),
                )
            })
        })
    };

    if let Err((status, message)) = transition_result {
        return (status, Json(json!({"error": message})));
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
            "state": migration_state_wire_value(&migration.state),
            "updated_at": migration.updated_at,
        })),
    )
}

fn is_supported_provider(provider: &str) -> bool {
    ALL_PROVIDERS.contains(&provider)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProviderCliApiAction {
    ConfirmPromote,
    Rollback,
    RollbackToPrevious,
}

fn advance_to(
    state: &mut crate::services::provider_cli::ProviderCliMigrationState,
    next: MigrationState,
    evidence: Option<String>,
) -> Result<(), String> {
    if state_is_at_or_past(&state.state, &next) {
        return Ok(());
    }
    transition(state, next, evidence).map_err(|e| e.to_string())
}

fn state_is_at_or_past(current: &MigrationState, next: &MigrationState) -> bool {
    match (migration_state_rank(current), migration_state_rank(next)) {
        (Some(current_rank), Some(next_rank)) => current_rank >= next_rank,
        _ => current == next,
    }
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
        let db = crate::db::test_db();
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
        };
        let (status, _) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_unknown_provider_returns_bad_request_before_file_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "rollback".to_string(),
            evidence: None,
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("../codex".to_string()), Json(body)).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            value["error"].as_str(),
            Some("unsupported provider: ../codex")
        );
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
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["state"].as_str().unwrap(), "provider_agents_migrated");
        let registry = crate::services::provider_cli::io::load_registry(dir.path())
            .unwrap()
            .unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&candidate));
        assert!(channels.agent_overrides.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_confirm_promote_promotes_registry_before_terminal_state_save() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        use crate::services::provider_cli::paths::migration_state_path;
        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliChannel, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::Utc;
        use std::os::unix::fs::PermissionsExt;

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
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(current.clone()),
                candidate: Some(candidate.clone()),
                ..Default::default()
            },
        );
        crate::services::provider_cli::io::save_registry(dir.path(), &registry).unwrap();

        let state_path = migration_state_path(dir.path(), "codex");
        let mut read_only = std::fs::metadata(&state_path).unwrap().permissions();
        read_only.set_mode(0o444);
        std::fs::set_permissions(&state_path, read_only).unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "confirm_promote".to_string(),
            evidence: Some("operator approved".to_string()),
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;

        let mut writable = std::fs::metadata(&state_path).unwrap().permissions();
        writable.set_mode(0o644);
        std::fs::set_permissions(&state_path, writable).unwrap();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|error| error.contains("save migration state"))
        );
        let registry = crate::services::provider_cli::io::load_registry(dir.path())
            .unwrap()
            .unwrap();
        let saved_state =
            crate::services::provider_cli::io::load_migration_state(dir.path(), "codex")
                .unwrap()
                .unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(saved_state.state, MigrationState::AwaitingOperatorPromote);
        assert_eq!(channels.previous.as_ref(), Some(&current));
        assert_eq!(channels.current.as_ref(), Some(&candidate));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn patch_confirm_promote_registry_failure_keeps_migration_retriable() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        use crate::services::provider_cli::paths::registry_path;
        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliChannel, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::Utc;
        use std::os::unix::fs::PermissionsExt;

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
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(current.clone()),
                candidate: Some(candidate),
                ..Default::default()
            },
        );
        crate::services::provider_cli::io::save_registry(dir.path(), &registry).unwrap();

        let registry_path = registry_path(dir.path());
        let mut read_only = std::fs::metadata(&registry_path).unwrap().permissions();
        read_only.set_mode(0o444);
        std::fs::set_permissions(&registry_path, read_only).unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "confirm_promote".to_string(),
            evidence: Some("operator approved".to_string()),
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;

        let mut writable = std::fs::metadata(&registry_path).unwrap().permissions();
        writable.set_mode(0o644);
        std::fs::set_permissions(&registry_path, writable).unwrap();

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|error| error.contains("update provider registry"))
        );
        let saved_state =
            crate::services::provider_cli::io::load_migration_state(dir.path(), "codex")
                .unwrap()
                .unwrap();
        let registry = crate::services::provider_cli::io::load_registry(dir.path())
            .unwrap()
            .unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(saved_state.state, MigrationState::AwaitingOperatorPromote);
        assert_eq!(channels.current.as_ref(), Some(&current));
    }

    #[tokio::test]
    async fn patch_rollback_clears_provider_overrides() {
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
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current.clone()),
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        crate::services::provider_cli::io::save_migration_state(dir.path(), &ms).unwrap();

        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels {
            current: Some(current),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        crate::services::provider_cli::io::save_registry(dir.path(), &registry).unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "rollback".to_string(),
            evidence: Some("operator rollback".to_string()),
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["state"].as_str().unwrap(), "rolled_back");

        let registry = crate::services::provider_cli::io::load_registry(dir.path())
            .unwrap()
            .unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert!(channels.agent_overrides.is_empty());
    }

    #[tokio::test]
    async fn patch_rollback_registry_io_failure_returns_5xx() {
        let dir = tempfile::tempdir().unwrap();
        let _runtime_root = RuntimeRootOverrideGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            MigrationState, ProviderCliChannel, ProviderCliMigrationState,
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
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current),
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        crate::services::provider_cli::io::save_migration_state(dir.path(), &ms).unwrap();
        std::fs::write(dir.path().join("config"), b"not a directory").unwrap();

        let state = make_state();
        let body = ProviderCliActionRequest {
            action: "rollback".to_string(),
            evidence: Some("operator rollback".to_string()),
        };
        let (status, Json(value)) =
            patch_provider_cli(State(state), Path("codex".to_string()), Json(body)).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            value["error"]
                .as_str()
                .is_some_and(|error| error.contains("update provider registry"))
        );
    }
}
