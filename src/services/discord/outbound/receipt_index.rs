//! Read-only projection index over confirmed Discord delivery receipts.
//!
//! This is the 4987 S2 observation seam. It reads the existing delivery-record
//! store but neither mutates it nor turns coverage into a health verdict. Later
//! reachability composition may consume the returned fact; this module has no
//! destructive-action authority and is intentionally not wired to production.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::delivery_record::{ConfirmedDeliveryReceipt, DeliveryRecord, read_record_at};
use crate::services::provider::ProviderKind;

/// A successful read, a genuinely absent store, or a present store whose
/// coverage cannot safely be interpreted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReceiptIndexRead {
    Ready(ReceiptIndex),
    Absent,
    Unknown(ReceiptIndexUnknownReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ReceiptIndexUnknownReason {
    ReceiptStoreUnreadable,
}

/// The exact receipt projection from 4987 section -1.3. Byte ranges are values
/// under this key, so no turn coordinate is introduced here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReceiptProjectionKey {
    provider: ProviderKind,
    tmux_session_name: String,
    generation_mtime_ns: i64,
}

/// Confirmed receipt ranges grouped by their incarnation projection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct ReceiptIndex {
    ranges: HashMap<ReceiptProjectionKey, Vec<(u64, u64)>>,
}

impl ReceiptIndex {
    /// Pure coverage comparison for obligation `[start, end)`.
    ///
    /// `turn_nonce` is deliberately absent. Ignoring it can only loosen the
    /// match and increase the set called covered, so it cannot create a false
    /// `Unreachable`; this is the safe direction for 4987's asymmetric cost.
    pub(in crate::services::discord) fn covers(
        &self,
        provider: &ProviderKind,
        tmux_session_name: &str,
        generation_mtime_ns: i64,
        obligation: (u64, u64),
    ) -> bool {
        if obligation.1 <= obligation.0 {
            return false;
        }
        let key = ReceiptProjectionKey {
            provider: provider.clone(),
            tmux_session_name: tmux_session_name.to_owned(),
            generation_mtime_ns,
        };
        self.ranges.get(&key).is_some_and(|ranges| {
            ranges
                .iter()
                .any(|receipt| receipt.0 <= obligation.0 && receipt.1 >= obligation.1)
        })
    }

    /// Pure adapter from the durable record shape into the projection index.
    fn from_record(record: DeliveryRecord) -> Result<Self, ReceiptIndexUnknownReason> {
        let mut index = Self::default();
        for receipt in record.confirmed_deliveries {
            let (key, range) = project_receipt(receipt)
                .ok_or(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)?;
            index.ranges.entry(key).or_default().push(range);
        }
        Ok(index)
    }
}

/// I/O adapter around delivery_record's canonical read path.
///
/// `read_record_at` intentionally merges missing and malformed into `None`, so
/// `symlink_metadata` performs only the required second classification:
/// missing is `Absent`; every present or unreadable path is `Unknown`.
pub(in crate::services::discord) fn read_receipt_index_at(path: &Path) -> ReceiptIndexRead {
    if let Some(record) = read_record_at(path) {
        return match ReceiptIndex::from_record(record) {
            Ok(index) => ReceiptIndexRead::Ready(index),
            Err(reason) => ReceiptIndexRead::Unknown(reason),
        };
    }

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReceiptIndexRead::Absent,
        _ => ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable),
    }
}

fn project_receipt(
    receipt: ConfirmedDeliveryReceipt,
) -> Option<(ReceiptProjectionKey, (u64, u64))> {
    if !receipt.source.is_authoritative()
        || receipt.delivery_channel_id != receipt.source.delivery_channel_id
        || receipt.message_id == 0
    {
        return None;
    }
    let provider = ProviderKind::from_str(&receipt.source.provider)?;
    let range = receipt.source.range;
    Some((
        ReceiptProjectionKey {
            provider,
            tmux_session_name: receipt.source.tmux_session_name,
            generation_mtime_ns: receipt.source.generation_mtime_ns,
        },
        range,
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;
    use crate::services::discord::outbound::delivery_record::ExactJsonlSourceIdentity;

    fn receipt(range: (u64, u64), generation: i64, turn_nonce: &str) -> ConfirmedDeliveryReceipt {
        ConfirmedDeliveryReceipt {
            source: ExactJsonlSourceIdentity {
                provider: "claude".to_string(),
                tmux_session_name: "AgentDesk-42".to_string(),
                turn_nonce: turn_nonce.to_string(),
                range,
                generation_mtime_ns: generation,
                offset_authority_channel_id: 41,
                delivery_channel_id: 42,
            },
            delivery_channel_id: 42,
            message_id: 99,
        }
    }

    fn index(receipts: Vec<ConfirmedDeliveryReceipt>) -> ReceiptIndex {
        ReceiptIndex::from_record(DeliveryRecord {
            confirmed_deliveries: receipts,
            ..DeliveryRecord::default()
        })
        .expect("authoritative receipt index")
    }

    fn covered(index: &ReceiptIndex, generation: i64, range: (u64, u64)) -> bool {
        index.covers(&ProviderKind::Claude, "AgentDesk-42", generation, range)
    }

    #[test]
    fn projection_covers_exact_and_contained_ranges() {
        let index = index(vec![receipt((10, 20), 700, "turn-a")]);

        assert!(covered(&index, 700, (10, 20)));
        assert!(covered(&index, 700, (12, 18)));
    }

    #[test]
    fn projection_rejects_partial_overlap_and_adjacent_ranges() {
        let index = index(vec![receipt((10, 20), 700, "turn-a")]);

        assert!(!covered(&index, 700, (5, 15)));
        assert!(!covered(&index, 700, (15, 25)));
        assert!(!covered(&index, 700, (20, 25)));
        assert!(!covered(&index, 700, (5, 10)));
    }

    #[test]
    fn wrong_generation_never_covers() {
        let index = index(vec![receipt((10, 20), 700, "turn-a")]);

        assert!(!covered(&index, 701, (12, 18)));
    }

    #[test]
    fn turn_nonce_is_not_part_of_the_projection() {
        let index = index(vec![
            receipt((10, 15), 700, "turn-a"),
            receipt((15, 20), 700, "turn-b"),
        ]);

        assert!(covered(&index, 700, (10, 15)));
        assert!(covered(&index, 700, (15, 20)));
    }

    #[test]
    fn malformed_present_store_is_unknown() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        fs::write(&path, "{not-json").expect("write malformed record");

        assert_eq!(
            read_receipt_index_at(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn semantically_incomplete_receipt_is_unknown() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        let record = DeliveryRecord {
            confirmed_deliveries: vec![ConfirmedDeliveryReceipt {
                message_id: 0,
                ..receipt((10, 20), 700, "turn-a")
            }],
            ..DeliveryRecord::default()
        };
        fs::write(
            &path,
            serde_json::to_string(&record).expect("serialize record"),
        )
        .expect("write record");

        assert_eq!(
            read_receipt_index_at(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn empty_and_absent_stores_remain_distinct() {
        let directory = tempdir().expect("temporary directory");
        let empty_path = directory.path().join("empty.json");
        let absent_path = directory.path().join("absent.json");
        fs::write(&empty_path, "{}").expect("write empty record");

        let ReceiptIndexRead::Ready(index) = read_receipt_index_at(&empty_path) else {
            panic!("valid empty record must produce an empty index");
        };
        assert!(!covered(&index, 700, (10, 20)));
        assert_eq!(
            read_receipt_index_at(&absent_path),
            ReceiptIndexRead::Absent
        );
    }
}
