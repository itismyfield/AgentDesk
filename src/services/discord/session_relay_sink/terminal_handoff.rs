use async_trait::async_trait;

use super::{SessionBoundDiscordRelaySink, SessionRelayDelivery, delivery_frontier};
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame,
};

#[cfg(test)]
#[path = "p5925b_s1_source_tests.rs"]
pub(in crate::services::discord) mod p5925b_s1_source_tests;

fn recover_native_frame_response(frame: &StreamFrame) -> Result<Option<String>, RelaySinkError> {
    use crate::services::discord::tmux::tmux_output_stream::{
        is_native_codex_payload, read_native_codex_state,
    };
    if !is_native_codex_payload(&frame.binding.provider, &frame.payload) {
        return Ok(None);
    }
    let (Some(start), Some(end)) = (frame.turn_start_offset, frame.terminal_consumed_end) else {
        return Ok(None); // Ordered idle delivery retains its existing range contract.
    };
    let stamp = frame.relay_source_stamp.ok_or_else(|| {
        RelaySinkError::Transient(
            "native Codex terminal frame has no captured source witness".into(),
        )
    })?;
    let source = super::idle_jsonl_relay_source_for_matched(&frame.binding);
    read_native_codex_state(
        &source.path,
        start,
        end,
        stamp.file,
        &frame.session_name,
        frame.relay_generation_mtime_ns.unwrap_or(0),
        Some(stamp),
    )
    .and_then(|decoder| decoder.completed_response())
    .map(Some)
    .map_err(RelaySinkError::Transient)
}

/// #3041 P1-5: the SINK-LOCAL terminal outcome stays deliberately 2-way — the sink
/// always KNOWS its result: confirmed POST/edit → `Delivered`; deterministic
/// route decline (foreign-owner block / bridge-owned / mismatched inflight) →
/// `NotDelivered`; transport/format failure → `Err`. NO sink-local `Unknown` (that
/// is the cross-actor relay-ring + watcher state). `NotDelivered` (former `Skipped`)
/// maps to `RelaySinkOutcome::TerminalNotDelivered`, routed through §3.2
/// reconciliation — never a blind skip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SessionRelayDeliveryOutcome {
    Delivered,
    LandedStale,
    LandedUnrecorded,
    FreshDelivered {
        committed_to: Option<u64>,
        persistence_recorded: bool,
    },
    SentButUncommitted,
    NotDelivered,
}

impl SessionRelayDeliveryOutcome {
    pub(super) fn from_proof(result: delivery_frontier::SinkDeliveryProofResult) -> Self {
        match result {
            delivery_frontier::SinkDeliveryProofResult::Persisted => Self::Delivered,
            delivery_frontier::SinkDeliveryProofResult::LandedStale => Self::LandedStale,
            delivery_frontier::SinkDeliveryProofResult::LandedUnrecorded => Self::LandedUnrecorded,
        }
    }
}

impl SessionBoundDiscordRelaySink {
    fn coordinate_source_allowed(
        &self,
        provider: &crate::services::provider::ProviderKind,
        channel: u64,
        session: &str,
        generation: Option<i64>,
        stamp: Option<crate::services::cluster::stream_relay::SourceStamp>,
        end: Option<u64>,
    ) -> Result<bool, RelaySinkError> {
        use crate::services::cluster::stream_relay::SourceFileIdentity;
        use crate::services::discord::{inflight, outbound::delivery_record};
        if inflight::inflight_runtime_root().is_none() {
            return Ok(true);
        }
        let state = inflight::load_inflight_state_read_only_result(provider, channel)
            .map_err(RelaySinkError::Transient)?;
        let Some(state) = state else {
            return Ok(true);
        };
        let Some(marker) = state.legacy_replay_coordinate.as_ref() else {
            return Ok(true);
        };
        if marker.version != 1
            || state.provider_kind().as_ref() != Some(provider)
            || state.channel_id != channel
        {
            return Ok(false);
        }
        let Some(installed) = marker.installed.as_ref() else {
            return Ok(false);
        };
        if serde_json::to_value(marker.phase)
            .map_err(|e| RelaySinkError::Transient(e.to_string()))?
            != serde_json::json!("installed")
            || state.tmux_session_name.as_deref() != Some(session)
            || state.output_path.as_deref() != Some(installed.output_path.as_str())
            || generation != Some(installed.generation_mtime_ns)
            || installed.generation_mtime_ns == 0
            || delivery_record::current_generation_mtime_ns(session)
                != installed.generation_mtime_ns
        {
            return Ok(false);
        }
        let Some(stamp) = stamp else {
            return Ok(false);
        };
        let stamped_generation = match stamp.witness.generation {
            #[cfg(unix)]
            Some(crate::services::cluster::stream_relay::GenerationSourceIdentity::Unix {
                mtime_ns,
                ..
            }) => Some(mtime_ns),
            Some(
                crate::services::cluster::stream_relay::GenerationSourceIdentity::Unsupported {
                    mtime_ns,
                },
            ) => Some(mtime_ns),
            None => None,
        };
        if stamped_generation != generation {
            return Ok(false);
        }
        #[cfg(unix)]
        if let Some(crate::services::cluster::stream_relay::GenerationSourceIdentity::Unix {
            dev,
            ino,
            ..
        }) = stamp.witness.generation
        {
            let marker_path =
                crate::services::tmux_common::resolve_session_temp_path(session, "generation")
                    .ok_or_else(|| {
                        RelaySinkError::Transient("generation marker disappeared".into())
                    })?;
            let marker_file = std::fs::File::open(marker_path)
                .map_err(|e| RelaySinkError::Transient(e.to_string()))?;
            if SourceFileIdentity::from_open_file(&marker_file)
                != (SourceFileIdentity::Unix { dev, ino })
            {
                return Ok(false);
            }
        } else {
            return Ok(false);
        }
        #[cfg(unix)]
        let expected = SourceFileIdentity::Unix {
            dev: installed.file_identity.0,
            ino: installed.file_identity.1,
        };
        #[cfg(not(unix))]
        let expected = SourceFileIdentity::Unavailable;
        let file = std::fs::File::open(&installed.output_path)
            .map_err(|e| RelaySinkError::Transient(e.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|e| RelaySinkError::Transient(e.to_string()))?;
        if !metadata.is_file() || end.is_some_and(|end| end > metadata.len()) {
            return Ok(false);
        }
        Ok(expected != SourceFileIdentity::Unavailable
            && stamp.file == expected
            && SourceFileIdentity::from_open_file(&file) == expected)
    }

    pub(super) fn source_allowed_before_transport(
        &self,
        delivery: &SessionRelayDelivery,
    ) -> Result<bool, RelaySinkError> {
        if !self.coordinate_source_allowed(
            &delivery.provider,
            delivery.channel_id,
            &delivery.session_name,
            delivery.relay_generation_mtime_ns,
            delivery.relay_source_stamp,
            delivery
                .terminal_consumed_end
                .into_iter()
                .chain(delivery.relay_range.map(|(_, end)| end))
                .max(),
        )? {
            return Ok(false);
        }
        if delivery.relay_range.is_some() {
            let generation = delivery.relay_generation_mtime_ns.unwrap_or(0);
            if generation == 0
                || crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                    &delivery.session_name,
                ) != generation
                || super::idle_jsonl_current_eof(&delivery.provider, &delivery.session_name)
                    .is_none_or(|eof| delivery.relay_range.is_some_and(|(_, end)| end > eof))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn ingest_frame(
        &self,
        frame: &StreamFrame,
        native_response: Option<&str>,
    ) -> Vec<SessionRelayDelivery> {
        self.frames_total
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let Ok(mut sessions) = self.by_session.lock() else {
            return Vec::new();
        };
        let parser = sessions.entry(frame.session_name.clone()).or_default();
        match native_response {
            Some(response) => parser.ingest_verified_native_terminal(frame, response),
            None => parser.ingest_frame(frame),
        }
    }
}

#[async_trait]
impl RelaySink for SessionBoundDiscordRelaySink {
    async fn deliver(&self, frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        let channel = frame
            .binding
            .channel_id
            .parse::<u64>()
            .map_err(|e| RelaySinkError::Transient(e.to_string()))?;
        if !self.coordinate_source_allowed(
            &frame.binding.provider,
            channel,
            &frame.session_name,
            frame.relay_generation_mtime_ns,
            frame.relay_source_stamp,
            frame
                .terminal_consumed_end
                .into_iter()
                .chain(frame.relay_range.map(|(_, end)| end))
                .chain(frame.source_span.map(|(_, end)| end))
                .max(),
        )? {
            return Ok(RelaySinkOutcome::TerminalNotDelivered);
        }
        if frame.relay_range.is_some()
            && (super::super::tmux::tmux_output_stream::is_native_codex_payload(
                &frame.binding.provider,
                &frame.payload,
            ) || super::idle_jsonl_relay_source_for_matched(&frame.binding)
                .allow_continued_session_without_init)
        {
            if let Ok(channel) = frame.binding.channel_id.parse::<u64>()
                && let Some(shared) = self
                    .health_registry
                    .shared_for_provider(&frame.binding.provider)
                    .await
                && super::idle_jsonl::idle_range_is_committed(
                    &shared,
                    &frame.binding.provider,
                    channel,
                    &frame.session_name,
                    frame.relay_range,
                    frame.relay_generation_mtime_ns,
                )
            {
                return Ok(RelaySinkOutcome::TerminalDelivered);
            }
            // Native turn boundaries belong to codex_idle_rollout. Retain this
            // physical range until that owner commits; it can contain two turns.
            return Ok(RelaySinkOutcome::TerminalNotDelivered);
        }
        let native_response = recover_native_frame_response(frame)?;
        // #3041 P1-3 R5 (codex — REVERT R4 fence-gating of the outcome): a result-bearing
        // delivery reports Delivered/NotDelivered REGARDLESS of a fence on this frame
        // (R4's gate BLACK-HOLED the legitimate no-inflight terminal — no fence but a real
        // terminal → `FrameAccepted` → watcher timed out). The co-chunked confusion is now
        // handled by the per-sequence ACK. The fence still ONLY gates the OFFSET ADVANCE
        // (inline in `deliver_response`) — outcome and advance are decoupled.
        let deliveries = self.ingest_frame(frame, native_response.as_deref());
        let fenced_terminal_without_delivery = deliveries.is_empty()
            && matches!(
                (frame.turn_start_offset, frame.terminal_consumed_end),
                (Some(start), Some(end)) if end > start
            );
        let mut terminal_delivered = false;
        let mut terminal_fresh_delivered = None;
        let mut terminal_not_delivered = false;
        for mut delivery in deliveries {
            if let Some(response) = &native_response {
                delivery.response_text.clone_from(response);
            }
            let delivery_outcome = if native_response.is_some()
                && delivery_frontier::current_inflight_matches(
                    &delivery.provider,
                    delivery.channel_id,
                    &delivery.session_name,
                    &delivery,
                )
                .is_none()
                && !self.cancelled_episode_is_retained(&delivery).await
            {
                Ok(SessionRelayDeliveryOutcome::NotDelivered)
            } else {
                self.deliver_response(delivery).await
            };
            #[cfg(test)]
            if let (Ok(outcome), Some(outcomes)) =
                (delivery_outcome.as_ref(), &self.test_delivery_outcomes)
            {
                outcomes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(*outcome);
            }
            match delivery_outcome {
                Ok(SessionRelayDeliveryOutcome::Delivered) => {
                    // #3041 P1-3 (B1 CLOSED): the offset advance is owned INLINE by
                    // `deliver_response` — see `advance_after_confirmed_post`.
                    terminal_delivered = true;
                }
                Ok(
                    SessionRelayDeliveryOutcome::LandedStale
                    | SessionRelayDeliveryOutcome::LandedUnrecorded,
                ) => {
                    // Transport landed, but the captured source authority was
                    // stale or its proof could not be durably recorded. Never
                    // retry this POST into the replacement incarnation.
                    terminal_delivered = true;
                }
                Ok(SessionRelayDeliveryOutcome::FreshDelivered {
                    committed_to,
                    persistence_recorded,
                }) => {
                    terminal_fresh_delivered = Some((committed_to, persistence_recorded));
                }
                Ok(SessionRelayDeliveryOutcome::SentButUncommitted) => {
                    return Ok(RelaySinkOutcome::TerminalUnknown);
                }
                Ok(SessionRelayDeliveryOutcome::NotDelivered) => {
                    terminal_not_delivered = true;
                }
                Err(error) => return Err(error),
            }
        }
        // #3041 P1-3 R5: surface the outcome on THIS frame's sequence (the watcher
        // resolves its own terminal ACK on its exact seq, so a co-chunked tail can't
        // satisfy another turn's ACK). A valid terminal commit fence proves this exact
        // sequence needs a terminal resolution even when parser visibility policy emits
        // no delivery; resolve it as NotDelivered so the watcher reconciles immediately.
        // An unfenced frame with no result-bearing delivery remains `FrameAccepted`.
        // #3041 P1-5: NO `TerminalUnknown` (the sink always KNOWS its result).
        if terminal_delivered {
            Ok(RelaySinkOutcome::TerminalDelivered)
        } else if let Some((committed_to, persistence_recorded)) = terminal_fresh_delivered {
            Ok(RelaySinkOutcome::TerminalFreshDelivered {
                committed_to,
                persistence_recorded,
            })
        } else if terminal_not_delivered || fenced_terminal_without_delivery {
            Ok(RelaySinkOutcome::TerminalNotDelivered)
        } else {
            Ok(RelaySinkOutcome::FrameAccepted)
        }
    }
}
