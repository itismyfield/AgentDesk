//! Task-notification kind lifecycle + tracked background-child close helpers.
//!
//! #3479 extraction: the byte-identical helpers that maintain the active
//! `TaskNotificationKind` priority (merge/release) and drain the tracked
//! background-child session queue on terminal task notifications. Moved
//! verbatim from `turn_bridge/mod.rs` and re-exported there so call sites
//! stay identical.

use super::*;

pub(in crate::services::discord) fn merge_task_notification_kind(
    current: Option<TaskNotificationKind>,
    new_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };

    match current {
        Some(existing) if priority(existing) >= priority(new_kind) => Some(existing),
        _ => Some(new_kind),
    }
}

pub(in crate::services::discord) fn release_task_notification_kind(
    current: Option<TaskNotificationKind>,
    closed_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    match current {
        Some(existing) if existing == closed_kind => None,
        other => other,
    }
}

pub(in crate::services::discord) async fn close_next_tracked_background_child(
    pg_pool: Option<&sqlx::PgPool>,
    child_session_ids: &mut Vec<i64>,
    status: &str,
    reason: &str,
) {
    let Some(pg_pool) = pg_pool else {
        return;
    };
    if child_session_ids.is_empty() {
        return;
    }
    let child_session_id = child_session_ids.remove(0);
    match close_background_child_pg(pg_pool, child_session_id, status).await {
        Ok(_) => {}
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to close background child session {child_session_id} after {reason}: {error}"
            );
        }
    }
}

pub(in crate::services::discord) async fn close_all_tracked_background_children(
    pg_pool: Option<&sqlx::PgPool>,
    child_session_ids: &mut Vec<i64>,
    status: &str,
    reason: &str,
) {
    while !child_session_ids.is_empty() {
        close_next_tracked_background_child(pg_pool, child_session_ids, status, reason).await;
    }
}

pub(in crate::services::discord) fn task_notification_closes_background_child(
    kind: TaskNotificationKind,
    status: &str,
) -> bool {
    if !matches!(
        kind,
        TaskNotificationKind::Background | TaskNotificationKind::Subagent
    ) {
        return false;
    }
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "completed"
            | "done"
            | "finished"
            | "aborted"
            | "cancelled"
            | "canceled"
            | "failed"
            | "error"
    )
}

/// codex P2 followup (#1670): unit-level coverage of the
/// "preserve task_notification_kind while other children remain" rule.
/// This mirrors the production logic at the
/// `task_notification_closes_background_child` arm in
/// `StreamMessage::TaskNotification` (search: `codex P2 followup`).
///
/// The existing exhaustive-status coverage lives in
/// `tests::task_notification_kind_resets_after_terminal_status` but that
/// module is feature-gated on `legacy-sqlite-tests`; this lighter copy is
/// added in the non-gated mod so the regression is observable in normal
/// `cargo test` runs.
#[cfg(test)]
mod task_notification_kind_lifecycle_tests {
    use super::{
        TaskNotificationKind, merge_task_notification_kind, release_task_notification_kind,
        task_notification_closes_background_child,
    };

    #[test]
    fn kind_persists_while_other_children_remain() {
        let mut child_ids: Vec<i64> = vec![100, 101];
        let mut kind: Option<TaskNotificationKind> = None;
        kind = merge_task_notification_kind(kind, TaskNotificationKind::Subagent);

        let new_kind = TaskNotificationKind::Subagent;
        let status = "completed";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            // Mirror the single-pop behavior of
            // `close_next_tracked_background_child` at the call site.
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert_eq!(child_ids, vec![101]);
        assert_eq!(
            kind,
            Some(TaskNotificationKind::Subagent),
            "#1670 P2: kind must persist while other tracked children remain"
        );

        // Second terminal closes the queue, kind releases.
        let new_kind = TaskNotificationKind::Subagent;
        let status = "completed";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }
        assert!(child_ids.is_empty());
        assert_eq!(
            kind, None,
            "#1670 P2: kind must release once the last child closes"
        );
    }

    #[test]
    fn kind_releases_immediately_when_queue_was_already_singleton() {
        let mut child_ids: Vec<i64> = vec![42];
        let mut kind: Option<TaskNotificationKind> = Some(TaskNotificationKind::Background);

        let new_kind = TaskNotificationKind::Background;
        let status = "aborted";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert!(child_ids.is_empty());
        assert_eq!(
            kind, None,
            "#1670 P2: with a single tracked child, terminal status must release the kind"
        );
    }

    #[test]
    fn non_terminal_status_keeps_kind_and_does_not_pop() {
        let mut child_ids: Vec<i64> = vec![1, 2];
        let mut kind: Option<TaskNotificationKind> = None;
        kind = merge_task_notification_kind(kind, TaskNotificationKind::Subagent);

        let new_kind = TaskNotificationKind::Subagent;
        let status = "started";
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, status) {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert_eq!(child_ids, vec![1, 2], "non-terminal must not pop");
        assert_eq!(
            kind,
            Some(TaskNotificationKind::Subagent),
            "non-terminal must keep kind"
        );
    }

    #[test]
    fn lower_priority_child_close_preserves_higher_priority_kind() {
        let mut child_ids: Vec<i64> = vec![7];
        let mut kind: Option<TaskNotificationKind> = Some(TaskNotificationKind::MonitorAutoTurn);

        let new_kind = TaskNotificationKind::Subagent;
        kind = merge_task_notification_kind(kind, new_kind);
        if task_notification_closes_background_child(new_kind, "completed") {
            let _ = child_ids.remove(0);
            if child_ids.is_empty() {
                kind = release_task_notification_kind(kind, new_kind);
            }
        }

        assert!(child_ids.is_empty());
        assert_eq!(
            kind,
            Some(TaskNotificationKind::MonitorAutoTurn),
            "#1683: lower-priority child terminal notification must not clear a higher-priority active kind"
        );
    }
}
