//! Durable observation metadata has no enqueue, retry, or output authority.
use serde_json::{Value, json};
use sqlx::{Acquire, Postgres, Transaction};

/// Capture provenance only; enqueue success is never a transport observation.
pub(super) fn capture(source: &str) -> Option<Value> {
    if source != "headless_turn" {
        return crate::services::discord::outbound::source_registry::is_registered_nonbody_source(
            source,
        )
        .then(|| {
            json!({"schema_version":1,"notice_kind":source,
                "producer":crate::services::legacy_body_census::producer_identity()})
        });
    }
    crate::services::discord::outbound::current_census().map(|tag| {
        json!({"schema_version":1,"tag":tag,
            "producer":crate::services::legacy_body_census::producer_identity()})
    })
}

/// Only persisted producer evidence can establish a nonbody exclusion.
pub(crate) fn delivery_tag(source: &str, stored: Option<Value>) -> Value {
    if let Some(tag) = stored.as_ref().and_then(restore) {
        return tag;
    }
    json!({"site":if source=="headless_turn" { json!(crate::services::tui_o::cutover::BodySite::BridgeHeadless) } else { Value::Null },
        "output_class":"UnknownBody","binding_mode":"UNKNOWN","actual_runtime":"unknown",
        "session_temperature":"unknown","source_incarnation":null,"producer_boot_id":null,
        "producer_host":null,"producer_process_host":null,"input_origin":"unknown",
        "body_owner":"Legacy","metadata_status":"missing_or_unrecognized"})
}

// Unknown or incomplete envelopes cannot establish a non-TUI exclusion.
fn restore(stored: &Value) -> Option<Value> {
    let envelope = stored.as_object()?;
    let producer = stored["producer"].as_object()?;
    let nullable_string =
        |value: Option<&Value>| value.is_some_and(|v| v.is_null() || v.is_string());
    if envelope.len() != 3
        || stored["schema_version"].as_u64() != Some(1)
        || producer.len() != 2
        || !producer
            .get("boot_id")
            .and_then(Value::as_str)
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        || !nullable_string(producer.get("host"))
    {
        return None;
    }
    if let Some(kind) = stored.get("notice_kind").and_then(Value::as_str) {
        return crate::services::discord::outbound::source_registry::is_static_notice_kind(kind)
            .then(|| json!({"output_class":"NonBody"}));
    }
    let mut tag = stored["tag"].as_object()?.clone();
    let present_string = |key: &str| {
        tag.get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
    };
    if !["site", "actual_runtime", "input_origin", "reason"]
        .into_iter()
        .all(present_string)
        || !["producer_host", "provider"]
            .into_iter()
            .all(|key| nullable_string(tag.get(key)))
        || !tag
            .get("source_incarnation")
            .is_some_and(|v| v.is_null() || v.is_object())
        || !matches!(
            tag.get("binding_mode").and_then(Value::as_str),
            Some("TUI" | "NON_TUI" | "UNKNOWN")
        )
        || !matches!(
            tag.get("output_class").and_then(Value::as_str),
            Some("LegacyBody" | "RecoveredLegacyBody" | "UnknownBody" | "NonBody")
        )
        || !matches!(
            tag.get("session_temperature").and_then(Value::as_str),
            Some("warm" | "cold" | "unknown")
        )
        || tag.get("body_owner").and_then(Value::as_str) != Some("Legacy")
        || tag.get("producer_boot_id").is_some_and(|v| {
            !v.is_null()
                && !v
                    .as_str()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        })
        || tag
            .get("producer_process_host")
            .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return None;
    }
    // Keep an earlier producer stamp when an already restored tag is forwarded again.
    tag.entry("producer_boot_id")
        .or_insert_with(|| stored["producer"]["boot_id"].clone());
    tag.entry("producer_process_host")
        .or_insert_with(|| stored["producer"]["host"].clone());
    Some(Value::Object(tag))
}

/// Isolate optional-column failures without aborting the original enqueue transaction.
/// A failed savepoint BEGIN makes sqlx roll back the outer transaction, so that error ends the enqueue.
pub(super) async fn store(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    census: Option<&Value>,
) -> Result<(), sqlx::Error> {
    let Some(census) = census else {
        return Ok(());
    };
    let mut observation = tx.begin().await?;
    if sqlx::query("UPDATE message_outbox SET body_census=$2 WHERE id=$1")
        .bind(id)
        .bind(census)
        .execute(&mut *observation)
        .await
        .is_err()
    {
        // A failed rollback or release still leaves sqlx's queued ROLLBACK TO SAVEPOINT at the
        // outer depth, so the caller's next statement or COMMIT reaches the server and reports.
        if observation.rollback().await.is_err() {
            tracing::warn!(outbox_id = id, "outbox observation rollback failed");
        }
        tracing::warn!(
            outbox_id = id,
            "outbox observation not stored; delivery will remain unknown"
        );
    } else if observation.commit().await.is_err() {
        tracing::warn!(
            outbox_id = id,
            "outbox observation savepoint release failed"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "body_census_tests.rs"]
mod tests;
