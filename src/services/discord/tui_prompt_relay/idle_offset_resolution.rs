//! #3479: idle-tail transcript start-offset resolution helpers, moved verbatim
//! out of tui_prompt_relay.rs (behavior-preserving — only visibility prefixes
//! adjusted and the shared `#[cfg(unix)]` lifted from each fn to the parent's
//! `mod`/`use` decls). Every dependency is reached via `use super::*;`.

use super::*;

/// #3154 P1 (timestamp-anchor output loss): single choke point that resolves the
/// idle-tail start offset.
///
/// When `explicit_start_offset` is `Some` (the deferred-BridgeAdapter worker path),
/// the tail anchors DIRECTLY to that transcript byte offset — the claim's post-drain
/// EOF `turn_start_offset`, the authoritative byte boundary for this synthetic turn —
/// and the `observed_at` timestamp scan is BYPASSED. The worker synthesizes
/// `observed_at = Utc::now()` only AFTER the deferred-claim wait, so every byte
/// written to the transcript during that wait predates it; a timestamp scan would
/// find no boundary line and SKIP those bytes (uncapped output loss). The explicit
/// EOF offset includes every byte of this turn (no skip) and never precedes the
/// prior bytes (no prior-turn re-relay). `normalize_transcript_fallback_offset`
/// guards a stale-high offset (past EOF → 0); the committed-offset clamp in
/// `spawn_claude_idle_response_tail_once` still dedupes against watcher delivery.
///
/// When `explicit_start_offset` is `None` (inline / non-deferred path), the original
/// `observed_at` timestamp-scan anchoring is preserved unchanged.
pub(super) fn resolve_idle_tail_start_offset(
    transcript_path: &Path,
    explicit_start_offset: Option<u64>,
    observed_at: chrono::DateTime<chrono::Utc>,
    fallback_offset: u64,
) -> u64 {
    match explicit_start_offset {
        Some(offset) => normalize_transcript_fallback_offset(transcript_path, offset),
        None => claude_idle_response_start_offset_after_timestamp(
            transcript_path,
            observed_at,
            fallback_offset,
        ),
    }
}

pub(super) fn claude_idle_response_start_offset_after_timestamp(
    transcript_path: &Path,
    turn_started_at: chrono::DateTime<chrono::Utc>,
    fallback_offset: u64,
) -> u64 {
    match crate::services::claude_tui::transcript_tail::claude_transcript_timestamp_at_or_after(
        transcript_path,
        turn_started_at,
    ) {
        Ok(Some(offset)) => offset,
        Ok(None) => normalize_transcript_fallback_offset(transcript_path, fallback_offset),
        Err(error) => {
            tracing::debug!(
                transcript_path = %transcript_path.display(),
                error = %error,
                fallback_offset,
                "Claude idle transcript timestamp scan failed; using fallback offset"
            );
            normalize_transcript_fallback_offset(transcript_path, fallback_offset)
        }
    }
}

fn normalize_transcript_fallback_offset(transcript_path: &Path, fallback_offset: u64) -> u64 {
    match std::fs::metadata(transcript_path).map(|metadata| metadata.len()) {
        Ok(file_len) if fallback_offset > file_len => 0,
        _ => fallback_offset,
    }
}

/// #3183: clamp the idle-tail start offset to at least the watcher's committed
/// delivery offset.
///
/// The idle-tail start offset is derived purely from the prompt timestamp (or
/// the just-scanned prompt's `line_end_offset`), so it does NOT account for what
/// the tmux watcher already delivered for this turn. When the watcher relayed a
/// terminal response and THEN the idle tail spawns (e.g. an owner-arbitration /
/// watcher-registration race lets the tail through before the
/// `watcher_covers_current_transcript` suppression observes it), the tail
/// re-relays the SAME byte range — the duplicate message in #3183.
///
/// `committed_relay_offset` is the single authoritative "JSONL byte offset
/// (exclusive) past which the watcher has confirmed delivery" (#3017), in the
/// SAME transcript-byte space as `start_offset`. Clamping
/// `start_offset = max(start_offset, committed)` means the tail only ever scans
/// output the watcher has NOT delivered:
///   - watcher delivered up to X  → tail starts at >= X (no duplicate; if the
///     watcher covered the whole turn the tail finds nothing to relay).
///   - watcher stopped / not covering (the #3176 outage case) → `committed` is
///     0 or below the timestamp offset, so the clamp is a no-op and the tail
///     still relays from the timestamp offset (no relay-loss regression).
///
/// Returns the clamped offset; `committed == 0` (no confirmed delivery this
/// process lifetime) leaves `start_offset` untouched.
pub(super) fn clamp_idle_tail_start_offset_to_committed(
    start_offset: u64,
    committed_offset: u64,
) -> u64 {
    start_offset.max(committed_offset)
}
