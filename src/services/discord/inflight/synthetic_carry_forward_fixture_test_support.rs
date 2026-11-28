use super::{InflightTurnState, ProviderKind};

// The on-disk row carries the watcher's COMMITTED frontier as last_offset
// (the watcher is the single authority — #3017 `confirmed_end_offset`). When
// the synthetic re-claims/refreshes the row, it writes its BIRTH offset back:
//   * Pre-fix: birth == lagging `relay_last_offset()` (2821677) < committed
//     frontier (2838484). Same identity (the watcher row is still keyed by
//     this turn's anchor + this birth turn_start_offset) → BACKWARD write →
//     the `last_offset` + `response_sent_offset` monotonicity guards fire (the
//     incident's ERROR triple). This is the REPRODUCE witness.
//   * Post-fix: `synthetic_start_offset_carry_forward` births the synthetic at
//     the committed frontier (2838484) — equal to the on-disk last_offset, so
//     the re-claim is a forward/equal write and NO invariant fires.
pub(in crate::services::discord) fn build_synth_3358(birth: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Claude,
        321,
        Some("adk-cc".to_string()),
        1,
        1_514_752_860_691_370_014, // synthetic user_msg_id (anchor id)
        9002,
        "new synthetic prompt".to_string(),
        None,
        Some("AgentDesk-claude-adk-cc".to_string()),
        Some("/tmp/out.jsonl".to_string()),
        None,
        birth,
    )
}

/// #3358: the two monotonicity tests catch panics via `catch_unwind` while
/// sharing the process-global panic hook with every parallel test thread —
/// serialize them so a sibling's hook traffic cannot interleave (rare
/// parallel-run flake observed on loaded machines).
pub(in crate::services::discord) fn monotonic_3358_test_mutex() -> &'static std::sync::Mutex<()> {
    static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(()))
}
