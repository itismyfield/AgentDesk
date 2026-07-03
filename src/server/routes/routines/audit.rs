use crate::services::routines::DeleteRoutineResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoutineDeleteAuditLevel {
    Info,
    Warn,
}

#[derive(Debug, PartialEq, Eq)]
struct RoutineDeleteAuditRecord {
    outcome: &'static str,
    routine_id: String,
    owner: String,
    caller: String,
    deleted_runs: Option<u64>,
    level: RoutineDeleteAuditLevel,
}

fn audit_agent_label(agent_id: Option<&str>, empty_label: &'static str) -> String {
    agent_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(empty_label)
        .to_string()
}

fn routine_delete_audit_record(
    routine_id: &str,
    result: &DeleteRoutineResult,
) -> RoutineDeleteAuditRecord {
    match result {
        DeleteRoutineResult::Deleted {
            run_history_deleted,
            routine_agent_id,
            caller_agent_id,
        } => RoutineDeleteAuditRecord {
            outcome: "deleted",
            routine_id: routine_id.to_string(),
            owner: audit_agent_label(routine_agent_id.as_deref(), "unowned"),
            caller: audit_agent_label(caller_agent_id.as_deref(), "unresolved"),
            deleted_runs: Some(*run_history_deleted),
            level: RoutineDeleteAuditLevel::Info,
        },
        DeleteRoutineResult::NotFound { caller_agent_id } => RoutineDeleteAuditRecord {
            outcome: "not_found",
            routine_id: routine_id.to_string(),
            owner: "unknown".to_string(),
            caller: audit_agent_label(caller_agent_id.as_deref(), "unresolved"),
            deleted_runs: None,
            level: RoutineDeleteAuditLevel::Warn,
        },
        DeleteRoutineResult::NotDetached {
            routine_agent_id,
            caller_agent_id,
            ..
        } => RoutineDeleteAuditRecord {
            outcome: "not_detached",
            routine_id: routine_id.to_string(),
            owner: audit_agent_label(routine_agent_id.as_deref(), "unowned"),
            caller: audit_agent_label(caller_agent_id.as_deref(), "unresolved"),
            deleted_runs: None,
            level: RoutineDeleteAuditLevel::Warn,
        },
        DeleteRoutineResult::InFlight {
            routine_agent_id,
            caller_agent_id,
        } => RoutineDeleteAuditRecord {
            outcome: "in_flight",
            routine_id: routine_id.to_string(),
            owner: audit_agent_label(routine_agent_id.as_deref(), "unowned"),
            caller: audit_agent_label(caller_agent_id.as_deref(), "unresolved"),
            deleted_runs: None,
            level: RoutineDeleteAuditLevel::Warn,
        },
        DeleteRoutineResult::Forbidden {
            owner,
            caller_agent_id,
        } => RoutineDeleteAuditRecord {
            outcome: "forbidden",
            routine_id: routine_id.to_string(),
            owner: audit_agent_label(Some(owner), "unknown"),
            caller: audit_agent_label(caller_agent_id.as_deref(), "unresolved"),
            deleted_runs: None,
            level: RoutineDeleteAuditLevel::Warn,
        },
    }
}

pub(super) fn audit_routine_delete(routine_id: &str, result: &DeleteRoutineResult) {
    let record = routine_delete_audit_record(routine_id, result);
    match (record.level, record.deleted_runs) {
        (RoutineDeleteAuditLevel::Info, Some(deleted_runs)) => {
            tracing::info!(
                event = "routine_delete_audit",
                outcome = record.outcome,
                routine_id = %record.routine_id,
                owner = %record.owner,
                caller = %record.caller,
                deleted_runs = deleted_runs,
                "routine hard delete audited"
            );
        }
        _ => {
            tracing::warn!(
                event = "routine_delete_audit",
                outcome = record.outcome,
                routine_id = %record.routine_id,
                owner = %record.owner,
                caller = %record.caller,
                "routine hard delete rejected"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RoutineDeleteAuditLevel, routine_delete_audit_record};
    use crate::services::routines::DeleteRoutineResult;

    #[test]
    fn delete_routine_audit_record_covers_all_outcomes() {
        let record = routine_delete_audit_record(
            "routine-1",
            &DeleteRoutineResult::Deleted {
                run_history_deleted: 2,
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: Some("codex".to_string()),
            },
        );
        assert_eq!(record.outcome, "deleted");
        assert_eq!(record.routine_id, "routine-1");
        assert_eq!(record.owner, "codex");
        assert_eq!(record.caller, "codex");
        assert_eq!(record.deleted_runs, Some(2));
        assert_eq!(record.level, RoutineDeleteAuditLevel::Info);

        let record = routine_delete_audit_record(
            "routine-1",
            &DeleteRoutineResult::Forbidden {
                owner: "codex".to_string(),
                caller_agent_id: Some("claude".to_string()),
            },
        );
        assert_eq!(record.outcome, "forbidden");
        assert_eq!(record.owner, "codex");
        assert_eq!(record.caller, "claude");
        assert_eq!(record.deleted_runs, None);
        assert_eq!(record.level, RoutineDeleteAuditLevel::Warn);

        let record = routine_delete_audit_record(
            "routine-1",
            &DeleteRoutineResult::NotDetached {
                status: "paused".to_string(),
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: Some("codex".to_string()),
            },
        );
        assert_eq!(record.outcome, "not_detached");
        assert_eq!(record.owner, "codex");
        assert_eq!(record.caller, "codex");
        assert_eq!(record.deleted_runs, None);
        assert_eq!(record.level, RoutineDeleteAuditLevel::Warn);

        let record = routine_delete_audit_record(
            "routine-1",
            &DeleteRoutineResult::InFlight {
                routine_agent_id: Some("codex".to_string()),
                caller_agent_id: None,
            },
        );
        assert_eq!(record.outcome, "in_flight");
        assert_eq!(record.owner, "codex");
        assert_eq!(record.caller, "unresolved");
        assert_eq!(record.deleted_runs, None);
        assert_eq!(record.level, RoutineDeleteAuditLevel::Warn);

        let record = routine_delete_audit_record(
            "routine-1",
            &DeleteRoutineResult::NotFound {
                caller_agent_id: Some("codex".to_string()),
            },
        );
        assert_eq!(record.outcome, "not_found");
        assert_eq!(record.owner, "unknown");
        assert_eq!(record.caller, "codex");
        assert_eq!(record.deleted_runs, None);
        assert_eq!(record.level, RoutineDeleteAuditLevel::Warn);
    }
}
