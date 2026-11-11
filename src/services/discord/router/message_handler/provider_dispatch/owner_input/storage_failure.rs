use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe,
};
use crate::services::turn_orchestrator::{OwnedRecoveryFailure, RecoveryPass};
use sha2::{Digest, Sha256};

pub(super) async fn catalog(
    pool: Option<&sqlx::PgPool>,
    pass: &RecoveryPass,
    failure: &OwnedRecoveryFailure,
) -> Result<i64, String> {
    let key = pass.notice_key(failure)?;
    post(pool, &failure.provider, failure.channel.get(), &key).await
}

pub(super) async fn cleanup(
    pool: Option<&sqlx::PgPool>,
    (provider, channel, namespace): (&str, u64, &str),
    (cleanup_id, stage): (&str, &str),
) -> Result<i64, String> {
    let identity = serde_json::to_vec(&(provider, namespace, cleanup_id, stage))
        .map_err(|error| error.to_string())?;
    let key = format!("A-io:v1:{}", hex::encode(Sha256::digest(identity)));
    post(pool, provider, channel, &key).await
}

async fn post(
    pool: Option<&sqlx::PgPool>,
    provider: &str,
    channel: u64,
    key: &str,
) -> Result<i64, String> {
    let pool = pool.ok_or("input storage notice needs PostgreSQL; recovery remains held")?;
    let text = crate::services::discord::dispatch_policy::prepend_operational_alert_origin(
        "⚠️ 저장된 입력 상태를 확인할 수 없어 이 채널의 자동 처리를 보류했습니다. 저장 파일은 보존했습니다.",
    );
    enqueue_outbox_pg_returning_id_with_persistent_dedupe(
        pool,
        OutboxMessage {
            target: &format!("channel:{channel}"),
            content: &text,
            bot: provider,
            source: "retained_input_notice",
            reason_code: Some("input.storage_failure"),
            session_key: Some(key),
        },
    )
    .await
    .map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "storage_failure_tests.rs"]
mod tests;
