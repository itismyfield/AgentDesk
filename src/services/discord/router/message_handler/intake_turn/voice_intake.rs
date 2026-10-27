use super::*;

/// Resolves the authorized durable announcement without changing its claim owner.
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
        // Re-reading the live row can lose to a sibling gateway that already consumed it.
        // Keep the gate-authorized payload; the per-message durable claim still deduplicates it.
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

pub(super) fn prompt(announcement: &crate::voice::prompt::VoiceTranscriptAnnouncement) -> String {
    let mut context = format!("voice_utterance_id: {}", announcement.utterance_id);
    if let Some(started_at) = announcement.started_at.as_deref() {
        context.push_str(&format!("\nvoice_started_at: {started_at}"));
    }
    if let Some(completed_at) = announcement.completed_at.as_deref() {
        context.push_str(&format!("\nvoice_completed_at: {completed_at}"));
    }
    if let Some(samples_written) = announcement.samples_written {
        context.push_str(&format!("\nvoice_samples_written: {samples_written}"));
    }
    crate::voice::prompt::voice_bridge_prompt(
        &announcement.transcript,
        &announcement.language,
        announcement.verbose_progress,
        Some(&context),
    )
}

#[cfg(test)]
mod preparation_tests {
    use super::*;
    #[test]
    fn extracted_voice_prompt_keeps_transcription_and_provenance_bytes() {
        let voice: crate::voice::prompt::VoiceTranscriptAnnouncement = serde_json::from_value(serde_json::json!({
            "transcript":"두 문단\n\n_ literal WIP", "user_id":"42", "utterance_id":"utterance", "language":"ko-KR", "verbose_progress":true,
            "started_at":"start", "completed_at":"end", "samples_written":48000,
            "control_channel_id":null, "stt_mode":null, "stt_latency_ms":null })).unwrap();
        let saved = serde_json::to_value(&voice).unwrap();
        let expected = crate::voice::prompt::voice_bridge_prompt(
            "두 문단\n\n_ literal WIP",
            "ko-KR",
            true,
            Some(
                "voice_utterance_id: utterance\nvoice_started_at: start\nvoice_completed_at: end\nvoice_samples_written: 48000",
            ),
        );
        let normalize = |text: String| {
            let tags = regex::Regex::new(r"user_transcript_([0-9a-f]{32})").unwrap();
            let captures: Vec<_> = tags.captures_iter(&text).map(|c| c[1].to_owned()).collect();
            assert_eq!(captures.len(), 4);
            assert!(captures.iter().all(|nonce| nonce == &captures[0]));
            text.replace(&captures[0], "NONCE")
        };
        assert_eq!(
            normalize(prompt(&voice)).as_bytes(),
            normalize(expected).as_bytes()
        );
        assert_eq!(serde_json::to_value(&voice).unwrap(), saved);
    }
}
