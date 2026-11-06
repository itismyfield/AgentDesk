//! Generation-scoped frontier readers and edit-failure validation.

use super::*;

/// Blocks a same-turn backward inflight write unless it is an empty-body `RetryBoundary` reset.
pub(in crate::services::discord) fn authority_blocks_backward_inflight_write(
    response_sent_offset_monotonic: bool,
    last_offset_monotonic: bool,
    is_legitimate_full_reset: bool,
) -> bool {
    (!response_sent_offset_monotonic || !last_offset_monotonic) && !is_legitimate_full_reset
}

/// Pure fusion (testable): the effective committed offset under the flip. The
/// durable frontier END can only RAISE the dedup floor above what the in-memory
/// authority sees (a pre-restart / cross-actor delivery the live atomic missed);
/// it must NEVER lower it. So fuse with `max` — a missed durable write (the
/// coverage hazard) can therefore never drop the floor below in-memory and cause
/// a false skip / lost relay. `None` durable (I3 conservative) → pure in-memory =
/// exactly today's behavior. Pure-durable-only authority waits for B3 hydration.
pub(super) fn fuse_committed_offset(durable_end: Option<u64>, in_memory: u64) -> u64 {
    match durable_end {
        Some(end) => end.max(in_memory),
        None => in_memory,
    }
}

/// #1270 generation guard (pure, testable): trust the durable frontier to RAISE
/// the dedup floor ONLY if it was written by the CURRENT wrapper generation. A
/// stale-high frontier from a PRIOR same-named tmux generation must NOT be
/// fused — the in-memory generation-reset already zeroed the live offset, and a
/// mismatched durable value would falsely skip the NEW generation's fresh output
/// (lost relay). `current_gen == 0` (no/unreadable `.generation` file) → cannot
/// validate → distrust. Write/read parity: the durable `generation_mtime_ns` is
/// the coord's `confirmed_end_generation_mtime_ns`, itself set from
/// `read_generation_file_mtime_ns` at advance time (tmux.rs), so equality holds
/// within a generation and breaks across one.
pub(in crate::services::discord) fn durable_frontier_generation_current(
    durable_mtime: i64,
    current_gen_mtime: i64,
) -> bool {
    current_gen_mtime != 0 && durable_mtime == current_gen_mtime
}

/// Path-based core (pure-ish, testable): the durable `delivered_frontier`, but
/// ONLY when it was written by the CURRENT wrapper generation (#1270 guard, via
/// [`durable_frontier_generation_current`]) AND its END is physically inside the
/// current transcript byte length (#4188 EOF guard). `None` when the record is
/// absent/malformed (I3 conservative), from a PRIOR generation (stale-high →
/// distrust), or when the current transcript EOF is unavailable (cannot
/// bound-check → distrust). `current_gen_mtime == 0` (no/unreadable
/// `.generation` file) → the guard distrusts everything → `None`.
pub(in crate::services::discord::outbound) fn current_generation_durable_frontier_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: Option<u64>,
) -> Option<DeliveredCommit> {
    let frontier = read_record_at(path).and_then(|r| r.delivered_frontier)?;
    if !super::durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime)
    {
        return None;
    }

    let Some(current_transcript_eof) = current_transcript_eof else {
        tracing::debug!(target: "agentdesk::services::discord::outbound::delivery_record",
            delivery_record_path = %path.display(),
            frontier_end = frontier.range.1,
            frontier_generation_mtime_ns = frontier.generation_mtime_ns,
            current_gen_mtime,
            "durable delivered_frontier current transcript EOF unavailable — distrusting unbounded frontier"
        );
        return None;
    };

    if frontier.range.1 > current_transcript_eof {
        tracing::warn!(target: "agentdesk::services::discord::outbound::delivery_record",
            delivery_record_path = %path.display(),
            frontier_end = frontier.range.1,
            current_transcript_eof,
            frontier_generation_mtime_ns = frontier.generation_mtime_ns,
            current_gen_mtime,
            "durable delivered_frontier end exceeds current transcript EOF — distrusting stale-length frontier (compaction/rotation)"
        );
        return None;
    }

    Some(frontier)
}

/// Path-based core (pure-ish, testable): the durable `delivered_frontier` END.
/// This is the SINGLE durable frontier END reader; the env-resolved
/// [`delivered_frontier_end_current_generation`] and
/// [`effective_committed_offset`] both funnel through it so the generation and
/// EOF-bounds gating logic exists in exactly one place.
pub(in crate::services::discord::outbound) fn current_generation_durable_frontier_end_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: Option<u64>,
) -> Option<u64> {
    current_generation_durable_frontier_at(path, current_gen_mtime, current_transcript_eof)
        .map(|f| f.range.1)
}

/// Resolve the current wrapper-generation watermark for `tmux_session_name`. The
/// `tmux` module is `#[cfg(unix)]`; on non-unix targets (windows CI cross-compile
/// check) there is no wrapper generation file, so the generation is absent (`0`),
/// which [`durable_frontier_generation_current`] treats as "distrust".
pub(in crate::services::discord) fn current_generation_mtime_ns(tmux_session_name: &str) -> i64 {
    #[cfg(unix)]
    {
        crate::services::discord::tmux::read_generation_file_mtime_ns(tmux_session_name)
    }
    #[cfg(not(unix))]
    {
        let _ = tmux_session_name;
        0
    }
}

// #4712 decomposition: the env-resolved delivered-frontier END readers moved to
// the `delivery_frontier_probe` sibling, beside the other delivered-frontier
// readers. Re-exported so every `delivery_record::…` call site is unchanged.
pub(in crate::services::discord) use super::super::delivery_frontier_probe::{
    delivered_frontier_end_current_generation, resolved_delivered_frontier_end_current_generation,
};

pub(super) fn current_generation_frontier_exceeding_eof_at(
    path: &Path,
    current_gen_mtime: i64,
    current_transcript_eof: u64,
) -> Option<u64> {
    read_record_at(path)
        .and_then(|record| record.delivered_frontier)
        .filter(|frontier| {
            durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime)
                && frontier.range.1 > current_transcript_eof
        })
        .map(|frontier| frontier.range.1)
}

/// #4549: detect the exact same-generation frontier/EOF regression that the
/// ordinary durable reader distrusts. The Claude idle scanner uses this signal
/// only on an unchanged transcript path to re-anchor its scan cursor at EOF after
/// an in-place `/compact` rewrite. A rotated UUID/path bypasses this predicate and
/// keeps the fresh-transcript lookback path, so replacement-file prompts are not
/// lost.
pub(in crate::services::discord) fn delivered_frontier_exceeding_current_eof(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: u64,
) -> Option<u64> {
    let path = delivery_record_path(provider, channel.get())?;
    current_generation_frontier_exceeding_eof_at(
        &path,
        current_generation_mtime_ns(tmux_session_name),
        current_transcript_eof,
    )
}

pub(super) fn reanchor_current_generation_frontier_at(
    path: &Path,
    current_gen_mtime: i64,
    expected_frontier_end: u64,
    reanchor_offset: u64,
) -> Result<bool, String> {
    let _lock = lock_record_path(path)?;
    let Some(mut record) = read_record_at(path) else {
        return Ok(false);
    };
    let Some(frontier) = record.delivered_frontier.as_mut() else {
        return Ok(false);
    };
    if !durable_frontier_generation_current(frontier.generation_mtime_ns, current_gen_mtime)
        || frontier.range.1 != expected_frontier_end
        || frontier.range.1 <= reanchor_offset
    {
        return Ok(false);
    }
    frontier.range.1 = reanchor_offset;
    if frontier.range.0 > reanchor_offset {
        frontier.range.0 = reanchor_offset;
    }
    write_record_at(path, &record)?;
    Ok(true)
}

/// #4841: converge a stale same-generation durable frontier after an in-place
/// `/compact`. This is a correction of already-confirmed coordinate space, not a
/// new delivery: it preserves the record's generation/attempt/anchor identity and
/// only lowers the END after the caller proved the current transcript boundary.
pub(in crate::services::discord) fn reanchor_current_generation_frontier(
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    expected_frontier_end: u64,
    reanchor_offset: u64,
) -> Result<bool, String> {
    let Some(path) = delivery_record_path(provider, channel.get()) else {
        return Ok(false);
    };
    reanchor_current_generation_frontier_at(
        &path,
        current_generation_mtime_ns(tmux_session_name),
        expected_frontier_end,
        reanchor_offset,
    )
}

/// The dedup floor: `max(in-memory offset, current-generation durable END within EOF)`.
pub(in crate::services::discord) fn effective_committed_offset(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: Option<u64>,
) -> u64 {
    let in_memory = shared.committed_relay_offset(channel);
    let durable_end = delivery_record_path(provider, channel.get()).and_then(|path| {
        current_generation_durable_frontier_end_at(
            &path,
            current_generation_mtime_ns(tmux_session_name),
            current_transcript_eof,
        )
    });
    fuse_committed_offset(durable_end, in_memory)
}

/// The re-send floor: [`effective_committed_offset`] raised by a current-generation durable read.
pub(in crate::services::discord) fn committed_floor_for_resend_dedup(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    current_transcript_eof: Option<u64>,
) -> u64 {
    effective_committed_offset(
        shared,
        provider,
        channel,
        tmux_session_name,
        current_transcript_eof,
    )
    .max(delivered_frontier_end_current_generation(
        provider,
        channel,
        tmux_session_name,
        current_transcript_eof,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct EditFailureTranscriptIdentity {
    pub(super) output_path: PathBuf,
    pub(super) generation_mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptFileSnapshot {
    eof: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

fn transcript_file_snapshot(path: &Path) -> Option<TranscriptFileSnapshot> {
    let metadata = fs::metadata(path).ok()?;
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Some(TranscriptFileSnapshot {
        eof: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    })
}

/// Capture the transcript identity before the Discord edit await. A missing
/// watcher path or generation marker cannot establish range identity and leaves
/// fallback delivery enabled.
pub(in crate::services::discord) fn capture_edit_failure_transcript_identity(
    shared: &crate::services::discord::SharedData,
    tmux_session_name: &str,
) -> Option<EditFailureTranscriptIdentity> {
    let output_path = PathBuf::from(
        shared
            .tmux_watchers
            .watcher_output_path(tmux_session_name)?,
    );
    let generation_mtime_ns = current_generation_mtime_ns(tmux_session_name);
    if generation_mtime_ns == 0 {
        return None;
    }
    Some(EditFailureTranscriptIdentity {
        output_path,
        generation_mtime_ns,
    })
}

pub(super) fn stable_edit_failure_frontier_at<P, G, H>(
    record_path: &Path,
    expected: &EditFailureTranscriptIdentity,
    mut current_output_path: P,
    mut current_generation: G,
    after_first_snapshot: H,
) -> Option<DeliveredCommit>
where
    P: FnMut() -> Option<PathBuf>,
    G: FnMut() -> i64,
    H: FnOnce(),
{
    let _lock = lock_record_path(record_path).ok()?;
    if current_output_path().as_deref() != Some(expected.output_path.as_path())
        || current_generation() != expected.generation_mtime_ns
    {
        return None;
    }
    let before = transcript_file_snapshot(&expected.output_path)?;
    after_first_snapshot();

    let frontier = read_record_at(record_path)?.delivered_frontier?;
    if !durable_frontier_generation_current(
        frontier.generation_mtime_ns,
        expected.generation_mtime_ns,
    ) || frontier.range.1 > before.eof
    {
        return None;
    }

    let after_path = current_output_path()?;
    let after_generation = current_generation();
    let after = transcript_file_snapshot(&expected.output_path)?;
    if after_path != expected.output_path
        || after_generation != expected.generation_mtime_ns
        || after != before
    {
        return None;
    }
    Some(frontier)
}

/// Re-read a stable path+generation+EOF+frontier snapshot after an edit failure
/// and decide whether a fallback POST would duplicate a confirmed JSONL range.
/// The expected path and generation were captured before the edit await. The
/// record lock serializes conforming frontier writers, while the post-read
/// identity check detects wrapper rotation or transcript replacement during the
/// snapshot. Any missing or changing component fails open for delivery.
pub(in crate::services::discord) fn range_committed_after_edit_failure(
    shared: &crate::services::discord::SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux_session_name: &str,
    expected: Option<&EditFailureTranscriptIdentity>,
    range_end: u64,
) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    let Some(record_path) = delivery_record_path(provider, channel.get()) else {
        return false;
    };
    let frontier = stable_edit_failure_frontier_at(
        &record_path,
        expected,
        || {
            shared
                .tmux_watchers
                .watcher_output_path(tmux_session_name)
                .map(PathBuf::from)
        },
        || current_generation_mtime_ns(tmux_session_name),
        || {},
    );
    range_already_committed(
        range_end,
        frontier.map(|frontier| frontier.range.1).unwrap_or(0),
    )
}

/// A nonzero JSONL range is already delivered when its end is at or below the committed offset.
/// Both offsets must use JSONL bytes, never assistant-text bytes; either zero suppresses nothing.
pub(in crate::services::discord) fn range_already_committed(
    range_end: u64,
    committed: u64,
) -> bool {
    range_end > 0 && range_end <= committed
}
