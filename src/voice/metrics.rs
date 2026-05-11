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
/// voice pipeline. `tts_play_ms` is the time-to-first-audio (start of playback)
/// rather than total playback duration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyTurn {
    pub channel_id: u64,
    pub utterance_id: Option<String>,
    pub stt_ms: u64,
    pub agent_ms: u64,
    pub tts_synth_ms: u64,
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
        Self {
            channel_id,
            utterance_id: partial.utterance_id,
            stt_ms,
            agent_ms,
            tts_synth_ms,
            tts_play_ms,
            total_ms: stt_ms
                .saturating_add(agent_ms)
                .saturating_add(tts_synth_ms)
                .saturating_add(tts_play_ms),
            recorded_at_ms: now_millis(),
        }
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

/// Record TTS stage timings and finalize the turn — the partial entry for
/// `channel_id` is consumed, a [`LatencyTurn`] is built, and an event is
/// pushed into the structured event log so the periodic JSONL flusher writes
/// it to disk.
pub fn record_tts(channel_id: u64, tts_synth_ms: u64, tts_play_ms: u64) -> Option<LatencyTurn> {
    let partial = {
        let Ok(mut map) = registry().lock() else {
            return None;
        };
        let mut entry = map.remove(&channel_id).unwrap_or_default();
        entry.tts_synth_ms = Some(tts_synth_ms);
        entry.tts_play_ms = Some(tts_play_ms);
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatencySummary {
    pub sample_count: usize,
    pub avg_stt_ms: u64,
    pub avg_agent_ms: u64,
    pub avg_tts_synth_ms: u64,
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
        .take(limit)
        .collect();

    let n = samples.len();
    if n == 0 {
        return LatencySummary {
            sample_count: 0,
            avg_stt_ms: 0,
            avg_agent_ms: 0,
            avg_tts_synth_ms: 0,
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
        assert_eq!(turn.tts_play_ms, 200);
        assert_eq!(turn.total_ms, 1470);
        assert_eq!(turn.utterance_id.as_deref(), Some("utt-1"));
    }

    #[test]
    fn missing_stages_default_to_zero_total_uses_known() {
        let ch = fresh_channel(2);
        record_agent(ch, 400);
        let turn = record_tts(ch, 100, 50).expect("turn");
        assert_eq!(turn.stt_ms, 0);
        assert_eq!(turn.agent_ms, 400);
        assert_eq!(turn.total_ms, 550);
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
