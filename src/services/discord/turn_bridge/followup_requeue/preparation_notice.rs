use super::*;

/// A non-turn outbox event belongs to the queued source, never to a fabricated Discord id.
pub(in crate::services::discord) async fn enqueue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    source: u64,
    generation: u64,
    reason: &str,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        tracing::warn!(
            source,
            reason,
            "preparation hold notice awaits a durable outbox"
        );
        return;
    };
    let target = format!("channel:{}", channel.get());
    let identity = format!(
        "input-hold:{}:{source}:{generation}:{reason}",
        channel.get()
    );
    let content = if reason == "submission_uncertain" {
        "⏳ 입력 제출 여부를 아직 확인하지 못했습니다. 원문과 기존 세션을 보존하고 있으며, 확인 전에는 다시 입력하지 않습니다."
    } else {
        "⏳ 입력창 내용을 안전하게 보관하거나 화면 상태를 확인할 수 없어 메시지가 대기 중입니다. 기존 세션은 유지하며, 입력 가능한 상태가 되면 자동으로 이어갑니다."
    };
    let content =
        crate::services::discord::dispatch_policy::prepend_operational_alert_origin(content);
    let result =
        crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            pool,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: &content,
                bot: provider.as_str(),
                source: "system",
                reason_code: Some("claude.input.preparation_hold"),
                session_key: Some(&identity),
            },
        )
        .await;
    if let Err(error) = result {
        tracing::warn!(source, reason, %error, "preparation hold notice enqueue failed");
    }
}
