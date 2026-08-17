//! Read-only projection index over durable Discord delivery evidence.
//!
//! This is the 4987 S2 observation seam. It reads the existing delivery-record
//! store and normalizes, per projection key, the union of confirmed receipt
//! ranges plus the exact `[range.0, range.1)` range of a canonically validated
//! durable frontier. It neither
//! mutates the store nor turns coverage into a health verdict. Later reachability
//! composition may consume the returned fact; this module has no destructive-
//! action authority and is intentionally not wired to production.
//!
//! One anomalous receipt currently makes the whole index `Unknown`. A frontier
//! without canonical generation/EOF validation or a unique receipt-derived
//! provider/session identity contributes no coverage.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::delivery_record::{
    ConfirmedDeliveryReceipt, DeliveredCommit, DeliveryRecord,
    current_generation_durable_frontier_at, read_record_at,
};
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

/// Normalized delivered-range unions grouped by their incarnation projection.
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
        let Some(ranges) = self.ranges.get(&key) else {
            return false;
        };
        let candidate = ranges.partition_point(|range| range.1 <= obligation.0);
        ranges
            .get(candidate)
            .is_some_and(|range| range.0 <= obligation.0 && range.1 >= obligation.1)
    }

    /// Pure adapter from the durable record shape into the projection index.
    fn from_record(
        record: DeliveryRecord,
        projected_frontier: Option<(ReceiptProjectionKey, (u64, u64))>,
    ) -> Result<Self, ReceiptIndexUnknownReason> {
        let mut index = Self::default();

        if let Some((key, range)) = projected_frontier {
            index.ranges.entry(key).or_default().push(range);
        }

        for receipt in record.confirmed_deliveries {
            let (key, range) = project_receipt(&receipt)
                .ok_or(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)?;
            index.ranges.entry(key).or_default().push(range);
        }

        index.normalize_ranges();
        Ok(index)
    }

    fn normalize_ranges(&mut self) {
        for ranges in self.ranges.values_mut() {
            ranges.sort_unstable();
            let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
            for range in ranges.drain(..) {
                if let Some(previous) = merged.last_mut()
                    && range.0 <= previous.1
                {
                    previous.1 = previous.1.max(range.1);
                    continue;
                }
                merged.push(range);
            }
            *ranges = merged;
        }
    }
}

/// I/O adapter around delivery_record's canonical read path.
///
/// `read_record_at` intentionally merges missing and malformed into `None`, so
/// `symlink_metadata` performs only the required second classification:
/// missing is `Absent`; every present or unreadable path is `Unknown`.
pub(in crate::services::discord) fn read_receipt_index_at(
    path: &Path,
    current_generation_mtime_ns: i64,
    current_transcript_eof: Option<u64>,
) -> ReceiptIndexRead {
    if let Some(record) = read_record_at(path) {
        let projected_frontier = current_generation_durable_frontier_at(
            path,
            current_generation_mtime_ns,
            current_transcript_eof,
        )
        .and_then(|frontier| project_frontier(&record.confirmed_deliveries, frontier));
        return match ReceiptIndex::from_record(record, projected_frontier) {
            Ok(index) => ReceiptIndexRead::Ready(index),
            Err(reason) => ReceiptIndexRead::Unknown(reason),
        };
    }

    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ReceiptIndexRead::Absent,
        _ => ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable),
    }
}

fn project_frontier(
    receipts: &[ConfirmedDeliveryReceipt],
    frontier: DeliveredCommit,
) -> Option<(ReceiptProjectionKey, (u64, u64))> {
    let mut identity = None;
    for receipt in receipts {
        if receipt.source.generation_mtime_ns != frontier.generation_mtime_ns {
            continue;
        }
        let (candidate, _) = project_receipt(receipt)?;
        if identity
            .as_ref()
            .is_some_and(|existing| existing != &candidate)
        {
            return None;
        }
        identity = Some(candidate);
    }

    // `current_generation_durable_frontier_at` has already proved the current
    // generation and EOF bound. `DeliveredCommit` proves only its exact
    // committed range, under the unique stored receipt identity for that
    // generation; it does not prove a prefix before `range.0`.
    Some((identity?, frontier.range))
}

fn project_receipt(
    receipt: &ConfirmedDeliveryReceipt,
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
            tmux_session_name: receipt.source.tmux_session_name.clone(),
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
    use crate::services::discord::outbound::delivery_record::{
        DeliveredCommit, ExactJsonlSourceIdentity,
    };

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
        ReceiptIndex::from_record(
            DeliveryRecord {
                confirmed_deliveries: receipts,
                ..DeliveryRecord::default()
            },
            None,
        )
        .expect("authoritative receipt index")
    }

    fn read_index(path: &Path) -> ReceiptIndexRead {
        read_receipt_index_at(path, 700, Some(200))
    }

    fn write_record(path: &Path, record: &DeliveryRecord) {
        fs::write(
            path,
            serde_json::to_string(record).expect("serialize record"),
        )
        .expect("write record");
    }

    fn frontier(range: (u64, u64), generation_mtime_ns: i64) -> DeliveredCommit {
        DeliveredCommit {
            range,
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        }
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
    fn adjacent_receipts_union_covers_spanning_obligation() {
        let index = index(vec![
            receipt((100, 150), 700, "turn-a"),
            receipt((150, 200), 700, "turn-b"),
        ]);

        assert!(covered(&index, 700, (100, 200)));
    }

    #[test]
    fn overlapping_receipts_union_covers_spanning_obligation() {
        let index = index(vec![
            receipt((100, 175), 700, "turn-a"),
            receipt((150, 200), 700, "turn-b"),
        ]);

        assert!(covered(&index, 700, (100, 200)));
    }

    #[test]
    fn one_byte_gap_remains_uncovered() {
        let index = index(vec![
            receipt((100, 150), 700, "turn-a"),
            receipt((151, 200), 700, "turn-b"),
        ]);

        assert!(!covered(&index, 700, (100, 200)));
    }

    #[test]
    fn receipts_from_different_projection_keys_do_not_union() {
        let mut other_session = receipt((150, 200), 700, "turn-b");
        other_session.source.tmux_session_name = "AgentDesk-else".to_string();
        let index = index(vec![receipt((100, 150), 700, "turn-a"), other_session]);

        assert!(!covered(&index, 700, (100, 200)));
    }

    #[test]
    fn frontier_covers_only_its_committed_range_boundaries() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        write_record(
            &path,
            &DeliveryRecord {
                delivered_frontier: Some(frontier((150, 200), 700)),
                confirmed_deliveries: vec![receipt((10, 20), 700, "identity")],
                ..DeliveryRecord::default()
            },
        );

        let ReceiptIndexRead::Ready(index) = read_index(&path) else {
            panic!("canonical frontier must produce an index");
        };
        assert!(!covered(&index, 700, (149, 200)));
        assert!(covered(&index, 700, (150, 200)));
        assert!(covered(&index, 700, (151, 199)));
        assert!(!covered(&index, 700, (199, 201)));
    }

    #[test]
    fn frontier_beyond_current_transcript_eof_is_uncovered() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        write_record(
            &path,
            &DeliveryRecord {
                delivered_frontier: Some(frontier((u64::MAX - 1, u64::MAX), 700)),
                confirmed_deliveries: vec![receipt((10, 20), 700, "identity")],
                ..DeliveryRecord::default()
            },
        );

        let ReceiptIndexRead::Ready(index) = read_index(&path) else {
            panic!("stale frontier must be omitted from an otherwise valid index");
        };
        assert!(!covered(&index, 700, (u64::MAX - 1, u64::MAX)));
    }

    #[test]
    fn frontier_requires_and_uses_record_receipt_identity() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        let mut record = DeliveryRecord {
            delivered_frontier: Some(frontier((150, 200), 700)),
            ..DeliveryRecord::default()
        };
        write_record(&path, &record);

        let ReceiptIndexRead::Ready(index) = read_index(&path) else {
            panic!("identity-less frontier must yield an empty index");
        };
        assert!(!covered(&index, 700, (150, 200)));

        record.confirmed_deliveries = vec![receipt((10, 20), 700, "identity")];
        write_record(&path, &record);
        let ReceiptIndexRead::Ready(index) = read_index(&path) else {
            panic!("receipt identity must enable the canonical frontier");
        };
        assert!(!index.covers(&ProviderKind::Codex, "AgentDesk-42", 700, (150, 200)));
        assert!(!index.covers(&ProviderKind::Claude, "AgentDesk-other", 700, (150, 200)));
        assert!(covered(&index, 700, (150, 200)));
    }

    #[test]
    fn malformed_present_store_is_unknown() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("record.json");
        fs::write(&path, "{not-json").expect("write malformed record");

        assert_eq!(
            read_index(&path),
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
        write_record(&path, &record);

        assert_eq!(
            read_index(&path),
            ReceiptIndexRead::Unknown(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)
        );
    }

    #[test]
    fn empty_and_absent_stores_remain_distinct() {
        let directory = tempdir().expect("temporary directory");
        let empty_path = directory.path().join("empty.json");
        let absent_path = directory.path().join("absent.json");
        fs::write(&empty_path, "{}").expect("write empty record");

        let ReceiptIndexRead::Ready(index) = read_index(&empty_path) else {
            panic!("valid empty record must produce an empty index");
        };
        assert!(!covered(&index, 700, (10, 20)));
        assert_eq!(read_index(&absent_path), ReceiptIndexRead::Absent);
    }
}
