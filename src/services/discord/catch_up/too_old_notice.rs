use super::{
    CATCH_UP_TOO_OLD_NOTICE_PREFIX, CatchUpClassification, classification::too_old_is_actionable,
};

pub(super) const CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS: usize = 10;

#[derive(Clone, Debug)]
pub(super) struct CatchUpTooOldDrop {
    pub(super) author_id: u64,
    pub(super) snippet: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CatchUpTooOldOutboxRequest {
    pub(super) target: String,
    pub(super) content: String,
    pub(super) bot: &'static str,
    pub(super) source: &'static str,
    pub(super) reason_code: &'static str,
    pub(super) session_key: String,
}

pub(super) fn actionable_drop(
    outcome: CatchUpClassification,
    author_id: u64,
    author_is_bot: bool,
    allowed_bot_ids: &[u64],
    announce_bot_id: Option<u64>,
    notify_bot_id: Option<u64>,
    text: &str,
) -> Option<CatchUpTooOldDrop> {
    too_old_is_actionable(
        outcome,
        author_id,
        author_is_bot,
        allowed_bot_ids,
        announce_bot_id,
        notify_bot_id,
    )
    .then(|| CatchUpTooOldDrop {
        author_id,
        snippet: catch_up_too_old_snippet(text),
    })
}

pub(super) fn catch_up_too_old_snippet(text: &str) -> String {
    const MAX: usize = 80;
    let trimmed = text.trim();
    let mut snippet: String = trimmed.chars().take(MAX).collect();
    if trimmed.chars().count() > MAX {
        snippet.push('…');
    }
    if snippet.is_empty() {
        snippet.push_str("(빈 메시지)");
    }
    snippet
}

pub(super) fn notice(drops: &[CatchUpTooOldDrop]) -> Option<String> {
    if drops.is_empty() {
        return None;
    }
    let mut body = format!(
        "{CATCH_UP_TOO_OLD_NOTICE_PREFIX} {}건이 5분 초과로 미처리되었습니다. 필요하면 다시 보내주세요:",
        drops.len()
    );
    for drop in drops.iter().take(CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS) {
        body.push_str(&format!("\n• `{}`: {}", drop.author_id, drop.snippet));
    }
    if drops.len() > CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS {
        body.push_str(&format!(
            "\n… 외 {}건",
            drops.len() - CATCH_UP_TOO_OLD_NOTICE_MAX_ITEMS
        ));
    }
    Some(body)
}

fn outbox_message(
    request: &CatchUpTooOldOutboxRequest,
) -> crate::services::message_outbox::OutboxMessage<'_> {
    crate::services::message_outbox::OutboxMessage {
        target: &request.target,
        content: &request.content,
        bot: request.bot,
        source: request.source,
        reason_code: Some(request.reason_code),
        session_key: Some(&request.session_key),
    }
}

pub(super) fn spawn_outbox(
    pool: sqlx::PgPool,
    request: CatchUpTooOldOutboxRequest,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let message = outbox_message(&request);
        // Persistent dedupe (NULL expiry) instead of the rolling 5-minute TTL:
        // the batch identity ("catch_up_too_old:{channel}:{batch_id}") is stable,
        // so a restart more than 5 minutes after the first notice must not re-notify
        // the same too-old batch. The returned row id is a best-effort audit handle
        // and is intentionally discarded here.
        if let Err(error) =
            crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
                &pool, message,
            )
            .await
        {
            tracing::warn!(
                "[dlq] failed to enqueue catch-up too-old notice (best-effort): {error}"
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn default_adapter_preserves_outbox_contract_and_pg_dedupe_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_catch_up_too_old_default_adapter",
            "catch-up too-old default outbox adapter tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let http = std::sync::Arc::new(poise::serenity_prelude::Http::new("test-token"));
        let api = super::super::SerenityCatchUpDiscordApi { http: &http };
        let target = "channel:4453005".to_string();
        let first = CatchUpTooOldOutboxRequest {
            target: target.clone(),
            content: "첫 사용자 요청을 다시 보내주세요".to_string(),
            bot: "notify",
            source: "catch_up_too_old",
            reason_code: "catch_up.too_old",
            session_key: "catch_up_too_old:4453005:4453101".to_string(),
        };

        for request in [first.clone(), first] {
            super::super::CatchUpDiscordApi::enqueue_too_old_notice(
                &api,
                Some(pool.clone()),
                request,
            )
            .expect("default adapter must spawn the PG producer")
            .await
            .expect("default adapter producer task");
        }
        super::super::CatchUpDiscordApi::enqueue_too_old_notice(
            &api,
            Some(pool.clone()),
            CatchUpTooOldOutboxRequest {
                target: target.clone(),
                content: "새 사용자 요청을 다시 보내주세요".to_string(),
                bot: "notify",
                source: "catch_up_too_old",
                reason_code: "catch_up.too_old",
                session_key: "catch_up_too_old:4453005:4453102".to_string(),
            },
        )
        .expect("default adapter must spawn the new-batch PG producer")
        .await
        .expect("new-batch producer task");

        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT target, content, bot, source, reason_code, session_key
               FROM message_outbox
              WHERE target = $1
              ORDER BY id",
        )
        .bind(&target)
        .fetch_all(&pool)
        .await
        .expect("load catch-up outbox rows");
        assert_eq!(
            rows,
            vec![
                (
                    target.clone(),
                    "첫 사용자 요청을 다시 보내주세요".to_string(),
                    "notify".to_string(),
                    "catch_up_too_old".to_string(),
                    "catch_up.too_old".to_string(),
                    "catch_up_too_old:4453005:4453101".to_string(),
                ),
                (
                    target,
                    "새 사용자 요청을 다시 보내주세요".to_string(),
                    "notify".to_string(),
                    "catch_up_too_old".to_string(),
                    "catch_up.too_old".to_string(),
                    "catch_up_too_old:4453005:4453102".to_string(),
                ),
            ],
            "same batch must dedupe while a distinct human batch inserts with the exact producer contract"
        );
        pool.close().await;
        pg_db.drop().await;
    }

    /// Mutation-proof for slice4 (#4564): the same too-old batch must be noticed
    /// exactly once even when the second attempt lands more than the 5-minute
    /// dedupe TTL after the first — i.e. a control-plane restart with a wide gap.
    ///
    /// The batch identity `session_key = "catch_up_too_old:{channel}:{batch_id}"`
    /// is stable, so `spawn_outbox` now stages via
    /// `enqueue_outbox_pg_returning_id_with_persistent_dedupe` (NULL expiry,
    /// `ON CONFLICT(dedupe_key) WHERE status NOT IN ('failed', 'cancelled')`). The
    /// dedupe key has no
    /// time component, so the restart converges on the original row.
    ///
    /// We simulate the >5-minute gap by shifting the first row back in time by
    /// 10 minutes (both `created_at` and any `dedupe_expires_at`). This is exactly
    /// where the reverted rolling-TTL path would break: with `enqueue_outbox_pg`
    /// (300s TTL), the aged row falls outside the `created_at >= NOW() - 300s`
    /// duplicate window *and* its `dedupe_expires_at` has passed, so the expired
    /// key is released and a SECOND row is inserted — a duplicate notice. Under
    /// that mutation this test FAILS on the `assert_eq!(count, 1, ...)` below.
    #[tokio::test(flavor = "current_thread")]
    async fn persistent_dedupe_suppresses_too_old_notice_across_restart_gap_pg() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_catch_up_too_old_persistent_dedupe",
            "catch-up too-old persistent dedupe across restart gap",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let http = std::sync::Arc::new(poise::serenity_prelude::Http::new("test-token"));
        let api = super::super::SerenityCatchUpDiscordApi { http: &http };
        let target = "channel:4453005".to_string();
        let batch = CatchUpTooOldOutboxRequest {
            target: target.clone(),
            content: "재시작 공백 배치를 다시 보내주세요".to_string(),
            bot: "notify",
            source: "catch_up_too_old",
            reason_code: "catch_up.too_old",
            session_key: "catch_up_too_old:4453005:4453900".to_string(),
        };

        // First notice for the batch (control plane boot #1).
        super::super::CatchUpDiscordApi::enqueue_too_old_notice(
            &api,
            Some(pool.clone()),
            batch.clone(),
        )
        .expect("first boot must spawn the PG producer")
        .await
        .expect("first boot producer task");

        // Age the staged row past the 5-minute TTL window to emulate a restart
        // whose gap exceeds the rolling dedupe horizon. Shift the whole row back
        // in time so a reverted TTL path would treat it as fully expired.
        sqlx::query(
            "UPDATE message_outbox
                SET created_at = created_at - INTERVAL '10 minutes',
                    dedupe_expires_at = dedupe_expires_at - INTERVAL '10 minutes'
              WHERE target = $1",
        )
        .bind(&target)
        .execute(&pool)
        .await
        .expect("age the staged too-old notice row");

        // Second notice for the *same* batch (control plane boot #2, >5min later).
        super::super::CatchUpDiscordApi::enqueue_too_old_notice(&api, Some(pool.clone()), batch)
            .expect("second boot must spawn the PG producer")
            .await
            .expect("second boot producer task");

        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM message_outbox
              WHERE target = $1
                AND session_key = 'catch_up_too_old:4453005:4453900'",
        )
        .bind(&target)
        .fetch_one(&pool)
        .await
        .expect("count staged too-old notices for the batch");

        assert_eq!(
            count, 1,
            "persistent dedupe must notice the same too-old batch exactly once across a >5min \
             restart gap; a rolling-TTL revert would insert a duplicate row here"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
