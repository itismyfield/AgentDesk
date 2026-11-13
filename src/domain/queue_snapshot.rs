#[derive(Clone, Debug, Default)]
pub struct PendingQueueSnapshot {
    pub queue_depth: usize,
    pub disk_present: Option<bool>,
    /// #5176: the primary Discord message id of every queued intervention, in
    /// queue order. Depth alone told operators that a cancel lost "one item"
    /// and nothing else — not which user instruction vanished, and not whether
    /// a same-depth queue had actually been swapped out underneath them. A
    /// user-message-lossless contract cannot be audited by counting.
    pub message_ids: Vec<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(snapshot: &PendingQueueSnapshot) -> (usize, Option<bool>, &[u64]) {
        (
            snapshot.queue_depth,
            snapshot.disk_present,
            &snapshot.message_ids,
        )
    }

    #[test]
    fn snapshot_clone_keeps_ids_in_order_and_all_observations() {
        let snapshot = PendingQueueSnapshot {
            queue_depth: 2,
            disk_present: Some(true),
            message_ids: vec![42, 17],
        };
        let cloned = snapshot.clone();
        assert_eq!(values(&cloned), values(&snapshot));
        assert_eq!(cloned.message_ids, vec![42, 17]);
        for message_ids in [vec![17, 42], vec![42, 99]] {
            let changed = PendingQueueSnapshot {
                message_ids,
                ..snapshot.clone()
            };
            assert_eq!(changed.queue_depth, snapshot.queue_depth);
            assert_ne!(values(&changed), values(&snapshot));
        }
    }

    #[test]
    fn unknown_disk_remains_distinct_from_absent_and_present() {
        let snapshot = PendingQueueSnapshot {
            queue_depth: 2,
            disk_present: None,
            message_ids: vec![42, 17],
        };
        assert_eq!(values(&snapshot.clone()), values(&snapshot));
        assert_eq!(snapshot.disk_present, None);
        for disk_present in [Some(false), Some(true)] {
            let measured = PendingQueueSnapshot {
                disk_present,
                ..snapshot.clone()
            };
            assert_ne!(values(&snapshot), values(&measured));
        }
        let default = PendingQueueSnapshot::default();
        assert_eq!(values(&default), (0, None, &[][..]));
    }
}
