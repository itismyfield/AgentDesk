//! Footer-only background completion marker rendering and durable handoff.

use super::{TaskCardEvent, TaskCardPayload};
use crate::services::message_outbox::{self, OutboxMessage};

const FOOTER_ONLY_MARKER_DETAIL_LIMIT: usize = 600;
const BACKGROUND_REASON: &str = "lifecycle.background_task_complete";

impl TaskCardEvent {
    pub(in crate::services::discord) fn footer_only_marker_content(&self) -> String {
        use super::super::tui_task_card as card;
        let TaskCardPayload::Task(note) = &self.payload else {
            return "-# 📋 Task notification".to_string();
        };
        let status = super::clean_line(note.status.as_deref().unwrap_or(""));
        let summary = card::decode_entities_once(note.summary.as_deref().unwrap_or(""));
        let summary = summary
            .lines()
            .filter(|line| {
                ![
                    "<task-notification>",
                    "<task-id>",
                    "<tool-use-id>",
                    "<output-file>",
                ]
                .iter()
                .any(|anchor| line.contains(anchor))
            })
            .collect::<Vec<_>>()
            .join(" ");
        let summary = super::clean_line(&summary);
        let detail = if summary.is_empty() {
            if status.is_empty() {
                "Task notification".to_string()
            } else {
                format!("Task {status}")
            }
        } else {
            summary
        };
        let content = format!(
            "-# {} {}",
            card::status_icon(&status),
            card::sanitize_oneline(&detail)
        );
        card::clamp_discord_message_content(&card::truncate_chars_ascii(
            &content,
            FOOTER_ONLY_MARKER_DETAIL_LIMIT,
        ))
    }

    /// Keep the semantic key across both producers, restarts, and older TTL rows.
    pub(in crate::services::discord) async fn enqueue_footer_only_marker(
        &self,
        pool: Option<&sqlx::PgPool>,
    ) -> Result<i64, String> {
        let pool =
            pool.ok_or_else(|| "background marker requires the PostgreSQL outbox".to_string())?;
        let target = format!("channel:{}", self.scope.channel_id);
        let session_key = super::footer_background_marker_session_key(
            super::super::ChannelId::new(self.scope.channel_id),
            self.event_key(),
        );
        let content = self.footer_only_marker_content();
        let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
        let id = message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
            &mut tx,
            OutboxMessage {
                target: &target,
                content: &content,
                bot: &self.scope.provider,
                source: message_outbox::LIFECYCLE_NOTIFIER_SOURCE,
                reason_code: Some(BACKGROUND_REASON),
                session_key: Some(&session_key),
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        // Refresh unsent legacy notices without rewriting a confirmed delivery.
        sqlx::query(
            "UPDATE message_outbox SET content = $2, bot = $3 WHERE id = $1 AND status = 'pending'",
        )
        .bind(id)
        .bind(&content)
        .bind(&self.scope.provider)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        Ok(id)
    }
}
