//! Atomic Discord + external-provider handoff for one scheduled push fire.

use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use crate::db::scheduled_messages as db;
use crate::db::scheduled_messages::ClaimedFire;
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};

use super::OUTBOX_SOURCE;
use super::external_delivery::{ExternalHandoffError, enqueue_for_message_tx};
use super::timing::compute_resume;

const DEFAULT_EXTERNAL_DELIVERY_WINDOW_HOURS: i64 = 24;

fn render_discord_content(content: &str, user_ids: &[String]) -> String {
    if user_ids.is_empty() {
        return content.to_string();
    }
    let mentions = user_ids
        .iter()
        .map(|user_id| format!("<@{user_id}>"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{mentions}\n{content}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PushHandoffOutcome {
    Committed,
    Canceled,
}

#[derive(Debug, Error)]
pub(super) enum PushHandoffError {
    #[error("scheduled push target is invalid: {0}")]
    Invalid(&'static str),
    #[error("scheduled push handoff failed: {0}")]
    Transient(#[source] anyhow::Error),
}

pub(super) async fn commit(
    pool: &sqlx::PgPool,
    fire: &ClaimedFire,
    now: DateTime<Utc>,
) -> Result<PushHandoffOutcome, PushHandoffError> {
    let message = &fire.message;
    if message.target_channel_id.is_none() && message.provider_targets.is_none() {
        return Err(PushHandoffError::Invalid("no delivery target"));
    }
    let (next, forced_terminal) = compute_resume(
        message.schedule.as_deref(),
        &message.timezone,
        message.scheduled_at,
        message.expires_at,
        now,
    );
    let terminal_status = forced_terminal.unwrap_or(db::STATUS_SENT);
    let next = forced_terminal.is_none().then_some(next).flatten();
    let deliver_before = external_delivery_deadline(message.expires_at, next, now);

    let mut tx = pool.begin().await.map_err(transient)?;
    if !db::lock_active_delivery_tx(&mut tx, &message.id, &fire.delivery_id, &fire.claim_token)
        .await
        .map_err(transient)?
    {
        return Ok(PushHandoffOutcome::Canceled);
    }

    let mut discord_outbox_id = None;
    if let Some(channel_id) = message.target_channel_id.as_deref() {
        let discord_content =
            render_discord_content(&message.content, &message.discord_mention_user_ids);
        let target = format!("channel:{channel_id}");
        let reason_code = format!(
            "scheduled_message:v1:{}:{}",
            message.id,
            fire.fire_scheduled_at.timestamp_micros()
        );
        discord_outbox_id = Some(
            enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
                &mut tx,
                OutboxMessage {
                    target: &target,
                    content: &discord_content,
                    bot: &message.bot,
                    source: OUTBOX_SOURCE,
                    reason_code: Some(&reason_code),
                    session_key: None,
                },
            )
            .await
            .map_err(|error| PushHandoffError::Transient(anyhow::Error::new(error)))?,
        );
    }

    let external_count =
        enqueue_for_message_tx(&mut tx, message, &fire.delivery_id, deliver_before, now)
            .await
            .map_err(map_external_error)?;
    if discord_outbox_id.is_none() && external_count == 0 {
        return Err(PushHandoffError::Invalid("no usable delivery target"));
    }
    let transitioned = db::finish_locked_delivery_and_finalize_parent_tx(
        &mut tx,
        &fire.delivery_id,
        &fire.claim_token,
        db::DELIVERY_SENT,
        None,
        discord_outbox_id,
        None,
        &message.id,
        true,
        terminal_status,
        next,
    )
    .await
    .map_err(transient)?;
    if !transitioned {
        return Ok(PushHandoffOutcome::Canceled);
    }
    tx.commit().await.map_err(transient)?;
    Ok(PushHandoffOutcome::Committed)
}

fn external_delivery_deadline(
    expires_at: Option<DateTime<Utc>>,
    next_scheduled_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> DateTime<Utc> {
    [expires_at, next_scheduled_at]
        .into_iter()
        .flatten()
        .filter(|candidate| *candidate > now)
        .min()
        .unwrap_or(now + Duration::hours(DEFAULT_EXTERNAL_DELIVERY_WINDOW_HOURS))
}

fn transient(error: sqlx::Error) -> PushHandoffError {
    PushHandoffError::Transient(anyhow::Error::new(error))
}

fn map_external_error(error: ExternalHandoffError) -> PushHandoffError {
    match error {
        ExternalHandoffError::InvalidTarget(_) | ExternalHandoffError::Serialization => {
            PushHandoffError::Invalid("stored provider target plan is invalid")
        }
        ExternalHandoffError::Database(error) => transient(error),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn mentions_render_only_for_the_discord_handoff_body() {
        let user_ids = vec![
            "1469509284508340276".to_string(),
            "1469961339920453675".to_string(),
        ];
        assert_eq!(
            render_discord_content("Kakao keeps this exact body", &user_ids),
            "<@1469509284508340276> <@1469961339920453675>\nKakao keeps this exact body"
        );
        assert_eq!(render_discord_content("plain", &[]), "plain");
    }

    #[test]
    fn deadline_prefers_expiry_or_next_recurrence_over_default_window() {
        let now = Utc.with_ymd_and_hms(2026, 8, 27, 0, 0, 0).unwrap();
        let next = now + Duration::minutes(10);
        let expiry = now + Duration::hours(1);
        assert_eq!(
            external_delivery_deadline(Some(expiry), Some(next), now),
            next
        );
        assert_eq!(
            external_delivery_deadline(None, None, now),
            now + Duration::hours(24)
        );
    }
}
