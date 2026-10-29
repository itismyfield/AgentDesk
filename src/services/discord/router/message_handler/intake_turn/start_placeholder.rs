//! The processing placeholder an intake attempt shows, and its removal when that start is refused.

use super::*;
use std::time::Duration;

/// Bounds the refusal delete so a slow Discord answer cannot hold the caller; the sweeper retries.
const REFUSED_DELETE_WAIT: Duration = Duration::from_secs(5);

/// The attempt's anchor, none on a turn-mode channel; `fresh` only when this attempt POSTed it,
/// not when it reused a bound notice.
#[derive(Clone, Copy)]
pub(super) struct StartPlaceholder {
    pub(super) channel_id: ChannelId,
    pub(super) message_id: Option<MessageId>,
    pub(super) fresh: bool,
}

impl StartPlaceholder {
    /// Deletes this attempt's own fresh placeholder once its start was refused and its input handed
    /// back; a reused busy notice stays. A failed or slow delete goes to the runtime's orphan sweeper.
    pub(super) async fn discard_refused(
        self,
        http: &Arc<serenity::http::Http>,
        shared: &Arc<SharedData>,
        user_msg_id: MessageId,
        refusal: &'static str,
    ) {
        let Some(message_id) = self.message_id.filter(|_| self.fresh) else {
            return;
        };
        let channel_id = self.channel_id;
        let delete = async {
            rate_limit_wait(shared, channel_id).await;
            channel_id.delete_message(http, message_id).await
        };
        let error = match tokio::time::timeout(REFUSED_DELETE_WAIT, delete).await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("delete timed out".to_string()),
        };
        if error.is_some() {
            crate::services::discord::status_panel_orphan_store::enqueue(
                &shared.provider,
                &shared.token_hash,
                channel_id.get(),
                message_id.get(),
            );
        }
        tracing::info!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            placeholder_msg_id = message_id.get(),
            refusal,
            orphan_queued = error.is_some(),
            error = error.as_deref().unwrap_or(""),
            "removed the refused start's own processing placeholder"
        );
    }
}

#[cfg(all(test, unix))]
#[path = "start_placeholder_tests.rs"]
mod tests;
