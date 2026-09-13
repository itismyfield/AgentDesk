use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;

#[derive(PartialEq, Eq)]
struct SourceAtEof {
    file: SourceFileIdentity,
    modified: std::time::SystemTime,
    end: u64,
}

impl SourceAtEof {
    fn capture(row: &inflight::InflightTurnState, output: &Path) -> Option<Self> {
        if row.output_path.as_deref().map(Path::new) != Some(output) {
            return None;
        }
        let file = std::fs::File::open(output).ok()?;
        let metadata = file.metadata().ok()?;
        if metadata.len() != row.last_offset {
            return None;
        }
        Some(Self {
            file: SourceFileIdentity::from_open_file(&file),
            modified: metadata.modified().ok()?,
            end: metadata.len(),
        })
    }
}

pub(in crate::services::discord) async fn recover_idle_partial_response(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    row: &inflight::InflightTurnState,
    output: &Path,
) -> bool {
    let Some(provider) = row.provider_kind() else {
        return false;
    };
    let retry_delay =
        super::super::health::relay_recovery_retry_delay_secs(row.recovery_relay_attempts);
    if retry_delay > 0
        && inflight::parse_updated_at_unix(&row.updated_at).is_none_or(|updated| {
            chrono::Utc::now()
                .timestamp()
                .saturating_sub(updated)
                .max(0)
                < retry_delay
        })
    {
        return false;
    }
    let Some(tmux) = row.tmux_session_name.as_deref() else {
        return false;
    };
    if !recovery_ready_without_output_has_captured_response(row)
        || row.restart_mode.is_some()
        || row.rebind_origin
        || row.terminal_delivery_completed()
        || row.current_msg_id == 0
        || shared.relay_emission_in_flight(ChannelId::new(row.channel_id))
        || !crate::services::provider::tmux_session_fallback_ready_for_input(
            tmux,
            &provider,
            row.runtime_kind,
        )
        .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
    {
        return false;
    }
    let Some(source) = SourceAtEof::capture(row, output) else {
        return false;
    };
    let Some(start) = row.turn_start_offset.filter(|start| *start < source.end) else {
        return false;
    };
    if extract_response_from_output(&output.to_string_lossy(), start) != row.full_response {
        return false;
    }
    let Some(claim) =
        super::super::tui_prompt_relay::capture_dormant_partial(shared, row, output).await
    else {
        return false;
    };
    if SourceAtEof::capture(&claim.row, output).as_ref() != Some(&source)
        || shared.relay_emission_in_flight(ChannelId::new(row.channel_id))
    {
        return false;
    }
    let state = &claim.row;
    let context = RecoveryDeliveryContext::from_state(
        shared,
        &provider,
        state,
        None,
        shared.restart.current_generation,
    );
    let Some(context) = context else { return false };
    let Some(response) = state
        .full_response
        .get(state.response_sent_offset..)
        .filter(|body| !body.trim().is_empty())
    else {
        return false;
    };
    let Some(_lease) = context.try_acquire_fresh_send_lease(shared, response) else {
        return false;
    };
    restore_inflight::settle_ready_without_output_for_actor(
        shared,
        &provider,
        state,
        Some(&claim.actor),
        |text| {
            let provider = &provider;
            let source = &source;
            async move {
                let outcome =
                    relay_recovery_terminal_notice(http, shared, provider, state, &text).await;
                if SourceAtEof::capture(state, output).as_ref() == Some(source) {
                    outcome
                } else {
                    RecoveryRelayOutcome::TransientFailure
                }
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_partial_source_stamp_refuses_append_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("source.jsonl");
        std::fs::write(&output, b"old").unwrap();
        let mut row = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            5071808,
            None,
            1,
            2,
            3,
            String::new(),
            None,
            None,
            Some(output.to_string_lossy().into_owned()),
            None,
            0,
        );
        row.last_offset = 3;
        let initial = SourceAtEof::capture(&row, &output).unwrap();
        assert!(SourceAtEof::capture(&row, &output).as_ref() == Some(&initial));
        std::fs::write(&output, b"longer").unwrap();
        assert!(SourceAtEof::capture(&row, &output).is_none());
        let replacement = root.path().join("replacement");
        std::fs::write(&replacement, b"new").unwrap();
        std::fs::rename(replacement, &output).unwrap();
        assert!(SourceAtEof::capture(&row, &output).as_ref() != Some(&initial));
    }
}
