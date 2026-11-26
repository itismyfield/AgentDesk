use super::discord_api_url;
use crate::services::session_host::thread_host::{PolicyHost, parent_policy};
use sqlx::PgPool;

pub(crate) async fn archive_duplicate_slot_threads(
    client: &reqwest::Client,
    token: &str,
    discord_api_base: &str,
    pg_pool: Option<&PgPool>,
    expected_parent: u64,
    keep_thread_id: &str,
    candidate_thread_ids: &[String],
) {
    if !crate::config::session_hosts::with_boot(|boot| boot.is_some())
        || !matches!(parent_policy(expected_parent), PolicyHost::Tmux)
    {
        return;
    }
    let Some(pool) = pg_pool else {
        return;
    };
    for thread_id in candidate_thread_ids {
        if thread_id == keep_thread_id {
            continue;
        }

        let thread_info_url = discord_api_url(discord_api_base, &format!("/channels/{thread_id}"));
        let response = match client
            .get(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to inspect duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        if !response.status().is_success() {
            continue;
        }

        let thread_info = match response.json::<serde_json::Value>().await {
            Ok(thread_info) => thread_info,
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to parse duplicate slot thread {thread_id}: {err}"
                );
                continue;
            }
        };

        let parent_id = thread_info
            .get("parent_id")
            .and_then(|value| value.as_str())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default();
        if parent_id != expected_parent {
            continue;
        }

        let already_archived = thread_info
            .get("thread_metadata")
            .and_then(|metadata| metadata.get("archived"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if already_archived {
            continue;
        }

        let hosted = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM sessions
                 WHERE (channel_id = $1 OR thread_channel_id = $1)
                   AND hosted_execution IS NOT NULL
             )",
        )
        .bind(thread_id)
        .fetch_one(pool)
        .await;
        if !matches!(hosted, Ok(false)) {
            continue;
        }

        match crate::services::dispatches::thread_archive_defer::should_defer_thread_archive_pg(
            pg_pool, thread_id,
        )
        .await
        {
            Ok(true) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active turn or fresh inflight still present"
                );
                continue;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Skipping duplicate slot thread archive for {thread_id}: active-check failed: {err}"
                );
                continue;
            }
        }

        match client
            .patch(&thread_info_url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"archived": true}))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("[dispatch] Archived duplicate slot thread {thread_id}");
            }
            Ok(resp) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {}",
                    resp.status()
                );
            }
            Err(err) => {
                tracing::warn!(
                    "[dispatch] Failed to archive duplicate slot thread {thread_id}: {err}"
                );
            }
        }
    }
}
