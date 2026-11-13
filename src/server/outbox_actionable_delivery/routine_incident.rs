//! Delivery of the routine failure incident row: post at most once, ack only on a real Discord
//! message id, and close every uncertain outcome as `unknown` instead of retrying it.
use sqlx::PgPool;

use super::super::{MessageOutboxFailureAction, PendingMessageOutboxRow};
use crate::services::message_outbox::routine_incident::{self, Lease};

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Ack(String),
    /// Refused before any Discord request was made.
    NotSent,
    MayHaveSent,
}

/// Statuses the send path returns before it reaches Discord.
const PRE_POST_REFUSALS: [&str; 4] = [
    "400 Bad Request",
    "409 Conflict",
    "retired_source",
    "invalid_attachment",
];

fn outcome(status: &str, body: &str) -> Outcome {
    if status == "200 OK" {
        let response = serde_json::from_str::<serde_json::Value>(body).ok();
        let skipped = response
            .as_ref()
            .and_then(|value| value.get("delivery"))
            .and_then(serde_json::Value::as_str)
            == Some("skipped");
        let message_id = response
            .as_ref()
            .and_then(|value| value.get("message_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()));
        return match message_id {
            Some(id) if !skipped => Outcome::Ack(id.to_string()),
            _ => Outcome::MayHaveSent,
        };
    }
    if PRE_POST_REFUSALS.contains(&status) {
        Outcome::NotSent
    } else {
        Outcome::MayHaveSent
    }
}

pub(in crate::server) async fn deliver<F, Fut>(
    pool: &PgPool,
    row: &PendingMessageOutboxRow,
    send: F,
) where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let lease = Lease {
        id: row.id,
        claim_owner: &row.claim_owner,
        claimed_at: row.claimed_at,
    };
    match routine_incident::begin_posting(pool, lease).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            tracing::warn!(outbox_id = row.id, %error, "[outbox] incident posting not stored; send deferred");
            return;
        }
    }
    let (status, body) = send().await;
    let settled = match outcome(&status, &body) {
        Outcome::Ack(message_id) => match routine_incident::ack(pool, lease, &message_id).await {
            Ok(true) => Ok(true),
            _ => routine_incident::mark_unknown(pool, lease, "ack not stored after post").await,
        },
        Outcome::MayHaveSent => {
            routine_incident::mark_unknown(pool, lease, &format!("{status}: {body}")).await
        }
        Outcome::NotSent => {
            let retry = match super::failure_action(row, &status) {
                MessageOutboxFailureAction::Retry {
                    retry_count,
                    backoff_secs,
                } => Some((retry_count, backoff_secs)),
                MessageOutboxFailureAction::Fail { .. } => None,
            };
            routine_incident::release_not_sent(pool, lease, &format!("{status}: {body}"), retry)
                .await
        }
    };
    if !matches!(settled, Ok(true)) {
        // The row stays `posting`; the abandoned-posting sweep closes it as unknown.
        tracing::warn!(outbox_id = row.id, %status, ?settled, "[outbox] incident outcome not stored");
    }
}

#[cfg(test)]
mod tests {
    use super::{Outcome, outcome};

    #[test]
    fn only_a_real_message_id_acks_and_only_pre_post_refusals_may_retry() {
        let ok = |body: &str| outcome("200 OK", body);
        assert_eq!(
            ok(r#"{"ok":true,"message_id":"1234"}"#),
            Outcome::Ack("1234".into())
        );
        assert_eq!(
            ok(r#"{"ok":true,"message_id":"1234","delivery":"duplicate"}"#),
            Outcome::Ack("1234".into())
        );
        for body in [
            r#"{"ok":true,"message_id":""}"#,
            r#"{"ok":true,"message_id":"","delivery":"skipped"}"#,
            r#"{"ok":true,"message_id":"1234","delivery":"skipped"}"#,
            r#"{"ok":true}"#,
            "not json",
        ] {
            assert_eq!(ok(body), Outcome::MayHaveSent, "{body}");
        }
        assert_eq!(outcome("409 Conflict", ""), Outcome::NotSent);
        assert_eq!(outcome("400 Bad Request", ""), Outcome::NotSent);
        for status in [
            "500 Internal Server Error",
            "503 Service Unavailable",
            "timeout",
        ] {
            assert_eq!(outcome(status, ""), Outcome::MayHaveSent, "{status}");
        }
    }
}
