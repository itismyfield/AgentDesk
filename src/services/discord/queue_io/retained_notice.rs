//! Durable retention projects one immutable, non-turn event into the existing outbox.
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
};
use sha2::{Digest, Sha256};

pub(crate) async fn enqueue(
    pool: &sqlx::PgPool,
    provider: &str,
    channel: u64,
    namespace: &str,
    identity: &serde_json::Value,
) -> Result<i64, String> {
    let stable = (
        &identity["input"]["identity"],
        &identity["input"]["attempt"]["nonce"],
        &identity["notice"]["kind"],
    );
    let key = format!(
        "retained:{:x}",
        Sha256::digest(
            serde_json::to_vec(&(provider, channel, namespace, stable))
                .map_err(|error| error.to_string())?
        )
    );
    // A proven-unsent original asks for a resend; only a true ambiguity warns of a duplicate.
    // A batch some of whose sources were already taken asks only for its new part.
    let body = match identity["notice"]["kind"].as_str() {
        _ if identity["disposition"] == "opaque_unsent" => {
            "⚠️ 일부 출처가 이미 접수되어 이 묶음은 추가로 전달하지 않고 보관했습니다. 다음 입력은 계속 처리합니다. 필요한 새 부분을 따로 보내 주세요. 전체를 다시 보내면 중복 실행될 수 있습니다."
        }
        Some("retained_retry_required") => {
            "⚠️ 이 입력은 전달되지 않은 것을 확인해 원문을 보관했습니다. 다음 입력은 계속 처리합니다. 필요하면 같은 내용을 다시 보내 주세요."
        }
        _ => {
            "⚠️ 이 입력은 전달 여부를 확인하지 못해 원문을 보관했습니다. 다음 입력은 계속 처리합니다. 자동으로 다시 보내지 않습니다. 같은 내용을 새 메시지로 보내면 중복 실행될 수 있습니다."
        }
    };
    let text = super::super::dispatch_policy::prepend_operational_alert_origin(body);
    enqueue_outbox_pg_returning_id_with_persistent_dedupe(
        pool,
        OutboxMessage {
            target: &format!("channel:{channel}"),
            content: &text,
            bot: provider,
            source: "retained_input_notice",
            reason_code: Some("retained.unconfirmed"),
            session_key: Some(&key),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

impl crate::services::turn_orchestrator::ChannelMailboxHandle {
    pub(crate) fn v2_notice_is_non_turn(text: &str) -> bool {
        super::super::dispatch_policy::has_non_turn_provenance(text)
            && !super::super::dispatch_policy::should_process_allowed_bot_turn_text(text)
    }
    pub(crate) async fn v2_notice(
        pool: &sqlx::PgPool,
        provider: &str,
        channel: u64,
        namespace: &str,
        identity: &serde_json::Value,
    ) -> Result<i64, String> {
        enqueue(pool, provider, channel, namespace, identity).await
    }
}

#[test]
fn retained_notice_provenance_prevents_allowed_bot_turn_admission() {
    let text = super::super::dispatch_policy::prepend_operational_alert_origin("retained original");
    assert!(crate::services::turn_orchestrator::ChannelMailboxHandle::v2_notice_is_non_turn(&text));
}

#[cfg(all(test, unix))]
#[path = "retained_notice_tests.rs"]
mod tests;
