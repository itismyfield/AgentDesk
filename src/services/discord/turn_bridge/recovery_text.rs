use std::sync::Arc;

use crate::config::local_api_url;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use serenity::all::{ChannelId, MessageId};

/// Auto-retry a failed resume by fetching recent Discord history,
/// storing it in kv_meta for the router to inject into the LLM prompt,
/// and queueing the original message as an internal intervention.
/// Discord only sees the next provider reply — the full history is LLM-only.
pub(in crate::services::discord) async fn auto_retry_with_history(
    http: &serenity::http::Http,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    user_message_id: MessageId,
    user_text: &str,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    // Dedup guard: use a static set to prevent turn_bridge + watcher from
    // both firing auto-retry for the same channel simultaneously.
    use std::sync::LazyLock;
    static RETRY_PENDING: LazyLock<dashmap::DashSet<u64>> =
        LazyLock::new(|| dashmap::DashSet::new());
    if !RETRY_PENDING.insert(channel_id.get()) {
        eprintln!("  [{ts}] ⏭ auto-retry: skipped (dedup) for channel {channel_id}");
        return;
    }
    // Clean up guard after 30 seconds (allow future retries)
    let ch_id = channel_id.get();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        RETRY_PENDING.remove(&ch_id);
    });

    eprintln!("  [{ts}] ↻ auto-retry: fetching last 10 messages for channel {channel_id}");

    // Fetch last 10 messages from Discord
    let history = match channel_id
        .messages(http, serenity::builder::GetMessages::new().limit(10))
        .await
    {
        Ok(msgs) => {
            let mut lines = Vec::new();
            for msg in msgs.iter().rev() {
                let author = &msg.author.name;
                let content = msg.content.chars().take(300).collect::<String>();
                if !content.trim().is_empty() {
                    lines.push(format!("{}: {}", author, content));
                }
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Err(e) => {
            eprintln!("  [{ts}] ⚠ auto-retry: failed to fetch history: {e}");
            None
        }
    };

    // Store history in kv_meta for the router to inject into LLM prompt.
    // Key: session_retry_context:{channel_id} — consumed on next turn start.
    if let Some(ref hist) = history {
        let _ = reqwest::Client::new()
            .post(local_api_url(shared.api_port, "/api/kv"))
            .json(&serde_json::json!({
                "key": format!("session_retry_context:{}", channel_id),
                "value": hist,
            }))
            .send()
            .await;
    }

    // Discord message: short notice only — history stays LLM-side
    let retry_content = format!(
        "[이전 대화 복원 — 세션이 만료되어 최근 대화를 컨텍스트로 제공합니다]\n\n{}",
        user_text
    );
    let enqueued = super::super::enqueue_internal_followup(
        shared,
        provider,
        channel_id,
        user_message_id,
        retry_content,
        "auto-retry with history",
    )
    .await;
    if !enqueued {
        eprintln!("  [{ts}] ⏭ auto-retry: follow-up deduped for channel {channel_id}");
    }
}
