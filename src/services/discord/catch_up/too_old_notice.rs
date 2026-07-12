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
        if let Err(error) = crate::services::message_outbox::enqueue_outbox_pg(&pool, message).await
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
}
