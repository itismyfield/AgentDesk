use super::ManualOutboundDeliveryId;
use crate::services::discord::outbound::outbound_fingerprint;

const MESSAGE_OUTBOX_CORRELATION_PREFIX: &str = "message_outbox:";
const MESSAGE_OUTBOX_SEMANTIC_PREFIX: &str = "message_outbox:";
const MESSAGE_OUTBOX_SEMANTIC_SUFFIX: &str = ":deliver";
const HEADLESS_DISCORD_NONCE_FLAG: &str = "AGENTDESK_HEADLESS_DISCORD_NONCE";

pub(super) fn durable_outbox_delivery_nonce(
    enabled: bool,
    source: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> Option<String> {
    resolve_durable_outbox_delivery_nonce(enabled, source, delivery_id)
}

pub(super) fn durable_outbox_nonce_enabled(authorized_outbox_path: bool) -> bool {
    resolve_rollout_enabled(
        authorized_outbox_path,
        resolve_opt_in_flag(std::env::var(HEADLESS_DISCORD_NONCE_FLAG).ok().as_deref()),
    )
}

fn resolve_rollout_enabled(authorized_outbox_path: bool, rollout_flag_enabled: bool) -> bool {
    authorized_outbox_path && rollout_flag_enabled
}

/// Resolves the nonce for any durable `message_outbox` delivery.
///
/// Keeping the identity tied to the outbox row, rather than to the rendered
/// content or transport shape, makes retries converge for both inline text and
/// binary-attachment sends.
pub(super) fn resolve_durable_outbox_delivery_nonce(
    enabled: bool,
    source: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> Option<String> {
    enabled.then_some(())?;
    let row_id = durable_message_outbox_row_id(source, delivery_id)?;
    Some(outbound_fingerprint(&[
        "headless-message-outbox-delivery",
        &row_id.to_string(),
    ]))
}

fn resolve_opt_in_flag(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| {
        let value = value.trim();
        value == "1" || value.eq_ignore_ascii_case("true")
    })
}

fn durable_message_outbox_row_id(
    source: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> Option<i64> {
    // `PendingMessageOutboxRow::delivery_ids` owns both shapes. Require the
    // source segment to match as well, so a trusted caller cannot accidentally
    // reuse a nonce for a different outbox producer.
    let correlation_prefix = format!("{MESSAGE_OUTBOX_CORRELATION_PREFIX}{}:", source.trim());
    if source.trim().is_empty() || !delivery_id.correlation_id.starts_with(&correlation_prefix) {
        return None;
    }
    let row_id = delivery_id
        .semantic_event_id
        .strip_prefix(MESSAGE_OUTBOX_SEMANTIC_PREFIX)?
        .strip_suffix(MESSAGE_OUTBOX_SEMANTIC_SUFFIX)?;
    if row_id.is_empty() || !row_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    row_id.parse::<i64>().ok().filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery_id(
        correlation_id: &'static str,
        semantic_event_id: &'static str,
    ) -> ManualOutboundDeliveryId<'static> {
        ManualOutboundDeliveryId {
            correlation_id,
            semantic_event_id,
        }
    }

    #[test]
    fn flag_is_compiled_default_off_and_only_explicitly_enabled() {
        assert!(!resolve_opt_in_flag(None));
        assert!(!resolve_opt_in_flag(Some("0")));
        assert!(!resolve_opt_in_flag(Some("false")));
        assert!(resolve_opt_in_flag(Some("1")));
        assert!(resolve_opt_in_flag(Some(" TRUE ")));
        assert!(!resolve_rollout_enabled(false, true));
        assert!(!resolve_rollout_enabled(true, false));
        assert!(resolve_rollout_enabled(true, true));
    }

    #[test]
    fn durable_identity_accepts_only_complete_outbox_delivery_ids() {
        let complete = delivery_id(
            "message_outbox:headless_turn:terminal:session-a",
            "message_outbox:41:deliver",
        );
        assert_eq!(
            durable_message_outbox_row_id("headless_turn", complete),
            Some(41)
        );
        assert_eq!(
            durable_message_outbox_row_id("lifecycle_notifier", complete),
            None
        );
        assert_eq!(
            durable_message_outbox_row_id(
                "headless_turn",
                delivery_id("manual:headless_turn", "message_outbox:41:deliver"),
            ),
            None
        );
        assert_eq!(
            durable_message_outbox_row_id(
                "headless_turn",
                delivery_id("message_outbox:headless_turn:terminal", "message_outbox:41"),
            ),
            None
        );
        assert_eq!(
            durable_message_outbox_row_id(
                "headless_turn",
                delivery_id(
                    "message_outbox:headless_turn:terminal",
                    "message_outbox:not-a-row:deliver",
                ),
            ),
            None
        );
    }

    #[test]
    fn nonce_requires_flag_source_and_complete_durable_identity() {
        let complete = delivery_id(
            "message_outbox:headless_turn:terminal:session-a",
            "message_outbox:41:deliver",
        );
        assert!(resolve_durable_outbox_delivery_nonce(true, "headless_turn", complete).is_some());
        let scheduled = delivery_id(
            "message_outbox:scheduled_message:42",
            "message_outbox:42:deliver",
        );
        assert!(
            resolve_durable_outbox_delivery_nonce(true, "scheduled_message", scheduled).is_some()
        );
        assert_eq!(
            resolve_durable_outbox_delivery_nonce(false, "headless_turn", complete),
            None
        );
        assert_eq!(
            resolve_durable_outbox_delivery_nonce(true, "lifecycle_notifier", complete),
            None
        );
        assert_eq!(
            resolve_durable_outbox_delivery_nonce(
                true,
                "headless_turn",
                delivery_id("manual:headless_turn", "message_outbox:41:deliver"),
            ),
            None
        );
    }

    #[test]
    fn inline_nonce_is_stable_by_parsed_durable_row_across_retry_variations() {
        let first = resolve_durable_outbox_delivery_nonce(
            true,
            "headless_turn",
            delivery_id(
                "message_outbox:headless_turn:reason-a:session-a",
                "message_outbox:41:deliver",
            ),
        )
        .expect("eligible durable row");
        let changed_correlation_and_raw_spelling = resolve_durable_outbox_delivery_nonce(
            true,
            "headless_turn",
            delivery_id(
                "message_outbox:headless_turn:reason-b:session-b",
                "message_outbox:041:deliver",
            ),
        )
        .expect("same parsed durable row");
        let other_row = resolve_durable_outbox_delivery_nonce(
            true,
            "headless_turn",
            delivery_id(
                "message_outbox:headless_turn:reason-a:session-a",
                "message_outbox:42:deliver",
            ),
        )
        .expect("distinct durable row");

        // Content is deliberately absent from the resolver contract: callers
        // may retry the same durable row after mutable content/correlation
        // metadata changes, but its Discord nonce must remain stable.
        assert_eq!(first, changed_correlation_and_raw_spelling);
        assert_ne!(first, other_row);
        assert_eq!(first.len(), 16);
    }

    #[test]
    fn durable_row_parser_rejects_non_positive_and_overflow_identities() {
        for semantic_event_id in [
            "message_outbox:0:deliver",
            "message_outbox:-1:deliver",
            "message_outbox:9223372036854775808:deliver",
        ] {
            assert_eq!(
                resolve_durable_outbox_delivery_nonce(
                    true,
                    "headless_turn",
                    delivery_id(
                        "message_outbox:headless_turn:terminal:session-a",
                        semantic_event_id,
                    ),
                ),
                None,
                "{semantic_event_id} must not authorize a nonce",
            );
        }
    }
}
