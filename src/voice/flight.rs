//! Compact structured diagnostics for the voice utterance flight path.
//!
//! This complements `voice_latency_turn`: latency keeps the existing numeric
//! rollup, while `voice_flight_event` records route and correlation decisions
//! at the points where they are already known.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::services::observability::events;

pub(crate) const VOICE_FLIGHT_EVENT_TYPE: &str = "voice_flight_event";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VoiceFlightRoute {
    ForegroundSpeak,
    ForegroundSilence,
    BackgroundHandoff,
    Queued,
    Deferred,
    ExplicitStop,
    IgnoredNoise,
    FallbackNormalTurn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VoiceFlightEvent {
    pub(crate) route: VoiceFlightRoute,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) voice_channel_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) control_channel_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) background_channel_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cancel_channel_id: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) utterance_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stt_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stt_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transcript_chars: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) foreground_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) foreground_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) foreground_latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) foreground_decision: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_correlation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_message_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) background_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) turn_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tts_chars: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tts_first_audio_ms: Option<u64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) barge_in: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cancel_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cancelled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) already_stopping: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

impl VoiceFlightEvent {
    pub(crate) fn new(route: VoiceFlightRoute) -> Self {
        Self {
            route,
            voice_channel_id: None,
            control_channel_id: None,
            background_channel_id: None,
            cancel_channel_id: None,
            agent_id: None,
            user_id: None,
            utterance_id: None,
            stt_mode: None,
            stt_latency_ms: None,
            transcript_chars: None,
            foreground_provider: None,
            foreground_model: None,
            foreground_latency_ms: None,
            foreground_decision: None,
            handoff_correlation_id: None,
            handoff_message_id: None,
            background_turn_id: None,
            turn_id: None,
            tts_chars: None,
            tts_first_audio_ms: None,
            barge_in: None,
            cancel_source: None,
            cancelled: None,
            already_stopping: None,
            reason: None,
        }
    }

    pub(crate) fn to_payload(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({ "route": "serialize_error" }))
    }

    fn event_channel_id(&self) -> Option<u64> {
        self.voice_channel_id.or(self.control_channel_id)
    }
}

pub(crate) fn record_voice_flight_event(event: VoiceFlightEvent) {
    events::record_simple(
        VOICE_FLIGHT_EVENT_TYPE,
        event.event_channel_id(),
        Some("voice"),
        event.to_payload(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_for(event: VoiceFlightEvent) -> Value {
        event.to_payload()
    }

    #[test]
    fn foreground_speak_payload_carries_route_and_tts_metadata() {
        let mut event = VoiceFlightEvent::new(VoiceFlightRoute::ForegroundSpeak);
        event.voice_channel_id = Some(123);
        event.user_id = Some("42".to_string());
        event.utterance_id = Some("utt-1".to_string());
        event.foreground_provider = Some("codex".to_string());
        event.foreground_model = Some("gpt-5.4".to_string());
        event.foreground_decision = Some("speak".to_string());
        event.transcript_chars = Some(9);
        event.tts_chars = Some(4);

        let payload = payload_for(event);
        assert_eq!(payload["route"], "foreground_speak");
        assert_eq!(payload["voice_channel_id"], 123);
        assert_eq!(payload["foreground_provider"], "codex");
        assert_eq!(payload["foreground_model"], "gpt-5.4");
        assert_eq!(payload["foreground_decision"], "speak");
        assert_eq!(payload["transcript_chars"], 9);
        assert_eq!(payload["tts_chars"], 4);
    }

    #[test]
    fn foreground_silence_payload_carries_decision_metadata() {
        let mut event = VoiceFlightEvent::new(VoiceFlightRoute::ForegroundSilence);
        event.voice_channel_id = Some(123);
        event.utterance_id = Some("utt-2".to_string());
        event.foreground_latency_ms = Some(77);
        event.foreground_decision = Some("silence".to_string());

        let payload = payload_for(event);
        assert_eq!(payload["route"], "foreground_silence");
        assert_eq!(payload["foreground_decision"], "silence");
        assert_eq!(payload["foreground_latency_ms"], 77);
    }

    #[test]
    fn background_handoff_payload_carries_correlation_metadata() {
        let mut event = VoiceFlightEvent::new(VoiceFlightRoute::BackgroundHandoff);
        event.voice_channel_id = Some(123);
        event.background_channel_id = Some(456);
        event.handoff_correlation_id = Some("voice-bg:abc".to_string());
        event.handoff_message_id = Some(789);
        event.background_turn_id = Some("turn-1".to_string());
        event.foreground_decision = Some("handoff_background".to_string());

        let payload = payload_for(event);
        assert_eq!(payload["route"], "background_handoff");
        assert_eq!(payload["background_channel_id"], 456);
        assert_eq!(payload["handoff_correlation_id"], "voice-bg:abc");
        assert_eq!(payload["handoff_message_id"], 789);
        assert_eq!(payload["background_turn_id"], "turn-1");
    }

    #[test]
    fn explicit_stop_and_deferred_payloads_carry_barge_in_metadata() {
        let mut stop = VoiceFlightEvent::new(VoiceFlightRoute::ExplicitStop);
        stop.voice_channel_id = Some(123);
        stop.cancel_channel_id = Some(456);
        stop.barge_in = Some(true);
        stop.cancel_source = Some("voice_barge_in_explicit_stop".to_string());
        stop.cancelled = Some(true);
        stop.already_stopping = Some(false);
        let stop_payload = payload_for(stop);
        assert_eq!(stop_payload["route"], "explicit_stop");
        assert_eq!(stop_payload["cancel_channel_id"], 456);
        assert_eq!(stop_payload["barge_in"], true);
        assert_eq!(
            stop_payload["cancel_source"],
            "voice_barge_in_explicit_stop"
        );
        assert_eq!(stop_payload["cancelled"], true);
        assert_eq!(stop_payload["already_stopping"], false);

        let mut deferred = VoiceFlightEvent::new(VoiceFlightRoute::Deferred);
        deferred.voice_channel_id = Some(123);
        deferred.barge_in = Some(true);
        deferred.transcript_chars = Some(11);
        deferred.reason = Some("processing_barge_in_defer".to_string());
        let deferred_payload = payload_for(deferred);
        assert_eq!(deferred_payload["route"], "deferred");
        assert_eq!(deferred_payload["barge_in"], true);
        assert_eq!(deferred_payload["reason"], "processing_barge_in_defer");
    }

    #[test]
    fn record_writes_structured_voice_flight_event() {
        crate::services::observability::events::global().clear();
        let mut event = VoiceFlightEvent::new(VoiceFlightRoute::Queued);
        event.voice_channel_id = Some(123);
        event.control_channel_id = Some(123);
        event.background_channel_id = Some(456);
        event.stt_mode = Some("file".to_string());
        event.stt_latency_ms = Some(88);
        record_voice_flight_event(event);

        let recent = crate::services::observability::events::recent(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].event_type, VOICE_FLIGHT_EVENT_TYPE);
        assert_eq!(recent[0].channel_id, Some(123));
        assert_eq!(recent[0].provider.as_deref(), Some("voice"));
        assert_eq!(recent[0].payload["route"], "queued");
        assert_eq!(recent[0].payload["stt_mode"], "file");
        assert_eq!(recent[0].payload["stt_latency_ms"], 88);
    }
}
