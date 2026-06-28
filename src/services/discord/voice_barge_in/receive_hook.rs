use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::provider::ProviderKind;
use crate::voice::{CompletedUtterance, VoiceReceiveHook};

use super::{SharedData, VoiceBargeInRuntime};

pub(in crate::services::discord) struct DiscordVoiceBargeInHook {
    runtime: Arc<VoiceBargeInRuntime>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
}

impl DiscordVoiceBargeInHook {
    pub(in crate::services::discord) fn new(
        runtime: Arc<VoiceBargeInRuntime>,
        shared: Arc<SharedData>,
        provider: ProviderKind,
    ) -> Self {
        Self {
            runtime,
            shared,
            provider,
        }
    }
}

impl VoiceReceiveHook for DiscordVoiceBargeInHook {
    fn observe_pcm(&self, control_channel_id: u64, user_id: u64, samples: &[i16]) {
        let channel_id = ChannelId::new(control_channel_id);
        self.runtime
            .observe_streaming_stt_pcm_i16(channel_id, user_id, samples);
        let Some(cut) = self.runtime.observe_live_pcm_i16(channel_id, samples) else {
            return;
        };

        let shared = self.shared.clone();
        // F22 (#2046): playback_owner 라벨 추가 — 어떤 progress / spoken_result
        // playback 이 cut 되었는지 사후 분석 가능.
        let playback_owner = self
            .runtime
            .playbacks
            .get(&channel_id.get())
            .and_then(|entry| entry.value().owner);
        // Issue #2335 (b): the live PCM cut path is a parallel termination
        // path that, prior to this fix, did NOT call
        // `cancel_inflight_foreground_calls`. As a result PCM cut would
        // silence the speaker / kill the background turn while a
        // foreground Codex/Claude child kept running to natural exit.
        //
        // Codex review (round 2): perform the foreground-token cancel
        // SYNCHRONOUSLY here, BEFORE the tokio::spawn that handles the (async)
        // mailbox cancel. If deferred into the spawned task, a fast foreground
        // call could complete and unregister its token between cut detection and
        // the spawn being scheduled, in which case `cancel_inflight_foreground_calls`
        // would see an empty registry and the stale reply / ack / handoff would
        // still proceed after the user explicitly barged in.
        let foreground_cancelled = self
            .runtime
            .cancel_inflight_foreground_calls(channel_id, "voice_barge_in_live_cut");
        let runtime = self.runtime.clone();
        tokio::spawn(async move {
            let cancel_channel = runtime
                .active_barge_in_mailbox_channel(&shared, channel_id)
                .await
                .unwrap_or(channel_id);
            let result = super::super::mailbox_cancel_active_turn_with_reason(
                &shared,
                cancel_channel,
                "voice_barge_in_live_cut",
            )
            .await;
            tracing::info!(
                channel_id = channel_id.get(),
                cancel_channel_id = cancel_channel.get(),
                mean_db = cut.levels.mean_db,
                max_db = cut.levels.max_db,
                sensitivity = ?cut.sensitivity,
                candidate_frames = cut.candidate_frames,
                playback_owner = ?playback_owner,
                cancelled = result.token.is_some(),
                already_stopping = result.already_stopping,
                foreground_cancelled,
                "voice live barge-in cut processed"
            );
        });
    }

    fn utterance_completed(&self, control_channel_id: u64, utterance: &CompletedUtterance) {
        let runtime = self.runtime.clone();
        let shared = self.shared.clone();
        let provider = self.provider.clone();
        let utterance = utterance.clone();
        tokio::spawn(async move {
            let channel_id = ChannelId::new(control_channel_id);
            let outcome = runtime
                .process_completed_utterance(&shared, &provider, channel_id, &utterance)
                .await;
            tracing::debug!(
                channel_id = channel_id.get(),
                utterance_id = %utterance.utterance_id,
                outcome = ?outcome,
                "voice barge-in transcript processing finished"
            );
            // #2156: STT 및 후속 처리가 완료된 시점이므로 utterance wav / segment /
            // transcript sidecar 를 정리한다. config 가 keep_recordings=true 거나
            // 환경변수 ADK_VOICE_KEEP_WAV=1 인 경우 cleanup_utterance_artifacts 내부에서
            // no-op 처리된다.
            runtime.cleanup_utterance_artifacts(&utterance).await;
        });
    }
}
