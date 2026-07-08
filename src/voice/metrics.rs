//! Voice turn latency metrics — STT/agent/TTS stage millis aggregated per
//! channel and emitted to the structured event log when the TTS stage completes.
//!
//! Hot path callers populate per-stage timing via `record_stt`, `record_agent`,
//! and `record_tts`, all keyed by the channel that owns the active voice turn.
//! When TTS finishes, `record_tts` consumes the partial state, builds a
//! [`LatencyTurn`], pushes it into the structured event log
//! (`event_type = "voice_latency_turn"`) and clears the channel slot.
//!
//! `recent_summary` exposes a snapshot for the `/voice latency` slash command
//! by replaying the same JSONL we just wrote.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::services::observability::events;

/// Final per-turn latency record. Times are wall-clock millis spent inside the
/// voice pipeline. `first_audio_out_ms` is the time-to-first-audio (start of
/// playback) rather than total playback duration. `tts_play_ms` is retained as
/// the legacy field name for existing dashboards/events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyTurn {
    pub channel_id: u64,
    pub utterance_id: Option<String>,
    pub stt_ms: u64,
    pub agent_ms: u64,
    pub tts_synth_ms: u64,
    #[serde(default)]
    pub first_audio_out_ms: u64,
    pub tts_play_ms: u64,
    pub total_ms: u64,
    pub recorded_at_ms: i64,
}

impl LatencyTurn {
    fn from_partial(partial: PartialLatency, channel_id: u64) -> Self {
        let stt_ms = partial.stt_ms.unwrap_or(0);
        let agent_ms = partial.agent_ms.unwrap_or(0);
        let tts_synth_ms = partial.tts_synth_ms.unwrap_or(0);
        let tts_play_ms = partial.tts_play_ms.unwrap_or(0);
        let first_audio_out_ms = tts_play_ms;
        Self {
            channel_id,
            utterance_id: partial.utterance_id,
            stt_ms,
            agent_ms,
            tts_synth_ms,
            first_audio_out_ms,
            tts_play_ms,
            // total_ms is the intake→first-audio wall clock: STT + agent + the
            // TTS time-to-first-audio (`first_audio_out_ms`). `first_audio_out_ms`
            // is measured from the start of TTS playback, so it already subsumes
            // the first chunk's synthesis time (`tts_synth_ms`). `tts_synth_ms` is
            // therefore retained only as a standalone observability sub-metric and
            // must NOT be added here as well, otherwise the first-chunk synthesis
            // would be double-counted and total_ms over-reported (#3913).
            total_ms: stt_ms
                .saturating_add(agent_ms)
                .saturating_add(first_audio_out_ms),
            recorded_at_ms: now_millis(),
        }
    }

    fn with_latency_aliases(mut self) -> Self {
        if self.first_audio_out_ms == 0 && self.tts_play_ms != 0 {
            self.first_audio_out_ms = self.tts_play_ms;
        }
        self
    }

    pub fn to_payload(&self) -> Value {
        // Single source of truth via Serialize derive — fields stay in sync if
        // LatencyTurn grows. Falls back to an empty object if (somehow)
        // serialization fails so callers never see Result here.
        serde_json::to_value(self).unwrap_or_else(|_| json!({}))
    }
}

#[derive(Debug, Default, Clone)]
struct PartialLatency {
    utterance_id: Option<String>,
    stt_ms: Option<u64>,
    agent_ms: Option<u64>,
    tts_synth_ms: Option<u64>,
    tts_play_ms: Option<u64>,
}

fn registry() -> &'static Mutex<HashMap<u64, PartialLatency>> {
    static REG: OnceLock<Mutex<HashMap<u64, PartialLatency>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

fn agent_start_registry() -> &'static Mutex<HashMap<u64, Instant>> {
    static REG: OnceLock<Mutex<HashMap<u64, Instant>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mark the moment the agent turn was kicked off for `channel_id`. Replaces
/// any prior pending start. Pair with [`finish_agent_start`] when the agent
/// answer is about to enter TTS.
pub fn mark_agent_start(channel_id: u64) {
    if let Ok(mut map) = agent_start_registry().lock() {
        map.insert(channel_id, Instant::now());
    }
}

/// Pop the matching start instant set by [`mark_agent_start`] and record the
/// elapsed agent stage millis. Returns `Some(ms)` if a paired start was found.
pub fn finish_agent_start(channel_id: u64) -> Option<u64> {
    let started_at = {
        let mut map = agent_start_registry().lock().ok()?;
        map.remove(&channel_id)?
    };
    let ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
    record_agent(channel_id, ms);
    Some(ms)
}

/// Drop a pending [`mark_agent_start`] without recording an agent_ms — used
/// when the turn fails before the answer is ready (e.g. `start_voice_turn`
/// errors out) so the next turn's `mark_agent_start` doesn't carry a stale
/// instant.
// reason: voice runtime is wired only when voice config is enabled; no compile
// target exercises it. See #3034.
#[allow(dead_code)]
pub fn discard_agent_start(channel_id: u64) {
    if let Ok(mut map) = agent_start_registry().lock() {
        map.remove(&channel_id);
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn upsert<F: FnOnce(&mut PartialLatency)>(channel_id: u64, mutate: F) -> Option<PartialLatency> {
    let Ok(mut map) = registry().lock() else {
        return None;
    };
    let entry = map.entry(channel_id).or_default();
    mutate(entry);
    Some(entry.clone())
}

/// Record the STT stage millis for a channel's active voice turn. Optionally
/// attach the source utterance identifier (used as a debug correlation key).
pub fn record_stt(channel_id: u64, utterance_id: Option<&str>, stt_ms: u64) {
    upsert(channel_id, |partial| {
        partial.stt_ms = Some(stt_ms);
        if let Some(id) = utterance_id {
            partial.utterance_id = Some(id.to_string());
        }
    });
}

/// Record the agent (LLM turn) duration in millis.
pub fn record_agent(channel_id: u64, agent_ms: u64) {
    upsert(channel_id, |partial| {
        partial.agent_ms = Some(agent_ms);
    });
}

/// Record TTS stage timings and finalize the turn. The final argument is
/// first-audio-out millis; it is stored under both `first_audio_out_ms` and the
/// legacy `tts_play_ms` field until downstream readers migrate.
pub fn record_tts(
    channel_id: u64,
    tts_synth_ms: u64,
    first_audio_out_ms: u64,
) -> Option<LatencyTurn> {
    let partial = {
        let Ok(mut map) = registry().lock() else {
            return None;
        };
        let mut entry = map.remove(&channel_id).unwrap_or_default();
        entry.tts_synth_ms = Some(tts_synth_ms);
        entry.tts_play_ms = Some(first_audio_out_ms);
        entry
    };
    let turn = LatencyTurn::from_partial(partial, channel_id);
    events::record_simple(
        "voice_latency_turn",
        Some(channel_id),
        Some("voice"),
        turn.to_payload(),
    );
    Some(turn)
}

/// Drop any half-built record for a channel — used when a turn is cancelled
/// before TTS playback so the next turn doesn't inherit stale numbers.
pub fn discard(channel_id: u64) {
    if let Ok(mut map) = registry().lock() {
        map.remove(&channel_id);
    }
}

/// #3914: terminal outcome of a file-mode STT transcription. STT previously
/// swallowed low-volume skips and empty-after-retry results as
/// `Ok(String::new())` with at most a debug log, so a systemic regression
/// (whisper returning empty, the volume gate over-skipping, or `volumedetect`
/// failing) was invisible. These outcomes are counted process-wide and the
/// anomalous ones are emitted as a structured `voice_stt_outcome` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SttOutcome {
    /// whisper produced a usable (non-empty, cleaned) transcript.
    Transcribed,
    /// the utterance was gated out by the low-volume silence check before whisper.
    LowVolumeSkipped,
    /// whisper ran (including one retry) but the cleaned transcript was empty.
    EmptyAfterRetry,
    /// the `volumedetect` pre-pass failed; STT continued without the gate.
    VolumeDetectFailed,
}

impl SttOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transcribed => "transcribed",
            Self::LowVolumeSkipped => "low_volume_skipped",
            Self::EmptyAfterRetry => "empty_after_retry",
            Self::VolumeDetectFailed => "volumedetect_failed",
        }
    }

    /// #4238: genuine STT failures worth an operator `warn!`, as opposed to the
    /// benign `LowVolumeSkipped` (the silence gate firing on a quiet utterance,
    /// which is the common expected case and must not spam the log).
    fn is_failure(self) -> bool {
        matches!(self, Self::EmptyAfterRetry | Self::VolumeDetectFailed)
    }
}

fn stt_outcome_registry() -> &'static Mutex<HashMap<&'static str, u64>> {
    static REG: OnceLock<Mutex<HashMap<&'static str, u64>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record one STT outcome: bumps the process-wide counter and (for the
/// anomalous outcomes) emits a structured `voice_stt_outcome` event. The common
/// success path only bumps the counter so the shared event buffer is not
/// crowded out of its `voice_latency_turn` samples.
pub fn record_stt_outcome(outcome: SttOutcome) {
    if let Ok(mut map) = stt_outcome_registry().lock() {
        *map.entry(outcome.as_str()).or_insert(0) += 1;
    }
    if !matches!(outcome, SttOutcome::Transcribed) {
        events::record_simple(
            "voice_stt_outcome",
            None,
            Some("voice"),
            json!({ "outcome": outcome.as_str() }),
        );
        // #4238: the metric counter alone was invisible to operators watching
        // logs. Surface genuine STT failures as a structured `warn!` so a
        // recovery-worthy regression (whisper returning empty, volume pre-pass
        // failing) is observable, not just silently tallied.
        if outcome.is_failure() {
            tracing::warn!(
                outcome = outcome.as_str(),
                "voice STT turn failed to yield a usable transcript (#4238)"
            );
        }
    }
}

#[cfg(test)]
pub fn stt_outcome_count(outcome: SttOutcome) -> u64 {
    stt_outcome_registry()
        .lock()
        .ok()
        .and_then(|map| map.get(outcome.as_str()).copied())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencySummary {
    pub sample_count: usize,
    pub avg_stt_ms: u64,
    pub avg_agent_ms: u64,
    pub avg_tts_synth_ms: u64,
    pub avg_first_audio_out_ms: u64,
    pub avg_tts_play_ms: u64,
    pub avg_total_ms: u64,
    pub samples: Vec<LatencyTurn>,
}

/// Pull the most recent voice-latency events out of the structured event log
/// and average the stage millis. Filters down to `voice_latency_turn` events
/// then keeps the last `limit` entries (newest first in `samples`).
pub fn recent_summary(limit: usize) -> LatencySummary {
    let limit = limit.max(1);
    // Pull a generous window so non-voice traffic in the buffer doesn't crowd
    // out our voice samples.
    let window = limit.saturating_mul(20).max(events::MAX_EVENTS / 4);
    let raw = events::recent(window);
    let mut samples: Vec<LatencyTurn> = raw
        .into_iter()
        .rev()
        .filter(|ev| ev.event_type == "voice_latency_turn")
        .filter_map(|ev| serde_json::from_value::<LatencyTurn>(ev.payload).ok())
        .map(LatencyTurn::with_latency_aliases)
        .take(limit)
        .collect();

    let n = samples.len();
    if n == 0 {
        return LatencySummary {
            sample_count: 0,
            avg_stt_ms: 0,
            avg_agent_ms: 0,
            avg_tts_synth_ms: 0,
            avg_first_audio_out_ms: 0,
            avg_tts_play_ms: 0,
            avg_total_ms: 0,
            samples,
        };
    }

    let mut sum_stt: u128 = 0;
    let mut sum_agent: u128 = 0;
    let mut sum_synth: u128 = 0;
    let mut sum_play: u128 = 0;
    let mut sum_total: u128 = 0;
    for s in &samples {
        sum_stt += s.stt_ms as u128;
        sum_agent += s.agent_ms as u128;
        sum_synth += s.tts_synth_ms as u128;
        sum_play += s.tts_play_ms as u128;
        sum_total += s.total_ms as u128;
    }
    let avg = |sum: u128| -> u64 { (sum / n as u128) as u64 };
    samples.reverse(); // newest first → newest last for chronological display
    LatencySummary {
        sample_count: n,
        avg_stt_ms: avg(sum_stt),
        avg_agent_ms: avg(sum_agent),
        avg_tts_synth_ms: avg(sum_synth),
        avg_first_audio_out_ms: avg(sum_play),
        avg_tts_play_ms: avg(sum_play),
        avg_total_ms: avg(sum_total),
        samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_channel(seed: u64) -> u64 {
        // Use a high random-ish channel id to avoid collisions with other tests.
        0xABCD_0000_0000_0000 + seed
    }

    #[test]
    fn record_tts_emits_jsonl_event() {
        let ch = fresh_channel(1);
        record_stt(ch, Some("utt-1"), 120);
        record_agent(ch, 850);
        let turn = record_tts(ch, 300, 200).expect("turn emitted");
        assert_eq!(turn.channel_id, ch);
        assert_eq!(turn.stt_ms, 120);
        assert_eq!(turn.agent_ms, 850);
        assert_eq!(turn.tts_synth_ms, 300);
        assert_eq!(turn.first_audio_out_ms, 200);
        assert_eq!(turn.tts_play_ms, 200);
        // total = stt(120) + agent(850) + first_audio_out(200); tts_synth_ms is a
        // sub-metric already inside first_audio_out_ms and must not be re-added.
        assert_eq!(turn.total_ms, 1170);
        assert_eq!(turn.utterance_id.as_deref(), Some("utt-1"));
        assert_eq!(turn.to_payload()["first_audio_out_ms"], 200);
        assert_eq!(turn.to_payload()["tts_play_ms"], 200);
    }

    #[test]
    fn missing_stages_default_to_zero_total_uses_known() {
        let ch = fresh_channel(2);
        record_agent(ch, 400);
        let turn = record_tts(ch, 100, 50).expect("turn");
        assert_eq!(turn.stt_ms, 0);
        assert_eq!(turn.agent_ms, 400);
        assert_eq!(turn.first_audio_out_ms, 50);
        // total = stt(0) + agent(400) + first_audio_out(50); synth(100) excluded.
        assert_eq!(turn.total_ms, 450);
    }

    #[test]
    fn total_ms_does_not_double_count_first_chunk_synthesis() {
        // Reproduces the call-site contract from
        // voice_barge_in/final_result_playback.rs: `first_audio_out_ms`
        // (the second record_tts arg) is measured from the START of TTS playback,
        // so it ALREADY contains the first chunk's synthesis time. The synth time
        // passed as the first arg is a sub-component of it, not an additive phase.
        let ch = fresh_channel(4);
        record_stt(ch, Some("utt-dbl"), 100);
        record_agent(ch, 500);

        // Synthetic timing: first chunk took 300ms to synthesize, and first audio
        // went out 360ms after playback began (300ms synth + 60ms queue/handoff).
        let first_chunk_synthesis_ms = 300;
        let first_audio_out_ms = 360;
        let turn = record_tts(ch, first_chunk_synthesis_ms, first_audio_out_ms).expect("turn");

        // synth is preserved as a standalone observability sub-metric.
        assert_eq!(turn.tts_synth_ms, 300);
        assert_eq!(turn.first_audio_out_ms, 360);

        // Correct total: intake→first-audio = stt + agent + time-to-first-audio.
        let expected = 100 + 500 + first_audio_out_ms;
        assert_eq!(turn.total_ms, expected, "total must be intake→first-audio");

        // Guard against regression to the old double-counting accounting, which
        // would have produced stt + agent + synth + first_audio_out.
        let old_inflated = 100 + 500 + first_chunk_synthesis_ms + first_audio_out_ms;
        assert!(
            turn.total_ms < old_inflated,
            "total must not re-add first-chunk synth ({} vs inflated {})",
            turn.total_ms,
            old_inflated,
        );
        assert_eq!(
            old_inflated - turn.total_ms,
            first_chunk_synthesis_ms,
            "inflation removed must equal exactly the double-counted synth time",
        );
    }

    #[test]
    fn discard_clears_partial_state() {
        let ch = fresh_channel(3);
        record_stt(ch, None, 100);
        discard(ch);
        let turn = record_tts(ch, 50, 50).expect("turn");
        assert_eq!(turn.stt_ms, 0); // no carry-over from discarded record
        assert_eq!(turn.tts_synth_ms, 50);
    }

    #[test]
    fn summary_handles_empty_window() {
        let sum = recent_summary(5);
        // Even if other tests pushed events we just need the API not to panic.
        assert!(sum.samples.len() <= 5);
        if sum.sample_count == 0 {
            assert_eq!(sum.avg_total_ms, 0);
        }
    }
}
