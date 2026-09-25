//! Versioned durable encoding of a manual-steer operation. Decoding fails closed: an unknown
//! version, unknown or missing field, or invalid identity is an error, never a default.

use serde::{Deserialize, Serialize};

use super::operation::ManualSteerOperation;

pub(crate) const RECORD_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecordError {
    UnsupportedVersion(u32),
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

pub(crate) fn encode_record(operation: &ManualSteerOperation) -> Vec<u8> {
    serde_json::to_vec(&RecordOut {
        schema_version: RECORD_SCHEMA_VERSION,
        operation,
    })
    .expect("manual-steer record contains only JSON-safe values")
}

pub(crate) fn decode_record(bytes: &[u8]) -> Result<ManualSteerOperation, RecordError> {
    let malformed = |err: serde_json::Error| RecordError::Malformed(err.to_string());
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(malformed)?;
    if probe.schema_version != RECORD_SCHEMA_VERSION {
        return Err(RecordError::UnsupportedVersion(probe.schema_version));
    }
    let record: RecordIn = serde_json::from_slice(bytes).map_err(malformed)?;
    validate_identity(&record.operation)?;
    Ok(record.operation)
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
        serde_json::from_slice(&encode_record(operation)).unwrap()
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
            assert_eq!(decode_record(&encode_record(&bare)), Ok(bare.clone()));

            let mut observed = bare;
            observed.evidence = EvidenceState {
                consumed_in_a: true,
                a_episode_ended: true,
                delivery_failed: true,
                no_evidence_since_ms: Some(1_700_000_000_500),
            };
            observed.wire_digest = Some(Bytes256::digest_of(b"wire"));
            assert_eq!(decode_record(&encode_record(&observed)), Ok(observed));
        }
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
    }
}
