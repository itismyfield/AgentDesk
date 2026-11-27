// Leave time for watcher delivery while keeping the recovery retry and reprobes
// within the input wait budget.
const CAUGHT_UP_TERMINAL_CAPTURE_STALE_SECS: i64 = 120;

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
    terminal_envelope_present: impl FnOnce() -> bool,
) -> bool {
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

    let capture_caught_up = evidence.output_len_now.is_some_and(|len| {
        evidence.output_len_at_snapshot == Some(len)
            && evidence.relay_frontier_at_snapshot == Some(len)
            && evidence.relay_frontier_now == Some(len)
    });
    evidence.output_mtime_age_secs.is_some_and(|age_secs| {
        age_secs < stale_after_secs
            && (age_secs < CAUGHT_UP_TERMINAL_CAPTURE_STALE_SECS
                || !capture_caught_up
                || !terminal_envelope_present())
    })
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
            !fresh_watcher_heartbeat_blocks_rebind(stale_capture, stale_after_secs, || false),
            "fresh heartbeat alone must not block when the watched capture is stale"
        );
        assert!(
            fresh_watcher_heartbeat_blocks_rebind(growing_capture, stale_after_secs, || true),
            "fresh heartbeat still blocks when the watched capture advanced"
        );
        for (age_secs, blocks) in [(119, true), (120, false)] {
            assert_eq!(
                fresh_watcher_heartbeat_blocks_rebind(
                    CaptureProgressEvidence {
                        output_mtime_age_secs: Some(age_secs),
                        ..stale_capture
                    },
                    stale_after_secs,
                    || true,
                ),
                blocks,
                "age {age_secs}"
            );
        }
    }
}
