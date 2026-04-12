use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlGuardTarget {
    KanbanStatus,
    KanbanReviewStatus,
    KanbanLatestDispatchId,
    TaskDispatches,
    CardReviewState,
    AutoQueueEntries,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlGuardViolation {
    target: SqlGuardTarget,
}

impl SqlGuardViolation {
    #[cfg(test)]
    pub fn target(self) -> SqlGuardTarget {
        self.target
    }

    pub fn error_message(self) -> &'static str {
        match self.target {
            SqlGuardTarget::KanbanStatus => {
                "Direct kanban_cards status UPDATE is blocked. Use agentdesk.kanban.setStatus(cardId, newStatus) instead."
            }
            SqlGuardTarget::KanbanReviewStatus => {
                "Direct kanban_cards review_status UPDATE is blocked. Use agentdesk.kanban.setReviewStatus(cardId, status, opts) instead."
            }
            SqlGuardTarget::KanbanLatestDispatchId => {
                "Direct kanban_cards latest_dispatch_id UPDATE is blocked. Use agentdesk.dispatch.create() or related dispatch helpers instead."
            }
            SqlGuardTarget::TaskDispatches => {
                "Direct task_dispatches mutation is blocked. Use agentdesk.dispatch.create()/markFailed()/markCompleted()/setRetryCount() instead."
            }
            SqlGuardTarget::CardReviewState => {
                "Direct card_review_state mutation is blocked. Use agentdesk.reviewState.sync(cardId, state, opts) instead."
            }
            SqlGuardTarget::AutoQueueEntries => {
                "Direct auto_queue_entries mutation is blocked. Use agentdesk.autoQueue.updateEntryStatus(entryId, status, source, opts) instead."
            }
        }
    }

    pub fn warning_message(self, origin: &str, sql: &str) -> String {
        format!(
            "[policy-sql-guard] blocked raw SQL write on {} via {}; use {} sql={}",
            self.target_label(),
            origin,
            self.remediation(),
            sql_snippet(sql)
        )
    }

    fn target_label(self) -> &'static str {
        match self.target {
            SqlGuardTarget::KanbanStatus => "kanban_cards.status",
            SqlGuardTarget::KanbanReviewStatus => "kanban_cards.review_status",
            SqlGuardTarget::KanbanLatestDispatchId => "kanban_cards.latest_dispatch_id",
            SqlGuardTarget::TaskDispatches => "task_dispatches",
            SqlGuardTarget::CardReviewState => "card_review_state",
            SqlGuardTarget::AutoQueueEntries => "auto_queue_entries",
        }
    }

    fn remediation(self) -> &'static str {
        match self.target {
            SqlGuardTarget::KanbanStatus => {
                "agentdesk.kanban.setStatus(cardId, newStatus) instead."
            }
            SqlGuardTarget::KanbanReviewStatus => {
                "agentdesk.kanban.setReviewStatus(cardId, status, opts) instead."
            }
            SqlGuardTarget::KanbanLatestDispatchId => {
                "agentdesk.dispatch.create()/markFailed()/markCompleted() instead."
            }
            SqlGuardTarget::TaskDispatches => {
                "agentdesk.dispatch.create()/markFailed()/markCompleted()/setRetryCount() instead."
            }
            SqlGuardTarget::CardReviewState => {
                "agentdesk.reviewState.sync(cardId, state, opts) instead."
            }
            SqlGuardTarget::AutoQueueEntries => {
                "agentdesk.autoQueue.updateEntryStatus(entryId, status, source, opts) instead."
            }
        }
    }
}

pub fn detect_core_table_write(sql: &str) -> Option<SqlGuardViolation> {
    let sql_upper = sql.to_uppercase();
    if sql_upper.contains("UPDATE")
        && sql_upper.contains("KANBAN_CARDS")
        && update_kanban_cards_re().is_match(sql)
    {
        if status_assign_re().is_match(sql) {
            return Some(SqlGuardViolation {
                target: SqlGuardTarget::KanbanStatus,
            });
        }
        if review_status_assign_re().is_match(sql) {
            return Some(SqlGuardViolation {
                target: SqlGuardTarget::KanbanReviewStatus,
            });
        }
        if latest_dispatch_id_assign_re().is_match(sql) {
            return Some(SqlGuardViolation {
                target: SqlGuardTarget::KanbanLatestDispatchId,
            });
        }
    }

    if task_dispatches_mutation_re().is_match(sql) {
        return Some(SqlGuardViolation {
            target: SqlGuardTarget::TaskDispatches,
        });
    }

    if card_review_state_mutation_re().is_match(sql) {
        return Some(SqlGuardViolation {
            target: SqlGuardTarget::CardReviewState,
        });
    }

    if auto_queue_entries_mutation_re().is_match(sql) {
        return Some(SqlGuardViolation {
            target: SqlGuardTarget::AutoQueueEntries,
        });
    }

    None
}

fn sql_snippet(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= 120 {
        compact
    } else {
        format!("{}...", &compact[..117])
    }
}

fn update_kanban_cards_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bUPDATE\s+kanban_cards\b").unwrap())
}

fn status_assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[\s,])status\s*=").unwrap())
}

fn review_status_assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[\s,])review_status\s*=").unwrap())
}

fn latest_dispatch_id_assign_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(?:^|[\s,])latest_dispatch_id\s*=").unwrap())
}

fn task_dispatches_mutation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:INSERT(?:\s+OR\s+REPLACE)?\s+INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+task_dispatches\b",
        )
        .unwrap()
    })
}

fn card_review_state_mutation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:INSERT(?:\s+OR\s+REPLACE)?\s+INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+card_review_state\b",
        )
        .unwrap()
    })
}

fn auto_queue_entries_mutation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:INSERT(?:\s+OR\s+REPLACE)?\s+INTO|REPLACE\s+INTO|UPDATE|DELETE\s+FROM)\s+auto_queue_entries\b",
        )
        .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::{SqlGuardTarget, detect_core_table_write};

    #[test]
    fn detects_task_dispatches_delete() {
        let violation = detect_core_table_write("DELETE FROM task_dispatches WHERE id = ?")
            .expect("task_dispatches DELETE must be guarded");
        assert_eq!(violation.target(), SqlGuardTarget::TaskDispatches);
        assert!(violation.error_message().contains("task_dispatches"));
        let warning = violation.warning_message(
            "agentdesk.db.execute",
            "DELETE FROM task_dispatches WHERE id = ?",
        );
        assert!(warning.contains("[policy-sql-guard]"));
        assert!(warning.contains("task_dispatches"));
    }

    #[test]
    fn ignores_non_core_table_update() {
        assert!(
            detect_core_table_write("UPDATE kanban_cards SET blocked_reason = 'x' WHERE id = ?")
                .is_none()
        );
    }

    #[test]
    fn detects_auto_queue_entry_update() {
        let violation =
            detect_core_table_write("UPDATE auto_queue_entries SET status = 'done' WHERE id = ?")
                .expect("auto_queue_entries UPDATE must be guarded");
        assert_eq!(violation.target(), SqlGuardTarget::AutoQueueEntries);
        assert!(violation.error_message().contains("auto_queue_entries"));
    }
}
