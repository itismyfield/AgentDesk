//! Live barge-in sensitivity state extracted from `VoiceBargeInRuntime`
//! (#3038 god-object split, sensitivity slice).
//!
//! Hosts [`SensitivityState`], the three sensitivity-related fields
//! (`default_sensitivity` boot default, lock-free `AtomicU8` mirror, and the
//! shared `Arc<RwLock<BargeInSensitivityState>>`) that previously lived directly
//! on `VoiceBargeInRuntime`. Moving them into this sibling module isolates the
//! concern and physically shrinks the `voice_barge_in.rs` giant rather than
//! re-inflating it.
//!
//! Concurrency semantics are preserved byte-for-byte relative to the
//! pre-extraction layout:
//! - the `AtomicU8` mirror is always stored *before* the `RwLock` write
//!   (F18, #2046), so a concurrent `try_read` failure never regresses to the
//!   boot-time default;
//! - `current()` falls back to the atomic mirror (not the boot default) on
//!   `try_read` contention;
//! - the exact memory `Ordering` (`Relaxed` on both store and load) and the
//!   `Arc<RwLock<_>>` sharing of `state_handle()` are unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::voice::barge_in::{BargeInSensitivity, BargeInSensitivityState};

/// Cohesive sub-concern of `VoiceBargeInRuntime`: the live barge-in
/// sensitivity state (#3038 STT/TTS/playback/routing split, sensitivity slice).
///
/// Bundles the three sensitivity-related fields that previously lived directly
/// on `VoiceBargeInRuntime`. Behavior is preserved exactly: the atomic mirror is
/// always stored *before* the `RwLock` write (F18, #2046), and `current` falls
/// back to the atomic mirror on `try_read` contention.
pub(in crate::services::discord) struct SensitivityState {
    // F18 (#2046): boot-time default. Retained for parity with the original
    // field layout; never read after construction.
    #[allow(dead_code)]
    default_sensitivity: BargeInSensitivity,
    // F18 (#2046): RwLock try_read 실패 시 default 로 폴백하면 사용자가 설정한
    // Conservative 가 잠깐 Normal 로 잘못 평가될 수 있다. 최신 값을 lock-free 로
    // 읽을 수 있도록 atomic mirror 유지.
    atom: AtomicU8,
    state: Arc<RwLock<BargeInSensitivityState>>,
}

impl SensitivityState {
    pub(in crate::services::discord) fn new(
        default_sensitivity: BargeInSensitivity,
        conservative_ttl: Duration,
    ) -> Self {
        Self {
            default_sensitivity,
            atom: AtomicU8::new(default_sensitivity.as_u8()),
            state: Arc::new(RwLock::new(BargeInSensitivityState::new(
                default_sensitivity,
                conservative_ttl,
            ))),
        }
    }

    pub(in crate::services::discord) fn disabled() -> Self {
        let default_sensitivity = BargeInSensitivity::Normal;
        Self {
            default_sensitivity,
            atom: AtomicU8::new(default_sensitivity.as_u8()),
            state: Arc::new(RwLock::new(BargeInSensitivityState::default())),
        }
    }

    /// Clone the shared `RwLock` handle for the TTL reset background task.
    pub(in crate::services::discord) fn state_handle(
        &self,
    ) -> Arc<RwLock<BargeInSensitivityState>> {
        self.state.clone()
    }

    /// F18 (#2046): atomic mirror 를 먼저 갱신해 두면 try_read 충돌 윈도우에서도
    /// `current` 가 최신 값을 본다.
    pub(in crate::services::discord) async fn set(&self, sensitivity: BargeInSensitivity) {
        self.atom.store(sensitivity.as_u8(), Ordering::Relaxed);
        self.state
            .write()
            .await
            .set_sensitivity(sensitivity, Instant::now());
    }

    pub(in crate::services::discord) async fn apply_voice_command(
        &self,
        transcript: &str,
    ) -> Option<BargeInSensitivity> {
        self.state
            .write()
            .await
            .apply_voice_command(transcript, Instant::now())
    }

    /// F18 (#2046): try_read 실패 시 boot-time default 가 아닌 가장 최근에
    /// 설정된 sensitivity 를 반환하도록 atomic mirror 로 폴백한다.
    pub(in crate::services::discord) fn current(&self) -> BargeInSensitivity {
        self.state
            .try_read()
            .map(|state| state.sensitivity())
            .unwrap_or_else(|_| BargeInSensitivity::from_u8(self.atom.load(Ordering::Relaxed)))
    }
}
