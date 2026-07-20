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
use crate::error::{AppError, AppResult, ErrorCode};

const ALL_PROVIDERS: &[&str] = &["codex", "claude", "gemini", "qwen"];

/// GET /api/provider-cli — current registry channels + migration states.
pub async fn get_provider_cli_status(
    State(_state): State<AppState>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let root = crate::config::runtime_root().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "runtime root not configured",
        )
    })?;

    let registry = load_registry(&root)
        .map_err(|e| AppError::internal(format!("load registry: {e}")))?
        .unwrap_or_default();

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

    let value = serde_json::to_value(&response)
        .map_err(|e| AppError::internal(format!("serialize: {e}")))?;
    Ok((StatusCode::OK, Json(value)))
}

/// PATCH /api/provider-cli/{provider} — apply action to migration state.
///
/// Accepted actions: "confirm_promote", "rollback", "rollback_to_previous".
pub async fn patch_provider_cli(
    State(_state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<ProviderCliActionRequest>,
) -> AppResult<(StatusCode, Json<Value>)> {
    let action = match body.action.as_str() {
        "confirm_promote" => ProviderCliApiAction::ConfirmPromote,
        "rollback" => ProviderCliApiAction::Rollback,
        "rollback_to_previous" => ProviderCliApiAction::RollbackToPrevious,
        action => return Err(AppError::bad_request(format!("unknown action: {action}"))),
    };

    if !is_supported_provider(&provider) {
        return Err(AppError::bad_request(format!(
            "unsupported provider: {provider}"
        )));
    }

    let root = crate::config::runtime_root().ok_or_else(|| {
        AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Config,
            "runtime root not configured",
        )
    })?;

    let mut migration = load_migration_state(&root, &provider)
        .map_err(|e| AppError::internal(format!("load migration state: {e}")))?
        .ok_or_else(|| {
            AppError::not_found(format!("no migration state for provider: {provider}"))
        })?;

    if matches!(action, ProviderCliApiAction::ConfirmPromote)
        && migration.state == MigrationState::ProviderAgentsMigrated
    {
        return Ok((
            StatusCode::OK,
            Json(json!({
                "provider": provider,
                "action": body.action,
                "state": migration_state_wire_value(&migration.state),
                "updated_at": migration.updated_at,
            })),
        ));
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

    transition_result.map_err(|(status, message)| {
        AppError::new(
            status,
            if status == StatusCode::UNPROCESSABLE_ENTITY {
                ErrorCode::Validation
            } else {
                ErrorCode::Internal
            },
            message,
        )
    })?;

    save_migration_state(&root, &migration)
        .map_err(|e| AppError::internal(format!("save migration state: {e}")))?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "provider": provider,
            "action": body.action,
            "state": migration_state_wire_value(&migration.state),
            "updated_at": migration.updated_at,
        })),
    ))
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
