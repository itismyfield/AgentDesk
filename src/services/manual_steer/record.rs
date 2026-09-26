//! Versioned durable encoding of a manual-steer operation. Decoding fails closed: an oversized or
//! noncanonical record, unknown version, unknown or missing field, or invalid identity is an error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::operation::ManualSteerOperation;

pub(crate) const RECORD_SCHEMA_VERSION: u32 = 1;

/// Queue merging folds consecutive same-author messages without a count limit; this is far past
/// any human burst and bounds what one operation may name.
pub(crate) const MAX_SOURCES: usize = 1024;

/// A source encodes to at most 77 bytes (budgeted at 80); 2 KiB covers every fixed field.
const MAX_RECORD_BYTES: usize = 2048 + MAX_SOURCES * 80;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecordError {
    UnsupportedVersion(u32),
    /// Over the byte budget (refused before any parse) or the source budget.
    TooLarge,
    Malformed(String),
}

#[derive(Serialize)]
struct RecordOut<'a> {
    schema_version: u32,
    operation: &'a ManualSteerOperation,
}

#[derive(Deserialize)]
struct VersionProbe {
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordIn {
    #[serde(rename = "schema_version")]
    _schema_version: u32,
    operation: ManualSteerOperation,
}

/// Refuses only an operation over the source budget; identity is validated on decode, not here.
pub(crate) fn encode_record(operation: &ManualSteerOperation) -> Result<Vec<u8>, RecordError> {
    check_source_budget(operation)?;
    Ok(serde_json::to_vec(&record_out(operation))
        .expect("manual-steer record contains only JSON-safe values"))
}

pub(crate) fn decode_record(bytes: &[u8]) -> Result<ManualSteerOperation, RecordError> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(RecordError::TooLarge);
    }
    let malformed = |err: serde_json::Error| RecordError::Malformed(err.to_string());
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(malformed)?;
    if probe.schema_version != RECORD_SCHEMA_VERSION {
        return Err(RecordError::UnsupportedVersion(probe.schema_version));
    }
    // Typed parse runs on the raw bytes so duplicate keys stay an error.
    let record: RecordIn = serde_json::from_slice(bytes).map_err(malformed)?;
    check_source_budget(&record.operation)?;
    validate_identity(&record.operation)?;
    // Serde also accepts positional arrays for structs; only the object form re-encodes equal.
    let canonical = serde_json::to_value(record_out(&record.operation))
        .expect("manual-steer record contains only JSON-safe values");
    if serde_json::from_slice::<Value>(bytes).map_err(malformed)? != canonical {
        return Err(RecordError::Malformed(
            "noncanonical record shape".to_string(),
        ));
    }
    Ok(record.operation)
}

fn record_out(operation: &ManualSteerOperation) -> RecordOut<'_> {
    RecordOut {
        schema_version: RECORD_SCHEMA_VERSION,
        operation,
    }
}

fn check_source_budget(operation: &ManualSteerOperation) -> Result<(), RecordError> {
    if operation.identity.sources.len() > MAX_SOURCES {
        return Err(RecordError::TooLarge);
    }
    Ok(())
}

fn validate_identity(operation: &ManualSteerOperation) -> Result<(), RecordError> {
    let identity = &operation.identity;
    let reject = |reason: &str| Err(RecordError::Malformed(reason.to_string()));
    if identity.channel_id == 0 || identity.entry_id == 0 || identity.card.card_message_id == 0 {
        return reject("zero discord id");
    }
    if identity.sources.is_empty() {
        return reject("operation has no source message");
    }
    if identity.sources.iter().any(|source| source.message_id == 0) {
        return reject("zero source message id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::services::manual_steer::action_handle::ActionHandle;
    use crate::services::manual_steer::operation::{
        Bytes256, CardBinding, EvidenceState, OperationIdentity, OperationStage, Settlement,
        SourceRef, SubmitAck, UnresolvedReason,
    };

    fn operation(stage: OperationStage) -> ManualSteerOperation {
        ManualSteerOperation {
            identity: OperationIdentity {
                channel_id: 11,
                entry_id: 22,
                entry_version: 3,
                payload_digest: Bytes256::digest_of(b"[User: a (ID: 7)] hello"),
                sources: vec![
                    SourceRef {
                        message_id: 22,
                        queued_generation: 4,
                    },
                    SourceRef {
                        message_id: 23,
                        queued_generation: 5,
                    },
                ],
                ordinal: 2,
                a_episode: Bytes256::from_bytes([9; 32]),
                runtime_incarnation: 6,
                card: CardBinding {
                    card_message_id: 33,
                    epoch: 1,
                },
                action: ActionHandle::from_bytes([5; 16]).unwrap(),
            },
            stage,
            evidence: EvidenceState::default(),
            permit_expires_at_ms: 1_700_000_000_000,
            wire_digest: None,
        }
    }

    fn encoded_json(operation: &ManualSteerOperation) -> Value {
        serde_json::from_slice(&encode_record(operation).unwrap()).unwrap()
    }

    fn decode_json(value: &Value) -> Result<ManualSteerOperation, RecordError> {
        decode_record(&serde_json::to_vec(value).unwrap())
    }

    #[test]
    fn record_round_trips_every_stage_and_optional_evidence() {
        let mut stages = vec![
            OperationStage::Queued,
            OperationStage::Preparing,
            OperationStage::Reserved,
            OperationStage::MutationArmed {
                enter_attempted: false,
            },
            OperationStage::MutationArmed {
                enter_attempted: true,
            },
            OperationStage::Watching(SubmitAck::AcceptedOrQueued),
            OperationStage::Watching(SubmitAck::Uncertain),
            OperationStage::VerifiedNotDelivered,
            OperationStage::Retired(Settlement::ConsumedInA),
            OperationStage::Retired(Settlement::SeparateDelivered),
        ];
        stages.extend(
            [
                UnresolvedReason::DeliveryFailed,
                UnresolvedReason::SubmitUnknown,
                UnresolvedReason::ConsumptionUnknown,
                UnresolvedReason::Revoked,
            ]
            .map(|reason| OperationStage::Retired(Settlement::Unresolved(reason))),
        );
        for stage in stages {
            let bare = operation(stage);
            assert_eq!(
                decode_record(&encode_record(&bare).unwrap()),
                Ok(bare.clone())
            );

            let mut observed = bare;
            observed.evidence = EvidenceState {
                consumed_in_a: true,
                a_episode_ended: true,
                delivery_failed: true,
                no_evidence_since_ms: Some(1_700_000_000_500),
            };
            observed.wire_digest = Some(Bytes256::digest_of(b"wire"));
            assert_eq!(
                decode_record(&encode_record(&observed).unwrap()),
                Ok(observed)
            );
        }
    }

    #[test]
    fn record_budget_admits_the_widest_record_and_rejects_past_each_limit() {
        let mut widest = operation(OperationStage::Retired(Settlement::Unresolved(
            UnresolvedReason::ConsumptionUnknown,
        )));
        let identity = &mut widest.identity;
        identity.channel_id = u64::MAX;
        identity.entry_id = u64::MAX;
        identity.entry_version = u64::MAX;
        identity.ordinal = u64::MAX;
        identity.runtime_incarnation = u64::MAX;
        identity.card = CardBinding {
            card_message_id: u64::MAX,
            epoch: u64::MAX,
        };
        let widest_source = SourceRef {
            message_id: u64::MAX,
            queued_generation: u64::MAX,
        };
        identity.sources = vec![widest_source; MAX_SOURCES];
        widest.evidence = EvidenceState {
            consumed_in_a: true,
            a_episode_ended: true,
            delivery_failed: true,
            no_evidence_since_ms: Some(i64::MIN),
        };
        widest.permit_expires_at_ms = i64::MIN;
        widest.wire_digest = Some(Bytes256::digest_of(b"wire"));
        let mut bytes = encode_record(&widest).unwrap();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        bytes.resize(MAX_RECORD_BYTES, b' ');
        assert_eq!(decode_record(&bytes), Ok(widest.clone()));
        bytes.push(b' ');
        assert_eq!(decode_record(&bytes), Err(RecordError::TooLarge));

        let mut crowded = operation(OperationStage::Queued);
        crowded.identity.sources = (1..=MAX_SOURCES as u64 + 1)
            .map(|message_id| SourceRef {
                message_id,
                queued_generation: 1,
            })
            .collect();
        assert_eq!(encode_record(&crowded), Err(RecordError::TooLarge));
        let mut value = encoded_json(&operation(OperationStage::Queued));
        value["operation"]["identity"]["sources"] = json!(crowded.identity.sources);
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(bytes.len() <= MAX_RECORD_BYTES);
        assert_eq!(decode_record(&bytes), Err(RecordError::TooLarge));
    }

    #[test]
    fn record_decode_rejects_unknown_or_missing_schema_version() {
        let mut value = encoded_json(&operation(OperationStage::Reserved));
        value["schema_version"] = json!(2);
        assert_eq!(decode_json(&value), Err(RecordError::UnsupportedVersion(2)));

        value.as_object_mut().unwrap().remove("schema_version");
        assert!(matches!(
            decode_json(&value),
            Err(RecordError::Malformed(_))
        ));
    }

    #[test]
    fn record_decode_fails_closed_on_missing_unknown_or_invalid_fields() {
        let base = encoded_json(&operation(OperationStage::Watching(SubmitAck::Uncertain)));
        type Edit = (&'static str, fn(&mut Value));
        let edits: Vec<Edit> = vec![
            ("missing optional wire digest", |v| {
                v["operation"]
                    .as_object_mut()
                    .unwrap()
                    .remove("wire_digest");
            }),
            ("missing optional no-evidence clock", |v| {
                v["operation"]["evidence"]
                    .as_object_mut()
                    .unwrap()
                    .remove("no_evidence_since_ms");
            }),
            ("missing nonce", |v| {
                v["operation"]["identity"]
                    .as_object_mut()
                    .unwrap()
                    .remove("action");
            }),
            ("unknown top-level field", |v| v["extra"] = json!(1)),
            ("unknown identity field", |v| {
                v["operation"]["identity"]["extra"] = json!(1)
            }),
            ("unknown stage", |v| {
                v["operation"]["stage"] = json!("settled")
            }),
            ("unknown unresolved reason", |v| {
                v["operation"]["stage"] = json!({"retired": {"unresolved": "timeout"}});
            }),
            ("truncated payload digest", |v| {
                let digest = v["operation"]["identity"]["payload_digest"]
                    .as_str()
                    .unwrap();
                v["operation"]["identity"]["payload_digest"] = json!(digest[..32].to_string());
            }),
            ("uppercase payload digest", |v| {
                let digest = v["operation"]["identity"]["payload_digest"]
                    .as_str()
                    .unwrap();
                v["operation"]["identity"]["payload_digest"] = json!(digest.to_uppercase());
            }),
            ("zero action handle", |v| {
                v["operation"]["identity"]["action"] = json!("0".repeat(32));
            }),
            ("no source message", |v| {
                v["operation"]["identity"]["sources"] = json!([])
            }),
            ("zero source message id", |v| {
                v["operation"]["identity"]["sources"][0]["message_id"] = json!(0);
            }),
            ("zero channel id", |v| {
                v["operation"]["identity"]["channel_id"] = json!(0)
            }),
            ("zero card id", |v| {
                v["operation"]["identity"]["card"]["card_message_id"] = json!(0);
            }),
            ("positional evidence", |v| {
                v["operation"]["evidence"] = json!([true, false, false, null]);
            }),
            ("positional card binding", |v| {
                v["operation"]["identity"]["card"] = json!([33, 1])
            }),
            ("positional source", |v| {
                v["operation"]["identity"]["sources"][0] = json!([22, 4])
            }),
            ("positional stage fields", |v| {
                v["operation"]["stage"] = json!({"mutation_armed": [true]})
            }),
        ];
        assert!(
            decode_json(&base).is_ok(),
            "fixture must decode before edits"
        );
        for (label, edit) in edits {
            let mut value = base.clone();
            edit(&mut value);
            assert!(
                matches!(decode_json(&value), Err(RecordError::Malformed(_))),
                "{label} must be rejected"
            );
        }
        // A repeated key is rejected, never resolved last-wins.
        let text = serde_json::to_string(&base).unwrap().replacen(
            "\"channel_id\":11",
            "\"channel_id\":0,\"channel_id\":11",
            1,
        );
        assert!(matches!(
            decode_record(text.as_bytes()),
            Err(RecordError::Malformed(_))
        ));
    }
}
