//! Monotonic ID allocators extracted from `VoiceBargeInRuntime`
//! (#3038 god-object split, id-sequence slice).
//!
//! Hosts [`VoiceIdSequences`], the three independent `AtomicU64` counters that
//! previously lived directly on `VoiceBargeInRuntime`. Moving them (together
//! with their sole private seed constant `PROGRESS_PLAYBACK_OWNER_START`) into
//! this sibling module isolates the concern and physically shrinks the
//! `voice_barge_in.rs` giant rather than re-inflating it. Each `next_*`
//! accessor preserves the original `fetch_add` semantics byte-for-byte —
//! including the exact memory `Ordering` and the per-counter seed — so issued
//! IDs are identical to the pre-extraction layout.

use std::sync::atomic::{AtomicU64, Ordering};

use super::voice_barge_in::INTERNAL_VOICE_MESSAGE_ID_START;

// F4 (#2046): progress/ack 재생 owner id 시작점. spoken_result owner 공간(1..)과
// 분리하기 위해 high range 사용.
const PROGRESS_PLAYBACK_OWNER_START: u64 = 1u64 << 63;

/// Cohesive sub-concern of `VoiceBargeInRuntime`: monotonic ID allocators
/// (#3038 god-object split, id-sequence slice).
///
/// Bundles the three independent `AtomicU64` counters that previously lived
/// directly on `VoiceBargeInRuntime`. Each `next_*` accessor preserves the
/// original `fetch_add` semantics — including the exact memory `Ordering` and
/// the per-counter seed value — so issued IDs are byte-for-byte identical to
/// the pre-extraction layout:
/// - spoken-result playbacks seed at `1` (`SeqCst`),
/// - progress playbacks seed at `PROGRESS_PLAYBACK_OWNER_START` (`SeqCst`),
/// - internal voice messages seed at `INTERNAL_VOICE_MESSAGE_ID_START`
///   (`Relaxed`).
pub(in crate::services::discord) struct VoiceIdSequences {
    next_spoken_result_playback_id: AtomicU64,
    // F4 (#2046): progress/ack 재생 owner id 발급용. 30s 만료 타이머가 owner 일치
    // 시에만 playback entry 를 정리하도록 한다.
    next_progress_playback_id: AtomicU64,
    next_internal_message_id: AtomicU64,
}

impl VoiceIdSequences {
    pub(in crate::services::discord) fn new() -> Self {
        Self {
            next_spoken_result_playback_id: AtomicU64::new(1),
            next_progress_playback_id: AtomicU64::new(PROGRESS_PLAYBACK_OWNER_START),
            next_internal_message_id: AtomicU64::new(INTERNAL_VOICE_MESSAGE_ID_START),
        }
    }

    /// Allocate the next spoken-result playback id (`SeqCst`, seeded at `1`).
    pub(in crate::services::discord) fn next_spoken_result_playback_id(&self) -> u64 {
        self.next_spoken_result_playback_id
            .fetch_add(1, Ordering::SeqCst)
    }

    /// Allocate the next progress/ack playback owner id (`SeqCst`, seeded at
    /// `PROGRESS_PLAYBACK_OWNER_START`).
    pub(in crate::services::discord) fn next_progress_playback_id(&self) -> u64 {
        self.next_progress_playback_id
            .fetch_add(1, Ordering::SeqCst)
    }

    /// Allocate the next internal voice message id (`Relaxed`, seeded at
    /// `INTERNAL_VOICE_MESSAGE_ID_START`).
    pub(in crate::services::discord) fn next_internal_message_id(&self) -> u64 {
        self.next_internal_message_id
            .fetch_add(1, Ordering::Relaxed)
    }
}
