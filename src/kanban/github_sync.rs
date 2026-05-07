//! GitHub issue synchronization for kanban transitions.

use sqlx::Row as SqlxRow;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;

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

async fn github_sync_target_for_card_pg(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
) -> Option<(String, i64)> {
    let row = sqlx::query(
        "SELECT
            COALESCE(repo_id, '') AS repo_id,
            COALESCE(github_issue_url, '') AS github_issue_url,
            github_issue_number::BIGINT AS github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pg_pool)
    .await
    .ok()??;

    let repo_id: String = row.try_get("repo_id").ok()?;
    let issue_url: String = row.try_get("github_issue_url").ok()?;
    let issue_number: Option<i64> = row.try_get("github_issue_number").ok()?;
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = issue_url
        .strip_prefix("https://github.com/")
        .and_then(|value| value.find("/issues/").map(|index| &value[..index]))?;
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM github_repos
            WHERE id = $1
              AND COALESCE(sync_enabled, TRUE) = TRUE
         )",
    )
    .bind(&repo_id)
    .fetch_one(pg_pool)
    .await
    .ok()
    .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|number| (repo_id, number))
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn github_sync_target_for_card(db: &Db, card_id: &str) -> Option<(String, i64)> {
    let info: Option<(String, String, Option<i64>)> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT COALESCE(repo_id, ''), COALESCE(github_issue_url, ''), github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    });

    let Some((repo_id, issue_url, issue_number)) = info else {
        return None;
    };
    if repo_id.is_empty() || issue_url.is_empty() {
        return None;
    }

    let issue_repo = match issue_url
        .strip_prefix("https://github.com/")
        .and_then(|s| s.find("/issues/").map(|i| &s[..i]))
    {
        Some(r) => r,
        None => return None,
    };
    if issue_repo != repo_id {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: issue URL repo {issue_repo} does not match card repo_id {repo_id}"
        );
        return None;
    }

    let repo_registered = db
        .lock()
        .ok()
        .and_then(|conn| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM github_repos WHERE id = ?1 AND COALESCE(sync_enabled, 1) = 1)",
                [&repo_id],
                |row| row.get::<_, bool>(0),
            )
            .ok()
        })
        .unwrap_or(false);
    if !repo_registered {
        tracing::warn!(
            "[kanban] skip GitHub sync for card {card_id}: repo_id {repo_id} is not a registered sync-enabled repo"
        );
        return None;
    }

    issue_number.map(|num| (repo_id, num))
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::*;

    #[test]
    fn github_sync_target_requires_registered_repo_and_matching_issue_repo() {
        let db = test_db();
        seed_card(&db, "card-github-sync-guard", "review");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET repo_id = 'owner/allowed',
                     github_issue_url = 'https://github.com/owner/other/issues/101',
                     github_issue_number = 101
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }

        // Mismatched URL repo must be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET github_issue_url = 'https://github.com/owner/allowed/issues/101'
                 WHERE id = 'card-github-sync-guard'",
                [],
            )
            .unwrap();
        }
        // Matching repo but not registered must still be rejected.
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            None
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES ('owner/allowed', 'Allowed Repo', 1)",
                [],
            )
            .unwrap();
        }
        assert_eq!(
            github_sync_target_for_card(&db, "card-github-sync-guard"),
            Some(("owner/allowed".to_string(), 101))
        );
    }
}
