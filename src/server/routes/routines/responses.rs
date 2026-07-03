use axum::{Json, http::StatusCode};
use serde_json::{Value, json};

use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::routines::{DeleteRoutineResult, is_resume_routine_requires_next_due_at};

pub(super) fn delete_routine_response(
    routine_id: &str,
    result: DeleteRoutineResult,
) -> AppResult<(StatusCode, Json<Value>)> {
    match result {
        DeleteRoutineResult::Deleted {
            run_history_deleted,
            ..
        } => Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "routine_id": routine_id,
                "run_history_deleted": run_history_deleted,
            })),
        )),
        DeleteRoutineResult::NotFound { .. } => Err(AppError::not_found(format!(
            "routine {routine_id} not found"
        ))),
        DeleteRoutineResult::NotDetached { status, .. } => Err(AppError::conflict(format!(
            "routine {routine_id} must be detached before delete; current status is {status}"
        ))),
        DeleteRoutineResult::InFlight { .. } => Err(AppError::conflict(format!(
            "routine {routine_id} has an in-flight run and cannot be deleted"
        ))),
        DeleteRoutineResult::Forbidden {
            owner,
            caller_agent_id: Some(caller),
        } => Err(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            format!(
                "routine {routine_id} belongs to agent {owner}; caller agent {caller} cannot delete it"
            ),
        )),
        DeleteRoutineResult::Forbidden {
            owner,
            caller_agent_id: None,
        } => Err(AppError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Policy,
            format!(
                "routine {routine_id} belongs to agent {owner}; caller agent scope could not be resolved"
            ),
        )),
    }
}

pub(super) fn store_error(error: anyhow::Error) -> AppError {
    if is_resume_routine_requires_next_due_at(&error) {
        return AppError::conflict(error.to_string());
    }
    AppError::internal(error.to_string()).with_code(ErrorCode::Database)
}

pub(super) fn session_control_error(error: anyhow::Error) -> AppError {
    let message = error.to_string();
    if message.contains("not found") {
        AppError::not_found(message)
    } else if message.contains("not configured")
        || message.contains("not attached")
        || message.contains("invalid")
        || message.contains("requires execution_strategy")
    {
        AppError::conflict(message)
    } else {
        AppError::internal(message)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{delete_routine_response, store_error};
    use crate::error::ErrorCode;
    use crate::services::routines::DeleteRoutineResult;

    #[test]
    fn resume_missing_next_due_store_error_maps_to_conflict() {
        let err =
            store_error(crate::services::routines::store::ResumeRoutineRequiresNextDueAt.into());
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(
            err.message(),
            "next_due_at required to resume schedule-less routine"
        );
    }

    #[test]
    fn delete_routine_response_maps_success_and_conflicts() {
        let (status, body) = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::Deleted {
                run_history_deleted: 2,
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: Some("codex".to_string()),
            },
        )
        .expect("deleted response");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["ok"], true);
        assert_eq!(body.0["routine_id"], "routine-1");
        assert_eq!(body.0["run_history_deleted"], 2);

        let err = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::NotDetached {
                status: "paused".to_string(),
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: Some("codex".to_string()),
            },
        )
        .expect_err("non-detached delete must conflict");
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(
            err.message(),
            "routine routine-1 must be detached before delete; current status is paused"
        );

        let err = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::InFlight {
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: Some("codex".to_string()),
            },
        )
        .expect_err("in-flight delete must conflict");
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert_eq!(
            err.message(),
            "routine routine-1 has an in-flight run and cannot be deleted"
        );
    }

    #[test]
    fn delete_routine_scope_rejects_other_agent() {
        let err = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::Forbidden {
                owner: "codex".to_string(),
                caller_agent_id: Some("claude".to_string()),
            },
        )
        .expect_err("agent-scoped caller must not delete another agent's routine");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.code(), ErrorCode::Policy);
        assert_eq!(
            err.message(),
            "routine routine-1 belongs to agent codex; caller agent claude cannot delete it"
        );
    }

    #[test]
    fn delete_routine_scope_rejects_unresolved_declared_scope() {
        let err = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::Forbidden {
                owner: "codex".to_string(),
                caller_agent_id: None,
            },
        )
        .expect_err("declared but unresolved scope must fail closed");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.code(), ErrorCode::Policy);
        assert_eq!(
            err.message(),
            "routine routine-1 belongs to agent codex; caller agent scope could not be resolved"
        );
    }

    #[test]
    fn delete_routine_scope_rejects_absent_header_for_owned_routine() {
        let err = delete_routine_response(
            "routine-1",
            DeleteRoutineResult::Forbidden {
                owner: "codex".to_string(),
                caller_agent_id: None,
            },
        )
        .expect_err("absent caller scope must fail closed for owned routines");
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
        assert_eq!(err.code(), ErrorCode::Policy);
        assert_eq!(
            err.message(),
            "routine routine-1 belongs to agent codex; caller agent scope could not be resolved"
        );
    }
}
