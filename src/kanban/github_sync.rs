//! GitHub issue synchronization for kanban transitions.

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use super::github_sync_target::github_sync_target_for_card;
use super::github_sync_target::github_sync_target_for_card_pg;

pub(super) async fn github_sync_on_transition_pg(
    pg_pool: &sqlx::PgPool,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .is_some_and(|hooks| hooks.on_enter.iter().any(|name| name == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, issue_number)) = github_sync_target_for_card_pg(pg_pool, card_id).await
    else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, issue_number) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{issue_number} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, issue_number, comment);
    }
}

/// Sync GitHub issue state when kanban card transitions (pipeline-driven).
/// Terminal states -> close issue. States with OnReviewEnter hook -> comment.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn github_sync_on_transition(
    db: &Db,
    pipeline: &crate::pipeline::PipelineConfig,
    card_id: &str,
    new_status: &str,
) {
    let is_terminal = pipeline.is_terminal(new_status);
    let is_review_enter = pipeline
        .hooks_for_state(new_status)
        .map_or(false, |h| h.on_enter.iter().any(|n| n == "OnReviewEnter"));

    if !is_terminal && !is_review_enter {
        return;
    }

    let Some((repo_id, num)) = github_sync_target_for_card(db, card_id) else {
        return;
    };

    if is_terminal {
        if let Err(error) = crate::github::close_issue(&repo_id, num) {
            tracing::warn!(
                "[kanban] failed to close issue {repo_id}#{num} for terminal card {card_id}: {error}"
            );
        }
    } else if is_review_enter {
        let comment = "🔍 칸반 상태: **review** (카운터모델 리뷰 진행 중)";
        let _ = crate::github::comment_issue(&repo_id, num, comment);
    }
}
