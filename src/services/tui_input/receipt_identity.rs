//! Durable source provenance; rendering enrichment is never receipt identity.

use std::collections::BTreeSet;
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{AbandonReason, RowState, Rows};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptIdentity {
    pub source_ids: Vec<u64>,
    pub author_id: u64,
    pub original_channel_id: u64,
    pub execution_channel_id: u64,
}

impl ReceiptIdentity {
    pub fn new(
        key: u64,
        source_ids: Vec<u64>,
        author_id: u64,
        original_channel_id: u64,
        execution_channel_id: u64,
    ) -> io::Result<Self> {
        let mut identity = Self {
            source_ids,
            author_id,
            original_channel_id,
            execution_channel_id,
        };
        if !identity.validates(key) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "receipt requires complete source provenance including its primary",
            ));
        }
        identity.source_ids.sort_unstable();
        Ok(identity)
    }

    pub fn validates(&self, key: u64) -> bool {
        self.valid() && self.source_ids.contains(&key)
    }

    fn valid(&self) -> bool {
        let mut ids = BTreeSet::new();
        self.author_id != 0
            && self.original_channel_id != 0
            && self.execution_channel_id != 0
            && !self.source_ids.is_empty()
            && self.source_ids.iter().all(|id| *id != 0 && ids.insert(*id))
    }

    pub(super) fn from_input(key: u64, input: &Value) -> Option<Self> {
        #[cfg(test)]
        if mutant("drop_identity") {
            return None;
        }
        if let Some(saved) = input.get("receipt_identity") {
            let identity: Self = serde_json::from_value(saved.clone()).ok()?;
            return Self::new(
                key,
                identity.source_ids,
                identity.author_id,
                identity.original_channel_id,
                identity.execution_channel_id,
            )
            .ok();
        }
        let input = input.get("legacy_input").unwrap_or(input);
        let channel = input.get("channel_id")?.as_u64()?;
        Self::new(
            key,
            source_coverage(key, input)?,
            input.get("author_id")?.as_u64()?,
            input
                .get("original_channel_id")
                .map_or(Some(channel), Value::as_u64)?,
            input
                .get("execution_channel_id")
                .map_or(Some(channel), Value::as_u64)?,
        )
        .ok()
    }

    fn same_provenance(&self, other: &Self) -> bool {
        self.author_id == other.author_id
            && self.original_channel_id == other.original_channel_id
            && self.execution_channel_id == other.execution_channel_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Responsibility {
    Absent,
    Known { key: u64, received_seq: u64 },
    Legacy { key: u64, received_seq: u64 },
    Unknown,
    Conflict,
}

fn source_coverage(key: u64, input: &Value) -> Option<Vec<u64>> {
    let input = input.get("legacy_input").unwrap_or(input);
    let values = match input.get("receipt_identity") {
        Some(identity) => identity.get("source_ids")?.as_array()?,
        None => {
            if input.get("message_id")?.as_u64()? != key {
                return None;
            }
            input.get("source_message_ids")?.as_array()?
        }
    };
    let ids: Vec<_> = values.iter().map(Value::as_u64).collect::<Option<_>>()?;
    let unique: BTreeSet<_> = ids.iter().copied().collect();
    (ids.contains(&key) && !unique.contains(&0) && unique.len() == ids.len()).then_some(ids)
}

pub(super) fn lookup(rows: &Rows, source: &ReceiptIdentity) -> Responsibility {
    if !source.valid() {
        return Responsibility::Unknown;
    }
    let mut known = None;
    let mut matched = false;
    let mut missing_coverage = !rows.unbound.is_empty();
    let mut unknown_overlap = source.source_ids.iter().any(|id| rows.unbound.contains(id));
    // A retained index without its row cannot prove execution state or absence for its aliases.
    let orphan_overlap = rows.receipts.iter().any(|(key, identity)| {
        !rows.rows.contains_key(key)
            && source
                .source_ids
                .iter()
                .any(|id| id == key || identity.source_ids.contains(id))
    });
    #[cfg(test)]
    let orphan_overlap = orphan_overlap && !mutant("skip_orphan_receipt");
    unknown_overlap |= orphan_overlap;
    for (&key, row) in &rows.rows {
        let Some(identity) = rows.receipt_identity(key) else {
            match source_coverage(key, &row.input) {
                Some(ids) => unknown_overlap |= ids.iter().any(|id| source.source_ids.contains(id)),
                None => {
                    missing_coverage = true;
                    unknown_overlap |= source.source_ids.contains(&key);
                }
            }
            continue;
        };
        if !source
            .source_ids
            .iter()
            .any(|id| identity.source_ids.contains(id))
        {
            continue;
        }
        if matched
            || !identity.same_provenance(source)
            || source
                .source_ids
                .iter()
                .any(|id| !identity.source_ids.contains(id))
        {
            return Responsibility::Conflict;
        }
        matched = true;
        match row.received_seq {
            Some(received_seq) if received_seq != 0 => {
                #[cfg(test)]
                if mutant("owner_absent") && rows.owner(key) == super::Owner::Legacy {
                    return Responsibility::Absent;
                }
                known = Some(match row.state {
                    RowState::Abandoned(
                        AbandonReason::Handback | AbandonReason::HandbackAmbiguous,
                    ) => Responsibility::Legacy { key, received_seq },
                    _ => Responsibility::Known { key, received_seq },
                });
            }
            _ => unknown_overlap = true,
        }
    }
    // Missing old coverage blocks absence, but does not erase a positive canonical receipt.
    if unknown_overlap || (known.is_none() && missing_coverage) {
        #[cfg(test)]
        if mutant("unknown_absent") {
            return Responsibility::Absent;
        }
        Responsibility::Unknown
    } else {
        known.unwrap_or(Responsibility::Absent)
    }
}

#[cfg(test)]
pub(super) fn mutant(name: &str) -> bool {
    std::env::var("ADK_TEST_INPUT_G1A_MUTANT").is_ok_and(|value| value == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_input::ledger::Record;
    use crate::services::tui_input::rows::Rows;
    use serde_json::json;

    fn identity(key: u64, sources: &[u64]) -> ReceiptIdentity {
        ReceiptIdentity::new(key, sources.to_vec(), 7, 8, 9).unwrap()
    }

    fn rows(inputs: &[(u64, Value)]) -> Rows {
        let records: Vec<_> = inputs
            .iter()
            .enumerate()
            .map(|(index, (key, input))| Record {
                seq: index as u64 + 1,
                prev_crc: 0,
                crc: 0,
                kind: "received".into(),
                payload: json!({"key":key,"input":input}),
            })
            .collect();
        Rows::fold(None, &records).unwrap()
    }

    #[test]
    fn receipt_identity_requires_every_positive_field_and_primary_coverage() {
        for (key, ids, author, original, execution) in [
            (0, vec![0], 7, 8, 9),
            (10, vec![11], 7, 8, 9),
            (10, vec![10, 0], 7, 8, 9),
            (10, vec![10, 10], 7, 8, 9),
            (10, vec![], 7, 8, 9),
            (10, vec![10], 0, 8, 9),
            (10, vec![10], 7, 0, 9),
            (10, vec![10], 7, 8, 0),
        ] {
            assert!(ReceiptIdentity::new(key, ids, author, original, execution).is_err());
        }
        assert!(serde_json::from_value::<ReceiptIdentity>(json!({"source_ids":[10]})).is_err());
        assert_eq!(identity(11, &[11, 10]).source_ids, vec![10, 11]);
    }

    #[test]
    fn canonical_full_source_coverage_matches_aliases_and_ignores_enrichment() {
        let saved = identity(10, &[10, 11]);
        let canonical = json!({"receipt_identity":saved,"text":"first","reply_context":"reply","pending_uploads":["pin"],"queued_generation":3});
        let rows = rows(&[(10, canonical.clone())]);
        for query in [saved.clone(), identity(11, &[11])] {
            assert_eq!(
                rows.responsibility(&query),
                Responsibility::Known {
                    key: 10,
                    received_seq: 1
                }
            );
        }
        assert_eq!(rows.row(10).unwrap().input, canonical);
        assert_eq!(
            rows.responsibility(&identity(12, &[12])),
            Responsibility::Absent
        );
    }

    #[test]
    fn provenance_partial_overlap_and_multiple_canonical_rows_conflict() {
        let saved = identity(10, &[10, 11]);
        let input = json!({"receipt_identity":saved});
        let one = rows(&[(10, input.clone())]);
        assert_eq!(
            one.responsibility(&identity(11, &[11, 12])),
            Responsibility::Conflict
        );
        for query in [
            ReceiptIdentity {
                author_id: 6,
                ..saved.clone()
            },
            ReceiptIdentity {
                original_channel_id: 6,
                ..saved.clone()
            },
            ReceiptIdentity {
                execution_channel_id: 6,
                ..saved.clone()
            },
        ] {
            assert_eq!(one.responsibility(&query), Responsibility::Conflict);
        }
        let multiple = rows(&[
            (10, input),
            (11, json!({"receipt_identity":identity(11,&[11])})),
        ]);
        assert_eq!(
            multiple.responsibility(&identity(11, &[11])),
            Responsibility::Conflict
        );
        let mut snapshot = multiple.compact().unwrap();
        snapshot["rows"]["10"]
            .as_object_mut()
            .unwrap()
            .remove("received_seq");
        let restored: Rows = serde_json::from_value(snapshot).unwrap();
        assert_eq!(
            restored.responsibility(&identity(11, &[11])),
            Responsibility::Conflict
        );
    }

    #[test]
    fn old_null_partial_and_missing_order_remain_unknown() {
        let query = identity(11, &[11]);
        for input in [
            Value::Null,
            json!({"receipt_identity":{"source_ids":[11],"author_id":7,"original_channel_id":8,"execution_channel_id":9}}),
            json!({"receipt_identity":{"source_ids":[10,10],"author_id":7,"original_channel_id":8,"execution_channel_id":9}}),
        ] {
            let old = rows(&[(10, input)]);
            assert!(old.receipt_identity(10).is_none());
            assert_eq!(old.responsibility(&query), Responsibility::Unknown);
            let mut snapshot = old.compact().unwrap();
            assert!(snapshot.get("receipts").is_none());
            snapshot["rows"]["10"]["input"] = Value::Null;
            let restored: Rows = serde_json::from_value(snapshot).unwrap();
            assert_eq!(restored.responsibility(&query), Responsibility::Unknown);
        }
        let current = rows(&[(11, json!({"receipt_identity":query}))]);
        let mut snapshot = current.compact().unwrap();
        snapshot["rows"]["11"]
            .as_object_mut()
            .unwrap()
            .remove("received_seq");
        let restored: Rows = serde_json::from_value(snapshot).unwrap();
        assert_eq!(restored.responsibility(&query), Responsibility::Unknown);
    }

    #[test]
    fn unknown_aliases_and_unbound_population_prevent_positive_absence_or_duplicate() {
        let query = identity(11, &[11]);
        let mixed = rows(&[(10, Value::Null), (11, json!({"receipt_identity":query}))]);
        assert_eq!(
            mixed.responsibility(&query),
            Responsibility::Known {
                key: 11,
                received_seq: 2
            }
        );
        assert!(mixed.receipt_identity_missing());
        assert_eq!(
            mixed.responsibility(&identity(12, &[12])),
            Responsibility::Unknown
        );
        let unbound = Rows::fold(
            None,
            &[Record {
                seq: 1,
                prev_crc: 0,
                crc: 0,
                kind: "move_committed".into(),
                payload: json!({"first_staged_seq":1,"ids":[10]}),
            }],
        )
        .unwrap();
        assert_eq!(unbound.responsibility(&query), Responsibility::Unknown);
    }

    #[test]
    fn partial_provenance_is_unknown_only_for_its_positive_source_coverage() {
        for input in [
            json!({"receipt_identity":{"source_ids":[10],"author_id":7}}),
            json!({"message_id":10,"source_message_ids":[10],"channel_id":9}),
            json!({"receipt_identity":{"source_ids":[10],"author_id":0,"original_channel_id":8,"execution_channel_id":9}}),
        ] {
            let partial = rows(&[(10, input)]);
            assert!(!partial.receipt_identity_missing());
            assert_eq!(
                partial.responsibility(&identity(11, &[11])),
                Responsibility::Absent
            );
            assert_eq!(
                partial.responsibility(&identity(10, &[10])),
                Responsibility::Unknown
            );
        }
    }

    #[test]
    fn orphan_receipt_aliases_are_unknown_and_disjoint_sources_remain_scoped() {
        let mut snapshot = serde_json::to_value(Rows::default()).unwrap();
        snapshot["receipts"] = json!({"10":identity(10,&[10,11])});
        let orphan: Rows = serde_json::from_value(snapshot).unwrap();
        assert!(orphan.receipt_identity_missing());
        assert_eq!(
            orphan.responsibility(&identity(11, &[11])),
            Responsibility::Unknown
        );
        assert_eq!(
            orphan.responsibility(&identity(12, &[12])),
            Responsibility::Absent
        );
    }

    #[test]
    fn malformed_retained_receipts_hold_health_without_guessing_fields() {
        let current = rows(&[(10, json!({"receipt_identity":identity(10,&[10])}))]);
        for field in ["author_id", "original_channel_id", "execution_channel_id"] {
            let mut snapshot = serde_json::to_value(&current).unwrap();
            snapshot["receipts"]["10"][field] = json!(0);
            let malformed: Rows = serde_json::from_value(snapshot).unwrap();
            assert!(malformed.receipt_identity_missing());
            assert_eq!(
                malformed.responsibility(&identity(10, &[10])),
                Responsibility::Unknown
            );
        }
        let mut snapshot = serde_json::to_value(&current).unwrap();
        snapshot["receipts"]["10"]["source_ids"] = json!([11]);
        let malformed: Rows = serde_json::from_value(snapshot).unwrap();
        assert!(malformed.receipt_identity_missing());
        let mut snapshot = serde_json::to_value(&current).unwrap();
        snapshot["receipts"]["10"]
            .as_object_mut()
            .unwrap()
            .remove("author_id");
        assert!(
            serde_json::from_value::<Rows>(snapshot).is_err(),
            "required provenance is never defaulted"
        );
    }

    #[test]
    fn complete_legacy_envelope_derives_identity_without_defaulted_fields() {
        let input =
            json!({"message_id":10,"source_message_ids":[11,10],"author_id":7,"channel_id":9});
        for input in [input.clone(), json!({"legacy_input":input})] {
            let restored = rows(&[(10, input)]);
            let expected = ReceiptIdentity::new(10, vec![10, 11], 7, 9, 9).unwrap();
            assert_eq!(restored.receipt_identity(10), Some(expected));
            let alias = ReceiptIdentity::new(11, vec![11], 7, 9, 9).unwrap();
            assert_eq!(
                restored.responsibility(&alias),
                Responsibility::Known {
                    key: 10,
                    received_seq: 1
                }
            );
        }
    }
}
