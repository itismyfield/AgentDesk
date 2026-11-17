//! Stable Discord create nonce for the routine failure incident row, so a post repeated for the
//! same durable row resolves to one Discord message. Only the trusted outbox path may use it.
use super::ManualOutboundDeliveryId;
use crate::services::discord::outbound::outbound_fingerprint;

const SOURCE: &str = "routine-runtime";
const CORRELATION_PREFIX: &str = "message_outbox:routine-runtime:routine_failure_incident:";

pub(super) fn enabled(authorized_outbox_path: bool, source: &str) -> bool {
    authorized_outbox_path && source == SOURCE
}

pub(super) fn nonce(
    enabled: bool,
    source: &str,
    delivery_id: ManualOutboundDeliveryId<'_>,
) -> Option<String> {
    if !enabled || source != SOURCE || !delivery_id.correlation_id.starts_with(CORRELATION_PREFIX) {
        return None;
    }
    let row_id = super::headless_nonce::durable_outbox_row_id(source, delivery_id)?;
    Some(outbound_fingerprint(&[
        "routine-incident-message-outbox-delivery",
        &row_id.to_string(),
    ]))
}

#[cfg(test)]
mod tests {
    use super::{ManualOutboundDeliveryId, enabled, nonce};

    fn id<'a>(correlation_id: &'a str, semantic_event_id: &'a str) -> ManualOutboundDeliveryId<'a> {
        ManualOutboundDeliveryId {
            correlation_id,
            semantic_event_id,
        }
    }

    #[test]
    fn incident_nonce_is_stable_per_row_and_refused_off_the_trusted_incident_path() {
        let incident = "message_outbox:routine-runtime:routine_failure_incident:routine:r1:failure_incident:run-1";
        let first = nonce(
            true,
            "routine-runtime",
            id(incident, "message_outbox:41:deliver"),
        );
        assert!(first.is_some());
        assert_eq!(
            first,
            nonce(
                true,
                "routine-runtime",
                id(incident, "message_outbox:041:deliver")
            )
        );
        assert_ne!(
            first,
            nonce(
                true,
                "routine-runtime",
                id(incident, "message_outbox:42:deliver")
            )
        );
        let notice = "message_outbox:routine-runtime:routine_headless_failure:routine:r1:x";
        assert_eq!(
            nonce(
                true,
                "routine-runtime",
                id(notice, "message_outbox:41:deliver")
            ),
            None
        );
        assert_eq!(
            nonce(
                false,
                "routine-runtime",
                id(incident, "message_outbox:41:deliver")
            ),
            None
        );
        assert_eq!(
            nonce(
                true,
                "headless_turn",
                id(incident, "message_outbox:41:deliver")
            ),
            None
        );
        assert!(enabled(true, "routine-runtime"));
        assert!(!enabled(false, "routine-runtime"));
        assert!(!enabled(true, "headless_turn"));
    }
}
