use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use std::sync::Arc;

/// Dispatch info returned by the card-thread internal API.
#[derive(Debug, Clone, Default)]
pub(super) struct DispatchInfo {
    pub(super) card_id: Option<String>,
    pub(super) card_title: Option<String>,
    pub(super) dispatch_title: Option<String>,
    pub(super) github_issue_url: Option<String>,
    pub(super) github_issue_number: Option<i64>,
    pub(super) issue_body: Option<String>,
    pub(super) deferred_dod: Option<serde_json::Value>,
    pub(super) active_thread_id: Option<String>,
    pub(super) dispatch_type: Option<String>,
    pub(super) discord_channel_alt: Option<String>,
    /// #259: Dispatch context JSON — used to extract worktree_path for session CWD.
    pub(super) context: Option<String>,
}

#[allow(dead_code)]
pub(super) async fn lookup_card_thread(api_port: u16, dispatch_id: &str) -> Option<String> {
    let info = lookup_dispatch_info(api_port, dispatch_id).await?;
    info.active_thread_id
}

pub(super) async fn lookup_dispatch_info(api_port: u16, dispatch_id: &str) -> Option<DispatchInfo> {
    let _ = api_port;
    let body = crate::services::discord::internal_api::lookup_dispatch_info(dispatch_id)
        .await
        .ok()?;
    Some(DispatchInfo {
        card_id: body
            .get("card_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        card_title: body
            .get("card_title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        dispatch_title: body
            .get("dispatch_title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        github_issue_url: body
            .get("github_issue_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        github_issue_number: body.get("github_issue_number").and_then(|v| v.as_i64()),
        issue_body: body
            .get("issue_body")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        deferred_dod: body
            .get("deferred_dod")
            .cloned()
            .filter(|value| !value.is_null()),
        active_thread_id: body
            .get("active_thread_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        dispatch_type: body
            .get("dispatch_type")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        discord_channel_alt: body
            .get("discord_channel_alt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        context: body
            .get("dispatch_context")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

/// Verify a thread is accessible and not locked via Discord API.
/// Returns true if the thread exists and is not locked.
pub(super) async fn verify_thread_accessible(
    http: &Arc<serenity::http::Http>,
    thread_id: ChannelId,
) -> bool {
    match http.get_channel(thread_id).await {
        Ok(channel) => {
            if let Some(guild_channel) = channel.guild() {
                // Check if thread is locked
                if let Some(ref metadata) = guild_channel.thread_metadata {
                    if metadata.locked {
                        return false;
                    }
                    // Unarchive if needed — send will fail on archived threads via gateway
                    if metadata.archived {
                        let edit =
                            poise::serenity_prelude::builder::EditThread::new().archived(false);
                        if let Err(e) = thread_id.edit_thread(http, edit).await {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] ⚠️ Failed to unarchive thread {thread_id}: {e}"
                            );
                            return false;
                        }
                    }
                }
                true
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Link a newly created dispatch thread to the card's active_thread_id via internal API.
pub(super) async fn link_dispatch_thread(
    api_port: u16,
    dispatch_id: &str,
    thread_id: u64,
    channel_id: u64,
) {
    let _ = api_port;
    let _ = crate::services::discord::internal_api::link_dispatch_thread(
        crate::server::routes::dispatches::LinkDispatchThreadBody {
            dispatch_id: dispatch_id.to_string(),
            thread_id: thread_id.to_string(),
            channel_id: Some(channel_id.to_string()),
        },
    )
    .await;
}
