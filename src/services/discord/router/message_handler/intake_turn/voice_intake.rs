use super::*;

/// #3837 decomposition: voice-transcript announcement resolution lifted
/// verbatim from `handle_text_message`. Behavior-preserving — returns the
/// resolved announcement (if any) plus the `already_accepted` flag exactly as
/// the inline `let voice_announcement = ...` expression and its
/// `voice_announcement_already_accepted` local produced.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_intake_voice_announcement(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    announce_bot_id: Option<u64>,
    is_readable_voice_announcement: bool,
    voice_announcement_ref: &Option<String>,
    gate_resolved_voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    stored_voice_announcement: Option<(crate::voice::prompt::VoiceTranscriptAnnouncement, bool)>,
) -> (
    Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    bool,
) {
    let mut voice_announcement_already_accepted = false;
    let voice_announcement = if let Some(resolved) = gate_resolved_voice_announcement {
        // #3905: the intake gate already resolved + authorized this voice
        // announcement (non-consuming) at `intake_gate::resolve_voice_transcript_announcement_for_intake`
        // before choosing direct dispatch. Trust that carry-forward instead of
        // re-deriving from scratch here — independent re-derivation re-checks the
        // LIVE durable row (`consumed_at IS NULL`) and loses to a sibling gateway
        // (one process hosts many agent-bot gateways) that consumed the row
        // between gate resolution and here, spuriously dropping a message the
        // gate already authorized (queued dispatch is immune because it carries
        // the payload via `Intervention.voice_announcement` accepted-replay;
        // only the direct path was exposed). `voice_announcement_already_accepted`
        // deliberately stays `false` so the per-message durable claim in
        // `route_voice_transcript_announcement_once` still runs: the claim winner
        // dispatches and racing siblings get `DuplicateSuppressed` (clean, no
        // warn), so the #3464 single-dispatch dedup is preserved — never a double
        // answer.
        Some(resolved)
    } else if announce_bot_id == Some(request_owner.get()) {
        if let Some((announcement, accepted_replay)) = stored_voice_announcement {
            if let Some(pool) = shared.pg_pool.as_ref() {
                match crate::voice::announce_meta::load_voice_announcement_durable(
                    pool,
                    user_msg_id,
                )
                .await
                {
                    Ok(Some(durable)) => Some(durable),
                    Ok(None) if accepted_replay => {
                        match crate::voice::announce_meta::load_consumed_voice_announcement_durable(
                            pool,
                            user_msg_id,
                        )
                        .await
                        {
                            Ok(Some(consumed)) => {
                                voice_announcement_already_accepted = true;
                                Some(consumed)
                            }
                            Ok(None) => {
                                tracing::info!(
                                    channel_id = channel_id.get(),
                                    message_id = user_msg_id.get(),
                                    "accepted queued voice transcript announcement has no consumed durable row; refusing local replay"
                                );
                                None
                            }
                            Err(error) => {
                                tracing::warn!(
                                    error = %error,
                                    channel_id = channel_id.get(),
                                    message_id = user_msg_id.get(),
                                    "accepted queued voice transcript announcement consumed durable metadata load failed"
                                );
                                None
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!(
                            channel_id = channel_id.get(),
                            message_id = user_msg_id.get(),
                            "stored voice transcript announcement has no live durable row; refusing local-only consume"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            channel_id = channel_id.get(),
                            message_id = user_msg_id.get(),
                            "voice transcript announcement durable metadata load failed after local store hit"
                        );
                        None
                    }
                }
            } else {
                Some(announcement)
            }
        } else if is_readable_voice_announcement {
            match shared.pg_pool.as_ref() {
                Some(pool) => match crate::voice::announce_meta::load_voice_announcement_durable(
                    pool,
                    user_msg_id,
                )
                .await
                {
                    Ok(Some(announcement)) => Some(announcement),
                    Ok(None) => {
                        if let Some(pending_key) = voice_announcement_ref.as_deref() {
                            match crate::voice::announce_meta::bind_pending_voice_announcement_by_key_durable(
                                pool,
                                pending_key,
                                channel_id,
                                user_msg_id,
                            )
                            .await
                            {
                                Ok(Some(announcement)) => Some(announcement),
                                Ok(None) => None,
                                Err(error) => {
                                    tracing::warn!(
                                        error = %error,
                                        channel_id = channel_id.get(),
                                        message_id = user_msg_id.get(),
                                        "voice transcript announcement pending metadata bind failed"
                                    );
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            channel_id = channel_id.get(),
                            message_id = user_msg_id.get(),
                            "voice transcript announcement durable metadata load failed"
                        );
                        None
                    }
                },
                None => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    (voice_announcement, voice_announcement_already_accepted)
}
