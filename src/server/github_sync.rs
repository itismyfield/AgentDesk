use super::{
    Arc, PgPool, release_pg_singleton_lock, try_acquire_pg_singleton_lock, upsert_kv_meta_pg_ignore,
};

const GITHUB_SYNC_ADVISORY_LOCK_ID: i64 = 7_801_002;

/// Background task that periodically syncs GitHub issues for all registered repos.
pub(super) async fn github_sync_loop(pg_pool: Arc<PgPool>, interval_minutes: u64) {
    use std::time::Duration;

    if !crate::github::gh_available() {
        tracing::warn!("[github-sync] gh CLI not available — periodic sync disabled");
        return;
    }

    tracing::info!(
        "[github-sync] Periodic sync enabled (every {} minutes)",
        interval_minutes
    );

    let interval = Duration::from_secs(interval_minutes * 60);

    loop {
        tokio::time::sleep(interval).await;
        let sync_start = std::time::Instant::now();

        let mut advisory_lock = match try_acquire_pg_singleton_lock(
            &pg_pool,
            GITHUB_SYNC_ADVISORY_LOCK_ID,
            "github-sync",
        )
        .await
        {
            Ok(Some(conn)) => Some(conn),
            Ok(None) => {
                tracing::debug!("[github-sync] skipped: advisory lock held elsewhere");
                continue;
            }
            Err(error) => {
                tracing::warn!("[github-sync] advisory lock failed: {error}");
                continue;
            }
        };

        tracing::debug!("[github-sync] Running periodic sync...");

        let repos = match crate::github::list_repos_pg(&pg_pool).await {
            Ok(repos) => repos,
            Err(error) => {
                tracing::error!("[github-sync] Failed to list repos from PG: {error}");
                record_periodic_job_execution_pg(
                    &pg_pool,
                    "github_sync",
                    "error",
                    sync_start.elapsed(),
                )
                .await;
                if let Some(conn) = advisory_lock.take() {
                    release_pg_singleton_lock(conn, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync")
                        .await;
                }
                continue;
            }
        };

        let mut had_errors = false;
        for repo in &repos {
            if !repo.sync_enabled {
                continue;
            }

            let issues = match crate::github::sync::fetch_issues(&repo.id) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("[github-sync] Fetch failed for {}: {e}", repo.id);
                    had_errors = true;
                    continue;
                }
            };

            match crate::github::triage::triage_new_issues_pg(&pg_pool, &repo.id, &issues).await {
                Ok(count) if count > 0 => {
                    tracing::info!("[github-sync] Triaged {count} new issues for {}", repo.id);
                }
                Err(error) => {
                    tracing::warn!("[github-sync] Triage failed for {}: {error}", repo.id);
                    had_errors = true;
                }
                _ => {}
            }

            match crate::github::sync::sync_github_issues_for_repo_pg(&pg_pool, &repo.id, &issues)
                .await
            {
                Ok(result) => {
                    if result.campaign_nodes.failed() {
                        tracing::warn!(
                            "[github-sync] {}: campaign node sync failed: {:?}",
                            repo.id,
                            result.campaign_nodes
                        );
                        had_errors = true;
                    }
                    if result.closed_count > 0
                        || result.inconsistency_count > 0
                        || result.stale_card_issue_check_count > 0
                        || result.stale_card_issue_error_count > 0
                    {
                        tracing::info!(
                            "[github-sync] {}: closed={}, inconsistencies={}, stale_issue_checks={}, stale_issue_batches={}, stale_issue_errors={}",
                            repo.id,
                            result.closed_count,
                            result.inconsistency_count,
                            result.stale_card_issue_check_count,
                            result.stale_card_issue_batch_count,
                            result.stale_card_issue_error_count
                        );
                    }
                }
                Err(error) => {
                    tracing::error!("[github-sync] Sync failed for {}: {error}", repo.id);
                    had_errors = true;
                }
            }
        }

        record_periodic_job_execution_pg(
            &pg_pool,
            "github_sync",
            if had_errors { "error" } else { "ok" },
            sync_start.elapsed(),
        )
        .await;

        if let Some(conn) = advisory_lock.take() {
            release_pg_singleton_lock(conn, GITHUB_SYNC_ADVISORY_LOCK_ID, "github-sync").await;
        }
    }
}

async fn record_periodic_job_execution_pg(
    pg_pool: &PgPool,
    label: &str,
    status: &str,
    elapsed: std::time::Duration,
) {
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();
    let elapsed_ms = elapsed.as_millis().to_string();
    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);
    let key_duration = format!("last_tick_{}_duration_ms", label);

    upsert_kv_meta_pg_ignore(pg_pool, &key_ms, &now_ms).await;
    upsert_kv_meta_pg_ignore(pg_pool, &key_status, status).await;
    upsert_kv_meta_pg_ignore(pg_pool, &key_duration, &elapsed_ms).await;
}
