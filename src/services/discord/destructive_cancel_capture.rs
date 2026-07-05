#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CaptureProgressEvidence {
    pub(super) output_len_at_snapshot: Option<u64>,
    pub(super) output_len_now: Option<u64>,
    pub(super) output_mtime_age_secs: Option<i64>,
    pub(super) relay_frontier_at_snapshot: Option<u64>,
    pub(super) relay_frontier_now: Option<u64>,
}

pub(super) fn fresh_watcher_heartbeat_blocks_rebind(
    evidence: CaptureProgressEvidence,
    stale_after_secs: i64,
) -> bool {
    capture_progress_recent(evidence, stale_after_secs)
}

fn capture_progress_recent(evidence: CaptureProgressEvidence, stale_after_secs: i64) -> bool {
    if let (Some(previous), Some(now)) = (evidence.output_len_at_snapshot, evidence.output_len_now)
        && now > previous
    {
        return true;
    }
    if let (Some(previous), Some(now)) = (
        evidence.relay_frontier_at_snapshot,
        evidence.relay_frontier_now,
    ) && now > previous
    {
        return true;
    }
    evidence
        .output_mtime_age_secs
        .is_some_and(|age_secs| age_secs < stale_after_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_heartbeat_without_capture_progress_does_not_block_rebind() {
        let stale_after_secs = 600;
        let stale_capture = CaptureProgressEvidence {
            output_len_at_snapshot: Some(512_146),
            output_len_now: Some(512_146),
            output_mtime_age_secs: Some(stale_after_secs + 1),
            relay_frontier_at_snapshot: Some(512_146),
            relay_frontier_now: Some(512_146),
        };
        let growing_capture = CaptureProgressEvidence {
            output_len_now: Some(512_147),
            ..stale_capture
        };

        assert!(
            !fresh_watcher_heartbeat_blocks_rebind(stale_capture, stale_after_secs),
            "fresh heartbeat alone must not block when the watched capture is stale"
        );
        assert!(
            fresh_watcher_heartbeat_blocks_rebind(growing_capture, stale_after_secs),
            "fresh heartbeat still blocks when the watched capture advanced"
        );
    }
}
