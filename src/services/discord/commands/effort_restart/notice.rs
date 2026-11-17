//! The one notice a turn cut by an effort restart is owed, staged in the outbox under that turn.
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
};
use sha2::{Digest, Sha256};

const TEXT: &str = "⚠️ effort 변경을 적용하려고 진행 중이던 작업을 중단했습니다. 같은 세션을 다시 시작한 뒤 자동으로 이어갑니다.";

/// Stages the notice keyed by the cut episode, so a repeat for that episode stages nothing more.
pub(super) async fn interrupted(
    pool: &sqlx::PgPool,
    provider: &str,
    channel: u64,
    episode: &str,
) -> Result<i64, String> {
    let stable = serde_json::to_vec(&(provider, channel, episode)).map_err(|e| e.to_string())?;
    let key = format!("effort-restart:{:x}", Sha256::digest(stable));
    let text = crate::services::discord::dispatch_policy::prepend_operational_alert_origin(TEXT);
    let message = OutboxMessage {
        target: &format!("channel:{channel}"),
        content: &text,
        bot: provider,
        source: "lifecycle_notifier",
        reason_code: Some("effort.restart_interrupted"),
        session_key: Some(&key),
    };
    let staged = enqueue_outbox_pg_returning_id_with_persistent_dedupe(pool, message).await;
    staged.map_err(|error| error.to_string())
}
