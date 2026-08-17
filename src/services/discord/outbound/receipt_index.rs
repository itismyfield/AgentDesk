//! Read-only projection index over durable Discord delivery evidence.
//!
//! This is the 4987 S2 observation seam. It reads the existing delivery-record
//! store and normalizes, per projection key, the union of confirmed receipt
//! ranges plus the durable frontier's guaranteed `[0, end)` prefix. It neither
//! mutates the store nor turns coverage into a health verdict. Later reachability
//! composition may consume the returned fact; this module has no destructive-
//! action authority and is intentionally not wired to production.
//!
//! One anomalous receipt or frontier currently makes the whole index `Unknown`;
//! choosing a finer failure granularity remains a prerequisite for S3.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::delivery_record::{
    ConfirmedDeliveryReceipt, DeliveredCommit, DeliveryRecord, read_record_at,
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
        record_provider: &ProviderKind,
        record_tmux_session_name: &str,
    ) -> Result<Self, ReceiptIndexUnknownReason> {
        let mut index = Self::default();

        if let Some(frontier) = record.delivered_frontier {
            let (key, range) =
                project_frontier(frontier, record_provider, record_tmux_session_name)
                    .ok_or(ReceiptIndexUnknownReason::ReceiptStoreUnreadable)?;
            index.ranges.entry(key).or_default().push(range);
        }

        for receipt in record.confirmed_deliveries {
            let (key, range) = project_receipt(receipt)
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
/// `record_provider` and `record_tmux_session_name` are the identity context of
/// this delivery-record path. `DeliveredCommit` persists no provider/session,
/// so callers must supply the same context used to select the record.
pub(in crate::services::discord) fn read_receipt_index_at(
    path: &Path,
    record_provider: &ProviderKind,
    record_tmux_session_name: &str,
) -> ReceiptIndexRead {
    if let Some(record) = read_record_at(path) {
        return match ReceiptIndex::from_record(record, record_provider, record_tmux_session_name) {
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
    frontier: DeliveredCommit,
    provider: &ProviderKind,
    tmux_session_name: &str,
) -> Option<(ReceiptProjectionKey, (u64, u64))> {
    if tmux_session_name.is_empty()
        || frontier.generation_mtime_ns == 0
        || frontier.range.1 <= frontier.range.0
    {
        return None;
    }

    // `DeliveredCommit` is the durable mirror of `confirmed_end_offset`, and
    // `range_already_committed` defines every range ending at or below that
    // high-water mark as delivered. Therefore only `[0, range.1)` is projected;
    // `range.0` describes the winning delivery, not the frontier's lower bound.
    Some((
        ReceiptProjectionKey {
            provider: provider.clone(),
            tmux_session_name: tmux_session_name.to_owned(),
            generation_mtime_ns: frontier.generation_mtime_ns,
        },
        (0, frontier.range.1),
    ))
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
            &ProviderKind::Claude,
            "AgentDesk-42",
        )
        .expect("authoritative receipt index")
    }

    fn read_index(path: &Path) -> ReceiptIndexRead {
        read_receipt_index_at(path, &ProviderKind::Claude, "AgentDesk-42")
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
    fn frontier_only_record_covers_prefix_but_not_beyond_high_water_mark() {
        let index = ReceiptIndex::from_record(
            DeliveryRecord {
                delivered_frontier: Some(DeliveredCommit {
                    range: (150, 200),
                    generation_mtime_ns: 700,
                    attempts: 1,
                    panel_msg_id: None,
                    panel_channel_id: None,
                }),
                confirmed_deliveries: Vec::new(),
                ..DeliveryRecord::default()
            },
            &ProviderKind::Claude,
            "AgentDesk-42",
        )
        .expect("authoritative frontier index");

        assert!(covered(&index, 700, (0, 200)));
        assert!(covered(&index, 700, (100, 200)));
        assert!(!covered(&index, 700, (199, 201)));
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
        fs::write(
            &path,
            serde_json::to_string(&record).expect("serialize record"),
        )
        .expect("write record");

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
