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
    PrTracking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlGuardViolation {
    target: SqlGuardTarget,
}

impl SqlGuardViolation {
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
            SqlGuardTarget::PrTracking => {
                "Direct pr_tracking mutation is blocked. Use agentdesk.prTracking methods instead."
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
            SqlGuardTarget::PrTracking => "pr_tracking",
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
            SqlGuardTarget::PrTracking => "agentdesk.prTracking methods instead.",
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

    // A broken guard expression must reject raw SQL rather than allow writes.
    let tracking_write = match pr_tracking_mutation_re() {
        Ok(pattern) => pattern.is_match(sql),
        Err(_) => true,
    };
    if tracking_write {
        return Some(SqlGuardViolation {
            target: SqlGuardTarget::PrTracking,
        });
    }

    None
}

fn sql_snippet(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= 120 {
        compact
    } else {
        let end = crate::utils::format::floor_char_boundary(&compact, 117);
        format!("{}...", &compact[..end])
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

fn pr_tracking_mutation_re() -> &'static Result<Regex, regex::Error> {
    static RE: OnceLock<Result<Regex, regex::Error>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(?:INSERT(?:\s+OR\s+\w+)?\s+INTO|REPLACE\s+INTO|MERGE\s+INTO|UPDATE(?:\s+ONLY)?|DELETE\s+FROM(?:\s+ONLY)?|TRUNCATE(?:\s+TABLE)?)\s+(?:["`]?\w+["`]?\s*\.\s*)?["`]?pr_tracking\b"#,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{SqlGuardTarget, detect_core_table_write, pr_tracking_mutation_re};

    #[test]
    fn pr_tracking_guard_expression_compiles() {
        assert!(pr_tracking_mutation_re().is_ok());
    }

    #[test]
    fn pr_tracking_guard_rejects_mutations() {
        for sql in [
            "UPDATE public.pr_tracking SET state = 'merged'",
            "UPDATE \"public\".\"pr_tracking\" SET state = 'merged'",
            "UPDATE ONLY pr_tracking SET state = 'merged'",
            "DELETE FROM ONLY public.pr_tracking WHERE card_id = 'card'",
            "MERGE INTO public.pr_tracking USING incoming ON false WHEN NOT MATCHED THEN INSERT (card_id) VALUES ('card')",
            "TRUNCATE pr_tracking",
            "TRUNCATE TABLE \"public\".\"pr_tracking\"",
            "WITH removed AS (DELETE FROM pr_tracking RETURNING *) SELECT * FROM removed",
        ] {
            assert_eq!(
                detect_core_table_write(sql).map(|violation| violation.target),
                Some(SqlGuardTarget::PrTracking),
                "unblocked tracking mutation: {sql}"
            );
        }
    }

    #[test]
    fn pr_tracking_guard_allows_unrelated_writes_and_reads() {
        for sql in [
            "UPDATE pr_tracking_archive SET state = 'merged'",
            "SELECT * FROM (SELECT card_id FROM public.pr_tracking) AS tracking",
            "UPDATE kanban_cards SET title = 'renamed' WHERE id = 'card'",
        ] {
            assert_eq!(detect_core_table_write(sql), None, "blocked SQL: {sql}");
        }
    }
}
