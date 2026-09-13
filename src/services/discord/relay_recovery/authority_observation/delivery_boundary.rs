//! A2 operation outcomes, emitted after the real decision (never at loop exit).
use super::*;
use crate::services::discord::outbound::delivery_frontier_probe::CurrentGenerationAnchor;
use crate::services::discord::outbound::delivery_record::ExactJsonlSourceIdentity;
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) struct TerminalReceiptDecisionRecord<'a> {
    pub provider: &'a ProviderKind,
    pub channel_id: u64,
    pub turn_id: u64,
    pub source: Option<&'a ExactJsonlSourceIdentity>,
    pub anchor: Option<CurrentGenerationAnchor>,
    pub current_message_id: u64,
    pub frontier_already_covers: Option<bool>,
    pub disposition: &'static str,
}

/// Consume the terminal gate's result; no independent authority or receipt read.
pub(in crate::services::discord) fn record_terminal_receipt_decision(
    record: TerminalReceiptDecisionRecord<'_>,
) {
    record_operation(
        record.provider,
        record.channel_id,
        "completion_terminal_receipt",
        serde_json::json!({
            "turn_id": record.turn_id,
            "source": record.source,
            "anchor": record.anchor.map(|anchor| serde_json::json!({
                "channel_id": anchor.panel_channel_id, "message_id": anchor.panel_msg_id,
                "range": anchor.range,
            })),
            "current_message_id": record.current_message_id,
            "frontier_already_covers": record.frontier_already_covers,
            "disposition": record.disposition,
        }),
    );
}

/// Only the caller's known unbound candidate belongs here, never a foreign anchor.
pub(in crate::services::discord) fn record_unbound_anchor_cleanup(
    provider: &ProviderKind,
    channel_id: u64,
    message_id: u64,
    delete_failed: bool,
) {
    record_operation(
        provider,
        channel_id,
        "completion_unbound_anchor_cleanup",
        serde_json::json!({
            "current_message_id": message_id,
            "unbound_anchor_left": delete_failed,
            "recovery_enqueue_attempted": delete_failed,
            // enqueue() returns (), so an attempt is not a durable recovery receipt.
            "recovery_enqueued": null,
        }),
    );
}

fn record_operation(provider: &ProviderKind, channel_id: u64, site: &str, mut event: Value) {
    let Some((mode, percent)) = observing_dial(channel_id) else {
        return;
    };
    event.as_object_mut().expect("operation fields").extend(
        serde_json::json!({
            "schema": OBSERVATION_SCHEMA,
            "site": site,
            "publish_reason": "operation_result",
            "host": std::env::var("HOSTNAME").unwrap_or_else(|_| "local".to_string()),
            "process_generation": runtime_store::process_generation(),
            "provider": provider.as_str(),
            "channel_id": channel_id,
            "observed_at": chrono::Local::now().to_rfc3339(),
            "cohort_fingerprint": cohort::cohort_fingerprint(mode, percent),
        })
        .as_object()
        .expect("operation stamp")
        .clone(),
    );
    match serde_json::to_string(&event) {
        Ok(line) => append_jsonl(&[line], &COMPLETION_SINK_DROPPED_RECORDS),
        Err(_) => drop_records(&COMPLETION_SINK_DROPPED_RECORDS, 1),
    }
}
