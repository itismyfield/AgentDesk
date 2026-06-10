//! #3038 decision_route decomposition: worktree / remote-mainline resolution,
//! review-dispatch context parsing, and stale-review-dispatch cancellation
//! (including the lifecycle-guarded scope-mismatch cleanup transaction).
//! Function bodies are verbatim moves from the former `decision_route.rs`
//! monolith.

use crate::app_state::AppState;

use super::adapters::cancel_dispatch_pg_first;
use super::repo_card::CardLifecycleSnapshot;

#[derive(Debug, Clone)]
pub(super) struct IssueWorktreeTarget {
    pub(super) worktree_path: String,
    pub(super) commit: String,
}

pub(super) async fn current_issue_worktree_target(
    pg_pool: Option<&sqlx::PgPool>,
    card_id: &str,
    issue_num: i64,
    context: Option<&serde_json::Value>,
) -> Option<IssueWorktreeTarget> {
    let Some(pool) = pg_pool else {
        tracing::warn!(
            "[review-decision] current_issue_worktree_commit: card {} issue #{}: postgres pool unavailable",
            card_id,
            issue_num
        );
        return None;
    };

    match crate::dispatch::resolve_card_worktree(pool, card_id, context).await {
        Ok(Some((worktree_path, _branch, commit))) => Some(IssueWorktreeTarget {
            worktree_path,
            commit,
        }),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                "[review-decision] current_issue_worktree_commit: card {} issue #{}: {}",
                card_id,
                issue_num,
                err
            );
            None
        }
    }
}

pub(super) fn context_repo_dir(context: Option<&serde_json::Value>) -> Option<String> {
    context
        .and_then(|value| {
            value
                .get("target_repo")
                .and_then(|entry| entry.as_str())
                .or_else(|| value.get("worktree_path").and_then(|entry| entry.as_str()))
                .or_else(|| {
                    value
                        .get("completed_worktree_path")
                        .and_then(|entry| entry.as_str())
                })
        })
        .and_then(|target| {
            crate::services::platform::shell::resolve_repo_dir_for_target(Some(target))
                .ok()
                .flatten()
        })
}

pub(super) fn commit_is_on_remote_mainline(repo_dir: &str, commit: &str) -> Option<bool> {
    if repo_dir.trim().is_empty() || commit.trim().is_empty() {
        return None;
    }
    let refs = ["origin/main", "origin/master"];
    let mut saw_ref = false;
    for remote_ref in refs {
        let exists = crate::services::git::GitCommand::new()
            .repo(repo_dir)
            .args(["rev-parse", "--verify", remote_ref])
            .run_output()
            .ok()
            .is_some_and(|output| output.status.success());
        if !exists {
            continue;
        }
        saw_ref = true;
        let merged = crate::services::git::GitCommand::new()
            .repo(repo_dir)
            .args(["merge-base", "--is-ancestor", commit, remote_ref])
            .run_output()
            .ok()
            .is_some_and(|output| output.status.success());
        if merged {
            return Some(true);
        }
    }
    saw_ref.then_some(false)
}

#[derive(Debug, Clone)]
pub(super) struct ActiveReviewDispatch {
    pub(super) id: String,
    pub(super) reviewed_commit: Option<String>,
    pub(super) target_repo: Option<String>,
}

fn build_active_review_dispatch(id: String, context_raw: Option<String>) -> ActiveReviewDispatch {
    let context = context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let target_repo = context
        .as_ref()
        .and_then(|value| {
            value
                .get("target_repo")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            context
                .as_ref()
                .and_then(|value| value.get("worktree_path"))
                .and_then(|entry| entry.as_str())
                .and_then(|path| {
                    crate::services::platform::shell::resolve_repo_dir_for_target(Some(path))
                        .ok()
                        .flatten()
                })
        });
    ActiveReviewDispatch {
        id,
        reviewed_commit: context.as_ref().and_then(|value| {
            value
                .get("reviewed_commit")
                .and_then(|entry| entry.as_str())
                .map(str::to_string)
        }),
        target_repo,
    }
}

pub(super) async fn latest_active_review_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<ActiveReviewDispatch> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')
             ORDER BY updated_at DESC NULLS LAST, created_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|(id, context_raw)| build_active_review_dispatch(id, context_raw)),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres active review dispatch"
                );
                None
            }
        };
    }

    None
}

pub(super) async fn latest_completed_review_context_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<serde_json::Value> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, Option<String>>(
            "SELECT context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(context_raw) => context_raw
                .flatten()
                .and_then(|ctx| serde_json::from_str::<serde_json::Value>(&ctx).ok()),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres completed review context"
                );
                None
            }
        };
    }

    None
}

/// #2341 / #2200 sub-3 redesign: snapshot of the latest **completed** review
/// dispatch for a card. This is the context that is available in the
/// production flow at `/api/review-decision` time — the review dispatch has
/// already terminated by the time the operator decides to dispute, so the
/// out-of-scope close path MUST bind to a completed (not active) row.
///
/// We surface the dispatch id (so we can record `review_dispatch_id` in the
/// scope_mismatch_closed result for forensic correlation and so the by-id
/// pattern from sub-fix 4 can verify the dispute payload references this
/// completed dispatch), plus the parsed `reviewed_commit` and `target_repo`
/// from its context (so the scope check can re-run against the same commit
/// the reviewer saw).
#[derive(Debug, Clone)]
pub(super) struct CompletedReviewDispatch {
    pub(super) id: String,
    pub(super) reviewed_commit: Option<String>,
    pub(super) target_repo: Option<String>,
}

async fn latest_completed_review_dispatch_pg_first(
    state: &AppState,
    card_id: &str,
) -> Option<CompletedReviewDispatch> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id, context
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status = 'completed'
             ORDER BY completed_at DESC NULLS LAST, updated_at DESC NULLS LAST
             LIMIT 1",
        )
        .bind(card_id)
        .fetch_optional(pool)
        .await
        {
            Ok(row) => row.map(|(id, context_raw)| {
                let active = build_active_review_dispatch(id, context_raw);
                CompletedReviewDispatch {
                    id: active.id,
                    reviewed_commit: active.reviewed_commit,
                    target_repo: active.target_repo,
                }
            }),
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres completed review dispatch"
                );
                None
            }
        };
    }

    None
}

/// #2341 / #2200 sub-3: outcome of the source-review-by-id lookup.
#[derive(Debug)]
pub(super) enum SourceReviewLookup {
    /// Source review id was loaded from the review-decision context and
    /// resolved cleanly to a completed review row.
    ResolvedById(CompletedReviewDispatch),
    /// Review-decision context did NOT include `source_review_dispatch_id`
    /// (legacy row from before the persistence change). Caller may fall
    /// back to the latest completed review.
    LegacyFallback(Option<CompletedReviewDispatch>),
    /// Review-decision context referenced a `source_review_dispatch_id` that
    /// does NOT resolve to a completed review row (missing, uncompleted,
    /// cross-card, or wrong dispatch_type). Codex round-2 [medium]: caller
    /// MUST fail closed — falling back to latest-completed would bind to a
    /// duplicate or unrelated review.
    UnresolvedSourceId(String),
}

/// #2341 / #2200 sub-3 (Codex round-1 [medium] + round-2 [medium]): bind
/// the close path to the source review dispatch that produced THIS
/// review-decision, not to the latest completed review for the card.
/// Loads the review-decision dispatch context to extract
/// `source_review_dispatch_id` (persisted by `discord_delivery::orchestration`
/// when the follow-up was created), then loads that review row by id.
/// Returns:
///   * `ResolvedById` — the source id resolved to a completed review row.
///   * `LegacyFallback(latest)` — context predates the persistence change;
///     the caller may use latest-completed as a defensible fallback.
///   * `UnresolvedSourceId(srid)` — context has a source id but it does not
///     resolve; caller MUST fail closed (no silent fallback that could
///     bind to the wrong review row).
pub(super) async fn source_review_dispatch_for_decision_pg_first(
    state: &AppState,
    card_id: &str,
    rd_id: &str,
) -> SourceReviewLookup {
    // Load the review-decision dispatch context.
    let rd_context_raw: Option<String> = if let Some(pool) = state.pg_pool_ref() {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT context FROM task_dispatches WHERE id = $1 AND kanban_card_id = $2 AND dispatch_type = 'review-decision'",
        )
        .bind(rd_id)
        .bind(card_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
    } else {
        None
    };

    let source_review_dispatch_id: Option<String> = rd_context_raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|v| {
            v.get("source_review_dispatch_id")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        });

    if let Some(srid) = source_review_dispatch_id {
        // Load the EXACT review by id — scoped to the same card +
        // dispatch_type so a stale or unrelated id cannot bind.
        let row: Option<(String, Option<String>)> = if let Some(pool) = state.pg_pool_ref() {
            sqlx::query_as::<_, (String, Option<String>)>(
                "SELECT id, context FROM task_dispatches WHERE id = $1 AND kanban_card_id = $2 AND dispatch_type = 'review' AND status = 'completed'",
            )
            .bind(&srid)
            .bind(card_id)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        if let Some((id, context_raw)) = row {
            let active = build_active_review_dispatch(id, context_raw);
            return SourceReviewLookup::ResolvedById(CompletedReviewDispatch {
                id: active.id,
                reviewed_commit: active.reviewed_commit,
                target_repo: active.target_repo,
            });
        }
        // Codex round-2 [medium]: do NOT silently fall back to
        // latest-completed when an explicit source id was recorded but does
        // not resolve. That would reintroduce the wrong-row binding the by-id
        // path was meant to prevent.
        tracing::warn!(
            card_id,
            rd_id,
            source_review_dispatch_id = %srid,
            "[review-decision] #2341 source_review_dispatch_id from review-decision context did not resolve to a completed review row; failing closed (no silent latest-completed fallback)"
        );
        return SourceReviewLookup::UnresolvedSourceId(srid);
    }

    // Legacy fallback: review-decision context predates the
    // source_review_dispatch_id persistence change. Latest-completed is
    // defensible here because there was no recorded source id to honor.
    SourceReviewLookup::LegacyFallback(
        latest_completed_review_dispatch_pg_first(state, card_id).await,
    )
}

async fn stale_review_dispatch_ids_pg_first(state: &AppState, card_id: &str) -> Vec<String> {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query_scalar::<_, String>(
            "SELECT id
             FROM task_dispatches
             WHERE kanban_card_id = $1
               AND dispatch_type = 'review'
               AND status IN ('pending', 'dispatched')",
        )
        .bind(card_id)
        .fetch_all(pool)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(
                    card_id,
                    %error,
                    "[review-decision] failed to load postgres stale review dispatches"
                );
                Vec::new()
            }
        };
    }

    Vec::new()
}

async fn stale_review_dispatch_ids_required_pg_first(
    state: &AppState,
    card_id: &str,
) -> Result<Vec<String>, String> {
    let Some(pool) = state.pg_pool_ref() else {
        return Ok(stale_review_dispatch_ids_pg_first(state, card_id).await);
    };
    sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres stale review dispatches for {card_id}: {error}"))
}

pub(super) async fn cancel_stale_review_dispatches_required_pg_first(
    state: &AppState,
    card_id: &str,
    reason: &str,
) -> Result<usize, String> {
    let stale_ids = stale_review_dispatch_ids_required_pg_first(state, card_id).await?;
    let mut cancelled = 0usize;
    for stale_id in &stale_ids {
        cancelled += cancel_dispatch_pg_first(state, stale_id, Some(reason)).await?;
    }
    Ok(cancelled)
}

pub(super) async fn cancel_stale_review_dispatches_for_scope_mismatch_pg_first(
    state: &AppState,
    card_id: &str,
    reason: &str,
    expected_lifecycle: &CardLifecycleSnapshot,
) -> Result<usize, String> {
    let Some(pool) = state.pg_pool_ref() else {
        let stale_ids = stale_review_dispatch_ids_pg_first(state, card_id).await;
        let mut cancelled = 0usize;
        for stale_id in &stale_ids {
            if cancel_dispatch_pg_first(state, stale_id, Some(reason))
                .await
                .unwrap_or(0)
                > 0
            {
                cancelled += 1;
            }
        }
        return Ok(cancelled);
    };

    let mut tx = pool.begin().await.map_err(|error| {
        format!("begin guarded scope-mismatch cleanup tx for {card_id}: {error}")
    })?;

    let actual_latest_dispatch_id: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id FROM kanban_cards WHERE id = $1 FOR UPDATE",
    )
    .bind(card_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| {
        format!("guard postgres scope-mismatch cleanup card lifecycle for {card_id}: {error}")
    })?
    .flatten();
    let review_fields: Option<(Option<i64>, Option<chrono::DateTime<chrono::Utc>>)> =
        sqlx::query_as::<_, (Option<i64>, Option<chrono::DateTime<chrono::Utc>>)>(
            "SELECT review_round, review_entered_at
             FROM card_review_state
             WHERE card_id = $1
             FOR UPDATE",
        )
        .bind(card_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| {
            format!("guard postgres scope-mismatch cleanup review lifecycle for {card_id}: {error}")
        })?;
    let actual = CardLifecycleSnapshot {
        latest_dispatch_id: actual_latest_dispatch_id,
        review_round: review_fields.as_ref().and_then(|(round, _)| *round),
        review_entered_at_iso: review_fields
            .as_ref()
            .and_then(|(_, entered_at)| entered_at.as_ref())
            .map(|ts| ts.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)),
    };
    let lifecycle_matches = &actual == expected_lifecycle;

    if !lifecycle_matches {
        tx.rollback()
            .await
            .map_err(|error| format!("rollback stale scope-mismatch cleanup {card_id}: {error}"))?;
        tracing::warn!(
            card_id,
            ?expected_lifecycle,
            ?actual,
            "[review-decision] guarded scope_mismatch cleanup skipped because card lifecycle changed"
        );
        return Ok(0);
    }

    let dispatch_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status IN ('pending', 'dispatched')
         FOR UPDATE",
    )
    .bind(card_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|error| format!("load guarded stale review dispatches for {card_id}: {error}"))?;

    let mut cancelled = 0usize;
    let mut changed_dispatch_ids = Vec::new();
    for dispatch_id in &dispatch_ids {
        let changed = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg_tx(
            &mut tx,
            dispatch_id,
            Some(reason),
        )
        .await?;
        if changed > 0 {
            cancelled += changed;
            changed_dispatch_ids.push(dispatch_id.clone());
        }
    }

    tx.commit()
        .await
        .map_err(|error| format!("commit guarded scope-mismatch cleanup {card_id}: {error}"))?;

    for dispatch_id in changed_dispatch_ids {
        crate::services::dispatches::wait_queue::spawn_cached_constraint_release_wake(
            pool.clone(),
            "constraint_release",
            dispatch_id,
            "scope_mismatch_cleanup",
        );
    }

    Ok(cancelled)
}

// #3038 characterization tests (moved with their functions from the monolith).
#[cfg(test)]
mod tests {
    use super::*;

    // ── context_repo_dir / build_active_review_dispatch fixtures ─────────

    /// Create a temp dir initialized as a real git worktree so the
    /// `resolve_repo_dir_for_target` path checks succeed deterministically.
    fn init_git_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create tempdir");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .expect("spawn git init");
        assert!(status.success(), "git init failed in {:?}", dir.path());
        dir
    }

    fn canonical(path: &std::path::Path) -> String {
        std::fs::canonicalize(path)
            .expect("canonicalize")
            .to_string_lossy()
            .into_owned()
    }

    fn parse_context(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("valid test context json")
    }

    // ── context_repo_dir ─────────────────────────────────────────────────

    #[test]
    fn context_repo_dir_none_context_is_none() {
        assert_eq!(context_repo_dir(None), None);
    }

    #[test]
    fn context_repo_dir_without_recognized_keys_is_none() {
        let context = parse_context(r#"{"unrelated": "value"}"#);
        assert_eq!(context_repo_dir(Some(&context)), None);
    }

    #[test]
    fn context_repo_dir_nonexistent_target_repo_is_none() {
        let context = parse_context(r#"{"target_repo": "/definitely/missing/adk-3038-test-path"}"#);
        assert_eq!(context_repo_dir(Some(&context)), None);
    }

    #[test]
    fn context_repo_dir_resolves_git_worktree_target_repo() {
        let dir = init_git_dir();
        let context = parse_context(&format!(
            r#"{{"target_repo": "{}"}}"#,
            dir.path().to_string_lossy()
        ));
        assert_eq!(
            context_repo_dir(Some(&context)),
            Some(canonical(dir.path()))
        );
    }

    #[test]
    fn context_repo_dir_prefers_target_repo_over_worktree_path() {
        let target = init_git_dir();
        let worktree = init_git_dir();
        let context = parse_context(&format!(
            r#"{{"target_repo": "{}", "worktree_path": "{}"}}"#,
            target.path().to_string_lossy(),
            worktree.path().to_string_lossy()
        ));
        assert_eq!(
            context_repo_dir(Some(&context)),
            Some(canonical(target.path()))
        );
    }

    #[test]
    fn context_repo_dir_falls_back_to_worktree_path() {
        let worktree = init_git_dir();
        let context = parse_context(&format!(
            r#"{{"worktree_path": "{}"}}"#,
            worktree.path().to_string_lossy()
        ));
        assert_eq!(
            context_repo_dir(Some(&context)),
            Some(canonical(worktree.path()))
        );
    }

    // ── build_active_review_dispatch ─────────────────────────────────────

    #[test]
    fn build_active_review_dispatch_without_context_keeps_id_only() {
        let dispatch = build_active_review_dispatch("dispatch-1".to_string(), None);
        assert_eq!(dispatch.id, "dispatch-1");
        assert_eq!(dispatch.reviewed_commit, None);
        assert_eq!(dispatch.target_repo, None);
    }

    #[test]
    fn build_active_review_dispatch_invalid_json_context_keeps_id_only() {
        let dispatch =
            build_active_review_dispatch("dispatch-2".to_string(), Some("{not json".to_string()));
        assert_eq!(dispatch.id, "dispatch-2");
        assert_eq!(dispatch.reviewed_commit, None);
        assert_eq!(dispatch.target_repo, None);
    }

    #[test]
    fn build_active_review_dispatch_target_repo_passes_through_unresolved() {
        // `target_repo` is taken verbatim from the context — no filesystem
        // resolution happens on this branch.
        let dispatch = build_active_review_dispatch(
            "dispatch-3".to_string(),
            Some(r#"{"target_repo": "/srv/some-repo", "reviewed_commit": "abc1234"}"#.to_string()),
        );
        assert_eq!(dispatch.id, "dispatch-3");
        assert_eq!(dispatch.reviewed_commit, Some("abc1234".to_string()));
        assert_eq!(dispatch.target_repo, Some("/srv/some-repo".to_string()));
    }

    #[test]
    fn build_active_review_dispatch_worktree_path_fallback_resolves_git_dir() {
        let worktree = init_git_dir();
        let dispatch = build_active_review_dispatch(
            "dispatch-4".to_string(),
            Some(format!(
                r#"{{"worktree_path": "{}"}}"#,
                worktree.path().to_string_lossy()
            )),
        );
        assert_eq!(dispatch.target_repo, Some(canonical(worktree.path())));
    }

    #[test]
    fn build_active_review_dispatch_nonexistent_worktree_path_is_none() {
        let dispatch = build_active_review_dispatch(
            "dispatch-5".to_string(),
            Some(r#"{"worktree_path": "/definitely/missing/adk-3038-test-path"}"#.to_string()),
        );
        assert_eq!(dispatch.target_repo, None);
    }

    #[test]
    fn build_active_review_dispatch_non_string_reviewed_commit_is_none() {
        let dispatch = build_active_review_dispatch(
            "dispatch-6".to_string(),
            Some(r#"{"reviewed_commit": 42}"#.to_string()),
        );
        assert_eq!(dispatch.reviewed_commit, None);
    }
}
